//! Quarantine (DESIGN.md §8.2): held batches and the versions they hold.
//!
//! When the brake holds a batch, every incoming entry that produced an
//! apply-set item is stored **as received**, keyed by `(path, version)`,
//! under a held item named by the batch id. Quarantine is by version, not by
//! sender: the same version from any peer is held, and a version that
//! dominates a quarantined one joins the same held item. Unrelated paths
//! are unaffected. `approve` releases an item and re-classifies its entries
//! against the index as it stands; `deny` releases it after bumping this
//! machine's versions over every quarantined one (§8.2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::brake::HoldReason;
use crate::entry::Entry;
use crate::id::{BatchId, NodeId};
use crate::path::RelPath;
use crate::time::Timestamp;
use crate::version::Version;

/// One held batch, as `delocal review` shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldItem {
    /// The batch that was held. Later batches that join keep this id.
    pub batch: BatchId,
    pub source: NodeId,
    pub seq_high: u64,
    pub held_at: Timestamp,
    pub reason: HoldReason,
    /// The latest quarantined entry per path, as received.
    pub entries: BTreeMap<RelPath, Entry>,
}

/// The folder's quarantine: held items and an index of the versions in them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantine {
    items: BTreeMap<BatchId, HeldItem>,
    /// Every quarantined version per path with the item it belongs to, in
    /// arrival order.
    versions: BTreeMap<RelPath, Vec<(Version, BatchId)>>,
}

impl Quarantine {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Held items in batch-id order.
    pub fn items(&self) -> impl Iterator<Item = &HeldItem> {
        self.items.values()
    }

    pub fn get(&self, batch: BatchId) -> Option<&HeldItem> {
        self.items.get(&batch)
    }

    /// The held item an incoming entry belongs to, if its version equals or
    /// dominates a quarantined version at its path (§8.2). The earliest such
    /// item wins when several match.
    pub fn matching(&self, entry: &Entry) -> Option<BatchId> {
        self.versions
            .get(&entry.path)?
            .iter()
            .find(|(v, _)| entry.version.dominates_or_equals(v))
            .map(|(_, id)| *id)
    }

    /// The largest stamp among the quarantined entries at `path`, `None` if
    /// none is held there. A `deny` bump (§8.2) dominates every one of them
    /// and must rank above them all (§7.6), so its stamp starts here.
    pub fn max_stamp_at(&self, path: &RelPath) -> Option<i64> {
        self.items
            .values()
            .filter_map(|item| item.entries.get(path))
            .map(|e| e.stamp)
            .max()
    }

    /// Every quarantined version at `path`, across all items.
    pub fn versions_at(&self, path: &RelPath) -> Vec<Version> {
        self.versions
            .get(path)
            .map(|v| v.iter().map(|(ver, _)| ver.clone()).collect())
            .unwrap_or_default()
    }

    /// Hold a new item with its entries.
    pub fn hold(&mut self, item: HeldItem) {
        for entry in item.entries.values() {
            self.record(&entry.path, entry.version.clone(), item.batch);
        }
        self.items.insert(item.batch, item);
    }

    /// An incoming entry joins an existing item: its version is quarantined
    /// and it replaces the stored entry for its path if it dominates or
    /// equals it. Returns false if the item does not exist.
    pub fn join(&mut self, batch: BatchId, entry: Entry) -> bool {
        if !self.items.contains_key(&batch) {
            return false;
        }
        self.record(&entry.path, entry.version.clone(), batch);
        if let Some(item) = self.items.get_mut(&batch) {
            let replace = item
                .entries
                .get(&entry.path)
                .is_none_or(|stored| entry.version.dominates_or_equals(&stored.version));
            if replace {
                item.entries.insert(entry.path.clone(), entry);
            }
        }
        true
    }

    /// Release an item: remove it and every version it held.
    pub fn release(&mut self, batch: BatchId) -> Option<HeldItem> {
        let item = self.items.remove(&batch)?;
        for path in item.entries.keys() {
            if let Some(list) = self.versions.get_mut(path) {
                list.retain(|(_, id)| *id != batch);
                if list.is_empty() {
                    self.versions.remove(path);
                }
            }
        }
        Some(item)
    }

    fn record(&mut self, path: &RelPath, version: Version, batch: BatchId) {
        let list = self.versions.entry(path.clone()).or_default();
        if !list.iter().any(|(v, id)| *v == version && *id == batch) {
            list.push((version, batch));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{ContentHash, Kind};
    use crate::id::HostName;

    fn node(i: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = i;
        NodeId::from_bytes(b)
    }

    fn batch(i: u8) -> BatchId {
        let mut b = [0u8; 16];
        b[15] = i;
        BatchId::from_bytes(b)
    }

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn entry(path: &str, version: Version) -> Entry {
        Entry {
            path: p(path),
            kind: Kind::File,
            size: 1,
            mtime_ns: 1,
            stamp: 1,
            exec: false,
            hash: ContentHash::EMPTY,
            prev_hash: ContentHash::EMPTY,
            version,
            deleted: false,
            modified_by: node(2),
            author_host: HostName::new("h").unwrap(),
        }
    }

    fn v(pairs: &[(u8, u64)]) -> Version {
        pairs.iter().map(|(n, c)| (node(*n), *c)).collect()
    }

    fn item(id: u8, entries: Vec<Entry>) -> HeldItem {
        HeldItem {
            batch: batch(id),
            source: node(2),
            seq_high: 9,
            held_at: Timestamp::from_unix_nanos(1),
            reason: HoldReason::Size { bytes: 1 },
            entries: entries.into_iter().map(|e| (e.path.clone(), e)).collect(),
        }
    }

    #[test]
    fn same_version_from_anyone_and_dominating_versions_match() {
        let mut q = Quarantine::default();
        q.hold(item(
            1,
            vec![entry("a", v(&[(2, 3)])), entry("b", v(&[(2, 1)]))],
        ));
        assert_eq!(
            q.matching(&entry("a", v(&[(2, 3)]))),
            Some(batch(1)),
            "equal"
        );
        assert_eq!(
            q.matching(&entry("a", v(&[(2, 4)]))),
            Some(batch(1)),
            "dominates"
        );
        assert_eq!(
            q.matching(&entry("a", v(&[(2, 3), (3, 1)]))),
            Some(batch(1))
        );
        assert_eq!(
            q.matching(&entry("a", v(&[(2, 2)]))),
            None,
            "dominated: an older version"
        );
        assert_eq!(q.matching(&entry("a", v(&[(3, 9)]))), None, "concurrent");
        assert_eq!(
            q.matching(&entry("c", v(&[(2, 3)]))),
            None,
            "unrelated path"
        );
    }

    #[test]
    fn join_keeps_the_dominating_entry_and_release_clears_versions() {
        let mut q = Quarantine::default();
        q.hold(item(1, vec![entry("a", v(&[(2, 3)]))]));
        assert!(q.join(batch(1), entry("a", v(&[(2, 4)]))));
        assert_eq!(
            q.get(batch(1)).unwrap().entries[&p("a")].version,
            v(&[(2, 4)])
        );
        assert!(
            q.join(batch(1), entry("a", v(&[(2, 3)]))),
            "older join is recorded, not stored"
        );
        assert_eq!(
            q.get(batch(1)).unwrap().entries[&p("a")].version,
            v(&[(2, 4)])
        );
        assert_eq!(q.versions_at(&p("a")).len(), 2);
        assert!(!q.join(batch(7), entry("a", v(&[(2, 5)]))));
        let released = q.release(batch(1)).unwrap();
        assert_eq!(released.entries.len(), 1);
        assert!(q.is_empty());
        assert!(q.versions_at(&p("a")).is_empty());
        assert_eq!(q.matching(&entry("a", v(&[(2, 4)]))), None);
    }

    #[test]
    fn two_items_can_hold_the_same_path() {
        let mut q = Quarantine::default();
        q.hold(item(1, vec![entry("a", v(&[(2, 3)]))]));
        q.hold(item(2, vec![entry("a", v(&[(3, 1)]))]));
        assert_eq!(q.len(), 2);
        assert_eq!(q.versions_at(&p("a")).len(), 2);
        q.release(batch(1));
        assert_eq!(q.versions_at(&p("a")), vec![v(&[(3, 1)])]);
        assert_eq!(q.matching(&entry("a", v(&[(3, 2)]))), Some(batch(2)));
    }

    #[test]
    fn round_trips() {
        let mut q = Quarantine::default();
        q.hold(item(1, vec![entry("a", v(&[(2, 3)]))]));
        let bytes = postcard::to_stdvec(&q).unwrap();
        assert_eq!(postcard::from_bytes::<Quarantine>(&bytes).unwrap(), q);
    }
}
