//! Version vectors (DESIGN.md §7.2).
//!
//! Every entry carries a `Version`: a map from node ID to a counter. Missing
//! keys read as 0. Three operations, all pure:
//!
//! - [`Version::compare`]: equal, dominates, dominated or concurrent.
//! - [`Version::merge`]: component-wise maximum, no increment. Used when
//!   concurrent versions have identical content (§7.6) and as the first step
//!   of `W′` (§7.6) and `deny` (§8.2).
//! - [`Version::incremented`]: the local-change rule, `v[self] = v[self] + 1`
//!   on the entry's own vector. There is no counter outside the vector.
//!
//! Wall-clock time never participates (§7.8).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::id::NodeId;

/// How two versions relate (§7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Relation {
    /// Same counters for every node.
    Equal,
    /// `self` is at least `other` everywhere and greater somewhere.
    Dominates,
    /// `other` is at least `self` everywhere and greater somewhere.
    Dominated,
    /// Each is greater somewhere: a conflict unless content is identical (§7.6).
    Concurrent,
}

impl Relation {
    /// The relation seen from the other side: `a.compare(b).flip() == b.compare(a)`.
    pub const fn flip(self) -> Self {
        match self {
            Self::Dominates => Self::Dominated,
            Self::Dominated => Self::Dominates,
            other => other,
        }
    }

    /// True for `Dominates` and `Equal`: "we already have at least this".
    pub const fn dominates_or_equal(self) -> bool {
        matches!(self, Self::Dominates | Self::Equal)
    }
}

/// A version vector: node ID to counter, missing keys read as 0.
///
/// Zero counters are never stored, so the derived equality matches
/// [`Relation::Equal`]. Deliberately not `Ord`: the natural order on
/// versions is partial, and a total order here would invite misuse.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "BTreeMap<NodeId, u64>", into = "BTreeMap<NodeId, u64>")]
pub struct Version(BTreeMap<NodeId, u64>);

impl Version {
    /// The version of an entry nobody has touched. Dominated by everything else.
    pub fn empty() -> Self {
        Self::default()
    }

    /// True if no node has a non-zero counter.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// This node's counter, 0 if absent.
    pub fn counter(&self, node: NodeId) -> u64 {
        self.0.get(&node).copied().unwrap_or(0)
    }

    /// Nodes with a non-zero counter, in byte order, with their counters.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, u64)> + '_ {
        self.0.iter().map(|(n, c)| (*n, *c))
    }

    /// Compare two versions (§7.2). Missing keys read as 0.
    pub fn compare(&self, other: &Self) -> Relation {
        let mut self_greater = false;
        let mut other_greater = false;
        for (node, mine) in &self.0 {
            let theirs = other.counter(*node);
            if *mine > theirs {
                self_greater = true;
            } else if *mine < theirs {
                other_greater = true;
            }
        }
        for node in other.0.keys() {
            if !self.0.contains_key(node) {
                // Ours is 0 and theirs is stored, so theirs is non-zero.
                other_greater = true;
            }
        }
        match (self_greater, other_greater) {
            (false, false) => Relation::Equal,
            (true, false) => Relation::Dominates,
            (false, true) => Relation::Dominated,
            (true, true) => Relation::Concurrent,
        }
    }

    /// `self` dominates `other` strictly.
    pub fn dominates(&self, other: &Self) -> bool {
        self.compare(other) == Relation::Dominates
    }

    /// `self` dominates or equals `other`.
    pub fn dominates_or_equals(&self, other: &Self) -> bool {
        self.compare(other).dominates_or_equal()
    }

    /// Component-wise maximum, no increment (§7.2).
    ///
    /// Commutative, associative and idempotent, so machines merging the same
    /// pair independently converge without talking.
    pub fn merge(&self, other: &Self) -> Self {
        let mut out = self.0.clone();
        for (node, theirs) in &other.0 {
            let slot = out.entry(*node).or_insert(0);
            *slot = (*slot).max(*theirs);
        }
        Self(out)
    }

    /// The local-change rule (§7.2): this vector with `node`'s counter one
    /// higher. Missing key reads as 0, so a first change yields 1.
    ///
    /// Saturates at `u64::MAX` rather than panic. Reaching it would take
    /// more changes to one path than any machine will ever make.
    pub fn incremented(&self, node: NodeId) -> Self {
        let mut out = self.0.clone();
        let slot = out.entry(node).or_insert(0);
        *slot = slot.saturating_add(1);
        Self(out)
    }
}

impl From<BTreeMap<NodeId, u64>> for Version {
    /// Zero counters are dropped so equality and `compare` agree.
    fn from(map: BTreeMap<NodeId, u64>) -> Self {
        Self(map.into_iter().filter(|(_, c)| *c > 0).collect())
    }
}

impl From<Version> for BTreeMap<NodeId, u64> {
    fn from(v: Version) -> Self {
        v.0
    }
}

impl FromIterator<(NodeId, u64)> for Version {
    fn from_iter<I: IntoIterator<Item = (NodeId, u64)>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<BTreeMap<_, _>>())
    }
}

impl fmt::Debug for Version {
    /// `{ab12cd34: 3, ...}` with short node IDs, readable in test failures.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (node, counter) in &self.0 {
            map.entry(&node.short(), counter);
        }
        map.finish()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// A small pool of node IDs so generated versions share keys often.
    fn node(i: u8) -> NodeId {
        let mut bytes = [0u8; 16];
        bytes[15] = i;
        NodeId::from_bytes(bytes)
    }

    fn version(pairs: &[(u8, u64)]) -> Version {
        pairs.iter().map(|(i, c)| (node(*i), *c)).collect()
    }

    fn any_version() -> impl Strategy<Value = Version> {
        prop::collection::btree_map(0u8..4, 1u64..6, 0..=4)
            .prop_map(|m| m.into_iter().map(|(i, c)| (node(i), c)).collect())
    }

    fn any_node() -> impl Strategy<Value = NodeId> {
        (0u8..5).prop_map(node)
    }

    // ---- examples from the text -------------------------------------------

    #[test]
    fn empty_is_dominated_by_anything_non_empty() {
        let e = Version::empty();
        let a = version(&[(0, 1)]);
        assert_eq!(e.compare(&a), Relation::Dominated);
        assert_eq!(a.compare(&e), Relation::Dominates);
        assert_eq!(e.compare(&e), Relation::Equal);
        assert!(e.is_empty());
    }

    #[test]
    fn compare_treats_missing_as_zero() {
        let a = version(&[(0, 3)]);
        let b = version(&[(0, 3), (1, 5)]);
        assert_eq!(a.compare(&b), Relation::Dominated);
        let c = version(&[(0, 4)]);
        assert_eq!(c.compare(&b), Relation::Concurrent);
    }

    #[test]
    fn the_deny_counterexample_from_the_plan() {
        // A plain bump of {A:3} to {A:4} does not dominate {A:3, B:5};
        // increment(merge(local, quarantined)) does. This is why §8.2 says
        // merge first.
        let local = version(&[(0, 3)]);
        let quarantined = version(&[(0, 3), (1, 5)]);
        let plain = local.incremented(node(0));
        assert_eq!(plain.compare(&quarantined), Relation::Concurrent);
        let deny = local.merge(&quarantined).incremented(node(0));
        assert_eq!(deny.compare(&quarantined), Relation::Dominates);
        assert_eq!(deny.compare(&local), Relation::Dominates);
    }

    #[test]
    fn first_local_change_yields_one() {
        let v = Version::empty().incremented(node(2));
        assert_eq!(v.counter(node(2)), 1);
        assert_eq!(v.counter(node(0)), 0);
        assert_eq!(v.iter().count(), 1);
    }

    #[test]
    fn increment_saturates_instead_of_panicking() {
        let v: Version = [(node(0), u64::MAX)].into_iter().collect();
        assert_eq!(v.incremented(node(0)).counter(node(0)), u64::MAX);
    }

    #[test]
    fn zero_counters_are_normalised_away() {
        let with_zero = version(&[(0, 0), (1, 2)]);
        let without = version(&[(1, 2)]);
        assert_eq!(with_zero, without);
        assert_eq!(with_zero.compare(&without), Relation::Equal);
        let map: BTreeMap<NodeId, u64> = with_zero.into();
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn debug_uses_short_ids() {
        let v = version(&[(1, 2)]);
        assert_eq!(format!("{v:?}"), "{00000000: 2}");
    }

    // ---- laws ----------------------------------------------------------------

    proptest! {
        #[test]
        fn compare_is_reflexive(a in any_version()) {
            prop_assert_eq!(a.compare(&a), Relation::Equal);
        }

        #[test]
        fn equal_relation_matches_equality(a in any_version(), b in any_version()) {
            prop_assert_eq!(a.compare(&b) == Relation::Equal, a == b);
        }

        /// compare(a, b) and compare(b, a) are mirror images. In particular
        /// a dominates b implies not (b dominates a).
        #[test]
        fn compare_is_antisymmetric(a in any_version(), b in any_version()) {
            prop_assert_eq!(a.compare(&b).flip(), b.compare(&a));
            if a.dominates(&b) {
                prop_assert!(!b.dominates(&a));
            }
        }

        #[test]
        fn dominance_is_transitive(a in any_version(), b in any_version(), c in any_version()) {
            if a.dominates_or_equals(&b) && b.dominates_or_equals(&c) {
                prop_assert!(a.dominates_or_equals(&c));
            }
        }

        #[test]
        fn merge_is_commutative(a in any_version(), b in any_version()) {
            prop_assert_eq!(a.merge(&b), b.merge(&a));
        }

        #[test]
        fn merge_is_associative(a in any_version(), b in any_version(), c in any_version()) {
            prop_assert_eq!(a.merge(&b).merge(&c), a.merge(&b.merge(&c)));
        }

        #[test]
        fn merge_is_idempotent(a in any_version(), b in any_version()) {
            prop_assert_eq!(a.merge(&a), a.clone());
            let m = a.merge(&b);
            prop_assert_eq!(m.merge(&b), m.clone());
            prop_assert_eq!(m.merge(&a), m);
        }

        #[test]
        fn merge_dominates_or_equals_both(a in any_version(), b in any_version()) {
            let m = a.merge(&b);
            prop_assert!(m.dominates_or_equals(&a));
            prop_assert!(m.dominates_or_equals(&b));
        }

        /// merge is the least upper bound: anything above both is above the merge.
        #[test]
        fn merge_is_least_upper_bound(a in any_version(), b in any_version(), c in any_version()) {
            if c.dominates_or_equals(&a) && c.dominates_or_equals(&b) {
                prop_assert!(c.dominates_or_equals(&a.merge(&b)));
            }
        }

        #[test]
        fn increment_dominates_original(v in any_version(), n in any_node()) {
            prop_assert!(v.incremented(n).dominates(&v));
        }

        /// increment changes exactly one counter, by exactly one.
        #[test]
        fn increment_touches_only_its_node(v in any_version(), n in any_node(), other in any_node()) {
            let inc = v.incremented(n);
            prop_assert_eq!(inc.counter(n), v.counter(n) + 1);
            if other != n {
                prop_assert_eq!(inc.counter(other), v.counter(other));
            }
        }

        /// Two machines incrementing the same base independently are concurrent.
        #[test]
        fn independent_increments_are_concurrent(v in any_version(), n in any_node(), m in any_node()) {
            if n != m {
                prop_assert_eq!(v.incremented(n).compare(&v.incremented(m)), Relation::Concurrent);
            }
        }

        #[test]
        fn map_round_trip_preserves_version(a in any_version()) {
            let map: BTreeMap<NodeId, u64> = a.clone().into();
            prop_assert_eq!(Version::from(map), a);
        }
    }
}
