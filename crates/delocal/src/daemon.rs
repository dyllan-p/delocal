//! The daemon (DESIGN.md §10 `delocal daemon`, §11, §7.8): the long-running
//! process the service runs, and its core, the engine driven by real events.
//!
//! [`run`] is what `delocal daemon --state-dir <dir>` does, in order:
//!
//! 1. Make the state directory if it is missing, and take its lock
//!    ([`lock`]): a second daemon on the same directory refuses to start.
//! 2. Send the logs to `logs/`, rotating daily and keeping a week ([`logs`]).
//! 3. Raise the soft limit on open files to the hard one ([`limits`]).
//! 4. Read `node.json`, or make it with a fresh node ID ([`node`], §5).
//! 5. Start: open the store, restore the engine, start each folder, and
//!    run the engine task with the startup full scan ([`Daemon::start`]).
//! 6. Serve until `SIGTERM` or `SIGINT`, or until something fails that the
//!    daemon cannot run on without, the store first of all.
//!
//! **Inside**, three parts share the work, each with its own module:
//!
//! - the **engine task** (`core`), on the tokio runtime, the one place the
//!   engine runs: events in, writes to the group-commit writer, effects
//!   released once durable, and every timer;
//! - the **releaser**, a task beside it, which waits for each group to be
//!   durable, in order, and only then passes its effects on;
//! - the **executor** (`executor`), one thread that performs them in order
//!   (commits, sends, `Observe` answers) and starts each scan on a thread
//!   of its own.
//!
//! The store's writer is a thread of its own too ([`crate::store::writer`]).
//!
//! **Time** is [`clock::Clock`]: the wall clock at start plus the monotonic
//! time since (§7.8).
//!
//! **What is not here yet.** No transport (Phase 3): sends and fetches go
//! to an [`Outbox`] that drops them, and no peer ever connects. No watcher:
//! local changes are seen by the startup scan and the hourly one. No mtime
//! shim (§7.3) and no CLI socket (Phase 4).

use std::fmt;
use std::fs::DirBuilder;
use std::io;
use std::num::NonZeroUsize;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use delocal_engine::{HostName, NodeConfig};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tracing::{error, info, warn};

use crate::fs::{Folder, RealFolder};
use crate::scan;
use crate::store::{StoreError, WriteError};

pub mod clock;
mod core;
mod executor;
pub mod limits;
pub mod lock;
pub mod logs;
pub mod node;
mod startup;
#[cfg(test)]
mod tests;

pub use self::core::{QUEUE, SETTLE, SPACE_CHECK};
pub use executor::{Fetch, NoTransport, Outbox, SPACE_FLOOR};

use self::clock::Clock;
use self::core::{Core, Input, Watch};
use self::executor::{Executor, Kept};
use self::lock::{LockError, StateLock};
use self::node::NodeError;

/// The store's file, in the state directory (§11).
pub const STORE: &str = "delocal.db";

/// How often a folder is scanned in full after the startup scan (§7.3,
/// §17 question 7).
pub const RESCAN: Duration = Duration::from_secs(60 * 60);

/// How many reports may wait for the engine task before the threads that
/// send them wait too.
const INPUTS: usize = 1024;

/// What `delocal daemon` is given.
#[derive(Clone, Debug)]
pub struct Options {
    pub state_dir: PathBuf,
}

/// The folder at a member's path: the real one, or a spy in tests.
pub type FolderAt = Box<dyn Fn(&Path) -> Arc<dyn Folder> + Send + Sync>;

/// Everything [`Daemon::start`] needs.
pub struct Setup {
    pub state_dir: PathBuf,
    /// The state directory's lock, held for as long as the daemon runs.
    pub lock: StateLock,
    pub node: NodeConfig,
    pub clock: Arc<Clock>,
    pub folder: FolderAt,
    pub outbox: Box<dyn Outbox>,
    /// How often a folder is scanned in full: [`RESCAN`].
    pub rescan: Duration,
    /// How often a folder paused for a full disk checks for space:
    /// [`SPACE_CHECK`].
    pub space_check: Duration,
    pub hashers: NonZeroUsize,
}

impl Setup {
    /// The daemon as it runs for real: folders on disk, no transport yet,
    /// a full scan every hour.
    pub fn new(state_dir: PathBuf, lock: StateLock, node: NodeConfig, clock: Arc<Clock>) -> Self {
        Self {
            state_dir,
            lock,
            node,
            clock,
            folder: Box::new(|path| Arc::new(RealFolder::new(path))),
            outbox: Box::new(NoTransport),
            rescan: RESCAN,
            space_check: SPACE_CHECK,
            hashers: scan::default_hashers(),
        }
    }
}

/// Why the daemon could not start, or stopped.
#[derive(Debug)]
pub enum DaemonError {
    /// The state directory could not be made.
    StateDir {
        dir: PathBuf,
        error: io::Error,
    },
    Lock(LockError),
    Logs(tracing_appender::rolling::InitError),
    Node(NodeError),
    /// The store could not be opened or read: a newer schema among them.
    Store(StoreError),
    /// A group will never be durable: the daemon stops rather than run on
    /// without persisting (§11).
    Write(WriteError),
    /// The runtime or a thread could not be started.
    Spawn(io::Error),
    /// A part of the daemon stopped without saying why.
    Stopped,
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateDir { dir, error } => write!(f, "{}: {error}", dir.display()),
            Self::Lock(e) => e.fmt(f),
            Self::Logs(e) => write!(f, "the logs could not be opened: {e}"),
            Self::Node(e) => e.fmt(f),
            Self::Store(e) => e.fmt(f),
            Self::Write(e) => write!(f, "the store stopped taking writes: {e}"),
            Self::Spawn(e) => write!(f, "could not start: {e}"),
            Self::Stopped => f.write_str("a part of the daemon stopped unexpectedly"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<LockError> for DaemonError {
    fn from(e: LockError) -> Self {
        Self::Lock(e)
    }
}

impl From<NodeError> for DaemonError {
    fn from(e: NodeError) -> Self {
        Self::Node(e)
    }
}

impl From<StoreError> for DaemonError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<WriteError> for DaemonError {
    fn from(e: WriteError) -> Self {
        Self::Write(e)
    }
}

/// `delocal daemon`: steps 1 to 6 of the module docs. Returns once the
/// daemon has stopped.
pub fn run(options: &Options) -> Result<(), DaemonError> {
    let dir = &options.state_dir;
    // Only this user's: it holds this machine's identity.
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| DaemonError::StateDir {
            dir: dir.clone(),
            error,
        })?;
    let lock = lock::lock(dir)?;
    let _logs = logs::init(dir).map_err(DaemonError::Logs)?;
    info!(version = delocal_engine::VERSION, state_dir = %dir.display(), "starting");
    match limits::raise_open_files() {
        limits::Raised::To { from, to } => info!(from, ?to, "open-file limit raised"),
        limits::Raised::Already { at } => info!(?at, "open-file limit"),
        limits::Raised::Refused { at } => warn!(at, "open-file limit could not be raised"),
    }
    let clock = Arc::new(Clock::start());
    let node = node::load_or_create(dir, clock.now())?;
    let config = NodeConfig {
        node_id: node.node_id,
        author_host: hostname(),
    };
    info!(node = %node.node_id.short(), host = %config.author_host, "this machine");
    let result =
        Daemon::start(Setup::new(dir.clone(), lock, config, clock)).and_then(Daemon::serve);
    match &result {
        Ok(()) => info!("stopped"),
        Err(e) => error!(%e, "stopped"),
    }
    result
}

/// This machine's name for `author_host` (§7.1): the Tailscale hostname is
/// Phase 3's, so until then the system's, cut to its first label and to
/// the characters a hostname may have here.
pub fn hostname() -> HostName {
    let uname = rustix::system::uname();
    let name = uname.nodename().to_string_lossy();
    let label: String = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(HostName::MAX_LEN)
        .collect();
    HostName::new(label).unwrap_or_else(|_| HostName::empty())
}

/// A running daemon. See the module docs.
pub struct Daemon {
    runtime: tokio::runtime::Runtime,
    inputs: tokio_mpsc::Sender<Input>,
    core: tokio::task::JoinHandle<Result<(), DaemonError>>,
    executor: thread::JoinHandle<()>,
    /// Set once stopping: the executor performs nothing more.
    stopping: Arc<AtomicBool>,
    #[cfg(test)]
    busy: Arc<AtomicUsize>,
    #[cfg(test)]
    trace: Arc<Trace>,
    // Last: the lock goes only once everything else has stopped.
    _lock: StateLock,
}

impl Daemon {
    /// Start the daemon: open the store, restore the engine, start each
    /// folder, all before this returns, then run the engine task, which
    /// begins with the startup full scan.
    pub fn start(setup: Setup) -> Result<Self, DaemonError> {
        let started = startup::start(&setup, &setup.node, &setup.clock)?;
        let Setup {
            state_dir: _state_dir,
            lock,
            node: _,
            clock,
            folder: _,
            outbox,
            rescan,
            space_check,
            hashers,
        } = setup;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("delocal")
            .build()
            .map_err(DaemonError::Spawn)?;
        let (inputs, inputs_rx) = tokio_mpsc::channel(INPUTS);
        let (release, releases) = tokio_mpsc::channel(QUEUE);
        let (jobs, jobs_rx) = mpsc::channel();
        let (failed, failed_rx) = oneshot::channel();
        let busy = Arc::new(AtomicUsize::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let writer = Arc::new(started.writer);
        #[cfg(test)]
        let trace = Arc::new(Trace::default());

        let mut watches = std::collections::BTreeMap::new();
        let mut kept = std::collections::BTreeMap::new();
        for opened in started.folders {
            watches.insert(
                opened.id,
                Watch {
                    names: opened.names.clone(),
                    ..Watch::default()
                },
            );
            kept.insert(
                opened.id,
                Kept {
                    folder: opened.folder,
                    records: Arc::new(opened.records),
                    names: opened.names,
                    disk: opened.disk,
                },
            );
        }
        let executor = Executor {
            folders: kept,
            writer: Arc::clone(&writer),
            clock: Arc::clone(&clock),
            inputs: inputs.clone(),
            outbox,
            hashers,
            busy: Arc::clone(&busy),
            stopping: Arc::clone(&stopping),
            #[cfg(test)]
            trace: Arc::clone(&trace),
        };
        let executor = thread::Builder::new()
            .name("delocal-exec".to_owned())
            .spawn(move || executor.run(&jobs_rx))
            .map_err(DaemonError::Spawn)?;
        let core = Core {
            engine: started.engine,
            clock,
            writer,
            release,
            jobs: jobs.clone(),
            folders: watches,
            wake: None,
            rescan,
            space_check,
            joins: started.joins,
            reobserve: self::core::Reobserve::default(),
            busy: Arc::clone(&busy),
            #[cfg(test)]
            trace: Arc::clone(&trace),
            #[cfg(test)]
            probes: tests::Probes::new(_state_dir.join(STORE)),
        };
        runtime.spawn(self::core::release(
            releases,
            jobs,
            failed,
            Arc::clone(&busy),
        ));
        let core = runtime.spawn(core.run(inputs_rx, failed_rx));
        Ok(Self {
            runtime,
            inputs,
            core,
            executor,
            stopping,
            #[cfg(test)]
            busy,
            #[cfg(test)]
            trace,
            _lock: lock,
        })
    }

    /// Serve until `SIGTERM` or `SIGINT`, then stop; or until the daemon
    /// stops by itself, which only a failure makes it do.
    pub fn serve(mut self) -> Result<(), DaemonError> {
        let core = &mut self.core;
        let signalled = self.runtime.block_on(async {
            let (mut term, mut int) = match (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) {
                (Ok(term), Ok(int)) => (term, int),
                (Err(e), _) | (_, Err(e)) => return Err(DaemonError::Spawn(e)),
            };
            tokio::select! {
                result = core => Ok(Some(result)),
                _ = term.recv() => Ok(None),
                _ = int.recv() => Ok(None),
            }
        })?;
        match signalled {
            // The engine task ended by itself.
            Some(ended) => {
                let result = ended.unwrap_or(Err(DaemonError::Stopped));
                self.finish();
                result
            }
            None => {
                info!("signalled: stopping");
                self.stop()
            }
        }
    }

    /// Stop: the engine task returns once it reaches the stop, effects not
    /// yet performed then are dropped (a restart redoes them, as after a
    /// crash), and the writer commits what is queued before the lock goes.
    /// The executor runs on until the engine task has returned, so that its
    /// going cannot read as a failure.
    pub fn stop(mut self) -> Result<(), DaemonError> {
        let inputs = self.inputs.clone();
        let core = &mut self.core;
        let result = self.runtime.block_on(async {
            let _ = inputs.send(Input::Stop).await;
            core.await.unwrap_or(Err(DaemonError::Stopped))
        });
        self.finish();
        result
    }

    /// After the engine task has ended: let the executor go, then the
    /// runtime, then the writer.
    fn finish(self) {
        self.stopping.store(true, Ordering::SeqCst);
        let Self {
            runtime,
            inputs,
            executor,
            ..
        } = self;
        drop(inputs);
        // Ends the releaser, whose sender of jobs is the executor's last.
        runtime.shutdown_timeout(Duration::from_secs(5));
        let _ = executor.join();
    }

    /// How many jobs are handed off and not done.
    #[cfg(test)]
    fn busy(&self) -> usize {
        self.busy.load(Ordering::SeqCst)
    }
}

/// What the engine task emitted and the executor performed, in order: the
/// effects only, by a short label, for the tests that check the order.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Trace {
    emitted: std::sync::Mutex<Vec<String>>,
    performed: std::sync::Mutex<Vec<String>>,
    /// Reports the engine task has handed to the engine.
    pub(crate) handled: AtomicUsize,
}

#[cfg(test)]
impl Trace {
    fn label(job: &executor::Job) -> Option<String> {
        use delocal_engine::{Action, Outbound};
        use executor::Job;
        let action = match job {
            Job::Commit { action, .. } | Job::Outbound(action) => action,
            Job::Observe { path, .. } => return Some(format!("observe {path}")),
            Job::Records { .. }
            | Job::Names { .. }
            | Job::Again { .. }
            | Job::Scan { .. }
            | Job::Space { .. } => {
                return None;
            }
        };
        Some(match action {
            Action::Write { path, .. } => format!("write {path}"),
            Action::Remove { path, .. } => format!("remove {path}"),
            Action::SetMeta { path, .. } => format!("set_meta {path}"),
            Action::MoveToTrash { path, .. } => format!("move_to_trash {path}"),
            Action::Fetch { path, .. } => format!("fetch {path}"),
            Action::Send { payload, .. } => match payload {
                Outbound::Batch(_) => "send batch".to_owned(),
                Outbound::Decision(_) => "send decision".to_owned(),
                Outbound::HaveUpTo { .. } => "send have_up_to".to_owned(),
            },
            other => format!("{other:?}"),
        })
    }

    pub(crate) fn emitted(&self, jobs: &[executor::Job]) {
        let mut emitted = self.emitted.lock().unwrap();
        emitted.extend(jobs.iter().filter_map(Self::label));
    }

    pub(crate) fn performed(&self, job: &executor::Job) {
        if let Some(label) = Self::label(job) {
            self.performed.lock().unwrap().push(label);
        }
    }

    pub(crate) fn emitted_list(&self) -> Vec<String> {
        self.emitted.lock().unwrap().clone()
    }

    pub(crate) fn performed_list(&self) -> Vec<String> {
        self.performed.lock().unwrap().clone()
    }
}
