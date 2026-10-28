//! The daemon's clock (DESIGN.md §7.8): the `now` the host hands the engine.
//!
//! It is the wall clock sampled once, at daemon start, plus the monotonic
//! time elapsed since. So it reads as Unix nanoseconds, like an mtime, but
//! never runs backwards or leaps within a run: the batch window, the stall
//! deadline and the backoffs are safe from a clock adjustment, and a
//! tombstone's `mtime_ns` or a batch's `created_at` is off by at most the
//! drift since start. Every observation, commit report and event is dated by
//! it, which is what lets the engine compare their times (§7.3).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use delocal_engine::Timestamp;

use crate::scan;

/// See the module docs.
#[derive(Debug)]
pub struct Clock {
    /// The wall clock at start, in nanoseconds since the Unix epoch.
    wall: i64,
    /// The monotonic clock at the same moment.
    start: Instant,
}

impl Clock {
    /// Sample the wall clock now, and count from here.
    pub fn start() -> Self {
        let start = Instant::now();
        let wall = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => nanos(since),
            // A clock set before 1970: nanoseconds before the epoch.
            Err(e) => -nanos(e.duration()),
        };
        Self { wall, start }
    }

    /// The wall clock at start plus the monotonic time elapsed since.
    pub fn now(&self) -> Timestamp {
        Timestamp::from_unix_nanos(self.wall.saturating_add(nanos(self.start.elapsed())))
    }

    /// The monotonic instant at which [`Clock::now`] reads `at`: when to
    /// wake for it. A time before the start is the start, which has passed.
    pub fn instant(&self, at: Timestamp) -> Instant {
        let after = at.since(Timestamp::from_unix_nanos(self.wall));
        let after = Duration::from_nanos(u64::try_from(after).unwrap_or(0));
        self.start.checked_add(after).unwrap_or(self.start)
    }
}

impl scan::Clock for Clock {
    fn now(&self) -> Timestamp {
        Clock::now(self)
    }
}

/// A duration in nanoseconds, saturating: `i64` nanoseconds run out after
/// 292 years.
fn nanos(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_starts_at_the_wall_clock_and_never_runs_backwards() {
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let clock = Clock::start();
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let first = clock.now().as_unix_nanos();
        assert!(first >= nanos(before), "{first} before {before:?}");
        // Within a second of the wall clock read just after.
        assert!(first < nanos(after) + 1_000_000_000);
        let mut last = clock.now();
        for _ in 0..10_000 {
            let now = clock.now();
            assert!(now >= last);
            last = now;
        }
    }

    #[test]
    fn an_instant_is_when_the_clock_reads_that_time() {
        let clock = Clock::start();
        let now = clock.now();
        let soon = now.plus_nanos(250_000_000);
        let at = clock.instant(soon);
        let gap = at.duration_since(clock.start);
        let expected = soon.since(Timestamp::from_unix_nanos(clock.wall));
        assert_eq!(nanos(gap), expected);
        // A time before the start wakes at once.
        assert_eq!(clock.instant(now.plus_nanos(-10_000_000_000)), clock.start);
        assert_eq!(
            clock.instant(Timestamp::from_unix_nanos(i64::MIN)),
            clock.start
        );
    }
}
