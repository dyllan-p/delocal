//! Per-folder state (DESIGN.md §7.3 scan brackets, §7.4 batch window and
//! receiving, §7.5 commit bookkeeping, §8.1 brake and pause, §8.2
//! quarantine, §8.3 revert).
//!
//! One [`FolderState`] per folder this machine is a member of. It owns the
//! folder's [`Index`], the batch window, the open scan bracket, the apply
//! sets accepted but not yet committed, each peer's acknowledgement of this
//! machine's `seq`, the quarantine, and the paused state. `Engine` (in
//! `engine.rs`) drives it and turns what it returns into actions.
//!
//! **Window.** Any index write, local or adopted, opens the window if it is
//! closed and records the time of the last write. The window is due at
//! `min(opened + 10 s, last + 2 s)` (§7.4).
//!
//! **Tick.** When the window is due the sender pre-check runs (§8.1) over
//! the unannounced local changes, with the tracked count as of the last
//! announcement as H1's denominator. Pass: the batch forms and goes out.
//! Hold: nothing leaves, the folder is paused under a reserved batch id, and
//! it stays paused until `approve` or `revert`, whatever later changes do;
//! later ticks re-run the check for `status` only.
//!
//! **Receive.** Items whose incoming version is quarantined, or dominates a
//! quarantined version, join that held item. On a paused folder, items for
//! paths in the pending set are frozen (kept in arrival order) until the
//! folder unpauses. The brake runs over the rest with the current tracked
//! count; `Held` quarantines the raw incoming entries, `Accepted` stores
//! the apply set.
//!
//! **In flight.** A path with an accepted item is in flight: observations
//! of it are ignored and the scan bracket's deletion pass skips it, so a
//! displacement or a revert's trash move is never mistaken for a local
//! deletion (§7.5, §8.3). The path leaves in flight when its item commits
//! or defers.
//!
//! **Scan bracket.** Between `ScanStarted` and `ScanFinished` every path the
//! host reports is marked seen; at `ScanFinished` every live record not seen
//! becomes a tombstone dated then (§7.3). `ScanAborted` drops the bracket
//! and announces nothing.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::batch::{self, ApplyItem, ApplyMode, ApplySet, Batch, Decision, Summary};
use crate::brake::{self, HoldReason, Verdict};
use crate::conflict::ConflictCopy;
use crate::entry::{Entry, Kind, Observed};
use crate::id::{BatchId, FolderId, HostName, NodeId};
use crate::index::{Index, IndexRecord, LocalChange};
use crate::path::RelPath;
use crate::quarantine::{HeldItem, Quarantine};
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

/// Why an incoming entry is waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeferredReason {
    /// Its commit found the file changed underneath; re-evaluated at the
    /// next observation of the path (§7.5 step 6).
    ChangedUnderneath,
    /// Its path is in a paused folder's pending set; re-evaluated when the
    /// folder unpauses (§8.1).
    Frozen,
}

/// An incoming entry waiting to be classified again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deferred {
    /// The entry to re-classify: the incoming entry, or `M` for a conflict.
    pub entry: Entry,
    /// The batch it arrived in, so the re-evaluated item keeps its origin.
    pub batch: BatchId,
    pub source: NodeId,
    pub seq_high: u64,
    pub reason: DeferredReason,
}

/// A paused folder (§8.1): the sender pre-check tripped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    /// Reserved when the folder paused; `approve <id>` sends with it.
    pub batch: BatchId,
    pub since: Timestamp,
    /// Why it paused, and the pending set's counts at the last check.
    pub reason: HoldReason,
    pub summary: Summary,
    /// True if the pending set would pass the pre-check now (`status`
    /// says so); the folder still waits for `approve` or `revert`.
    pub would_pass: bool,
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
    /// The sender pre-check tripped; nothing left (§8.1).
    Paused {
        batch: BatchId,
        reason: HoldReason,
        summary: Summary,
    },
    /// A later tick re-ran the pre-check on a paused folder (§8.1).
    PausedRecheck {
        batch: BatchId,
        would_pass: bool,
        summary: Summary,
    },
    /// A received batch was held (§8.1, §8.2).
    Held {
        batch: BatchId,
        source: NodeId,
        reason: HoldReason,
        summary: Summary,
        paths: usize,
    },
    /// Incoming versions joined an existing held item (§8.2).
    JoinedHeld { batch: BatchId, count: usize },
    /// `approve` released a held item; this many items are being applied.
    Released { batch: BatchId, items: usize },
    /// `approve` sent a paused batch.
    Unpaused { batch: BatchId },
    /// `deny` bumped this many paths over the quarantined versions.
    Denied { batch: BatchId, paths: usize },
    /// `revert` undid the pending changes (§8.3).
    Reverted {
        batch: BatchId,
        trashed: usize,
        refetch: usize,
    },
    /// `revert` on a folder that is not paused: a no-op (§8.3).
    NotPaused,
    /// `approve` or `deny` named a batch that is neither held nor paused.
    UnknownBatch { batch: BatchId },
    /// A rule change was evaluated against a held item or the paused batch;
    /// nothing is released by itself (§8.1).
    RulesRecheck { batch: BatchId, would_pass: bool },
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

/// What a due tick did (§7.4, §8.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ticked {
    /// The window was not due, or there was nothing to send.
    Nothing,
    /// Batches formed and are to be sent.
    Sent(Vec<Batch>),
    /// The pre-check tripped, or re-ran on an already paused folder.
    Paused {
        /// First time: record `snapshot` in history under the reserved id.
        first: bool,
        snapshot: Vec<Batch>,
        status: FolderStatus,
    },
}

/// What receiving a batch produced (§7.4, §8.1, §8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    /// The items that went through the brake (joined and frozen ones removed).
    pub set: ApplySet,
    pub decision: Decision,
    /// Receiver-side counts of `set` (§8.1).
    pub summary: Summary,
    /// Items that joined an existing held item.
    pub joined: usize,
    /// Items frozen because their path is pending on a paused folder.
    pub frozen: usize,
    /// The hold reason, if held.
    pub held: Option<HoldReason>,
}

/// What `approve` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Approved {
    /// A held item was released; its re-classified items, if any.
    Released(Option<ApplySet>),
    /// The paused batch formed and is to be sent.
    Sent(Vec<Batch>),
    /// Neither a held item nor the paused batch.
    Unknown,
}

/// What `revert` did (§8.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertOutcome {
    pub batch: BatchId,
    /// Paths whose current file the host moves to trash.
    pub trash: Vec<RelPath>,
    /// Restored live entries put in flight to be fetched again.
    pub refetch: usize,
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
    /// Entries waiting to be classified again, by path, in arrival order.
    deferred: BTreeMap<RelPath, Vec<Deferred>>,
    /// How many conflicts rule 5 of §7.6 has decided here. Should stay 0.
    winner_fallbacks: u64,
    quarantine: Quarantine,
    paused: Option<Paused>,
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
            quarantine: Quarantine::default(),
            paused: None,
        }
    }

    pub fn id(&self) -> FolderId {
        self.id
    }

    pub fn rules(&self) -> &Rules {
        &self.rules
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

    /// Entries waiting to be classified again, in path then arrival order.
    pub fn deferred(&self) -> impl Iterator<Item = &Deferred> {
        self.deferred.values().flatten()
    }

    /// Conflicts decided by rule 5 of §7.6 so far. Any value above 0 is a
    /// bug the simulator should find.
    pub fn winner_fallbacks(&self) -> u64 {
        self.winner_fallbacks
    }

    /// Held batches (§8.2).
    pub fn quarantine(&self) -> &Quarantine {
        &self.quarantine
    }

    /// The paused state, if the sender pre-check tripped (§8.1).
    pub fn paused(&self) -> Option<&Paused> {
        self.paused.as_ref()
    }

    /// True if an accepted item exists for `path` (§7.5, §8.3).
    pub fn in_flight(&self, path: &RelPath) -> bool {
        self.accepted
            .iter()
            .any(|set| set.items.iter().any(|item| item.path() == path))
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

    /// The host reported `state` at `path` (§7.3), inside or outside a
    /// bracket. Reports for a path in flight are ignored.
    pub fn scanned(&mut self, now: Timestamp, path: RelPath, state: ScanState) -> Scanned {
        if let Some(seen) = &mut self.scan {
            seen.insert(path.clone());
        }
        if self.in_flight(&path) {
            return Scanned::default();
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
        self.reconsider(&path, None);
        Scanned {
            change,
            status: None,
        }
    }

    /// Re-classify the deferred entries at `path` against the index as it
    /// now stands, in arrival order; only those with `reason` if given.
    /// Each becomes a one-item accepted set (an `Apply`, possibly a
    /// conflict's `M`) or is dropped if the index has caught up with it.
    fn reconsider(&mut self, path: &RelPath, reason: Option<DeferredReason>) {
        let Some(list) = self.deferred.remove(path) else {
            return;
        };
        let (take, keep): (Vec<Deferred>, Vec<Deferred>) = list
            .into_iter()
            .partition(|d| reason.is_none_or(|r| d.reason == r));
        if !keep.is_empty() {
            self.deferred.insert(path.clone(), keep);
        }
        for deferred in take {
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
    }

    /// The folder unpaused: every frozen entry is classified again.
    fn unfreeze(&mut self) {
        let paths: Vec<RelPath> = self.deferred.keys().cloned().collect();
        for path in paths {
            self.reconsider(&path, Some(DeferredReason::Frozen));
        }
    }

    /// A full scan begins: start collecting the paths it reports.
    pub fn scan_started(&mut self) {
        self.scan = Some(BTreeSet::new());
    }

    /// A full scan ended: every live record it did not report is gone
    /// (§7.3), except paths in flight. Returns the tombstones, or `Err` if
    /// no bracket was open.
    pub fn scan_finished(&mut self, now: Timestamp) -> Result<Vec<LocalChange>, FolderStatus> {
        let seen = self.scan.take().ok_or(FolderStatus::ScanNotOpen)?;
        let gone: Vec<RelPath> = self
            .index
            .live_records()
            .map(|r| r.entry.path.clone())
            .filter(|p| !seen.contains(p) && !self.in_flight(p))
            .collect();
        let mut changes = Vec::new();
        for path in &gone {
            if let Some(change) = self.index.observe_absent(path, now.as_unix_nanos()) {
                changes.push(change);
            }
            // A deferred entry whose file vanished underneath is only ever
            // caught here: later scans never report a tombstoned path.
            self.reconsider(path, None);
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

    /// The sender pre-check (§8.1): the summary the next batch would carry,
    /// against the tracked count as of the last announcement.
    pub fn precheck(&self) -> (Summary, Verdict) {
        let mut summary = Summary::default();
        for (record, kind) in self.index.unannounced() {
            if let Some(kind) = kind {
                summary.count(&record.entry, kind);
            }
        }
        let verdict = brake::evaluate(&self.rules, &summary, self.index.announced_tracked());
        (summary, verdict)
    }

    /// Form the batch or batches for everything written since the last one
    /// (§7.4) and close the window. `base` is the fresh id for the first;
    /// the rest are its successors. No pre-check: see [`FolderState::tick`].
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

    /// The window is due (§7.4): run the pre-check (§8.1) and form the
    /// batch, or pause. On a paused folder the check runs for `status`
    /// only. `base` is reserved for the paused batch on the first pause.
    pub fn tick(&mut self, now: Timestamp, base: BatchId) -> Ticked {
        if !self.is_due(now) {
            return Ticked::Nothing;
        }
        let (summary, verdict) = self.precheck();
        self.window = None;
        if let Some(paused) = &mut self.paused {
            paused.would_pass = !verdict.is_hold();
            paused.summary = summary;
            return Ticked::Paused {
                first: false,
                snapshot: Vec::new(),
                status: FolderStatus::PausedRecheck {
                    batch: paused.batch,
                    would_pass: paused.would_pass,
                    summary,
                },
            };
        }
        match verdict {
            Verdict::Pass => {
                let batches = self.form_batches(now, base);
                if batches.is_empty() {
                    Ticked::Nothing
                } else {
                    Ticked::Sent(batches)
                }
            }
            Verdict::Hold(reason) => {
                let snapshot = batch::form(
                    base,
                    self.id,
                    self.index.own(),
                    now,
                    &self.index.unannounced(),
                );
                self.paused = Some(Paused {
                    batch: base,
                    since: now,
                    reason,
                    summary,
                    would_pass: false,
                });
                Ticked::Paused {
                    first: true,
                    snapshot,
                    status: FolderStatus::Paused {
                        batch: base,
                        reason,
                        summary,
                    },
                }
            }
        }
    }

    /// A batch arrived (§7.4): apply set, quarantine joins, frozen paths,
    /// the brake, and the decision (§8.1, §8.2).
    pub fn receive(&mut self, now: Timestamp, batch: &Batch) -> Received {
        self.index.set_peer_seq(batch.source, batch.seq_high);
        let mut set = batch::apply_set(&self.index, batch);
        self.winner_fallbacks += set.fallbacks as u64;

        // The raw incoming entry per path, last by seq (§7.4).
        let mut raw: BTreeMap<&RelPath, &Entry> = BTreeMap::new();
        for entry in &batch.entries {
            raw.insert(&entry.path, entry);
        }

        let mut joined = 0;
        let mut frozen = 0;
        let mut kept = Vec::with_capacity(set.items.len());
        for item in std::mem::take(&mut set.items) {
            let Some(incoming) = raw.get(item.path()).copied() else {
                kept.push(item);
                continue;
            };
            if let Some(held) = self.quarantine.matching(incoming) {
                self.quarantine.join(held, incoming.clone());
                joined += 1;
            } else if self.paused.is_some() && self.index.is_pending(item.path()) {
                self.deferred
                    .entry(item.path().clone())
                    .or_default()
                    .push(Deferred {
                        entry: incoming.clone(),
                        batch: batch.id,
                        source: batch.source,
                        seq_high: batch.seq_high,
                        reason: DeferredReason::Frozen,
                    });
                frozen += 1;
            } else {
                kept.push(item);
            }
        }
        set.items = kept;

        let summary = brake::receiver_summary(&self.index, &set);
        match brake::evaluate(&self.rules, &summary, self.index.tracked_count()) {
            Verdict::Hold(reason) => {
                let entries = set
                    .items
                    .iter()
                    .filter_map(|item| {
                        raw.get(item.path())
                            .map(|e| (item.path().clone(), (*e).clone()))
                    })
                    .collect();
                self.quarantine.hold(HeldItem {
                    batch: batch.id,
                    source: batch.source,
                    seq_high: batch.seq_high,
                    held_at: now,
                    reason,
                    entries,
                });
                Received {
                    set,
                    decision: Decision::Held {
                        reason: reason.to_string(),
                    },
                    summary,
                    joined,
                    frozen,
                    held: Some(reason),
                }
            }
            Verdict::Pass => {
                if !set.is_empty() {
                    self.accepted.push(set.clone());
                }
                Received {
                    set,
                    decision: Decision::Accepted,
                    summary,
                    joined,
                    frozen,
                    held: None,
                }
            }
        }
    }

    /// `approve <batch>` (§8.2, §8.1): release a held item, re-classifying
    /// its entries against the index as it stands, or send the paused batch.
    pub fn approve(&mut self, now: Timestamp, batch: BatchId) -> Approved {
        if let Some(item) = self.quarantine.release(batch) {
            let mut items = Vec::new();
            let mut fallbacks = 0;
            for entry in item.entries.values() {
                let classified = batch::classify(&self.index, entry);
                if classified.fallback {
                    self.winner_fallbacks += 1;
                    fallbacks += 1;
                }
                items.extend(classified.item);
            }
            items.sort_by(|a, b| a.path().cmp(b.path()));
            if items.is_empty() {
                return Approved::Released(None);
            }
            let set = ApplySet {
                folder: self.id,
                batch: item.batch,
                source: item.source,
                seq_high: item.seq_high,
                items,
                ignored: 0,
                duplicates: 0,
                fallbacks,
            };
            self.accepted.push(set.clone());
            return Approved::Released(Some(set));
        }
        match &self.paused {
            Some(paused) if paused.batch == batch => {
                let batches = self.form_batches(now, batch);
                self.paused = None;
                self.unfreeze();
                Approved::Sent(batches)
            }
            _ => Approved::Unknown,
        }
    }

    /// `deny <batch>` (§8.2): this machine's copies win. Every quarantined
    /// path gets a local version that dominates every quarantined version
    /// there, content unchanged; the item is dropped. `None` if unknown.
    pub fn deny(&mut self, now: Timestamp, batch: BatchId) -> Option<Vec<LocalChange>> {
        let paths: Vec<RelPath> = self
            .quarantine
            .get(batch)?
            .entries
            .keys()
            .cloned()
            .collect();
        let mut changes = Vec::with_capacity(paths.len());
        for path in &paths {
            let over = self.quarantine.versions_at(path);
            changes.push(self.index.bump_over(path, &over, now.as_unix_nanos()));
        }
        self.quarantine.release(batch);
        if !changes.is_empty() {
            self.touched(now);
        }
        Some(changes)
    }

    /// `revert` (§8.3) on a paused folder: discard the pending changes,
    /// restore every path's announced record, move the current files to
    /// trash, and put the restored live entries in flight to be fetched.
    /// `None` if the folder is not paused.
    pub fn revert(&mut self, now: Timestamp) -> Option<RevertOutcome> {
        let paused = self.paused.take()?;
        let mut trash = Vec::new();
        let mut items = Vec::new();
        for reverted in self.index.revert_pending() {
            if reverted.current.is_some_and(|e| !e.deleted) {
                trash.push(reverted.path.clone());
            }
            if let Some(record) = reverted.restored
                && !record.entry.deleted
            {
                let mode = if record.entry.kind == Kind::Dir {
                    ApplyMode::Direct
                } else {
                    ApplyMode::Fetch
                };
                items.push(ApplyItem::Apply {
                    entry: record.entry,
                    mode,
                    conflict: None,
                });
            }
        }
        let refetch = items.len();
        if !items.is_empty() {
            self.accepted.push(ApplySet {
                folder: self.id,
                batch: paused.batch,
                source: self.index.own(),
                seq_high: 0,
                items,
                ignored: 0,
                duplicates: 0,
                fallbacks: 0,
            });
        }
        self.window = None;
        let _ = now;
        self.unfreeze();
        Some(RevertOutcome {
            batch: paused.batch,
            trash,
            refetch,
        })
    }

    /// New rules (§8.1): re-evaluate every held item and the paused batch
    /// for `status`. Nothing is released by itself.
    pub fn rules_changed(&mut self, rules: Rules) -> Vec<FolderStatus> {
        self.rules = rules;
        let mut out = Vec::new();
        for item in self.quarantine.items() {
            let items: Vec<ApplyItem> = item
                .entries
                .values()
                .filter_map(|e| batch::classify(&self.index, e).item)
                .collect();
            let probe = ApplySet {
                folder: self.id,
                batch: item.batch,
                source: item.source,
                seq_high: item.seq_high,
                items,
                ignored: 0,
                duplicates: 0,
                fallbacks: 0,
            };
            let summary = brake::receiver_summary(&self.index, &probe);
            let verdict = brake::evaluate(&self.rules, &summary, self.index.tracked_count());
            out.push(FolderStatus::RulesRecheck {
                batch: item.batch,
                would_pass: !verdict.is_hold(),
            });
        }
        if self.paused.is_some() {
            let (summary, verdict) = self.precheck();
            if let Some(paused) = &mut self.paused {
                paused.would_pass = !verdict.is_hold();
                paused.summary = summary;
                out.push(FolderStatus::RulesRecheck {
                    batch: paused.batch,
                    would_pass: paused.would_pass,
                });
            }
        }
        out
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
                        reason: DeferredReason::ChangedUnderneath,
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
                self.deferred
                    .entry(path.clone())
                    .or_default()
                    .push(deferred);
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
        let set = b.receive(t(0.0), &batch).set;
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
        let again = b.receive(t(0.0), &batch).set;
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
        let set = b.receive(t(0.0), &batch).set;
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

        let set = b.receive(t(0.0), &batch).set;
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
        let entry = b.receive(t(0.0), &batch).set.items[0].incoming().clone();
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
        b.receive(t(0.0), &batch);
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
        let set = b.receive(t(0.0), &batch).set;
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
        let set = b.receive(t(0.0), &batch).set;
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

    // ---- §8: brake, quarantine, pause, approve, deny, revert ----------------

    fn tight() -> Rules {
        Rules {
            hold_count: 3,
            hold_pct: 25,
            hold_size: 1_000,
            ..Rules::default()
        }
    }

    fn folder_with(rules: Rules, own: u8, host: &str) -> FolderState {
        FolderState::new(
            FolderId::from_bytes([7; 16]),
            rules,
            [node(1), node(2), node(3)],
            node(own),
            HostName::new(host).unwrap(),
        )
    }

    fn bid(i: u8) -> BatchId {
        let mut b = [0xb0u8; 16];
        b[15] = i;
        BatchId::from_bytes(b)
    }

    /// A (node 1) with `n` announced files, and B (node 2, `rules`) holding
    /// the same files after committing A's batch.
    fn a_and_b(n: usize, rules: Rules) -> (FolderState, FolderState) {
        let mut a = folder_with(Rules::default(), 1, "alpha");
        for i in 0..n {
            a.scanned(t(1.0), p(&format!("f{i:02}")), file(1, 1));
        }
        let batch = a.form_batches(t(3.0), bid(1)).remove(0);
        // The base is built under the defaults so a tight H2 in `rules`
        // cannot hold the initial adds; the rules apply afterwards.
        let mut b = folder_with(Rules::default(), 2, "bravo");
        let r = b.receive(t(3.0), &batch);
        assert_eq!(r.decision, Decision::Accepted, "adds never trip H1");
        commit_all(&mut b, t(4.0));
        b.form_batches(t(6.0), bid(2));
        assert!(b.rules_changed(rules).is_empty());
        assert_eq!(b.index().tracked_count(), n);
        assert_eq!(b.index().announced_tracked(), n);
        (a, b)
    }

    #[test]
    fn sender_pre_check_pauses_and_stays_paused_until_approve() {
        let (_, mut b) = a_and_b(10, tight());
        for i in 0..8 {
            b.scanned(t(10.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        let (summary, verdict) = b.precheck();
        assert_eq!(summary.dels, 8);
        assert_eq!(
            verdict,
            Verdict::Hold(HoldReason::Count {
                destructive: 8,
                tracked: 10
            }),
            "denominator is the tracked count at the last announcement, not the current 2"
        );
        match b.tick(t(12.0), bid(5)) {
            Ticked::Paused {
                first,
                snapshot,
                status,
            } => {
                assert!(first);
                assert_eq!(snapshot.len(), 1);
                assert_eq!(snapshot[0].id, bid(5));
                assert_eq!(snapshot[0].summary.dels, 8);
                assert!(matches!(status, FolderStatus::Paused { batch, .. } if batch == bid(5)));
            }
            other => panic!("expected a pause, got {other:?}"),
        }
        assert_eq!(b.paused().unwrap().batch, bid(5));
        assert_eq!(b.due(), None, "the window closed");
        assert_eq!(b.index().pending_count(), 8, "nothing announced");
        assert_eq!(b.index().unannounced().len(), 8);

        // The user puts 7 files back: the set would now pass, but paused
        // means paused.
        for i in 0..7 {
            b.scanned(t(20.0), p(&format!("f{i:02}")), file(1, 1));
        }
        match b.tick(t(22.0), bid(6)) {
            Ticked::Paused {
                first,
                snapshot,
                status,
            } => {
                assert!(!first);
                assert!(snapshot.is_empty());
                assert_eq!(
                    status,
                    FolderStatus::PausedRecheck {
                        batch: bid(5),
                        would_pass: true,
                        summary: b.paused().unwrap().summary,
                    }
                );
            }
            other => panic!("expected a recheck, got {other:?}"),
        }
        assert!(b.paused().unwrap().would_pass);
        assert_eq!(b.paused().unwrap().batch, bid(5), "the reserved id is kept");

        assert_eq!(b.approve(t(30.0), bid(9)), Approved::Unknown);
        match b.approve(t(30.0), bid(5)) {
            Approved::Sent(batches) => {
                assert_eq!(batches[0].id, bid(5), "sent under the reserved id");
                assert_eq!(batches[0].summary.dels, 1);
                assert_eq!(batches[0].summary.mods, 0, "7 restored: touches");
            }
            other => panic!("expected Sent, got {other:?}"),
        }
        assert!(b.paused().is_none());
        assert_eq!(b.index().pending_count(), 0);
    }

    #[test]
    fn receiver_holds_and_quarantines_raw_entries_that_later_versions_join() {
        let (mut a, mut b) = a_and_b(10, tight());
        for i in 0..8 {
            a.scanned(t(10.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        let dels = a.form_batches(t(12.0), bid(3)).remove(0);
        let r = b.receive(t(12.0), &dels);
        assert_eq!(
            r.held,
            Some(HoldReason::Count {
                destructive: 8,
                tracked: 10
            })
        );
        assert!(matches!(r.decision, Decision::Held { .. }));
        assert_eq!(r.summary.dels, 8);
        assert!(b.accepted().is_empty(), "nothing applied");
        assert_eq!(b.index().tracked_count(), 10);
        let held = b.quarantine().get(bid(3)).unwrap();
        assert_eq!(held.entries.len(), 8);
        assert_eq!(held.source, node(1));
        assert!(
            held.entries.values().all(|e| e.deleted),
            "raw entries, as received"
        );
        assert_eq!(
            b.index().peer_seq(node(1)),
            dels.seq_high,
            "held still acknowledges"
        );

        // The same versions relayed by C are still held, and nothing else
        // is left for the brake.
        let mut relayed = dels.clone();
        relayed.id = bid(4);
        relayed.source = node(3);
        let r = b.receive(t(13.0), &relayed);
        assert_eq!(r.joined, 8);
        assert_eq!(r.decision, Decision::Accepted, "the empty remainder passes");
        assert!(r.set.is_empty());
        assert_eq!(b.quarantine().len(), 1);

        // A re-creates one file: it dominates the quarantined tombstone and
        // joins the item, replacing the stored entry.
        a.scanned(t(14.0), p("f00"), file(7, 7));
        let recreate = a.form_batches(t(16.0), bid(5)).remove(0);
        let r = b.receive(t(16.0), &recreate);
        assert_eq!(r.joined, 1);
        assert!(b.accepted().is_empty());
        let stored = &b.quarantine().get(bid(3)).unwrap().entries[&p("f00")];
        assert!(!stored.deleted);
        assert_eq!(stored.hash, hash(7));
        assert_eq!(b.quarantine().versions_at(&p("f00")).len(), 2);

        // Unrelated paths from the same source still sync.
        a.scanned(t(17.0), p("f09"), file(9, 9));
        let other = a.form_batches(t(19.0), bid(6)).remove(0);
        let r = b.receive(t(19.0), &other);
        assert_eq!((r.joined, r.decision), (0, Decision::Accepted));
        assert_eq!(b.accepted().len(), 1);
        commit_all(&mut b, t(20.0));

        // Approve re-classifies the stored entries as they stand now.
        match b.approve(t(21.0), bid(3)) {
            Approved::Released(Some(set)) => {
                assert_eq!(set.batch, bid(3));
                assert_eq!(set.items.len(), 8);
                let f00 = set.items.iter().find(|i| i.path() == &p("f00")).unwrap();
                assert_eq!(
                    f00.mode(),
                    ApplyMode::Fetch,
                    "the re-created file, not the tombstone"
                );
                assert_eq!(f00.incoming().hash, hash(7));
                assert_eq!(
                    set.items
                        .iter()
                        .filter(|i| i.mode() == ApplyMode::Direct)
                        .count(),
                    7
                );
            }
            other => panic!("expected a release, got {other:?}"),
        }
        assert!(b.quarantine().is_empty());
        assert_eq!(b.accepted().len(), 1);
        assert_eq!(b.approve(t(22.0), bid(3)), Approved::Unknown);
    }

    #[test]
    fn deny_bumps_this_machines_copies_over_the_quarantine() {
        let (mut a, mut b) = a_and_b(10, tight());
        for i in 0..8 {
            a.scanned(t(10.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        let dels = a.form_batches(t(12.0), bid(3)).remove(0);
        b.receive(t(12.0), &dels);
        let quarantined: Vec<Version> = b.quarantine().versions_at(&p("f03"));
        assert_eq!(b.deny(t(13.0), bid(9)), None);
        let changes = b.deny(t(13.0), bid(3)).unwrap();
        assert_eq!(changes.len(), 8);
        for c in &changes {
            let e = &c.record.entry;
            assert!(!e.deleted, "B's copies win");
            assert!(e.is_metadata_only(), "content unchanged");
            assert_eq!(e.modified_by, node(2));
            assert_eq!(c.kind, ChangeKind::Modify);
            for q in b.quarantine().versions_at(&e.path) {
                assert!(e.version.dominates(&q));
            }
        }
        let f03 = &b.index().get(&p("f03")).unwrap().entry;
        for q in &quarantined {
            assert!(f03.version.dominates(q));
        }
        assert!(b.quarantine().is_empty());
        assert_eq!(b.index().pending_count(), 8);
        assert!(b.due().is_some(), "the bumps go out in the next batch");
        let out = b.form_batches(t(16.0), bid(4)).remove(0);
        assert_eq!(out.entries.len(), 8);
        assert_eq!(
            out.summary,
            Summary::default(),
            "touches: invisible to the brake"
        );

        // On A the bumps dominate its tombstones: eight adds, which do not
        // count, so A gets its files back.
        let r = a.receive(t(16.0), &out);
        assert_eq!(r.decision, Decision::Accepted);
        assert_eq!(r.summary.adds, 8);
        assert!(r.set.items.iter().all(|i| i.mode() == ApplyMode::Fetch));
    }

    #[test]
    fn revert_restores_announced_records_trashes_files_and_refetches() {
        let (_, mut b) = a_and_b(10, tight());
        let announced: Vec<IndexRecord> = b.index().records().cloned().collect();
        for i in 0..8 {
            b.scanned(t(10.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        b.scanned(t(10.0), p("f08"), file(5, 5)); // modified
        b.scanned(t(10.0), p("junk"), file(6, 6)); // new
        assert_eq!(b.revert(t(11.0)), None, "not paused yet");
        assert!(matches!(
            b.tick(t(12.0), bid(5)),
            Ticked::Paused { first: true, .. }
        ));
        let out = b.revert(t(13.0)).unwrap();
        assert_eq!(out.batch, bid(5));
        assert_eq!(
            out.trash,
            vec![p("f08"), p("junk")],
            "deleted paths have nothing to trash"
        );
        assert_eq!(
            out.refetch, 9,
            "eight deleted and one modified are fetched again"
        );
        assert!(b.paused().is_none());
        assert_eq!(b.index().pending_count(), 0);
        assert_eq!(b.index().get(&p("junk")), None);
        let now: Vec<IndexRecord> = b.index().records().cloned().collect();
        assert_eq!(now, announced, "every record back exactly, seq included");
        assert!(b.index().unannounced().is_empty(), "nothing to re-announce");
        assert_eq!(b.accepted().len(), 1);
        assert!(
            b.accepted()[0]
                .items
                .iter()
                .all(|i| i.mode() == ApplyMode::Fetch && i.conflict().is_none())
        );
        assert!(b.in_flight(&p("f03")));

        // In flight: the trash move is not a deletion, and a full scan that
        // does not see the files does not tombstone them.
        assert_eq!(
            b.scanned(t(14.0), p("f03"), ScanState::Absent),
            Scanned::default()
        );
        b.scan_started();
        b.scanned(t(15.0), p("f09"), ScanState::Unchanged);
        assert!(b.scan_finished(t(16.0)).unwrap().is_empty());
        assert_eq!(b.index().tracked_count(), 10);
        // Once the host re-fetches, the path leaves in flight.
        commit_all(&mut b, t(17.0));
        assert!(!b.in_flight(&p("f03")));
        assert_eq!(b.revert(t(18.0)), None);
    }

    #[test]
    fn frozen_paths_keep_every_incoming_version_until_unpause() {
        let (mut a, mut b) = a_and_b(10, tight());
        for i in 0..8 {
            b.scanned(t(10.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        assert!(matches!(b.tick(t(12.0), bid(5)), Ticked::Paused { .. }));
        // A edits f00 twice and adds z while B is paused.
        a.scanned(t(13.0), p("f00"), file(3, 3));
        a.scanned(t(13.0), p("z"), file(4, 4));
        let first = a.form_batches(t(15.0), bid(6)).remove(0);
        a.scanned(t(16.0), p("f00"), file(5, 5));
        let second = a.form_batches(t(18.0), bid(7)).remove(0);
        let r = b.receive(t(15.0), &first);
        assert_eq!(
            (r.frozen, r.set.items.len()),
            (1, 1),
            "f00 frozen, z applied"
        );
        assert_eq!(r.decision, Decision::Accepted);
        let r = b.receive(t(18.0), &second);
        assert_eq!(r.frozen, 1);
        let frozen: Vec<&Deferred> = b.deferred().collect();
        assert_eq!(frozen.len(), 2, "both kept, in arrival order");
        assert_eq!(frozen[0].entry.hash, hash(3));
        assert_eq!(frozen[1].entry.hash, hash(5));
        assert!(frozen.iter().all(|d| d.reason == DeferredReason::Frozen));
        assert_eq!(
            b.index().peer_seq(node(1)),
            second.seq_high,
            "acknowledged all the same"
        );

        assert!(matches!(b.approve(t(20.0), bid(5)), Approved::Sent(_)));
        assert!(b.deferred().next().is_none());
        // Both versions were classified against B's tombstone: the first is
        // a conflict (delete vs modify) resolving to A's content, the second
        // dominates whatever the first produced and is classified too.
        let f00_items: Vec<&ApplyItem> = b
            .accepted()
            .iter()
            .flat_map(|s| s.items.iter())
            .filter(|i| i.path() == &p("f00"))
            .collect();
        assert_eq!(f00_items.len(), 2);
        assert!(f00_items.iter().all(|i| !i.incoming().deleted));
    }

    #[test]
    fn rule_changes_report_and_release_nothing() {
        let (mut a, mut b) = a_and_b(10, tight());
        for i in 0..8 {
            a.scanned(t(10.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        let dels = a.form_batches(t(12.0), bid(3)).remove(0);
        b.receive(t(12.0), &dels);
        for i in 0..8 {
            b.scanned(t(13.0), p(&format!("f{i:02}")), file(2, 2));
        }
        assert!(matches!(b.tick(t(15.0), bid(8)), Ticked::Paused { .. }));
        let loose = Rules {
            hold_count: 0,
            ..Rules::default()
        };
        let statuses = b.rules_changed(loose.clone());
        assert_eq!(
            statuses,
            [
                FolderStatus::RulesRecheck {
                    batch: bid(3),
                    would_pass: true
                },
                FolderStatus::RulesRecheck {
                    batch: bid(8),
                    would_pass: true
                },
            ]
        );
        assert_eq!(b.rules(), &loose);
        assert_eq!(b.quarantine().len(), 1, "still held");
        assert!(b.paused().unwrap().would_pass);
        assert!(b.paused().is_some(), "still paused");
        assert!(b.accepted().is_empty());
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

            c.receive(t(0.0), &batch_l);
            commit_all(&mut c, t(7.0));
            prop_assert_eq!(&c.index().get(&p("x")).unwrap().entry, &l);

            let sb = b.receive(t(0.0), &batch_w).set;
            let sc = c.receive(t(0.0), &batch_w).set;
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
            let sc2 = c.receive(t(0.0), &from_b).set;
            let sb2 = b.receive(t(0.0), &from_c).set;
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
            let sa = a.receive(t(0.0), &from_b).set;
            prop_assert_eq!(sa.items.len(), 2);
            commit_all(&mut a, t(12.0));
            prop_assert_eq!(&a.index().get(&p("x")).unwrap().entry, &m);
            prop_assert!(a.index().get(&copy_b.path).unwrap().entry.same_content(&copy_b.loser));
            prop_assert_eq!(b.winner_fallbacks() + c.winner_fallbacks() + a.winner_fallbacks(), 0);
        }

        /// §8.1 and §8.2: a batch is Held or Accepted exactly as the brake
        /// says, identically on two machines with the same state; a held
        /// batch is never applied without Approve, and its raw entries are
        /// all quarantined; Deny dominates every quarantined version.
        #[test]
        fn brake_holds_deterministically_and_deny_dominates(
            n in 4usize..12,
            steps in prop::collection::vec((0u8..12, 0u8..3, any::<bool>()), 1..14),
            hold_count in 1u64..6,
            hold_pct in 0u8..60,
            hold_size in 0u64..200,
        ) {
            let rules = Rules { hold_count, hold_pct, hold_size, ..Rules::default() };
            let (mut a, mut b) = a_and_b(n, rules.clone());
            for (i, h, gone) in steps {
                let path = p(&format!("f{:02}", i as usize % n));
                if gone {
                    a.scanned(t(10.0), path, ScanState::Absent);
                } else {
                    a.scanned(t(10.0), path, file(h + 1, 10));
                }
            }
            let batches = a.form_batches(t(12.0), bid(3));
            prop_assume!(!batches.is_empty());
            let batch = &batches[0];
            let before = b.clone();
            let expected_set = crate::batch::apply_set(before.index(), batch);
            let expected = brake::evaluate(&rules, &brake::receiver_summary(before.index(), &expected_set), before.index().tracked_count());

            let r = b.receive(t(12.0), batch);
            let mut twin = before.clone();
            let r2 = twin.receive(t(12.0), batch);
            prop_assert_eq!(&r, &r2);
            prop_assert_eq!(postcard::to_stdvec(&b).unwrap(), postcard::to_stdvec(&twin).unwrap());

            match expected {
                Verdict::Hold(reason) => {
                    prop_assert_eq!(r.held, Some(reason));
                    prop_assert!(b.accepted().is_empty(), "held: nothing applied");
                    let item = b.quarantine().get(batch.id).unwrap();
                    prop_assert_eq!(item.entries.len(), expected_set.items.len());
                    for e in item.entries.values() {
                        prop_assert!(batch.entries.contains(e), "raw entries, as received");
                        prop_assert!(b.quarantine().versions_at(&e.path).contains(&e.version));
                    }
                    // Deny: every bump dominates every quarantined version at its path.
                    let mut denier = b.clone();
                    let changes = denier.deny(t(13.0), batch.id).unwrap();
                    prop_assert_eq!(changes.len(), item.entries.len());
                    for c in &changes {
                        for q in b.quarantine().versions_at(&c.record.entry.path) {
                            prop_assert!(c.record.entry.version.dominates(&q));
                        }
                        prop_assert!(c.record.entry.is_metadata_only());
                        prop_assert_eq!(c.record.entry.modified_by, node(2));
                        let local = before.index().get(&c.record.entry.path).map(|r| &r.entry);
                        match local {
                            Some(l) => prop_assert!(c.record.entry.same_content(l)),
                            None => prop_assert!(c.record.entry.deleted),
                        }
                    }
                    prop_assert!(denier.quarantine().is_empty());
                    // Approve: the re-classified items are applied, nothing before.
                    match b.approve(t(13.0), batch.id) {
                        Approved::Released(set) => {
                            prop_assert_eq!(set.as_ref().map_or(0, |s| s.items.len()), expected_set.items.len());
                            prop_assert_eq!(b.accepted().len(), usize::from(!expected_set.items.is_empty()));
                        }
                        other => prop_assert!(false, "expected a release, got {other:?}"),
                    }
                    prop_assert!(b.quarantine().is_empty());
                }
                Verdict::Pass => {
                    prop_assert_eq!(r.held, None);
                    prop_assert_eq!(r.decision, Decision::Accepted);
                    prop_assert!(b.quarantine().is_empty());
                    prop_assert_eq!(b.accepted().len(), usize::from(!expected_set.items.is_empty()));
                }
            }
        }

        /// §8.3: after revert on a paused folder no path has an unannounced
        /// change, every record is back as announced (seq included) or gone
        /// if peers never saw it, one trash move per live current file, and
        /// every restored live entry is in flight.
        #[test]
        fn revert_leaves_nothing_pending(
            n in 2usize..8,
            steps in prop::collection::vec((0u8..10, 0u8..3, any::<bool>()), 1..12),
        ) {
            // hold_count 1 and 0 %: any del or content mod pauses.
            let rules = Rules { hold_count: 1, hold_pct: 0, ..Rules::default() };
            let (_, mut b) = a_and_b(n, rules);
            let announced: BTreeMap<RelPath, IndexRecord> =
                b.index().records().map(|r| (r.entry.path.clone(), r.clone())).collect();
            for (i, h, gone) in steps {
                let path = p(&format!("f{:02}", i as usize));
                if gone {
                    b.scanned(t(10.0), path, ScanState::Absent);
                } else {
                    b.scanned(t(10.0), path, file(h + 1, 10));
                }
            }
            let (summary, _) = b.precheck();
            prop_assume!(summary.destructive() >= 1);
            let paused = matches!(b.tick(t(12.0), bid(5)), Ticked::Paused { first: true, .. });
            prop_assert!(paused);
            let live_now: BTreeSet<RelPath> = b.index().pending_paths().filter(|p| b.index().live(p).is_some()).cloned().collect();
            let pending: Vec<RelPath> = b.index().pending_paths().cloned().collect();
            let out = b.revert(t(13.0)).unwrap();
            prop_assert_eq!(out.trash.iter().cloned().collect::<BTreeSet<_>>(), live_now);
            prop_assert_eq!(b.index().pending_count(), 0);
            prop_assert!(b.paused().is_none());
            prop_assert!(b.index().unannounced().is_empty());
            let mut refetch = 0;
            for path in &pending {
                match announced.get(path) {
                    Some(record) => {
                        prop_assert_eq!(b.index().get(path), Some(record));
                        if !record.entry.deleted {
                            refetch += 1;
                            prop_assert!(b.in_flight(path));
                        }
                    }
                    None => prop_assert!(b.index().get(path).is_none()),
                }
            }
            prop_assert_eq!(out.refetch, refetch);
            let all: BTreeMap<RelPath, IndexRecord> =
                b.index().records().map(|r| (r.entry.path.clone(), r.clone())).collect();
            prop_assert_eq!(all, announced);
        }
    }
}
