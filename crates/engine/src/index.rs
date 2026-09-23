//! The per-folder index (DESIGN.md §7.1), the local-change rule (§7.2) and
//! tombstones (§7.7), as an in-memory structure.
//!
//! One [`Index`] per folder per machine. It holds one [`IndexRecord`] per
//! path the machine knows about, deleted ones included, plus this machine's
//! `seq` counter and the highest `seq` seen from each peer. Persistence is
//! the host's job (Phase 2, SQLite); the engine reports every mutation.
//!
//! Two ways a record changes:
//!
//! - **Local change** ([`Index::observe`], [`Index::observe_absent`]): the
//!   scanner saw something different from the record. The new version is
//!   `incremented(self)` on the entry's current vector (§7.2), `modified_by`
//!   and `author_host` are this machine's, and `seq` advances.
//! - **Adopt** ([`Index::adopt`]): a remote version has been committed to
//!   disk, or a tombstone applied, and the record takes it over as is,
//!   with a new `seq`. Never called on accept, only after the host reports
//!   the commit (§7.1, "when `seq` advances").
//!
//! What counts as a local change: any difference between the observation
//! and the record in kind, size, mtime, exec or hash. An mtime-only change
//! (`touch`) is a change with unchanged content; receivers apply it as
//! metadata only (§7.5), so it is cheap, and it keeps every machine's mtime
//! identical, which the conflict tie-break relies on (§7.6).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::entry::{Entry, Hash, Observed};
use crate::id::{HostName, NodeId};
use crate::path::RelPath;
use crate::version::Version;

/// An [`Entry`] plus this machine's `seq` for it (§7.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRecord {
    pub entry: Entry,
    /// This machine's per-folder sequence number when the record was last
    /// written locally. For catch-up only; never compared as a version.
    pub seq: u64,
}

/// How a local change relates to what the index held before (§7.4 summary
/// counts, §8.1 brake).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    /// Nothing was there, or a tombstone was. Adds do not count toward H1.
    Add,
    /// A live entry changed.
    Modify,
    /// A live entry vanished. The record is now a tombstone.
    Delete,
}

/// A local change the index has recorded and the engine will batch (§7.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalChange {
    pub kind: ChangeKind,
    pub record: IndexRecord,
}

/// The per-folder index for one machine. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    own: NodeId,
    host: HostName,
    records: BTreeMap<RelPath, IndexRecord>,
    /// Last `seq` handed out. 0 means nothing has been written yet.
    seq: u64,
    /// Highest `seq` received from each remote member (§7.1, §7.4 catch-up).
    peer_seq: BTreeMap<NodeId, u64>,
}

impl Index {
    /// An empty index for this machine.
    pub fn new(own: NodeId, host: HostName) -> Self {
        Self {
            own,
            host,
            records: BTreeMap::new(),
            seq: 0,
            peer_seq: BTreeMap::new(),
        }
    }

    /// This machine's node ID.
    pub fn own(&self) -> NodeId {
        self.own
    }

    /// The record at a path, tombstones included.
    pub fn get(&self, path: &RelPath) -> Option<&IndexRecord> {
        self.records.get(path)
    }

    /// The live (non-deleted) entry at a path.
    pub fn live(&self, path: &RelPath) -> Option<&IndexRecord> {
        self.records.get(path).filter(|r| !r.entry.deleted)
    }

    /// Every record in path order, tombstones included.
    pub fn records(&self) -> impl Iterator<Item = &IndexRecord> {
        self.records.values()
    }

    /// Every live record in path order.
    pub fn live_records(&self) -> impl Iterator<Item = &IndexRecord> {
        self.records.values().filter(|r| !r.entry.deleted)
    }

    /// Number of tracked (non-deleted) entries: the denominator of H1 (§8.1).
    pub fn tracked_count(&self) -> usize {
        self.live_records().count()
    }

    /// Total number of records, tombstones included.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True if the index has no records at all.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The last `seq` written. Records with `seq` above a peer's ack are
    /// what catch-up sends (§7.4).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Records written after `after`, in path order. Catch-up (§7.4) sends
    /// these to a peer that has acknowledged `after`.
    pub fn records_since(&self, after: u64) -> impl Iterator<Item = &IndexRecord> {
        self.records.values().filter(move |r| r.seq > after)
    }

    /// Highest `seq` received from a peer, 0 if none.
    pub fn peer_seq(&self, peer: NodeId) -> u64 {
        self.peer_seq.get(&peer).copied().unwrap_or(0)
    }

    /// Record that `peer`'s records up to `seq` have been received. Never
    /// moves backwards.
    pub fn set_peer_seq(&mut self, peer: NodeId, seq: u64) {
        let slot = self.peer_seq.entry(peer).or_insert(0);
        *slot = (*slot).max(seq);
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The scanner saw `observed` at `path` (§7.3). Returns the local change
    /// if it differs from the record, `None` if the fast path says unchanged.
    ///
    /// The new version is the §7.2 local-change rule applied to whatever
    /// version the path had, tombstone included, so a re-created file
    /// dominates its own deletion (§7.7).
    pub fn observe(&mut self, path: RelPath, observed: Observed) -> Option<LocalChange> {
        let observed = observed.normalised();
        let previous = self.records.get(&path);
        let kind = match previous {
            Some(r) if !r.entry.deleted => {
                if r.entry.observed() == observed {
                    return None;
                }
                ChangeKind::Modify
            }
            _ => ChangeKind::Add,
        };
        let version = previous
            .map(|r| r.entry.version.clone())
            .unwrap_or_default()
            .incremented(self.own);
        let entry = Entry {
            path: path.clone(),
            kind: observed.kind,
            size: observed.size,
            mtime_ns: observed.mtime_ns,
            exec: observed.exec,
            hash: observed.hash,
            version,
            deleted: false,
            modified_by: self.own,
            author_host: self.host.clone(),
        };
        Some(self.write(kind, entry))
    }

    /// The scanner found nothing at `path`. If a live entry was there, it
    /// becomes a tombstone (§7.7) dated `at_ns`; otherwise nothing happens.
    pub fn observe_absent(&mut self, path: &RelPath, at_ns: i64) -> Option<LocalChange> {
        let previous = self.live(path)?;
        let entry = Entry {
            path: path.clone(),
            kind: previous.entry.kind,
            size: 0,
            mtime_ns: at_ns,
            exec: false,
            hash: Hash::EMPTY,
            version: previous.entry.version.incremented(self.own),
            deleted: true,
            modified_by: self.own,
            author_host: self.host.clone(),
        };
        Some(self.write(ChangeKind::Delete, entry))
    }

    /// A remote entry has been committed to disk (or a remote tombstone
    /// applied) and the index takes it over unchanged, with a new `seq`.
    /// Called only after the host reports the commit, never on accept.
    pub fn adopt(&mut self, entry: Entry) -> &IndexRecord {
        let seq = self.next_seq();
        let path = entry.path.clone();
        self.records
            .insert(path.clone(), IndexRecord { entry, seq });
        &self.records[&path]
    }

    fn write(&mut self, kind: ChangeKind, entry: Entry) -> LocalChange {
        let seq = self.next_seq();
        let record = IndexRecord { entry, seq };
        self.records
            .insert(record.entry.path.clone(), record.clone());
        LocalChange { kind, record }
    }
}

/// Convenience for building a [`Version`] that only this machine has touched.
impl Index {
    /// The version a brand-new local entry gets: `{own: 1}`.
    pub fn first_version(&self) -> Version {
        Version::empty().incremented(self.own)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::entry::Kind;
    use crate::version::Relation;

    fn node(i: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = i;
        NodeId::from_bytes(b)
    }

    fn hash(i: u8) -> Hash {
        let mut b = [0u8; 32];
        b[0] = i;
        Hash::from_bytes(b)
    }

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn file(h: u8, mtime_ns: i64) -> Observed {
        Observed {
            kind: Kind::File,
            size: 10,
            mtime_ns,
            exec: false,
            hash: hash(h),
        }
    }

    fn index() -> Index {
        Index::new(node(1), HostName::new("laptop").unwrap())
    }

    #[test]
    fn first_observation_is_an_add_with_version_one() {
        let mut idx = index();
        let change = idx.observe(p("a.txt"), file(1, 100)).unwrap();
        assert_eq!(change.kind, ChangeKind::Add);
        assert_eq!(change.record.seq, 1);
        assert_eq!(change.record.entry.version, idx.first_version());
        assert_eq!(change.record.entry.modified_by, node(1));
        assert_eq!(change.record.entry.author_host.as_str(), "laptop");
        assert!(!change.record.entry.deleted);
        assert_eq!(idx.tracked_count(), 1);
        assert_eq!(idx.seq(), 1);
    }

    #[test]
    fn same_observation_is_not_a_change() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        assert_eq!(idx.observe(p("a.txt"), file(1, 100)), None);
        assert_eq!(idx.seq(), 1, "seq must not advance on a no-op");
    }

    #[test]
    fn content_change_is_a_modify_that_dominates() {
        let mut idx = index();
        let v1 = idx
            .observe(p("a.txt"), file(1, 100))
            .unwrap()
            .record
            .entry
            .version;
        let change = idx.observe(p("a.txt"), file(2, 200)).unwrap();
        assert_eq!(change.kind, ChangeKind::Modify);
        assert_eq!(change.record.seq, 2);
        assert_eq!(
            change.record.entry.version.compare(&v1),
            Relation::Dominates
        );
        assert_eq!(change.record.entry.version.counter(node(1)), 2);
    }

    #[test]
    fn mtime_only_change_is_still_a_modify() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        let change = idx.observe(p("a.txt"), file(1, 101)).unwrap();
        assert_eq!(change.kind, ChangeKind::Modify);
        assert_eq!(change.record.entry.hash, hash(1));
    }

    #[test]
    fn delete_makes_a_tombstone_that_dominates() {
        let mut idx = index();
        let live = idx.observe(p("a.txt"), file(1, 100)).unwrap().record.entry;
        let change = idx.observe_absent(&p("a.txt"), 500).unwrap();
        assert_eq!(change.kind, ChangeKind::Delete);
        let dead = &change.record.entry;
        assert!(dead.deleted);
        assert_eq!(dead.kind, Kind::File, "tombstone remembers the kind");
        assert_eq!(
            (dead.size, dead.hash, dead.exec, dead.mtime_ns),
            (0, Hash::EMPTY, false, 500)
        );
        assert!(dead.version.dominates(&live.version));
        assert_eq!(idx.tracked_count(), 0);
        assert_eq!(idx.len(), 1, "tombstones are kept (§7.7)");
        assert!(idx.live(&p("a.txt")).is_none());
        assert!(idx.get(&p("a.txt")).is_some());
    }

    #[test]
    fn absent_unknown_or_already_deleted_is_not_a_change() {
        let mut idx = index();
        assert_eq!(idx.observe_absent(&p("nope"), 1), None);
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        idx.observe_absent(&p("a.txt"), 200).unwrap();
        assert_eq!(idx.observe_absent(&p("a.txt"), 300), None);
        assert_eq!(idx.seq(), 2);
    }

    #[test]
    fn recreation_after_delete_is_an_add_that_dominates_the_tombstone() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        let tomb = idx.observe_absent(&p("a.txt"), 200).unwrap().record.entry;
        let change = idx.observe(p("a.txt"), file(3, 300)).unwrap();
        assert_eq!(change.kind, ChangeKind::Add, "adds do not count toward H1");
        assert!(change.record.entry.version.dominates(&tomb.version));
        assert_eq!(change.record.entry.version.counter(node(1)), 3);
        assert_eq!(idx.tracked_count(), 1);
    }

    #[test]
    fn adopt_takes_a_remote_entry_as_is_with_a_new_seq() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        let remote = Entry {
            path: p("b.txt"),
            kind: Kind::File,
            size: 3,
            mtime_ns: 7,
            exec: true,
            hash: hash(9),
            version: Version::empty().incremented(node(2)),
            deleted: false,
            modified_by: node(2),
            author_host: HostName::new("desktop").unwrap(),
        };
        let record = idx.adopt(remote.clone()).clone();
        assert_eq!(record.entry, remote);
        assert_eq!(record.seq, 2);
        assert_eq!(idx.get(&p("b.txt")), Some(&record));
        // A later local change to the adopted entry continues its vector.
        let change = idx.observe(p("b.txt"), file(4, 8)).unwrap();
        assert_eq!(change.kind, ChangeKind::Modify);
        assert_eq!(change.record.entry.version.counter(node(2)), 1);
        assert_eq!(change.record.entry.version.counter(node(1)), 1);
        assert!(change.record.entry.version.dominates(&remote.version));
    }

    #[test]
    fn dir_observations_are_normalised() {
        let mut idx = index();
        let dir = Observed {
            kind: Kind::Dir,
            size: 4096,
            mtime_ns: 1,
            exec: true,
            hash: hash(1),
        };
        let e = idx.observe(p("d"), dir.clone()).unwrap().record.entry;
        assert_eq!((e.size, e.hash, e.exec), (0, Hash::EMPTY, false));
        // Reporting the un-normalised form again is still "unchanged".
        assert_eq!(idx.observe(p("d"), dir), None);
    }

    #[test]
    fn records_since_and_peer_seq() {
        let mut idx = index();
        idx.observe(p("a"), file(1, 1)).unwrap();
        idx.observe(p("b"), file(2, 2)).unwrap();
        idx.observe(p("a"), file(3, 3)).unwrap();
        let after_one: Vec<_> = idx
            .records_since(1)
            .map(|r| r.entry.path.as_str())
            .collect();
        assert_eq!(
            after_one,
            ["a", "b"],
            "a was rewritten at seq 3, b at seq 2"
        );
        assert_eq!(idx.records_since(3).count(), 0);
        assert_eq!(idx.peer_seq(node(2)), 0);
        idx.set_peer_seq(node(2), 5);
        idx.set_peer_seq(node(2), 3);
        assert_eq!(idx.peer_seq(node(2)), 5, "never moves backwards");
    }

    #[test]
    fn index_round_trips_through_serde() {
        let mut idx = index();
        idx.observe(p("a"), file(1, 1)).unwrap();
        idx.observe_absent(&p("a"), 2).unwrap();
        idx.set_peer_seq(node(2), 4);
        let json = serde_json::to_string(&idx).unwrap();
        assert_eq!(serde_json::from_str::<Index>(&json).unwrap(), idx);
        let bytes = postcard::to_stdvec(&idx).unwrap();
        assert_eq!(postcard::from_bytes::<Index>(&bytes).unwrap(), idx);
    }

    // ---- properties -----------------------------------------------------------

    #[derive(Clone, Debug)]
    enum Step {
        See { path: u8, hash: u8, mtime: i64 },
        Gone { path: u8, at: i64 },
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            (0u8..4, 0u8..3, 0i64..4).prop_map(|(path, hash, mtime)| Step::See {
                path,
                hash,
                mtime
            }),
            (0u8..4, 0i64..4).prop_map(|(path, at)| Step::Gone { path, at }),
        ]
    }

    fn path_of(i: u8) -> RelPath {
        p(&format!("p{i}"))
    }

    proptest! {
        /// Every change dominates the record it replaced, is authored by this
        /// machine, and advances seq by exactly one; no-ops leave seq alone.
        #[test]
        fn local_changes_are_monotone(steps in prop::collection::vec(step(), 1..40)) {
            let mut idx = index();
            let mut changes = 0u64;
            for s in steps {
                let (path, before) = match &s {
                    Step::See { path, .. } | Step::Gone { path, .. } => {
                        let path = path_of(*path);
                        (path.clone(), idx.get(&path).cloned())
                    }
                };
                let seq_before = idx.seq();
                let change = match s {
                    Step::See { hash, mtime, .. } => idx.observe(path.clone(), file(hash, mtime)),
                    Step::Gone { at, .. } => idx.observe_absent(&path, at),
                };
                match change {
                    None => prop_assert_eq!(idx.seq(), seq_before),
                    Some(c) => {
                        changes += 1;
                        prop_assert_eq!(idx.seq(), seq_before + 1);
                        prop_assert_eq!(c.record.seq, idx.seq());
                        prop_assert_eq!(c.record.entry.modified_by, node(1));
                        prop_assert_eq!(&c.record.entry.path, &path);
                        if let Some(b) = before {
                            prop_assert!(c.record.entry.version.dominates(&b.entry.version));
                            prop_assert_eq!(
                                c.record.entry.version.counter(node(1)),
                                b.entry.version.counter(node(1)) + 1
                            );
                        } else {
                            prop_assert_eq!(c.kind, ChangeKind::Add);
                            prop_assert_eq!(c.record.entry.version, idx.first_version());
                        }
                        prop_assert_eq!(c.kind == ChangeKind::Delete, c.record.entry.deleted);
                    }
                }
            }
            prop_assert_eq!(idx.seq(), changes);
            prop_assert_eq!(idx.records_since(0).count(), idx.len());
            prop_assert!(idx.tracked_count() <= idx.len());
        }

        /// Replaying the same observations onto the resulting index changes nothing.
        #[test]
        fn observations_are_idempotent(steps in prop::collection::vec(step(), 1..20)) {
            let mut idx = index();
            let mut last: BTreeMap<RelPath, Option<Observed>> = BTreeMap::new();
            for s in steps {
                match s {
                    Step::See { path, hash, mtime } => {
                        let path = path_of(path);
                        idx.observe(path.clone(), file(hash, mtime));
                        last.insert(path, Some(file(hash, mtime)));
                    }
                    Step::Gone { path, at } => {
                        let path = path_of(path);
                        idx.observe_absent(&path, at);
                        last.insert(path, None);
                    }
                }
            }
            let seq = idx.seq();
            for (path, obs) in last {
                let change = match obs {
                    Some(o) => idx.observe(path, o),
                    None => idx.observe_absent(&path, 99),
                };
                prop_assert_eq!(change, None);
            }
            prop_assert_eq!(idx.seq(), seq);
        }
    }
}
