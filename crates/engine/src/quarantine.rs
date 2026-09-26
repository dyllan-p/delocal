//! Quarantine (DESIGN.md §8.2): held batches and the versions they hold.
//!
//! When the brake holds a batch, every incoming entry that produced an
//! apply-set item is stored **as received**, keyed by `(path, version)`,
//! under a held item named by the batch id. Quarantine is by version, not by
//! sender: the same version from any peer is held, and a version that
//! dominates a quarantined one joins the same held item. Unrelated paths
//! are unaffected. `approve` releases an item and re-classifies its entries
//! against the index as it stands; `deny` withdraws it after bumping this
//! machine's versions over every quarantined one (§8.2), and a `revert`
//! that discards those bumps reinstates it (§8.3).
//!
//! **Persistence** (§11). The held items are a persisted part, one row per
//! item: a [`HeldRow`] with the item and every version it holds. Versions
//! at a path are kept in the order they arrived, across items, because the
//! earliest matching item is the one a later version joins; each carries
//! an **arrival number** from a per-folder counter, so a quarantine rebuilt
//! from its rows keeps that order whatever items the versions belong to.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::brake::HoldReason;
use crate::entry::Entry;
use crate::id::{BatchId, NodeId};
use crate::parts::Changed;
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

/// One quarantined version at a path (§8.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantined {
    pub version: Version,
    /// Its stamp, for the stamp of a `deny` bump over it (§7.1).
    pub stamp: i64,
    /// Its place in the folder's arrival order (§11).
    pub arrival: u64,
}

/// A held item with every version it holds (§11): one row of the held
/// items. Also what `deny` takes out, so that a `revert` discarding the
/// deny's bumps can put the item back as it was (§8.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldRow {
    pub item: HeldItem,
    /// The item's quarantined versions per path, in arrival order, joiners
    /// the stored entries do not show included.
    pub versions: BTreeMap<RelPath, Vec<Quarantined>>,
}

/// The folder's quarantine: held items and an index of the versions in them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantine {
    items: BTreeMap<BatchId, HeldItem>,
    /// Every quarantined version per path with the item it belongs to, in
    /// arrival order. Kept apart from the items' entries because `join`
    /// records a version it does not store: the stored entry is the latest
    /// per path, and a concurrent joiner is quarantined without replacing
    /// it.
    versions: BTreeMap<RelPath, Vec<(BatchId, Quarantined)>>,
    /// The last arrival number handed out.
    arrivals: u64,
    /// Held items whose row changed since the last drain.
    #[serde(skip)]
    changed: Changed<BatchId>,
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
            .find(|(_, q)| entry.version.dominates_or_equals(&q.version))
            .map(|(id, _)| *id)
    }

    /// The largest stamp among the quarantined versions at `path`, `None`
    /// if none is held there. A `deny` bump (§8.2) folds every one of them
    /// into its vector, so it dominates them all and must rank above them
    /// all (§7.1, §7.6): its stamp starts here. Read from the same list as
    /// `versions_at`, not from the stored entries, which lack the joiners
    /// that were concurrent with what an item already held.
    pub fn max_stamp_at(&self, path: &RelPath) -> Option<i64> {
        self.versions.get(path)?.iter().map(|(_, q)| q.stamp).max()
    }

    /// Every quarantined version at `path`, across all items.
    pub fn versions_at(&self, path: &RelPath) -> Vec<Version> {
        self.versions
            .get(path)
            .map(|v| v.iter().map(|(_, q)| q.version.clone()).collect())
            .unwrap_or_default()
    }

    /// Hold an item with its entries (§8.2). A batch id names one review
    /// item: if an item with this id is already held, the entries join it
    /// instead of replacing it. Entries re-admitted from the deferred set
    /// keep the id of the batch they arrived in, and `revert` returns a
    /// withdrawn item under its own id, so the same id can come back while
    /// an item holds it. Replacing that item would drop its entries while
    /// the version index still names them under the id; `release` would
    /// then leave those versions behind, and a later version dominating one
    /// of them would join an item that no longer exists and be lost.
    pub fn hold(&mut self, item: HeldItem) {
        if self.items.contains_key(&item.batch) {
            let batch = item.batch;
            for entry in item.entries.into_values() {
                self.join(batch, entry);
            }
            return;
        }
        for entry in item.entries.values() {
            self.record(&entry.path, entry.version.clone(), entry.stamp, item.batch);
        }
        self.changed.note(&item.batch);
        self.items.insert(item.batch, item);
    }

    /// An incoming entry joins an existing item: its version is quarantined
    /// and it replaces the stored entry for its path if it dominates or
    /// equals it. Returns false if the item does not exist.
    pub fn join(&mut self, batch: BatchId, entry: Entry) -> bool {
        if !self.items.contains_key(&batch) {
            return false;
        }
        self.record(&entry.path, entry.version.clone(), entry.stamp, batch);
        self.changed.note(&batch);
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
        self.changed.note(&batch);
        for path in item.entries.keys() {
            if let Some(list) = self.versions.get_mut(path) {
                list.retain(|(id, _)| *id != batch);
                if list.is_empty() {
                    self.versions.remove(path);
                }
            }
        }
        Some(item)
    }

    /// The item held under `batch` with every version it holds: its row
    /// (§11).
    pub fn row(&self, batch: BatchId) -> Option<HeldRow> {
        let item = self.items.get(&batch)?.clone();
        let versions = item
            .entries
            .keys()
            .map(|path| {
                let held: Vec<Quarantined> = self
                    .versions
                    .get(path)
                    .into_iter()
                    .flatten()
                    .filter(|(id, _)| *id == batch)
                    .map(|(_, q)| q.clone())
                    .collect();
                (path.clone(), held)
            })
            .collect();
        Some(HeldRow { item, versions })
    }

    /// Take an item out for `deny`: like `release`, but keep every version
    /// it held so that [`Quarantine::reinstate`] can restore it exactly.
    pub fn withdraw(&mut self, batch: BatchId) -> Option<HeldRow> {
        let row = self.row(batch)?;
        self.release(batch)?;
        Some(row)
    }

    /// Put a withdrawn item back with every version it held (§8.3). Its
    /// versions arrive again, at the end of each path's arrival order.
    pub fn reinstate(&mut self, withdrawn: HeldRow) {
        let batch = withdrawn.item.batch;
        for (path, held) in withdrawn.versions {
            for q in held {
                self.record(&path, q.version, q.stamp, batch);
            }
        }
        self.hold(withdrawn.item);
    }

    /// Held items whose row changed since the last call, each as it stands
    /// (`None` if it is no longer held), in batch-id order: the
    /// `HeldChanged` persistence hook (§11).
    pub fn drain_changes(&mut self) -> Vec<(BatchId, Option<HeldRow>)> {
        self.changed
            .take()
            .into_iter()
            .map(|batch| (batch, self.row(batch)))
            .collect()
    }

    /// Quarantine `version` at `path` for the item `batch`, with the next
    /// arrival number, unless the item already holds it there.
    fn record(&mut self, path: &RelPath, version: Version, stamp: i64, batch: BatchId) {
        let list = self.versions.entry(path.clone()).or_default();
        if !list
            .iter()
            .any(|(id, q)| q.version == version && *id == batch)
        {
            self.arrivals += 1;
            list.push((
                batch,
                Quarantined {
                    version,
                    stamp,
                    arrival: self.arrivals,
                },
            ));
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

    /// §8.3: an item `deny` withdrew and `revert` reinstates comes back as
    /// it was, a concurrent joiner the stored entry does not show included,
    /// and an item held meanwhile is untouched.
    #[test]
    fn a_withdrawn_item_is_reinstated_with_every_version_it_held() {
        let mut q = Quarantine::default();
        q.hold(item(
            1,
            vec![entry("a", v(&[(2, 3)])), entry("b", v(&[(2, 1)]))],
        ));
        assert!(
            q.join(batch(1), entry("a", v(&[(2, 2), (3, 1)]))),
            "a concurrent joiner"
        );
        q.hold(item(2, vec![entry("c", v(&[(4, 1)]))]));
        let before = q.clone();
        let withdrawn = q.withdraw(batch(1)).unwrap();
        assert_eq!(q.get(batch(1)), None);
        assert!(q.versions_at(&p("a")).is_empty());
        assert_eq!(q.withdraw(batch(9)), None);
        q.reinstate(withdrawn);
        for path in ["a", "b", "c"] {
            assert_eq!(q.versions_at(&p(path)), before.versions_at(&p(path)));
        }
        assert_eq!(
            q.items().collect::<Vec<_>>(),
            before.items().collect::<Vec<_>>()
        );
        assert_eq!(q.versions_at(&p("a")).len(), 2, "the joiner too");
        // The versions arrived again, after everything held before.
        let arrivals: Vec<u64> = q.row(batch(1)).unwrap().versions[&p("a")]
            .iter()
            .map(|q| q.arrival)
            .collect();
        assert_eq!(arrivals, [5, 6]);
    }

    /// §8.2: a batch id names one review item. Holding under an id that
    /// already has an item joins it: both sets of entries stay held, and
    /// releasing the item releases every version, leaving nothing in the
    /// version index for a later version to match.
    #[test]
    fn holding_under_an_existing_id_joins_the_item() {
        let mut q = Quarantine::default();
        q.hold(item(1, vec![entry("a", v(&[(2, 1)]))]));
        q.hold(item(1, vec![entry("b", v(&[(2, 2)]))]));
        assert_eq!(q.len(), 1);
        let held = q.get(batch(1)).unwrap();
        assert_eq!(held.entries.keys().collect::<Vec<_>>(), [&p("a"), &p("b")]);
        assert_eq!(q.versions_at(&p("a")), vec![v(&[(2, 1)])]);

        let released = q.release(batch(1)).unwrap();
        assert_eq!(released.entries.len(), 2);
        assert!(q.versions_at(&p("a")).is_empty());
        assert!(q.versions_at(&p("b")).is_empty());
        assert_eq!(q.matching(&entry("a", v(&[(2, 5)]))), None);
    }

    /// §8.2, §8.3, seed 90745: `deny` withdrew an item, entries re-admitted
    /// from the deferred set were held again under the same batch id, and
    /// `revert` then returned the withdrawn item. It joins the item held
    /// meanwhile, and releasing it releases both.
    #[test]
    fn an_item_returned_by_revert_joins_the_item_holding_its_id() {
        let mut q = Quarantine::default();
        q.hold(item(1, vec![entry("d1/f10", v(&[(2, 4), (3, 1)]))]));
        let withdrawn = q.withdraw(batch(1)).unwrap();
        q.hold(item(1, vec![entry("f0", v(&[(4, 1)]))]));
        q.reinstate(withdrawn);
        let held = q.get(batch(1)).unwrap();
        assert_eq!(
            held.entries.keys().collect::<Vec<_>>(),
            [&p("d1/f10"), &p("f0")]
        );
        assert_eq!(
            q.matching(&entry("d1/f10", v(&[(2, 8), (3, 9)]))),
            Some(batch(1)),
            "a later version at d1/f10 joins an item that exists"
        );

        q.release(batch(1)).unwrap();
        assert!(q.versions_at(&p("d1/f10")).is_empty());
        assert!(q.versions_at(&p("f0")).is_empty());
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
    fn a_concurrent_joiner_is_recorded_with_its_stamp() {
        let mut q = Quarantine::default();
        q.hold(item(1, vec![entry("a", v(&[(2, 3)]))]));
        let mut joiner = entry("a", v(&[(3, 1)]));
        joiner.stamp = 50;
        assert!(q.join(batch(1), joiner));
        assert_eq!(
            q.get(batch(1)).unwrap().entries[&p("a")].version,
            v(&[(2, 3)]),
            "concurrent: the stored entry stays"
        );
        assert_eq!(q.versions_at(&p("a")).len(), 2);
        assert_eq!(
            q.max_stamp_at(&p("a")),
            Some(50),
            "the bump over both versions must start above the joiner too"
        );
        q.release(batch(1));
        assert_eq!(q.max_stamp_at(&p("a")), None);
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

    /// §11: every change to a held item reports its row once, with every
    /// version it holds in arrival order, and a released item reports that
    /// it is gone.
    #[test]
    fn held_items_report_their_rows_as_they_change() {
        let mut q = Quarantine::default();
        q.hold(item(1, vec![entry("a", v(&[(2, 3)]))]));
        q.hold(item(2, vec![entry("a", v(&[(3, 1)]))]));
        q.join(batch(1), entry("a", v(&[(2, 2), (4, 1)])));
        let changes = q.drain_changes();
        assert_eq!(
            changes.iter().map(|(b, _)| *b).collect::<Vec<_>>(),
            [batch(1), batch(2)]
        );
        let row = changes[0].1.as_ref().unwrap();
        let held: Vec<(Version, u64)> = row.versions[&p("a")]
            .iter()
            .map(|q| (q.version.clone(), q.arrival))
            .collect();
        assert_eq!(
            held,
            [(v(&[(2, 3)]), 1), (v(&[(2, 2), (4, 1)]), 3)],
            "the joiner too, after the other item's version"
        );
        assert!(q.drain_changes().is_empty());
        q.release(batch(2));
        assert_eq!(q.drain_changes(), [(batch(2), None)]);
    }

    #[test]
    fn round_trips() {
        let mut q = Quarantine::default();
        q.hold(item(1, vec![entry("a", v(&[(2, 3)]))]));
        let bytes = postcard::to_stdvec(&q).unwrap();
        assert_eq!(postcard::from_bytes::<Quarantine>(&bytes).unwrap(), q);
    }
}
