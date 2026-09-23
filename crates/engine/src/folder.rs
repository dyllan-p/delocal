//! Per-folder state (DESIGN.md §7.3 scan brackets, §7.4 batch window,
//! §7.4 receiving, §7.5 commit bookkeeping).
//!
//! One [`FolderState`] per folder this machine is a member of. It owns the
//! folder's [`Index`], the batch window, the open scan bracket, the apply
//! sets accepted but not yet committed, and each peer's acknowledgement of
//! this machine's `seq`. `Engine` (in `engine.rs`) drives it and turns what
//! it returns into actions.
//!
//! **Window.** Any index write, local or adopted, opens the window if it is
//! closed and records the time of the last write. The window is due at
//! `min(opened + 10 s, last + 2 s)` (§7.4). Adoptions open it too, so a
//! relayed change (§7.4, A → B → C) does not wait for B to change something
//! of its own.
//!
//! **Scan bracket.** Between `ScanStarted` and `ScanFinished` every path the
//! host reports is marked seen; at `ScanFinished` every live record not seen
//! becomes a tombstone dated then (§7.3). `ScanAborted` drops the bracket and
//! announces nothing, which is the root guard's promise. Reports outside a
//! bracket are watcher events and apply directly.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::batch::{self, ApplyItem, ApplySet, Batch};
use crate::conflict::ConflictCopy;
use crate::entry::{Entry, Observed};
use crate::id::{BatchId, FolderId, HostName, NodeId};
use crate::index::{Index, IndexRecord, LocalChange};
use crate::path::RelPath;
use crate::rules::Rules;
use crate::time::{DEBOUNCE_NANOS, Timestamp, WINDOW_NANOS};
use crate::version::Version;

/// What the host reports for one path (§7.3). `Unchanged` is the fast path:
/// size and mtime matched the record the host holds, so no hash was
/// computed; the engine only marks the path seen.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScanState {
    Absent,
    Unchanged,
    Observed(Observed),
}

/// The result of a host's commit attempt (§7.5 step 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApplyOutcome {
    /// Displaced, renamed in, index may adopt.
    Ok,
    /// The local file was not what the index said. Nothing was written. The
    /// engine keeps the entry and re-evaluates it after the next observation
    /// of the path (§7.5).
    ChangedUnderneath,
}

/// An incoming entry whose commit found the file changed underneath (§7.5
/// step 6), waiting for the next observation of its path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deferred {
    /// The entry to re-classify: the incoming entry, or `M` for a conflict.
    pub entry: Entry,
    /// The batch it arrived in, so the re-evaluated item keeps its origin.
    pub batch: BatchId,
    pub source: NodeId,
    pub seq_high: u64,
}

/// Something the folder wants `status` to know about.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FolderStatus {
    /// A batch or command arrived for a folder this machine has not joined.
    UnknownFolder,
    /// `FolderJoined` for a folder already joined; the existing state, index
    /// included, was kept.
    AlreadyJoined,
    /// The host reported `Unchanged` for a path with no live record: a host bug.
    UnchangedUnknownPath { path: RelPath },
    /// `ScanFinished` or `ScanAborted` without an open bracket: a host bug.
    ScanNotOpen,
    /// A received batch repeated a path; the last by `seq` was kept (§7.4).
    DuplicatePaths { batch: BatchId, count: usize },
    /// Rule 5 of the winner rule (§7.6) decided this many conflicts. It
    /// should never fire; any count is a bug to find.
    WinnerFallback { count: u64 },
}

/// An open batch window (§7.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Window {
    /// Time of the first write since the last batch.
    pub opened: Timestamp,
    /// Time of the most recent write.
    pub last: Timestamp,
}

impl Window {
    /// When a batch should form: 2 s after the last write or 10 s after
    /// the first, whichever comes first (§7.4).
    pub fn due(&self) -> Timestamp {
        self.opened
            .plus_nanos(WINDOW_NANOS)
            .min(self.last.plus_nanos(DEBOUNCE_NANOS))
    }
}

/// What handling one scan report produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scanned {
    pub change: Option<LocalChange>,
    pub status: Option<FolderStatus>,
}

/// State of one folder on this machine. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderState {
    id: FolderId,
    rules: Rules,
    members: BTreeSet<NodeId>,
    index: Index,
    window: Option<Window>,
    /// Paths reported since `ScanStarted`, while a bracket is open.
    scan: Option<BTreeSet<RelPath>>,
    /// Accepted apply sets whose items the host has not yet committed (PR 6
    /// turns these into fetch and commit actions).
    accepted: Vec<ApplySet>,
    /// This machine's `seq` as acknowledged by each peer's decisions (§7.4).
    acked: BTreeMap<NodeId, u64>,
    /// Entries whose commit found the file changed underneath, by path,
    /// waiting for the next observation (§7.5 step 6).
    deferred: BTreeMap<RelPath, Deferred>,
    /// How many conflicts rule 5 of §7.6 has decided here. Should stay 0.
    winner_fallbacks: u64,
}

impl FolderState {
    /// Join a folder with an empty index.
    pub fn new(
        id: FolderId,
        rules: Rules,
        members: impl IntoIterator<Item = NodeId>,
        own: NodeId,
        host: HostName,
    ) -> Self {
        Self {
            id,
            rules,
            members: members.into_iter().collect(),
            index: Index::new(own, host),
            window: None,
            scan: None,
            accepted: Vec::new(),
            acked: BTreeMap::new(),
            deferred: BTreeMap::new(),
            winner_fallbacks: 0,
        }
    }

    pub fn id(&self) -> FolderId {
        self.id
    }

    pub fn rules(&self) -> &Rules {
        &self.rules
    }

    pub fn set_rules(&mut self, rules: Rules) {
        self.rules = rules;
    }

    /// Members in `NodeId` order, this machine included if it was listed.
    pub fn members(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.members.iter().copied()
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    /// The open batch window, if any.
    pub fn window(&self) -> Option<Window> {
        self.window
    }

    /// When the next batch should form, if a window is open.
    pub fn due(&self) -> Option<Timestamp> {
        self.window.map(|w| w.due())
    }

    /// True if a window is open and due at or before `now`.
    pub fn is_due(&self, now: Timestamp) -> bool {
        self.due().is_some_and(|d| d <= now)
    }

    /// True while a scan bracket is open.
    pub fn scan_open(&self) -> bool {
        self.scan.is_some()
    }

    /// Apply sets accepted and not yet fully committed.
    pub fn accepted(&self) -> &[ApplySet] {
        &self.accepted
    }

    /// Highest `seq` of ours that `peer` has acknowledged, 0 if none.
    pub fn acked_by(&self, peer: NodeId) -> u64 {
        self.acked.get(&peer).copied().unwrap_or(0)
    }

    /// Entries waiting for the next observation of their path (§7.5 step 6).
    pub fn deferred(&self) -> impl Iterator<Item = &Deferred> {
        self.deferred.values()
    }

    /// Conflicts decided by rule 5 of §7.6 so far. Any value above 0 is a
    /// bug the simulator should find.
    pub fn winner_fallbacks(&self) -> u64 {
        self.winner_fallbacks
    }

    /// A record was written at `now`: open or extend the window.
    fn touched(&mut self, now: Timestamp) {
        self.window = Some(match self.window {
            None => Window {
                opened: now,
                last: now,
            },
            Some(w) => Window {
                opened: w.opened,
                last: w.last.max(now),
            },
        });
    }

    /// The host reported `state` at `path` (§7.3), inside or outside a bracket.
    pub fn scanned(&mut self, now: Timestamp, path: RelPath, state: ScanState) -> Scanned {
        if let Some(seen) = &mut self.scan {
            seen.insert(path.clone());
        }
        let change = match state {
            ScanState::Observed(observed) => self.index.observe(path.clone(), observed),
            ScanState::Absent => self.index.observe_absent(&path, now.as_unix_nanos()),
            ScanState::Unchanged => {
                if self.index.live(&path).is_none() {
                    return Scanned {
                        change: None,
                        status: Some(FolderStatus::UnchangedUnknownPath { path }),
                    };
                }
                None
            }
        };
        if change.is_some() {
            self.touched(now);
        }
        self.reconsider(&path);
        Scanned {
            change,
            status: None,
        }
    }

    /// A path with a deferred entry has been observed again: classify the
    /// entry against the index as it now stands (§7.5 step 6). It becomes a
    /// one-item accepted set, an `Apply` if it still dominates or a
    /// `Conflict` if the new local version is concurrent, or is dropped if
    /// the index has caught up with it.
    fn reconsider(&mut self, path: &RelPath) {
        let Some(deferred) = self.deferred.remove(path) else {
            return;
        };
        let classified = batch::classify(&self.index, &deferred.entry);
        if classified.fallback {
            self.winner_fallbacks += 1;
        }
        if let Some(item) = classified.item {
            self.accepted.push(ApplySet {
                folder: self.id,
                batch: deferred.batch,
                source: deferred.source,
                seq_high: deferred.seq_high,
                items: vec![item],
                ignored: 0,
                duplicates: 0,
                fallbacks: usize::from(classified.fallback),
            });
        }
    }

    /// A full scan begins: start collecting the paths it reports.
    pub fn scan_started(&mut self) {
        self.scan = Some(BTreeSet::new());
    }

    /// A full scan ended: every live record it did not report is gone
    /// (§7.3). Returns the tombstones, or `Err` if no bracket was open.
    pub fn scan_finished(&mut self, now: Timestamp) -> Result<Vec<LocalChange>, FolderStatus> {
        let seen = self.scan.take().ok_or(FolderStatus::ScanNotOpen)?;
        let gone: Vec<RelPath> = self
            .index
            .live_records()
            .map(|r| r.entry.path.clone())
            .filter(|p| !seen.contains(p))
            .collect();
        let mut changes = Vec::new();
        for path in &gone {
            if let Some(change) = self.index.observe_absent(path, now.as_unix_nanos()) {
                changes.push(change);
            }
            // A deferred entry whose file vanished underneath is only ever
            // caught here: later scans never report a tombstoned path.
            self.reconsider(path);
        }
        if !changes.is_empty() {
            self.touched(now);
        }
        Ok(changes)
    }

    /// The host stopped a scan (root guard, read error): forget what it
    /// reported and announce nothing (§7.3). Observations already applied
    /// inside the bracket stand; only the deletion pass is skipped.
    pub fn scan_aborted(&mut self) -> Result<(), FolderStatus> {
        self.scan
            .take()
            .map(|_| ())
            .ok_or(FolderStatus::ScanNotOpen)
    }

    /// Form the batch or batches for everything written since the last one
    /// (§7.4) and close the window. `base` is the fresh id for the first;
    /// the rest are its successors.
    pub fn form_batches(&mut self, now: Timestamp, base: BatchId) -> Vec<Batch> {
        let batches = batch::form(
            base,
            self.id,
            self.index.own(),
            now,
            &self.index.unannounced(),
        );
        self.index.mark_announced();
        self.window = None;
        batches
    }

    /// A batch arrived: compute its apply set (§7.4), keep it for the host
    /// to work through, and note how far the source's records now reach.
    /// The brake (PR 5) will sit between the apply set and the decision.
    pub fn receive(&mut self, batch: &Batch) -> ApplySet {
        self.index.set_peer_seq(batch.source, batch.seq_high);
        let set = batch::apply_set(&self.index, batch);
        self.winner_fallbacks += set.fallbacks as u64;
        if !set.is_empty() {
            self.accepted.push(set.clone());
        }
        set
    }

    /// A peer's decision on one of our batches acknowledges our records up
    /// to `seq_high` (§7.4). Never moves backwards.
    pub fn acknowledged(&mut self, peer: NodeId, seq_high: u64) {
        let slot = self.acked.entry(peer).or_insert(0);
        *slot = (*slot).max(seq_high);
    }

    /// The host finished committing (or failed to commit) an accepted item
    /// (§7.5 steps 6 to 9). On `Ok` the index adopts the entry and the
    /// window opens so the adoption is announced (§7.4); if the item
    /// carried a conflict copy, the displaced file is recorded at the
    /// conflict path as this machine's local add (§7.6), without waiting for
    /// a scan. On `ChangedUnderneath` the entry is kept in the deferred set
    /// until the path is observed again; the sender has been acknowledged
    /// for it and will not send it twice. Either way the item leaves the
    /// accepted sets. Returns every record written, in order; empty if
    /// nothing matched or the commit did not happen.
    pub fn applied(
        &mut self,
        now: Timestamp,
        path: &RelPath,
        version: &Version,
        outcome: ApplyOutcome,
    ) -> Vec<IndexRecord> {
        let mut found: Option<(Deferred, Option<ConflictCopy>)> = None;
        for set in &mut self.accepted {
            if let Some(pos) = set.items.iter().position(|item| {
                let e = item.incoming();
                &e.path == path && &e.version == version
            }) {
                let ApplyItem::Apply {
                    entry, conflict, ..
                } = set.items.remove(pos);
                found = Some((
                    Deferred {
                        entry,
                        batch: set.batch,
                        source: set.source,
                        seq_high: set.seq_high,
                    },
                    conflict,
                ));
                break;
            }
        }
        self.accepted.retain(|set| !set.is_empty());
        let Some((deferred, conflict)) = found else {
            return Vec::new();
        };
        match outcome {
            ApplyOutcome::Ok => {
                let mut written = vec![self.adopt(now, deferred.entry)];
                if let Some(copy) = conflict {
                    let change = self.index.record_conflict_copy(copy.path, &copy.loser);
                    self.touched(now);
                    written.push(change.record);
                }
                written
            }
            ApplyOutcome::ChangedUnderneath => {
                self.deferred.insert(path.clone(), deferred);
                Vec::new()
            }
        }
    }

    /// Take a committed remote entry into the index and open the window so
    /// it is announced (§7.4 "adopted records are announced too").
    pub fn adopt(&mut self, now: Timestamp, entry: Entry) -> IndexRecord {
        let record = self.index.adopt(entry).clone();
        self.touched(now);
        record
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::batch::ApplyMode;
    use crate::conflict::{Side, resolve};
    use crate::entry::{ContentHash, Kind};
    use crate::index::ChangeKind;
    use crate::time::NANOS_PER_SECOND;

    fn node(i: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = i;
        NodeId::from_bytes(b)
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

    fn t(secs: f64) -> Timestamp {
        Timestamp::from_unix_nanos((secs * NANOS_PER_SECOND as f64) as i64)
    }

    fn file(h: u8, mtime_ns: i64) -> ScanState {
        ScanState::Observed(Observed {
            kind: Kind::File,
            size: 10,
            mtime_ns,
            exec: false,
            hash: hash(h),
        })
    }

    fn folder() -> FolderState {
        FolderState::new(
            FolderId::from_bytes([7; 16]),
            Rules::default(),
            [node(1), node(2)],
            node(1),
            HostName::new("laptop").unwrap(),
        )
    }

    fn batch_id() -> BatchId {
        BatchId::from_bytes([9; 16])
    }

    #[test]
    fn window_due_is_min_of_debounce_and_cap() {
        let mut f = folder();
        assert_eq!(f.due(), None);
        assert!(!f.is_due(t(100.0)));
        f.scanned(t(10.0), p("a"), file(1, 1));
        assert_eq!(
            f.window(),
            Some(Window {
                opened: t(10.0),
                last: t(10.0)
            })
        );
        assert_eq!(f.due(), Some(t(12.0)), "2 s of quiet");
        f.scanned(t(11.5), p("b"), file(1, 1));
        assert_eq!(f.due(), Some(t(13.5)));
        f.scanned(t(13.0), p("c"), file(1, 1));
        f.scanned(t(15.0), p("d"), file(1, 1));
        f.scanned(t(17.0), p("e"), file(1, 1));
        f.scanned(t(19.0), p("f"), file(1, 1));
        assert_eq!(f.due(), Some(t(20.0)), "capped at 10 s after the first");
        assert!(!f.is_due(t(19.9)));
        assert!(f.is_due(t(20.0)));
        let batches = f.form_batches(t(20.0), batch_id());
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].entries.len(), 6);
        assert_eq!(f.due(), None, "window closes");
        assert!(f.index().unannounced().is_empty());
    }

    #[test]
    fn a_no_op_report_does_not_open_the_window() {
        let mut f = folder();
        f.scanned(t(1.0), p("a"), file(1, 1));
        f.form_batches(t(3.0), batch_id());
        let out = f.scanned(t(4.0), p("a"), file(1, 1));
        assert_eq!(out, Scanned::default());
        assert_eq!(f.due(), None);
        let out = f.scanned(t(4.0), p("a"), ScanState::Unchanged);
        assert_eq!(out, Scanned::default());
        assert_eq!(f.due(), None);
    }

    #[test]
    fn unchanged_for_an_unknown_path_is_a_host_bug() {
        let mut f = folder();
        let out = f.scanned(t(1.0), p("ghost"), ScanState::Unchanged);
        assert_eq!(out.change, None);
        assert_eq!(
            out.status,
            Some(FolderStatus::UnchangedUnknownPath { path: p("ghost") })
        );
        assert_eq!(f.index().len(), 0);
        assert_eq!(f.due(), None);
    }

    #[test]
    fn scan_bracket_tombstones_what_it_did_not_see() {
        let mut f = folder();
        f.scanned(t(1.0), p("keep"), file(1, 1));
        f.scanned(t(1.0), p("gone"), file(2, 1));
        f.scanned(t(1.0), p("dir/inner"), file(3, 1));
        f.form_batches(t(3.0), batch_id());
        f.scan_started();
        assert!(f.scan_open());
        f.scanned(t(5.0), p("keep"), ScanState::Unchanged);
        f.scanned(t(5.0), p("dir/inner"), file(4, 2));
        let changes = f.scan_finished(t(6.0)).unwrap();
        assert!(!f.scan_open());
        assert_eq!(changes.len(), 1);
        let tomb = &changes[0].record.entry;
        assert_eq!(tomb.path, p("gone"));
        assert!(tomb.deleted);
        assert_eq!(tomb.mtime_ns, t(6.0).as_unix_nanos());
        assert_eq!(f.index().tracked_count(), 2);
        assert_eq!(
            f.due(),
            Some(t(8.0)),
            "window opened at the 5 s write in the bracket; the 6 s tombstone extends the quiet period"
        );
    }

    #[test]
    fn aborted_scan_announces_no_deletes() {
        let mut f = folder();
        f.scanned(t(1.0), p("a"), file(1, 1));
        f.scanned(t(1.0), p("b"), file(2, 1));
        f.form_batches(t(3.0), batch_id());
        f.scan_started();
        f.scanned(t(5.0), p("a"), ScanState::Unchanged);
        assert_eq!(f.scan_aborted(), Ok(()));
        assert!(!f.scan_open());
        assert_eq!(f.index().tracked_count(), 2);
        assert_eq!(f.due(), None);
        assert_eq!(f.scan_aborted(), Err(FolderStatus::ScanNotOpen));
        assert_eq!(f.scan_finished(t(6.0)), Err(FolderStatus::ScanNotOpen));
    }

    #[test]
    fn watcher_absent_outside_a_bracket_deletes_directly() {
        let mut f = folder();
        f.scanned(t(1.0), p("a"), file(1, 1));
        let out = f.scanned(t(2.0), p("a"), ScanState::Absent);
        assert!(out.change.unwrap().record.entry.deleted);
        assert_eq!(
            f.scanned(t(3.0), p("a"), ScanState::Absent),
            Scanned::default()
        );
    }

    #[test]
    fn receive_stores_non_empty_apply_sets_and_applied_adopts() {
        let mut a = folder();
        a.scanned(t(1.0), p("x"), file(1, 1));
        let batch = a.form_batches(t(3.0), batch_id()).remove(0);

        let mut b = FolderState::new(
            a.id(),
            Rules::default(),
            [node(1), node(2)],
            node(2),
            HostName::new("desktop").unwrap(),
        );
        let set = b.receive(&batch);
        assert_eq!(set.items.len(), 1);
        assert_eq!(b.accepted().len(), 1);
        // Receiving is not writing: no window.
        assert_eq!(b.due(), None);

        let entry = set.items[0].incoming().clone();
        let none = b.applied(t(5.0), &p("nope"), &entry.version, ApplyOutcome::Ok);
        assert!(none.is_empty());
        let record = b
            .applied(t(5.0), &entry.path, &entry.version, ApplyOutcome::Ok)
            .remove(0);
        assert_eq!(record.entry, entry);
        assert_eq!(record.seq, 1);
        assert!(b.accepted().is_empty(), "the set emptied and was dropped");
        assert_eq!(b.due(), Some(t(7.0)), "an adoption opens the window");
        let relayed = b.form_batches(t(7.0), batch_id().successor(5)).remove(0);
        assert_eq!(relayed.entries, vec![entry.clone()]);
        assert_eq!(relayed.summary, Default::default(), "adopted, not counted");

        // A second copy of the same batch is entirely ignored now.
        let again = b.receive(&batch);
        assert!(again.is_empty());
        assert_eq!(again.ignored, 1);
        assert!(b.accepted().is_empty());
    }

    #[test]
    fn changed_underneath_keeps_the_entry_until_the_path_is_observed() {
        let mut a = folder();
        a.scanned(t(1.0), p("x"), file(1, 1));
        let batch = a.form_batches(t(3.0), batch_id()).remove(0);
        let mut b = FolderState::new(
            a.id(),
            Rules::default(),
            [node(1), node(2)],
            node(2),
            HostName::new("desktop").unwrap(),
        );
        let set = b.receive(&batch);
        let entry = set.items[0].incoming().clone();
        let out = b.applied(
            t(5.0),
            &entry.path,
            &entry.version,
            ApplyOutcome::ChangedUnderneath,
        );
        assert!(out.is_empty());
        assert!(b.accepted().is_empty());
        assert_eq!(b.index().get(&p("x")), None);
        assert_eq!(b.due(), None);
        let kept: Vec<_> = b.deferred().collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].entry, entry);
        assert_eq!(
            (kept[0].batch, kept[0].source, kept[0].seq_high),
            (batch.id, node(1), 1)
        );

        // The scan finds the file the user created underneath: a new local
        // version, concurrent with A's and with different content. A's has
        // the larger mtime, so A wins and the local file becomes the copy.
        b.scanned(t(6.0), p("x"), file(9, 0));
        assert!(b.deferred().next().is_none());
        assert_eq!(b.accepted().len(), 1);
        let set = &b.accepted()[0];
        assert_eq!(
            (set.batch, set.source, set.seq_high),
            (batch.id, node(1), 1)
        );
        let item = &set.items[0];
        assert_eq!(item.mode(), ApplyMode::Fetch);
        let m = item.incoming();
        assert_eq!(m.hash, hash(1), "M carries the winner's content");
        assert!(m.version.dominates(&entry.version));
        assert!(
            m.version
                .dominates(&b.index().get(&p("x")).unwrap().entry.version)
        );
        let copy = item.conflict().unwrap();
        assert_eq!(copy.loser.hash, hash(9));
        assert_eq!(copy.loser.modified_by, node(2));
        assert_eq!(copy.path.as_str(), "x.conflict-19700101-000000-desktop");
    }

    #[test]
    fn a_deferred_entry_whose_path_vanishes_in_a_scan_bracket_becomes_a_conflict() {
        // B creates x and announces it; A adopts it, edits it and announces
        // the edit, which dominates B's version.
        let mut b = FolderState::new(
            FolderId::from_bytes([7; 16]),
            Rules::default(),
            [node(1), node(2)],
            node(2),
            HostName::new("desktop").unwrap(),
        );
        let b_entry = b
            .scanned(t(0.5), p("x"), file(5, 5))
            .change
            .unwrap()
            .record
            .entry;
        b.form_batches(t(2.5), batch_id().successor(1));
        let mut a = folder();
        a.adopt(t(3.0), b_entry);
        a.scanned(t(3.5), p("x"), file(1, 1));
        let batch = a.form_batches(t(5.5), batch_id()).remove(0);

        let set = b.receive(&batch);
        let entry = set.items[0].incoming().clone();
        assert!(matches!(set.items[0], ApplyItem::Apply { .. }));
        // The user edited x meanwhile, so the commit finds it changed.
        b.applied(
            t(6.0),
            &entry.path,
            &entry.version,
            ApplyOutcome::ChangedUnderneath,
        );
        assert_eq!(b.deferred().count(), 1);

        // Then the user deleted x. The watcher missed it; only the full scan
        // notices, and a scan never reports a path that is gone.
        b.scan_started();
        let changes = b.scan_finished(t(7.0)).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].record.entry.deleted);
        assert!(b.deferred().next().is_none(), "not stuck");
        assert_eq!(b.accepted().len(), 1);
        // Delete vs modify: the live incoming side wins (§7.6 rule 1). There is
        // no local file to displace, so no conflict copy; M carries A's
        // content under the merged vector.
        let item = &b.accepted()[0].items[0];
        assert_eq!(item.mode(), ApplyMode::Fetch);
        assert!(item.conflict().is_none());
        let m = item.incoming();
        assert!(m.same_content(&entry));
        assert!(!m.deleted);
        let tomb = &b.index().get(&p("x")).unwrap().entry;
        assert!(tomb.deleted);
        assert!(m.version.dominates(&tomb.version));
        assert!(m.version.dominates(&entry.version));
    }

    #[test]
    fn a_deferred_entry_that_still_dominates_is_re_accepted() {
        let mut a = folder();
        a.scanned(t(1.0), p("x"), file(1, 1));
        let batch = a.form_batches(t(3.0), batch_id()).remove(0);
        let mut b = folder();
        let entry = b.receive(&batch).items[0].incoming().clone();
        b.applied(
            t(5.0),
            &entry.path,
            &entry.version,
            ApplyOutcome::ChangedUnderneath,
        );
        // The observation finds nothing there after all (a transient file).
        b.scanned(t(6.0), p("x"), ScanState::Absent);
        assert_eq!(b.accepted().len(), 1);
        assert!(matches!(
            &b.accepted()[0].items[0],
            ApplyItem::Apply { entry: e, mode: ApplyMode::Fetch, conflict: None } if *e == entry
        ));
        assert!(b.deferred().next().is_none());
    }

    #[test]
    fn receive_records_the_source_seq() {
        let mut a = folder();
        a.scanned(t(1.0), p("x"), file(1, 1));
        a.scanned(t(1.0), p("y"), file(2, 1));
        let batch = a.form_batches(t(3.0), batch_id()).remove(0);
        let mut b = folder();
        assert_eq!(b.index().peer_seq(node(1)), 0);
        b.receive(&batch);
        assert_eq!(b.index().peer_seq(node(1)), 2);
        assert_eq!(b.index().peer_seq(node(1)), batch.seq_high);
    }

    #[test]
    fn acknowledgements_never_move_backwards() {
        let mut f = folder();
        assert_eq!(f.acked_by(node(2)), 0);
        f.acknowledged(node(2), 7);
        f.acknowledged(node(2), 3);
        assert_eq!(f.acked_by(node(2)), 7);
    }

    #[test]
    fn folder_state_round_trips() {
        let mut f = folder();
        f.scanned(t(1.0), p("a"), file(1, 1));
        f.scan_started();
        f.scanned(t(1.5), p("a"), ScanState::Unchanged);
        f.acknowledged(node(2), 1);
        let bytes = postcard::to_stdvec(&f).unwrap();
        assert_eq!(postcard::from_bytes::<FolderState>(&bytes).unwrap(), f);
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<FolderState>(&json).unwrap(), f);
    }

    fn desktop() -> FolderState {
        FolderState::new(
            FolderId::from_bytes([7; 16]),
            Rules::default(),
            [node(1), node(2), node(3)],
            node(2),
            HostName::new("desktop").unwrap(),
        )
    }

    fn observed(kind: Kind, h: u8, mtime_ns: i64, exec: bool) -> ScanState {
        ScanState::Observed(Observed {
            kind,
            size: if kind == Kind::Dir { 0 } else { 10 },
            mtime_ns,
            exec,
            hash: if kind == Kind::Dir {
                ContentHash::EMPTY
            } else {
                hash(h)
            },
        })
    }

    /// Host shorthand: commit every accepted item as `Ok`.
    fn commit_all(f: &mut FolderState, now: Timestamp) -> Vec<IndexRecord> {
        let items: Vec<(RelPath, Version)> = f
            .accepted()
            .iter()
            .flat_map(|s| s.items.iter())
            .map(|i| (i.path().clone(), i.incoming().version.clone()))
            .collect();
        let mut out = Vec::new();
        for (path, version) in items {
            out.extend(f.applied(now, &path, &version, ApplyOutcome::Ok));
        }
        out
    }

    #[test]
    fn w_holder_adopts_m_index_only_and_nothing_pends_for_the_copy() {
        // B creates the base; A adopts it. Both edit: B later (wins), A earlier.
        let mut b = desktop();
        let base = b
            .scanned(t(1.0), p("x"), file(1, 1))
            .change
            .unwrap()
            .record
            .entry;
        b.form_batches(t(3.0), batch_id());
        let mut a = folder();
        a.adopt(t(3.0), base);
        a.scanned(t(4.0), p("x"), file(2, 100)); // L
        let batch = a.form_batches(t(6.0), batch_id().successor(1)).remove(0);
        b.scanned(t(4.5), p("x"), file(3, 200)); // W
        let set = b.receive(&batch);
        let item = &set.items[0];
        assert_eq!(item.mode(), ApplyMode::IndexOnly);
        assert!(item.conflict().is_none());
        let m = item.incoming();
        assert_eq!(m.hash, hash(3), "M has the local (winning) content");
        assert_eq!(m.modified_by, node(2));
        let written = commit_all(&mut b, t(7.0));
        assert_eq!(written.len(), 1, "no conflict copy on the winner's side");
        assert_eq!(b.index().get(&p("x")).unwrap().entry, *m);
        assert_eq!(b.index().pending_count(), 0, "B's edit is announced as M");
        let out = b.form_batches(t(9.0), batch_id().successor(2)).remove(0);
        assert_eq!(out.entries, vec![m.clone()]);
    }

    #[test]
    fn a_non_empty_directory_that_loses_is_displaced_with_its_children() {
        let mut b = desktop();
        b.scanned(t(1.0), p("d"), observed(Kind::Dir, 0, 0, false));
        b.scanned(t(1.0), p("d/a"), file(1, 1));
        b.scanned(t(1.0), p("d/b"), file(2, 1));
        b.form_batches(t(3.0), batch_id());
        // A never saw the directory and created a file named d.
        let mut a = folder();
        a.scanned(t(2.0), p("d"), file(7, 5));
        let batch = a.form_batches(t(4.0), batch_id().successor(1)).remove(0);
        let set = b.receive(&batch);
        let item = &set.items[0];
        assert_eq!(
            item.mode(),
            ApplyMode::Fetch,
            "the file wins: mtime 5 beats a directory's 0"
        );
        let copy = item.conflict().unwrap().clone();
        assert_eq!(copy.path.as_str(), "d.conflict-19700101-000000-desktop");
        assert_eq!(copy.loser.kind, Kind::Dir);
        // The host moves the whole directory aside and writes the file.
        let written = commit_all(&mut b, t(5.0));
        assert_eq!(written.len(), 2);
        assert_eq!(written[0].entry.kind, Kind::File);
        assert_eq!(written[1].entry.kind, Kind::Dir);
        assert_eq!(written[1].entry.path, copy.path);
        // The next full scan sees the moved children and not the old ones.
        b.scan_started();
        b.scanned(t(6.0), p("d"), ScanState::Unchanged);
        b.scanned(t(6.0), copy.path.clone(), ScanState::Unchanged);
        b.scanned(t(6.0), copy.path.join("a").unwrap(), file(1, 1));
        b.scanned(t(6.0), copy.path.join("b").unwrap(), file(2, 1));
        b.scan_finished(t(7.0)).unwrap();
        let pending: Vec<(String, ChangeKind)> = b
            .index()
            .pending()
            .map(|(r, k)| (r.entry.path.as_str().to_owned(), k))
            .collect();
        assert_eq!(
            pending,
            [
                (
                    "d.conflict-19700101-000000-desktop".to_owned(),
                    ChangeKind::Add
                ),
                (
                    "d.conflict-19700101-000000-desktop/a".to_owned(),
                    ChangeKind::Add
                ),
                (
                    "d.conflict-19700101-000000-desktop/b".to_owned(),
                    ChangeKind::Add
                ),
                ("d/a".to_owned(), ChangeKind::Delete),
                ("d/b".to_owned(), ChangeKind::Delete),
            ],
            "the copy and two adds, two tombstones; known behaviour per §7.6"
        );
    }

    proptest! {
        /// Under random write times the window is due at exactly
        /// min(first + 10 s, last + 2 s), never earlier, and a batch formed
        /// when due carries every write since the last batch.
        #[test]
        fn window_rule_holds_for_random_write_times(
            gaps in prop::collection::vec(0i64..4 * NANOS_PER_SECOND, 1..12)
        ) {
            let mut f = folder();
            let mut now = t(100.0);
            let mut first = None;
            for (i, gap) in gaps.iter().enumerate() {
                now = now.plus_nanos(*gap);
                // Nothing forms before it is due.
                if let Some(d) = f.due() {
                    prop_assert!(!f.is_due(d.plus_nanos(-1)));
                }
                f.scanned(now, p(&format!("f{i}")), file(1, i as i64));
                let first = *first.get_or_insert(now);
                // `now` is the time of the last write.
                let expected = first.plus_nanos(WINDOW_NANOS).min(now.plus_nanos(DEBOUNCE_NANOS));
                prop_assert_eq!(f.due(), Some(expected));
            }
            let due = f.due().unwrap();
            prop_assert!(f.is_due(due));
            let batches = f.form_batches(due, batch_id());
            let total: usize = batches.iter().map(|b| b.entries.len()).sum();
            prop_assert_eq!(total, gaps.len());
            prop_assert_eq!(f.due(), None);
            prop_assert!(f.index().unannounced().is_empty());
        }

        /// §7.6: two L-holders make conflict copies with the same path and the
        /// same content, which then merge under §7.2 once they exchange
        /// batches; every machine ends with the same M at the original path.
        #[test]
        fn two_losers_make_identical_copies_that_merge(
            l_hash in 0u8..3, hash_gap in 1u8..3, l_mtime in 0i64..3, mtime_gap in 1i64..3,
            l_exec: bool, w_exec: bool,
        ) {
            // W always has different content (hash) and a later mtime than L,
            // so W wins by rule 3 without any assumption to reject on.
            let w_hash = (l_hash + hash_gap) % 3;
            let w_mtime = l_mtime + mtime_gap;
            // B makes the base; A and C adopt it. B edits it (L), A edits it (W):
            // concurrent. C takes B's edit first and so also holds L.
            let mut b = desktop();
            // hash 9 and mtime 9 are outside the ranges the edits draw from.
            let base = b.scanned(t(1.0), p("x"), file(9, 9)).change.unwrap().record.entry;
            b.form_batches(t(3.0), batch_id());
            let mut a = folder();
            a.adopt(t(3.0), base.clone());
            let mut c = FolderState::new(b.id(), Rules::default(), [node(1), node(2), node(3)], node(3), HostName::new("charlie").unwrap());
            c.adopt(t(3.0), base);

            let l = b.scanned(t(4.0), p("x"), observed(Kind::File, l_hash, l_mtime, l_exec)).change.unwrap().record.entry;
            let batch_l = b.form_batches(t(6.0), batch_id().successor(1)).remove(0);
            let w = a.scanned(t(4.0), p("x"), observed(Kind::File, w_hash, w_mtime, w_exec)).change.unwrap().record.entry;
            let batch_w = a.form_batches(t(6.0), batch_id().successor(2)).remove(0);
            prop_assert!(!l.same_content(&w));
            prop_assert_eq!(resolve(&w, &l).winner, Side::First, "W must win for B and C to be L-holders");

            c.receive(&batch_l);
            commit_all(&mut c, t(7.0));
            prop_assert_eq!(&c.index().get(&p("x")).unwrap().entry, &l);

            let sb = b.receive(&batch_w);
            let sc = c.receive(&batch_w);
            let copy_b = sb.items[0].conflict().unwrap().clone();
            let copy_c = sc.items[0].conflict().unwrap().clone();
            prop_assert_eq!(&copy_b.path, &copy_c.path);
            prop_assert!(copy_b.loser.same_content(&copy_c.loser));
            let m = sb.items[0].incoming().clone();
            prop_assert_eq!(&m, sc.items[0].incoming());
            prop_assert_eq!(&m.version, &w.version.merge(&l.version));

            let wb = commit_all(&mut b, t(8.0));
            let wc = commit_all(&mut c, t(8.0));
            prop_assert_eq!(wb.len(), 2);
            prop_assert_eq!(wc.len(), 2);
            prop_assert!(wb[1].entry.same_content(&wc[1].entry));
            prop_assert_eq!(wb[1].entry.modified_by, node(2));
            prop_assert_eq!(wc[1].entry.modified_by, node(3));
            prop_assert_eq!(&wb[1].entry.path, &copy_b.path);

            // Exchange: each announces M and its copy; the copies are concurrent
            // with identical content and merge under §7.2.
            let from_b = b.form_batches(t(10.0), batch_id().successor(3)).remove(0);
            let from_c = c.form_batches(t(10.0), batch_id().successor(4)).remove(0);
            let sc2 = c.receive(&from_b);
            let sb2 = b.receive(&from_c);
            prop_assert_eq!(sc2.ignored, 1, "M is equal on both sides");
            prop_assert_eq!(sb2.ignored, 1);
            prop_assert_eq!(sc2.items.len(), 1);
            prop_assert!(matches!(sc2.items[0].mode(), ApplyMode::IndexOnly | ApplyMode::MetadataOnly));
            commit_all(&mut c, t(11.0));
            commit_all(&mut b, t(11.0));
            prop_assert_eq!(b.index().get(&copy_b.path), c.index().get(&copy_b.path));
            prop_assert_eq!(&b.index().get(&p("x")).unwrap().entry, &m);
            prop_assert_eq!(&c.index().get(&p("x")).unwrap().entry, &m);

            // A, the W-holder, takes M as a metadata-only apply and the copy as an add.
            let sa = a.receive(&from_b);
            prop_assert_eq!(sa.items.len(), 2);
            commit_all(&mut a, t(12.0));
            prop_assert_eq!(&a.index().get(&p("x")).unwrap().entry, &m);
            prop_assert!(a.index().get(&copy_b.path).unwrap().entry.same_content(&copy_b.loser));
            prop_assert_eq!(b.winner_fallbacks() + c.winner_fallbacks() + a.winner_fallbacks(), 0);
        }
    }
}
