//! Per-folder rules (DESIGN.md §8.1 brake thresholds, §6.5 tier size limits).
//!
//! Defaults are the **\[decision\]** values in the design; `delocal rules`
//! changes them per folder (Phase 4). The engine only reads them: the brake
//! (PR 5) uses the `hold_*` fields and the want-list (PR 6) the `*_limit`
//! fields. Nothing here is enforced yet.

use serde::{Deserialize, Serialize};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// Thresholds and limits for one folder.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rules {
    /// H1 count rule: `dels + mods` at or above this, together with the
    /// percentage rule, holds a batch. 0 disables H1 (§8.1).
    pub hold_count: u64,
    /// H1 percentage rule: `dels + mods` at or above this percent of the
    /// folder's tracked entries before the batch (§8.1).
    pub hold_pct: u8,
    /// H2 size rule: total bytes of adds and mods above this holds a batch (§8.1).
    pub hold_size: u64,
    /// Files over this are deferred on a `direct` tier (§6.5).
    pub direct_limit: u64,
    /// Files over this are deferred on a `relay` tier (§6.5).
    pub relay_limit: u64,
}

impl Default for Rules {
    /// §8.1: 50 entries and 25 %, 20 GiB. §6.5: 1 GiB direct, 50 MiB relay.
    fn default() -> Self {
        Self {
            hold_count: 50,
            hold_pct: 25,
            hold_size: 20 * GIB,
            direct_limit: GIB,
            relay_limit: 50 * MIB,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_design() {
        let r = Rules::default();
        assert_eq!(r.hold_count, 50);
        assert_eq!(r.hold_pct, 25);
        assert_eq!(r.hold_size, 21_474_836_480);
        assert_eq!(r.direct_limit, 1_073_741_824);
        assert_eq!(r.relay_limit, 52_428_800);
    }

    #[test]
    fn round_trips() {
        let r = Rules {
            hold_count: 0,
            ..Rules::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Rules>(&json).unwrap(), r);
        let bytes = postcard::to_stdvec(&r).unwrap();
        assert_eq!(postcard::from_bytes::<Rules>(&bytes).unwrap(), r);
    }
}
