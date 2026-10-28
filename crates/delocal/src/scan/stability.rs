//! The stability check (DESIGN.md §7.3, draft 66): whether a file has been
//! left alone long enough to hash.
//!
//! **By the real-time clock.** A file is hashed only once its inode change
//! time (`ctime`) is at least 2 s old by the real-time clock
//! (`CLOCK_REALTIME`), the clock the kernel set it by. Not by the engine's
//! clock (§7.8), which is the real-time clock at start plus the monotonic
//! time since: that one stops while a laptop sleeps, and keeps counting from
//! wherever the real-time clock was at start when NTP corrects it later, so
//! after either it disagrees with every `ctime` the kernel writes. A file
//! changed after a night's sleep would have looked changed in the future
//! until the machine had been awake for as long again.
//!
//! **Two looks when `ctime` is in the future.** If `ctime` is ahead of the
//! real-time clock, the clock was set back since the file changed, and the
//! file's age says nothing. The host judges by what it saw instead: the
//! file is stable once two looks at it, at least 2 s apart by the monotonic
//! clock, find it unchanged (its size, mtime and `ctime`). The first look
//! is kept here, per path, and every look until then is `Skipped {
//! Unstable }`, which has the host look again 2 s later. A look that finds
//! the file changed starts again from that look. A path whose `ctime` is
//! no longer in the future drops its first look; one that is deleted
//! meanwhile keeps it, a few dozen bytes, until the daemon restarts.
//!
//! Either way the hash itself is checked as before: the file is stated
//! again after it is hashed, and a change in its size, mtime or `ctime`
//! discards the hash ([`super::hash`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use delocal_engine::{RelPath, Timestamp};

use super::Clock;
use super::hash::{SETTLE_NANOS, settled};
use crate::fs::Stat;

/// The real-time clock, `CLOCK_REALTIME`: the clock the kernel sets
/// `ctime` by, as Unix nanoseconds.
pub fn realtime() -> Timestamp {
    let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
        // A clock set before 1970.
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    };
    Timestamp::from_unix_nanos(nanos)
}

/// One folder's stability check. See the module docs.
pub struct Stability {
    /// The real-time clock; [`realtime`] but in tests.
    realtime: Arc<dyn Clock + Send + Sync>,
    /// The first look at each file whose `ctime` was in the future.
    looks: Mutex<BTreeMap<RelPath, Look>>,
}

/// What a look at a file saw, and when by the monotonic clock.
#[derive(Clone, Copy, Debug)]
struct Look {
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    at: Instant,
}

impl Look {
    fn of(stat: &Stat, at: Instant) -> Self {
        Self {
            size: stat.size,
            mtime_ns: stat.mtime_ns,
            ctime_ns: stat.ctime_ns,
            at,
        }
    }

    fn same(&self, stat: &Stat) -> bool {
        (self.size, self.mtime_ns, self.ctime_ns) == (stat.size, stat.mtime_ns, stat.ctime_ns)
    }
}

impl Stability {
    /// A check by the real-time clock `realtime`.
    pub fn new(realtime: Arc<dyn Clock + Send + Sync>) -> Self {
        Self {
            realtime,
            looks: Mutex::default(),
        }
    }

    /// A check by this machine's real-time clock.
    pub fn real() -> Self {
        Self::new(Arc::new(realtime as fn() -> Timestamp))
    }

    /// Whether the file at `path`, stated as `stat` just now, has been left
    /// alone long enough to hash. See the module docs.
    pub fn settled(&self, path: &RelPath, stat: &Stat) -> bool {
        self.settled_at(path, stat, Instant::now())
    }

    /// [`Stability::settled`], with `now` the monotonic clock's reading.
    fn settled_at(&self, path: &RelPath, stat: &Stat, now: Instant) -> bool {
        let real = self.realtime.now();
        let mut looks = self.looks.lock().unwrap_or_else(PoisonError::into_inner);
        if stat.ctime_ns <= real.as_unix_nanos() {
            looks.remove(path);
            return settled(stat.ctime_ns, real);
        }
        match looks.get(path) {
            Some(first) if first.same(stat) => now.duration_since(first.at) >= SETTLE,
            _ => {
                looks.insert(path.clone(), Look::of(stat, now));
                false
            }
        }
    }
}

/// How far apart two looks must be (§7.3).
const SETTLE: Duration = Duration::from_nanos(SETTLE_NANOS.unsigned_abs());

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;
    use crate::fs::FileKind;

    const SECOND: i64 = 1_000_000_000;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn stat(ctime_ns: i64, size: u64) -> Stat {
        Stat {
            kind: FileKind::File,
            size,
            mtime_ns: 1,
            ctime_ns,
            mode: 0o644,
            dev: 1,
            ino: 1,
        }
    }

    /// A check whose real-time clock reads `real`, which the test moves.
    fn check(real: &Arc<AtomicI64>) -> Stability {
        let real = Arc::clone(real);
        Stability::new(Arc::new(move || {
            Timestamp::from_unix_nanos(real.load(Ordering::SeqCst))
        }))
    }

    #[test]
    fn a_change_time_in_the_past_is_judged_by_its_age_on_the_real_time_clock() {
        let real = Arc::new(AtomicI64::new(1_000 * SECOND));
        let stable = check(&real);
        let now = Instant::now();
        let f = p("f");
        assert!(stable.settled_at(&f, &stat(998 * SECOND, 3), now));
        assert!(!stable.settled_at(&f, &stat(998 * SECOND + 1, 3), now));
        assert!(!stable.settled_at(&f, &stat(1_000 * SECOND, 3), now));
        // The real-time clock moves on, and the same file has settled.
        real.store(1_003 * SECOND, Ordering::SeqCst);
        assert!(stable.settled_at(&f, &stat(1_000 * SECOND, 3), now));
    }

    #[test]
    fn a_change_time_in_the_future_needs_two_unchanged_looks_two_seconds_apart() {
        // The real-time clock was set back a minute after the file changed.
        let real = Arc::new(AtomicI64::new(1_000 * SECOND));
        let stable = check(&real);
        let first = Instant::now();
        let at = |ms: u64| first + Duration::from_millis(ms);
        let f = p("f");
        let future = stat(1_060 * SECOND, 3);
        assert!(!stable.settled_at(&f, &future, at(0)), "the first look");
        assert!(!stable.settled_at(&f, &future, at(1_999)));
        assert!(stable.settled_at(&f, &future, at(2_000)));
        assert!(stable.settled_at(&f, &future, at(5_000)));

        // A look that finds it changed starts again from that look.
        let g = p("g");
        assert!(!stable.settled_at(&g, &stat(1_060 * SECOND, 3), at(0)));
        let grown = stat(1_061 * SECOND, 4);
        assert!(!stable.settled_at(&g, &grown, at(2_500)));
        assert!(!stable.settled_at(&g, &grown, at(4_499)));
        assert!(stable.settled_at(&g, &grown, at(4_500)));

        // Each path has its own first look.
        let h = p("h");
        assert!(!stable.settled_at(&h, &future, at(5_000)));
    }

    #[test]
    fn a_change_time_that_is_no_longer_in_the_future_is_judged_by_its_age_again() {
        let real = Arc::new(AtomicI64::new(1_000 * SECOND));
        let stable = check(&real);
        let now = Instant::now();
        let f = p("f");
        let changed = stat(1_001 * SECOND, 3);
        assert!(!stable.settled_at(&f, &changed, now));
        // The clock catches up past the change: its age decides, and the
        // first look is gone.
        real.store(1_001 * SECOND + SECOND / 2, Ordering::SeqCst);
        assert!(!stable.settled_at(&f, &changed, now + SETTLE * 10));
        real.store(1_003 * SECOND, Ordering::SeqCst);
        assert!(stable.settled_at(&f, &changed, now));
        assert!(stable.looks.lock().unwrap().is_empty());
    }

    #[test]
    fn the_real_time_clock_is_the_system_s() {
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let read = realtime().as_unix_nanos();
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert!(read >= i64::try_from(before.as_nanos()).unwrap());
        assert!(read <= i64::try_from(after.as_nanos()).unwrap());
    }
}
