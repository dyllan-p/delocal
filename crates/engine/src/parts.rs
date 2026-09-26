//! Persistence by part (DESIGN.md §11).
//!
//! The engine's per-folder state is persisted part by part, never as a
//! whole-state blob, and the engine reports every part as it changes: the
//! index, the wants, the pending set, the held items, the deferred paths
//! and the small rest. The keyed parts remember which rows changed since
//! they were last reported, in a [`Changed`], and the engine drains it
//! once per event into persistence actions. The small rest is one row,
//! [`Rest`]; the engine reports it whenever it differs from the one it
//! reported last.
//!
//! [`FolderParts`] is the other direction: every row, as the daemon loads
//! them at start-up, plus the folder's metadata, which the host keeps. The
//! folder state is rebuilt from it and nothing else
//! ([`crate::FolderState::from_parts`], [`crate::Engine::restore`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::folder::{Deferred, Paused, Queued};
use crate::id::{BatchId, FolderId, NodeId};
use crate::index::{IndexRecord, Pending, Watermark};
use crate::path::RelPath;
use crate::quarantine::{HeldRow, HeldState};
use crate::rules::Rules;
use crate::want::Want;

/// The small rest (§11): one row per folder, for the engine state that is
/// kept neither per path nor per held item.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rest {
    /// This machine's last `seq` for the folder (§7.1).
    pub seq: u64,
    /// `seq` of the newest record announced in a batch (§7.4).
    pub announced_seq: u64,
    /// The tracked count at that announcement: the sender pre-check's H1
    /// denominator (§8.1).
    pub announced_tracked: usize,
    /// What this machine holds of each peer's `seq` space (§7.4).
    pub watermarks: BTreeMap<NodeId, Watermark>,
    /// This machine's `seq` as each peer last acknowledged it (§7.4).
    pub acked: BTreeMap<NodeId, u64>,
    /// Set while the sender pre-check holds the folder (§8.1).
    pub paused: Option<Paused>,
    /// `deny` and `revert` waiting for the folder to settle, in order (§8.3).
    pub queued: Vec<Queued>,
    /// The last arrival number the quarantine handed out (§8.2, §11).
    pub arrivals: u64,
    /// Conflicts decided by the last step of the winner rule (§7.6); any
    /// count above zero is a bug to find.
    pub winner_fallbacks: u64,
}

/// A folder as persisted (§11): its metadata, from the host's `folders`
/// and `members` tables, and every row of every engine part, keyed as the
/// hooks report them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderParts {
    pub id: FolderId,
    pub rules: Rules,
    pub members: BTreeSet<NodeId>,
    /// The index, one record per path (`IndexChanged`, `IndexRemoved`).
    pub records: BTreeMap<RelPath, IndexRecord>,
    /// The wants, one per path (`WantChanged`).
    pub wants: BTreeMap<RelPath, Want>,
    /// The pending set, one row per unannounced path (`PendingChanged`).
    pub pending: BTreeMap<RelPath, Pending>,
    /// The held items, held or denied, one row per item (`HeldChanged`).
    pub held: BTreeMap<(BatchId, HeldState), HeldRow>,
    /// The deferred paths, one row per path (`DeferredChanged`).
    pub deferred: BTreeMap<RelPath, Vec<Deferred>>,
    /// The small rest (`RestChanged`).
    pub rest: Rest,
}

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
