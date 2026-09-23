//! The per-folder index (DESIGN.md §7.1), the local-change rule (§7.2) and
//! tombstones (§7.7), as an in-memory structure.
//!
//! One [`Index`] per folder per machine. It holds one [`IndexRecord`] per
//! path the machine knows about, deleted ones included, plus this machine's
//! `seq` counter, the highest `seq` seen from each peer, and the set of
//! local changes not yet announced. Persistence is the host's job (Phase 2,
//! SQLite); the engine reports every mutation.
//!
//! Two ways a record changes:
//!
//! - **Local change** ([`Index::observe`], [`Index::observe_absent`]): the
//!   scanner saw something different from the record. The new version is
//!   `incremented(self)` on the entry's current vector (§7.2), `modified_by`
//!   and `author_host` are this machine's, `prev_hash` is the hash of the
//!   last version peers could have seen (below), and `seq` advances. The
//!   path joins the pending set until [`Index::mark_announced`].
//! - **Adopt** ([`Index::adopt`]): a remote version has been committed to
//!   disk, or a tombstone applied, and the record takes it over as is,
//!   with a new `seq`. Never called on accept, only after the host reports
//!   the commit (§7.1, "when `seq` advances"). This is the only way a
//!   received version enters the index: there is no slot for an
//!   accepted-but-unapplied version.
//!
//! **`prev_hash` and coalescing.** `prev_hash` is the hash of the version
//! this change replaced *as peers last saw it* (§7.1). For a path with no
//! pending change that is the current record's hash. For a path that
//! already has a pending (unannounced) change, it is the pending record's
//! own `prev_hash`, so several changes inside one batch window collapse to
//! one step from the last announced version. The change's kind is derived
//! the same way, from whether peers last saw a live entry and whether one
//! is there now: an add then a modify is still an add, a delete then a
//! re-creation is a modify, an add then a delete is a delete of something
//! peers never had (a tombstone with `hash == prev_hash == EMPTY`).
//!
//! What counts as a local change: any difference between the observation
//! and the record in kind, size, mtime, exec or hash. An mtime-only change
//! (`touch`) is a change with `hash == prev_hash` (§7.3): receivers apply it
//! as metadata only (§7.5), the brake ignores it (§8.1), and it loses to
//! any real edit in a conflict (§7.6). No tolerance is applied to the
//! size-and-mtime fast path; the mtime precision shim is host work (§7.3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::entry::{ContentHash, Entry, Kind, Observed};
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

/// How a pending local change relates to the last version peers saw (§7.4
/// summary counts, §8.1 brake).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    /// Nothing was there, or a tombstone was. Adds do not count toward H1.
    Add,
    /// A live entry changed.
    Modify,
    /// A live entry vanished. The record is now a tombstone.
    Delete,
}

impl ChangeKind {
    /// The step from what peers last saw to what is there now.
    fn between(announced_live: bool, now_live: bool) -> Self {
        match (announced_live, now_live) {
            (false, true) => Self::Add,
            (true, true) => Self::Modify,
            (_, false) => Self::Delete,
        }
    }
}

/// A local change the index has recorded and the engine will batch (§7.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalChange {
    /// The step from what peers last saw to the new record (module docs).
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
    /// Local changes not yet announced. The value is whether peers last saw
    /// a live entry at the path, which fixes the change's kind.
    pending: BTreeMap<RelPath, bool>,
    /// `seq` of the newest record in the last batch this machine formed
    /// (§7.4). Records above it, local or adopted, go in the next batch.
    announced_seq: u64,
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
            pending: BTreeMap::new(),
            announced_seq: 0,
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

    /// Number of tracked entries, the denominator of H1 (§8.1): live and
    /// not a directory. Directories are excluded from both sides of the
    /// ratio because a directory and its tombstone have the same content.
    pub fn tracked_count(&self) -> usize {
        self.live_records()
            .filter(|r| r.entry.kind != Kind::Dir)
            .count()
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

    /// Local changes not yet announced, in path order, with the coalesced
    /// kind of each. Batch formation (§7.4) reads this.
    pub fn pending(&self) -> impl Iterator<Item = (&IndexRecord, ChangeKind)> {
        self.pending.iter().filter_map(|(path, announced_live)| {
            self.records
                .get(path)
                .map(|r| (r, ChangeKind::between(*announced_live, !r.entry.deleted)))
        })
    }

    /// Number of paths with an unannounced local change.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The coalesced kind of the pending local change at `path`, if any.
    pub fn pending_kind(&self, path: &RelPath) -> Option<ChangeKind> {
        let announced_live = *self.pending.get(path)?;
        let record = self.records.get(path)?;
        Some(ChangeKind::between(announced_live, !record.entry.deleted))
    }

    /// `seq` of the newest record already announced in a batch (§7.4).
    pub fn announced_seq(&self) -> u64 {
        self.announced_seq
    }

    /// Every record written since the last batch, in `seq` order, with the
    /// coalesced kind for local changes and `None` for adopted records
    /// (§7.4 "adopted records are announced too"). What the next batch
    /// carries.
    pub fn unannounced(&self) -> Vec<(&IndexRecord, Option<ChangeKind>)> {
        let mut out: Vec<_> = self
            .records
            .iter()
            .filter(|(_, r)| r.seq > self.announced_seq)
            .map(|(path, r)| (r, self.pending_kind(path)))
            .collect();
        out.sort_by_key(|(r, _)| r.seq);
        out
    }

    /// Everything written so far has been announced (the batch was formed).
    /// Later changes start a new step from the announced version, and the
    /// next batch starts above the current `seq`.
    pub fn mark_announced(&mut self) {
        self.pending.clear();
        self.announced_seq = self.seq;
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The hash of the last version of `path` that peers could have seen:
    /// the pending change's `prev_hash` if there is one, else the current
    /// record's hash, else `EMPTY`.
    fn announced_hash(&self, path: &RelPath) -> ContentHash {
        match self.records.get(path) {
            None => ContentHash::EMPTY,
            Some(r) if self.pending.contains_key(path) => r.entry.prev_hash,
            Some(r) => r.entry.hash,
        }
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
        let previous_live = match previous {
            Some(r) if !r.entry.deleted => {
                if r.entry.observed() == observed {
                    return None;
                }
                true
            }
            _ => false,
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
            prev_hash: self.announced_hash(&path),
            version,
            deleted: false,
            modified_by: self.own,
            author_host: self.host.clone(),
        };
        Some(self.write(previous_live, entry))
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
            hash: ContentHash::EMPTY,
            prev_hash: self.announced_hash(path),
            version: previous.entry.version.incremented(self.own),
            deleted: true,
            modified_by: self.own,
            author_host: self.host.clone(),
        };
        Some(self.write(true, entry))
    }

    /// A remote entry has been committed to disk (or a remote tombstone
    /// applied) and the index takes it over unchanged, with a new `seq`.
    /// Called only after the host reports the commit, never on accept.
    ///
    /// The adopted version must dominate or equal what the record holds;
    /// the engine only applies versions that do. Checked in debug builds
    /// so the simulator catches the first time it is not.
    pub fn adopt(&mut self, entry: Entry) -> &IndexRecord {
        debug_assert!(
            self.records
                .get(&entry.path)
                .is_none_or(|r| entry.version.dominates_or_equals(&r.entry.version)),
            "adopting a version that does not dominate the record at {}",
            entry.path
        );
        let seq = self.next_seq();
        let path = entry.path.clone();
        self.pending.remove(&path);
        self.records
            .insert(path.clone(), IndexRecord { entry, seq });
        &self.records[&path]
    }

    /// Record a local change. `previous_live` says whether a live entry was
    /// at the path just before; if the path already has a pending change,
    /// what peers last saw is kept from that instead.
    fn write(&mut self, previous_live: bool, entry: Entry) -> LocalChange {
        let seq = self.next_seq();
        let path = entry.path.clone();
        let announced_live = *self.pending.entry(path.clone()).or_insert(previous_live);
        let kind = ChangeKind::between(announced_live, !entry.deleted);
        let record = IndexRecord { entry, seq };
        self.records.insert(path, record.clone());
        LocalChange { kind, record }
    }

    /// The version a brand-new local entry gets: `{own: 1}`.
    pub fn first_version(&self) -> Version {
        Version::empty().incremented(self.own)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::version::Relation;

    fn node(i: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = i;
        NodeId::from_bytes(b)
    }

    /// Never `EMPTY`, whatever `i` is: real content always has a real hash.
    fn hash(i: u8) -> ContentHash {
        let mut b = [0u8; 32];
        b[0] = i;
        b[31] = 1;
        ContentHash::from_bytes(b)
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
    fn first_observation_is_an_add_with_version_one_and_empty_prev_hash() {
        let mut idx = index();
        let change = idx.observe(p("a.txt"), file(1, 100)).unwrap();
        assert_eq!(change.kind, ChangeKind::Add);
        assert_eq!(change.record.seq, 1);
        assert_eq!(change.record.entry.version, idx.first_version());
        assert_eq!(change.record.entry.modified_by, node(1));
        assert_eq!(change.record.entry.author_host.as_str(), "laptop");
        assert_eq!(change.record.entry.prev_hash, ContentHash::EMPTY);
        assert!(!change.record.entry.is_metadata_only());
        assert!(!change.record.entry.deleted);
        assert_eq!(idx.tracked_count(), 1);
        assert_eq!(idx.seq(), 1);
        assert_eq!(idx.pending_count(), 1);
    }

    #[test]
    fn same_observation_is_not_a_change() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        assert_eq!(idx.observe(p("a.txt"), file(1, 100)), None);
        assert_eq!(idx.seq(), 1, "seq must not advance on a no-op");
    }

    #[test]
    fn content_change_is_a_modify_that_dominates_with_prev_hash() {
        let mut idx = index();
        let v1 = idx
            .observe(p("a.txt"), file(1, 100))
            .unwrap()
            .record
            .entry
            .version;
        idx.mark_announced();
        let change = idx.observe(p("a.txt"), file(2, 200)).unwrap();
        assert_eq!(change.kind, ChangeKind::Modify);
        assert_eq!(change.record.seq, 2);
        assert_eq!(
            change.record.entry.version.compare(&v1),
            Relation::Dominates
        );
        assert_eq!(change.record.entry.version.counter(node(1)), 2);
        assert_eq!(change.record.entry.prev_hash, hash(1));
        assert!(!change.record.entry.is_metadata_only());
    }

    #[test]
    fn touch_is_a_modify_with_hash_equal_to_prev_hash() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        idx.mark_announced();
        let change = idx.observe(p("a.txt"), file(1, 101)).unwrap();
        assert_eq!(change.kind, ChangeKind::Modify);
        assert_eq!(change.record.entry.hash, hash(1));
        assert_eq!(change.record.entry.prev_hash, hash(1));
        assert!(change.record.entry.is_metadata_only());
    }

    #[test]
    fn changes_within_one_window_coalesce_to_one_step() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        idx.mark_announced();
        // Three edits before the batch goes out.
        let c2 = idx.observe(p("a.txt"), file(2, 200)).unwrap();
        let c3 = idx.observe(p("a.txt"), file(3, 300)).unwrap();
        let c4 = idx.observe(p("a.txt"), file(1, 400)).unwrap();
        for c in [&c2, &c3, &c4] {
            assert_eq!(
                c.record.entry.prev_hash,
                hash(1),
                "always the announced hash"
            );
            assert_eq!(c.kind, ChangeKind::Modify);
        }
        assert!(
            c4.record.entry.is_metadata_only(),
            "edited back to the announced content: peers see a touch"
        );
        assert_eq!(idx.pending_count(), 1);
        assert_eq!(idx.seq(), 4);
    }

    #[test]
    fn add_then_modify_in_one_window_is_an_add() {
        let mut idx = index();
        idx.observe(p("n"), file(1, 1)).unwrap();
        let c = idx.observe(p("n"), file(2, 2)).unwrap();
        assert_eq!(c.kind, ChangeKind::Add);
        assert_eq!(c.record.entry.prev_hash, ContentHash::EMPTY);
        assert_eq!(
            idx.pending().map(|(_, k)| k).collect::<Vec<_>>(),
            [ChangeKind::Add]
        );
    }

    #[test]
    fn add_then_delete_in_one_window_is_an_invisible_tombstone() {
        let mut idx = index();
        idx.observe(p("n"), file(1, 1)).unwrap();
        let c = idx.observe_absent(&p("n"), 2).unwrap();
        assert_eq!(c.kind, ChangeKind::Delete);
        assert!(c.record.entry.deleted);
        assert_eq!(c.record.entry.prev_hash, ContentHash::EMPTY);
        assert!(c.record.entry.is_metadata_only(), "§8.1 will not count it");
    }

    #[test]
    fn delete_then_recreate_in_one_window_is_a_modify() {
        let mut idx = index();
        idx.observe(p("a"), file(1, 1)).unwrap();
        idx.mark_announced();
        idx.observe_absent(&p("a"), 2).unwrap();
        let c = idx.observe(p("a"), file(2, 3)).unwrap();
        assert_eq!(c.kind, ChangeKind::Modify);
        assert_eq!(c.record.entry.prev_hash, hash(1));
        assert!(!c.record.entry.is_metadata_only());
        assert_eq!(c.record.entry.version.counter(node(1)), 3);
    }

    #[test]
    fn delete_makes_a_tombstone_that_dominates() {
        let mut idx = index();
        let live = idx.observe(p("a.txt"), file(1, 100)).unwrap().record.entry;
        idx.mark_announced();
        let change = idx.observe_absent(&p("a.txt"), 500).unwrap();
        assert_eq!(change.kind, ChangeKind::Delete);
        let dead = &change.record.entry;
        assert!(dead.deleted);
        assert_eq!(dead.kind, Kind::File, "tombstone remembers the kind");
        assert_eq!(
            (dead.size, dead.hash, dead.exec, dead.mtime_ns),
            (0, ContentHash::EMPTY, false, 500)
        );
        assert_eq!(dead.prev_hash, hash(1));
        assert!(!dead.is_metadata_only(), "a deletion is a content change");
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
    fn recreation_after_announced_delete_is_an_add_that_dominates_the_tombstone() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        let tomb = idx.observe_absent(&p("a.txt"), 200).unwrap().record.entry;
        idx.mark_announced();
        let change = idx.observe(p("a.txt"), file(3, 300)).unwrap();
        assert_eq!(change.kind, ChangeKind::Add, "adds do not count toward H1");
        assert_eq!(change.record.entry.prev_hash, ContentHash::EMPTY);
        assert!(!change.record.entry.is_metadata_only());
        assert!(change.record.entry.version.dominates(&tomb.version));
        assert_eq!(change.record.entry.version.counter(node(1)), 3);
        assert_eq!(idx.tracked_count(), 1);
    }

    fn remote(path: &str, h: u8, version: Version) -> Entry {
        Entry {
            path: p(path),
            kind: Kind::File,
            size: 3,
            mtime_ns: 7,
            exec: true,
            hash: hash(h),
            prev_hash: ContentHash::EMPTY,
            version,
            deleted: false,
            modified_by: node(2),
            author_host: HostName::new("desktop").unwrap(),
        }
    }

    #[test]
    fn adopt_takes_a_remote_entry_as_is_with_a_new_seq() {
        let mut idx = index();
        idx.observe(p("a.txt"), file(1, 100)).unwrap();
        let entry = remote("b.txt", 9, Version::empty().incremented(node(2)));
        let record = idx.adopt(entry.clone()).clone();
        assert_eq!(record.entry, entry);
        assert_eq!(record.seq, 2);
        assert_eq!(idx.get(&p("b.txt")), Some(&record));
        assert_eq!(idx.pending_count(), 1, "adopting is not a local change");
        // A later local change to the adopted entry continues its vector and
        // replaces the remote hash, which peers have seen.
        let change = idx.observe(p("b.txt"), file(4, 8)).unwrap();
        assert_eq!(change.kind, ChangeKind::Modify);
        assert_eq!(change.record.entry.prev_hash, hash(9));
        assert_eq!(change.record.entry.version.counter(node(2)), 1);
        assert_eq!(change.record.entry.version.counter(node(1)), 1);
        assert!(change.record.entry.version.dominates(&entry.version));
    }

    #[test]
    fn adopt_over_an_existing_record_needs_a_dominating_version() {
        let mut idx = index();
        let mine = idx
            .observe(p("a"), file(1, 1))
            .unwrap()
            .record
            .entry
            .version;
        let newer = remote("a", 5, mine.incremented(node(2)));
        idx.adopt(newer.clone());
        assert_eq!(idx.get(&p("a")).unwrap().entry, newer);
        assert_eq!(
            idx.pending_count(),
            0,
            "the pending local change was superseded"
        );
        // Equal is allowed too (a re-commit of the same version).
        idx.adopt(newer);
    }

    #[test]
    #[should_panic(expected = "does not dominate")]
    fn adopt_of_a_concurrent_version_is_a_bug() {
        let mut idx = index();
        idx.observe(p("a"), file(1, 1)).unwrap();
        idx.adopt(remote("a", 5, Version::empty().incremented(node(2))));
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
        assert_eq!((e.size, e.hash, e.exec), (0, ContentHash::EMPTY, false));
        // Reporting the un-normalised form again is still "unchanged".
        assert_eq!(idx.observe(p("d"), dir), None);
    }

    #[test]
    fn a_child_under_an_announced_directory_does_not_touch_the_directory() {
        let mut idx = index();
        let dir = |mtime_ns| Observed {
            kind: Kind::Dir,
            size: 0,
            mtime_ns,
            exec: false,
            hash: ContentHash::EMPTY,
        };
        idx.observe(p("d"), dir(100)).unwrap();
        idx.mark_announced();
        // Creating d/child bumps the directory's mtime on disk; the host
        // reports the directory again with the new mtime.
        idx.observe(p("d/child"), file(1, 200)).unwrap();
        assert_eq!(idx.observe(p("d"), dir(200)), None);
        assert_eq!(idx.pending_kind(&p("d")), None);
        assert_eq!(idx.pending_count(), 1, "only the child is pending");
        assert_eq!(idx.get(&p("d")).unwrap().entry.mtime_ns, 0);
    }

    #[test]
    fn tracked_count_excludes_directories_and_tombstones() {
        let mut idx = index();
        idx.observe(
            p("d"),
            Observed {
                kind: Kind::Dir,
                size: 0,
                mtime_ns: 0,
                exec: false,
                hash: ContentHash::EMPTY,
            },
        )
        .unwrap();
        idx.observe(p("d/a"), file(1, 1)).unwrap();
        idx.observe(p("d/b"), file(2, 1)).unwrap();
        assert_eq!(idx.tracked_count(), 2);
        idx.observe_absent(&p("d/b"), 5).unwrap();
        assert_eq!(idx.tracked_count(), 1);
        assert_eq!(idx.len(), 3);
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
    fn unannounced_is_in_seq_order_and_marks_adopted_records() {
        let mut idx = index();
        idx.observe(p("b"), file(1, 1)).unwrap(); // seq 1
        idx.observe(p("a"), file(2, 2)).unwrap(); // seq 2
        idx.mark_announced();
        assert_eq!(idx.announced_seq(), 2);
        assert!(idx.unannounced().is_empty());
        idx.observe(p("z"), file(3, 3)).unwrap(); // seq 3, local add
        idx.adopt(remote("m", 9, Version::empty().incremented(node(2)))); // seq 4
        idx.observe(p("b"), file(4, 4)).unwrap(); // seq 5, local modify
        let got: Vec<_> = idx
            .unannounced()
            .into_iter()
            .map(|(r, k)| (r.entry.path.as_str().to_owned(), r.seq, k))
            .collect();
        assert_eq!(
            got,
            [
                ("z".to_owned(), 3, Some(ChangeKind::Add)),
                ("m".to_owned(), 4, None),
                ("b".to_owned(), 5, Some(ChangeKind::Modify)),
            ]
        );
        assert_eq!(idx.pending_kind(&p("b")), Some(ChangeKind::Modify));
        assert_eq!(idx.pending_kind(&p("m")), None);
        idx.mark_announced();
        assert_eq!(idx.announced_seq(), 5);
        assert_eq!(idx.pending_count(), 0);
    }

    #[test]
    fn index_round_trips_through_serde() {
        let mut idx = index();
        idx.observe(p("a"), file(1, 1)).unwrap();
        idx.observe_absent(&p("a"), 2).unwrap();
        idx.observe(p("b"), file(1, 1)).unwrap();
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
        Announce,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            4 => (0u8..4, 0u8..3, 0i64..4)
                .prop_map(|(path, hash, mtime)| Step::See { path, hash, mtime }),
            2 => (0u8..4, 0i64..4).prop_map(|(path, at)| Step::Gone { path, at }),
            1 => Just(Step::Announce),
        ]
    }

    fn path_of(i: u8) -> RelPath {
        p(&format!("p{i}"))
    }

    proptest! {
        /// Every change dominates the record it replaced, is authored by this
        /// machine, advances seq by exactly one, and carries as prev_hash the
        /// hash of the last version peers could have seen (EMPTY if none).
        /// No-ops leave seq alone.
        #[test]
        fn local_changes_are_monotone_and_prev_hash_tracks_announced(
            steps in prop::collection::vec(step(), 1..50)
        ) {
            let mut idx = index();
            // What peers have seen: hash and liveness of the last announced
            // record per path.
            let mut announced: BTreeMap<RelPath, (ContentHash, bool)> = BTreeMap::new();
            let mut changes = 0u64;
            for s in steps {
                let path = match &s {
                    Step::See { path, .. } | Step::Gone { path, .. } => path_of(*path),
                    Step::Announce => {
                        for (record, _) in idx.pending() {
                            announced.insert(
                                record.entry.path.clone(),
                                (record.entry.hash, !record.entry.deleted),
                            );
                        }
                        idx.mark_announced();
                        prop_assert_eq!(idx.pending_count(), 0);
                        continue;
                    }
                };
                let before = idx.get(&path).cloned();
                let was_pending = idx.pending().any(|(r, _)| r.entry.path == path);
                let seq_before = idx.seq();
                let change = match s {
                    Step::See { hash, mtime, .. } => idx.observe(path.clone(), file(hash, mtime)),
                    Step::Gone { at, .. } => idx.observe_absent(&path, at),
                    Step::Announce => unreachable!(),
                };
                match change {
                    None => prop_assert_eq!(idx.seq(), seq_before),
                    Some(c) => {
                        changes += 1;
                        prop_assert_eq!(idx.seq(), seq_before + 1);
                        prop_assert_eq!(c.record.seq, idx.seq());
                        prop_assert_eq!(c.record.entry.modified_by, node(1));
                        prop_assert_eq!(&c.record.entry.path, &path);
                        prop_assert_eq!(
                            c.record.entry.prev_hash,
                            announced.get(&path).map_or(ContentHash::EMPTY, |(h, _)| *h)
                        );
                        match &before {
                            Some(b) => {
                                prop_assert!(c.record.entry.version.dominates(&b.entry.version));
                                prop_assert_eq!(
                                    c.record.entry.version.counter(node(1)),
                                    b.entry.version.counter(node(1)) + 1
                                );
                                if !was_pending {
                                    prop_assert_eq!(c.record.entry.prev_hash, b.entry.hash);
                                }
                            }
                            None => {
                                prop_assert_eq!(c.kind, ChangeKind::Add);
                                prop_assert_eq!(c.record.entry.version, idx.first_version());
                                prop_assert_eq!(c.record.entry.prev_hash, ContentHash::EMPTY);
                            }
                        }
                        prop_assert_eq!(c.record.entry.deleted, c.kind == ChangeKind::Delete);
                        if c.record.entry.deleted {
                            prop_assert_eq!(c.record.entry.hash, ContentHash::EMPTY);
                        }
                        // The pending kind is the step from the announced state.
                        let announced_live = announced.get(&path).is_some_and(|(_, live)| *live);
                        let now_live = !c.record.entry.deleted;
                        let expected = match (announced_live, now_live) {
                            (false, true) => ChangeKind::Add,
                            (true, true) => ChangeKind::Modify,
                            (_, false) => ChangeKind::Delete,
                        };
                        prop_assert_eq!(c.kind, expected);
                    }
                }
            }
            prop_assert_eq!(idx.seq(), changes);
            prop_assert_eq!(idx.records_since(0).count(), idx.len());
            prop_assert!(idx.tracked_count() <= idx.len());
            prop_assert!(idx.pending_count() <= idx.len());
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
                    Step::Announce => idx.mark_announced(),
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
