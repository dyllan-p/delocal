//! Persistence by part (DESIGN.md §11).
//!
//! The engine's per-folder state is persisted part by part, never as a
//! whole-state blob, and the engine reports every part as it changes: the
//! index, the wants, the pending set, the held items, the deferred paths
//! and the small rest. The keyed parts remember which rows changed since
//! they were last reported, in a [`Changed`], and the engine drains it
//! once per event into persistence actions.

use std::collections::BTreeSet;

/// Keys of a part whose rows changed since the engine last reported them
/// (§11). Bookkeeping for the persistence hooks, not folder state: it is
/// not serialised, and two states that differ only here are equal, so a
/// state restored from its parts compares equal to the one that wrote
/// them.
#[derive(Clone, Debug)]
pub struct Changed<K>(BTreeSet<K>);

impl<K> Default for Changed<K> {
    fn default() -> Self {
        Self(BTreeSet::new())
    }
}

impl<K> PartialEq for Changed<K> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl<K> Eq for Changed<K> {}

impl<K: Ord + Clone> Changed<K> {
    /// The row at `key` changed, or went away.
    pub fn note(&mut self, key: &K) {
        if !self.0.contains(key) {
            self.0.insert(key.clone());
        }
    }

    /// Every key noted since the last call, in key order.
    pub fn take(&mut self) -> BTreeSet<K> {
        std::mem::take(&mut self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_keys_come_out_once_in_order_and_never_affect_equality() {
        let mut c = Changed::default();
        c.note(&3);
        c.note(&1);
        c.note(&3);
        assert_eq!(c, Changed::default());
        assert_eq!(c.take().into_iter().collect::<Vec<_>>(), [1, 3]);
        assert!(c.take().is_empty());
    }
}
