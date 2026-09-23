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

/// A fetch with no progress reported for this long is stalled and the want
/// returns to *wanted* (§7.5).
pub const FETCH_STALL_NANOS: i64 = 60 * NANOS_PER_SECOND;

/// A commit not reported within this long is overdue (§7.5).
pub const COMMIT_DEADLINE_NANOS: i64 = 30 * NANOS_PER_SECOND;

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

/// Format nanoseconds since the Unix epoch as `YYYYMMDD-HHMMSS` in UTC,
/// the timestamp inside a conflict-copy name (§7.6).
///
/// Civil date from days per Howard Hinnant's algorithm; no calendar crate
/// is in Appendix A and twenty lines is cheaper than one. Negative inputs
/// (before 1970) work; the `i64` nanosecond range covers 1677 to 2262.
pub fn format_utc_compact(nanos: i64) -> String {
    let secs = nanos.div_euclid(NANOS_PER_SECOND);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Proleptic Gregorian (year, month, day) for a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era, 0..146096
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // year of era
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, March-based
    let mp = (5 * doy + 2) / 153; // month index, March = 0
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_table() {
        let s = NANOS_PER_SECOND;
        let cases: [(i64, &str); 11] = [
            (0, "19700101-000000"),
            (-s, "19691231-235959"),
            (951_782_400 * s, "20000229-000000"), // 2000 is a leap year (400 rule)
            (1_709_251_199 * s, "20240229-235959"), // 2024-02-29
            (1_709_251_200 * s, "20240301-000000"),
            (2_147_483_647 * s, "20380119-031407"), // i32 seconds rollover
            (2_147_483_648 * s, "20380119-031408"),
            (4_107_542_399 * s, "21000228-235959"), // 2100 is not a leap year
            (4_107_542_400 * s, "21000301-000000"),
            (1_758_551_405 * s + 999_999_999, "20250922-143005"), // sub-second part ignored
            (i64::MAX, "22620411-234716"),
        ];
        for (nanos, want) in cases {
            assert_eq!(format_utc_compact(nanos), want, "nanos = {nanos}");
        }
    }

    #[test]
    fn civil_from_days_round_trips_over_a_wide_range() {
        // Walk day by day across several century boundaries and check the
        // calendar advances by exactly one day each time.
        let mut prev = civil_from_days(-150_000); // ~1559
        for day in -149_999..200_000 {
            let (y, m, d) = civil_from_days(day);
            let days_in_month = match m {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                4 | 6 | 9 | 11 => 30,
                _ => {
                    if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                        29
                    } else {
                        28
                    }
                }
            };
            let expected = if prev.2 < days_in_month_of(prev) {
                (prev.0, prev.1, prev.2 + 1)
            } else if prev.1 < 12 {
                (prev.0, prev.1 + 1, 1)
            } else {
                (prev.0 + 1, 1, 1)
            };
            assert_eq!((y, m, d), expected, "day {day}");
            assert!(d <= days_in_month);
            prev = (y, m, d);
        }
    }

    fn days_in_month_of((y, m, _): (i64, u32, u32)) -> u32 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ => {
                if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                    29
                } else {
                    28
                }
            }
        }
    }

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
        assert_eq!(FETCH_STALL_NANOS, 60_000_000_000);
        assert_eq!(COMMIT_DEADLINE_NANOS, 30_000_000_000);
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
