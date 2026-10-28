//! The executor (DESIGN.md §7.5, §8.3, §7.3, §11): the one thread that
//! does what the engine asked of the disk and the network, in order.
//!
//! **Jobs** arrive on one queue, in the order the engine task released them
//! (see [`super::core`]): an effect is released only once the group holding
//! its writes is durable, so by the time a job runs here everything it
//! depends on is on disk. The executor performs them one at a time, which
//! is what "then all run in order" rests on: a commit, a send and an
//! `Observe` answer run in the order the engine emitted them, and a
//! commit's report is back with the engine task, its writes on their way to
//! the store, before anything that follows it here runs.
//!
//! - **Commits** (`Write`, `Remove`, `SetMeta`, `MoveToTrash`) run through
//!   [`Commit::run`], with the folder's journal in the store
//!   ([`StoreJournal`]). The report goes back with the host writes that
//!   belong in its group (§11).
//! - **Sends and fetches** go to the [`Outbox`], the transport. Phase 3
//!   brings one; until then [`NoTransport`] drops them, and there is no peer
//!   to connect anyway.
//! - **`Observe { path }`** is answered with [`Commit::observe`], the
//!   scanner's single-path observation behind the root guard (§8.3).
//! - **A scan** first resolves every open journal row it can
//!   ([`Commit::resolve`], draft 64), here, between commits, since both
//!   move files; then it runs on a thread of its own, so a long scan never
//!   holds up a commit (§7.3's stale rule is what makes the two safe
//!   together).
//! - **Space checks** read the free space of a folder whose inbound a full
//!   disk paused (§7.5), and report `SpaceRecovered` once there is room.
//!
//! **What the executor knows of the index** is its own copy of each
//! folder's live records and names on disk, kept up to date by jobs the
//! engine task releases in the same order as the effects. A commit or an
//! observation therefore compares against the records as of the event that
//! asked for it. A scan compares against a copy taken when it starts. Each
//! report of `Unchanged` goes back with the record it was compared against,
//! for the engine task to check against the engine's record now (§7.3:
//! `Unchanged` is relative to a record).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use delocal_engine::{
    Action, ContentHash, Entry, Event, FolderId, NodeId, Outbound, RelPath, Version,
};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info, warn};

use super::clock::Clock;
use super::core::{Input, Report, Source};
use crate::commit::{Commit, DiskState, StoreJournal};
use crate::fs::Folder;
use crate::scan::{Scan, ScanReport};
use crate::store::Writer;

/// Free space below which a folder paused for a full disk stays paused
/// (§7.5). The design names no figure: 64 MiB is room for the next commit
/// of all but a large file, and a commit that still finds the disk full
/// says so and pauses the folder again.
pub const SPACE_FLOOR: u64 = 64 * 1024 * 1024;

/// One fetch the engine asked for (§7.5 steps 1 to 5): a `RequestFile`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub folder: FolderId,
    pub path: RelPath,
    pub version: Version,
    pub hash: ContentHash,
    pub size: u64,
    pub from: NodeId,
}

/// Where messages for peers go (§12): the transport.
pub trait Outbox: Send {
    /// A message for a connected peer.
    fn send(&mut self, to: NodeId, payload: Outbound);
    /// A file to pull from `fetch.from`. Its report comes back as an event.
    fn fetch(&mut self, fetch: Fetch);
}

/// No transport yet (Phase 3): messages are dropped, which is what a
/// disconnected peer would do with them. The engine sends only to
/// connected peers, and with no transport none connects.
#[derive(Debug, Default)]
pub struct NoTransport;

impl Outbox for NoTransport {
    fn send(&mut self, to: NodeId, payload: Outbound) {
        debug!(peer = %to.short(), ?payload, "no transport: message dropped");
    }

    fn fetch(&mut self, fetch: Fetch) {
        debug!(peer = %fetch.from.short(), path = %fetch.path, "no transport: fetch dropped");
    }
}

/// Something for the executor to do.
#[derive(Debug)]
pub(crate) enum Job {
    /// Records the engine wrote: `Some` for a live one, `None` for one that
    /// is gone or a tombstone.
    Records {
        folder: FolderId,
        updates: Vec<(RelPath, Option<Entry>)>,
    },
    /// Names on disk the host wrote: `None` for a pair that went.
    Names {
        folder: FolderId,
        updates: Vec<(RelPath, Option<OsString>)>,
    },
    /// A commit, with the version its report goes under.
    Commit {
        folder: FolderId,
        action: Action,
        version: Version,
    },
    /// A `Send` or `Fetch`.
    Outbound(Action),
    /// The engine's `Observe`: observe one path and report it as a scan
    /// would (§8.3).
    Observe { folder: FolderId, path: RelPath },
    /// The host's second look at a path it observed as unstable (§7.3),
    /// reported the same way.
    Again { folder: FolderId, path: RelPath },
    /// Resolve the open journal rows, then scan.
    Scan { folder: FolderId },
    /// Check a paused folder's free space.
    Space { folder: FolderId },
}

/// One folder as the executor keeps it.
pub(crate) struct Kept {
    pub folder: Arc<dyn Folder>,
    /// Its live records, as the engine last released them.
    pub records: Arc<BTreeMap<RelPath, Entry>>,
    /// Its names on disk that are not their index paths'.
    pub names: BTreeMap<RelPath, OsString>,
    pub disk: Arc<DiskState>,
}

impl Kept {
    /// The folder's commits, with what the executor keeps of it.
    fn commit<'a>(
        &'a self,
        id: FolderId,
        journal: &'a StoreJournal<'a>,
        clock: &'a Clock,
    ) -> Commit<'a> {
        Commit {
            id,
            folder: &*self.folder,
            records: &*self.records,
            names: &self.names,
            journal,
            clock,
            disk: &self.disk,
        }
    }
}

/// The executor's state. See the module docs.
pub(crate) struct Executor {
    pub folders: BTreeMap<FolderId, Kept>,
    pub writer: Arc<Writer>,
    pub clock: Arc<Clock>,
    pub inputs: tokio_mpsc::Sender<Input>,
    pub outbox: Box<dyn Outbox>,
    pub hashers: NonZeroUsize,
    /// Jobs handed off and not yet done, for [`super::Daemon`]'s idle check.
    pub busy: Arc<AtomicUsize>,
    /// Set once the daemon is stopping: what is still queued is dropped,
    /// as a crash would drop it, rather than performed for an engine that
    /// will never hear the reports.
    pub stopping: Arc<AtomicBool>,
    #[cfg(test)]
    pub trace: Arc<super::Trace>,
}

/// The engine task has stopped: nothing more is read.
struct Stopped;

impl Executor {
    /// Run jobs until the queue ends or the engine task stops.
    pub fn run(mut self, jobs: &mpsc::Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let outcome = self.job(job);
            self.busy.fetch_sub(1, Ordering::SeqCst);
            if outcome.is_err() {
                break;
            }
        }
    }

    /// The folders' journal: the store's, through the group-commit writer.
    fn journal(&self) -> StoreJournal<'_> {
        StoreJournal {
            writer: &self.writer,
            clock: &*self.clock,
        }
    }

    fn report(&self, input: Input) -> Result<(), Stopped> {
        self.inputs.blocking_send(input).map_err(|_| Stopped)
    }

    fn job(&mut self, job: Job) -> Result<(), Stopped> {
        #[cfg(test)]
        self.trace.performed(&job);
        match job {
            Job::Records { folder, updates } => {
                if let Some(kept) = self.folders.get_mut(&folder) {
                    let records = Arc::make_mut(&mut kept.records);
                    for (path, entry) in updates {
                        match entry {
                            Some(entry) => records.insert(path, entry),
                            None => records.remove(&path),
                        };
                    }
                }
                Ok(())
            }
            Job::Names { folder, updates } => {
                if let Some(kept) = self.folders.get_mut(&folder) {
                    for (path, name) in updates {
                        match name {
                            Some(name) => kept.names.insert(path, name),
                            None => kept.names.remove(&path),
                        };
                    }
                }
                Ok(())
            }
            Job::Outbound(Action::Send { to, payload }) => {
                self.outbox.send(to, payload);
                Ok(())
            }
            Job::Outbound(Action::Fetch {
                folder,
                path,
                version,
                hash,
                size,
                from,
            }) => {
                self.outbox.fetch(Fetch {
                    folder,
                    path,
                    version,
                    hash,
                    size,
                    from,
                });
                Ok(())
            }
            Job::Outbound(other) => {
                warn!(?other, "not a message; dropped");
                Ok(())
            }
            Job::Commit {
                folder,
                action,
                version,
            } => self.commit(folder, &action, &version),
            Job::Observe { folder, path } | Job::Again { folder, path } => {
                self.observe(folder, path)
            }
            Job::Scan { folder } => self.scan(folder),
            Job::Space { folder } => self.space(folder),
        }
    }

    fn commit(&self, id: FolderId, action: &Action, version: &Version) -> Result<(), Stopped> {
        let Some(kept) = self.folders.get(&id) else {
            return Ok(());
        };
        let journal = self.journal();
        let commit = kept.commit(id, &journal, &self.clock);
        // No transfer has a temp file to commit before Phase 3's fetches.
        let Some(committed) = commit.run(action, version, None) else {
            return Ok(());
        };
        let path = committed_path(&committed.event);
        for error in &committed.unsynced {
            warn!(folder = %id.short(), ?path, %error, "committed, but not known durable");
        }
        if !committed.holds_ignored.is_empty() {
            info!(
                folder = %id.short(), ?path, files = ?committed.holds_ignored,
                "a directory to remove holds ignored files; kept"
            );
        }
        if !committed.created.is_empty() {
            debug!(folder = %id.short(), ?path, created = ?committed.created, "made parents");
        }
        let compared = path.and_then(|path| kept.records.get(path).cloned());
        self.report(Input::report(Report {
            event: committed.event,
            writes: committed.writes,
            compared,
            source: Source::Commit,
        }))
    }

    fn observe(&self, id: FolderId, path: RelPath) -> Result<(), Stopped> {
        let Some(kept) = self.folders.get(&id) else {
            return Ok(());
        };
        let journal = self.journal();
        let commit = kept.commit(id, &journal, &self.clock);
        let (state, at) = commit.observe(&path);
        let compared = kept.records.get(&path).cloned();
        self.report(Input::report(Report {
            event: Event::Scanned {
                folder: id,
                path,
                state,
                at,
            },
            writes: Vec::new(),
            compared,
            source: Source::Observe,
        }))
    }

    fn scan(&self, id: FolderId) -> Result<(), Stopped> {
        let Some(kept) = self.folders.get(&id) else {
            return Ok(());
        };
        let journal = self.journal();
        let commit = kept.commit(id, &journal, &self.clock);
        let resolved = commit.resolve();
        for (path, error) in &resolved.errors {
            warn!(folder = %id.short(), %path, %error, "journal row not resolved");
        }
        if !resolved.resolved.is_empty() {
            info!(folder = %id.short(), paths = ?resolved.resolved, "journal rows resolved");
        }
        if !resolved.unrecorded.is_empty() {
            self.report(Input::Writes(resolved.unrecorded))?;
        }
        let scanner = Scanner {
            id,
            folder: Arc::clone(&kept.folder),
            records: Arc::clone(&kept.records),
            disk: Arc::clone(&kept.disk),
            clock: Arc::clone(&self.clock),
            inputs: self.inputs.clone(),
            hashers: self.hashers,
        };
        let spawned = std::thread::Builder::new()
            .name("delocal-scan".to_owned())
            .spawn(move || scanner.run());
        if let Err(error) = spawned {
            // The scan never started: report it ended, so the next one is
            // scheduled; nothing was observed, so nothing is taken from it.
            warn!(folder = %id.short(), %error, "could not start a scan");
            self.report(Input::ScanDone {
                folder: id,
                report: None,
            })?;
        }
        Ok(())
    }

    fn space(&self, id: FolderId) -> Result<(), Stopped> {
        let Some(kept) = self.folders.get(&id) else {
            return Ok(());
        };
        match kept
            .folder
            .open()
            .and_then(|fs| fs.available_space(Path::new("")))
        {
            Ok(free) if free >= SPACE_FLOOR => {
                info!(folder = %id.short(), free, "space recovered");
                self.report(Input::report(Report {
                    event: Event::SpaceRecovered { folder: id },
                    writes: Vec::new(),
                    compared: None,
                    source: Source::Host,
                }))
            }
            Ok(free) => {
                debug!(folder = %id.short(), free, "still no space");
                Ok(())
            }
            Err(error) => {
                warn!(folder = %id.short(), %error, "free space unknown");
                Ok(())
            }
        }
    }
}

/// The path a commit report is for.
fn committed_path(event: &Event) -> Option<&RelPath> {
    match event {
        Event::Applied { path, .. } | Event::Trashed { path, .. } => Some(path),
        _ => None,
    }
}

/// One full scan on a thread of its own (§7.3).
struct Scanner {
    id: FolderId,
    folder: Arc<dyn Folder>,
    /// The live records as the scan starts: what its fast path compares
    /// against.
    records: Arc<BTreeMap<RelPath, Entry>>,
    disk: Arc<DiskState>,
    clock: Arc<Clock>,
    inputs: tokio_mpsc::Sender<Input>,
    hashers: NonZeroUsize,
}

impl Scanner {
    fn run(self) {
        let scan = Scan {
            id: self.id,
            folder: &*self.folder,
            records: &*self.records,
            open: &*self.disk,
            clock: &*self.clock,
            hashers: self.hashers,
        };
        // A send fails only once the engine task has stopped; the scan
        // then runs to its end with nobody reading, which a stop at most
        // costs.
        let send = |event| {
            let compared = match &event {
                Event::Scanned { path, .. } => self.records.get(path).cloned(),
                _ => None,
            };
            let _ = self.inputs.blocking_send(Input::report(Report {
                event,
                writes: Vec::new(),
                compared,
                source: Source::Scan,
            }));
        };
        let ran = std::panic::catch_unwind(AssertUnwindSafe(|| scan.run(&mut |event| send(event))));
        let report = match ran {
            Ok(report) => Some(report),
            // A bug, but not one to stop the folder's scans for good: the
            // bracket closes as aborted, without its deletion pass, so
            // nothing the scan never reached is taken for deleted (§7.3),
            // and the next scan is scheduled as after any other.
            Err(_) => {
                error!(folder = %self.id.short(), "the scan panicked; aborted");
                send(Event::ScanAborted { folder: self.id });
                None
            }
        };
        let _ = self.inputs.blocking_send(Input::ScanDone {
            folder: self.id,
            report,
        });
    }
}

/// What a scan's report says for the log.
pub(crate) fn log_report(id: FolderId, report: &ScanReport) {
    let folder = id.short();
    if let Some(abort) = &report.aborted {
        warn!(%folder, ?abort, "scan aborted by the root guard");
    }
    if !report.skipped.is_empty() {
        info!(%folder, skipped = ?report.skipped, "paths skipped");
    }
    for (name, why) in &report.unobservable {
        warn!(%folder, name = %name.display(), ?why, "unobservable name");
    }
    for rule in &report.invalid_rules {
        warn!(%folder, ?rule, ".delocalignore rule left out");
    }
    if let Some(error) = &report.ignore_file {
        warn!(%folder, %error, ".delocalignore unreadable: the folder was skipped");
    }
}
