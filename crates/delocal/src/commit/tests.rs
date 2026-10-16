//! Tests of the commit path (DESIGN.md §7.5 steps 6 to 9, §8.3 step 2) on
//! real directories: each action and each way it ends, one at a time, and
//! the journal's undo at start. The reference table ported from the
//! simulator's host is in [`reference`], the races with a user writing at
//! the path in [`races`], and a fault and a `kill -9` at every step of a
//! commit in [`faults`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Mutex;

use delocal_engine::{HostName, NodeId};

use super::*;
use crate::fs::RealFolder;
use crate::fs::spy::Spy;
use crate::scan::hash::hash_target;

mod faults;
mod races;
mod reference;

pub const SECOND: i64 = 1_000_000_000;
/// An mtime an hour before the clock the tests run at.
pub const OLD: i64 = 1_700_000_000 * SECOND;

pub fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

pub fn blake3_of(bytes: &[u8]) -> ContentHash {
    ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
}

/// The real clock an hour ahead, so that every file a test made has
/// settled and a failed report's observation hashes it (§7.3).
pub fn settled_now() -> Timestamp {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    Timestamp::from_unix_nanos(i64::try_from(since.as_nanos()).unwrap() + 3600 * SECOND)
}

pub fn folder_id() -> FolderId {
    FolderId::from_bytes([7; 16])
}

/// A journal that keeps what it was asked to make durable, and can be made
/// to refuse.
#[derive(Default)]
pub struct MemJournal {
    pub writes: Mutex<Vec<HostWrite>>,
    pub refuse: AtomicBool,
}

impl Journal for MemJournal {
    fn durable(&self, writes: Vec<HostWrite>) -> io::Result<()> {
        if self.refuse.load(Ordering::Relaxed) {
            return Err(io::Error::other("the journal refused"));
        }
        self.writes.lock().unwrap().extend(writes);
        Ok(())
    }
}

impl MemJournal {
    pub fn taken(&self) -> Vec<HostWrite> {
        std::mem::take(&mut *self.writes.lock().unwrap())
    }
}

/// A folder on disk, with its marker and `.delocal/tmp/`, and what a commit
/// needs from the host.
pub struct Disk {
    pub tmp: tempfile::TempDir,
    pub records: BTreeMap<RelPath, Entry>,
    pub names: BTreeMap<RelPath, OsString>,
    pub journal: MemJournal,
    pub state: DiskState,
}

impl Disk {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(tmp::DIR)).unwrap();
        std::fs::write(tmp.path().join(RESERVED_DIR).join(MARKER), "{}").unwrap();
        Self {
            tmp,
            records: BTreeMap::new(),
            names: BTreeMap::new(),
            journal: MemJournal::default(),
            state: DiskState::new(),
        }
    }

    pub fn root(&self) -> &Path {
        self.tmp.path()
    }

    pub fn at(&self, path: &str) -> PathBuf {
        self.root().join(path)
    }

    pub fn folder(&self) -> RealFolder {
        RealFolder::new(self.root())
    }

    pub fn dir(&self, path: &str) {
        std::fs::create_dir_all(self.at(path)).unwrap();
    }

    /// A file holding `bytes`, with an mtime and an exec bit; what the
    /// fast path sees of it.
    pub fn file(&self, path: &str, bytes: &[u8], mtime_ns: i64, exec: bool) -> Observed {
        std::fs::write(self.at(path), bytes).unwrap();
        let mode = if exec { 0o755 } else { 0o644 };
        std::fs::set_permissions(self.at(path), std::fs::Permissions::from_mode(mode)).unwrap();
        let fs = self.folder().open().unwrap();
        fs.set_mtime(Path::new(path), mtime_ns).unwrap();
        Observed {
            kind: Kind::File,
            size: bytes.len() as u64,
            mtime_ns,
            exec,
            hash: blake3_of(bytes),
        }
    }

    pub fn symlink(&self, path: &str, target: &str) -> Observed {
        std::os::unix::fs::symlink(target, self.at(path)).unwrap();
        link(target)
    }

    /// A verified temp file holding `bytes`, as a fetch leaves it.
    pub fn temp(&self, bytes: &[u8]) -> PathBuf {
        let path = tmp::transfer(&blake3_of(bytes));
        std::fs::write(self.at(path.to_str().unwrap()), bytes).unwrap();
        path
    }

    pub fn commit_with(
        &self,
        folder: &dyn Folder,
        action: &Action,
        temp: Option<&Path>,
    ) -> Committed {
        self.commit_through(folder, &self.journal, action, temp)
    }

    /// A commit through `folder`, making its journal rows durable in
    /// `journal`.
    pub fn commit_through(
        &self,
        folder: &dyn Folder,
        journal: &dyn Journal,
        action: &Action,
        temp: Option<&Path>,
    ) -> Committed {
        let clock = settled_now;
        Commit {
            id: folder_id(),
            folder,
            records: &self.records,
            names: &self.names,
            journal,
            clock: &clock,
            disk: &self.state,
        }
        .run(action, &version(), temp)
        .unwrap()
    }

    pub fn commit(&self, action: &Action, temp: Option<&Path>) -> Committed {
        self.commit_with(&self.folder(), action, temp)
    }

    pub fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.at(path)).unwrap()
    }

    pub fn exists(&self, path: &str) -> bool {
        std::fs::symlink_metadata(self.at(path)).is_ok()
    }

    /// Every file and symlink in the trash, by content.
    pub fn trash(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        collect(&self.at(trash::DIR), &mut out);
        out.sort();
        out
    }
}

/// Every file's content and symlink's target under `dir`.
pub fn collect(dir: &Path, out: &mut Vec<Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            collect(&entry.path(), out);
        } else if kind.is_symlink() {
            let target = std::fs::read_link(entry.path()).unwrap();
            out.push(target.into_os_string().into_encoded_bytes());
        } else {
            out.push(std::fs::read(entry.path()).unwrap());
        }
    }
}

pub fn link(target: &str) -> Observed {
    Observed {
        kind: Kind::Symlink,
        size: target.len() as u64,
        mtime_ns: 0,
        exec: false,
        hash: hash_target(target.as_bytes()),
    }
}

pub fn dir() -> Observed {
    Observed {
        kind: Kind::Dir,
        size: 0,
        mtime_ns: 0,
        exec: false,
        hash: ContentHash::EMPTY,
    }
}

pub fn version() -> Version {
    Version::empty().incremented(NodeId::from_bytes([1; 16]))
}

/// The entry a write of `path` brings: `kind`, `bytes` (a file's content or
/// a symlink's target), an mtime and an exec bit.
pub fn entry(path: &str, kind: Kind, bytes: &[u8], mtime_ns: i64, exec: bool) -> Entry {
    let (size, hash) = match kind {
        Kind::Dir => (0, ContentHash::EMPTY),
        Kind::File | Kind::Symlink => (bytes.len() as u64, blake3_of(bytes)),
    };
    Entry {
        path: p(path),
        kind,
        size,
        mtime_ns: if kind == Kind::File { mtime_ns } else { 0 },
        stamp: mtime_ns,
        exec: exec && kind == Kind::File,
        hash,
        prev_hash: ContentHash::EMPTY,
        version: version(),
        deleted: false,
        modified_by: NodeId::from_bytes([1; 16]),
        author_host: HostName::new("laptop").unwrap(),
    }
}

pub fn write(path: &str, entry: Entry, expected: Option<Observed>, displace: Displace) -> Action {
    Action::Write {
        folder: folder_id(),
        path: p(path),
        entry,
        expected,
        displace,
    }
}

pub fn remove(path: &str, expected: Option<Observed>, displace: Displace) -> Action {
    Action::Remove {
        folder: folder_id(),
        path: p(path),
        expected,
        displace,
    }
}

pub fn set_meta(path: &str, expected: Option<Observed>, mtime_ns: i64, exec: bool) -> Action {
    Action::SetMeta {
        folder: folder_id(),
        path: p(path),
        expected,
        mtime_ns,
        exec,
    }
}

pub fn move_to_trash(path: &str, expected: Observed) -> Action {
    Action::MoveToTrash {
        folder: folder_id(),
        path: p(path),
        version: version(),
        expected,
    }
}

/// The outcome a report carries.
pub fn outcome(committed: &Committed) -> &ApplyOutcome {
    match &committed.event {
        Event::Applied { outcome, .. } | Event::Trashed { outcome, .. } => outcome,
        other => panic!("not a commit report: {other:?}"),
    }
}

fn observed_file(bytes: &[u8], mtime_ns: i64, exec: bool) -> ScanState {
    ScanState::Observed(Observed {
        kind: Kind::File,
        size: bytes.len() as u64,
        mtime_ns,
        exec,
        hash: blake3_of(bytes),
    })
}

fn changed(found: ScanState) -> ApplyOutcome {
    ApplyOutcome::ChangedUnderneath { found }
}

#[test]
fn a_new_file_lands_with_its_mtime_and_exec_bit_and_its_parents() {
    let disk = Disk::new();
    let temp = disk.temp(b"#!/bin/sh\n");
    let incoming = entry("bin/tools/run", Kind::File, b"#!/bin/sh\n", OLD, true);
    let done = disk.commit(
        &write("bin/tools/run", incoming, None, Displace::Trash),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(done.created, [p("bin"), p("bin/tools")]);
    assert!(
        done.writes.is_empty(),
        "nothing displaced, nothing journalled"
    );
    assert!(disk.journal.taken().is_empty());
    assert_eq!(disk.read("bin/tools/run"), b"#!/bin/sh\n");
    let meta = std::fs::symlink_metadata(disk.at("bin/tools/run")).unwrap();
    assert_eq!(meta.mtime() * SECOND + meta.mtime_nsec(), OLD);
    // Written like git (§7.1): execute wherever read is set.
    let read = meta.mode() & 0o444;
    assert_eq!(meta.mode() & 0o111, read >> 2);
    assert!(
        !disk.exists(temp.to_str().unwrap()),
        "the temp file is the file now"
    );

    // A plain file clears every execute bit.
    let temp = disk.temp(b"text");
    std::fs::set_permissions(
        disk.at(temp.to_str().unwrap()),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let incoming = entry("notes", Kind::File, b"text", OLD, false);
    disk.commit(
        &write("notes", incoming, None, Displace::Trash),
        Some(&temp),
    );
    assert_eq!(
        std::fs::symlink_metadata(disk.at("notes")).unwrap().mode() & 0o111,
        0
    );
}

#[test]
fn a_file_written_over_goes_to_the_trash_behind_a_durable_row() {
    let disk = Disk::new();
    let old = disk.file("report.txt", b"old", OLD, false);
    let temp = disk.temp(b"new");
    let incoming = entry("report.txt", Kind::File, b"new", OLD + SECOND, false);
    let done = disk.commit(
        &write("report.txt", incoming, Some(old), Displace::Trash),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(disk.read("report.txt"), b"new");
    assert_eq!(disk.trash(), [b"old".to_vec()]);

    // The journal row and the trash row were durable before the move, and
    // the report removes the journal row.
    let durable = disk.journal.taken();
    let [
        HostWrite::PutJournal { row, .. },
        HostWrite::PutTrash { row: trashed, .. },
    ] = &durable[..]
    else {
        panic!("expected the journal and trash rows, got {durable:?}");
    };
    assert_eq!(row.path, p("report.txt"));
    assert_eq!(row.temp_file.as_deref(), Some(temp.as_path()));
    assert_eq!(
        row.displaced_to,
        Path::new(trash::DIR).join(&trashed.trashed_path)
    );
    assert_eq!(trashed.hash, blake3_of(b"old"));
    assert_eq!(trashed.original_path, p("report.txt"));
    assert_eq!(
        done.writes,
        [HostWrite::RemoveJournal {
            folder: folder_id(),
            path: p("report.txt")
        }]
    );
}

#[test]
fn a_losing_file_goes_to_its_conflict_copy_path() {
    let disk = Disk::new();
    let old = disk.file("a.txt", b"mine", OLD, false);
    let temp = disk.temp(b"theirs");
    let incoming = entry("a.txt", Kind::File, b"theirs", OLD, false);
    let copy = p("a.conflict-20231114-221320-laptop.txt");
    let action = write(
        "a.txt",
        incoming.clone(),
        Some(old.clone()),
        Displace::ConflictCopy(copy.clone()),
    );
    let done = disk.commit(&action, Some(&temp));
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(disk.read("a.txt"), b"theirs");
    assert_eq!(disk.read(copy.as_str()), b"mine");
    assert!(disk.trash().is_empty());

    // A conflict-copy path taken since the engine looked fails the guard.
    let temp = disk.temp(b"again");
    let theirs = disk.file("a.txt", b"theirs", OLD, false);
    let done = disk.commit(
        &write(
            "a.txt",
            entry("a.txt", Kind::File, b"again", OLD, false),
            Some(theirs),
            Displace::ConflictCopy(copy),
        ),
        Some(&temp),
    );
    assert_eq!(
        outcome(&done),
        &changed(observed_file(b"theirs", OLD, false))
    );
    assert_eq!(disk.read("a.txt"), b"theirs");
}

#[test]
fn a_path_not_as_expected_fails_the_guard_and_moves_nothing() {
    let disk = Disk::new();
    disk.file("f", b"edited", OLD + SECOND, false);
    let temp = disk.temp(b"new");
    let expected = Observed {
        kind: Kind::File,
        size: 6,
        mtime_ns: OLD,
        exec: false,
        hash: blake3_of(b"before"),
    };
    let done = disk.commit(
        &write(
            "f",
            entry("f", Kind::File, b"new", OLD, false),
            Some(expected),
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(
        outcome(&done),
        &changed(observed_file(b"edited", OLD + SECOND, false))
    );
    assert_eq!(disk.read("f"), b"edited");
    assert!(
        disk.exists(temp.to_str().unwrap()),
        "the temp file stays to resume"
    );
    assert!(disk.journal.taken().is_empty());

    // Expected absent, and something is there.
    let done = disk.commit(
        &write(
            "f",
            entry("f", Kind::File, b"new", OLD, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(
        outcome(&done),
        &changed(observed_file(b"edited", OLD + SECOND, false))
    );

    // The report is dated by the host's observation, after the commit.
    let before = settled_now();
    let Event::Applied { at, .. } = done.event else {
        panic!()
    };
    assert!(at <= before);
}

#[test]
fn a_parent_that_is_not_a_directory_is_changed_underneath() {
    let disk = Disk::new();
    let fs_file = disk.file("parent", b"a file", OLD, false);
    let temp = disk.temp(b"x");
    let done = disk.commit(
        &write(
            "parent/x",
            entry("parent/x", Kind::File, b"x", OLD, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &changed(ScanState::Absent));
    // The same through a symlink to a directory: never followed.
    disk.dir("elsewhere");
    disk.symlink("link", "elsewhere");
    let done = disk.commit(
        &write(
            "link/x",
            entry("link/x", Kind::File, b"x", OLD, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &changed(ScanState::Absent));
    assert!(!disk.exists("elsewhere/x"));
    // A delete of a path that cannot be there has nothing to do (§7.5 as
    // the simulator's host does it).
    let done = disk.commit(&remove("parent/x", None, Displace::Trash), None);
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    let _ = fs_file;
}

#[test]
fn a_commit_never_writes_beneath_a_mount_point() {
    let disk = Disk::new();
    disk.dir("mnt/in");
    let old = disk.file("mnt/in/f", b"old", OLD, false);
    let spy = Spy::new(disk.root());
    spy.mount("mnt");
    let temp = disk.temp(b"new");
    let skipped_ignored = ApplyOutcome::Failed {
        error: LocalError::Io,
        found: ScanState::Skipped {
            reason: SkipReason::Ignored,
        },
    };
    let incoming = entry("mnt/in/f", Kind::File, b"new", OLD, false);
    let done = disk.commit_with(
        &spy,
        &write("mnt/in/f", incoming, Some(old.clone()), Displace::Trash),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &skipped_ignored);
    let done = disk.commit_with(
        &spy,
        &remove("mnt/in/f", Some(old.clone()), Displace::Trash),
        None,
    );
    assert_eq!(outcome(&done), &skipped_ignored);
    let done = disk.commit_with(&spy, &move_to_trash("mnt", dir()), None);
    assert_eq!(outcome(&done), &skipped_ignored);
    assert_eq!(disk.read("mnt/in/f"), b"old");
    assert!(disk.trash().is_empty());
    assert!(spy.calls("fs.rename_noreplace").is_empty());
}

/// §7.3, draft 48: a directory with a disk mounted inside is never moved.
/// A rename takes the mounted disk with it: into the trash, where pruning
/// would delete what is on it, or to a conflict copy.
#[test]
fn a_directory_with_a_disk_mounted_inside_is_never_moved() {
    let disk = Disk::new();
    disk.dir("x/mnt");
    disk.file("x/mnt/precious", b"on another device", OLD, false);
    let spy = Spy::new(disk.root());
    spy.mount("x/mnt");
    let temp = disk.temp(b"a file now");
    let incoming = entry("x", Kind::File, b"a file now", OLD, false);
    let copy = Displace::ConflictCopy(p("x.conflict"));
    for (action, temp) in [
        (
            write("x", incoming.clone(), Some(dir()), Displace::Trash),
            Some(temp.as_path()),
        ),
        (
            write("x", incoming.clone(), Some(dir()), copy.clone()),
            Some(temp.as_path()),
        ),
        (remove("x", Some(dir()), copy.clone()), None),
    ] {
        let done = disk.commit_with(&spy, &action, temp);
        assert!(
            matches!(
                outcome(&done),
                ApplyOutcome::Failed {
                    error: LocalError::Io,
                    found: ScanState::Observed(Observed {
                        kind: Kind::Dir,
                        ..
                    })
                }
            ),
            "{action:?}: {:?}",
            outcome(&done)
        );
        assert_eq!(disk.read("x/mnt/precious"), b"on another device");
        assert!(disk.trash().is_empty() && !disk.exists("x.conflict"));
        assert!(disk.journal.taken().is_empty(), "nothing journalled");
    }
    // A delete to the trash removes only an empty directory.
    let done = disk.commit_with(&spy, &remove("x", Some(dir()), Displace::Trash), None);
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::ChangedUnderneath { .. }
    ));
    // Once the disk is unmounted, the write goes ahead.
    let done = disk.commit(
        &write("x", incoming, Some(dir()), Displace::Trash),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(disk.trash(), [b"on another device".to_vec()]);
}

/// Trouble in the trash's own directories is the host's: a local failure
/// that backs off, never the path changing underneath, which would wait
/// for an observation that changes nothing.
#[test]
fn trouble_in_the_trash_is_a_local_failure() {
    let disk = Disk::new();
    let old = disk.file("f", b"old", OLD, false);
    let local = |done: &Committed| {
        matches!(
            outcome(done),
            ApplyOutcome::Failed {
                error: LocalError::Io,
                ..
            }
        )
    };
    // The trash is a file.
    std::fs::write(disk.at(trash::DIR), "not a directory").unwrap();
    let done = disk.commit(&remove("f", Some(old.clone()), Displace::Trash), None);
    assert!(local(&done), "{:?}", outcome(&done));
    let temp = disk.temp(b"new");
    let incoming = entry("f", Kind::File, b"new", OLD, false);
    let done = disk.commit(
        &write("f", incoming, Some(old.clone()), Displace::Trash),
        Some(&temp),
    );
    assert!(local(&done), "{:?}", outcome(&done));
    // Today's directory is a file.
    std::fs::remove_file(disk.at(trash::DIR)).unwrap();
    std::fs::create_dir(disk.at(trash::DIR)).unwrap();
    let today = Path::new(trash::DIR).join(trash::day(settled_now()));
    std::fs::write(disk.at(today.to_str().unwrap()), "not a directory").unwrap();
    let done = disk.commit(&move_to_trash("f", old.clone()), None);
    assert!(local(&done), "{:?}", outcome(&done));
    assert_eq!(disk.read("f"), b"old");
    // Today's directory goes between its making and the move into it: the
    // rename fails `ENOENT` for the trash's reasons, not the path's.
    std::fs::remove_file(disk.at(today.to_str().unwrap())).unwrap();
    let spy = Spy::new(disk.root());
    let day = disk.at(today.to_str().unwrap());
    spy.before("fs.rename_noreplace", "f", move || {
        std::fs::remove_dir_all(day).unwrap();
    });
    let done = disk.commit_with(&spy, &remove("f", Some(old), Displace::Trash), None);
    assert!(local(&done), "{:?}", outcome(&done));
    assert_eq!(disk.read("f"), b"old");
}

#[test]
fn a_folder_without_its_marker_is_not_written_and_reads_as_skipped() {
    let failed = ApplyOutcome::Failed {
        error: LocalError::Io,
        found: ScanState::Skipped {
            reason: SkipReason::Io,
        },
    };
    // The marker gone, `.delocal/` still there: nothing is written, and
    // what is at the path is not reported, since it may be an empty mount
    // point's (§7.3, the root guard).
    let disk = Disk::new();
    let old = disk.file("f", b"old", OLD, false);
    std::fs::remove_file(disk.at(RESERVED_DIR).join(MARKER)).unwrap();
    let temp = disk.temp(b"new");
    let done = disk.commit(
        &write(
            "f",
            entry("f", Kind::File, b"new", OLD, false),
            Some(old.clone()),
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &failed);
    let done = disk.commit(&remove("f", Some(old.clone()), Displace::Trash), None);
    assert_eq!(outcome(&done), &failed);
    let done = disk.commit(&move_to_trash("f", old.clone()), None);
    assert!(matches!(done.event, Event::Trashed { outcome, .. } if outcome == failed));
    assert_eq!(disk.read("f"), b"old");
    assert!(disk.exists(temp.to_str().unwrap()));
    assert!(disk.trash().is_empty());

    // `.delocal/` gone altogether: nothing makes it again.
    std::fs::remove_dir_all(disk.at(RESERVED_DIR)).unwrap();
    let temp = PathBuf::from(tmp::DIR).join("gone");
    let done = disk.commit(
        &write(
            "f",
            entry("f", Kind::File, b"new", OLD, false),
            Some(old.clone()),
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &failed);
    let done = disk.commit(&remove("f", Some(old), Displace::Trash), None);
    assert_eq!(outcome(&done), &failed);
    assert_eq!(disk.read("f"), b"old");
    assert!(!disk.exists(RESERVED_DIR), "nothing makes .delocal");
}

#[test]
fn symlinks_and_directories_are_written_in_place_of_anything() {
    let disk = Disk::new();
    // A symlink comes in through a symlink made in tmp/ from its target.
    let temp = disk.temp(b"../target");
    let done = disk.commit(
        &write(
            "s",
            entry("s", Kind::Symlink, b"../target", 0, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(
        std::fs::read_link(disk.at("s")).unwrap(),
        Path::new("../target")
    );
    assert!(!disk.exists(temp.to_str().unwrap()));
    assert_eq!(std::fs::read_dir(disk.at(tmp::DIR)).unwrap().count(), 0);

    // A temp file that is not the symlink's target is the host's failure.
    let temp = disk.temp(b"wrong");
    let done = disk.commit(
        &write(
            "t",
            entry("t", Kind::Symlink, b"right", 0, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::Failed {
            error: LocalError::Io,
            ..
        }
    ));

    // A directory replaces a file; the file goes to the trash.
    let old = disk.file("d", b"was a file", OLD, false);
    let done = disk.commit(
        &write(
            "d",
            entry("d", Kind::Dir, b"", 0, false),
            Some(old),
            Displace::Trash,
        ),
        None,
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert!(disk.at("d").is_dir());
    assert_eq!(disk.trash(), [b"was a file".to_vec()]);

    // A file replaces a directory, which goes whole with what is in it.
    std::fs::write(disk.at("d/inside"), "inside").unwrap();
    let temp = disk.temp(b"now a file");
    let done = disk.commit(
        &write(
            "d",
            entry("d", Kind::File, b"now a file", OLD, false),
            Some(dir()),
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(disk.read("d"), b"now a file");
    assert_eq!(disk.trash(), [b"inside".to_vec(), b"was a file".to_vec()]);
}

#[test]
fn a_delete_moves_a_file_to_the_trash_and_removes_only_an_empty_directory() {
    let disk = Disk::new();
    let old = disk.file("f", b"old", OLD, false);
    let done = disk.commit(&remove("f", Some(old.clone()), Displace::Trash), None);
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert!(!disk.exists("f"));
    assert_eq!(disk.trash(), [b"old".to_vec()]);
    let [HostWrite::PutTrash { row, .. }] = &done.writes[..] else {
        panic!("{:?}", done.writes);
    };
    assert_eq!((row.hash, row.size), (blake3_of(b"old"), 3));
    // Gone already, as expected: nothing to do.
    let done = disk.commit(&remove("f", None, Displace::Trash), None);
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    // Gone, but expected there: the guard says so.
    let done = disk.commit(&remove("f", Some(old), Displace::Trash), None);
    assert_eq!(outcome(&done), &changed(ScanState::Absent));

    disk.dir("full");
    std::fs::write(disk.at("full/x"), "x").unwrap();
    let done = disk.commit(&remove("full", Some(dir()), Displace::Trash), None);
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::ChangedUnderneath { .. }
    ));
    assert_eq!(disk.read("full/x"), b"x");
    disk.dir("empty");
    let done = disk.commit(&remove("empty", Some(dir()), Displace::Trash), None);
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert!(!disk.exists("empty"));

    // A directory displaced to its conflict copy goes whole (§7.6).
    let copy = p("full.conflict-20231114-221320-laptop");
    let done = disk.commit(
        &remove("full", Some(dir()), Displace::ConflictCopy(copy.clone())),
        None,
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(disk.read(&format!("{copy}/x")), b"x");
}

#[test]
fn a_metadata_only_apply_sets_the_mtime_and_exec_bit_it_guarded() {
    let disk = Disk::new();
    let old = disk.file("f", b"same", OLD, false);
    let done = disk.commit(&set_meta("f", Some(old.clone()), OLD + 5, true), None);
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    let meta = std::fs::symlink_metadata(disk.at("f")).unwrap();
    assert_eq!(meta.mtime() * SECOND + meta.mtime_nsec(), OLD + 5);
    assert_eq!(meta.mode() & 0o777, 0o755);
    assert!(done.unsynced.is_empty());
    // The file as it was is no longer there: the guard fails.
    let done = disk.commit(&set_meta("f", Some(old), OLD, false), None);
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::ChangedUnderneath { .. }
    ));
    // Not a file, or nothing: changed underneath, as the simulator's host.
    disk.dir("d");
    let done = disk.commit(&set_meta("d", Some(dir()), OLD, false), None);
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::ChangedUnderneath { .. }
    ));
    let done = disk.commit(&set_meta("nothing", None, OLD, false), None);
    assert_eq!(outcome(&done), &changed(ScanState::Absent));
}

#[test]
fn a_move_to_the_trash_is_reported_as_trashed() {
    let disk = Disk::new();
    let old = disk.file("f", b"discarded", OLD, false);
    let done = disk.commit(&move_to_trash("f", old.clone()), None);
    assert!(matches!(
        done.event,
        Event::Trashed {
            outcome: ApplyOutcome::Ok,
            ..
        }
    ));
    assert_eq!(disk.trash(), [b"discarded".to_vec()]);
    // An absent path is already where the move leaves it.
    let done = disk.commit(&move_to_trash("f", old), None);
    assert!(matches!(
        done.event,
        Event::Trashed {
            outcome: ApplyOutcome::Ok,
            ..
        }
    ));
    // Anything but the file revert trashed fails the guard.
    let other = disk.file("g", b"edited", OLD, false);
    let done = disk.commit(
        &move_to_trash(
            "g",
            Observed {
                mtime_ns: OLD + 1,
                ..other
            },
        ),
        None,
    );
    assert!(matches!(
        done.event,
        Event::Trashed {
            outcome: ApplyOutcome::ChangedUnderneath { .. },
            ..
        }
    ));
    // A directory moves only when empty (draft 61), and it is removed, so
    // nothing of it is left in the trash.
    disk.dir("d/keep");
    let done = disk.commit(&move_to_trash("d", dir()), None);
    assert!(matches!(
        done.event,
        Event::Trashed {
            outcome: ApplyOutcome::ChangedUnderneath { .. },
            ..
        }
    ));
    assert!(disk.at("d/keep").is_dir());
    let done = disk.commit(&move_to_trash("d/keep", dir()), None);
    assert!(matches!(
        done.event,
        Event::Trashed {
            outcome: ApplyOutcome::Ok,
            ..
        }
    ));
    assert!(!disk.exists("d/keep"));
}

#[test]
fn a_live_record_spelt_otherwise_is_a_case_collision() {
    let mut disk = Disk::new();
    disk.state = DiskState::knowing(Sensitivity {
        folds_case: true,
        folds_normalisation: true,
    });
    disk.records
        .insert(p("Docs"), entry("Docs", Kind::Dir, b"", 0, false));
    let temp = disk.temp(b"x");
    let done = disk.commit(
        &write(
            "docs/x",
            entry("docs/x", Kind::File, b"x", OLD, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(
        outcome(&done),
        &ApplyOutcome::Failed {
            error: LocalError::CaseCollision { with: p("Docs") },
            found: ScanState::Absent
        }
    );
    assert!(!disk.exists("docs"));
    // Only a write checks: a delete removes what is at its own path.
    let done = disk.commit(&remove("docs/x", None, Displace::Trash), None);
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
}

#[test]
fn a_name_on_disk_is_addressed_and_written_back_as_the_index_name() {
    let mut disk = Disk::new();
    let nfd = "cafe\u{301}";
    let nfc = "caf\u{e9}";
    let old = disk.file(nfd, b"user's", OLD, false);
    disk.names.insert(p(nfc), nfd.into());
    let temp = disk.temp(b"received");
    let done = disk.commit(
        &write(
            nfc,
            entry(nfc, Kind::File, b"received", OLD, false),
            Some(old),
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(disk.trash(), [b"user's".to_vec()]);
    // Received paths are written as they are (§7.3): the pair goes.
    assert!(done.writes.contains(&HostWrite::RemoveDiskName {
        folder: folder_id(),
        path: p(nfc)
    }));
    let names: Vec<OsString> = std::fs::read_dir(disk.root())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != RESERVED_DIR)
        .collect();
    assert_eq!(names.len(), 1);
    assert_eq!(disk.read(nfc), b"received");
}

#[test]
fn a_missing_temp_file_or_a_refusing_journal_fails_before_anything_moves() {
    let disk = Disk::new();
    let old = disk.file("f", b"old", OLD, false);
    let incoming = entry("f", Kind::File, b"new", OLD, false);
    let missing = PathBuf::from(tmp::DIR).join("missing");
    let done = disk.commit(
        &write("f", incoming.clone(), Some(old.clone()), Displace::Trash),
        Some(&missing),
    );
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::Failed {
            error: LocalError::Io,
            ..
        }
    ));
    let done = disk.commit(
        &write("f", incoming.clone(), Some(old.clone()), Displace::Trash),
        None,
    );
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::Failed {
            error: LocalError::Io,
            ..
        }
    ));

    disk.journal.refuse.store(true, Ordering::Relaxed);
    let temp = disk.temp(b"new");
    let done = disk.commit(
        &write("f", incoming, Some(old), Displace::Trash),
        Some(&temp),
    );
    assert_eq!(
        outcome(&done),
        &ApplyOutcome::Failed {
            error: LocalError::Io,
            found: observed_file(b"old", OLD, false)
        }
    );
    assert_eq!(disk.read("f"), b"old");
    assert!(disk.trash().is_empty());
    assert!(disk.exists(temp.to_str().unwrap()));
}

#[test]
fn exec_bits_are_written_like_gits() {
    assert_eq!(exec_mode(0o644, true), 0o755);
    assert_eq!(exec_mode(0o600, true), 0o700);
    assert_eq!(exec_mode(0o640, true), 0o750);
    assert_eq!(exec_mode(0o755, false), 0o644);
    assert_eq!(exec_mode(0o711, false), 0o600);
    assert_eq!(exec_mode(0o4755, false), 0o4644);
}

/// A journal row as a commit makes it, for a displacement of `path` to
/// `to`.
fn row(path: &str, to: &Path) -> JournalRow {
    JournalRow {
        path: p(path),
        displaced_to: to.to_path_buf(),
        temp_file: Some(PathBuf::from(tmp::DIR).join("t")),
    }
}

fn undo_rows(disk: &Disk, rows: &[JournalRow]) -> journal::Undone {
    let fs = disk.folder().open().unwrap();
    undo(&*fs, folder_id(), rows, &disk.names)
}

#[test]
fn the_journal_undoes_a_displacement_into_an_empty_path_only() {
    let disk = Disk::new();
    let in_trash = Path::new(trash::DIR).join("2026-10-15/a");
    let trash_row = HostWrite::RemoveTrash {
        folder: folder_id(),
        trashed_path: "2026-10-15/a".into(),
    };
    let journal_row = |path: &str| HostWrite::RemoveJournal {
        folder: folder_id(),
        path: p(path),
    };
    std::fs::create_dir_all(disk.at(in_trash.parent().unwrap().to_str().unwrap())).unwrap();

    // The crash came between the two renames: the path is empty, and what
    // was displaced comes back out of the trash, losing its trash row.
    std::fs::write(disk.at(in_trash.to_str().unwrap()), "displaced").unwrap();
    let undone = undo_rows(&disk, &[row("a", &in_trash)]);
    assert_eq!(undone.back, [p("a")]);
    assert_eq!(undone.writes, [journal_row("a"), trash_row.clone()]);
    assert_eq!(disk.read("a"), b"displaced");
    assert!(disk.trash().is_empty());

    // The rename had completed, or the user wrote the path while the
    // daemon was down: nothing moves, and the trash keeps its row.
    std::fs::write(disk.at(in_trash.to_str().unwrap()), "displaced").unwrap();
    std::fs::write(disk.at("a"), "occupant").unwrap();
    let undone = undo_rows(&disk, &[row("a", &in_trash)]);
    assert_eq!(undone.left, [p("a")]);
    assert_eq!(undone.writes, [journal_row("a")]);
    assert_eq!(disk.read("a"), b"occupant");
    assert_eq!(disk.trash(), [b"displaced".to_vec()]);

    // The crash came before the displacement: nothing to move back, and the
    // trash row made with the journal's describes nothing.
    std::fs::remove_file(disk.at(in_trash.to_str().unwrap())).unwrap();
    let undone = undo_rows(&disk, &[row("a", &in_trash)]);
    assert!(undone.back.is_empty() && undone.left.is_empty());
    assert_eq!(undone.writes, [journal_row("a"), trash_row]);

    // A conflict-copy displacement comes back from its copy path, and one
    // whose parent went while the daemon was down stays where it is.
    std::fs::write(disk.at("b.conflict"), "loser").unwrap();
    disk.dir("gone-dir");
    std::fs::write(disk.at("c.conflict"), "kept").unwrap();
    std::fs::remove_dir(disk.at("gone-dir")).unwrap();
    let undone = undo_rows(
        &disk,
        &[
            row("b", Path::new("b.conflict")),
            row("gone-dir/c", Path::new("c.conflict")),
        ],
    );
    assert_eq!(undone.back, [p("b")]);
    assert_eq!(undone.left, [p("gone-dir/c")]);
    assert_eq!(disk.read("b"), b"loser");
    assert_eq!(disk.read("c.conflict"), b"kept");
}

/// §7.5: a row whose path cannot be looked at, or is beneath a mount
/// point, stays open for the next start, which then undoes it.
#[test]
fn an_undo_that_cannot_look_keeps_its_row() {
    let disk = Disk::new();
    disk.dir("dir");
    let in_trash = Path::new(trash::DIR).join("2026-10-15/f");
    std::fs::create_dir_all(disk.at(in_trash.parent().unwrap().to_str().unwrap())).unwrap();
    std::fs::write(disk.at(in_trash.to_str().unwrap()), "displaced").unwrap();
    let rows = [row("dir/f", &in_trash)];

    let spy = Spy::new(disk.root());
    spy.mount("dir");
    let undone = undo(&*spy.open().unwrap(), folder_id(), &rows, &disk.names);
    assert_eq!(undone.failed.len(), 1);
    assert!(undone.writes.is_empty(), "the row stays");

    #[cfg(feature = "faults")]
    {
        use crate::fs::FaultyFolder;
        use crate::fs::faulty::{Fault, Op, Rule, Spec, Trigger};
        let rule = Rule {
            op: Op::Lstat,
            path: "dir".into(),
            at: Trigger::FromCall(1),
            fail: Fault::Eacces,
        };
        let faulty = FaultyFolder::new(disk.folder(), Spec { rules: vec![rule] }).unwrap();
        let undone = undo(&*faulty.open().unwrap(), folder_id(), &rows, &disk.names);
        assert_eq!(undone.failed.len(), 1);
        assert!(undone.writes.is_empty(), "the row stays");
    }
    assert!(!disk.exists("dir/f"));

    let undone = undo_rows(&disk, &rows);
    assert_eq!(undone.back, [p("dir/f")]);
    assert_eq!(disk.read("dir/f"), b"displaced");
}

/// What a test tree holds at `path`: a file's content, a directory, or
/// nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Held {
    Nothing,
    File(Vec<u8>),
    Dir,
}

fn held(disk: &Disk, path: &str) -> Held {
    match std::fs::symlink_metadata(disk.at(path)) {
        Err(_) => Held::Nothing,
        Ok(meta) if meta.is_dir() => Held::Dir,
        Ok(_) => Held::File(disk.read(path)),
    }
}

fn arb_held() -> impl Strategy<Value = Held> {
    prop_oneof![
        Just(Held::Nothing),
        Just(Held::Dir),
        proptest::collection::vec(any::<u8>(), 0..8).prop_map(Held::File),
    ]
}

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// §7.5, draft 36: whatever is at the path and at the displacement's
    /// target when the daemon starts, the undo replaces nothing. What was
    /// at the path is still there, and what was displaced is at the path
    /// or still where it was put.
    #[test]
    fn the_journal_undo_never_replaces_anything(
        at_path in arb_held(),
        displaced in arb_held(),
        to_trash in any::<bool>(),
    ) {
        let disk = Disk::new();
        let to = if to_trash {
            Path::new(trash::DIR).join("2026-10-15/x")
        } else {
            PathBuf::from("x.conflict")
        };
        let to_str = to.to_str().unwrap();
        std::fs::create_dir_all(disk.at(to_str).parent().unwrap()).unwrap();
        let make = |path: &str, what: &Held| match what {
            Held::Nothing => {}
            Held::Dir => {
                std::fs::create_dir(disk.at(path)).unwrap();
                std::fs::write(disk.at(path).join("inner"), path).unwrap();
            }
            Held::File(bytes) => std::fs::write(disk.at(path), bytes).unwrap(),
        };
        make("x", &at_path);
        make(to_str, &displaced);
        let inner_of = |path: &str| std::fs::read(disk.at(path).join("inner")).ok();
        let (inner_at, inner_displaced) = (inner_of("x"), inner_of(to_str));

        let undone = undo_rows(&disk, &[row("x", &to)]);
        prop_assert!(undone.failed.is_empty());
        if at_path != Held::Nothing {
            prop_assert_eq!(held(&disk, "x"), at_path);
            prop_assert_eq!(inner_of("x"), inner_at);
            prop_assert_eq!(held(&disk, to_str), displaced);
        } else if displaced != Held::Nothing {
            prop_assert_eq!(held(&disk, "x"), displaced);
            prop_assert_eq!(inner_of("x"), inner_displaced);
            prop_assert_eq!(held(&disk, to_str), Held::Nothing);
        }
    }
}

/// §7.5, §8.4: a folder's start undoes the journal, reconciles and prunes
/// the trash, and clears `tmp/`, in that order.
#[test]
fn a_start_undoes_reconciles_prunes_and_clears_tmp() {
    let disk = Disk::new();
    let now = settled_now();
    let day = trash::day(now);
    let trashed = |name: &str| Path::new(trash::DIR).join(&day).join(name);
    let row = |name: &str, at: Timestamp| TrashRow {
        trashed_path: Path::new(&day).join(name),
        original_path: p(name),
        hash: blake3_of(name.as_bytes()),
        trashed_at: at,
        size: name.len() as u64,
    };
    std::fs::create_dir_all(disk.at(trashed("").to_str().unwrap())).unwrap();
    // A displacement a crash left open, whose temp file a want claims.
    std::fs::write(disk.at(trashed("back").to_str().unwrap()), "back").unwrap();
    let claimed_temp = PathBuf::from(tmp::DIR).join("claimed");
    std::fs::write(disk.at(claimed_temp.to_str().unwrap()), "partial").unwrap();
    let open = JournalRow {
        path: p("back"),
        displaced_to: trashed("back"),
        temp_file: Some(claimed_temp.clone()),
    };
    // An entry with no row, and one old enough to prune.
    std::fs::write(disk.at(trashed("orphan").to_str().unwrap()), "orphan").unwrap();
    std::fs::write(disk.at(trashed("old").to_str().unwrap()), "old").unwrap();
    let rows = [
        row("back", now),
        row("old", now.plus_nanos(-40 * 86_400 * SECOND)),
    ];
    std::fs::write(disk.at(tmp::DIR).join("unclaimed"), "stale").unwrap();

    let fs = disk.folder().open().unwrap();
    let started = Start {
        folder: folder_id(),
        journal: &[open],
        trash: &rows,
        names: &disk.names,
        claimed: &|name| name == "claimed",
        now,
        policy: trash::Policy::default(),
    }
    .run(&*fs);

    assert_eq!(disk.read("back"), b"back");
    assert_eq!(disk.trash(), [b"orphan".to_vec()]);
    assert!(disk.exists(claimed_temp.to_str().unwrap()));
    assert_eq!(started.temps, [OsString::from("unclaimed")]);
    assert!(started.unpruned.is_empty() && started.tmp_error.is_none());
    let f = folder_id();
    let removed = |name: &str| HostWrite::RemoveTrash {
        folder: f,
        trashed_path: Path::new(&day).join(name),
    };
    assert_eq!(started.writes.len(), 4, "{:?}", started.writes);
    assert_eq!(
        started.writes[0],
        HostWrite::RemoveJournal {
            folder: f,
            path: p("back")
        }
    );
    assert_eq!(started.writes[1], removed("back"));
    assert!(
        matches!(&started.writes[2], HostWrite::PutTrash { row, .. } if row.trashed_path == Path::new(&day).join("orphan"))
    );
    assert_eq!(started.writes[3], removed("old"));
}
