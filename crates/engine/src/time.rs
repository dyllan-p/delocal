//! Time as an input (DESIGN.md §7, §7.4, §7.8).
//!
//! The engine never reads a clock. Every call to `Engine::handle` is stamped
//! by the host with a [`Timestamp`], and the only arithmetic the engine does
//! on it is the batch window rule of §7.4: a batch forms after 2 s of quiet
//! or 10 s after the first change, whichever comes first. Wall-clock time
//! never orders events (§7.8); version vectors do.

use serde::{Deserialize, Serialize};

/// Nanoseconds in one second.
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Quiet period after the last change before a batch forms (§7.4).
pub const DEBOUNCE_NANOS: i64 = 2 * NANOS_PER_SECOND;

/// Longest a batch window stays open after its first change (§7.4).
pub const WINDOW_NANOS: i64 = 10 * NANOS_PER_SECOND;

/// A point in time as nanoseconds since the Unix epoch, supplied by the host.
///
/// The same representation as `mtime_ns` on entries, so the two compare
/// directly. Ordered, so `min` and `max` work.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Wrap nanoseconds since the Unix epoch.
    pub const fn from_unix_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the Unix epoch.
    pub const fn as_unix_nanos(self) -> i64 {
        self.0
    }

    /// This time plus `nanos`, saturating at the ends of the range.
    pub const fn plus_nanos(self, nanos: i64) -> Self {
        Self(self.0.saturating_add(nanos))
    }

    /// Nanoseconds from `earlier` to `self`; negative if `self` is earlier.
    pub const fn since(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_and_ordering() {
        let t = Timestamp::from_unix_nanos(100);
        assert_eq!(t.plus_nanos(50).as_unix_nanos(), 150);
        assert_eq!(t.plus_nanos(50).since(t), 50);
        assert_eq!(t.since(t.plus_nanos(50)), -50);
        assert!(t < t.plus_nanos(1));
        assert_eq!(Timestamp::default().as_unix_nanos(), 0);
    }

    #[test]
    fn saturates_at_the_ends() {
        let max = Timestamp::from_unix_nanos(i64::MAX);
        assert_eq!(max.plus_nanos(1), max);
        let min = Timestamp::from_unix_nanos(i64::MIN);
        assert_eq!(min.plus_nanos(-1), min);
        assert_eq!(max.since(min), i64::MAX);
    }

    #[test]
    fn constants_are_what_the_design_says() {
        assert_eq!(DEBOUNCE_NANOS, 2_000_000_000);
        assert_eq!(WINDOW_NANOS, 10_000_000_000);
    }

    #[test]
    fn serde_is_a_plain_integer() {
        let t = Timestamp::from_unix_nanos(1_700_000_000_000_000_000);
        assert_eq!(serde_json::to_string(&t).unwrap(), "1700000000000000000");
        assert_eq!(
            serde_json::from_str::<Timestamp>("42")
                .unwrap()
                .as_unix_nanos(),
            42
        );
        let bytes = postcard::to_stdvec(&t).unwrap();
        assert_eq!(postcard::from_bytes::<Timestamp>(&bytes).unwrap(), t);
    }
}
