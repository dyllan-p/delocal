//! A fault injected at every step of a commit, and a `kill -9` at every
//! step (DESIGN.md §7.5, §14.2): neither ever loses the file a commit
//! displaces.
//!
//! **Steps.** A commit is run once cleanly through [`Spy`], which lists
//! every call it made. Each scenario then runs again on a fresh folder for
//! each step:
//!
//! - **A fault** at that step, through [`FaultyFolder`] (`EIO`, and
//!   `ENOSPC`), counted as FaultyFs counts calls. A failed commit must leave
//!   the folder and the trash as it found them, its temp file in place for
//!   the fetch to resume, and the trash's rows matching the trash; a commit
//!   whose fault came after its rename in has landed and says so. Both
//!   renames failing in a row leaves the journal row open, and the start's
//!   undo then puts the displaced file back.
//! - **A `kill -9`** at that step: the test binary runs the commit in a
//!   child process, which stops just before that call and is killed with
//!   `SIGKILL`. The journal row lives in a real store, as the daemon keeps
//!   it, and after the kill the start's undo runs from what the store kept.
//!   The displaced file must then be at the path, in the trash or at its
//!   conflict-copy path, and the undo must not have replaced anything.
//!
//! Names are fixed (the temp file's, and no filesystem probe) so the steps
//! of every run are the same calls on the same paths.

#[cfg(feature = "faults")]
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write as _};
use std::process::{Command, Stdio};

use super::*;
#[cfg(feature = "faults")]
use crate::fs::FaultyFolder;
#[cfg(feature = "faults")]
use crate::fs::faulty::{Fault, Op, Rule, Spec, Trigger};
use crate::store::{FolderRow, Group, Store};

const OLD_CONTENT: &[u8] = b"the displaced file";
const CHILD_CONTENT: &[u8] = b"inside the displaced directory";
const NEW_CONTENT: &[u8] = b"the received file";

/// One commit to break at every step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    /// A file over a file, which goes to the trash.
    WriteOverFile,
    /// A file over a file, which goes to its conflict-copy path.
    WriteToConflictCopy,
    /// A file over a directory holding a file, which goes to the trash
    /// whole.
    WriteOverDirectory,
    /// A file where there was nothing, beneath missing parents.
    WriteNew,
    /// A delete, to the trash.
    Remove,
    /// A revert's move to the trash.
    MoveToTrash,
    /// A metadata-only apply.
    SetMeta,
}

impl Scenario {
    const ALL: [Self; 7] = [
        Self::WriteOverFile,
        Self::WriteToConflictCopy,
        Self::WriteOverDirectory,
        Self::WriteNew,
        Self::Remove,
        Self::MoveToTrash,
        Self::SetMeta,
    ];

    fn name(self) -> String {
        format!("{self:?}")
    }

    fn from_name(name: &str) -> Self {
        Self::ALL.into_iter().find(|s| s.name() == name).unwrap()
    }

    fn path(self) -> &'static str {
        match self {
            Self::WriteNew => "a/b/f",
            Self::WriteOverDirectory => "d",
            _ => "dir/f",
        }
    }

    /// The temp file a write commits, at a fixed name.
    fn temp() -> PathBuf {
        PathBuf::from(tmp::DIR).join("incoming")
    }

    /// Make the folder as the commit expects to find it.
    fn setup(self, disk: &Disk) {
        disk.dir("dir");
        match self {
            Self::WriteNew => {}
            Self::WriteOverDirectory => {
                disk.dir("d");
                disk.file("d/c", CHILD_CONTENT, OLD, false);
            }
            _ => {
                disk.file("dir/f", OLD_CONTENT, OLD, false);
            }
        }
        std::fs::write(disk.at(Self::temp().to_str().unwrap()), NEW_CONTENT).unwrap();
    }

    fn action(self) -> (Action, Option<PathBuf>) {
        let path = self.path();
        let old = Observed {
            kind: Kind::File,
            size: OLD_CONTENT.len() as u64,
            mtime_ns: OLD,
            exec: false,
            hash: blake3_of(OLD_CONTENT),
        };
        let incoming = entry(path, Kind::File, NEW_CONTENT, OLD + SECOND, true);
        let temp = Some(Self::temp());
        match self {
            Self::WriteOverFile => (write(path, incoming, Some(old), Displace::Trash), temp),
            Self::WriteToConflictCopy => {
                let copy = Displace::ConflictCopy(p("dir/f.conflict"));
                (write(path, incoming, Some(old), copy), temp)
            }
            Self::WriteOverDirectory => (write(path, incoming, Some(dir()), Displace::Trash), temp),
            Self::WriteNew => (write(path, incoming, None, Displace::Trash), temp),
            Self::Remove => (remove(path, Some(old), Displace::Trash), None),
            Self::MoveToTrash => (move_to_trash(path, old), None),
            Self::SetMeta => (set_meta(path, Some(old), OLD + 7, true), None),
        }
    }

    /// The content that must never be lost.
    fn precious(self) -> Option<&'static [u8]> {
        match self {
            Self::WriteNew => None,
            Self::WriteOverDirectory => Some(CHILD_CONTENT),
            _ => Some(OLD_CONTENT),
        }
    }
}

/// A disk for the steps of `scenario`: set up, with the filesystem's
/// sensitivity known so that no probe adds steps with fresh names.
fn disk_for(scenario: Scenario) -> Disk {
    let mut disk = Disk::new();
    let fs = disk.folder().open().unwrap();
    disk.state = DiskState::knowing(Sensitivity::probe(&*fs).unwrap());
    scenario.setup(&disk);
    disk
}

/// What a test sees of an entry: a directory, or a file's content, mode
/// and mtime.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    dir: bool,
    content: Vec<u8>,
    mode: u32,
    mtime_ns: i64,
}

/// Everything outside `.delocal`, path by path, with each file's mode and
/// mtime: a failure must leave all of it as it found it.
fn folder_tree(disk: &Disk) -> BTreeMap<PathBuf, Seen> {
    fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, Seen>) {
        for entry in std::fs::read_dir(root.join(rel)).unwrap() {
            let entry = entry.unwrap();
            let rel = rel.join(entry.file_name());
            if rel == Path::new(RESERVED_DIR) {
                continue;
            }
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            if meta.is_dir() {
                let seen = Seen {
                    dir: true,
                    content: Vec::new(),
                    mode: 0,
                    mtime_ns: 0,
                };
                out.insert(rel.clone(), seen);
                walk(root, &rel, out);
            } else {
                let seen = Seen {
                    dir: false,
                    content: std::fs::read(entry.path()).unwrap(),
                    mode: meta.mode() & 0o7777,
                    mtime_ns: meta.mtime() * SECOND + meta.mtime_nsec(),
                };
                out.insert(rel, seen);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(disk.root(), Path::new(""), &mut out);
    out
}

/// Every file's content in the folder outside `.delocal`, and in the trash.
fn everything(disk: &Disk) -> Vec<Vec<u8>> {
    let mut all: Vec<Vec<u8>> = folder_tree(disk)
        .into_values()
        .filter(|seen| !seen.dir)
        .map(|seen| seen.content)
        .collect();
    all.extend(disk.trash());
    all
}

/// The trash's rows, as the journal made them durable and the report then
/// changed them, against what is in the trash: every row names an entry,
/// and every file in the trash is at or beneath a row.
#[cfg(feature = "faults")]
fn rows_match_the_trash(
    disk: &Disk,
    durable: &[HostWrite],
    report: &[HostWrite],
) -> Result<(), String> {
    let mut rows: BTreeSet<PathBuf> = BTreeSet::new();
    for write in durable.iter().chain(report) {
        match write {
            HostWrite::PutTrash { row, .. } => {
                rows.insert(row.trashed_path.clone());
            }
            HostWrite::RemoveTrash { trashed_path, .. } => {
                rows.remove(trashed_path);
            }
            _ => {}
        }
    }
    let trash = disk.at(trash::DIR);
    let mut files = Vec::new();
    fn walk(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let rel = rel.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &rel, out);
            } else {
                out.push(rel);
            }
        }
    }
    walk(&trash, Path::new(""), &mut files);
    for row in &rows {
        if std::fs::symlink_metadata(trash.join(row)).is_err() {
            return Err(format!("a row for {} with nothing there", row.display()));
        }
    }
    for file in &files {
        if !rows.iter().any(|row| file.starts_with(row)) {
            return Err(format!("{} in the trash with no row", file.display()));
        }
    }
    Ok(())
}

/// The journal rows a commit left open: put durably, not removed by its
/// report.
#[cfg(feature = "faults")]
fn open_rows(durable: &[HostWrite], report: &[HostWrite]) -> Vec<JournalRow> {
    let mut rows: BTreeMap<RelPath, JournalRow> = BTreeMap::new();
    for write in durable.iter().chain(report) {
        match write {
            HostWrite::PutJournal { row, .. } => {
                rows.insert(row.path.clone(), row.clone());
            }
            HostWrite::RemoveJournal { path, .. } => {
                rows.remove(path);
            }
            _ => {}
        }
    }
    rows.into_values().collect()
}

/// Whether `scenario`'s commit is on disk: what came in at the path, or
/// the path gone, or its new mtime.
#[cfg(feature = "faults")]
fn landed(disk: &Disk, scenario: Scenario) -> Result<(), String> {
    let at_path = std::fs::symlink_metadata(disk.at(scenario.path())).ok();
    let ok = match scenario {
        Scenario::Remove | Scenario::MoveToTrash => at_path.is_none(),
        Scenario::SetMeta => {
            at_path.is_some_and(|m| m.mtime() * SECOND + m.mtime_nsec() == OLD + 7)
        }
        _ => std::fs::read(disk.at(scenario.path())).ok().as_deref() == Some(NEW_CONTENT),
    };
    if ok {
        Ok(())
    } else {
        Err("reported Ok, but not on disk".into())
    }
}

/// The step a journal write is, among a commit's calls.
const JOURNAL: &str = "journal.durable";

/// A journal whose writes are steps of the commit like its filesystem
/// calls: each is marked on a Spy just before it is made durable, so a hook
/// (the kill) can land on it, and the `fail_at`th can be made to fail.
struct StepJournal<'a> {
    inner: &'a dyn Journal,
    spy: Option<&'a Spy>,
    fail_at: Option<usize>,
    calls: std::sync::atomic::AtomicUsize,
}

impl<'a> StepJournal<'a> {
    fn new(inner: &'a dyn Journal, spy: Option<&'a Spy>, fail_at: Option<usize>) -> Self {
        Self {
            inner,
            spy,
            fail_at,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Journal for StepJournal<'_> {
    fn durable(&self, writes: Vec<HostWrite>) -> io::Result<()> {
        if let Some(spy) = self.spy {
            spy.mark(JOURNAL);
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_at == Some(n) {
            return Err(io::Error::other("the journal failed"));
        }
        self.inner.durable(writes)
    }
}

/// The steps a clean run of `scenario` makes: its filesystem calls with all
/// their paths, and its journal writes.
fn steps(scenario: Scenario) -> Vec<(&'static str, Vec<PathBuf>)> {
    let disk = disk_for(scenario);
    let spy = Spy::new(disk.root());
    let journal = StepJournal::new(&disk.journal, Some(&spy), None);
    let (action, temp) = scenario.action();
    let done = disk.commit_through(&spy, &journal, &action, temp.as_deref());
    assert_eq!(outcome(&done), &ApplyOutcome::Ok, "{scenario:?}");
    spy.steps()
}

#[cfg(feature = "faults")]
fn op(name: &str) -> Op {
    match name.trim_start_matches("fs.").trim_start_matches("dir.") {
        "open_root" => Op::OpenRoot,
        "open_dir" => Op::OpenDir,
        "read_dir" => Op::ReadDir,
        "lstat" => Op::Lstat,
        "read_link" => Op::ReadLink,
        "open_read" => Op::OpenRead,
        "create_new" => Op::CreateNew,
        "open_append" => Op::OpenAppend,
        "sync_file" => Op::SyncFile,
        "sync_dir" => Op::SyncDir,
        "rename" => Op::Rename,
        "rename_noreplace" => Op::RenameNoreplace,
        "remove_file" => Op::RemoveFile,
        "remove_dir" => Op::RemoveDir,
        "create_dir" => Op::CreateDir,
        "symlink" => Op::Symlink,
        "set_mtime" => Op::SetMtime,
        "set_mode" => Op::SetMode,
        "available_space" => Op::AvailableSpace,
        other => panic!("no fault op for {other}"),
    }
}

/// The FaultyFs rule that fails exactly step `k` of `steps` with `fail`:
/// its op, its first path as the pattern, and its count among the calls
/// that rule matches, as FaultyFs counts them (a rename on either path).
#[cfg(feature = "faults")]
fn rule_at(steps: &[(&'static str, Vec<PathBuf>)], k: usize, fail: Fault) -> Rule {
    let (name, paths) = &steps[k];
    let pattern = paths[0].clone();
    let matching = steps[..=k]
        .iter()
        .filter(|(n, ps)| *n != JOURNAL && op(n) == op(name) && ps.contains(&pattern))
        .count();
    Rule {
        op: op(name),
        path: pattern.to_str().unwrap().to_owned(),
        at: Trigger::Call(matching as u64),
        fail,
    }
}

/// §7.5, §14.2: a fault at each step of each scenario, `EIO` and `ENOSPC`.
#[cfg(feature = "faults")]
#[test]
fn a_fault_at_any_step_never_loses_the_displaced_file() {
    let mut tried = 0;
    for scenario in Scenario::ALL {
        let steps = steps(scenario);
        for k in 0..steps.len() {
            for fail in [Fault::Eio, Fault::Enospc] {
                tried += 1;
                let disk = disk_for(scenario);
                let before = folder_tree(&disk);
                let what = format!("{scenario:?}, step {k} {:?}, {fail:?}", steps[k]);
                let (action, temp) = scenario.action();
                let done = if steps[k].0 == JOURNAL {
                    // The journal write fails: the nth of the commit's.
                    let nth = steps[..k].iter().filter(|(n, _)| *n == JOURNAL).count();
                    let journal = StepJournal::new(&disk.journal, None, Some(nth));
                    disk.commit_through(&disk.folder(), &journal, &action, temp.as_deref())
                } else {
                    let rule = rule_at(&steps, k, fail);
                    let faulty =
                        FaultyFolder::new(Spy::new(disk.root()), Spec { rules: vec![rule] })
                            .unwrap();
                    let done = disk.commit_with(&faulty, &action, temp.as_deref());
                    assert_eq!(faulty.injected().len(), 1, "{what}: the fault did not fire");
                    done
                };
                let durable = disk.journal.taken();

                if let Some(precious) = scenario.precious() {
                    assert!(
                        everything(&disk).iter().any(|c| c == precious),
                        "{what}: lost"
                    );
                }
                rows_match_the_trash(&disk, &durable, &done.writes)
                    .map_err(|e| format!("{what}: {e}"))
                    .unwrap();
                // A row stays open only where the rename landed and an
                // fsync after it failed: it is not known to be durable.
                let unsure = *outcome(&done) == ApplyOutcome::Ok && !done.unsynced.is_empty();
                let open = open_rows(&durable, &done.writes);
                let displaces = matches!(
                    scenario,
                    Scenario::WriteOverFile
                        | Scenario::WriteToConflictCopy
                        | Scenario::WriteOverDirectory
                );
                assert_eq!(!open.is_empty(), unsure && displaces, "{what}: {open:?}");
                if !open.is_empty() {
                    // The next start leaves the landed commit alone.
                    let undone = undo_rows(&disk, &open);
                    assert_eq!(undone.left, [p(scenario.path())], "{what}");
                    landed(&disk, scenario)
                        .map_err(|e| format!("{what}: {e}"))
                        .unwrap();
                }
                match outcome(&done) {
                    // A fault the commit could carry on past (an fsync after
                    // the rename in, the trash row's size): it landed.
                    ApplyOutcome::Ok => landed(&disk, scenario)
                        .map_err(|e| format!("{what}: {e}"))
                        .unwrap(),
                    ApplyOutcome::Failed { error, found } => {
                        // A full disk is `DiskFull`, except where the host
                        // could not look at all (the root, its marker):
                        // that is `Io`, carrying `Skipped`.
                        let blind = matches!(found, ScanState::Skipped { .. });
                        let journal = steps[k].0 == JOURNAL;
                        let expected = if fail == Fault::Enospc && !blind && !journal {
                            LocalError::DiskFull
                        } else {
                            LocalError::Io
                        };
                        assert_eq!(*error, expected, "{what}");
                        assert_eq!(folder_tree(&disk), before, "{what}: not left as found");
                        if let Some(temp) = &temp {
                            assert!(
                                disk.exists(temp.to_str().unwrap()),
                                "{what}: the temp file went"
                            );
                        }
                    }
                    ApplyOutcome::ChangedUnderneath { .. } => {
                        panic!("{what}: nothing changed underneath")
                    }
                }
            }
        }
    }
    assert!(tried > 200, "only {tried} faults tried");
}

/// §7.5: the rename in fails, and so does putting the displaced file back.
/// The report carries `Skipped`, never `Absent`: the path is empty only
/// because of this commit, and must not read as a local deletion. The
/// journal row stays open. Either the next start's undo puts the file back,
/// or, while the daemon runs, the next commit at the path does so before
/// anything else.
#[cfg(feature = "faults")]
#[test]
fn both_renames_failing_strand_the_file_until_it_is_put_back() {
    for (scenario, fail, restart) in [
        (Scenario::WriteOverFile, Fault::Eio, true),
        (Scenario::WriteOverFile, Fault::Enospc, false),
        (Scenario::WriteToConflictCopy, Fault::Eio, false),
        (Scenario::WriteToConflictCopy, Fault::Enospc, true),
        (Scenario::WriteOverDirectory, Fault::Eio, true),
        (Scenario::WriteOverDirectory, Fault::Eio, false),
    ] {
        let what = format!("{scenario:?}, {fail:?}, restart {restart}");
        let disk = disk_for(scenario);
        let temp = Scenario::temp();
        let rules = vec![
            Rule {
                op: Op::RenameNoreplace,
                path: temp.to_str().unwrap().to_owned(),
                at: Trigger::Call(1),
                fail,
            },
            // Putting it back renames to the path: its third rename on it.
            Rule {
                op: Op::RenameNoreplace,
                path: scenario.path().to_owned(),
                at: Trigger::Call(3),
                fail,
            },
        ];
        let faulty = FaultyFolder::new(Spy::new(disk.root()), Spec { rules }).unwrap();
        let (action, _) = scenario.action();
        let done = disk.commit_with(&faulty, &action, Some(&temp));
        assert_eq!(faulty.injected().len(), 2, "{what}");
        let error = if fail == Fault::Enospc {
            LocalError::DiskFull
        } else {
            LocalError::Io
        };
        assert_eq!(
            outcome(&done),
            &ApplyOutcome::Failed {
                error,
                found: ScanState::Skipped {
                    reason: SkipReason::Io
                }
            },
            "{what}"
        );
        let mut rows = disk.journal.taken();
        rows.extend(done.writes);
        assert_eq!(open_rows(&rows, &[]).len(), 1, "{what}");
        let precious = scenario.precious().unwrap().to_vec();
        assert!(everything(&disk).contains(&precious), "{what}: lost");

        if restart {
            let undone = undo_rows(&disk, &open_rows(&rows, &[]));
            assert_eq!(undone.back, [p(scenario.path())], "{what}");
            rows.extend(undone.writes);
            let found = folder_tree(&disk);
            assert!(
                found.values().any(|seen| seen.content == precious),
                "{what}: not back"
            );
        } else {
            // The engine retries the want: the file goes back first, and
            // the commit then runs as it should have.
            let again = disk.commit(&action, Some(&temp));
            assert_eq!(outcome(&again), &ApplyOutcome::Ok, "{what}");
            // The put-back's rows were made durable before the commit's
            // own, which may reuse the place in the trash it left.
            rows.extend(disk.journal.taken());
            rows.extend(again.writes);
            landed(&disk, scenario)
                .map_err(|e| format!("{what}: {e}"))
                .unwrap();
            assert!(everything(&disk).contains(&precious), "{what}: lost");
        }
        assert!(open_rows(&rows, &[]).is_empty(), "{what}: a row left open");
        rows_match_the_trash(&disk, &[], &rows)
            .map_err(|e| format!("{what}: {e}"))
            .unwrap();
    }
}

/// §7.5, draft 64: a stranded path is skipped by the scan until a resolve
/// puts the file back.
#[cfg(feature = "faults")]
#[test]
fn a_stranded_path_is_skipped_until_it_is_resolved() {
    let scenario = Scenario::WriteOverFile;
    let disk = disk_for(scenario);
    let temp = Scenario::temp();
    let path = p(scenario.path());
    let rules = vec![
        Rule {
            op: Op::RenameNoreplace,
            path: temp.to_str().unwrap().to_owned(),
            at: Trigger::Call(1),
            fail: Fault::Eio,
        },
        Rule {
            op: Op::RenameNoreplace,
            path: scenario.path().to_owned(),
            at: Trigger::FromCall(3),
            fail: Fault::Eio,
        },
    ];
    let faulty = FaultyFolder::new(Spy::new(disk.root()), Spec { rules }).unwrap();
    let (action, _) = scenario.action();
    disk.commit_with(&faulty, &action, Some(&temp));
    let skipped = ScanState::Skipped {
        reason: crate::scan::OPEN_ROW,
    };
    assert!(disk.state.is_open(&path));
    assert_eq!(disk.scanned().get(&path), Some(&skipped));
    // Still failing: still open.
    let resolved = disk.resolve(&faulty);
    assert_eq!(resolved.open, std::slice::from_ref(&path));
    assert_eq!(disk.scanned().get(&path), Some(&skipped));

    let mut rows = disk.journal.taken();
    let resolved = disk.resolve(&disk.folder());
    assert_eq!(resolved.resolved, std::slice::from_ref(&path));
    assert!(!disk.state.is_open(&path));
    assert_eq!(
        std::fs::read(disk.at(scenario.path())).unwrap(),
        OLD_CONTENT
    );
    rows.extend(disk.journal.taken());
    assert!(open_rows(&rows, &[]).is_empty());
    rows_match_the_trash(&disk, &[], &rows).unwrap();
    assert!(matches!(
        disk.scanned().get(&path),
        Some(ScanState::Observed(_))
    ));
}

/// §7.5, draft 64: a commit that landed while an fsync after it failed
/// keeps its path skipped until its directories sync; a commit there in
/// the meantime goes ahead.
#[cfg(feature = "faults")]
#[test]
fn an_unsynced_commit_is_skipped_until_its_directories_sync() {
    let scenario = Scenario::WriteOverFile;
    let disk = disk_for(scenario);
    let temp = Scenario::temp();
    let path = p(scenario.path());
    let rule = Rule {
        op: Op::SyncDir,
        path: "dir".into(),
        at: Trigger::FromCall(1),
        fail: Fault::Eio,
    };
    let faulty = FaultyFolder::new(Spy::new(disk.root()), Spec { rules: vec![rule] }).unwrap();
    let (action, _) = scenario.action();
    let done = disk.commit_with(&faulty, &action, Some(&temp));
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert!(disk.state.is_open(&path));
    let skipped = ScanState::Skipped {
        reason: crate::scan::OPEN_ROW,
    };
    assert_eq!(disk.scanned().get(&path), Some(&skipped));
    assert_eq!(disk.resolve(&faulty).open, std::slice::from_ref(&path));

    // A metadata-only apply there goes ahead; the row stays open.
    let landed = Observed {
        kind: Kind::File,
        size: NEW_CONTENT.len() as u64,
        mtime_ns: OLD + SECOND,
        exec: true,
        hash: blake3_of(NEW_CONTENT),
    };
    let done = disk.commit_with(
        &faulty,
        &set_meta(scenario.path(), Some(landed), OLD + 9, true),
        None,
    );
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert!(disk.state.is_open(&path));

    disk.journal.taken();
    let resolved = disk.resolve(&disk.folder());
    assert_eq!(resolved.resolved, std::slice::from_ref(&path));
    assert_eq!(
        disk.journal.taken(),
        [HostWrite::RemoveJournal {
            folder: folder_id(),
            path: path.clone()
        }]
    );
    assert!(!disk.state.is_open(&path));
    assert!(matches!(
        disk.scanned().get(&path),
        Some(ScanState::Observed(_))
    ));
}

/// While a stranded file cannot be put back, no commit at its path goes
/// ahead, and each reports `Skipped`.
#[cfg(feature = "faults")]
#[test]
fn a_stranded_file_blocks_its_path_until_it_is_put_back() {
    let scenario = Scenario::WriteOverFile;
    let disk = disk_for(scenario);
    let temp = Scenario::temp();
    let rules = vec![
        Rule {
            op: Op::RenameNoreplace,
            path: temp.to_str().unwrap().to_owned(),
            at: Trigger::Call(1),
            fail: Fault::Eio,
        },
        // Every rename back to the path fails from the third on.
        Rule {
            op: Op::RenameNoreplace,
            path: scenario.path().to_owned(),
            at: Trigger::FromCall(3),
            fail: Fault::Eio,
        },
    ];
    let faulty = FaultyFolder::new(Spy::new(disk.root()), Spec { rules }).unwrap();
    let (action, _) = scenario.action();
    let skipped = ApplyOutcome::Failed {
        error: LocalError::Io,
        found: ScanState::Skipped {
            reason: SkipReason::Io,
        },
    };
    assert_eq!(
        outcome(&disk.commit_with(&faulty, &action, Some(&temp))),
        &skipped
    );
    let again = disk.commit_with(&faulty, &action, Some(&temp));
    assert_eq!(outcome(&again), &skipped);
    assert!(again.writes.is_empty(), "the row stays open");
    // A delete there waits too: nothing at the path is the user's yet.
    let done = disk.commit_with(
        &faulty,
        &remove(scenario.path(), None, Displace::Trash),
        None,
    );
    assert_eq!(outcome(&done), &skipped);
    assert!(everything(&disk).contains(&OLD_CONTENT.to_vec()));
}

/// The folder id and a store with the folder in it, for a journal that
/// outlives a killed process.
fn store_at(path: &Path) {
    let writer = Store::open(path).unwrap().writer().unwrap();
    let mut group = Group::new(settled_now());
    group.host(HostWrite::PutFolder(FolderRow {
        id: folder_id(),
        name: "folder".into(),
        created_by: NodeId::from_bytes([1; 16]),
        rules: delocal_engine::Rules::default(),
        meta_version: 1,
    }));
    writer.submit(group).wait().unwrap();
    writer.close().unwrap();
}

const ROOT: &str = "DELOCAL_KILL_ROOT";
const DB: &str = "DELOCAL_KILL_DB";
const AT: &str = "DELOCAL_KILL_AT";
const SCENARIO: &str = "DELOCAL_KILL_SCENARIO";
const PAUSED: &str = "paused before the step";

/// Not a test on its own: the body of the process that
/// [`a_kill_at_any_step_never_loses_the_displaced_file`] starts and kills.
/// It runs the scenario's commit against the parent's folder and store, and
/// stops for good just before the step the parent kills it at. Without the
/// parent's environment it does nothing.
#[test]
#[ignore = "run by a_kill_at_any_step_never_loses_the_displaced_file as its child"]
fn kill_child() {
    let (Ok(root), Ok(db), Ok(at), Ok(scenario)) = (
        std::env::var(ROOT),
        std::env::var(DB),
        std::env::var(AT),
        std::env::var(SCENARIO),
    ) else {
        return;
    };
    let scenario = Scenario::from_name(&scenario);
    let at: usize = at.parse().unwrap();
    let spy = Spy::new(Path::new(&root));
    spy.at_call(at, || {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{PAUSED}").unwrap();
        out.flush().unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
    let writer = Store::open(Path::new(&db)).unwrap().writer().unwrap();
    let clock = settled_now;
    let store = StoreJournal {
        writer: &writer,
        clock: &clock,
    };
    // Its writes are steps too, so the kill can land just before one.
    let journal = StepJournal::new(&store, Some(&spy), None);
    let fs = RealFolder::new(&root).open().unwrap();
    let state = DiskState::knowing(Sensitivity::probe(&*fs).unwrap());
    drop(fs);
    let (records, names): (BTreeMap<RelPath, Entry>, BTreeMap<RelPath, OsString>) =
        Default::default();
    let (action, temp) = scenario.action();
    let _ = Commit {
        id: folder_id(),
        folder: &spy,
        records: &records,
        names: &names,
        journal: &journal,
        clock: &clock,
        disk: &state,
    }
    .run(&action, &version(), temp.as_deref());
}

/// §7.5, "Two renames, one commit": a `kill -9` at each step of each
/// scenario, then the start's undo from what the store kept.
#[test]
fn a_kill_at_any_step_never_loses_the_displaced_file() {
    let exe = std::env::current_exe().unwrap();
    let mut killed = 0;
    for scenario in Scenario::ALL {
        let count = steps(scenario).len();
        // One past the last step: the commit ran to its end, and its
        // report never reached the engine.
        for k in 0..=count {
            let what = format!("{scenario:?}, killed before step {k} of {count}");
            let disk = disk_for(scenario);
            let db_dir = tempfile::tempdir().unwrap();
            let db = db_dir.path().join("delocal.db");
            store_at(&db);
            let mut child = Command::new(&exe)
                .args([
                    "--exact",
                    "commit::tests::faults::kill_child",
                    "--ignored",
                    "--nocapture",
                    "--test-threads",
                    "1",
                ])
                .env(ROOT, disk.root())
                .env(DB, &db)
                .env(AT, k.to_string())
                .env(SCENARIO, scenario.name())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let paused = BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .any(|line| line.contains(PAUSED));
            // `Child::kill` is SIGKILL: `kill -9`.
            let _ = child.kill();
            child.wait().unwrap();
            assert_eq!(paused, k < count, "{what}");
            killed += usize::from(paused);

            // The start: undo every open journal row from the store.
            let store = Store::open(&db).unwrap();
            let rows = store.journal(folder_id()).unwrap();
            let occupant = std::fs::read(disk.at(scenario.path())).ok();
            let fs = disk.folder().open().unwrap();
            let undone = undo(&*fs, folder_id(), &rows, &disk.names);
            assert!(undone.failed.is_empty(), "{what}: {:?}", undone.failed);
            if occupant.is_some() {
                assert_eq!(
                    std::fs::read(disk.at(scenario.path())).ok(),
                    occupant,
                    "{what}: the undo replaced something"
                );
            }
            if let Some(precious) = scenario.precious() {
                assert!(
                    everything(&disk).iter().any(|c| c == precious),
                    "{what}: lost"
                );
            }
            // The journal's promise: a write that displaced something
            // leaves its path as it found it or as it committed it, never
            // empty, whenever the process dies.
            let path_now = disk.at(scenario.path());
            match scenario {
                Scenario::WriteOverFile | Scenario::WriteToConflictCopy => {
                    let now = std::fs::read(&path_now).ok();
                    assert!(
                        now.as_deref() == Some(OLD_CONTENT) || now.as_deref() == Some(NEW_CONTENT),
                        "{what}: {now:?}"
                    );
                }
                Scenario::WriteOverDirectory => {
                    let as_found =
                        std::fs::read(path_now.join("c")).ok().as_deref() == Some(CHILD_CONTENT);
                    let committed = std::fs::read(&path_now).ok().as_deref() == Some(NEW_CONTENT);
                    assert!(as_found || committed, "{what}");
                }
                _ => {}
            }
            // What came in is at the path, or still in tmp/ to resume.
            let incoming = NEW_CONTENT.to_vec();
            let at_path = std::fs::read(disk.at(scenario.path())).ok();
            let in_tmp = std::fs::read(disk.at(Scenario::temp().to_str().unwrap())).ok();
            if matches!(
                scenario,
                Scenario::WriteOverFile
                    | Scenario::WriteToConflictCopy
                    | Scenario::WriteOverDirectory
                    | Scenario::WriteNew
            ) {
                assert!(
                    at_path.as_ref() == Some(&incoming) || in_tmp.as_ref() == Some(&incoming),
                    "{what}"
                );
            }
        }
    }
    assert!(killed > 50, "only {killed} kills");
}
