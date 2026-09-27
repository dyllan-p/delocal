//! The group-commit writer (DESIGN.md §11): one thread owns the store's
//! connection, takes the [`Group`]s the daemon submits, commits every group
//! that is waiting in one transaction, and says when each is durable.
//!
//! An `fsync` per event would make a bulk import crawl, so while one commit
//! runs the next groups queue up, and the next transaction takes all of
//! them. Nothing waits to fill a transaction: a group submitted to an idle
//! writer is committed at once, alone.
//!
//! **Effects wait for [`Durable`].** [`Writer::submit`] returns a
//! [`Durable`] for the group, and the daemon performs none of the group's
//! effects until it resolves. It resolves `Ok` only after the commit that
//! holds the group has returned, so nothing is reported durable that a
//! crash could still take away. Groups resolve in the order they were
//! submitted, and a group with no writes resolves only once every group
//! before it has: its effects may depend on their writes. `Durable` can be
//! waited on ([`Durable::wait`]) or awaited, since it is a `Future`; it
//! needs no runtime of its own.
//!
//! **A failed commit stops the writer.** If a transaction fails, none of
//! its groups is durable, and every later group is refused
//! ([`WriteError::Poisoned`]) rather than committed: without the failed
//! group, a later one could leave the store holding something that is no
//! prefix of what was submitted, a want without the record it was
//! classified against. The daemon's state is then ahead of its store, and
//! the only way on is a restart from what is durable.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{self, SendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};

use super::{Group, Store, StoreError};

impl Store {
    /// Start the group-commit writer on its own thread; the connection
    /// moves there. [`Writer::close`] gives the store back.
    pub fn writer(self) -> Result<Writer, StoreError> {
        let (jobs, queue) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("delocal-store".to_owned())
            .spawn(move || run(self, &queue))
            .map_err(StoreError::Spawn)?;
        Ok(Writer {
            jobs: Some(jobs),
            thread: Some(thread),
        })
    }
}

/// The daemon's handle on the writer thread. See the module docs.
///
/// Dropping it waits until every group submitted so far is committed.
pub struct Writer {
    /// `None` once closing: the thread sees the queue end and stops.
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<Store>>,
}

/// A group on its way to the writer, and the signal it resolves.
struct Job {
    group: Group,
    done: Signal,
}

impl Writer {
    /// Queue `group` for the next transaction. The group is durable when
    /// the returned [`Durable`] resolves `Ok`.
    pub fn submit(&self, group: Group) -> Durable {
        let (done, durable) = signal();
        let job = Job { group, done };
        match &self.jobs {
            Some(jobs) => {
                if let Err(SendError(job)) = jobs.send(job) {
                    job.done.resolve(Err(WriteError::Stopped));
                }
            }
            None => job.done.resolve(Err(WriteError::Stopped)),
        }
        durable
    }

    /// Commit what is queued, stop the thread and give the store back.
    pub fn close(mut self) -> Result<Store, WriteError> {
        self.stop().ok_or(WriteError::Stopped)
    }

    /// End the queue and join the thread: the store, unless the thread
    /// panicked.
    fn stop(&mut self) -> Option<Store> {
        self.jobs = None;
        self.thread.take()?.join().ok()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The writer thread: commit what is waiting, resolve it, repeat, until
/// the queue ends.
fn run(mut store: Store, queue: &mpsc::Receiver<Job>) -> Store {
    let mut failed: Option<Arc<StoreError>> = None;
    while let Ok(first) = queue.recv() {
        let (groups, signals): (Vec<Group>, Vec<Signal>) = std::iter::once(first)
            .chain(queue.try_iter())
            .map(|job| (job.group, job.done))
            .unzip();
        let outcome = match &failed {
            Some(error) => Err(WriteError::Poisoned(Arc::clone(error))),
            // Nothing to write: durable once the groups before them are,
            // and they are.
            None if groups.iter().all(Group::is_empty) => Ok(()),
            None => store.commit(&groups).map_err(|error| {
                let error = Arc::new(error);
                failed = Some(Arc::clone(&error));
                WriteError::Failed(error)
            }),
        };
        for signal in signals {
            signal.resolve(outcome.clone());
        }
    }
    store
}

/// Why a group is not durable, and never will be.
#[derive(Clone, Debug)]
pub enum WriteError {
    /// The transaction that held the group failed, so nothing in it is
    /// durable.
    Failed(Arc<StoreError>),
    /// An earlier transaction failed with this, and the writer takes
    /// nothing after it (see the module docs).
    Poisoned(Arc<StoreError>),
    /// The writer stopped before it committed the group: its thread
    /// panicked, or it was closed.
    Stopped,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(e) => write!(f, "the commit failed: {e}"),
            Self::Poisoned(e) => {
                write!(f, "an earlier commit failed, so this was not written: {e}")
            }
            Self::Stopped => f.write_str("the store's writer stopped"),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failed(e) | Self::Poisoned(e) => Some(e.as_ref()),
            Self::Stopped => None,
        }
    }
}

/// What a [`Signal`] and its [`Durable`] share.
struct Shared {
    state: Mutex<State>,
    resolved: Condvar,
}

struct State {
    outcome: Option<Result<(), WriteError>>,
    waker: Option<Waker>,
}

/// A pair: the writer's end, and the daemon's.
fn signal() -> (Signal, Durable) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            outcome: None,
            waker: None,
        }),
        resolved: Condvar::new(),
    });
    (Signal(Arc::clone(&shared)), Durable(shared))
}

/// The state, even if a thread panicked while holding the lock: the lock
/// guards two plain assignments, which a panic cannot leave half done.
fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The writer's end of a [`Durable`]. Dropped without being resolved (the
/// writer thread panicked with the group in hand), it resolves as
/// [`WriteError::Stopped`], so nothing waits forever.
struct Signal(Arc<Shared>);

impl Signal {
    fn resolve(self, outcome: Result<(), WriteError>) {
        self.set(outcome);
    }

    /// Resolve, unless already resolved.
    fn set(&self, outcome: Result<(), WriteError>) {
        let waker = {
            let mut state = lock(&self.0);
            if state.outcome.is_some() {
                return;
            }
            state.outcome = Some(outcome);
            state.waker.take()
        };
        self.0.resolved.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        self.set(Err(WriteError::Stopped));
    }
}

/// Resolves when a submitted group is durable, or will never be. See the
/// module docs.
pub struct Durable(Arc<Shared>);

impl Durable {
    /// Block until the group is durable (`Ok`), or will never be.
    pub fn wait(&self) -> Result<(), WriteError> {
        let mut state = lock(&self.0);
        loop {
            if let Some(outcome) = &state.outcome {
                return outcome.clone();
            }
            state = self
                .0
                .resolved
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The outcome if it is known yet, without blocking.
    pub fn outcome(&self) -> Option<Result<(), WriteError>> {
        lock(&self.0).outcome.clone()
    }
}

impl Future for Durable {
    type Output = Result<(), WriteError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = lock(&self.0);
        match &state.outcome {
            Some(outcome) => Poll::Ready(outcome.clone()),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::task::Wake;
    use std::time::Duration;

    use delocal_engine::{Action, HeldState};
    use rusqlite::Connection;

    use super::super::HostWrite;
    use super::super::sample::*;
    use super::*;

    fn open(dir: &tempfile::TempDir) -> Store {
        Store::open(&dir.path().join("delocal.db")).unwrap()
    }

    /// A group that records folder 1.
    fn joined() -> Group {
        let mut g = Group::new(at(0));
        g.host(HostWrite::PutFolder(folder_row(folder(1))));
        g
    }

    /// A group that writes the record at `r<n>`, with `seq` `n`.
    fn record_group(n: u64) -> Group {
        let mut g = Group::new(at(0));
        let action = Action::IndexChanged {
            folder: folder(1),
            record: record(&format!("r{n}"), n),
        };
        assert!(g.push(action).is_none());
        g
    }

    /// The `seq` of every index row, in order.
    fn seqs(store: &Store) -> Vec<u64> {
        let mut stmt = store
            .conn
            .prepare("SELECT seq FROM entries ORDER BY seq")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(|seq| seq.unwrap().cast_unsigned())
            .collect()
    }

    #[test]
    fn groups_become_durable_in_order_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let writer = open(&dir).writer().unwrap();
        let mut durables = vec![writer.submit(joined())];
        durables.extend((1..=200).map(|n| writer.submit(record_group(n))));
        for durable in &durables {
            durable.wait().unwrap();
        }
        drop(writer.close().unwrap());
        assert_eq!(seqs(&open(&dir)), (1..=200).collect::<Vec<_>>());
    }

    /// A second connection holds the write lock, so the writer's
    /// transaction cannot begin, let alone commit, until it lets go.
    struct Lock(Connection);

    impl Lock {
        fn take(dir: &tempfile::TempDir) -> Self {
            let conn = Connection::open(dir.path().join("delocal.db")).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            Self(conn)
        }

        fn release(self) {
            self.0.execute_batch("COMMIT").unwrap();
        }
    }

    /// A store whose writer waits for a lock instead of failing on it.
    fn patient(dir: &tempfile::TempDir) -> Store {
        let store = open(dir);
        store.conn.busy_timeout(Duration::from_secs(30)).unwrap();
        store
    }

    #[test]
    fn nothing_is_reported_durable_until_the_commit_returns() {
        let dir = tempfile::tempdir().unwrap();
        let writer = patient(&dir).writer().unwrap();
        writer.submit(joined()).wait().unwrap();

        let lock = Lock::take(&dir);
        let written = writer.submit(record_group(1));
        // Nothing to write, but after a group that has something.
        let empty = writer.submit(Group::new(at(0)));
        thread::sleep(Duration::from_millis(200));
        assert!(written.outcome().is_none(), "reported durable while held");
        assert!(
            empty.outcome().is_none(),
            "an empty group overtook one before it"
        );

        lock.release();
        written.wait().unwrap();
        empty.wait().unwrap();
        drop(writer.close().unwrap());
        assert_eq!(seqs(&open(&dir)), [1]);
    }

    #[test]
    fn an_empty_group_on_an_idle_writer_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let writer = open(&dir).writer().unwrap();
        writer.submit(Group::new(at(0))).wait().unwrap();
    }

    #[test]
    fn a_failed_commit_refuses_every_later_group() {
        let dir = tempfile::tempdir().unwrap();
        let writer = open(&dir).writer().unwrap();
        writer.submit(joined()).wait().unwrap();
        writer.submit(record_group(1)).wait().unwrap();

        // A held row for a folder that was never recorded breaks a foreign
        // key.
        let mut bad = Group::new(at(0));
        let action = Action::HeldChanged {
            folder: folder(2),
            batch: batch_id(1),
            state: HeldState::Held,
            row: Some(Box::new(held_row(1))),
        };
        assert!(bad.push(action).is_none());
        assert!(matches!(
            writer.submit(bad).wait(),
            Err(WriteError::Failed(_))
        ));
        assert!(matches!(
            writer.submit(record_group(2)).wait(),
            Err(WriteError::Poisoned(_))
        ));
        assert!(matches!(
            writer.submit(Group::new(at(0))).wait(),
            Err(WriteError::Poisoned(_))
        ));
        drop(writer.close().unwrap());
        assert_eq!(seqs(&open(&dir)), [1]);
    }

    #[test]
    fn dropping_the_writer_commits_what_was_submitted() {
        let dir = tempfile::tempdir().unwrap();
        let writer = open(&dir).writer().unwrap();
        let first = writer.submit(joined());
        let durables: Vec<Durable> = (1..=50).map(|n| writer.submit(record_group(n))).collect();
        drop(writer);
        first.outcome().unwrap().unwrap();
        for durable in durables {
            durable.outcome().unwrap().unwrap();
        }
        assert_eq!(seqs(&open(&dir)), (1..=50).collect::<Vec<_>>());
    }

    #[test]
    fn a_signal_dropped_unresolved_resolves_as_stopped() {
        let (writer_end, durable) = signal();
        assert!(durable.outcome().is_none());
        drop(writer_end);
        assert!(matches!(durable.wait(), Err(WriteError::Stopped)));
        // Resolving twice keeps the first outcome.
        let (writer_end, durable) = signal();
        writer_end.set(Ok(()));
        drop(writer_end);
        assert!(durable.wait().is_ok());
    }

    /// The smallest executor: poll, and park the thread until woken.
    fn block_on<F: Future>(future: F) -> F::Output {
        struct Unpark(thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
            thread::park();
        }
    }

    #[test]
    fn durable_can_be_awaited() {
        let dir = tempfile::tempdir().unwrap();
        let writer = patient(&dir).writer().unwrap();
        block_on(writer.submit(joined())).unwrap();
        let lock = Lock::take(&dir);
        let durable = writer.submit(record_group(7));
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            lock.release();
        });
        // Pending at first, then woken by the writer once it commits.
        block_on(durable).unwrap();
        releaser.join().unwrap();
        drop(writer.close().unwrap());
        assert_eq!(seqs(&open(&dir)), [7]);
    }
}
