//! The watcher (DESIGN.md §7.3), through the daemon: what its events
//! become, the root guard over it, and what it never looks at.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use delocal_engine::{FolderParts, Kind};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use super::super::watch::QUIET;
use super::super::{Daemon, NoTransport};
use super::{Fixture, Input};
use crate::fs::{Folder, RealFolder};
use crate::scan::MARKER;

/// A daemon on `folder` with the watcher on.
fn watching(fixture: &Fixture, folder: Arc<dyn Folder>) -> Daemon {
    let mut setup = fixture.setup(folder, Box::new(NoTransport));
    setup.watch = true;
    Daemon::start(setup).unwrap()
}

fn real(fixture: &Fixture) -> Arc<dyn Folder> {
    Arc::new(RealFolder::new(fixture.root()))
}

/// Wait for every event so far to pass the debouncer, then for the daemon
/// to settle.
fn quiet(daemon: &Daemon) {
    std::thread::sleep(QUIET + Duration::from_millis(1500));
    daemon.settle();
}

fn records(daemon: &Daemon) -> FolderParts {
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    engine.into_iter().next().unwrap()
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

/// Random edits of a folder, and of a directory beside it that things move
/// in from and out to.
struct Edits {
    root: PathBuf,
    outside: PathBuf,
    rng: StdRng,
    made: u64,
}

impl Edits {
    const DIRS: [&str; 6] = ["", "a", "a/b", "c", "c/d", "e"];

    fn dir(&mut self) -> PathBuf {
        self.root
            .join(Self::DIRS[self.rng.random_range(0..Self::DIRS.len())])
    }

    fn file(&mut self) -> PathBuf {
        let name = format!("f{}", self.rng.random_range(0..10));
        self.dir().join(name)
    }

    fn bytes(&mut self) -> Vec<u8> {
        let len = self.rng.random_range(0..2000);
        (0..len).map(|_| self.rng.random()).collect()
    }

    /// One edit. Failures are the folder being as it is: a file that is
    /// not there, a directory not empty.
    fn one(&mut self) {
        self.made += 1;
        let n = self.made;
        let roll = self.rng.random_range(0..100);
        let _ = match roll {
            0..28 => {
                let (path, bytes) = (self.file(), self.bytes());
                std::fs::create_dir_all(path.parent().unwrap_or(&self.root))
                    .and_then(|()| std::fs::write(&path, bytes))
            }
            28..36 => {
                let path = self.file();
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .and_then(|mut f| f.write_all(b"more"))
            }
            36..44 => std::fs::remove_file(self.file()),
            44..49 => {
                use std::os::unix::fs::PermissionsExt;
                let path = self.file();
                std::fs::metadata(&path).and_then(|meta| {
                    let mode = meta.permissions().mode() ^ 0o111;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                })
            }
            49..53 => std::fs::create_dir_all(self.dir()),
            53..56 => std::fs::remove_dir(self.dir()),
            56..59 => {
                let dir = self.dir();
                if dir == self.root {
                    Ok(())
                } else {
                    std::fs::remove_dir_all(dir)
                }
            }
            59..67 => {
                let (from, to) = (self.file(), self.file());
                std::fs::rename(from, to)
            }
            67..71 => {
                let (from, to) = (self.dir(), self.dir().join(format!("moved{n}")));
                if from == self.root || to.starts_with(&from) {
                    Ok(())
                } else {
                    std::fs::rename(from, to)
                }
            }
            71..74 => {
                let from = self.dir();
                if from == self.root {
                    Ok(())
                } else {
                    std::fs::rename(from, self.outside.join(format!("out{n}")))
                }
            }
            74..78 => {
                // A tree made elsewhere and moved in whole.
                let made = self.outside.join(format!("in{n}"));
                let bytes = self.bytes();
                std::fs::create_dir_all(made.join("deeper"))
                    .and_then(|()| std::fs::write(made.join("x"), &bytes))
                    .and_then(|()| std::fs::write(made.join("deeper/y"), b"y"))
                    .and_then(|()| std::fs::rename(&made, self.dir().join(format!("in{n}"))))
            }
            78..82 => {
                let path = self.file();
                let _ = std::fs::remove_file(&path);
                std::os::unix::fs::symlink(format!("target{n}"), &path)
            }
            82..88 => {
                // Gone and back at once: what FSEvents may merge.
                let (path, bytes) = (self.file(), self.bytes());
                std::fs::remove_file(&path).and_then(|()| std::fs::write(&path, bytes))
            }
            _ => {
                let path = self.file();
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.set_modified(std::time::SystemTime::now()))
            }
        };
    }
}

/// §7.3: with the watcher on, 1,000 random edits (files written, appended
/// to, deleted, chmodded, touched, renamed, deleted and written again at
/// once; directories made, removed, removed whole, renamed, moved out of
/// the folder and moved in with contents; symlinks) leave the index as the
/// disk is. A full scan afterwards makes no index change, and the watcher
/// never needed a full scan to catch up.
#[test]
fn after_a_thousand_random_edits_with_the_watcher_on_a_full_scan_changes_nothing() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    for dir in Edits::DIRS {
        std::fs::create_dir_all(fixture.at(dir)).unwrap();
    }
    write(&fixture.at("a/f0"), b"to begin with");
    let daemon = watching(&fixture, real(&fixture));
    daemon.settle();
    let mut edits = Edits {
        root: fixture.root(),
        outside,
        rng: StdRng::seed_from_u64(0x0077_6174_6368),
        made: 0,
    };
    for _ in 0..1000 {
        edits.one();
        let pause = edits.rng.random_range(0..3);
        std::thread::sleep(Duration::from_millis(pause));
    }
    quiet(&daemon);
    let rescans = daemon.trace.rescans.load(Ordering::SeqCst);
    let before = records(&daemon).records;
    let changes = daemon.scan(fixture.folder);
    if changes != 0 {
        let after = records(&daemon).records;
        let differ: Vec<String> = after
            .iter()
            .filter(|(path, record)| before.get(*path) != Some(*record))
            .map(|(path, record)| {
                format!(
                    "{path}: was {:?}, now {:?}",
                    before.get(path).map(|r| &r.entry),
                    record.entry
                )
            })
            .collect();
        panic!("a full scan made {changes} index changes: {differ:#?}");
    }
    assert_eq!(rescans, 0, "the watcher needed a full scan");
    assert_eq!(daemon.trace.guards.load(Ordering::SeqCst), 0);
    daemon.stop().unwrap();
}

/// §7.3, the root guard: `rm -rf` of the root reaches the engine as no
/// deletions, however fast the watcher is. When the folder is back, a scan
/// finds the marker and the folder is watched again.
#[test]
fn rm_rf_of_the_root_reaches_the_engine_as_no_deletions() {
    let fixture = Fixture::new();
    for d in 0..5 {
        for f in 0..40 {
            write(
                &fixture.at(&format!("d{d}/f{f}")),
                format!("{d} {f}").as_bytes(),
            );
        }
    }
    let daemon = watching(&fixture, real(&fixture));
    daemon.settle();
    let before = records(&daemon).records;
    assert_eq!(before.len(), 5 * 41);

    std::fs::remove_dir_all(fixture.root()).unwrap();
    quiet(&daemon);
    let after = records(&daemon).records;
    assert_eq!(after, before, "a deletion reached the engine");
    assert!(daemon.trace.guards.load(Ordering::SeqCst) >= 1);

    // The folder is back, empty but for its marker: the scan finds it, so
    // its deletions are real now; and once the folder is watched again,
    // and scanned again for what changed meanwhile, the watcher sees it.
    std::fs::create_dir_all(fixture.root().join(".delocal")).unwrap();
    std::fs::write(fixture.root().join(".delocal").join(MARKER), "{}").unwrap();
    assert_eq!(daemon.scan(fixture.folder), before.len());
    daemon.settle();
    write(&fixture.at("new"), b"seen by the watcher");
    quiet(&daemon);
    assert!(daemon.record(fixture.folder, "new").is_some());
    daemon.stop().unwrap();
}

/// §7.3, the root guard: with the marker gone, what the watcher finds
/// absent is dropped and the folder is paused as after an aborted scan,
/// until a scan finds the marker again.
#[test]
fn a_missing_marker_drops_what_the_watcher_finds_absent_until_a_scan_finds_it() {
    let fixture = Fixture::new();
    for name in ["f", "g", "h"] {
        write(&fixture.at(name), name.as_bytes());
    }
    let daemon = watching(&fixture, real(&fixture));
    daemon.settle();
    let marker = fixture.at(".delocal").join(MARKER);

    std::fs::remove_file(&marker).unwrap();
    std::fs::remove_file(fixture.at("f")).unwrap();
    quiet(&daemon);
    assert!(
        daemon.record(fixture.folder, "f").is_some(),
        "f taken for deleted"
    );
    assert_eq!(daemon.trace.guards.load(Ordering::SeqCst), 1);

    // The marker is back, but the folder stays paused until a scan.
    std::fs::write(&marker, "{}").unwrap();
    std::fs::remove_file(fixture.at("g")).unwrap();
    quiet(&daemon);
    assert!(daemon.record(fixture.folder, "g").is_some());

    // The scan finds the marker, and both deletions, which are real.
    assert_eq!(daemon.scan(fixture.folder), 2);
    std::fs::remove_file(fixture.at("h")).unwrap();
    quiet(&daemon);
    assert!(daemon.record(fixture.folder, "h").is_none());
    daemon.stop().unwrap();
}

/// Draft 48: the watcher never crosses onto another device. What changes
/// beneath a mount point inside the folder is never observed or reported.
#[test]
fn the_watcher_reports_nothing_beneath_a_mount_point() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.at("mnt")).unwrap();
    let spy = Arc::new(crate::fs::spy::Spy::new(&fixture.root()));
    spy.mount("mnt");
    let daemon = watching(&fixture, Arc::clone(&spy) as Arc<dyn Folder>);
    daemon.settle();
    write(&fixture.at("mnt/f"), b"on another disk");
    write(&fixture.at("mnt/d/x"), b"deeper");
    write(&fixture.at("g"), b"on this one");
    quiet(&daemon);
    assert!(daemon.record(fixture.folder, "g").is_some());
    for path in ["mnt", "mnt/f", "mnt/d", "mnt/d/x"] {
        assert!(daemon.record(fixture.folder, path).is_none(), "{path}");
    }
    daemon.stop().unwrap();
}

/// An observation caught between a creation and a removal is taken again:
/// the debouncer drops a create followed by a remove as a pair, so no
/// event follows, and the first look saw what was there for a moment.
#[test]
fn an_observation_caught_between_a_creation_and_a_removal_is_taken_again() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.at("e")).unwrap();
    let spy = Arc::new(crate::fs::spy::Spy::new(&fixture.root()));
    let daemon = watching(&fixture, Arc::clone(&spy) as Arc<dyn Folder>);
    daemon.settle();
    assert!(daemon.record(fixture.folder, "e").is_some());
    // When the watcher looks at `e` after its removal, `e` is back, for a
    // moment: a creation and a removal, which the debouncer drops.
    let e = fixture.at("e");
    spy.before("fs.lstat", "e", move || {
        std::fs::create_dir(&e).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::remove_dir(&e).unwrap();
        });
    });
    std::fs::remove_dir(fixture.at("e")).unwrap();
    quiet(&daemon);
    assert!(!fixture.at("e").exists());
    assert!(
        daemon.record(fixture.folder, "e").is_none(),
        "seen there for good"
    );
    daemon.stop().unwrap();
}

/// A full scan follows when the watcher says it lost events.
#[test]
fn lost_events_bring_a_full_scan() {
    let fixture = Fixture::new();
    let daemon = fixture.start();
    daemon.settle();
    // Unwatched here, so only a scan can find it.
    write(&fixture.at("unseen"), b"x");
    std::thread::sleep(Duration::from_millis(2100));
    daemon
        .inputs
        .blocking_send(Input::Rescan {
            folder: fixture.folder,
        })
        .unwrap();
    daemon.settle();
    assert!(daemon.record(fixture.folder, "unseen").is_some());
    assert_eq!(daemon.trace.rescans.load(Ordering::SeqCst), 1);
    daemon.stop().unwrap();
}

/// Draft 64: the watcher never reports beneath a path with an open journal
/// row. A crash left `f` displaced, and its undo keeps failing, so its row
/// stays open; a file the user writes there is not observed until the row
/// is resolved, while the rest of the folder is.
#[cfg(feature = "faults")]
#[test]
fn the_watcher_reports_nothing_beneath_an_open_journal_row() {
    use crate::fs::faulty::{FaultyFolder, Spec};
    use crate::store::{Group, HostWrite, JournalRow, Store};

    let fixture = Fixture::new();
    write(&fixture.at("f"), b"before the crash");
    let daemon = fixture.start();
    daemon.settle();
    let record = daemon.record(fixture.folder, "f").unwrap();
    daemon.stop().unwrap();

    let day = fixture.at(".delocal/trash/2026-10-28");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::rename(fixture.at("f"), day.join("f")).unwrap();
    let temp = PathBuf::from(crate::tmp::DIR).join("0000000000000000-f");
    write(&fixture.root().join(&temp), b"incoming");
    let mut group = Group::new(delocal_engine::Timestamp::from_unix_nanos(2));
    group.host(HostWrite::PutJournal {
        folder: fixture.folder,
        row: JournalRow {
            path: super::p("f"),
            displaced_to: PathBuf::from(".delocal/trash/2026-10-28/f"),
            temp_file: temp,
        },
    });
    Store::open(&fixture.state().join(super::super::STORE))
        .unwrap()
        .commit(&[group])
        .unwrap();
    let spec = r#"{ "rules": [
        { "op": "rename_noreplace", "path": "f", "at": { "from_call": 1 }, "fail": "EIO" }
    ] }"#;
    let folder = Arc::new(
        FaultyFolder::new(RealFolder::new(fixture.root()), Spec::parse(spec).unwrap()).unwrap(),
    );
    let daemon = watching(&fixture, folder);
    daemon.settle();

    write(&fixture.at("f"), b"the user's, while the row is open");
    write(&fixture.at("g"), b"elsewhere");
    quiet(&daemon);
    assert!(daemon.record(fixture.folder, "g").is_some());
    let now = daemon.record(fixture.folder, "f").unwrap();
    assert_eq!(now, record, "observed beneath an open row");
    daemon.stop().unwrap();
}

/// §7.3 **[verify]**: what FSEvents' coalescing does under
/// notify-debouncer-full, on macOS, beside what inotify does on Linux.
/// Each case's debounced events are written to the real stderr, past the
/// test harness's capture, for CI's log; nothing is asserted.
#[test]
fn what_the_debouncer_passes_on_for_each_kind_of_change() {
    use notify::RecursiveMode;
    use notify_debouncer_full::{DebounceEventResult, new_debouncer};

    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(QUIET, None, tx).unwrap();
    debouncer.watch(&root, RecursiveMode::Recursive).unwrap();
    let at = |name: &str| root.join(name);
    let settle = || std::thread::sleep(QUIET + Duration::from_millis(1500));
    let report = |case: &str| {
        let mut seen = Vec::new();
        while let Ok(result) = rx.try_recv() {
            match result {
                Ok(events) => {
                    for event in events {
                        let paths: Vec<String> = event
                            .paths
                            .iter()
                            .map(|p| p.strip_prefix(&root).unwrap_or(p).display().to_string())
                            .collect();
                        seen.push(format!("{:?} {}", event.kind, paths.join(" -> ")));
                    }
                }
                Err(errors) => seen.push(format!("errors {errors:?}")),
            }
        }
        let line = format!(
            "VERIFY {} {case}: {}\n",
            std::env::consts::OS,
            if seen.is_empty() {
                "NO EVENT".to_owned()
            } else {
                seen.join("; ")
            }
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
    };
    settle();
    report("watch start");

    std::fs::write(at("a"), b"a").unwrap();
    settle();
    report("create a");
    std::fs::remove_file(at("a")).unwrap();
    settle();
    report("delete a, made 3.5 s before");

    std::fs::write(at("b"), b"b").unwrap();
    settle();
    report("create b");
    std::fs::remove_file(at("b")).unwrap();
    std::fs::write(at("b"), b"b again").unwrap();
    settle();
    report("delete b and write it again at once");

    std::fs::write(at("c"), b"c").unwrap();
    std::fs::remove_file(at("c")).unwrap();
    settle();
    report("create c and delete it at once");

    std::fs::write(at("d"), b"d").unwrap();
    settle();
    report("create d");
    for i in 0..10 {
        std::fs::write(at("d"), format!("d {i}")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    settle();
    report("write d ten times in a second");

    std::fs::write(at("e"), b"e").unwrap();
    settle();
    report("create e");
    std::fs::rename(at("e"), at("f")).unwrap();
    settle();
    report("rename e to f");

    std::fs::create_dir_all(at("t/u")).unwrap();
    std::fs::write(at("t/u/v"), b"v").unwrap();
    settle();
    report("make t/u/v");
    std::fs::remove_dir_all(at("t")).unwrap();
    settle();
    report("remove t whole");

    // The debouncer files a rename under its destination; a directory's
    // removal drops the queues beneath it.
    std::fs::create_dir(at("g")).unwrap();
    std::fs::create_dir(at("h")).unwrap();
    settle();
    report("make g and h");
    std::fs::rename(at("g"), at("h/g")).unwrap();
    std::fs::remove_dir_all(at("h")).unwrap();
    settle();
    report("rename g into h, then remove h whole at once (does anything name g?)");
}

/// What a directory that appears with contents brings, and what one that
/// goes takes: no event names the paths inside either.
#[test]
fn a_directory_moved_in_brings_its_contents_and_one_moved_out_takes_them() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("outside");
    write(&outside.join("tree/x"), b"x");
    write(&outside.join("tree/deeper/y"), b"y");
    let daemon = watching(&fixture, real(&fixture));
    daemon.settle();
    std::fs::rename(outside.join("tree"), fixture.at("tree")).unwrap();
    quiet(&daemon);
    for path in ["tree", "tree/x", "tree/deeper", "tree/deeper/y"] {
        assert!(daemon.record(fixture.folder, path).is_some(), "{path}");
    }
    assert_eq!(
        daemon
            .record(fixture.folder, "tree/deeper")
            .unwrap()
            .entry
            .kind,
        Kind::Dir
    );

    std::fs::rename(fixture.at("tree"), outside.join("back")).unwrap();
    quiet(&daemon);
    for path in ["tree", "tree/x", "tree/deeper", "tree/deeper/y"] {
        assert!(daemon.record(fixture.folder, path).is_none(), "{path}");
    }
    assert_eq!(daemon.scan(fixture.folder), 0);
    daemon.stop().unwrap();
}
