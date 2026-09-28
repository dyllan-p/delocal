//! Work on several threads, with the results handed back in the order the
//! work was handed out (DESIGN.md §7.3: hashing is parallel across files).
//!
//! A scan walks on one thread and hashes on several. The engine does not
//! need the reports in any particular order, but a scan whose reports came
//! in whatever order the hashing threads finished would differ from run to
//! run for the same tree, and so would the `seq` order of the changes it
//! finds. [`Ordered`] keeps the walk's order: each item gets the next number
//! as it is handed in, either as a result that needs no work
//! ([`Ordered::ready`]) or as a job for a thread ([`Ordered::submit`]), and
//! results are handed back strictly by number, a finished result waiting
//! for the ones before it.
//!
//! **Bounded.** At most [`WINDOW`] items are ever between being handed in
//! and being handed back: when a long job holds up the ones behind it,
//! `submit` waits for it rather than let the waiting results grow with the
//! size of the tree. Jobs queue two per thread, so a walk runs a little
//! ahead of the hashing and no further.
//!
//! **A job that panics** is handed back as `None` in its place, so the
//! results behind it are not stranded and nothing waits forever. Its thread
//! ends, the panic reaches the caller when the scope the threads run in
//! ends, and if every thread has ended, every job still outstanding is
//! handed back as `None`. A panic is a bug; this only keeps it from turning
//! into a hang.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::Scope;

/// The most items between being handed in and being handed back.
pub const WINDOW: u64 = 1024;

/// Jobs waiting for a thread, per thread.
const QUEUED_PER_THREAD: usize = 2;

/// Work on several threads, results in order. See the module docs.
pub struct Ordered<J, R> {
    /// `None` once [`finish`](Self::finish) has begun, so the threads stop
    /// when the queue is empty.
    jobs: Option<SyncSender<(u64, J)>>,
    results: Receiver<(u64, Option<R>)>,
    /// Results that came back before the ones ahead of them.
    waiting: BTreeMap<u64, Option<R>>,
    /// The number the next item will get.
    next: u64,
    /// The number of the next item to hand back.
    handed_back: u64,
}

impl<J: Send, R: Send> Ordered<J, R> {
    /// Start `threads` threads in `scope`, each doing `work` on one job at a
    /// time.
    pub fn start<'scope, 'env, W>(
        scope: &'scope Scope<'scope, 'env>,
        threads: NonZeroUsize,
        work: &'scope W,
    ) -> Self
    where
        W: Fn(J) -> R + Sync,
        J: 'scope,
        R: 'scope,
    {
        let (jobs, queue) = mpsc::sync_channel(threads.get() * QUEUED_PER_THREAD);
        let (reply, results) = mpsc::channel();
        let queue = Arc::new(Mutex::new(queue));
        for _ in 0..threads.get() {
            let queue = Arc::clone(&queue);
            let reply = reply.clone();
            scope.spawn(move || worker(&queue, &reply, work));
        }
        Self {
            jobs: Some(jobs),
            results,
            waiting: BTreeMap::new(),
            next: 0,
            handed_back: 0,
        }
    }

    /// A result that needs no work, in its place. Waits while the window is
    /// full, and hands back to `out` whatever is ready.
    pub fn ready(&mut self, result: R, out: &mut impl FnMut(Option<R>)) {
        let number = self.number(out);
        self.waiting.insert(number, Some(result));
        self.hand_back(out);
    }

    /// A job for a thread, its result in its place. Waits while the window
    /// is full or the queue is, and hands back to `out` whatever is ready.
    pub fn submit(&mut self, job: J, out: &mut impl FnMut(Option<R>)) {
        let number = self.number(out);
        let sent = self
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send((number, job)).is_ok());
        if !sent {
            // Every thread has ended, so no one will do it.
            self.waiting.insert(number, None);
        }
        self.hand_back(out);
    }

    /// Wait for every job and hand back everything still to come.
    pub fn finish(mut self, out: &mut impl FnMut(Option<R>)) {
        self.jobs = None;
        while self.handed_back < self.next {
            self.receive(out);
        }
    }

    /// The next item's number, once the window has room for it.
    fn number(&mut self, out: &mut impl FnMut(Option<R>)) -> u64 {
        while self.next - self.handed_back >= WINDOW {
            self.receive(out);
        }
        let number = self.next;
        self.next += 1;
        number
    }

    /// Take the results that have come back, and hand back every one whose
    /// turn it is.
    fn hand_back(&mut self, out: &mut impl FnMut(Option<R>)) {
        while let Ok((number, result)) = self.results.try_recv() {
            self.waiting.insert(number, result);
        }
        while let Some(result) = self.waiting.remove(&self.handed_back) {
            self.handed_back += 1;
            out(result);
        }
    }

    /// Wait for one result, then hand back what is ready. Called only while
    /// the next item to hand back is a job still out, so waiting is right.
    fn receive(&mut self, out: &mut impl FnMut(Option<R>)) {
        match self.results.recv() {
            Ok((number, result)) => {
                self.waiting.insert(number, result);
            }
            // Every thread has ended: nothing more will come back.
            Err(_) => {
                for number in self.handed_back..self.next {
                    self.waiting.entry(number).or_insert(None);
                }
            }
        }
        self.hand_back(out);
    }
}

/// One thread: take a job, do it, send back the result, until the queue
/// closes.
fn worker<J, R>(
    queue: &Mutex<Receiver<(u64, J)>>,
    reply: &Sender<(u64, Option<R>)>,
    work: &impl Fn(J) -> R,
) {
    loop {
        // The lock is never held while working, so a panic cannot poison
        // it; carry on regardless.
        let next = queue.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok((number, job)) = next else {
            return;
        };
        let answer = Answer {
            number,
            reply,
            sent: false,
        };
        answer.send(work(job));
    }
}

/// A job's result on its way back. Dropped unsent, which only a panic in
/// the job does, it sends `None` in the result's place.
struct Answer<'a, R> {
    number: u64,
    reply: &'a Sender<(u64, Option<R>)>,
    sent: bool,
}

impl<R> Answer<'_, R> {
    fn send(mut self, result: R) {
        self.sent = true;
        // The receiver is gone only if the caller is unwinding.
        let _ = self.reply.send((self.number, Some(result)));
    }
}

impl<R> Drop for Answer<'_, R> {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.reply.send((self.number, None));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use proptest::prelude::*;

    use super::*;

    fn threads(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    /// An item: a result that needs no work, or a job that sleeps this many
    /// microseconds first.
    #[derive(Clone, Debug)]
    enum Item {
        Ready,
        Job(u64),
    }

    /// Hand in `items` in order, numbered, and return what came back.
    fn run(items: &[Item], n: usize) -> Vec<Option<usize>> {
        let work = |(index, micros): (usize, u64)| {
            std::thread::sleep(Duration::from_micros(micros));
            index
        };
        let mut out = Vec::new();
        std::thread::scope(|scope| {
            let mut ordered = Ordered::start(scope, threads(n), &work);
            let mut push = |r| out.push(r);
            for (index, item) in items.iter().enumerate() {
                match item {
                    Item::Ready => ordered.ready(index, &mut push),
                    Item::Job(micros) => ordered.submit((index, *micros), &mut push),
                }
            }
            ordered.finish(&mut push);
        });
        out
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Whatever the threads and however long each job takes, results
        /// come back in the order they were handed in, each exactly once.
        #[test]
        fn results_come_back_in_order(
            items in prop::collection::vec(
                prop_oneof![Just(Item::Ready), (0..300u64).prop_map(Item::Job)],
                0..200,
            ),
            n in 1..8usize,
        ) {
            let expected: Vec<Option<usize>> = (0..items.len()).map(Some).collect();
            prop_assert_eq!(run(&items, n), expected);
        }
    }

    #[test]
    fn a_long_job_holds_back_at_most_the_window() {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        // Job 0 waits to be released; the rest are instant.
        let work = |index: u64| {
            if index == 0 {
                let _ = released.lock().unwrap().recv();
            }
            index
        };
        let handed_in = AtomicU64::new(0);
        let seen_while_held = AtomicU64::new(0);
        let mut out = Vec::new();
        std::thread::scope(|scope| {
            let mut ordered = Ordered::start(scope, threads(2), &work);
            // Once the hander-in has had time to fill the window, note how
            // far it got, then release job 0.
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                seen_while_held.store(handed_in.load(Ordering::SeqCst), Ordering::SeqCst);
                release.send(()).unwrap();
            });
            let mut push = |r| out.push(r);
            for index in 0..3 * WINDOW {
                if index % 2 == 0 {
                    ordered.submit(index, &mut push);
                } else {
                    ordered.ready(index, &mut push);
                }
                handed_in.fetch_add(1, Ordering::SeqCst);
            }
            ordered.finish(&mut push);
        });
        assert_eq!(seen_while_held.load(Ordering::SeqCst), WINDOW);
        let expected: Vec<Option<u64>> = (0..3 * WINDOW).map(Some).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn a_job_that_panics_comes_back_as_none_and_nothing_hangs() {
        let work = |index: usize| {
            assert_ne!(index % 5, 3, "job {index} panics");
            index
        };
        let mut out = Vec::new();
        let scope = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::thread::scope(|scope| {
                let mut ordered = Ordered::start(scope, threads(3), &work);
                let mut push = |r| out.push(r);
                for index in 0..40 {
                    ordered.submit(index, &mut push);
                }
                ordered.finish(&mut push);
            });
        }));
        assert!(scope.is_err(), "the panic reaches the caller");
        assert_eq!(out.len(), 40, "every job came back");
        for (index, result) in out.iter().enumerate() {
            // Job 3 panics on its thread. Later jobs whose number ends in 3
            // or 8 panic too, while a thread is left; once all three are
            // gone, everything still out comes back as None.
            match result {
                Some(i) => assert_eq!(*i, index),
                None => assert!(index >= 3, "job {index}"),
            }
        }
        assert_eq!(out[3], None);
        assert_eq!(out[..3], [Some(0), Some(1), Some(2)]);
    }
}
