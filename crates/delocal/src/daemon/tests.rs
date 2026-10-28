//! Tests for the daemon (DESIGN.md §11, §7.8, §10): its probes first.

use std::path::PathBuf;

use delocal_engine::{Engine, FolderId, FolderParts, FolderState};
use tokio::sync::oneshot;

use super::DaemonError;
use super::core::Core;
use crate::store::{Group, Store};

/// A question for the engine task, answered between events.
pub(crate) enum Probe {
    /// Whether everything handed off is done: no report queued, no job or
    /// group outstanding, no scan running, no path waiting to be looked at
    /// again.
    Idle(oneshot::Sender<bool>),
    /// What the engine task is waiting on, for a test that failed to settle.
    State(oneshot::Sender<String>),
    /// Run something on the engine as it stands.
    Engine(Box<dyn FnOnce(&Engine) + Send>),
    /// Once every group so far is durable: the store's parts, read through
    /// a connection of its own, and the engine's.
    Parts(oneshot::Sender<(Vec<FolderParts>, Vec<FolderParts>)>),
    /// A full scan of the folder: how many `IndexChanged` its events made.
    Scan(FolderId, oneshot::Sender<usize>),
    /// A job for the executor, as the engine task would hand it one.
    Job(Box<super::executor::Job>),
}

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Idle(_) => "Idle",
            Self::State(_) => "State",
            Self::Engine(_) => "Engine",
            Self::Parts(_) => "Parts",
            Self::Scan(..) => "Scan",
            Self::Job(_) => "Job",
        })
    }
}

/// The engine task's side of the probes.
pub(crate) struct Probes {
    /// Whether a report was queued behind the input being handled.
    pub queued: bool,
    db: PathBuf,
    scans: Vec<(FolderId, oneshot::Sender<usize>)>,
}

impl Probes {
    pub fn new(db: PathBuf) -> Self {
        Self {
            queued: false,
            db,
            scans: Vec::new(),
        }
    }

    /// A scan of `folder` ended with `changes` index changes.
    pub fn scanned(&mut self, folder: FolderId, changes: usize) {
        let (done, waiting) = std::mem::take(&mut self.scans)
            .into_iter()
            .partition(|(f, _)| *f == folder);
        self.scans = waiting;
        for (_, reply) in done {
            let _ = reply.send(changes);
        }
    }
}

impl Core {
    pub(crate) async fn probe(&mut self, probe: Probe) -> Result<(), DaemonError> {
        match probe {
            Probe::Idle(reply) => {
                let idle = !self.probes.queued
                    && self.busy.load(std::sync::atomic::Ordering::SeqCst) == 0
                    && self.folders.values().all(|w| !w.scanning)
                    && self.reobserve.is_empty();
                let _ = reply.send(idle);
            }
            Probe::State(reply) => {
                let scanning: Vec<_> = self.folders.iter().filter(|(_, w)| w.scanning).collect();
                let _ = reply.send(format!(
                    "queued {}, scanning {scanning:?}, reobserve {:?}, wake {:?}",
                    self.probes.queued, self.reobserve, self.wake
                ));
            }
            Probe::Engine(run) => run(&self.engine),
            Probe::Parts(reply) => {
                self.writer.submit(Group::new(self.clock.now())).await?;
                let stored = Store::open(&self.probes.db)?.load()?;
                let engine = self.engine.folders().map(FolderState::parts).collect();
                let _ = reply.send((stored, engine));
            }
            Probe::Job(job) => self.direct(*job)?,
            Probe::Scan(folder, reply) => {
                self.probes.scans.push((folder, reply));
                if self.folders.get(&folder).is_some_and(|w| !w.scanning) {
                    self.start_scan(folder)?;
                }
            }
        }
        Ok(())
    }
}

// The daemon under test, and what drives it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use delocal_engine::{
    Batch, BatchId, ContentHash, Entry, Event, HostName, IndexRecord, Kind, NodeConfig, NodeId,
    Outbound, RelPath, Rules, Summary, Timestamp,
};

use super::clock::Clock;
use super::core::{Input, Report, Source};
use super::{Daemon, FolderAt, Outbox, Setup, lock};
#[cfg(feature = "faults")]
use crate::fs::faulty::{FaultyFolder, Spec};
use crate::fs::{Folder, RealFolder};
use crate::scan::MARKER;
use crate::store::{FolderRow, HostWrite, Member, Mode};

pub fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

/// The ids every fixture uses: this machine, its one peer, the folder.
pub struct Ids {
    pub node: NodeConfig,
    pub peer: NodeId,
    pub folder: FolderId,
}

/// A state directory and a folder whose store records this machine and
/// one peer as members, with its marker in place.
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub node: NodeConfig,
    pub peer: NodeId,
    pub folder: FolderId,
}

impl Fixture {
    pub fn ids() -> Ids {
        Ids {
            node: NodeConfig {
                node_id: NodeId::from_bytes([1; 16]),
                author_host: HostName::new("here").unwrap(),
            },
            peer: NodeId::from_bytes([2; 16]),
            folder: FolderId::from_bytes([9; 16]),
        }
    }

    pub fn new() -> Self {
        let Ids { node, peer, folder } = Self::ids();
        let fixture = Self {
            dir: tempfile::tempdir().unwrap(),
            node,
            peer,
            folder,
        };
        std::fs::create_dir_all(fixture.state()).unwrap();
        std::fs::create_dir_all(fixture.root().join(".delocal")).unwrap();
        std::fs::write(fixture.root().join(".delocal").join(MARKER), "{}").unwrap();
        let mut store = Store::open(&fixture.state().join(super::STORE)).unwrap();
        let mut group = Group::new(Timestamp::from_unix_nanos(1));
        group.host(HostWrite::PutFolder(FolderRow {
            id: fixture.folder,
            name: "folder".to_owned(),
            created_by: fixture.node.node_id,
            rules: Rules::default(),
            meta_version: 0,
        }));
        for (node, path) in [
            (fixture.node.node_id, fixture.root()),
            (fixture.peer, PathBuf::from("/elsewhere")),
        ] {
            group.host(HostWrite::PutMember {
                folder: fixture.folder,
                member: Member {
                    node,
                    path,
                    mode: Mode::TwoWay,
                    joined_at: Timestamp::from_unix_nanos(1),
                },
            });
        }
        store.commit(&[group]).unwrap();
        fixture
    }

    pub fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().join("folder")
    }

    pub fn at(&self, path: &str) -> PathBuf {
        self.root().join(path)
    }

    /// The daemon's setup, on `folder`, with hourly scans.
    pub fn setup(&self, folder: Arc<dyn Folder>, outbox: Box<dyn Outbox>) -> Setup {
        let at: FolderAt = Box::new(move |_| Arc::clone(&folder));
        Setup {
            state_dir: self.state(),
            lock: lock::lock(&self.state()).unwrap(),
            node: self.node.clone(),
            clock: Arc::new(Clock::start()),
            realtime: Arc::new(crate::scan::stability::realtime as fn() -> Timestamp),
            folder: at,
            outbox,
            rescan: super::RESCAN,
            space_check: super::SPACE_CHECK,
            hashers: std::num::NonZeroUsize::MIN,
        }
    }

    /// A daemon on the real folder, with no transport.
    pub fn start(&self) -> Daemon {
        let folder: Arc<dyn Folder> = Arc::new(RealFolder::new(self.root()));
        Daemon::start(self.setup(folder, Box::new(super::NoTransport))).unwrap()
    }
}

/// Sends, kept for the test to read.
#[derive(Clone, Default)]
pub struct Sent(pub Arc<Mutex<Vec<(NodeId, Outbound)>>>);

impl Outbox for Sent {
    fn send(&mut self, to: NodeId, payload: Outbound) {
        self.0.lock().unwrap().push((to, payload));
    }

    fn fetch(&mut self, _: super::Fetch) {}
}

impl Daemon {
    fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Probe) -> T {
        let (reply, answer) = oneshot::channel();
        self.inputs
            .blocking_send(Input::Probe(make(reply)))
            .unwrap();
        answer.blocking_recv().unwrap()
    }

    /// Hand the engine an event, as if the host had observed it.
    pub fn inject(&self, event: Event) {
        self.inputs
            .blocking_send(Input::report(Report {
                event,
                writes: Vec::new(),
                compared: None,
                source: Source::Host,
            }))
            .unwrap();
    }

    pub fn idle(&self) -> bool {
        self.ask(Probe::Idle)
    }

    /// Wait until the daemon has been idle for a moment.
    pub fn settle(&self) {
        if let Err(e) = self.settled(Duration::from_secs(60)) {
            panic!("{e}");
        }
    }

    /// Wait up to `within` for the daemon to be idle for a moment.
    pub fn settled(&self, within: Duration) -> Result<(), String> {
        let deadline = Instant::now() + within;
        let mut quiet = 0;
        while quiet < 3 {
            if Instant::now() > deadline {
                let state = self.ask(Probe::State);
                return Err(format!("never idle: {} busy; {state}", self.busy()));
            }
            quiet = if self.idle() { quiet + 1 } else { 0 };
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    pub fn engine<T: Send + 'static>(&self, run: impl FnOnce(&Engine) -> T + Send + 'static) -> T {
        let (reply, answer) = std::sync::mpsc::channel();
        self.inputs
            .blocking_send(Input::Probe(Probe::Engine(Box::new(move |engine| {
                let _ = reply.send(run(engine));
            }))))
            .unwrap();
        answer.recv().unwrap()
    }

    /// The live record at `path` in `folder`, as the engine holds it.
    pub fn record(&self, folder: FolderId, path: &str) -> Option<IndexRecord> {
        let path = p(path);
        self.engine(move |engine| engine.folder(folder)?.index().live(&path).cloned())
    }

    /// The store's parts and the engine's, once everything so far is
    /// durable.
    pub fn parts(&self) -> (Vec<FolderParts>, Vec<FolderParts>) {
        self.ask(Probe::Parts)
    }

    /// Scan `folder` in full: how many index changes it made.
    pub fn scan(&self, folder: FolderId) -> usize {
        self.ask(|reply| Probe::Scan(folder, reply))
    }

    pub fn trace(&self) -> (Vec<String>, Vec<String>) {
        (self.trace.emitted_list(), self.trace.performed_list())
    }
}

/// An entry from the peer at `path`, made from `record` (or nothing).
pub fn from_peer(peer: NodeId, path: &str, record: Option<&IndexRecord>) -> Entry {
    let version = record.map_or_else(delocal_engine::Version::empty, |r| r.entry.version.clone());
    Entry {
        path: p(path),
        kind: Kind::Dir,
        size: 0,
        mtime_ns: 0,
        stamp: record.map_or(1, |r| r.entry.stamp + 1),
        exec: false,
        hash: ContentHash::EMPTY,
        prev_hash: record.map_or(ContentHash::EMPTY, |r| r.entry.hash),
        version: version.incremented(peer),
        deleted: false,
        modified_by: peer,
        author_host: HostName::new("peer").unwrap(),
    }
}

/// The peer's tombstone for `record`.
pub fn tombstone(peer: NodeId, record: &IndexRecord, at: Timestamp) -> Entry {
    Entry {
        kind: record.entry.kind,
        mtime_ns: at.as_unix_nanos(),
        deleted: true,
        ..from_peer(peer, record.entry.path.as_str(), Some(record))
    }
}

/// The peer's touch of the file `record`: its mtime a second later.
pub fn touch(peer: NodeId, record: &IndexRecord) -> Entry {
    Entry {
        kind: record.entry.kind,
        size: record.entry.size,
        mtime_ns: record.entry.mtime_ns + 1_000_000_000,
        exec: record.entry.exec,
        hash: record.entry.hash,
        ..from_peer(peer, record.entry.path.as_str(), Some(record))
    }
}

/// A directory at `path`, observed outside a scan: an event with writes.
pub fn observed_dir(folder: FolderId, path: &str) -> Event {
    Event::Scanned {
        folder,
        path: p(path),
        state: delocal_engine::ScanState::Observed(delocal_engine::Observed {
            kind: Kind::Dir,
            size: 0,
            mtime_ns: 0,
            exec: false,
            hash: ContentHash::EMPTY,
        }),
        at: Timestamp::from_unix_nanos(i64::MAX / 2),
    }
}

/// A batch from `peer` with `entries`, after `seq_low`.
pub fn batch(peer: NodeId, folder: FolderId, seq_low: u64, entries: Vec<Entry>) -> Batch {
    Batch {
        id: BatchId::from_bytes(rand::random()),
        folder,
        source: peer,
        created_at: Timestamp::from_unix_nanos(1),
        seq_low,
        seq_high: seq_low + entries.len() as u64,
        entries,
        summary: Summary::default(),
    }
}

/// A daemon starts, scans what is there, persists it, and starts again
/// from what it persisted: the store's parts are the engine's, and a scan
/// of an unchanged folder changes nothing.
#[test]
fn a_daemon_scans_persists_and_restarts_from_what_it_persisted() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.at("a/b")).unwrap();
    std::fs::write(fixture.at("a/b/f"), "one").unwrap();
    std::fs::write(fixture.at("g"), "two").unwrap();
    std::os::unix::fs::symlink("g", fixture.at("l")).unwrap();
    let daemon = fixture.start();
    daemon.settle();
    for path in ["a", "a/b", "a/b/f", "g", "l"] {
        assert!(daemon.record(fixture.folder, path).is_some(), "{path}");
    }
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    daemon.stop().unwrap();

    let daemon = fixture.start();
    daemon.settle();
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    assert_eq!(engine[0].records.len(), 5);
    assert_eq!(daemon.scan(fixture.folder), 0);
    daemon.stop().unwrap();
}

/// A second daemon on the same state directory refuses to start, before
/// it touches anything there, and the first runs on.
#[test]
fn a_second_daemon_on_the_same_state_directory_refuses_to_start() {
    let fixture = Fixture::new();
    std::fs::write(fixture.at("f"), "one").unwrap();
    let daemon = fixture.start();
    daemon.settle();
    let before = std::fs::read_dir(fixture.state()).unwrap().count();
    let second = super::run(&super::Options {
        state_dir: fixture.state(),
    });
    assert!(
        matches!(second, Err(DaemonError::Lock(lock::LockError::Held { .. }))),
        "{second:?}"
    );
    assert!(!fixture.state().join(super::node::FILE).exists());
    assert!(!fixture.state().join(super::logs::DIR).exists());
    assert_eq!(std::fs::read_dir(fixture.state()).unwrap().count(), before);
    // The first is unharmed. A file written just now is unstable to the
    // scan, and observed again 2 s later (§7.3).
    std::fs::write(fixture.at("g"), "two").unwrap();
    assert_eq!(daemon.scan(fixture.folder), 0);
    daemon.settle();
    assert!(daemon.record(fixture.folder, "g").is_some());
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    daemon.stop().unwrap();
}

/// A write lock on the store held from another connection: the writer's
/// next commit waits on it (up to the store's busy timeout) until it goes.
struct Held(rusqlite::Connection);

impl Held {
    fn on(fixture: &Fixture) -> Self {
        let conn = rusqlite::Connection::open(fixture.state().join(super::STORE)).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        Self(conn)
    }

    fn release(self) {
        self.0.execute_batch("ROLLBACK").unwrap();
    }
}

/// Wait until no batch window is open, so no tick comes while a test
/// watches.
fn windows_closed(daemon: &Daemon, folder: FolderId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while daemon.engine(move |e| e.folder(folder).is_some_and(|f| f.due().is_some())) {
        assert!(Instant::now() < deadline, "the batch window never closed");
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon.settle();
}

/// §11: no file operation and no message happens while the group holding
/// the writes that explain it is not durable. With the store's commit held
/// open, a peer's batch and its connection make commits and sends, and
/// none of them touches the disk or the network until the commit returns;
/// then every one runs, in the order the engine emitted them.
#[test]
fn with_the_store_commit_held_open_nothing_happens_until_it_returns_then_all_in_order() {
    let fixture = Fixture::new();
    std::fs::write(fixture.at("f"), "deleted by the peer").unwrap();
    std::fs::write(fixture.at("g"), "touched by the peer").unwrap();
    let spy = Arc::new(crate::fs::spy::Spy::new(&fixture.root()));
    let sent = Sent::default();
    let folder: Arc<dyn Folder> = Arc::clone(&spy) as Arc<dyn Folder>;
    let daemon = Daemon::start(fixture.setup(folder, Box::new(sent.clone()))).unwrap();
    daemon.settle();
    windows_closed(&daemon, fixture.folder);
    let f = daemon.record(fixture.folder, "f").unwrap();
    let g = daemon.record(fixture.folder, "g").unwrap();
    let (emitted_before, _) = daemon.trace();

    let held = Held::on(&fixture);
    let calls = spy.all().len();
    let entries = vec![
        from_peer(fixture.peer, "d", None),
        tombstone(fixture.peer, &f, Timestamp::from_unix_nanos(5)),
        touch(fixture.peer, &g),
    ];
    daemon.inject(Event::BatchReceived {
        from: fixture.peer,
        batch: batch(fixture.peer, fixture.folder, 0, entries),
    });
    daemon.inject(Event::PeerConnected {
        peer: fixture.peer,
        tier: delocal_engine::Tier::Lan,
    });
    std::thread::sleep(Duration::from_millis(1500));
    let (emitted, performed) = daemon.trace();
    let new: Vec<&str> = emitted[emitted_before.len()..]
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        new,
        // The engine's order: its decision, then creates before deletes
        // (§7.5), then the connection's watermark.
        [
            "send decision",
            "write d",
            "set_meta g",
            "remove f",
            "send have_up_to"
        ]
    );
    assert_eq!(performed.len(), emitted_before.len(), "{performed:?}");
    assert_eq!(spy.all().len(), calls, "{:?}", &spy.all()[calls..]);
    assert!(sent.0.lock().unwrap().is_empty());
    assert!(!fixture.at("d").exists());

    held.release();
    daemon.settle();
    let (emitted, performed) = daemon.trace();
    assert_eq!(performed, emitted);
    let sent: Vec<String> = sent
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|(to, payload)| {
            assert_eq!(*to, fixture.peer);
            match payload {
                Outbound::Batch(_) => "batch",
                Outbound::Decision(_) => "decision",
                Outbound::HaveUpTo { .. } => "have_up_to",
            }
            .to_owned()
        })
        .collect();
    assert_eq!(sent[..2], ["decision", "have_up_to"]);
    assert!(fixture.at("d").is_dir());
    assert!(!fixture.at("f").exists());
    assert!(fixture.at("g").exists());
    assert_eq!(
        daemon.record(fixture.folder, "g").unwrap().entry.mtime_ns,
        g.entry.mtime_ns + 1_000_000_000
    );
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    daemon.stop().unwrap();
}

/// §11 backpressure: at most QUEUE groups wait to be durable, and the
/// engine task waits, taking no more events, until the store catches up.
#[test]
fn the_engine_task_waits_while_the_writer_is_full() {
    let fixture = Fixture::new();
    let daemon = fixture.start();
    daemon.settle();
    windows_closed(&daemon, fixture.folder);
    let handled = daemon
        .trace
        .handled
        .load(std::sync::atomic::Ordering::SeqCst);

    let held = Held::on(&fixture);
    let more = super::QUEUE + 64;
    for n in 0..more {
        daemon.inject(observed_dir(fixture.folder, &format!("x{n}")));
    }
    std::thread::sleep(Duration::from_millis(1500));
    let waiting = daemon
        .trace
        .handled
        .load(std::sync::atomic::Ordering::SeqCst)
        - handled;
    // One group in the stuck commit, QUEUE queued behind it, and the
    // event whose group could not be queued.
    assert_eq!(waiting, super::QUEUE + 2);

    held.release();
    daemon.settle();
    let done = daemon
        .trace
        .handled
        .load(std::sync::atomic::Ordering::SeqCst)
        - handled;
    assert_eq!(done, more);
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    assert!(
        engine[0]
            .records
            .contains_key(&p(&format!("x{}", more - 1)))
    );
    daemon.stop().unwrap();
}

/// §7.4: the engine's `WakeAt` ticks it, and a tick forms the batch of what
/// the startup scan found, recorded in history though no peer is there.
#[test]
fn the_engine_is_ticked_when_it_asks_and_forms_its_batch() {
    let fixture = Fixture::new();
    std::fs::write(fixture.at("f"), "one").unwrap();
    let daemon = fixture.start();
    daemon.settle();
    windows_closed(&daemon, fixture.folder);
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    assert!(engine[0].pending.is_empty(), "{:?}", engine[0].pending);
    assert!(engine[0].rest.announced_seq > 0);
    daemon.stop().unwrap();
    let store = Store::open(&fixture.state().join(super::STORE)).unwrap();
    let history = store.history(fixture.folder).unwrap();
    assert!(
        history
            .iter()
            .any(|row| row.role == delocal_engine::BatchRole::Sent
                && row.batch.entries.iter().any(|e| e.path == p("f"))),
        "{history:?}"
    );
}

/// §7.3: a folder is scanned again after the rescan interval, without
/// being asked, and what changed meanwhile is found.
#[test]
fn a_folder_is_scanned_again_after_the_rescan_interval() {
    let fixture = Fixture::new();
    let folder: Arc<dyn Folder> = Arc::new(RealFolder::new(fixture.root()));
    let mut setup = fixture.setup(folder, Box::new(super::NoTransport));
    setup.rescan = Duration::from_millis(300);
    let daemon = Daemon::start(setup).unwrap();
    daemon.settle();
    std::fs::write(fixture.at("later"), "found by the next scan").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while daemon.record(fixture.folder, "later").is_none() {
        assert!(Instant::now() < deadline, "never scanned again");
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon.stop().unwrap();
}

/// §11: if the writer fails, the daemon stops, and performs none of the
/// effects whose writes were lost. A write lock held past the store's busy
/// timeout fails the commit holding a peer's batch, with more events
/// queued behind it than the queue to the store takes, so the engine task
/// is waiting on that queue when the failure comes; the store's error is
/// still the one the daemon stops with.
#[test]
fn if_the_writer_fails_the_daemon_stops_and_performs_nothing_more() {
    let fixture = Fixture::new();
    let spy = Arc::new(crate::fs::spy::Spy::new(&fixture.root()));
    let sent = Sent::default();
    let folder: Arc<dyn Folder> = Arc::clone(&spy) as Arc<dyn Folder>;
    let daemon = Daemon::start(fixture.setup(folder, Box::new(sent.clone()))).unwrap();
    daemon.settle();
    windows_closed(&daemon, fixture.folder);

    let held = Held::on(&fixture);
    let calls = spy.all().len();
    daemon.inject(Event::BatchReceived {
        from: fixture.peer,
        batch: batch(
            fixture.peer,
            fixture.folder,
            0,
            vec![from_peer(fixture.peer, "d", None)],
        ),
    });
    for n in 0..super::QUEUE + 64 {
        daemon.inject(observed_dir(fixture.folder, &format!("x{n}")));
    }
    let deadline = Instant::now() + crate::store::BUSY_TIMEOUT + Duration::from_secs(10);
    while !daemon.core.is_finished() {
        assert!(Instant::now() < deadline, "the daemon ran on");
        std::thread::sleep(Duration::from_millis(50));
    }
    held.release();
    assert_eq!(spy.all().len(), calls, "{:?}", &spy.all()[calls..]);
    assert!(sent.0.lock().unwrap().is_empty());
    assert!(!fixture.at("d").exists());
    let stopped = daemon.stop();
    assert!(matches!(stopped, Err(DaemonError::Write(_))), "{stopped:?}");
}

/// §11: a store written by a newer delocal is refused, untouched.
#[test]
fn a_store_from_a_newer_delocal_is_refused() {
    let fixture = Fixture::new();
    let db = fixture.state().join(super::STORE);
    rusqlite::Connection::open(&db)
        .unwrap()
        .pragma_update(None, "user_version", crate::store::SCHEMA_VERSION + 1)
        .unwrap();
    let folder: Arc<dyn Folder> = Arc::new(RealFolder::new(fixture.root()));
    let started = Daemon::start(fixture.setup(folder, Box::new(super::NoTransport)));
    assert!(
        matches!(
            started,
            Err(DaemonError::Store(
                crate::store::StoreError::UnknownSchema { .. }
            ))
        ),
        "{:?}",
        started.err()
    );
}

/// §7.3, the root guard: a folder whose disk is not there has nothing done
/// to it. Its scan aborts, no record is tombstoned, and nothing is made at
/// its path; when it is back, it is as it was.
#[test]
fn a_folder_whose_disk_is_gone_is_left_alone_until_it_is_back() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.at("dir")).unwrap();
    std::fs::write(fixture.at("dir/f"), "kept").unwrap();
    let daemon = fixture.start();
    daemon.settle();
    let records = daemon.parts().1[0].records.clone();
    daemon.stop().unwrap();

    let away = fixture.dir.path().join("unmounted");
    std::fs::rename(fixture.root(), &away).unwrap();
    std::fs::create_dir(fixture.root()).unwrap();
    let daemon = fixture.start();
    daemon.settle();
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    assert_eq!(engine[0].records, records);
    assert_eq!(std::fs::read_dir(fixture.root()).unwrap().count(), 0);
    daemon.stop().unwrap();

    std::fs::remove_dir(fixture.root()).unwrap();
    std::fs::rename(&away, fixture.root()).unwrap();
    let daemon = fixture.start();
    daemon.settle();
    assert_eq!(daemon.scan(fixture.folder), 0);
    assert_eq!(daemon.parts().1[0].records, records);
    daemon.stop().unwrap();
}

/// A folder on disk whose filesystem layer injects `spec`'s faults.
#[cfg(feature = "faults")]
fn faulty(fixture: &Fixture, spec: &str) -> Arc<FaultyFolder<RealFolder>> {
    Arc::new(
        FaultyFolder::new(RealFolder::new(fixture.root()), Spec::parse(spec).unwrap()).unwrap(),
    )
}

/// §7.5 "local failures": a commit that finds the disk full pauses the
/// folder's inbound, the host checks for space every `space_check` while it
/// is paused, and once a check finds room the want commits again.
#[cfg(feature = "faults")]
#[test]
fn a_full_disk_pauses_inbound_until_a_check_finds_space() {
    let fixture = Fixture::new();
    // The directory's temp cannot be made, and the first check finds no
    // space; the second finds what the disk has.
    let folder = faulty(
        &fixture,
        r#"{ "rules": [
            { "op": "create_dir", "path": ".delocal/tmp/dir-*", "at": { "call": 1 }, "fail": "ENOSPC" },
            { "op": "available_space", "path": "", "at": { "call": 1 }, "fail": "ENOSPC" }
        ] }"#,
    );
    let mut setup = fixture.setup(
        Arc::clone(&folder) as Arc<dyn Folder>,
        Box::new(super::NoTransport),
    );
    setup.space_check = Duration::from_millis(200);
    let daemon = Daemon::start(setup).unwrap();
    daemon.settle();
    let id = fixture.folder;
    daemon.inject(Event::BatchReceived {
        from: fixture.peer,
        batch: batch(
            fixture.peer,
            id,
            0,
            vec![from_peer(fixture.peer, "d", None)],
        ),
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !fixture.at("d").is_dir() {
        assert!(
            Instant::now() < deadline,
            "never committed: {:?}",
            folder.injected()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon.settle();
    let injected: Vec<crate::fs::faulty::Op> = folder.injected().iter().map(|i| i.op).collect();
    assert_eq!(
        injected,
        [
            crate::fs::faulty::Op::CreateDir,
            crate::fs::faulty::Op::AvailableSpace
        ]
    );
    assert!(!daemon.engine(move |e| e.folder(id).unwrap().disk_full()));
    assert!(daemon.record(id, "d").is_some());
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    daemon.stop().unwrap();
}

/// The start (§7.5, draft 65): every open journal row is resolved before
/// the first scan, and `tmp/` is cleared of every temp no want claims and
/// no row still open names. A row the undo cannot resolve stays open: its
/// path is skipped, not tombstoned, its temp is kept, and a resolve before
/// a later scan puts its file back.
#[cfg(feature = "faults")]
#[test]
fn the_start_resolves_the_journal_and_keeps_the_temps_of_rows_still_open() {
    let fixture = Fixture::new();
    std::fs::write(fixture.at("f"), "f's old content").unwrap();
    std::fs::write(fixture.at("g"), "g's old content").unwrap();
    let daemon = fixture.start();
    daemon.settle();
    daemon.stop().unwrap();

    // A crash between the displacements and the renames of two commits:
    // both files in the trash, both temps in tmp/, both rows open, and a
    // temp no one claims beside them.
    let day = fixture.at(".delocal/trash/2026-10-28");
    std::fs::create_dir_all(&day).unwrap();
    let tmp = fixture.at(crate::tmp::DIR);
    std::fs::create_dir_all(&tmp).unwrap();
    let mut group = Group::new(Timestamp::from_unix_nanos(2));
    for name in ["f", "g"] {
        std::fs::rename(fixture.at(name), day.join(name)).unwrap();
        std::fs::write(tmp.join(format!("0000000000000000-{name}")), "new").unwrap();
        group.host(HostWrite::PutJournal {
            folder: fixture.folder,
            row: crate::store::JournalRow {
                path: p(name),
                displaced_to: PathBuf::from(format!(".delocal/trash/2026-10-28/{name}")),
                temp_file: PathBuf::from(format!("{}/0000000000000000-{name}", crate::tmp::DIR)),
            },
        });
    }
    std::fs::write(tmp.join("stray"), "unclaimed").unwrap();
    Store::open(&fixture.state().join(super::STORE))
        .unwrap()
        .commit(&[group])
        .unwrap();

    // g's undo fails at the start, and again at the resolve before the
    // startup scan.
    let folder = faulty(
        &fixture,
        r#"{ "rules": [
            { "op": "rename_noreplace", "path": "g", "at": { "call": 1 }, "fail": "EIO" },
            { "op": "rename_noreplace", "path": "g", "at": { "call": 2 }, "fail": "EIO" }
        ] }"#,
    );
    let daemon = Daemon::start(fixture.setup(
        Arc::clone(&folder) as Arc<dyn Folder>,
        Box::new(super::NoTransport),
    ))
    .unwrap();
    daemon.settle();
    assert_eq!(
        std::fs::read_to_string(fixture.at("f")).unwrap(),
        "f's old content"
    );
    assert!(!fixture.at("g").exists());
    let mut temps: Vec<String> = std::fs::read_dir(&tmp)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    temps.sort();
    assert_eq!(temps, ["0000000000000000-g"]);
    // g was skipped by the startup scan, not taken for deleted.
    assert!(daemon.record(fixture.folder, "g").is_some());
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    let journal = Store::open(&fixture.state().join(super::STORE))
        .unwrap()
        .journal(fixture.folder)
        .unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].path, p("g"));

    // The next scan's resolve puts g back and drops its row and its
    // trash row.
    assert_eq!(daemon.scan(fixture.folder), 0);
    daemon.settle();
    assert_eq!(
        std::fs::read_to_string(fixture.at("g")).unwrap(),
        "g's old content"
    );
    let store = Store::open(&fixture.state().join(super::STORE)).unwrap();
    assert!(store.journal(fixture.folder).unwrap().is_empty());
    assert!(store.trash(fixture.folder).unwrap().is_empty());
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    daemon.stop().unwrap();
}

/// §8.3: the engine's `Observe { path }` is answered with the scanner's
/// single-path observation, behind the root guard: a folder whose marker is
/// gone answers `Skipped`, never `Absent`, so nothing is taken for deleted.
#[test]
fn observe_is_answered_as_a_scan_would_behind_the_root_guard() {
    let fixture = Fixture::new();
    let daemon = fixture.start();
    daemon.settle();
    let observe = |path: &str| {
        daemon.ask(|reply: oneshot::Sender<()>| {
            let _ = reply.send(());
            Probe::Job(Box::new(super::executor::Job::Observe {
                folder: fixture.folder,
                path: p(path),
            }))
        });
        daemon.settle();
    };
    std::fs::create_dir(fixture.at("o")).unwrap();
    observe("o");
    assert!(daemon.record(fixture.folder, "o").is_some());

    std::fs::remove_dir(fixture.at("o")).unwrap();
    let marker = fixture.at(".delocal").join(MARKER);
    std::fs::remove_file(&marker).unwrap();
    observe("o");
    assert!(
        daemon.record(fixture.folder, "o").is_some(),
        "taken for deleted"
    );

    std::fs::write(&marker, "{}").unwrap();
    observe("o");
    assert!(daemon.record(fixture.folder, "o").is_none());
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    daemon.stop().unwrap();
}

/// The engine task's timers come before its inputs: a queue that never
/// empties still lets the engine tick, so a batch forms on time.
#[test]
fn the_timers_fire_while_inputs_keep_coming() {
    let fixture = Fixture::new();
    std::fs::write(fixture.at("f"), "one").unwrap();
    let daemon = fixture.start();
    daemon.settle();
    let id = fixture.folder;
    assert!(daemon.engine(move |e| e.folder(id).unwrap().due().is_some()));
    let flooding = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let floods: Vec<_> = (0..2)
        .map(|_| {
            let inputs = daemon.inputs.clone();
            let flooding = Arc::clone(&flooding);
            std::thread::spawn(move || {
                while flooding.load(std::sync::atomic::Ordering::SeqCst) {
                    // Seen, and nothing more: no group, no effect.
                    let event = Event::Scanned {
                        folder: id,
                        path: p("f"),
                        state: delocal_engine::ScanState::Skipped {
                            reason: delocal_engine::SkipReason::Io,
                        },
                        at: Timestamp::from_unix_nanos(1),
                    };
                    let _ = inputs.blocking_send(Input::report(Report {
                        event,
                        writes: Vec::new(),
                        compared: None,
                        source: Source::Host,
                    }));
                }
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut ticked = false;
    while Instant::now() < deadline {
        if daemon.engine(move |e| e.folder(id).unwrap().index().announced_seq() > 0) {
            ticked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    flooding.store(false, std::sync::atomic::Ordering::SeqCst);
    for flood in floods {
        flood.join().unwrap();
    }
    assert!(ticked, "the batch never formed while inputs kept coming");
    daemon.stop().unwrap();
}

/// A stop under load is a clean stop: the engine task reaches it before
/// the executor goes, so the executor's going is never read as a failure.
#[test]
fn a_stop_under_load_is_clean() {
    let fixture = Fixture::new();
    let daemon = fixture.start();
    daemon.settle();
    let held = Held::on(&fixture);
    // More batches than the queue to the store takes, each with effects.
    for n in 0..super::QUEUE as u64 + 100 {
        let entry = from_peer(fixture.peer, &format!("d{n}"), None);
        daemon.inject(Event::BatchReceived {
            from: fixture.peer,
            batch: batch(fixture.peer, fixture.folder, n, vec![entry]),
        });
    }
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        held.release();
    });
    let stopped = daemon.stop();
    release.join().unwrap();
    assert!(stopped.is_ok(), "{stopped:?}");
}

/// A scan whose thread panics closes its bracket as aborted, so the folder
/// is scanned again rather than left with a bracket open for good.
#[test]
fn a_scan_that_panics_is_aborted_and_the_next_one_runs() {
    struct Panicky {
        inner: RealFolder,
        panic: std::sync::atomic::AtomicBool,
    }
    impl Folder for Panicky {
        fn open(&self) -> std::io::Result<Box<dyn crate::fs::Fs>> {
            assert!(
                !self.panic.load(std::sync::atomic::Ordering::SeqCst),
                "a bug in the scan"
            );
            self.inner.open()
        }
    }
    let fixture = Fixture::new();
    std::fs::write(fixture.at("f"), "one").unwrap();
    let folder = Arc::new(Panicky {
        inner: RealFolder::new(fixture.root()),
        panic: std::sync::atomic::AtomicBool::new(false),
    });
    let daemon = Daemon::start(fixture.setup(
        Arc::clone(&folder) as Arc<dyn Folder>,
        Box::new(super::NoTransport),
    ))
    .unwrap();
    daemon.settle();
    let id = fixture.folder;
    folder
        .panic
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (reply, _answer) = oneshot::channel();
    daemon
        .inputs
        .blocking_send(Input::Probe(Probe::Scan(id, reply)))
        .unwrap();
    daemon.settled(Duration::from_secs(20)).unwrap();
    assert!(!daemon.engine(move |e| e.folder(id).unwrap().scan_open()));
    assert!(daemon.record(id, "f").is_some());
    folder
        .panic
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(daemon.scan(id), 0);
    daemon.stop().unwrap();
}

/// §7.3, `disk_names` in §11: a name on disk that is not its index path's
/// is kept for the path, commits address the file through it, and a
/// commit that writes the path anew writes the index name and drops the
/// pair. A decomposed `é` stands for a name from macOS or a Linux user.
#[test]
fn names_on_disk_are_kept_and_dropped_as_the_disk_changes() {
    let fixture = Fixture::new();
    let nfd = "cafe\u{301}";
    let nfc = "caf\u{e9}";
    std::fs::write(fixture.at(nfd), "a decomposed name").unwrap();
    let daemon = fixture.start();
    daemon.settle();
    let names = |fixture: &Fixture| {
        Store::open(&fixture.state().join(super::STORE))
            .unwrap()
            .disk_names(fixture.folder)
            .unwrap()
    };
    let pairs = names(&fixture);
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    assert_eq!(pairs[0].path, p(nfc));
    assert_eq!(pairs[0].name, std::ffi::OsString::from(nfd));
    let record = daemon.record(fixture.folder, nfc).unwrap();

    // The peer puts a directory where the file is.
    daemon.inject(Event::BatchReceived {
        from: fixture.peer,
        batch: batch(
            fixture.peer,
            fixture.folder,
            0,
            vec![from_peer(fixture.peer, nfc, Some(&record))],
        ),
    });
    daemon.settle();
    assert_eq!(
        daemon.record(fixture.folder, nfc).unwrap().entry.kind,
        Kind::Dir
    );
    let listed: Vec<_> = std::fs::read_dir(fixture.root())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != ".delocal")
        .collect();
    assert_eq!(listed, [std::ffi::OsString::from(nfc)]);
    assert!(names(&fixture).is_empty());
    assert_eq!(daemon.scan(fixture.folder), 0);
    assert!(names(&fixture).is_empty());
    let (stored, engine) = daemon.parts();
    assert_eq!(stored, engine);
    daemon.stop().unwrap();
}

mod kill;
