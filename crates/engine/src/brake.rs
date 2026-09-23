//! The brake (DESIGN.md §8.1): one function, two callers.
//!
//! A batch is held if either rule trips: **H1** (count) when `dels + mods`
//! is at least `hold_count` and at least `hold_pct` percent of the tracked
//! entries before the batch, or **H2** (size) when the bytes of adds and
//! mods exceed `hold_size`. Counting only: no content analysis.
//!
//! The receiver evaluates it over the apply set of an incoming batch, with
//! the current tracked count as the denominator, classifying each item
//! against the local record ([`receiver_summary`]). The sender evaluates
//! the same function over its unannounced local changes, with the tracked
//! count as of its last announcement as the denominator (§8.1), using the
//! [`Summary`] the batch would carry.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::batch::{ApplyItem, ApplyMode, ApplySet, Summary};
use crate::index::Index;
use crate::rules::Rules;

/// Which rule held a batch (§8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HoldReason {
    /// H1: `destructive = dels + mods` against the tracked count before the batch.
    Count { destructive: u64, tracked: usize },
    /// H2: bytes of adds and mods.
    Size { bytes: u64 },
}

impl fmt::Display for HoldReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count {
                destructive,
                tracked,
            } => {
                let pct = if *tracked == 0 {
                    100
                } else {
                    destructive * 100 / *tracked as u64
                };
                write!(
                    f,
                    "count: {destructive} deletes and modifies = {pct}% of {tracked} tracked"
                )
            }
            Self::Size { bytes } => write!(f, "size: {bytes} bytes of adds and modifies"),
        }
    }
}

/// The brake's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Verdict {
    Pass,
    Hold(HoldReason),
}

impl Verdict {
    pub fn is_hold(self) -> bool {
        matches!(self, Self::Hold(_))
    }
}

/// Evaluate H1 and H2 (§8.1). `tracked_before` is the number of tracked
/// entries before the batch: live and not a directory. H1 cannot trip when
/// it is zero, and `hold_count == 0` disables H1.
pub fn evaluate(rules: &Rules, summary: &Summary, tracked_before: usize) -> Verdict {
    let destructive = summary.destructive();
    let h1 = rules.hold_count > 0
        && tracked_before > 0
        && destructive >= rules.hold_count
        && destructive.saturating_mul(100)
            >= u64::from(rules.hold_pct).saturating_mul(tracked_before as u64);
    if h1 {
        return Verdict::Hold(HoldReason::Count {
            destructive,
            tracked: tracked_before,
        });
    }
    if summary.bytes > rules.hold_size {
        return Verdict::Hold(HoldReason::Size {
            bytes: summary.bytes,
        });
    }
    Verdict::Pass
}

/// Classify an apply set against the local records the way §8.1 says the
/// receiver can: a metadata-only or index-only apply counts as nothing; a
/// tombstone over a live record is a del; a live entry over absent or a
/// tombstone is an add; anything else (content, kind or exec differs) is a
/// mod. Bytes are the sizes of counted adds and mods. A conflict item is
/// `M` against the losing local record, so a mod.
pub fn receiver_summary(index: &Index, set: &ApplySet) -> Summary {
    let mut summary = Summary::default();
    for item in &set.items {
        let ApplyItem::Apply { entry, mode, .. } = item;
        if matches!(mode, ApplyMode::MetadataOnly | ApplyMode::IndexOnly) {
            continue;
        }
        let local_live = index.live(&entry.path).is_some();
        match (local_live, entry.deleted) {
            (true, true) => summary.dels += 1,
            (false, true) => {} // a tombstone for something we do not have
            (false, false) => {
                summary.adds += 1;
                summary.bytes += entry.size;
            }
            (true, false) => {
                summary.mods += 1;
                summary.bytes += entry.size;
            }
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules::default()
    }

    fn summary(adds: u64, mods: u64, dels: u64, bytes: u64) -> Summary {
        Summary {
            adds,
            mods,
            dels,
            bytes,
        }
    }

    #[test]
    fn h1_needs_both_the_count_and_the_percentage() {
        // 50 of 1000 is 5%: count reached, percentage not.
        assert_eq!(
            evaluate(&rules(), &summary(0, 0, 50, 0), 1000),
            Verdict::Pass
        );
        // 50 of 200 is exactly 25%: at-or-above trips.
        assert_eq!(
            evaluate(&rules(), &summary(0, 20, 30, 0), 200),
            Verdict::Hold(HoldReason::Count {
                destructive: 50,
                tracked: 200
            })
        );
        // 49 of 100 is 49%: percentage reached, count not.
        assert_eq!(
            evaluate(&rules(), &summary(0, 49, 0, 0), 100),
            Verdict::Pass
        );
        // 51 of 201 is 25.37%: integer arithmetic must not round it down.
        assert!(evaluate(&rules(), &summary(0, 0, 51, 0), 201).is_hold());
        // 50 of 201 is 24.87%: passes.
        assert_eq!(
            evaluate(&rules(), &summary(0, 0, 50, 0), 201),
            Verdict::Pass
        );
    }

    #[test]
    fn adds_never_count_toward_h1() {
        assert_eq!(
            evaluate(&rules(), &summary(10_000, 0, 0, 0), 10),
            Verdict::Pass
        );
    }

    #[test]
    fn h1_cannot_trip_on_an_empty_folder_and_zero_disables_it() {
        assert_eq!(evaluate(&rules(), &summary(0, 0, 500, 0), 0), Verdict::Pass);
        let off = Rules {
            hold_count: 0,
            ..rules()
        };
        assert_eq!(evaluate(&off, &summary(0, 0, 500, 0), 10), Verdict::Pass);
    }

    #[test]
    fn h2_is_strictly_above_and_checked_after_h1() {
        let r = rules();
        assert_eq!(
            evaluate(&r, &summary(1, 0, 0, r.hold_size), 10),
            Verdict::Pass
        );
        assert_eq!(
            evaluate(&r, &summary(1, 0, 0, r.hold_size + 1), 10),
            Verdict::Hold(HoldReason::Size {
                bytes: r.hold_size + 1
            })
        );
        // Both trip: the count reason is reported.
        assert!(matches!(
            evaluate(&r, &summary(0, 100, 0, r.hold_size + 1), 100),
            Verdict::Hold(HoldReason::Count { .. })
        ));
    }

    #[test]
    fn reasons_read_well() {
        assert_eq!(
            HoldReason::Count {
                destructive: 612,
                tracked: 1200
            }
            .to_string(),
            "count: 612 deletes and modifies = 51% of 1200 tracked"
        );
        assert_eq!(
            HoldReason::Size { bytes: 5 }.to_string(),
            "size: 5 bytes of adds and modifies"
        );
        let json = serde_json::to_string(&Verdict::Hold(HoldReason::Size { bytes: 5 })).unwrap();
        assert_eq!(
            serde_json::from_str::<Verdict>(&json).unwrap(),
            Verdict::Hold(HoldReason::Size { bytes: 5 })
        );
    }
}
