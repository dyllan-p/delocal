//! Batches (DESIGN.md §7.4): formation from the records written since the
//! last batch, the sender's summary, the decision that doubles as the
//! acknowledgement, and the apply set a receiver computes.
//!
//! **Formation.** A batch carries every record written since the last one,
//! local changes and adopted records alike, in the sender's `seq` order. A
//! run longer than [`MAX_BATCH_ENTRIES`] is split on `seq` boundaries so
//! every batch covers a contiguous `seq` range and its `seq_high` is a true
//! watermark. The summary counts only this machine's own local changes,
//! classified as in §8.1.
//!
//! **Receiving.** Each entry is compared with the local record (absent is
//! the empty version, dominated by everything): dominates gives a candidate
//! to apply, dominated or equal is dropped, concurrent is a conflict for
//! §7.6 (PR 4). The candidates, sorted by path, are the apply set.
//!
//! Both operations are pure functions of their inputs and deterministic:
//! entries by `seq`, apply set by path.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::entry::{Entry, Kind};
use crate::id::{BatchId, FolderId, NodeId};
use crate::index::{ChangeKind, Index, IndexRecord};
use crate::path::RelPath;
use crate::time::Timestamp;
use crate::version::Relation;

/// Most entries in one batch (§7.4). Longer runs split on `seq`.
pub const MAX_BATCH_ENTRIES: usize = 10_000;

/// Counts for a batch (§7.4), classified as in §8.1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Summary {
    pub adds: u64,
    pub mods: u64,
    pub dels: u64,
    /// Bytes of counted adds and mods.
    pub bytes: u64,
}

impl Summary {
    /// Count one of this machine's local changes (§8.1).
    ///
    /// Adds always count (they are exempt from H1 anyway, and the number is
    /// informative). A mod or del counts only if it changed content, that
    /// is `hash != prev_hash`. Metadata-only changes are invisible. The
    /// sender cannot see an exec-only flip here because it no longer holds
    /// the record the change replaced; receivers classify those against
    /// their own record.
    pub fn count(&mut self, entry: &Entry, kind: ChangeKind) {
        match kind {
            ChangeKind::Add => {
                self.adds += 1;
                self.bytes += entry.size;
            }
            ChangeKind::Modify if !entry.is_metadata_only() => {
                self.mods += 1;
                self.bytes += entry.size;
            }
            ChangeKind::Delete if !entry.is_metadata_only() => self.dels += 1,
            ChangeKind::Modify | ChangeKind::Delete => {}
        }
    }

    /// `dels + mods`, the quantity H1 looks at (§8.1).
    pub fn destructive(&self) -> u64 {
        self.dels + self.mods
    }
}

/// A set of entry versions announced together by one machine (§7.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub id: BatchId,
    pub folder: FolderId,
    pub source: NodeId,
    /// When the sender formed it. Informational (§7.8).
    pub created_at: Timestamp,
    /// The sender's `seq` of the newest entry. Acknowledged by the receiver's
    /// decision; the next batch to that receiver starts above it (§7.4).
    pub seq_high: u64,
    /// In the sender's `seq` order.
    pub entries: Vec<Entry>,
    /// The sender's own local changes in this batch, classified per §8.1.
    pub summary: Summary,
}

/// A receiver's verdict on a batch (§7.4, §8.1).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Decision {
    /// The apply set goes into the want-list.
    Accepted,
    /// The apply set is quarantined until a human approves (§8.2).
    Held { reason: String },
}

/// The reply to a batch, which is also the acknowledgement (§7.4, §12):
/// either decision means "I have your records up to `seq_high`".
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BatchDecision {
    pub batch: BatchId,
    /// Carried so the sender can update its per-peer watermark without
    /// remembering which folder each batch id belonged to.
    pub folder: FolderId,
    pub decision: Decision,
    pub seq_high: u64,
}

/// Why a batch is being recorded in history (§8.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BatchRole {
    /// This machine formed and sent it.
    Sent,
    /// A peer sent it and this machine decided on it.
    Received,
    /// This machine formed it and its own pre-check paused the folder (§8.1).
    Paused,
}

/// Form the batch or batches for records written since the last one.
///
/// `records` are `(record, kind)` pairs in `seq` order, `kind` being the
/// coalesced local change or `None` for an adopted record, exactly as
/// [`Index::unannounced`] returns them. Batch `i` gets `base.successor(i)`.
/// An empty input forms no batch.
pub fn form(
    base: BatchId,
    folder: FolderId,
    source: NodeId,
    now: Timestamp,
    records: &[(&IndexRecord, Option<ChangeKind>)],
) -> Vec<Batch> {
    debug_assert!(
        records.windows(2).all(|w| w[0].0.seq < w[1].0.seq),
        "records must be in strictly increasing seq order"
    );
    records
        .chunks(MAX_BATCH_ENTRIES)
        .enumerate()
        .map(|(i, chunk)| {
            let mut summary = Summary::default();
            let mut entries = Vec::with_capacity(chunk.len());
            for (record, kind) in chunk {
                if let Some(kind) = kind {
                    summary.count(&record.entry, *kind);
                }
                entries.push(record.entry.clone());
            }
            Batch {
                id: base.successor(i as u128),
                folder,
                source,
                created_at: now,
                seq_high: chunk.last().map_or(0, |(r, _)| r.seq),
                entries,
                summary,
            }
        })
        .collect()
}

/// What applying a dominating entry involves (§7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApplyMode {
    /// File or symlink content must be fetched first.
    Fetch,
    /// No content: a directory to create or an entry to delete.
    Direct,
    /// Same content as the local record: set mtime and exec, adopt the version.
    MetadataOnly,
}

/// One candidate in an apply set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyItem {
    /// The incoming version dominates the local record.
    Apply { entry: Entry, mode: ApplyMode },
    /// Concurrent with the local record and different content: §7.6 (PR 4).
    Conflict { incoming: Entry, local: Entry },
}

impl ApplyItem {
    /// The path this item is about.
    pub fn path(&self) -> &RelPath {
        match self {
            Self::Apply { entry, .. } => &entry.path,
            Self::Conflict { incoming, .. } => &incoming.path,
        }
    }

    /// The incoming entry.
    pub fn incoming(&self) -> &Entry {
        match self {
            Self::Apply { entry, .. } => entry,
            Self::Conflict { incoming, .. } => incoming,
        }
    }
}

/// The candidates from one received batch (§7.4), sorted by path so that
/// parents come before children (§7.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplySet {
    pub folder: FolderId,
    pub batch: BatchId,
    pub source: NodeId,
    pub seq_high: u64,
    pub items: Vec<ApplyItem>,
    /// Entries dropped as equal or dominated.
    pub ignored: usize,
    /// Paths that appeared more than once; the last by `seq` was kept.
    pub duplicates: usize,
}

impl ApplySet {
    /// True if there is nothing to do.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// How one dominating entry is applied over the local record (§7.5).
fn mode_for(incoming: &Entry, local: Option<&Entry>) -> ApplyMode {
    match local {
        Some(local) if local.same_content(incoming) => ApplyMode::MetadataOnly,
        _ if incoming.deleted || incoming.kind == Kind::Dir => ApplyMode::Direct,
        _ => ApplyMode::Fetch,
    }
}

/// Compute the apply set of `batch` against `index` (§7.4 "receiving").
///
/// Duplicate paths keep the last occurrence (the highest `seq`, since the
/// batch is in `seq` order) and are counted in `duplicates`.
pub fn apply_set(index: &Index, batch: &Batch) -> ApplySet {
    let mut last_by_path: BTreeMap<&RelPath, &Entry> = BTreeMap::new();
    let mut duplicates = 0;
    for entry in &batch.entries {
        if last_by_path.insert(&entry.path, entry).is_some() {
            duplicates += 1;
        }
    }
    let mut items = Vec::new();
    let mut ignored = 0;
    for (path, incoming) in last_by_path {
        let local = index.get(path).map(|r| &r.entry);
        let local_version = local.map(|e| &e.version);
        let relation = match local_version {
            Some(v) => incoming.version.compare(v),
            None => Relation::Dominates,
        };
        match relation {
            Relation::Dominates => items.push(ApplyItem::Apply {
                entry: incoming.clone(),
                mode: mode_for(incoming, local),
            }),
            Relation::Equal | Relation::Dominated => ignored += 1,
            Relation::Concurrent => {
                // `local` is Some here: an absent record is the empty version,
                // which is never concurrent with anything.
                if let Some(local) = local {
                    items.push(ApplyItem::Conflict {
                        incoming: incoming.clone(),
                        local: local.clone(),
                    });
                }
            }
        }
    }
    // BTreeMap iteration already gave path order; keep it explicit.
    items.sort_by(|a, b| a.path().cmp(b.path()));
    ApplySet {
        folder: batch.folder,
        batch: batch.id,
        source: batch.source,
        seq_high: batch.seq_high,
        items,
        ignored,
        duplicates,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::entry::{ContentHash, Observed};
    use crate::id::HostName;
    use crate::version::Version;

    fn node(i: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = i;
        NodeId::from_bytes(b)
    }

    fn folder() -> FolderId {
        FolderId::from_bytes([7; 16])
    }

    fn batch_id(i: u8) -> BatchId {
        let mut b = [0u8; 16];
        b[15] = i;
        BatchId::from_bytes(b)
    }

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

    fn dir() -> Observed {
        Observed {
            kind: Kind::Dir,
            size: 0,
            mtime_ns: 1,
            exec: false,
            hash: ContentHash::EMPTY,
        }
    }

    fn index(i: u8) -> Index {
        Index::new(node(i), HostName::new("h").unwrap())
    }

    fn now() -> Timestamp {
        Timestamp::from_unix_nanos(1_000)
    }

    fn entry(path: &str, h: u8, version: Version, by: u8) -> Entry {
        Entry {
            path: p(path),
            kind: Kind::File,
            size: 10,
            mtime_ns: 5,
            exec: false,
            hash: hash(h),
            prev_hash: ContentHash::EMPTY,
            version,
            deleted: false,
            modified_by: node(by),
            author_host: HostName::new("h").unwrap(),
        }
    }

    fn batch_of(entries: Vec<Entry>, seq_high: u64) -> Batch {
        Batch {
            id: batch_id(1),
            folder: folder(),
            source: node(2),
            created_at: now(),
            seq_high,
            entries,
            summary: Summary::default(),
        }
    }

    // ---- summary -------------------------------------------------------------

    #[test]
    fn summary_classifies_per_section_8_1() {
        let mut idx = index(1);
        idx.observe(p("a"), file(1, 1)).unwrap();
        idx.observe(p("b"), file(2, 1)).unwrap();
        idx.observe(p("d"), dir()).unwrap();
        idx.mark_announced();
        idx.observe(p("a"), file(3, 2)).unwrap(); // real edit
        idx.observe(p("b"), file(2, 9)).unwrap(); // touch
        idx.observe_absent(&p("d"), 3).unwrap(); // dir delete: EMPTY -> EMPTY
        idx.observe(p("n"), file(4, 4)).unwrap(); // add
        idx.observe(p("m"), file(5, 4)).unwrap();
        idx.observe_absent(&p("m"), 5).unwrap(); // add then delete in window
        let mut s = Summary::default();
        for (r, k) in idx.unannounced() {
            s.count(&r.entry, k.unwrap());
        }
        assert_eq!(
            s,
            Summary {
                adds: 1,
                mods: 1,
                dels: 0,
                bytes: 20
            }
        );
        assert_eq!(s.destructive(), 1);
    }

    #[test]
    fn summary_counts_a_real_delete() {
        let mut idx = index(1);
        idx.observe(p("a"), file(1, 1)).unwrap();
        idx.mark_announced();
        idx.observe_absent(&p("a"), 2).unwrap();
        let mut s = Summary::default();
        for (r, k) in idx.unannounced() {
            s.count(&r.entry, k.unwrap());
        }
        assert_eq!((s.dels, s.mods, s.adds, s.bytes), (1, 0, 0, 0));
    }

    // ---- formation -----------------------------------------------------------

    #[test]
    fn forms_nothing_from_nothing() {
        assert!(form(batch_id(1), folder(), node(1), now(), &[]).is_empty());
    }

    #[test]
    fn forms_one_batch_in_seq_order_counting_only_local_changes() {
        let mut idx = index(1);
        idx.observe(p("z"), file(1, 1)).unwrap(); // seq 1
        let adopted = entry("m", 9, Version::empty().incremented(node(2)), 2);
        idx.adopt(adopted.clone()); // seq 2
        idx.observe(p("a"), file(2, 2)).unwrap(); // seq 3
        let batches = form(batch_id(1), folder(), node(1), now(), &idx.unannounced());
        assert_eq!(batches.len(), 1);
        let b = &batches[0];
        assert_eq!(b.id, batch_id(1));
        assert_eq!(
            (b.folder, b.source, b.created_at),
            (folder(), node(1), now())
        );
        assert_eq!(b.seq_high, 3);
        let paths: Vec<_> = b.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["z", "m", "a"], "seq order, not path order");
        assert_eq!(b.entries[1], adopted, "adopted record goes as it stands");
        assert_eq!(b.summary.adds, 2, "the adopted record is not counted");
        assert_eq!(b.summary.bytes, 20);
    }

    #[test]
    fn splits_on_seq_boundaries_with_successor_ids() {
        let mut idx = index(1);
        for i in 0..(MAX_BATCH_ENTRIES + 5) {
            idx.observe(p(&format!("f{i:05}")), file(1, i as i64))
                .unwrap();
        }
        let batches = form(batch_id(1), folder(), node(1), now(), &idx.unannounced());
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].id, batch_id(1));
        assert_eq!(batches[1].id, batch_id(1).successor(1));
        assert_eq!(batches[0].entries.len(), MAX_BATCH_ENTRIES);
        assert_eq!(batches[1].entries.len(), 5);
        assert_eq!(batches[0].seq_high, MAX_BATCH_ENTRIES as u64);
        assert_eq!(batches[1].seq_high, MAX_BATCH_ENTRIES as u64 + 5);
        assert_eq!(batches[0].summary.adds, MAX_BATCH_ENTRIES as u64);
        assert_eq!(batches[1].summary.adds, 5);
    }

    // ---- apply set -------------------------------------------------------------

    #[test]
    fn apply_set_classifies_each_relation() {
        let mut idx = index(1);
        let mine = idx.observe(p("same"), file(1, 1)).unwrap().record.entry;
        idx.observe(p("older"), file(2, 1)).unwrap();
        idx.observe(p("conflict"), file(3, 1)).unwrap();
        idx.observe(p("touched"), file(4, 1)).unwrap();
        idx.observe(p("gone"), file(5, 1)).unwrap();
        let v = |path: &str| idx.get(&p(path)).unwrap().entry.version.clone();
        let remote2 = |path: &str, h: u8, ver: Version| entry(path, h, ver, 2);
        let mut touched = remote2("touched", 4, v("touched").incremented(node(2)));
        touched.mtime_ns = 99;
        let mut gone = remote2("gone", 5, v("gone").incremented(node(2)));
        gone.deleted = true;
        gone.hash = ContentHash::EMPTY;
        let mut newdir = remote2("newdir", 0, Version::empty().incremented(node(2)));
        newdir.kind = Kind::Dir;
        newdir.hash = ContentHash::EMPTY;
        let batch = batch_of(
            vec![
                remote2("same", 1, mine.version.clone()), // equal
                remote2("older", 9, Version::empty()),    // dominated (empty)
                remote2("conflict", 7, Version::empty().incremented(node(2))), // concurrent
                touched,                                  // dominates, same content
                gone,                                     // dominates, tombstone
                remote2("newfile", 8, Version::empty().incremented(node(2))), // absent locally
                newdir,
            ],
            7,
        );
        let set = apply_set(&idx, &batch);
        assert_eq!(
            (set.folder, set.batch, set.source, set.seq_high),
            (folder(), batch_id(1), node(2), 7)
        );
        assert_eq!(set.ignored, 2);
        assert_eq!(set.duplicates, 0);
        let got: Vec<(String, String)> = set
            .items
            .iter()
            .map(|i| {
                let what = match i {
                    ApplyItem::Apply { mode, .. } => format!("{mode:?}"),
                    ApplyItem::Conflict { .. } => "Conflict".to_owned(),
                };
                (i.path().as_str().to_owned(), what)
            })
            .collect();
        assert_eq!(
            got,
            [
                ("conflict".to_owned(), "Conflict".to_owned()),
                ("gone".to_owned(), "Direct".to_owned()),
                ("newdir".to_owned(), "Direct".to_owned()),
                ("newfile".to_owned(), "Fetch".to_owned()),
                ("touched".to_owned(), "MetadataOnly".to_owned()),
            ],
            "sorted by path"
        );
        if let ApplyItem::Conflict { local, .. } = &set.items[0] {
            assert_eq!(local.hash, hash(3));
        }
    }

    #[test]
    fn duplicate_paths_keep_the_last_and_are_counted() {
        let idx = index(1);
        let v1 = Version::empty().incremented(node(2));
        let v2 = v1.incremented(node(2));
        let batch = batch_of(vec![entry("a", 1, v1, 2), entry("a", 2, v2.clone(), 2)], 2);
        let set = apply_set(&idx, &batch);
        assert_eq!(set.duplicates, 1);
        assert_eq!(set.items.len(), 1);
        assert_eq!(set.items[0].incoming().version, v2);
        assert_eq!(set.items[0].incoming().hash, hash(2));
    }

    #[test]
    fn two_tombstones_are_metadata_only() {
        let mut idx = index(1);
        idx.observe(p("a"), file(1, 1)).unwrap();
        let tomb = idx.observe_absent(&p("a"), 2).unwrap().record.entry;
        let mut incoming = tomb.clone();
        incoming.version = tomb.version.incremented(node(2));
        incoming.modified_by = node(2);
        let set = apply_set(&idx, &batch_of(vec![incoming], 1));
        assert!(matches!(
            set.items[0],
            ApplyItem::Apply {
                mode: ApplyMode::MetadataOnly,
                ..
            }
        ));
    }

    #[test]
    fn batch_and_decision_round_trip() {
        let mut idx = index(1);
        idx.observe(p("a"), file(1, 1)).unwrap();
        let b = form(batch_id(1), folder(), node(1), now(), &idx.unannounced()).remove(0);
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<Batch>(&json).unwrap(), b);
        let bytes = postcard::to_stdvec(&b).unwrap();
        assert_eq!(postcard::from_bytes::<Batch>(&bytes).unwrap(), b);
        let d = BatchDecision {
            batch: b.id,
            folder: b.folder,
            decision: Decision::Held {
                reason: "count".into(),
            },
            seq_high: b.seq_high,
        };
        let bytes = postcard::to_stdvec(&d).unwrap();
        assert_eq!(postcard::from_bytes::<BatchDecision>(&bytes).unwrap(), d);
    }

    // ---- properties ------------------------------------------------------------

    #[derive(Clone, Debug)]
    enum Step {
        See { path: u8, hash: u8, mtime: i64 },
        Gone { path: u8, at: i64 },
        Adopt { path: u8, hash: u8 },
        Announce,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            4 => (0u8..5, 0u8..3, 0i64..4).prop_map(|(path, hash, mtime)| Step::See { path, hash, mtime }),
            2 => (0u8..5, 0i64..4).prop_map(|(path, at)| Step::Gone { path, at }),
            2 => (0u8..5, 0u8..3).prop_map(|(path, hash)| Step::Adopt { path, hash }),
            1 => Just(Step::Announce),
        ]
    }

    fn path_of(i: u8) -> RelPath {
        p(&format!("p{i}"))
    }

    fn drive(idx: &mut Index, steps: &[Step]) {
        for s in steps {
            match *s {
                Step::See { path, hash, mtime } => {
                    idx.observe(path_of(path), file(hash, mtime));
                }
                Step::Gone { path, at } => {
                    idx.observe_absent(&path_of(path), at);
                }
                Step::Adopt { path, hash } => {
                    // A remote version that dominates whatever we hold.
                    let base = idx
                        .get(&path_of(path))
                        .map(|r| r.entry.version.clone())
                        .unwrap_or_default();
                    let e = entry(&format!("p{path}"), hash, base.incremented(node(2)), 2);
                    idx.adopt(e);
                }
                Step::Announce => idx.mark_announced(),
            }
        }
    }

    proptest! {
        /// Every unannounced record is in exactly one batch, batches are in
        /// seq order with contiguous ranges and true watermarks, and forming
        /// leaves nothing unannounced once marked.
        #[test]
        fn formation_covers_unannounced_exactly_once(steps in prop::collection::vec(step(), 1..40)) {
            let mut idx = index(1);
            drive(&mut idx, &steps);
            let unannounced = idx.unannounced();
            let batches = form(batch_id(1), folder(), node(1), now(), &unannounced);
            let total: usize = batches.iter().map(|b| b.entries.len()).sum();
            prop_assert_eq!(total, unannounced.len());
            let mut expected = unannounced.iter().map(|(r, _)| r.entry.clone());
            let mut last_seq = idx.announced_seq();
            for (i, b) in batches.iter().enumerate() {
                prop_assert_eq!(b.id, batch_id(1).successor(i as u128));
                for e in &b.entries {
                    let next = expected.next();
                    prop_assert_eq!(Some(e), next.as_ref());
                    let seq = idx.get(&e.path).unwrap().seq;
                    prop_assert!(seq > last_seq);
                    last_seq = seq;
                }
                prop_assert_eq!(b.seq_high, last_seq);
            }
            prop_assert!(expected.next().is_none());
            if let Some(last) = batches.last() {
                prop_assert_eq!(last.seq_high, idx.seq());
            }
            idx.mark_announced();
            prop_assert!(idx.unannounced().is_empty());
        }

        /// I6 groundwork: forming from a cloned index gives byte-identical batches.
        #[test]
        fn formation_is_deterministic(steps in prop::collection::vec(step(), 1..40)) {
            let mut idx = index(1);
            drive(&mut idx, &steps);
            let other = idx.clone();
            let a = form(batch_id(1), folder(), node(1), now(), &idx.unannounced());
            let b = form(batch_id(1), folder(), node(1), now(), &other.unannounced());
            prop_assert_eq!(postcard::to_stdvec(&a).unwrap(), postcard::to_stdvec(&b).unwrap());
        }

        /// The apply set agrees with Version::compare and same_content for
        /// every entry, is sorted by path, and is deterministic.
        #[test]
        fn apply_set_agrees_with_compare(
            local_steps in prop::collection::vec(step(), 0..30),
            remote_steps in prop::collection::vec(step(), 1..30),
        ) {
            let mut local = index(1);
            drive(&mut local, &local_steps);
            let mut remote = index(3);
            drive(&mut remote, &remote_steps);
            let batches = form(batch_id(1), folder(), node(3), now(), &remote.unannounced());
            for batch in &batches {
                let set = apply_set(&local, batch);
                let again = apply_set(&local, batch);
                prop_assert_eq!(&set, &again);
                prop_assert!(set.items.windows(2).all(|w| w[0].path() < w[1].path()));
                let mut expected_items = 0;
                let mut expected_ignored = 0;
                let mut last_seen: BTreeMap<&RelPath, &Entry> = BTreeMap::new();
                for e in &batch.entries { last_seen.insert(&e.path, e); }
                prop_assert_eq!(set.duplicates, batch.entries.len() - last_seen.len());
                for (path, incoming) in last_seen {
                    let item = set.items.iter().find(|i| i.path() == path);
                    match local.get(path) {
                        None => {
                            expected_items += 1;
                            let is_apply = matches!(item, Some(ApplyItem::Apply { .. }));
                            prop_assert!(is_apply, "absent locally must be Apply, got {item:?}");
                        }
                        Some(r) => match incoming.version.compare(&r.entry.version) {
                            Relation::Dominates => {
                                expected_items += 1;
                                match item {
                                    Some(ApplyItem::Apply { mode, .. }) => {
                                        let want = if r.entry.same_content(incoming) {
                                            ApplyMode::MetadataOnly
                                        } else if incoming.deleted || incoming.kind == Kind::Dir {
                                            ApplyMode::Direct
                                        } else {
                                            ApplyMode::Fetch
                                        };
                                        prop_assert_eq!(*mode, want);
                                    }
                                    other => prop_assert!(false, "expected Apply, got {other:?}"),
                                }
                            }
                            Relation::Concurrent => {
                                expected_items += 1;
                                let is_conflict = matches!(item, Some(ApplyItem::Conflict { .. }));
                                prop_assert!(is_conflict, "concurrent must be Conflict, got {item:?}");
                            }
                            Relation::Equal | Relation::Dominated => {
                                expected_ignored += 1;
                                prop_assert!(item.is_none());
                            }
                        },
                    }
                }
                prop_assert_eq!(set.items.len(), expected_items);
                prop_assert_eq!(set.ignored, expected_ignored);
            }
        }
    }
}
