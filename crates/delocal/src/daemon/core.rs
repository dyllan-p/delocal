//! The engine task (DESIGN.md §11 "persistence contract", §7.3, §7.4,
//! §7.5, §7.8): the one place the engine runs.
//!
//! **Events in.** Everything the engine is told arrives on one queue
//! ([`Input`]): scan and observation reports, commit reports with the host
//! writes that go with them, the tier poll's findings, and the task's own
//! timers. Each is stamped with the clock's `now` (§7.8) and handed to
//! [`Engine::handle`].
//!
//! **Tiers** (§6.4). The tier poll ([`poll`](crate::tailscale::tier::poll))
//! sends the tier Tailscale's path info gives each node, and this task tells
//! the engine `PeerTierChanged` for each connected peer whose tier that
//! changes ([`Tiers::changes`]). A peer is matched to its Tailscale node by
//! its binding (§5): the `machines` rows, read at start and kept up to date
//! as this task writes them.
//!
//! **Writes to the group-commit writer.** The actions an event returns are
//! sorted ([`Group::push`]): every persistence hook and `RecordBatch` joins
//! the event's group, with the host's writes that came with the event, and
//! the group goes to the writer at once. The writer commits whatever has
//! queued in one transaction (§11), so a burst of events costs one `fsync`.
//!
//! **Effects only once durable.** What remains are effects: commits, trash
//! moves, sends, fetches and `Observe` answers. They go to the releaser with
//! the group's [`Durable`], in event order, and the releaser hands them to
//! the executor only when that group is durable. Groups resolve in order,
//! so every effect runs after every write of its own event and of each one
//! before it. Two things follow. No file operation and no message happens
//! while the record that explains it could still be lost. And a commit's
//! report is durable before the next commit at the same path is dispatched:
//! the report's writes are in the group of the event that handled it, and
//! the next commit at that path is an effect of that event or a later one
//! (the engine keeps one commit in flight per path, §7.5).
//!
//! **If the writer fails, the daemon stops.** A group that will never be
//! durable fails its `Durable`; the releaser performs nothing more and
//! tells this task, which returns the error. The engine is then ahead of
//! its store, and the only way on is a restart from what is durable.
//!
//! **Backpressure.** At most [`QUEUE`] groups wait to be durable. When that
//! many are queued, this task waits before it takes another event, and the
//! threads that feed it wait in turn on its full queue: a scan never runs
//! ahead of the store by more than that.
//!
//! **Timers**, all here: the engine's `WakeAt`, which ticks it with a fresh
//! batch id (§7.4); the hourly full scan (§7.3, §17 question 7), after the
//! startup scan; a second look 2 s after any observation of a path as
//! `Unstable` (§7.3, draft 47); and, while a full disk pauses a folder's
//! inbound, a check for free space every 30 s (§7.5).
//!
//! **The watcher** ([`super::watch`]) sends the paths its events name, and
//! this task hands them to the executor to observe. If the watcher lost
//! events, the folder is scanned in full (after the scan running, if one
//! is). If its root or marker went (§7.3, the root guard), the folder is
//! treated as after an aborted scan: what the watcher sees is dropped, and
//! nothing reaches the engine, until a scan finds the marker again and
//! the folder is watched afresh.
//!
//! **`Unchanged` is relative to a record** (§7.3). The executor and the
//! scanner compare against their own copies of the records, which can lag
//! the engine's. Each report of `Unchanged` comes with the record the host
//! compared against; if the engine's record now looks different to the
//! fast path, the report is relative to a record that is gone, and is
//! passed on as `Skipped { Unstable }` instead (seen, and nothing more) and
//! the path observed again 2 s later.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use delocal_engine::{
    Action, ApplyOutcome, BatchId, Engine, Entry, Event, FolderId, NodeId, RelPath, ScanState,
    SkipReason, Timestamp,
};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::DaemonError;
use super::clock::Clock;
use super::executor::{self, Job};
use super::watch::{Seen, Watcher};
use crate::scan::ScanReport;
use crate::scan::hash::SETTLE_NANOS;
use crate::store::{DiskName, Durable, Group, HostWrite, WriteError, Writer};
use crate::tailscale::StableId;
use crate::tailscale::tier::Tiers;

/// How many groups may wait to be durable before the engine task waits.
pub const QUEUE: usize = 256;

/// How long after an unstable observation the host looks again (§7.3).
pub const SETTLE: Duration = Duration::from_nanos(SETTLE_NANOS.unsigned_abs());

/// How often the host checks for free space while a full disk pauses a
/// folder's inbound (§7.5).
pub const SPACE_CHECK: Duration = Duration::from_secs(30);

/// Where a report came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Scan,
    Commit,
    Observe,
    /// The watcher's observations (§7.3).
    Watch,
    /// The host's own: a space check, or a test.
    Host,
}

/// An event for the engine, and what came with it.
#[derive(Debug)]
pub(crate) struct Report {
    pub event: Event,
    /// The host's writes that belong in the event's group (§11).
    pub writes: Vec<HostWrite>,
    /// For an `Unchanged`: the live record the host compared against.
    pub compared: Option<Entry>,
    pub source: Source,
}

/// Something for the engine task.
#[derive(Debug)]
pub(crate) enum Input {
    Report(Box<Report>),
    /// Host writes with no event: a resolve's rows the journal could not
    /// take.
    Writes(Vec<HostWrite>),
    /// A scan ended (or never started, `None`).
    ScanDone {
        folder: FolderId,
        report: Option<ScanReport>,
    },
    /// What a tier poll found (§6.4).
    Tiers(Tiers),
    /// Paths the watcher's events named, to observe (§7.3).
    Watched {
        folder: FolderId,
        paths: Vec<Seen>,
    },
    /// Paths to look at again as the watcher would, 2 s from now: their
    /// first look found the opposite of what their event said.
    Rewatch {
        folder: FolderId,
        paths: Vec<RelPath>,
    },
    /// The watcher lost events for the folder: a full scan finds them.
    Rescan {
        folder: FolderId,
    },
    /// The folder's root or its marker is gone, as the watcher found it
    /// (§7.3, the root guard). `root_gone` if the root itself went, and its
    /// watch with it.
    Guard {
        folder: FolderId,
        root_gone: bool,
    },
    Stop,
    #[cfg(test)]
    Probe(super::tests::Probe),
}

impl Input {
    pub fn report(report: Report) -> Self {
        Self::Report(Box::new(report))
    }
}

impl From<Tiers> for Input {
    fn from(tiers: Tiers) -> Self {
        Self::Tiers(tiers)
    }
}

/// One group on its way to durability, and the jobs that wait for it.
pub(crate) struct Release {
    durable: Durable,
    jobs: Vec<Job>,
}

/// Why the releaser stopped performing effects.
#[derive(Debug)]
pub(crate) enum Failure {
    /// A group will never be durable.
    Store(WriteError),
    /// The executor is gone.
    Executor,
}

/// What the engine task keeps per folder.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    pub scanning: bool,
    /// When the next full scan is due; `None` while one runs.
    pub next_scan: Option<Instant>,
    /// When to check for free space again, while the inbound is paused.
    pub space_at: Option<Instant>,
    /// The names on disk as the host has written them.
    pub names: BTreeMap<RelPath, OsString>,
    /// Paths the running scan reported `Skipped`: their names stay.
    pub skipped: Vec<RelPath>,
    /// Paths whose names on disk a commit changed while the scan ran: the
    /// scan may have walked past them before, so what it found there is
    /// not taken.
    pub touched: BTreeSet<RelPath>,
    /// `IndexChanged`s the running scan's events produced.
    pub changes: usize,
    /// Where the folder is on disk, for its watch.
    pub root: PathBuf,
    /// The root guard tripped for the watcher (§7.3): its observations are
    /// dropped until a scan finds the marker again.
    pub guarded: bool,
    /// The folder has no watch: it could not be set, or its root went.
    /// The next scan that finds the marker sets it again.
    pub unwatched: bool,
    /// The watcher lost events while a scan ran: another follows it.
    pub rescan_after: bool,
}

/// Paths to observe again once they have had time to settle (§7.3).
#[derive(Debug, Default)]
pub(crate) struct Reobserve {
    due: BTreeMap<(FolderId, RelPath), Instant>,
    order: BTreeSet<(Instant, FolderId, RelPath)>,
}

impl Reobserve {
    /// Look at `path` again at `at`, and not before.
    fn at(&mut self, folder: FolderId, path: RelPath, at: Instant) {
        if let Some(old) = self.due.insert((folder, path.clone()), at) {
            self.order.remove(&(old, folder, path.clone()));
        }
        self.order.insert((at, folder, path));
    }

    fn next(&self) -> Option<Instant> {
        self.order.first().map(|(at, _, _)| *at)
    }

    /// Every path due by `now`, in time order.
    fn take(&mut self, now: Instant) -> Vec<(FolderId, RelPath)> {
        let mut due = Vec::new();
        while let Some((at, folder, path)) = self.order.first().cloned() {
            if at > now {
                break;
            }
            self.order.remove(&(at, folder, path.clone()));
            self.due.remove(&(folder, path.clone()));
            due.push((folder, path));
        }
        due
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// The engine task's state. See the module docs.
pub(crate) struct Core {
    pub engine: Engine,
    pub clock: Arc<Clock>,
    pub writer: Arc<Writer>,
    /// To the releaser; bounded by [`QUEUE`].
    pub release: tokio_mpsc::Sender<Release>,
    /// To the executor, for jobs that wait for no group.
    pub jobs: mpsc::Sender<Job>,
    pub folders: BTreeMap<FolderId, Watch>,
    /// The earliest `WakeAt` not yet delivered.
    pub wake: Option<Timestamp>,
    pub rescan: Duration,
    pub space_check: Duration,
    /// `FolderJoined` for the folders the store has no parts of, handled
    /// before anything else.
    pub joins: Vec<Event>,
    /// Each known peer's Tailscale node (§5): the `machines` rows as this
    /// task has written them.
    pub bindings: BTreeMap<NodeId, StableId>,
    pub reobserve: Reobserve,
    /// Paths to look at again as the watcher would (see [`Input::Rewatch`]).
    pub rewatch: Reobserve,
    /// Jobs and releases handed off and not yet done.
    pub busy: Arc<AtomicUsize>,
    /// The watcher (§7.3), unless the daemon runs without one.
    pub watcher: Option<Watcher>,
    #[cfg(test)]
    pub trace: Arc<super::Trace>,
    #[cfg(test)]
    pub probes: super::tests::Probes,
}

impl Core {
    /// Run until stopped, or until the store or the executor fails. The
    /// first thing it does is the startup full scan of every folder.
    pub async fn run(
        mut self,
        mut inputs: tokio_mpsc::Receiver<Input>,
        mut failed: oneshot::Receiver<Failure>,
    ) -> Result<(), DaemonError> {
        for event in std::mem::take(&mut self.joins) {
            self.report(Report {
                event,
                writes: Vec::new(),
                compared: None,
                source: Source::Host,
            })
            .await?;
        }
        let ids: Vec<FolderId> = self.folders.keys().copied().collect();
        for id in ids {
            self.start_scan(id)?;
        }
        loop {
            let deadline = self.deadline();
            let sleep = async move {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            // Timers before inputs: a queue that never empties (a long
            // scan behind a slow store) must not keep the engine from its
            // ticks, nor a path from its second look.
            let step = tokio::select! {
                biased;
                failure = &mut failed => return Err(why(failure)),
                () = sleep => self.due().await,
                input = inputs.recv() => match input {
                    None | Some(Input::Stop) => return Ok(()),
                    Some(input) => {
                        #[cfg(test)]
                        {
                            self.probes.queued = !inputs.is_empty();
                        }
                        self.input(input).await
                    }
                },
            };
            if let Err(error) = step {
                // A send to the releaser fails once it has stopped, and it
                // says why before it stops.
                return Err(match error {
                    DaemonError::Stopped => match failed.try_recv() {
                        Ok(failure) => why(Ok(failure)),
                        Err(_) => DaemonError::Stopped,
                    },
                    error => error,
                });
            }
        }
    }

    async fn input(&mut self, input: Input) -> Result<(), DaemonError> {
        match input {
            Input::Report(report) => self.report(*report).await,
            Input::Writes(writes) => self.emit(self.clock.now(), Vec::new(), writes).await,
            Input::ScanDone { folder, report } => self.scan_done(folder, report).await,
            Input::Tiers(tiers) => self.tiers(&tiers).await,
            Input::Watched { folder, paths } => match self.folders.get(&folder) {
                Some(watch) if !watch.guarded => self.direct(Job::Watched { folder, paths }),
                _ => Ok(()),
            },
            Input::Rewatch { folder, paths } => {
                let at = Instant::now() + SETTLE;
                for path in paths {
                    self.rewatch.at(folder, path, at);
                }
                Ok(())
            }
            Input::Rescan { folder } => self.rescan(folder),
            Input::Guard { folder, root_gone } => {
                self.guard(folder, root_gone);
                Ok(())
            }
            // Handled by `run`.
            Input::Stop => Ok(()),
            #[cfg(test)]
            Input::Probe(probe) => self.probe(probe).await,
        }
    }

    /// Hand one report to the engine, and its consequences on.
    pub(crate) async fn report(&mut self, report: Report) -> Result<(), DaemonError> {
        let Report {
            mut event,
            writes,
            compared,
            source,
        } = report;
        #[cfg(test)]
        self.trace.handled.fetch_add(1, Ordering::SeqCst);
        if let Some((folder, path)) = relative(&self.engine, &mut event, compared.as_ref()) {
            debug!(folder = %folder.short(), %path, "unchanged against a record that moved");
        }
        let unstable = unstable(&event);
        if source != Source::Scan
            && let Some(folder) = folder_of(&event)
            && let Some(watch) = self.folders.get_mut(&folder)
            && watch.scanning
        {
            for write in &writes {
                match write {
                    HostWrite::PutDiskName { name, .. } => {
                        watch.touched.insert(name.path.clone());
                    }
                    HostWrite::RemoveDiskName { path, .. } => {
                        watch.touched.insert(path.clone());
                    }
                    _ => {}
                }
            }
        }
        if source == Source::Scan
            && let Event::Scanned {
                folder,
                path,
                state: ScanState::Skipped { .. },
                ..
            } = &event
            && let Some(watch) = self.folders.get_mut(folder)
        {
            watch.skipped.push(path.clone());
        }
        let scan_of = match (&event, source) {
            (Event::Scanned { folder, .. } | Event::ScanFinished { folder }, Source::Scan) => {
                Some(*folder)
            }
            _ => None,
        };
        let now = self.clock.now();
        let actions = self.engine.handle(now, event);
        if let Some(folder) = scan_of
            && let Some(watch) = self.folders.get_mut(&folder)
        {
            watch.changes += actions
                .iter()
                .filter(|a| matches!(a, Action::IndexChanged { .. }))
                .count();
        }
        if let Some((folder, path)) = unstable {
            self.reobserve.at(folder, path, Instant::now() + SETTLE);
        }
        self.emit(now, actions, writes).await
    }

    /// Put an event's writes in a group, submit it, and send its effects
    /// to wait for it (see the module docs). `writes` are the host's.
    async fn emit(
        &mut self,
        now: Timestamp,
        actions: Vec<Action>,
        writes: Vec<HostWrite>,
    ) -> Result<(), DaemonError> {
        let mut group = Group::new(now);
        let mut records: BTreeMap<FolderId, Vec<(RelPath, Option<Entry>)>> = BTreeMap::new();
        let mut names: BTreeMap<FolderId, Vec<(RelPath, Option<OsString>)>> = BTreeMap::new();
        let mut effects = Vec::new();
        for write in writes {
            match &write {
                HostWrite::PutDiskName { folder, name } => {
                    names
                        .entry(*folder)
                        .or_default()
                        .push((name.path.clone(), Some(name.name.clone())));
                }
                HostWrite::RemoveDiskName { folder, path } => {
                    names.entry(*folder).or_default().push((path.clone(), None));
                }
                HostWrite::PutMachine(machine) => {
                    let id = StableId(machine.ts_stable_id.clone());
                    self.bindings.insert(machine.node, id);
                }
                HostWrite::RemoveMachine(node) => {
                    self.bindings.remove(node);
                }
                _ => {}
            }
            group.host(write);
        }
        for action in actions {
            match &action {
                Action::IndexChanged { folder, record } => {
                    let live = (!record.entry.deleted).then(|| record.entry.clone());
                    records
                        .entry(*folder)
                        .or_default()
                        .push((record.entry.path.clone(), live));
                }
                Action::IndexRemoved { folder, path } => {
                    records
                        .entry(*folder)
                        .or_default()
                        .push((path.clone(), None));
                }
                _ => {}
            }
            if let Some(effect) = group.push(action) {
                self.effect(effect, &mut effects);
            }
        }
        for (folder, updates) in &names {
            if let Some(watch) = self.folders.get_mut(folder) {
                for (path, name) in updates {
                    match name {
                        Some(name) => watch.names.insert(path.clone(), name.clone()),
                        None => watch.names.remove(path),
                    };
                }
            }
        }
        self.watch_space();
        if group.is_empty() && effects.is_empty() {
            return Ok(());
        }
        let mut jobs: Vec<Job> = records
            .into_iter()
            .map(|(folder, updates)| Job::Records { folder, updates })
            .collect();
        jobs.extend(
            names
                .into_iter()
                .map(|(folder, updates)| Job::Names { folder, updates }),
        );
        jobs.extend(effects);
        #[cfg(test)]
        self.trace.emitted(&jobs);
        let durable = self.writer.submit(group);
        self.busy.fetch_add(1 + jobs.len(), Ordering::SeqCst);
        // The backpressure: this waits while QUEUE groups are not durable.
        self.release
            .send(Release { durable, jobs })
            .await
            .map_err(|_| DaemonError::Stopped)
    }

    /// Where an effect goes.
    fn effect(&mut self, effect: Action, jobs: &mut Vec<Job>) {
        match effect {
            Action::WakeAt(at) => {
                self.wake = Some(self.wake.map_or(at, |wake| wake.min(at)));
            }
            Action::StatusChanged { folder, status } => {
                info!(folder = %folder.short(), ?status, "status");
            }
            Action::Send { .. } | Action::Fetch { .. } => jobs.push(Job::Outbound(effect)),
            Action::Write {
                folder, ref entry, ..
            } => {
                let version = entry.version.clone();
                jobs.push(Job::Commit {
                    folder,
                    action: effect,
                    version,
                });
            }
            Action::MoveToTrash {
                folder,
                ref version,
                ..
            } => {
                let version = version.clone();
                jobs.push(Job::Commit {
                    folder,
                    action: effect,
                    version,
                });
            }
            // These name no version: the report goes under the want's, as
            // it stands when the engine asks for the commit (§7.5).
            Action::Remove {
                folder, ref path, ..
            }
            | Action::SetMeta {
                folder, ref path, ..
            } => {
                let version = self
                    .engine
                    .folder(folder)
                    .and_then(|f| f.wants().get(path))
                    .map(|want| want.version().clone())
                    .unwrap_or_default();
                jobs.push(Job::Commit {
                    folder,
                    action: effect,
                    version,
                });
            }
            Action::Observe { folder, path } => jobs.push(Job::Observe { folder, path }),
            // Writes: `Group::push` never hands these back.
            Action::RecordBatch { .. }
            | Action::IndexChanged { .. }
            | Action::IndexRemoved { .. }
            | Action::WantChanged { .. }
            | Action::PendingChanged { .. }
            | Action::HeldChanged { .. }
            | Action::DeferredChanged { .. }
            | Action::RestChanged { .. } => {
                warn!(?effect, "a write handed back as an effect; ignored");
            }
        }
    }

    /// A tier poll's findings (§6.4): `PeerTierChanged` for each connected
    /// peer whose tier they change, one event at a time.
    async fn tiers(&mut self, tiers: &Tiers) -> Result<(), DaemonError> {
        for (peer, tier) in tiers.changes(self.engine.peers(), &self.bindings) {
            info!(peer = %peer.short(), ?tier, "tier changed");
            self.report(Report {
                event: Event::PeerTierChanged { peer, tier },
                writes: Vec::new(),
                compared: None,
                source: Source::Host,
            })
            .await?;
        }
        Ok(())
    }

    /// Check for free space every 30 s while a folder's inbound is paused
    /// for a full disk, and not otherwise.
    fn watch_space(&mut self) {
        let space_check = self.space_check;
        for (id, watch) in &mut self.folders {
            let paused = self.engine.folder(*id).is_some_and(|f| f.disk_full());
            if !paused {
                watch.space_at = None;
            } else if watch.space_at.is_none() {
                watch.space_at = Some(Instant::now() + space_check);
            }
        }
    }

    /// The next time a timer is due.
    fn deadline(&self) -> Option<Instant> {
        let wake = self
            .wake
            .map(|at| Instant::from_std(self.clock.instant(at)));
        let folders = self
            .folders
            .values()
            .flat_map(|watch| [watch.next_scan, watch.space_at]);
        [wake, self.reobserve.next(), self.rewatch.next()]
            .into_iter()
            .chain(folders)
            .flatten()
            .min()
    }

    /// Fire every timer that is due.
    async fn due(&mut self) -> Result<(), DaemonError> {
        let now = Instant::now();
        if let Some(wake) = self.wake
            && Instant::from_std(self.clock.instant(wake)) <= now
        {
            // The engine asks again for whatever is not due yet.
            self.wake = None;
            let fresh_batch_id = BatchId::from_bytes(rand::random());
            let at = self.clock.now();
            let actions = self.engine.handle(at, Event::Tick { fresh_batch_id });
            self.emit(at, actions, Vec::new()).await?;
        }
        let scans: Vec<FolderId> = self
            .folders
            .iter()
            .filter(|(_, w)| !w.scanning && w.next_scan.is_some_and(|at| at <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in scans {
            self.start_scan(id)?;
        }
        for (folder, path) in self.reobserve.take(now) {
            self.direct(Job::Again { folder, path })?;
        }
        let mut rewatch: BTreeMap<FolderId, Vec<Seen>> = BTreeMap::new();
        for (folder, path) in self.rewatch.take(now) {
            rewatch.entry(folder).or_default().push(Seen {
                path,
                came: false,
                went: false,
                again: true,
                names: Vec::new(),
            });
        }
        for (folder, paths) in rewatch {
            if self.folders.get(&folder).is_some_and(|w| !w.guarded) {
                self.direct(Job::Watched { folder, paths })?;
            }
        }
        let checks: Vec<FolderId> = self
            .folders
            .iter()
            .filter(|(_, w)| w.space_at.is_some_and(|at| at <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in checks {
            if let Some(watch) = self.folders.get_mut(&id) {
                watch.space_at = Some(now + self.space_check);
            }
            self.direct(Job::Space { folder: id })?;
        }
        Ok(())
    }

    /// A job that waits for no group.
    pub(crate) fn direct(&self, job: Job) -> Result<(), DaemonError> {
        self.busy.fetch_add(1, Ordering::SeqCst);
        self.jobs.send(job).map_err(|_| DaemonError::Stopped)
    }

    /// Start a full scan of `id`; `Commit::resolve` runs first, on the
    /// executor (§7.5, draft 64).
    pub(crate) fn start_scan(&mut self, id: FolderId) -> Result<(), DaemonError> {
        let Some(watch) = self.folders.get_mut(&id) else {
            return Ok(());
        };
        watch.scanning = true;
        watch.next_scan = None;
        watch.skipped.clear();
        watch.touched.clear();
        watch.changes = 0;
        self.direct(Job::Scan { folder: id })
    }

    /// The watcher lost events for `id`: scan it in full now, or as soon as
    /// the scan running ends.
    fn rescan(&mut self, id: FolderId) -> Result<(), DaemonError> {
        let Some(watch) = self.folders.get_mut(&id) else {
            return Ok(());
        };
        if watch.scanning {
            watch.rescan_after = true;
            #[cfg(test)]
            self.trace.rescans.fetch_add(1, Ordering::SeqCst);
            return Ok(());
        }
        info!(folder = %id.short(), "the watcher lost events: scanning");
        #[cfg(test)]
        self.trace.rescans.fetch_add(1, Ordering::SeqCst);
        self.start_scan(id)
    }

    /// The root guard tripped for the watcher (§7.3): the folder is treated
    /// as after an aborted scan. Nothing the watcher sees is observed until
    /// a scan finds the marker again, and nothing is announced meanwhile. A
    /// root that went took its watch with it, so the folder is watched again
    /// once it is back; one that is still there keeps its watch, which a
    /// second watch would only open a gap in.
    fn guard(&mut self, id: FolderId, root_gone: bool) {
        let Some(watch) = self.folders.get_mut(&id) else {
            return;
        };
        if root_gone {
            watch.unwatched = true;
        }
        if !watch.guarded {
            warn!(
                folder = %id.short(),
                "the folder's root or marker is gone: paused, announcing nothing, until a \
                 scan finds it"
            );
            watch.guarded = true;
            #[cfg(test)]
            self.trace.guards.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A scan ended: schedule the next, keep its names on disk, log what
    /// it found.
    async fn scan_done(
        &mut self,
        id: FolderId,
        report: Option<ScanReport>,
    ) -> Result<(), DaemonError> {
        let rescan = self.rescan;
        let Some(watch) = self.folders.get_mut(&id) else {
            return Ok(());
        };
        watch.scanning = false;
        watch.next_scan = Some(Instant::now() + rescan);
        #[cfg(test)]
        self.probes.scanned(id, watch.changes);
        let Some(report) = report else {
            return Ok(());
        };
        executor::log_report(id, &report);
        info!(folder = %id.short(), changes = watch.changes, "scan finished");
        if report.aborted.is_some() {
            return Ok(());
        }
        // The scan found the marker: the guard lifts, and a folder left
        // without a watch gets one again.
        if watch.guarded {
            info!(folder = %id.short(), "the folder is back: watching again");
            watch.guarded = false;
        }
        // What changed between this scan's walk and the new watch, the
        // next scan finds.
        if watch.unwatched
            && let Some(watcher) = self.watcher.as_mut()
        {
            match watcher.watch(id, &watch.root) {
                Ok(()) => {
                    watch.unwatched = false;
                    watch.rescan_after = true;
                }
                Err(error) => warn!(folder = %id.short(), %error, "not watched"),
            }
        }
        if std::mem::take(&mut watch.rescan_after) {
            self.start_scan(id)?;
        }
        let Some(watch) = self.folders.get_mut(&id) else {
            return Ok(());
        };
        let writes = names_written(id, watch, &report);
        if writes.is_empty() {
            return Ok(());
        }
        self.emit(self.clock.now(), Vec::new(), writes).await
    }
}

/// The writes that make the store's names on disk what a finished scan
/// found (§7.3, `disk_names` in §11): a pair for every name that differs
/// from its index path, and none for a path the scan saw by its index name.
/// A path the scan skipped keeps its pair, and so does everything beneath
/// it, since the scan did not look. So does a path a commit renamed while
/// the scan ran, with everything beneath it: the scan may have walked past
/// it first, and the commit's pair is the newer.
fn names_written(id: FolderId, watch: &Watch, report: &ScanReport) -> Vec<HostWrite> {
    let touched = |path: &RelPath| {
        watch
            .touched
            .iter()
            .any(|t| t == path || t.is_ancestor_of(path))
    };
    let found: BTreeMap<&RelPath, &OsString> = report
        .disk_names
        .iter()
        .map(|n| (&n.path, &n.name))
        .filter(|(path, _)| !touched(path))
        .collect();
    let mut writes = Vec::new();
    for (path, name) in &found {
        if watch.names.get(*path) != Some(*name) {
            writes.push(HostWrite::PutDiskName {
                folder: id,
                name: DiskName {
                    path: (*path).clone(),
                    name: (*name).clone(),
                },
            });
        }
    }
    for path in watch.names.keys() {
        let skipped = watch
            .skipped
            .iter()
            .any(|s| s == path || s.is_ancestor_of(path));
        if !found.contains_key(path) && !skipped && !touched(path) {
            writes.push(HostWrite::RemoveDiskName {
                folder: id,
                path: path.clone(),
            });
        }
    }
    writes
}

/// An `Unchanged` compared against a record the engine no longer holds
/// (see the module docs) becomes `Skipped { Unstable }`; the path, if so.
/// The fast path is the test: if `compared` and the engine's record now
/// would judge every file alike, "unchanged against one" is "unchanged
/// against the other".
fn relative(
    engine: &Engine,
    event: &mut Event,
    compared: Option<&Entry>,
) -> Option<(FolderId, RelPath)> {
    let (folder, path, state) = match event {
        Event::Scanned {
            folder,
            path,
            state,
            ..
        } => (*folder, &*path, state),
        Event::Applied {
            folder,
            path,
            outcome,
            ..
        }
        | Event::Trashed {
            folder,
            path,
            outcome,
            ..
        } => match outcome {
            ApplyOutcome::ChangedUnderneath { found } | ApplyOutcome::Failed { found, .. } => {
                (*folder, &*path, found)
            }
            ApplyOutcome::Ok => return None,
        },
        _ => return None,
    };
    if *state != ScanState::Unchanged {
        return None;
    }
    let now = engine
        .folder(folder)
        .and_then(|f| f.index().live(path))
        .map(|record| record.entry.observed());
    if now == compared.filter(|e| !e.deleted).map(Entry::observed) {
        return None;
    }
    *state = ScanState::Skipped {
        reason: SkipReason::Unstable,
    };
    Some((folder, path.clone()))
}

/// The folder an event is for, if it is a report about a path.
fn folder_of(event: &Event) -> Option<FolderId> {
    match event {
        Event::Scanned { folder, .. }
        | Event::Applied { folder, .. }
        | Event::Trashed { folder, .. } => Some(*folder),
        _ => None,
    }
}

/// The path an event reports `Skipped { Unstable }` for, to look at again.
fn unstable(event: &Event) -> Option<(FolderId, RelPath)> {
    const UNSTABLE: ScanState = ScanState::Skipped {
        reason: SkipReason::Unstable,
    };
    match event {
        Event::Scanned {
            folder,
            path,
            state,
            ..
        } if *state == UNSTABLE => Some((*folder, path.clone())),
        Event::Applied {
            folder,
            path,
            outcome,
            ..
        }
        | Event::Trashed {
            folder,
            path,
            outcome,
            ..
        } => match outcome {
            ApplyOutcome::ChangedUnderneath { found } | ApplyOutcome::Failed { found, .. }
                if *found == UNSTABLE =>
            {
                Some((*folder, path.clone()))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The daemon's error for the releaser's reason to stop.
fn why(failure: Result<Failure, oneshot::error::RecvError>) -> DaemonError {
    match failure {
        Ok(Failure::Store(error)) => DaemonError::Write(error),
        Ok(Failure::Executor) | Err(_) => DaemonError::Stopped,
    }
}

/// The releaser: wait for each group to be durable, in order, then hand
/// its jobs to the executor. Stops at the first group that will never be
/// durable, and says why on `failed`.
pub(crate) async fn release(
    mut releases: tokio_mpsc::Receiver<Release>,
    jobs: mpsc::Sender<Job>,
    failed: oneshot::Sender<Failure>,
    busy: Arc<AtomicUsize>,
) {
    while let Some(Release {
        durable,
        jobs: ready,
    }) = releases.recv().await
    {
        if let Err(error) = durable.await {
            let _ = failed.send(Failure::Store(error));
            return;
        }
        for job in ready {
            if jobs.send(job).is_err() {
                let _ = failed.send(Failure::Executor);
                return;
            }
        }
        busy.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use delocal_engine::{ContentHash, HostName, Kind, NodeConfig, NodeId, Observed, Rules};

    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    const FOLDER: FolderId = FolderId::from_bytes([9; 16]);

    fn file(mtime_ns: i64) -> Observed {
        Observed {
            kind: Kind::File,
            size: 3,
            mtime_ns,
            exec: false,
            hash: ContentHash::from_bytes([7; 32]),
        }
    }

    /// An engine whose folder holds the file `f` as observed with `mtime`.
    fn engine_with_f(mtime_ns: i64) -> Engine {
        let node = NodeId::from_bytes([1; 16]);
        let mut engine = Engine::new(NodeConfig {
            node_id: node,
            author_host: HostName::new("here").unwrap(),
        });
        let now = Timestamp::from_unix_nanos(1_000);
        engine.handle(
            now,
            Event::FolderJoined {
                folder: FOLDER,
                rules: Rules::default(),
                members: vec![node],
            },
        );
        engine.handle(
            now,
            Event::Scanned {
                folder: FOLDER,
                path: p("f"),
                state: ScanState::Observed(file(mtime_ns)),
                at: now,
            },
        );
        engine
    }

    fn unchanged() -> Event {
        Event::Scanned {
            folder: FOLDER,
            path: p("f"),
            state: ScanState::Unchanged,
            at: Timestamp::from_unix_nanos(2_000),
        }
    }

    const UNSTABLE: ScanState = ScanState::Skipped {
        reason: SkipReason::Unstable,
    };

    /// §7.3: `Unchanged` is relative to a record. One compared against the
    /// record the engine holds passes; one compared against another (or
    /// none) is only seen, and the path looked at again.
    #[test]
    fn an_unchanged_against_a_record_that_moved_is_seen_and_looked_at_again() {
        let engine = engine_with_f(5);
        let record = engine
            .folder(FOLDER)
            .unwrap()
            .index()
            .live(&p("f"))
            .unwrap()
            .entry
            .clone();

        let mut event = unchanged();
        assert_eq!(relative(&engine, &mut event, Some(&record)), None);
        assert_eq!(event, unchanged());

        // A record the fast path judges alike, whatever its version.
        let bumped = Entry {
            version: record.version.incremented(NodeId::from_bytes([2; 16])),
            ..record.clone()
        };
        let mut event = unchanged();
        assert_eq!(relative(&engine, &mut event, Some(&bumped)), None);

        for compared in [
            Some(Entry {
                mtime_ns: 6,
                ..record.clone()
            }),
            Some(Entry {
                hash: ContentHash::from_bytes([8; 32]),
                ..record.clone()
            }),
            Some(Entry {
                deleted: true,
                ..record.clone()
            }),
            None,
        ] {
            let mut event = unchanged();
            assert_eq!(
                relative(&engine, &mut event, compared.as_ref()),
                Some((FOLDER, p("f"))),
                "{compared:?}"
            );
            let Event::Scanned { state, .. } = &event else {
                panic!("{event:?}")
            };
            assert_eq!(*state, UNSTABLE);
            assert_eq!(unstable(&event), Some((FOLDER, p("f"))));
        }

        // A failed commit's observation is judged the same way.
        let mut event = Event::Applied {
            folder: FOLDER,
            path: p("f"),
            version: record.version.clone(),
            at: Timestamp::from_unix_nanos(3_000),
            outcome: ApplyOutcome::ChangedUnderneath {
                found: ScanState::Unchanged,
            },
        };
        let moved = Entry {
            mtime_ns: 6,
            ..record.clone()
        };
        assert!(relative(&engine, &mut event, Some(&moved)).is_some());
        assert!(matches!(
            event,
            Event::Applied {
                outcome: ApplyOutcome::ChangedUnderneath { found: UNSTABLE },
                ..
            }
        ));

        // Anything but `Unchanged` is absolute, and passes untouched.
        let mut observed = Event::Scanned {
            folder: FOLDER,
            path: p("f"),
            state: ScanState::Observed(file(9)),
            at: Timestamp::from_unix_nanos(2_000),
        };
        let before = observed.clone();
        assert_eq!(relative(&engine, &mut observed, None), None);
        assert_eq!(observed, before);
    }

    #[test]
    fn only_an_unstable_observation_is_looked_at_again() {
        let skipped = |reason| Event::Scanned {
            folder: FOLDER,
            path: p("f"),
            state: ScanState::Skipped { reason },
            at: Timestamp::from_unix_nanos(1),
        };
        assert_eq!(
            unstable(&skipped(SkipReason::Unstable)),
            Some((FOLDER, p("f")))
        );
        assert_eq!(unstable(&skipped(SkipReason::Io)), None);
        assert_eq!(unstable(&unchanged()), None);
        let failed = Event::Trashed {
            folder: FOLDER,
            path: p("f"),
            version: delocal_engine::Version::empty(),
            at: Timestamp::from_unix_nanos(1),
            outcome: ApplyOutcome::Failed {
                error: delocal_engine::LocalError::Io,
                found: UNSTABLE,
            },
        };
        assert_eq!(unstable(&failed), Some((FOLDER, p("f"))));
    }

    #[test]
    fn a_path_is_looked_at_again_once_at_its_latest_time() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut again = Reobserve::default();
        again.at(FOLDER, p("a"), at(10));
        again.at(FOLDER, p("b"), at(5));
        // A second unstable report pushes the first look back.
        again.at(FOLDER, p("a"), at(30));
        assert_eq!(again.next(), Some(at(5)));
        assert_eq!(again.take(at(20)), [(FOLDER, p("b"))]);
        assert_eq!(again.next(), Some(at(30)));
        assert!(again.take(at(29)).is_empty());
        assert_eq!(again.take(at(30)), [(FOLDER, p("a"))]);
        assert!(again.is_empty());
    }

    /// §7.3, §11: a finished scan's names on disk replace the store's,
    /// except at and beneath what it skipped or a commit renamed meanwhile.
    #[test]
    fn a_scan_keeps_the_names_it_found_and_those_it_could_not_look_at() {
        let name = |s: &str| DiskName {
            path: p(s),
            name: OsString::from(format!("{s}\u{301}")),
        };
        let mut watch = Watch::default();
        for path in ["gone", "kept/x", "kept", "same"] {
            watch.names.insert(p(path), name(path).name);
        }
        watch.skipped = vec![p("kept")];
        // A commit renamed `moved` while the scan ran, after the scan had
        // found its old name; and made `fresh` beneath `made`.
        watch.names.insert(p("made/fresh"), name("made/fresh").name);
        watch.touched = [p("moved"), p("made")].into();
        let report = ScanReport {
            disk_names: vec![name("new"), name("same"), name("moved"), name("made/old")],
            ..ScanReport::default()
        };
        assert_eq!(
            names_written(FOLDER, &watch, &report),
            [
                HostWrite::PutDiskName {
                    folder: FOLDER,
                    name: name("new"),
                },
                HostWrite::RemoveDiskName {
                    folder: FOLDER,
                    path: p("gone"),
                },
            ]
        );
    }
}
