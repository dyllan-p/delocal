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
//! **The pending set and `prev_hash`.** For every path with an unannounced
//! local change the index keeps the record peers last saw: the record that
//! was there before the first pending change, or nothing if the path was
//! unknown to them. `prev_hash` is that record's hash (§7.1), so several
//! changes inside one batch window collapse to one step from the last
//! announced version. The change's kind is derived the same way, from
//! whether peers last saw a live entry and whether one is there now: an add
//! then a modify is still an add, a delete then a re-creation is a modify,
//! an add then a delete is a delete of something peers never had (a
//! tombstone with `hash == prev_hash == EMPTY`). The same stored record is
//! what `revert` (§8.3) puts back, `seq` included.
//!
//! What counts as a local change: any difference between the observation
//! and the record in kind, size, mtime, exec or hash. An mtime-only change
//! (`touch`) is a change with `hash == prev_hash` (§7.3): receivers apply it
//! as metadata only (§7.5), the brake ignores it (§8.1), and it loses to
//! any real edit in a conflict (§7.6). The scanner's fast path (kind, size,
//! mtime and exec bit, [`Entry::unchanged_by_stat`]) is the host's to apply
//! before it reports; no tolerance is applied to mtime here, the precision
//! shim being host work too (§7.3).

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

/// One path undone by `revert` (§8.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reverted {
    pub path: RelPath,
    /// The record that was there, always present (it was pending).
    pub current: Option<Entry>,
    /// The record peers last saw, now back in place; `None` if the record
    /// was removed because peers never saw the path.
    pub restored: Option<IndexRecord>,
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
    /// What we hold of each remote member's `seq` space (§7.4): the
    /// contiguous watermark and the received ranges above it.
    peer_seq: BTreeMap<NodeId, Watermark>,
    /// Local changes not yet announced, with the record peers last saw at
    /// the path (`None` if they never saw it). Fixes the change's kind and
    /// `prev_hash`, and is what `revert` restores.
    pending: BTreeMap<RelPath, Option<IndexRecord>>,
    /// `seq` of the newest record in the last batch this machine formed
    /// (§7.4). Records above it, local or adopted, go in the next batch.
    announced_seq: u64,
    /// Tracked count as of the last announcement: the sender's H1
    /// denominator (§8.1).
    announced_tracked: usize,
}

/// What this machine holds of one peer's `seq` space (§7.4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    /// Every `seq` up to here has been received.
    contiguous: u64,
    /// Ranges `(low, high]` received above a gap, keyed by `low`.
    ranges: BTreeMap<u64, u64>,
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
            announced_tracked: 0,
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

    /// Where this machine may serve `hash` from, for a `RequestFile` at
    /// `path` (§7.5 steps 2 and 3): the record at `path` first if it is live
    /// with that hash, then every other live file or symlink with that hash
    /// in path order. Content is requested by hash, not by version, so a
    /// holder of the winning side serves a merged version it has not seen,
    /// and any concurrent holder of the same bytes serves them. Several
    /// candidates rather than one because the disk must still match the
    /// record (size and mtime), which only the host can check; it takes the
    /// first that does and answers `NotAvailable` if none does. Directories
    /// and tombstones carry [`ContentHash::EMPTY`] and serve nothing.
    pub fn locate<'a>(
        &'a self,
        path: &'a RelPath,
        hash: &'a ContentHash,
    ) -> impl Iterator<Item = &'a RelPath> + 'a {
        let holds = move |r: &&'a IndexRecord| {
            !r.entry.deleted && r.entry.kind != Kind::Dir && r.entry.hash == *hash
        };
        let here = self.records.get(path).filter(holds).map(|r| &r.entry.path);
        let elsewhere = self
            .records
            .values()
            .filter(holds)
            .filter(move |r| r.entry.path != *path)
            .map(|r| &r.entry.path);
        let any = *hash != ContentHash::EMPTY;
        here.into_iter().chain(elsewhere).filter(move |_| any)
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

    /// Every announced record with `seq` above `after`, in `seq` order: what
    /// catch-up sends a peer (§7.4). For a path with a pending local change
    /// the announced record is the one the pending set keeps, not the
    /// current record, so a paused folder's pending set never leaks.
    pub fn announced_since(&self, after: u64) -> Vec<&IndexRecord> {
        let mut out: Vec<&IndexRecord> = self
            .records
            .iter()
            .filter_map(|(path, record)| match self.pending.get(path) {
                Some(announced) => announced.as_ref(),
                None => (record.seq <= self.announced_seq).then_some(record),
            })
            .filter(|r| r.seq > after)
            .collect();
        out.sort_by_key(|r| r.seq);
        out
    }

    /// The contiguous watermark for `peer`: every `seq` of theirs up to it
    /// has been received, 0 if none. This is what a decision acknowledges
    /// and what `have_up_to` reports (§7.4).
    pub fn peer_seq(&self, peer: NodeId) -> u64 {
        self.peer_seq.get(&peer).map_or(0, |w| w.contiguous)
    }

    /// True if batches from `peer` above the watermark have arrived while
    /// something below them has not (§7.4).
    pub fn peer_has_gap(&self, peer: NodeId) -> bool {
        self.peer_seq
            .get(&peer)
            .is_some_and(|w| !w.ranges.is_empty())
    }

    /// A batch from `peer` covering `(seq_low, seq_high]` arrived (§7.4). The
    /// watermark advances when the range starts at or below it, and then
    /// absorbs every range that has become contiguous; a range above a gap
    /// is remembered until the gap fills.
    pub fn received_range(&mut self, peer: NodeId, seq_low: u64, seq_high: u64) {
        let w = self.peer_seq.entry(peer).or_default();
        if seq_high <= w.contiguous {
            return;
        }
        if seq_low <= w.contiguous {
            w.contiguous = seq_high;
        } else {
            let high = w.ranges.entry(seq_low).or_insert(seq_high);
            *high = (*high).max(seq_high);
        }
        while let Some((&low, &high)) = w.ranges.range(..=w.contiguous).next() {
            w.ranges.remove(&low);
            w.contiguous = w.contiguous.max(high);
        }
    }

    /// Local changes not yet announced, in path order, with the coalesced
    /// kind of each. Batch formation (§7.4) reads this.
    pub fn pending(&self) -> impl Iterator<Item = (&IndexRecord, ChangeKind)> {
        self.pending.iter().filter_map(|(path, announced)| {
            self.records.get(path).map(|r| {
                (
                    r,
                    ChangeKind::between(Self::is_live(announced), !r.entry.deleted),
                )
            })
        })
    }

    /// True if a stored announced record is a live entry.
    fn is_live(announced: &Option<IndexRecord>) -> bool {
        announced.as_ref().is_some_and(|r| !r.entry.deleted)
    }

    /// True if `path` has an unannounced local change.
    pub fn is_pending(&self, path: &RelPath) -> bool {
        self.pending.contains_key(path)
    }

    /// Paths with an unannounced local change, in path order.
    pub fn pending_paths(&self) -> impl Iterator<Item = &RelPath> {
        self.pending.keys()
    }

    /// Number of paths with an unannounced local change.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The coalesced kind of the pending local change at `path`, if any.
    pub fn pending_kind(&self, path: &RelPath) -> Option<ChangeKind> {
        let announced = self.pending.get(path)?;
        let record = self.records.get(path)?;
        Some(ChangeKind::between(
            Self::is_live(announced),
            !record.entry.deleted,
        ))
    }

    /// Tracked count as of the last announcement (§8.1, sender pre-check).
    pub fn announced_tracked(&self) -> usize {
        self.announced_tracked
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
        self.announced_tracked = self.tracked_count();
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The hash of the last version of `path` that peers could have seen:
    /// the pending change's `prev_hash` if there is one, else the current
    /// record's hash, else `EMPTY`.
    fn announced_hash(&self, path: &RelPath) -> ContentHash {
        match self.pending.get(path) {
            Some(announced) => announced
                .as_ref()
                .map_or(ContentHash::EMPTY, |r| r.entry.hash),
            None => self
                .records
                .get(path)
                .map_or(ContentHash::EMPTY, |r| r.entry.hash),
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
        if let Some(r) = previous
            && !r.entry.deleted
            && r.entry.observed() == observed
        {
            return None;
        }
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
        Some(self.write(entry))
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
        Some(self.write(entry))
    }

    /// The conflict copy of a displaced losing file (§7.6): `loser`'s kind,
    /// size, mtime, exec and hash at `path`, a fresh version, and this
    /// machine as `modified_by` and `author_host`, because the copy is this
    /// machine's change (the loser's author is in the file name). A local
    /// add; the path joins the pending set.
    ///
    /// `prev_hash` is the hash of the last version of `path` peers could have
    /// seen, exactly as for any local change. That is `EMPTY`, as §7.6 says,
    /// whenever the conflict path was absent or a tombstone when the
    /// conflict was classified, which is every case §7.6 describes. It
    /// differs only if peers saw a live file there that was deleted inside
    /// the current batch window, where the §7.1 definition of `prev_hash`
    /// must win or the announced step would be wrong.
    pub fn record_conflict_copy(&mut self, path: RelPath, loser: &Entry) -> LocalChange {
        let previous = self.records.get(&path);
        let version = previous
            .map(|r| r.entry.version.clone())
            .unwrap_or_default()
            .incremented(self.own);
        let prev_hash = self.announced_hash(&path);
        let entry = Entry {
            path,
            kind: loser.kind,
            size: loser.size,
            mtime_ns: loser.mtime_ns,
            exec: loser.exec,
            hash: loser.hash,
            prev_hash,
            version,
            deleted: false,
            modified_by: self.own,
            author_host: self.host.clone(),
        };
        self.write(entry)
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

    /// Record a local change. If the path has no pending change yet, the
    /// record being replaced is what peers last saw, and is kept for
    /// `prev_hash`, the change's kind and `revert`.
    fn write(&mut self, entry: Entry) -> LocalChange {
        let seq = self.next_seq();
        let path = entry.path.clone();
        let previous = self.records.get(&path).cloned();
        let announced = self.pending.entry(path.clone()).or_insert(previous);
        let kind = ChangeKind::between(Self::is_live(announced), !entry.deleted);
        let record = IndexRecord { entry, seq };
        self.records.insert(path, record.clone());
        LocalChange { kind, record }
    }

    /// `deny` (§8.2): a local change at `path` whose version is
    /// `merge(local, every quarantined version).incremented(self)`, so it
    /// dominates all of them, with content unchanged. With no local record
    /// the result is a tombstone dated `at_ns`. `hash == prev_hash`, so on
    /// peers that hold the same content this lands as a metadata-only apply.
    pub fn bump_over(&mut self, path: &RelPath, over: &[Version], at_ns: i64) -> LocalChange {
        let local = self.records.get(path).map(|r| r.entry.clone());
        let base = local
            .as_ref()
            .map(|e| e.version.clone())
            .unwrap_or_default();
        let version = over
            .iter()
            .fold(base, |acc, v| acc.merge(v))
            .incremented(self.own);
        let prev_hash = self.announced_hash(path);
        let entry = match local {
            Some(e) => Entry {
                version,
                prev_hash,
                modified_by: self.own,
                author_host: self.host.clone(),
                ..e
            },
            None => Entry {
                path: path.clone(),
                kind: Kind::File,
                size: 0,
                mtime_ns: at_ns,
                exec: false,
                hash: ContentHash::EMPTY,
                prev_hash,
                version,
                deleted: true,
                modified_by: self.own,
                author_host: self.host.clone(),
            },
        };
        self.write(entry)
    }

    /// `revert` (§8.3): undo every unannounced local change. Each pending
    /// path gets back the record peers last saw, `seq` included, so it is
    /// not re-announced; a path peers never saw loses its record. The
    /// pending set empties. Returns what happened per path, in path order.
    pub fn revert_pending(&mut self) -> Vec<Reverted> {
        let pending = std::mem::take(&mut self.pending);
        let mut out = Vec::with_capacity(pending.len());
        for (path, announced) in pending {
            let current = self.records.remove(&path);
            if let Some(record) = &announced {
                self.records.insert(path.clone(), record.clone());
            }
            out.push(Reverted {
                path,
                current: current.map(|r| r.entry),
                restored: announced,
            });
        }
        out
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
    fn conflict_copy_is_a_local_add_with_the_losers_content_and_our_authorship() {
        let mut idx = index();
        let loser = remote("x.txt", 4, Version::empty().incremented(node(2)));
        let change = idx.record_conflict_copy(p("x.conflict-copy.txt"), &loser);
        assert_eq!(change.kind, ChangeKind::Add);
        let e = &change.record.entry;
        assert_eq!(e.path, p("x.conflict-copy.txt"));
        assert_eq!(
            (e.kind, e.size, e.mtime_ns, e.exec, e.hash),
            (
                loser.kind,
                loser.size,
                loser.mtime_ns,
                loser.exec,
                loser.hash
            )
        );
        assert_eq!(e.prev_hash, ContentHash::EMPTY);
        assert_eq!(e.version, idx.first_version());
        assert_eq!(
            e.modified_by,
            node(1),
            "this machine, not the loser's author"
        );
        assert_eq!(e.author_host.as_str(), "laptop");
        assert!(!e.deleted);
        assert_eq!(
            idx.pending_kind(&p("x.conflict-copy.txt")),
            Some(ChangeKind::Add)
        );
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
            "the one legitimate adopt over a pending path: a conflict's merged version M (§7.6)"
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
    fn announced_tracked_is_the_count_at_the_last_announcement() {
        let mut idx = index();
        for i in 0..10 {
            idx.observe(p(&format!("f{i}")), file(1, 1)).unwrap();
        }
        assert_eq!(idx.announced_tracked(), 0, "nothing announced yet");
        idx.mark_announced();
        assert_eq!(idx.announced_tracked(), 10);
        for i in 0..8 {
            idx.observe_absent(&p(&format!("f{i}")), 5).unwrap();
        }
        assert_eq!(idx.tracked_count(), 2);
        assert_eq!(
            idx.announced_tracked(),
            10,
            "the denominator the brake needs"
        );
    }

    #[test]
    fn bump_over_dominates_every_version_and_keeps_content() {
        let mut idx = index();
        idx.observe(p("a"), file(1, 1)).unwrap();
        idx.mark_announced();
        let mine = idx.get(&p("a")).unwrap().entry.clone();
        let q1 = Version::empty().incremented(node(2)).incremented(node(2));
        let q2: Version = [(node(3), 4)].into_iter().collect();
        let c = idx.bump_over(&p("a"), &[q1.clone(), q2.clone()], 9);
        let e = &c.record.entry;
        assert!(e.version.dominates(&q1));
        assert!(e.version.dominates(&q2));
        assert!(e.version.dominates(&mine.version));
        assert_eq!(e.version.counter(node(1)), 2);
        assert_eq!(
            (e.hash, e.size, e.mtime_ns, e.kind),
            (mine.hash, mine.size, mine.mtime_ns, mine.kind)
        );
        assert!(e.is_metadata_only(), "content unchanged");
        assert_eq!(e.modified_by, node(1));
        assert_eq!(c.kind, ChangeKind::Modify);
        // No record: a tombstone that still dominates.
        let c = idx.bump_over(&p("never"), std::slice::from_ref(&q1), 77);
        let e = &c.record.entry;
        assert!(e.deleted);
        assert!(e.version.dominates(&q1));
        assert_eq!(e.mtime_ns, 77);
        assert!(e.is_metadata_only());
        assert_eq!(c.kind, ChangeKind::Delete);
    }

    #[test]
    fn revert_pending_restores_announced_records_exactly_and_removes_new_ones() {
        let mut idx = index();
        idx.observe(p("keep"), file(1, 1)).unwrap();
        idx.observe(p("edit"), file(2, 2)).unwrap(); // seq 2
        idx.observe(p("gone"), file(3, 3)).unwrap(); // seq 3
        idx.mark_announced();
        let edit_before = idx.get(&p("edit")).unwrap().clone();
        let gone_before = idx.get(&p("gone")).unwrap().clone();
        idx.observe(p("edit"), file(4, 4)).unwrap();
        idx.observe(p("edit"), file(5, 5)).unwrap();
        idx.observe_absent(&p("gone"), 6).unwrap();
        idx.observe(p("new"), file(6, 6)).unwrap();
        idx.adopt(remote("adopted", 9, Version::empty().incremented(node(2))));
        let seq_before = idx.seq();
        assert_eq!(idx.pending_count(), 3);

        let reverted = idx.revert_pending();
        let paths: Vec<_> = reverted.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["edit", "gone", "new"]);
        assert_eq!(idx.get(&p("edit")), Some(&edit_before), "seq included");
        assert_eq!(idx.get(&p("gone")), Some(&gone_before));
        assert_eq!(idx.get(&p("new")), None, "peers never saw it");
        assert_eq!(reverted[2].restored, None);
        assert_eq!(reverted[2].current.as_ref().unwrap().hash, hash(6));
        assert!(reverted[1].current.as_ref().unwrap().deleted);
        assert_eq!(idx.pending_count(), 0);
        assert_eq!(idx.seq(), seq_before, "no new seq");
        let unannounced: Vec<_> = idx
            .unannounced()
            .iter()
            .map(|(r, _)| r.entry.path.as_str().to_owned())
            .collect();
        assert_eq!(
            unannounced,
            ["adopted"],
            "restored records are not re-announced"
        );
        assert_eq!(idx.get(&p("keep")).unwrap().entry.hash, hash(1));
    }

    #[test]
    fn announced_since_uses_the_pending_sets_announced_records() {
        let mut idx = index();
        idx.observe(p("a"), file(1, 1)).unwrap(); // seq 1
        idx.observe(p("b"), file(2, 2)).unwrap(); // seq 2
        idx.observe(p("c"), file(3, 3)).unwrap(); // seq 3
        idx.mark_announced();
        let b_announced = idx.get(&p("b")).unwrap().clone();
        idx.observe_absent(&p("b"), 9).unwrap(); // seq 4, pending
        idx.observe(p("d"), file(4, 4)).unwrap(); // seq 5, pending, never announced
        let got: Vec<(&str, u64)> = idx
            .announced_since(0)
            .iter()
            .map(|r| (r.entry.path.as_str(), r.seq))
            .collect();
        assert_eq!(
            got,
            [("a", 1), ("b", 2), ("c", 3)],
            "b as announced, d not at all"
        );
        assert_eq!(idx.announced_since(0)[1], &b_announced);
        assert_eq!(idx.announced_since(2).len(), 1);
        assert!(idx.announced_since(3).is_empty());
    }

    #[test]
    fn locate_prefers_the_path_then_any_live_holder_of_the_hash() {
        let mut idx = index();
        idx.observe(p("a"), file(1, 1));
        idx.observe(p("b"), file(1, 1));
        idx.observe(p("c"), file(2, 1));
        idx.observe(
            p("d"),
            Observed {
                kind: Kind::Dir,
                ..file(1, 1)
            }
            .normalised(),
        );
        fn at(idx: &Index, path: &str, h: u8) -> Vec<String> {
            idx.locate(&p(path), &hash(h))
                .map(|r| r.as_str().to_owned())
                .collect()
        }
        assert_eq!(at(&idx, "b", 1), ["b", "a"], "the requested path first");
        assert_eq!(at(&idx, "c", 1), ["a", "b"], "content elsewhere serves too");
        assert_eq!(at(&idx, "a", 2), ["c"]);
        assert_eq!(at(&idx, "a", 3), Vec::<String>::new(), "nobody has it");
        assert!(
            at(&idx, "d", 0).is_empty(),
            "a directory's hash serves nothing"
        );
        idx.observe_absent(&p("a"), 2);
        assert_eq!(at(&idx, "a", 1), ["b"], "a tombstone serves nothing");
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
        idx.received_range(node(2), 0, 5);
        idx.received_range(node(2), 0, 3);
        assert_eq!(idx.peer_seq(node(2)), 5, "never moves backwards");
    }

    #[test]
    fn the_watermark_waits_for_a_gap_to_fill() {
        let mut idx = index();
        idx.received_range(node(2), 0, 4);
        // Batches (7, 9] and (9, 12] arrive before (4, 7]: a gap.
        idx.received_range(node(2), 7, 9);
        idx.received_range(node(2), 9, 12);
        assert_eq!(idx.peer_seq(node(2)), 4, "acknowledged at the watermark");
        assert!(idx.peer_has_gap(node(2)));
        // Catch-up re-sends everything above the acknowledged 4.
        idx.received_range(node(2), 4, 7);
        assert_eq!(
            idx.peer_seq(node(2)),
            12,
            "the gap filled, the ranges absorbed"
        );
        assert!(!idx.peer_has_gap(node(2)));
        // A range entirely below the watermark is old news.
        idx.received_range(node(2), 2, 3);
        assert_eq!(idx.peer_seq(node(2)), 12);
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
        idx.received_range(node(2), 0, 4);
        let json = serde_json::to_string(&idx).unwrap();
        assert_eq!(serde_json::from_str::<Index>(&json).unwrap(), idx);
        let bytes = postcard::to_stdvec(&idx).unwrap();
        assert_eq!(postcard::from_bytes::<Index>(&bytes).unwrap(), idx);
    }

    // ---- properties -----------------------------------------------------------

    #[derive(Clone, Debug)]
    enum Step {
        See {
            path: u8,
            hash: u8,
            mtime: i64,
        },
        Gone {
            path: u8,
            at: i64,
        },
        /// A conflict copy landing at `path`, only if no live record is there
        /// (classification guarantees that, §7.6).
        Copy {
            path: u8,
            hash: u8,
        },
        Announce,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            4 => (0u8..4, 0u8..3, 0i64..4)
                .prop_map(|(path, hash, mtime)| Step::See { path, hash, mtime }),
            2 => (0u8..4, 0i64..4).prop_map(|(path, at)| Step::Gone { path, at }),
            1 => (0u8..4, 0u8..3).prop_map(|(path, hash)| Step::Copy { path, hash }),
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
                    Step::See { path, .. } | Step::Gone { path, .. } | Step::Copy { path, .. } => {
                        path_of(*path)
                    }
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

                    Step::Copy { hash, .. } => {

                        if idx.live(&path).is_some() {

                            continue;

                        }

                        let loser = remote("loser", hash, Version::empty().incremented(node(2)));

                        Some(idx.record_conflict_copy(path.clone(), &loser))

                    }
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
                    Step::Copy { path, hash } => {

                        let path = path_of(path);

                        if idx.live(&path).is_none() {

                            let loser = remote("loser", hash, Version::empty().incremented(node(2)));

                            let c = idx.record_conflict_copy(path.clone(), &loser);

                            last.insert(path, Some(c.record.entry.observed()));

                        }

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
