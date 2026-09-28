//! Tests of the scanner (DESIGN.md §7.3) on real directories. Where what
//! matters is what the engine does with the reports (above all, that
//! nothing a scan could not see is ever tombstoned) a real engine sits
//! behind the scanner.

use std::collections::BTreeMap;

use delocal_engine::{Action, Engine, HostName, NodeConfig, NodeId, Rules, Version};

use super::*;
use crate::fs::RealFolder;

mod reference;
mod spy;

use spy::Spy;

const SECOND: i64 = 1_000_000_000;
/// The tests' now: early 2027.
const NOW: i64 = 1_800_000_000 * SECOND;
/// An mtime long settled at [`NOW`].
const OLD: i64 = NOW - 3600 * SECOND;

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

fn id() -> FolderId {
    FolderId::from_bytes([7; 16])
}

fn at(nanos: i64) -> Timestamp {
    Timestamp::from_unix_nanos(nanos)
}

fn blake3_of(bytes: &[u8]) -> ContentHash {
    ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
}

/// A folder on disk: a temporary directory with the root guard's marker.
struct Disk {
    tmp: tempfile::TempDir,
}

impl Disk {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(RESERVED_DIR)).unwrap();
        std::fs::write(tmp.path().join(RESERVED_DIR).join(MARKER), "{}").unwrap();
        Self { tmp }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root().join(path)
    }

    fn folder(&self) -> RealFolder {
        RealFolder::new(self.root())
    }

    fn dir(&self, path: &str) {
        std::fs::create_dir_all(self.at(path)).unwrap();
    }

    /// A file holding `bytes`, with an mtime and an exec bit.
    fn file(&self, path: &str, bytes: &[u8], mtime_ns: i64, exec: bool) {
        std::fs::write(self.at(path), bytes).unwrap();
        let fs = self.folder().open().unwrap();
        let mode = if exec { 0o755 } else { 0o644 };
        fs.set_mode(Path::new(path), mode).unwrap();
        fs.set_mtime(Path::new(path), mtime_ns).unwrap();
    }

    fn symlink(&self, path: &str, target: &str) {
        std::os::unix::fs::symlink(target, self.at(path)).unwrap();
    }

    fn touch(&self, path: &str, mtime_ns: i64) {
        let fs = self.folder().open().unwrap();
        fs.set_mtime(Path::new(path), mtime_ns).unwrap();
    }
}

/// Scan `folder` against `records` with the clock at `now`, on `hashers`
/// threads: the events, and the report.
fn scan_at(
    folder: &dyn Folder,
    records: &dyn Records,
    now: i64,
    hashers: usize,
) -> (Vec<Event>, ScanReport) {
    let clock = move || at(now);
    let mut events = Vec::new();
    let report = Scan {
        id: id(),
        folder,
        records,
        clock: &clock,
        hashers: NonZeroUsize::new(hashers).unwrap(),
    }
    .run(&mut |event| events.push(event));
    (events, report)
}

fn scan(folder: &dyn Folder, records: &dyn Records) -> (Vec<Event>, ScanReport) {
    scan_at(folder, records, NOW, 4)
}

/// Whether `events` are one bracket that finished.
fn finished(events: &[Event]) -> bool {
    events.first() == Some(&Event::ScanStarted { folder: id() })
        && events.last() == Some(&Event::ScanFinished { folder: id() })
}

/// Whether `events` are a bracket aborted before anything was reported.
fn aborted_at_once(events: &[Event]) -> bool {
    events
        == [
            Event::ScanStarted { folder: id() },
            Event::ScanAborted { folder: id() },
        ]
}

/// The `Scanned` reports of one bracket, in order.
fn reports(events: &[Event]) -> Vec<(RelPath, ScanState)> {
    assert_eq!(events.first(), Some(&Event::ScanStarted { folder: id() }));
    let mut out = Vec::new();
    for event in &events[1..events.len() - 1] {
        match event {
            Event::Scanned {
                folder,
                path,
                state,
            } if *folder == id() => out.push((path.clone(), state.clone())),
            other => panic!("not a report of this folder: {other:?}"),
        }
    }
    out
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

fn observed_dir() -> ScanState {
    ScanState::Observed(Observed {
        kind: Kind::Dir,
        size: 0,
        mtime_ns: 0,
        exec: false,
        hash: ContentHash::EMPTY,
    })
}

fn observed_link(target: &str) -> ScanState {
    ScanState::Observed(Observed {
        kind: Kind::Symlink,
        size: target.len() as u64,
        mtime_ns: 0,
        exec: false,
        hash: blake3_of(target.as_bytes()),
    })
}

fn skip(reason: SkipReason) -> ScanState {
    ScanState::Skipped { reason }
}

/// An engine that is the only member of the folder.
fn engine() -> Engine {
    let own = NodeId::from_bytes([1; 16]);
    let mut engine = Engine::new(NodeConfig {
        node_id: own,
        author_host: HostName::new("laptop").unwrap(),
    });
    engine.handle(
        at(NOW),
        Event::FolderJoined {
            folder: id(),
            rules: Rules::default(),
            members: vec![own],
        },
    );
    engine
}

/// The engine's index of the folder: the host's records.
fn index(engine: &Engine) -> Index {
    engine.folder(id()).unwrap().index().clone()
}

/// What the engine holds live, path by path.
fn live(engine: &Engine) -> BTreeMap<RelPath, Observed> {
    index(engine)
        .live_records()
        .map(|record| (record.entry.path.clone(), record.entry.observed()))
        .collect()
}

/// Hand `events` to `engine` and return everything it did.
fn feed(engine: &mut Engine, events: Vec<Event>) -> Vec<Action> {
    events
        .into_iter()
        .flat_map(|event| engine.handle(at(NOW), event))
        .collect()
}

/// The paths `actions` wrote an index record at, and those they tombstoned.
fn written(actions: &[Action]) -> (Vec<RelPath>, Vec<RelPath>) {
    let mut written = Vec::new();
    let mut tombstoned = Vec::new();
    for action in actions {
        if let Action::IndexChanged { record, .. } = action {
            written.push(record.entry.path.clone());
            if record.entry.deleted {
                tombstoned.push(record.entry.path.clone());
            }
        }
        assert!(
            !matches!(action, Action::IndexRemoved { .. }),
            "a scan never removes a record: {action:?}"
        );
    }
    (written, tombstoned)
}

/// A tree with some of everything, the engine that has scanned it once,
/// and every path in it.
fn tracked_tree() -> (Disk, Engine) {
    let disk = Disk::new();
    disk.dir("d/sub");
    disk.dir("e");
    disk.file("d/f", b"eff", OLD, false);
    disk.file("d/sub/g", &vec![3; 2 * hash::CHUNK + 7], OLD, false);
    disk.file("e/h", b"aitch", OLD, true);
    disk.file("top", b"top", OLD, false);
    disk.symlink("d/l", "../top");
    std::fs::write(disk.at(ignore_rules::IGNORE_FILE), "*.tmp\n").unwrap();
    disk.touch(ignore_rules::IGNORE_FILE, OLD);
    let mut engine = engine();
    let (events, _) = scan(&disk.folder(), &BTreeMap::new());
    assert!(finished(&events));
    feed(&mut engine, events);
    assert_eq!(live(&engine).len(), 9);
    (disk, engine)
}

#[test]
fn a_tree_scans_to_what_is_on_disk_then_to_nothing_new() {
    let disk = Disk::new();
    let big = vec![9; 3 * hash::CHUNK + 1];
    disk.dir("docs/empty");
    disk.file("docs/a.txt", b"alpha", OLD, false);
    disk.file("run.sh", b"#!/bin/sh\n", OLD - SECOND, true);
    disk.file("big", &big, OLD, false);
    disk.symlink("docs/link", "../run.sh");

    let (events, report) = scan(&disk.folder(), &BTreeMap::new());
    assert!(finished(&events));
    assert!(report.aborted.is_none());
    assert!(report.skipped.is_empty() && report.disk_names.is_empty());
    // Depth first, each directory's names by bytes, a directory before
    // what is in it; `.delocal` is never an entry.
    assert_eq!(
        reports(&events),
        [
            (p("big"), observed_file(&big, OLD, false)),
            (p("docs"), observed_dir()),
            (p("docs/a.txt"), observed_file(b"alpha", OLD, false)),
            (p("docs/empty"), observed_dir()),
            (p("docs/link"), observed_link("../run.sh")),
            (
                p("run.sh"),
                observed_file(b"#!/bin/sh\n", OLD - SECOND, true)
            ),
        ]
    );
    // Whichever threads hash, the events are the same.
    assert_eq!(scan_at(&disk.folder(), &BTreeMap::new(), NOW, 1).0, events);

    // The engine records it all; scanned again against those records,
    // nothing is hashed and nothing changes.
    let mut engine = engine();
    feed(&mut engine, events);
    assert_eq!(live(&engine).len(), 6);
    let (again, _) = scan(&disk.folder(), &index(&engine));
    let states: Vec<ScanState> = reports(&again).into_iter().map(|(_, s)| s).collect();
    assert_eq!(states, vec![ScanState::Unchanged; 6]);
    assert_eq!(written(&feed(&mut engine, again)), (vec![], vec![]));
}

#[test]
fn every_change_the_fast_path_must_see_is_seen() {
    let (disk, engine) = tracked_tree();
    // A chmod, a touch, a retarget to a target of the same length, and a
    // file that became a directory: none changes a size.
    let fs = disk.folder().open().unwrap();
    fs.set_mode(Path::new("d/f"), 0o755).unwrap();
    disk.touch("e/h", OLD + SECOND);
    std::fs::remove_file(disk.at("d/l")).unwrap();
    disk.symlink("d/l", "../abc");
    std::fs::remove_file(disk.at("top")).unwrap();
    disk.dir("top");

    let (events, _) = scan(&disk.folder(), &index(&engine));
    let changed: BTreeMap<RelPath, ScanState> = reports(&events)
        .into_iter()
        .filter(|(_, state)| *state != ScanState::Unchanged)
        .collect();
    assert_eq!(
        changed,
        BTreeMap::from([
            (p("d/f"), observed_file(b"eff", OLD, true)),
            (p("d/l"), observed_link("../abc")),
            (p("e/h"), observed_file(b"aitch", OLD + SECOND, true)),
            (p("top"), observed_dir()),
        ])
    );
}

/// The exec bit (§7.1) is the owner's execute bit, as git reads it: the
/// group's and others' do not make a file executable, and the owner's
/// alone does.
#[test]
fn the_exec_bit_is_the_owners() {
    let disk = Disk::new();
    let modes = [
        ("owner", 0o700),
        ("others", 0o655),
        ("group", 0o610),
        ("none", 0o644),
    ];
    let fs = disk.folder().open().unwrap();
    for (name, mode) in modes {
        disk.file(name, b"x", OLD, false);
        fs.set_mode(Path::new(name), mode).unwrap();
    }
    let (events, _) = scan(&disk.folder(), &BTreeMap::new());
    let exec: Vec<(String, bool)> = reports(&events)
        .into_iter()
        .map(|(path, state)| match state {
            ScanState::Observed(seen) => (path.to_string(), seen.exec),
            other => panic!("{path}: {other:?}"),
        })
        .collect();
    assert_eq!(
        exec,
        [
            ("group".to_string(), false),
            ("none".to_string(), false),
            ("others".to_string(), false),
            ("owner".to_string(), true),
        ]
    );
}

#[test]
fn names_on_disk_map_to_index_paths() {
    let disk = Disk::new();
    // "café" as a decomposed name: its index path is NFC, and the pair is
    // kept, for its children too.
    disk.dir("cafe\u{301}");
    disk.file("cafe\u{301}/menu", b"m", OLD, false);
    let (events, report) = scan(&disk.folder(), &BTreeMap::new());
    assert_eq!(
        reports(&events),
        [
            (p("café"), observed_dir()),
            (p("café/menu"), observed_file(b"m", OLD, false)),
        ]
    );
    assert_eq!(
        report.disk_names,
        [DiskName {
            path: p("café"),
            name: "cafe\u{301}".into(),
        }]
    );

    // Linux keeps both forms side by side, and bytes that are not UTF-8.
    // Two names that coincide are skipped once at their index path, and
    // the walk goes into neither; a name with no index path is not
    // reported at all. APFS refuses both.
    if cfg!(target_os = "linux") {
        disk.dir("café");
        disk.file("café/other", b"o", OLD, false);
        std::fs::write(disk.root().join(OsStr::from_bytes(b"caf\xe9")), "x").unwrap();
        let (events, report) = scan(&disk.folder(), &BTreeMap::new());
        assert_eq!(
            reports(&events),
            [(p("café"), skip(SkipReason::CoincidingNames))]
        );
        let coincides = Unobservable::Coincides { path: p("café") };
        assert_eq!(
            report.unobservable,
            [
                (PathBuf::from("cafe\u{301}"), coincides.clone()),
                (PathBuf::from("café"), coincides),
                (
                    PathBuf::from(OsStr::from_bytes(b"caf\xe9")),
                    Unobservable::NotUtf8
                ),
            ]
        );
        assert_eq!(
            report.skipped,
            BTreeMap::from([(SkipReason::CoincidingNames, 1)])
        );
        assert!(report.disk_names.is_empty());
    }
}

#[test]
fn a_fifo_or_a_socket_is_not_an_entry() {
    let disk = Disk::new();
    disk.file("f", b"x", OLD, false);
    let made = std::process::Command::new("mkfifo")
        .arg(disk.at("fifo"))
        .status();
    assert!(made.unwrap().success());
    let _socket = std::os::unix::net::UnixListener::bind(disk.at("sock")).unwrap();
    let (events, _) = scan(&disk.folder(), &BTreeMap::new());
    assert_eq!(
        reports(&events),
        [(p("f"), observed_file(b"x", OLD, false))]
    );
}

#[test]
fn ignored_paths_are_left_alone_and_tracked_ones_are_frozen() {
    let disk = Disk::new();
    disk.dir("build");
    disk.file("build/out.o", b"o", OLD, false);
    disk.file("a.log", b"log", OLD, false);
    disk.file("keep.log", b"keep", OLD, false);
    disk.file("notes", b"n", OLD, false);
    disk.file(".DS_Store", b"ds", OLD, false);

    // Tracked while nothing but the defaults ignore them.
    let mut engine = engine();
    let (events, _) = scan(&disk.folder(), &BTreeMap::new());
    let paths: Vec<RelPath> = reports(&events).into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        paths,
        [
            p("a.log"),
            p("build"),
            p("build/out.o"),
            p("keep.log"),
            p("notes")
        ]
    );
    feed(&mut engine, events);

    // A rule arrives: what it ignores is reported Skipped after the walk,
    // and nothing is tombstoned.
    disk.file(
        ignore_rules::IGNORE_FILE,
        b"*.log\n!keep.log\nbuild/\n",
        OLD,
        false,
    );
    let (events, _) = scan(&disk.folder(), &index(&engine));
    assert_eq!(
        reports(&events),
        [
            (
                p(".delocalignore"),
                observed_file(b"*.log\n!keep.log\nbuild/\n", OLD, false)
            ),
            (p("keep.log"), ScanState::Unchanged),
            (p("notes"), ScanState::Unchanged),
            (p("a.log"), skip(SkipReason::Ignored)),
            (p("build"), skip(SkipReason::Ignored)),
            (p("build/out.o"), skip(SkipReason::Ignored)),
        ]
    );
    let (_, tombstoned) = written(&feed(&mut engine, events));
    assert!(tombstoned.is_empty());

    // While ignored, a tracked path is frozen even if it goes: its
    // deletion must not read as one.
    std::fs::remove_file(disk.at("a.log")).unwrap();
    std::fs::remove_dir_all(disk.at("build")).unwrap();
    let (events, _) = scan(&disk.folder(), &index(&engine));
    let (_, tombstoned) = written(&feed(&mut engine, events));
    assert!(tombstoned.is_empty());
    assert!(live(&engine).contains_key(&p("build/out.o")));

    // Once the rule goes, the next bracket sees the paths as they are.
    std::fs::remove_file(disk.at(ignore_rules::IGNORE_FILE)).unwrap();
    let (events, _) = scan(&disk.folder(), &index(&engine));
    let (_, tombstoned) = written(&feed(&mut engine, events));
    assert_eq!(
        tombstoned,
        [
            p(".delocalignore"),
            p("a.log"),
            p("build"),
            p("build/out.o")
        ]
    );
}

/// Damage done to a tracked tree before a scan, which must abort before
/// observing anything: the root guard's cases.
fn guarded(damage: impl FnOnce(&Disk), why: fn(&Abort) -> bool) {
    let (disk, mut engine) = tracked_tree();
    let before = live(&engine);
    damage(&disk);
    let (events, report) = scan(&disk.folder(), &index(&engine));
    assert!(aborted_at_once(&events), "{events:?}");
    let abort = report.aborted.expect("aborted");
    assert!(why(&abort), "{abort:?}");
    assert_eq!(written(&feed(&mut engine, events)), (vec![], vec![]));
    assert_eq!(live(&engine), before);
}

#[test]
fn a_missing_marker_aborts_the_scan_before_anything_is_observed() {
    let delocal = |disk: &Disk| disk.root().join(RESERVED_DIR);
    guarded(
        |disk| std::fs::remove_file(delocal(disk).join(MARKER)).unwrap(),
        |abort| matches!(abort, Abort::Marker(e) if e.kind() == io::ErrorKind::NotFound),
    );
    guarded(
        |disk| {
            std::fs::remove_file(delocal(disk).join(MARKER)).unwrap();
            std::fs::create_dir(delocal(disk).join(MARKER)).unwrap();
        },
        |abort| matches!(abort, Abort::Marker(e) if NotAFile::of(e).is_some()),
    );
    // A `.delocal` that is a symlink to a directory holding a marker is not
    // followed.
    guarded(
        |disk| {
            let elsewhere = disk.root().join("elsewhere");
            std::fs::rename(delocal(disk), &elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, delocal(disk)).unwrap();
        },
        |abort| matches!(abort, Abort::Marker(e) if e.kind() == io::ErrorKind::NotADirectory),
    );
    // The folder itself is gone, or was never mounted.
    guarded(
        |disk| {
            for entry in std::fs::read_dir(disk.root()).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() && !path.is_symlink() {
                    std::fs::remove_dir_all(path).unwrap();
                } else {
                    std::fs::remove_file(path).unwrap();
                }
            }
            std::fs::remove_dir(disk.root()).unwrap();
        },
        |abort| matches!(abort, Abort::Root(e) if e.kind() == io::ErrorKind::NotFound),
    );
    // `.delocalignore` cannot be read, so which paths are ignored is
    // unknown.
    guarded(
        |disk| {
            std::fs::remove_file(disk.at(ignore_rules::IGNORE_FILE)).unwrap();
            std::fs::create_dir(disk.at(ignore_rules::IGNORE_FILE)).unwrap();
        },
        |abort| matches!(abort, Abort::IgnoreFile(e) if NotAFile::of(e).is_some()),
    );
}

#[test]
fn a_folder_deleted_while_it_is_scanned_is_not_a_mass_deletion() {
    let (disk, mut engine) = tracked_tree();
    let root = disk.root().to_path_buf();
    let mut deleted = false;
    let mut events = Vec::new();
    let clock = || at(NOW);
    let records = index(&engine);
    let folder = disk.folder();
    let report = Scan {
        id: id(),
        folder: &folder,
        records: &records,
        clock: &clock,
        hashers: NonZeroUsize::MIN,
    }
    .run(&mut |event| {
        // At the first report, everything goes, `.delocal` with it.
        if matches!(event, Event::Scanned { .. }) && !deleted {
            deleted = true;
            for entry in std::fs::read_dir(&root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    std::fs::remove_dir_all(path).unwrap();
                } else {
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
        events.push(event);
    });
    assert!(deleted);
    assert!(
        matches!(report.aborted, Some(Abort::Marker(_))),
        "{report:?}"
    );
    assert_eq!(events.last(), Some(&Event::ScanAborted { folder: id() }));
    let (_, tombstoned) = written(&feed(&mut engine, events));
    assert!(tombstoned.is_empty());
    assert_eq!(live(&engine).len(), 9);
}

#[test]
fn a_directory_that_cannot_be_listed_is_skipped_once_for_all_beneath() {
    let (disk, mut engine) = tracked_tree();
    let before = live(&engine);
    // Modes are set with `std`: the layer's `set_mode` needs a descriptor
    // it can read through, which a mode of 000 refuses (§7.3).
    let chmod = |path: &str, mode: u32| {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(disk.at(path), permissions).unwrap();
    };
    let unlisted = |events: &[Event]| -> Vec<(RelPath, ScanState)> {
        reports(events)
            .into_iter()
            .filter(|(path, _)| path.as_str().starts_with('d'))
            .collect()
    };

    // Only runs where permissions mean something: not as root.
    chmod("d", 0o000);
    let denied = std::fs::read_dir(disk.at("d")).is_err();
    let (events, report) = scan(&disk.folder(), &index(&engine));
    chmod("d", 0o755);
    if denied {
        assert_eq!(
            unlisted(&events),
            [(p("d"), skip(SkipReason::PermissionDenied))]
        );
        assert_eq!(
            report.skipped,
            BTreeMap::from([(SkipReason::PermissionDenied, 1)])
        );
        assert_eq!(written(&feed(&mut engine, events)), (vec![], vec![]));
        assert_eq!(live(&engine), before);
    }

    // A directory that can be listed but not entered: each name in it is
    // skipped, and still nothing is tombstoned.
    chmod("d", 0o444);
    let denied = std::fs::symlink_metadata(disk.at("d/f")).is_err();
    let (events, _) = scan(&disk.folder(), &index(&engine));
    chmod("d", 0o755);
    if denied {
        let denied = skip(SkipReason::PermissionDenied);
        assert_eq!(
            unlisted(&events),
            [
                (p("d"), ScanState::Unchanged),
                (p("d/f"), denied.clone()),
                (p("d/l"), denied.clone()),
                (p("d/sub"), denied),
            ]
        );
        assert_eq!(written(&feed(&mut engine, events)), (vec![], vec![]));
        assert_eq!(live(&engine), before);
    }

    // A file that cannot be read, once it needs hashing.
    disk.touch("e/h", OLD + SECOND);
    chmod("e/h", 0o000);
    let denied = std::fs::read(disk.at("e/h")).is_err();
    let (events, _) = scan(&disk.folder(), &index(&engine));
    chmod("e/h", 0o755);
    if denied {
        assert!(reports(&events).contains(&(p("e/h"), skip(SkipReason::PermissionDenied))));
        assert_eq!(written(&feed(&mut engine, events)), (vec![], vec![]));
    }
}

#[test]
fn a_file_is_hashed_only_once_its_mtime_has_settled() {
    let disk = Disk::new();
    disk.file("recent", b"r", NOW - SECOND, false);
    disk.file("ahead", b"a", NOW + 60 * SECOND, false);
    let spy = Spy::new(disk.root());

    let (events, report) = scan(&spy, &BTreeMap::new());
    assert_eq!(
        reports(&events),
        [
            (p("ahead"), skip(SkipReason::Unstable)),
            (p("recent"), skip(SkipReason::Unstable)),
        ]
    );
    assert_eq!(report.skipped, BTreeMap::from([(SkipReason::Unstable, 2)]));
    let opened = spy.calls("dir.open_read");
    assert_eq!(
        opened,
        [PathBuf::from(ignore_rules::IGNORE_FILE)],
        "neither file even opened"
    );

    // Two seconds after its mtime, it is hashed; the one from the future
    // waits until the clock passes it.
    let (events, _) = scan_at(&spy, &BTreeMap::new(), NOW + SECOND, 1);
    assert_eq!(
        reports(&events),
        [
            (p("ahead"), skip(SkipReason::Unstable)),
            (p("recent"), observed_file(b"r", NOW - SECOND, false)),
        ]
    );
}

/// A file changed while it is hashed, in each way the re-stat can see, is
/// `Skipped { Unstable }`: its record stands, no torn hash is recorded, and
/// the next scan sees it as it now is.
#[test]
fn a_file_changed_while_it_is_hashed_is_unstable() {
    let size = 2 * hash::CHUNK + 5;
    /// A change made to the file at a path, mid-hash.
    type Change = fn(&Path);
    let cases: [(&str, Change); 3] = [
        // Appended to: size and mtime move.
        ("appended", |path| {
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            std::io::Write::write_all(&mut file, b"more").unwrap();
        }),
        // Rewritten in place, and its mtime put back: only the change time
        // moves.
        ("rewritten", |path| {
            let mtime = std::fs::symlink_metadata(path).unwrap().modified().unwrap();
            let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            std::io::Write::write_all(&mut file, &vec![0xee; 2 * hash::CHUNK + 5]).unwrap();
            file.set_modified(mtime).unwrap();
        }),
        // Replaced by a rename with a file of the same size and mtime: only
        // the inode moves.
        ("swapped", |path| {
            let mtime = std::fs::symlink_metadata(path).unwrap().modified().unwrap();
            let other = path.with_extension("new");
            std::fs::write(&other, vec![0xdd; 2 * hash::CHUNK + 5]).unwrap();
            let file = std::fs::File::options().write(true).open(&other).unwrap();
            file.set_modified(mtime).unwrap();
            std::fs::rename(&other, path).unwrap();
        }),
    ];
    for (name, change) in cases {
        let (disk, mut engine) = tracked_tree();
        disk.file("big", &vec![1; size], OLD, false);
        let (events, _) = scan(&disk.folder(), &index(&engine));
        feed(&mut engine, events);
        let recorded = live(&engine)[&p("big")].clone();

        // Touched, so the next scan must hash it; the change time has to
        // move on a coarse clock before the change is made.
        disk.touch("big", OLD + SECOND);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let spy = Spy::new(disk.root());
        let path = disk.at("big");
        spy.meddle("big", move || change(&path));
        let (events, _) = scan(&spy, &index(&engine));
        assert!(spy.meddled(), "{name}: the change was made mid-hash");
        let found: Vec<ScanState> = reports(&events)
            .into_iter()
            .filter(|(p, _)| p.as_str() == "big")
            .map(|(_, s)| s)
            .collect();
        assert_eq!(found, [skip(SkipReason::Unstable)], "{name}");
        let (written, _) = written(&feed(&mut engine, events));
        assert!(!written.contains(&p("big")), "{name}: the record stands");
        assert_eq!(live(&engine)[&p("big")], recorded, "{name}");

        // The next scan sees it as it is now. The append's mtime is the
        // real clock's, so this scan's clock is later than any.
        let now = std::fs::read(disk.at("big")).unwrap();
        let (events, _) = scan_at(&disk.folder(), &index(&engine), i64::MAX, 4);
        let state = reports(&events)
            .into_iter()
            .find(|(p, _)| p.as_str() == "big")
            .map(|(_, s)| s);
        match state {
            Some(ScanState::Observed(seen)) => assert_eq!(seen.hash, blake3_of(&now), "{name}"),
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn the_walk_opens_each_directory_once() {
    let disk = Disk::new();
    // A deep chain, a wide directory, files and symlinks everywhere.
    let mut deep = String::from("deep");
    for _ in 0..100 {
        disk.dir(&deep);
        disk.file(&format!("{deep}/f"), b"f", OLD, false);
        deep.push_str("/d");
    }
    for i in 0..50 {
        disk.dir(&format!("wide/w{i:02}"));
        disk.file(&format!("wide/w{i:02}/f"), b"f", OLD, false);
        disk.symlink(&format!("wide/w{i:02}/l"), "f");
    }
    let spy = Spy::new(disk.root());
    let (events, _) = scan(&spy, &BTreeMap::new());
    assert!(finished(&events));

    let mut dirs: Vec<PathBuf> = reports(&events)
        .into_iter()
        .filter(|(_, state)| *state == observed_dir())
        .map(|(path, _)| PathBuf::from(path.as_str()))
        .collect();
    assert_eq!(dirs.len(), 100 + 51);
    // The root, and `.delocal` for the marker, are held too.
    dirs.push(PathBuf::new());
    dirs.push(PathBuf::from(RESERVED_DIR));
    dirs.sort();
    let mut opened: Vec<PathBuf> = spy.calls("fs.open_dir");
    opened.extend(spy.calls("dir.open_dir"));
    opened.sort();
    assert_eq!(opened, dirs, "each directory opened exactly once");
    assert_eq!(spy.calls("open_root"), [PathBuf::new()]);
    // Nothing reached by a whole path: the root is held as it is, and
    // everything else is reached from the directory it is in.
    assert_eq!(spy.calls("fs.open_dir"), [PathBuf::new()]);
    let by_path: Vec<_> = spy
        .all()
        .into_iter()
        .filter(|(op, _)| op.starts_with("fs.") && *op != "fs.open_dir")
        .collect();
    assert!(by_path.is_empty(), "{by_path:?}");
    // And each directory listed once.
    let mut listed = spy.calls("dir.read_dir");
    listed.sort();
    let mut walked: Vec<PathBuf> = dirs
        .into_iter()
        .filter(|d| d.as_os_str() != RESERVED_DIR)
        .collect();
    walked.sort();
    assert_eq!(listed, walked);
}

#[cfg(feature = "faults")]
mod faults {
    use proptest::prelude::*;

    use super::*;
    use crate::fs::FaultyFolder;
    use crate::fs::faulty::{Fault, Op, Rule, Spec, Trigger};

    /// Every path of [`tracked_tree`], the root, `.delocal` and its marker,
    /// and everything.
    const PATTERNS: [&str; 13] = [
        "",
        ".delocal",
        ".delocal/folder.json",
        ".delocalignore",
        "d",
        "d/f",
        "d/l",
        "d/sub",
        "d/sub/g",
        "e",
        "e/h",
        "top",
        "**",
    ];

    const OPS: [Op; 7] = [
        Op::OpenRoot,
        Op::OpenDir,
        Op::ReadDir,
        Op::Lstat,
        Op::ReadLink,
        Op::OpenRead,
        Op::Read,
    ];

    /// The tree, tracked, then touched so every file must be hashed again.
    fn touched_tree() -> (Disk, Engine) {
        let (disk, engine) = tracked_tree();
        for file in ["d/f", "d/sub/g", "e/h", "top", ".delocalignore"] {
            disk.touch(file, OLD + SECOND);
        }
        (disk, engine)
    }

    /// Scan with `rules` injected. Nothing the engine holds live may be
    /// taken away, whatever fails: the live paths are the same after.
    /// Returns how many faults fired.
    fn no_live_record_lost(disk: &Disk, tracked: &Engine, rules: Vec<Rule>) -> usize {
        let mut engine = tracked.clone();
        let before: Vec<RelPath> = live(&engine).into_keys().collect();
        let folder = FaultyFolder::new(
            disk.folder(),
            Spec {
                rules: rules.clone(),
            },
        )
        .unwrap();
        let (events, report) = scan(&folder, &index(&engine));
        let (_, tombstoned) = written(&feed(&mut engine, events));
        assert!(tombstoned.is_empty(), "{rules:?} tombstoned {tombstoned:?}");
        let after: Vec<RelPath> = live(&engine).into_keys().collect();
        assert_eq!(after, before, "{rules:?} ({report:?})");
        folder.injected().len()
    }

    /// EIO or EACCES on any read, listing or open, at any path, once or
    /// from then on: every live record stays live.
    #[test]
    fn a_fault_on_any_call_never_takes_a_live_record_away() {
        let (disk, engine) = touched_tree();
        let mut fired: std::collections::HashMap<Op, usize> = Default::default();
        for op in OPS {
            for fail in [Fault::Eio, Fault::Eacces] {
                for path in PATTERNS {
                    for at in [Trigger::Call(1), Trigger::FromCall(1), Trigger::Call(2)] {
                        let rule = Rule {
                            op,
                            path: path.into(),
                            at,
                            fail,
                        };
                        *fired.entry(op).or_default() +=
                            no_live_record_lost(&disk, &engine, vec![rule]);
                    }
                }
            }
        }
        // Every kind of call was reached, so every fault was really tried.
        for op in OPS {
            assert!(
                fired.get(&op).copied().unwrap_or(0) > 0,
                "{op:?} never fired"
            );
        }
    }

    fn arb_rule() -> impl Strategy<Value = Rule> {
        let op = prop::sample::select(OPS.to_vec());
        let path = prop::sample::select(PATTERNS.to_vec());
        let at = prop_oneof![
            (1..6u64).prop_map(Trigger::Call),
            (1..6u64).prop_map(Trigger::FromCall),
        ];
        let fail = prop::sample::select(vec![Fault::Eio, Fault::Eacces]);
        (op, path, at, fail).prop_map(|(op, path, at, fail)| Rule {
            op,
            path: path.into(),
            at,
            fail,
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Several faults at once, anywhere: still no live record lost.
        #[test]
        fn several_faults_at_once_never_take_a_live_record_away(
            rules in prop::collection::vec(arb_rule(), 1..5),
        ) {
            let (disk, engine) = touched_tree();
            no_live_record_lost(&disk, &engine, rules);
        }
    }

    #[test]
    fn a_directory_whose_open_fails_is_skipped_once_with_its_reason() {
        let (disk, engine) = touched_tree();
        for (op, fail, reason) in [
            (Op::OpenDir, Fault::Eacces, SkipReason::PermissionDenied),
            (Op::ReadDir, Fault::Eio, SkipReason::Io),
        ] {
            let rule = Rule {
                op,
                path: "d".into(),
                at: Trigger::Call(1),
                fail,
            };
            let folder = FaultyFolder::new(disk.folder(), Spec { rules: vec![rule] }).unwrap();
            let (events, _) = scan(&folder, &index(&engine));
            let under_d: Vec<(RelPath, ScanState)> = reports(&events)
                .into_iter()
                .filter(|(path, _)| path.as_str().starts_with('d'))
                .collect();
            assert_eq!(under_d, [(p("d"), skip(reason))], "{op:?}");
        }
    }

    #[test]
    fn a_root_that_cannot_be_opened_or_listed_aborts_the_scan() {
        let (disk, mut engine) = touched_tree();
        for (op, path) in [(Op::OpenRoot, ""), (Op::OpenDir, ""), (Op::ReadDir, "")] {
            let rule = Rule {
                op,
                path: path.into(),
                at: Trigger::Call(1),
                fail: Fault::Eio,
            };
            let folder = FaultyFolder::new(disk.folder(), Spec { rules: vec![rule] }).unwrap();
            let (events, report) = scan(&folder, &index(&engine));
            assert!(aborted_at_once(&events), "{op:?}");
            let listing = op == Op::ReadDir;
            assert_eq!(
                matches!(report.aborted, Some(Abort::List(_))),
                listing,
                "{op:?}: {report:?}"
            );
            assert_eq!(written(&feed(&mut engine, events)), (vec![], vec![]));
        }
    }
}
