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

use delocal_engine::{Entry, Event, IndexRecord, Kind, Timestamp};

use super::super::clock::Clock;
use super::super::{Daemon, NoTransport, RESCAN, SETTLE, SPACE_CHECK, STORE, Setup, lock};
use super::{Fixture, batch, touch};
use crate::fs::coarse::{Coarse, coarse};
use crate::fs::{Folder, Fs, RealFolder};
use crate::store::Store;
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
