//! The mtime precision shim in the daemon (DESIGN.md §7.3), on a folder
//! whose filesystem keeps mtimes to 2 s ([`Coarse`]): received files
//! survive a hundred scans and a `kill -9` without gaining a new version, a
//! real edit still makes one, and a commit that lands while a scan runs is
//! not read as a touch by that scan.
//!
//! A file reaches this daemon the only way Phase 2 lets one: a peer's touch
//! of a file it holds, applied as a metadata-only apply (§7.5), which sets
//! the mtime. A received file's content needs a fetch, which the transport
//! brings in Phase 3; the commit path's write is tested with a temp file in
//! `commit::tests::shim`.

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use delocal_engine::{Entry, Event, IndexRecord, Kind, RelPath, Rules, Timestamp};

use super::super::clock::Clock;
use super::super::core::Input;
use super::super::{Daemon, NoTransport, RESCAN, SETTLE, SPACE_CHECK, STORE, Setup, lock};
use super::{Fixture, batch, touch, windows_closed};
use crate::fs::coarse::{Coarse, coarse};
use crate::fs::{Folder, Fs, RealFolder};
use crate::store::{HostWrite, Shim, Store};
use crate::tailscale::tier;

const STATE: &str = "DELOCAL_SHIM_STATE";
const ROOT: &str = "DELOCAL_SHIM_ROOT";

/// The files the peer touches.
const FILES: usize = 5;

/// How far past a file's mtime the peer touches it: 3.234567891 s, which a
/// 2 s filesystem keeps as 2 s.
const OFFSET: i64 = 3_234_567_891;

const SECOND: i64 = 1_000_000_000;

/// The daemon on `root`, whose filesystem keeps mtimes to 2 s.
fn setup(state: &Path, root: &Path) -> Setup {
    let root = root.to_path_buf();
    Setup {
        state_dir: state.to_path_buf(),
        lock: lock::lock(state).unwrap(),
        node: Fixture::ids().node,
        clock: Arc::new(Clock::start()),
        realtime: Arc::new(crate::scan::stability::realtime as fn() -> Timestamp),
        folder: Box::new(move |_| Arc::new(Coarse::new(Arc::new(RealFolder::new(&root))))),
        outbox: Box::new(NoTransport),
        rescan: RESCAN,
        space_check: SPACE_CHECK,
        hashers: std::num::NonZeroUsize::MIN,
        tailscale: None,
        tier_poll: tier::POLL,
        // The scans are what is under test.
        watch: false,
    }
}

/// The live file records, by path.
fn files(daemon: &Daemon) -> Vec<IndexRecord> {
    let folder = Fixture::ids().folder;
    daemon.engine(move |engine| {
        engine.folder(folder).map_or_else(Vec::new, |f| {
            f.index()
                .live_records()
                .filter(|r| r.entry.kind == Kind::File)
                .cloned()
                .collect()
        })
    })
}

/// The peer's touch of `record` to [`OFFSET`] past its mtime.
fn touched(record: &IndexRecord) -> Entry {
    let peer = Fixture::ids().peer;
    Entry {
        mtime_ns: record.entry.mtime_ns + OFFSET,
        ..touch(peer, record)
    }
}

/// Run by [`received_files_survive_a_hundred_scans_and_a_kill_9`] as its
/// child, and killed by it once it prints `ready`.
#[test]
#[ignore = "run by received_files_survive_a_hundred_scans_and_a_kill_9 as its child"]
fn child() {
    let (Ok(state), Ok(root)) = (std::env::var(STATE), std::env::var(ROOT)) else {
        return;
    };
    let out = |line: String| {
        let mut stdout = io::stdout();
        writeln!(stdout, "{line}").unwrap();
        stdout.flush().unwrap();
    };
    let ids = Fixture::ids();
    let daemon = Daemon::start(setup(Path::new(&state), Path::new(&root))).unwrap();
    daemon.settle();
    let records = files(&daemon);
    if records.len() != FILES {
        out(format!("failed: {} files recorded", records.len()));
        return;
    }
    let entries: Vec<Entry> = records.iter().map(touched).collect();
    daemon.inject(Event::BatchReceived {
        from: ids.peer,
        batch: batch(ids.peer, ids.folder, 0, entries.clone()),
    });
    daemon.settle();
    for (record, entry) in files(&daemon).iter().zip(&entries) {
        if (&record.entry.version, record.entry.mtime_ns) != (&entry.version, entry.mtime_ns) {
            out(format!("failed: {} not as received", record.entry.path));
            return;
        }
    }
    // Past the settle interval first: a file changed within 2 s is skipped
    // as unstable rather than read, which would hide a missing pair.
    std::thread::sleep(SETTLE);
    for round in 0..100 {
        let changes = daemon.scan(ids.folder);
        if changes != 0 {
            out(format!("failed: scan {round} made {changes} index changes"));
            return;
        }
    }
    // Every second look the scans asked for, done.
    daemon.settle();
    if files(&daemon)
        .iter()
        .map(|r| &r.entry.version)
        .ne(entries.iter().map(|e| &e.version))
    {
        out("failed: a version changed after the scans".to_owned());
        return;
    }
    out("ready".to_owned());
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// §7.3, §13: a daemon on a 2 s filesystem receives a touch of every file
/// it holds, to mtimes the filesystem cannot keep, and a hundred scans make
/// no index change. It is killed with `SIGKILL`. Its store holds a pair
/// for each file. The daemon started again from what it left has every
/// record as it was, and a hundred scans still change nothing. A real edit
/// then makes a new local version of that file, and of no other.
#[test]
fn received_files_survive_a_hundred_scans_and_a_kill_9() {
    let fixture = Fixture::new();
    for n in 0..FILES {
        std::fs::write(fixture.at(&format!("f{n}")), format!("file {n}")).unwrap();
    }
    let stderr = std::fs::File::create(fixture.dir.path().join("child.err")).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "daemon::tests::shim::child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(STATE, fixture.state())
        .env(ROOT, fixture.root())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (lines, said) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // libtest prints `test ... ` and no newline before the test's own
        // output, so a line can start with that.
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("ready") || line.contains("failed: ") {
                let _ = lines.send(line);
            }
        }
    });
    let line = said.recv_timeout(Duration::from_secs(300));
    child.kill().unwrap();
    child.wait().unwrap();
    let err = std::fs::read_to_string(fixture.dir.path().join("child.err")).unwrap_or_default();
    match line {
        Ok(line) if line.ends_with("ready") => {}
        other => panic!("the child: {other:?}\n{err}"),
    }

    // What the killed daemon left: every file received, and its pair.
    let store = Store::open(&fixture.state().join(STORE)).unwrap();
    let stored = store.load().unwrap().remove(0).records;
    let shims = store.shims(fixture.folder).unwrap();
    drop(store);
    assert_eq!(shims.len(), FILES, "{shims:?}");
    for shim in &shims {
        let record = &stored[&shim.path].entry;
        assert_eq!(shim.requested_ns, record.mtime_ns);
        assert_eq!(shim.stored_ns, coarse(record.mtime_ns));
        assert_ne!(shim.stored_ns, shim.requested_ns);
    }

    let daemon = Daemon::start(setup(&fixture.state(), &fixture.root())).unwrap();
    daemon.settle();
    let restarted = |daemon: &Daemon| -> Vec<IndexRecord> {
        let mut records = files(daemon);
        records.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
        records
    };
    let before: Vec<&IndexRecord> = stored
        .values()
        .filter(|r| r.entry.kind == Kind::File)
        .collect();
    let before: Vec<IndexRecord> = before.into_iter().cloned().collect();
    assert_eq!(restarted(&daemon), before);
    for round in 0..100 {
        assert_eq!(
            daemon.scan(fixture.folder),
            0,
            "scan {round} after the kill"
        );
    }
    daemon.settle();
    assert_eq!(restarted(&daemon), before);

    // A real edit: a new local version of that file, and only that one.
    // It changes the size. A same-size edit within the same 2 s as the
    // mtime the filesystem stored cannot be seen on such a filesystem, shim
    // or no shim, and this test runs fast enough to make one.
    std::fs::write(fixture.at("f0"), "edited, and longer").unwrap();
    daemon.scan(fixture.folder);
    daemon.settle();
    let after = restarted(&daemon);
    let node = fixture.node.node_id;
    for (old, new) in before.iter().zip(&after) {
        if new.entry.path.as_str() == "f0" {
            assert!(
                new.entry.version.counter(node) > old.entry.version.counter(node),
                "{new:?}"
            );
            assert_ne!(new.entry.hash, old.entry.hash);
        } else {
            assert_eq!(new, old);
        }
    }

    // A deletion drops the pair of a file the restart read from the store.
    std::fs::remove_file(fixture.at("f1")).unwrap();
    daemon.scan(fixture.folder);
    daemon.settle();
    let paths: Vec<String> = stored_pairs(&fixture)
        .iter()
        .map(|shim| shim.path.to_string())
        .collect();
    assert_eq!(paths, ["f0", "f2", "f3", "f4"]);
    daemon.stop().unwrap();
}

/// A folder whose scans wait at a gate: the first scan to open the folder
/// after [`Gate::arm`] waits there until [`Gate::open`], so that a test
/// can commit while a scan is under way. Only a scan's thread waits;
/// commits and observations go through.
struct Gated {
    inner: Arc<dyn Folder>,
    gate: Arc<Gate>,
}

#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GateState {
    #[default]
    Open,
    Armed,
    Holding,
}

impl Gate {
    fn arm(&self) {
        *self.state.lock().unwrap() = GateState::Armed;
    }

    /// Wait until a scan is held at the gate.
    fn holding(&self) {
        let state = self.state.lock().unwrap();
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(30), |s| *s != GateState::Holding)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "no scan came to the gate: {:?}",
            *state
        );
    }

    fn open(&self) {
        *self.state.lock().unwrap() = GateState::Open;
        self.changed.notify_all();
    }
}

impl Folder for Gated {
    fn open(&self) -> io::Result<Box<dyn Fs>> {
        if std::thread::current().name() == Some("delocal-scan") {
            let mut state = self.gate.state.lock().unwrap();
            if *state == GateState::Armed {
                *state = GateState::Holding;
                self.gate.changed.notify_all();
                while *state == GateState::Holding {
                    state = self.gate.changed.wait(state).unwrap();
                }
            }
        }
        self.inner.open()
    }
}

/// The real-time clock an hour ahead: every file has settled, so a scan
/// hashes what it finds rather than skipping it as just changed (§7.3).
fn an_hour_ahead() -> Timestamp {
    let now = crate::scan::stability::realtime();
    Timestamp::from_unix_nanos(now.as_unix_nanos() + 3600 * SECOND)
}

/// §7.3: a scan works from the pairs as they were when it started. A
/// peer's touch lands while it runs and puts the path's pair; the scan then
/// reaches the path and finds the mtime the filesystem stored, which its
/// copy of the pairs does not translate. That report is set aside and the
/// path observed again through the pairs as they are, so the scan makes no
/// index change and the record stays the peer's version.
#[test]
fn a_pair_a_commit_puts_while_a_scan_runs_is_not_read_as_a_touch() {
    let fixture = Fixture::new();
    std::fs::write(fixture.at("f"), "content").unwrap();
    let gate = Arc::new(Gate::default());
    let folder: Arc<dyn Folder> = Arc::new(Gated {
        inner: Arc::new(Coarse::new(Arc::new(RealFolder::new(fixture.root())))),
        gate: Arc::clone(&gate),
    });
    let mut setup = fixture.setup(folder, Box::new(NoTransport));
    setup.realtime = Arc::new(an_hour_ahead as fn() -> Timestamp);
    let daemon = Daemon::start(setup).unwrap();
    daemon.settle();
    let record = daemon.record(fixture.folder, "f").unwrap();
    let entry = touched(&record);

    gate.arm();
    let changes = std::thread::scope(|scope| {
        let scan = scope.spawn(|| daemon.scan(fixture.folder));
        gate.holding();
        daemon.inject(Event::BatchReceived {
            from: fixture.peer,
            batch: batch(fixture.peer, fixture.folder, 0, vec![entry.clone()]),
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        while daemon
            .record(fixture.folder, "f")
            .is_none_or(|r| r.entry.version != entry.version)
        {
            assert!(Instant::now() < deadline, "the touch never landed");
            std::thread::sleep(Duration::from_millis(10));
        }
        gate.open();
        scan.join().unwrap()
    });
    assert_eq!(changes, 0);
    daemon.settle();
    let after = daemon.record(fixture.folder, "f").unwrap();
    assert_eq!(after.entry.version, entry.version);
    assert_eq!(after.entry.mtime_ns, entry.mtime_ns);
    assert_eq!(daemon.scan(fixture.folder), 0);
    daemon.stop().unwrap();
}

/// The folder at `fixture`'s root on a 2 s filesystem.
fn coarse_folder(fixture: &Fixture) -> Arc<dyn Folder> {
    Arc::new(Coarse::new(Arc::new(RealFolder::new(fixture.root()))))
}

/// The pairs the store holds for `fixture`'s folder, read through a
/// connection of its own.
fn stored_pairs(fixture: &Fixture) -> Vec<Shim> {
    Store::open(&fixture.state().join(STORE))
        .unwrap()
        .shims(fixture.folder)
        .unwrap()
}

/// §7.3: a path whose file is deleted loses its pair. A peer's touch gives
/// `f` a pair; the user deletes it, and the scan's tombstone drops the pair
/// from the store. A file made again at the path, with the very mtime the
/// filesystem stored for the pair, is recorded with that mtime rather than
/// the one the pair was for: the executor's copy has dropped it too, since
/// that file is observed by its second look, 2 s after the scan.
#[test]
fn a_deleted_path_loses_its_pair() {
    let fixture = Fixture::new();
    std::fs::write(fixture.at("f"), "content").unwrap();
    let daemon =
        Daemon::start(fixture.setup(coarse_folder(&fixture), Box::new(NoTransport))).unwrap();
    daemon.settle();
    let entry = touched(&daemon.record(fixture.folder, "f").unwrap());
    daemon.inject(Event::BatchReceived {
        from: fixture.peer,
        batch: batch(fixture.peer, fixture.folder, 0, vec![entry.clone()]),
    });
    daemon.settle();
    let stored = coarse(entry.mtime_ns);
    let pair = Shim {
        path: entry.path.clone(),
        requested_ns: entry.mtime_ns,
        stored_ns: stored,
    };
    assert_eq!(stored_pairs(&fixture), [pair]);

    std::fs::remove_file(fixture.at("f")).unwrap();
    assert_eq!(daemon.scan(fixture.folder), 1);
    daemon.settle();
    assert!(daemon.record(fixture.folder, "f").is_none());
    assert!(stored_pairs(&fixture).is_empty());

    std::fs::write(fixture.at("f"), "content").unwrap();
    let fs = RealFolder::new(fixture.root()).open().unwrap();
    fs.set_mtime(Path::new("f"), stored).unwrap();
    daemon.scan(fixture.folder);
    daemon.settle();
    let made = daemon.record(fixture.folder, "f").unwrap();
    assert_eq!(made.entry.mtime_ns, stored);
    daemon.stop().unwrap();
}

/// §7.3, §8.3: a record `revert` removes loses its pair. The brake is set
/// to hold any deletion, and the folder pauses on two. A file added
/// meanwhile, never announced, has a pair, as a conflict copy made while
/// the folder is paused would; it is gone from disk by the revert, so the
/// revert's move to the trash finds nothing to do, and only the record's
/// removal can drop the pair.
#[test]
fn a_record_revert_removes_loses_its_pair() {
    let fixture = Fixture::new();
    for name in ["a", "b", "c"] {
        std::fs::write(fixture.at(name), name).unwrap();
    }
    let daemon =
        Daemon::start(fixture.setup(coarse_folder(&fixture), Box::new(NoTransport))).unwrap();
    daemon.settle();
    daemon.inject(Event::RulesChanged {
        folder: fixture.folder,
        rules: Rules {
            hold_count: 1,
            hold_pct: 1,
            ..Rules::default()
        },
    });
    // The startup scan's adds are announced: the deletions below count.
    windows_closed(&daemon, fixture.folder);

    std::fs::remove_file(fixture.at("a")).unwrap();
    std::fs::remove_file(fixture.at("b")).unwrap();
    std::fs::write(fixture.at("n"), "new").unwrap();
    daemon.scan(fixture.folder);
    let folder = fixture.folder;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !daemon.engine(move |e| {
        e.folder(folder).is_some_and(|f| {
            f.paused().is_some() && f.index().live(&RelPath::new("n").unwrap()).is_some()
        })
    }) {
        assert!(Instant::now() < deadline, "the folder never paused with n");
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon.settle();

    let pair = Shim {
        path: RelPath::new("n").unwrap(),
        requested_ns: 1_700_000_001_234_567_891,
        stored_ns: 1_700_000_000_000_000_000,
    };
    daemon
        .inputs
        .blocking_send(Input::Writes(vec![HostWrite::PutShim {
            folder,
            shim: pair.clone(),
        }]))
        .unwrap();
    daemon.settle();
    assert_eq!(stored_pairs(&fixture), [pair]);

    std::fs::remove_file(fixture.at("n")).unwrap();
    daemon.inject(Event::Revert { folder });
    daemon.settle();
    assert!(daemon.engine(move |e| {
        e.folder(folder)
            .is_some_and(|f| f.index().get(&RelPath::new("n").unwrap()).is_none())
    }));
    assert!(stored_pairs(&fixture).is_empty());
    daemon.stop().unwrap();
}
