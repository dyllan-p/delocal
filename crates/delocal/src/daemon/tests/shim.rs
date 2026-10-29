//! The mtime precision shim in the daemon (DESIGN.md §7.3), on a folder
//! whose filesystem keeps mtimes to 2 s ([`Coarse`]): a commit that lands
//! while a scan runs is not read as a touch by that scan.
//!
//! A file reaches this daemon the only way Phase 2 lets one: a peer's touch
//! of a file it holds, applied as a metadata-only apply (§7.5), which sets
//! the mtime. A received file's content needs a fetch, which the transport
//! brings in Phase 3; the commit path's write is tested with a temp file in
//! `commit::tests::shim`.

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use delocal_engine::{Entry, Event, IndexRecord, Timestamp};

use super::super::{Daemon, NoTransport};
use super::{Fixture, batch, touch};
use crate::fs::coarse::Coarse;
use crate::fs::{Folder, Fs, RealFolder};

/// How far past a file's mtime the peer touches it: 3.234567891 s, which a
/// 2 s filesystem keeps as 2 s.
const OFFSET: i64 = 3_234_567_891;

const SECOND: i64 = 1_000_000_000;

/// The peer's touch of `record` to [`OFFSET`] past its mtime.
fn touched(record: &IndexRecord) -> Entry {
    let peer = Fixture::ids().peer;
    Entry {
        mtime_ns: record.entry.mtime_ns + OFFSET,
        ..touch(peer, record)
    }
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
