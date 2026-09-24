//! Per-folder state (DESIGN.md §7.3 scan brackets, §7.4 batch window,
//! receiving and catch-up, §7.5 want-list and commit bookkeeping, §8.1
//! brake and pause, §8.2 quarantine, §8.3 revert).
//!
//! One [`FolderState`] per folder this machine is a member of. It owns the
//! folder's [`Index`], the batch window, the open scan bracket, the
//! want-list, each peer's acknowledgement of this machine's `seq`, the
//! quarantine, and the paused state. `Engine` (in `engine.rs`) drives it
//! and turns what it returns into actions.
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
//! **Catch-up** (§7.4). A peer's `have_up_to` replaces our memory of its
//! acknowledgement and marks it for catch-up; at the next tick every
//! announced record above that `seq` goes to it through the same `form()`
//! as a live batch. Unannounced records wait for the live path and its
//! pre-check.
//!
//! **Receive.** Items whose incoming version is quarantined, or dominates a
//! quarantined version, join that held item. On a paused folder, items for
//! paths in the pending set are frozen (kept in arrival order) until the
//! folder unpauses. The brake runs over the rest with the current tracked
//! count; `Held` quarantines the raw incoming entries, `Accepted` puts the
//! items in the want-list (§7.5).
//!
//! **Want-list and in flight** (§7.5). See [`crate::want`]. A path whose
//! want is in a short-lived state is in flight: observations of it are
//! ignored and the scan bracket's deletion pass skips it. A `revert`-made
//! want ignores `Absent` in every state (the trash move, §8.3). A local
//! change at an observable wanted path re-classifies the want.
//!
//! **Scan bracket.** Between `ScanStarted` and `ScanFinished` every path the
//! host reports is marked seen; at `ScanFinished` every live record not seen
//! becomes a tombstone dated then (§7.3). `ScanAborted` drops the bracket
//! and announces nothing.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::batch::{self, ApplyItem, ApplyMode, ApplySet, Batch, Decision, Summary};
use crate::brake::{self, HoldReason, Verdict};
use crate::entry::{Entry, Kind, Observed};
use crate::id::{BatchId, FolderId, HostName, NodeId};
use crate::index::{Index, IndexRecord, LocalChange, Reverted};
use crate::path::RelPath;
use crate::quarantine::{HeldItem, Quarantine};
use crate::rules::Rules;
use crate::time::{DEBOUNCE_NANOS, Timestamp, WINDOW_NANOS};
use crate::version::Version;
use crate::want::{FetchReport, Tier, Want, WantList, WantState, WantStep};

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
    /// The local file was not what the index said, or the displacement
    /// target appeared. Nothing was written. The engine keeps the entry and
    /// re-evaluates it after the next observation of the path (§7.5).
    ChangedUnderneath,
}

/// Why an incoming entry is waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeferredReason {
    /// Its commit found the file changed underneath, or it is concurrent
    /// with a version already wanted at its path; re-evaluated when the
    /// path is observed or its want ends (§7.5 step 6).
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

/// What the index believes is at a path when a commit is ordered (§7.5
/// step 6). `None` means absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Expected {
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i64,
}

/// Where a commit moves the file it displaces (§7.5 step 7).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Displace {
    /// To `.delocal/trash/` (§8.4).
    Trash,
    /// To the conflict-copy path: the displaced file is the losing content (§7.6).
    ConflictCopy(RelPath),
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
    /// A want gave up after two hash mismatches (§7.5 step 4).
    GaveUp { path: RelPath },
    /// A reverted path's content exists on no member: every member answered
    /// `NotAvailable`, so the deletion stands and the record is a tombstone
    /// again (§8.3 step 4).
    Unrecoverable { path: RelPath },
    /// A fetch or commit deadline passed; the want is wanted again (§7.5).
    Stalled { path: RelPath },
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
    /// Items whose version this machine already wants (or has superseded in
    /// its want-list); they only named another source (§7.5).
    pub already_wanted: usize,
    /// The hold reason, if held.
    pub held: Option<HoldReason>,
}

/// What a fetch report did (§7.5, §8.3 step 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// The want's state after the report, if one matched and remains.
    pub state: Option<WantState>,
    /// The tombstone written because a restoring want ran out of members
    /// to ask; announced like any local change.
    pub unrecoverable: Option<LocalChange>,
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
    /// Every index write the revert made, in index order: the record put
    /// back (`restored`) or removed. The persistence hooks come from here;
    /// §11 says every write the engine makes is reported.
    pub reverted: Vec<Reverted>,
    /// Paths whose current file the host moves to trash.
    pub trash: Vec<RelPath>,
    /// Restored live entries wanted again.
    pub refetch: usize,
}

/// One thing the host is asked to do for the want-list (§7.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostStep {
    Fetch {
        path: RelPath,
        version: Version,
        hash: crate::entry::ContentHash,
        size: u64,
        from: NodeId,
    },
    Write {
        path: RelPath,
        entry: Entry,
        expected: Option<Expected>,
        displace: Displace,
    },
    /// Delete as one operation: check `expected`, move the file aside,
    /// report. `displace` says where it goes: the trash, or the conflict-copy
    /// path when the removed file is the losing content of a conflict a
    /// tombstone won (§7.6).
    Remove {
        path: RelPath,
        expected: Option<Expected>,
        displace: Displace,
    },
    SetMeta {
        path: RelPath,
        mtime_ns: i64,
        exec: bool,
    },
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
    wants: WantList,
    /// This machine's `seq` as acknowledged by each peer's decisions or its
    /// `have_up_to` (§7.4).
    acked: BTreeMap<NodeId, u64>,
    /// Peers whose `have_up_to` arrived and await catch-up at the next tick.
    catchup: BTreeSet<NodeId>,
    /// Entries waiting to be classified again, by path, in arrival order.
    deferred: BTreeMap<RelPath, Vec<Deferred>>,
    /// How many conflicts rule 5 of §7.6 has decided here. Should stay 0.
    winner_fallbacks: u64,
    quarantine: Quarantine,
    paused: Option<Paused>,
    /// Statuses raised outside a direct call's return value (a hold made
    /// while admitting re-classified entries); the engine drains them.
    #[serde(skip)]
    statuses: Vec<FolderStatus>,
}

/// A re-classified item on its way into the want-list, with the batch it
/// came from.
struct Candidate {
    item: ApplyItem,
    /// The entry as it arrived, for the quarantine (§8.2 holds versions as
    /// received, never a conflict's `M`).
    raw: Entry,
    batch: BatchId,
    source: NodeId,
    seq_high: u64,
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
            wants: WantList::default(),
            acked: BTreeMap::new(),
            catchup: BTreeSet::new(),
            deferred: BTreeMap::new(),
            winner_fallbacks: 0,
            quarantine: Quarantine::default(),
            paused: None,
            statuses: Vec::new(),
        }
    }

    /// Statuses raised since the last call (see `statuses`).
    pub fn take_statuses(&mut self) -> Vec<FolderStatus> {
        std::mem::take(&mut self.statuses)
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

    /// True if `node` is a member of this folder.
    pub fn is_member(&self, node: NodeId) -> bool {
        self.members.contains(&node)
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    /// The open batch window, if any.
    pub fn window(&self) -> Option<Window> {
        self.window
    }

    /// When the engine next needs a tick: the window, or the earliest want
    /// deadline, whichever is first.
    pub fn due(&self) -> Option<Timestamp> {
        match (self.window.map(|w| w.due()), self.wants.next_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// True if the window is open and due at or before `now`.
    pub fn is_due(&self, now: Timestamp) -> bool {
        self.window.is_some_and(|w| w.due() <= now)
    }

    /// True if a tick is wanted right away (a peer awaits catch-up).
    pub fn wake_now(&self) -> bool {
        !self.catchup.is_empty()
    }

    /// True while a scan bracket is open.
    pub fn scan_open(&self) -> bool {
        self.scan.is_some()
    }

    /// The want-list (§7.5).
    pub fn wants(&self) -> &WantList {
        &self.wants
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

    /// True if the path's want is in a short-lived state (§7.5).
    pub fn in_flight(&self, path: &RelPath) -> bool {
        self.wants.in_flight(path)
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

    /// What the index believes is at `path`, for a commit's check (§7.5 step 6).
    fn expected(&self, path: &RelPath) -> Option<Expected> {
        self.index.live(path).map(|r| Expected {
            kind: r.entry.kind,
            size: r.entry.size,
            mtime_ns: r.entry.mtime_ns,
        })
    }

    /// The host reported `state` at `path` (§7.3), inside or outside a
    /// bracket. Reports for a path in flight are ignored; so is `Absent`
    /// for a `revert`-made want (§8.3). A change at an observable wanted
    /// path re-classifies the want.
    pub fn scanned(&mut self, now: Timestamp, path: RelPath, state: ScanState) -> Scanned {
        if let Some(seen) = &mut self.scan {
            seen.insert(path.clone());
        }
        if self.wants.in_flight(&path) {
            return Scanned::default();
        }
        if state == ScanState::Absent && self.wants.restoring(&path) {
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
            self.reclassify_want(now, &path);
        }
        self.reconsider(now, &path, DeferredReason::ChangedUnderneath);
        Scanned {
            change,
            status: None,
        }
    }

    /// The index changed under an observable want: classify the wanted
    /// entry against the new record. A dominating result replaces the want
    /// (keeping its sources when the version is unchanged); anything else
    /// ends it.
    fn reclassify_want(&mut self, now: Timestamp, path: &RelPath) {
        let Some(old) = self.wants.remove(path) else {
            return;
        };
        let classified = batch::classify(&self.index, &old.entry);
        if classified.fallback {
            self.winner_fallbacks += 1;
        }
        if let Some(item) = classified.item {
            let same = item.incoming().version == old.entry.version;
            // A want keeps only what it is to end up with; for a conflict
            // that is `M`, the nearest thing to the entry as received.
            let raw = old.entry.clone();
            self.admit(
                now,
                vec![Candidate {
                    item,
                    raw,
                    batch: old.batch,
                    source: old.source,
                    seq_high: old.seq_high,
                }],
                true,
            );
            if same {
                for src in old.sources {
                    self.wants.note_announced(path, &old.entry.hash, src);
                }
            }
        }
    }

    /// Admit re-classified items the way `receive` admits new ones (§7.4,
    /// §8.1, §8.2): an item whose version this machine already wants only
    /// names a source; one whose version is quarantined, or dominates a
    /// quarantined version, joins that held item; one whose own batch is
    /// still under review joins it too, since a batch that tripped the
    /// brake is applied only by approve, however its entries arrive at the
    /// want-list; the rest go through the brake as one apply set per
    /// originating batch, and are held under that batch's id or wanted.
    /// `brake` is false for an explicit `approve`, which is the release
    /// itself.
    fn admit(&mut self, now: Timestamp, candidates: Vec<Candidate>, brake: bool) {
        let mut groups: BTreeMap<(BatchId, NodeId, u64), Vec<(ApplyItem, Entry)>> = BTreeMap::new();
        let mut joined: BTreeMap<BatchId, usize> = BTreeMap::new();
        for c in candidates {
            let entry = c.item.incoming();
            if let Some(want) = self.wants.get(c.item.path())
                && (entry.version == *want.version() || want.version().dominates(&entry.version))
            {
                self.wants
                    .note_announced(c.item.path(), &entry.hash, c.source);
                continue;
            }
            if let Some(held) = self.quarantine.matching(&c.raw) {
                self.quarantine.join(held, c.raw);
                *joined.entry(held).or_insert(0) += 1;
                continue;
            }
            if brake && self.quarantine.get(c.batch).is_some() {
                self.quarantine.join(c.batch, c.raw);
                *joined.entry(c.batch).or_insert(0) += 1;
                continue;
            }
            groups
                .entry((c.batch, c.source, c.seq_high))
                .or_default()
                .push((c.item, c.raw));
        }
        for (batch, count) in joined {
            self.statuses
                .push(FolderStatus::JoinedHeld { batch, count });
        }
        for ((batch, source, seq_high), mut pairs) in groups {
            pairs.sort_by(|a, b| a.0.path().cmp(b.0.path()));
            let mut raws: BTreeMap<RelPath, Entry> = BTreeMap::new();
            let mut items = Vec::with_capacity(pairs.len());
            for (item, raw) in pairs {
                raws.insert(item.path().clone(), raw);
                items.push(item);
            }
            let set = ApplySet {
                folder: self.id,
                batch,
                source,
                seq_high,
                items,
                ignored: 0,
                duplicates: 0,
                fallbacks: 0,
            };
            let summary = brake::receiver_summary(&self.index, &set);
            let verdict = if brake {
                brake::evaluate(&self.rules, &summary, self.index.tracked_count())
            } else {
                Verdict::Pass
            };
            match verdict {
                Verdict::Hold(reason) => {
                    let paths = set.items.len();
                    if self.quarantine.get(batch).is_some() {
                        for (_, raw) in raws {
                            self.quarantine.join(batch, raw);
                        }
                        self.statuses.push(FolderStatus::JoinedHeld {
                            batch,
                            count: paths,
                        });
                    } else {
                        self.quarantine.hold(HeldItem {
                            batch,
                            source,
                            seq_high,
                            held_at: now,
                            reason,
                            entries: raws,
                        });
                        self.statuses.push(FolderStatus::Held {
                            batch,
                            source,
                            reason,
                            summary,
                            paths,
                        });
                    }
                }
                Verdict::Pass => {
                    for item in set.items {
                        self.want(item, batch, source, seq_high);
                    }
                }
            }
        }
    }

    /// Re-classify the deferred entries at `path` that wait for `reason`,
    /// against the index as it now stands, in arrival order. Each becomes
    /// a want (an `Apply`, possibly a conflict's `M`) or is dropped if the
    /// index has caught up with it. Observations and commits re-evaluate
    /// `ChangedUnderneath` entries only: a `Frozen` entry waits for the
    /// folder to unpause (§8.1), however often its path is scanned meanwhile.
    fn reconsider(&mut self, now: Timestamp, path: &RelPath, reason: DeferredReason) {
        let Some(list) = self.deferred.remove(path) else {
            return;
        };
        let (take, keep): (Vec<Deferred>, Vec<Deferred>) =
            list.into_iter().partition(|d| d.reason == reason);
        if !keep.is_empty() {
            self.deferred.insert(path.clone(), keep);
        }
        let candidates = self.classify_deferred(take);
        self.admit(now, candidates, true);
    }

    /// Classify deferred entries against the index as it now stands, in
    /// arrival order, dropping those the index has caught up with.
    fn classify_deferred(&mut self, deferred: Vec<Deferred>) -> Vec<Candidate> {
        let mut out = Vec::new();
        for d in deferred {
            let classified = batch::classify(&self.index, &d.entry);
            if classified.fallback {
                self.winner_fallbacks += 1;
            }
            if let Some(item) = classified.item {
                out.push(Candidate {
                    item,
                    raw: d.entry,
                    batch: d.batch,
                    source: d.source,
                    seq_high: d.seq_high,
                });
            }
        }
        out
    }

    /// Put an accepted item in the want-list, or defer it if it is
    /// concurrent with or older than the version already wanted there.
    fn want(&mut self, item: ApplyItem, batch: BatchId, source: NodeId, seq_high: u64) {
        if let Some(item) = self.wants.insert(item, batch, source, seq_high) {
            self.deferred
                .entry(item.path().clone())
                .or_default()
                .push(Deferred {
                    entry: item.incoming().clone(),
                    batch,
                    source,
                    seq_high,
                    reason: DeferredReason::ChangedUnderneath,
                });
        }
    }

    /// The folder unpaused: every frozen entry is classified again and
    /// admitted together, so a mass change that arrived while paused meets
    /// the brake as the batch it was (§8.1).
    fn unfreeze(&mut self, now: Timestamp) {
        let paths: Vec<RelPath> = self.deferred.keys().cloned().collect();
        let mut frozen = Vec::new();
        for path in paths {
            let Some(list) = self.deferred.remove(&path) else {
                continue;
            };
            let (take, keep): (Vec<Deferred>, Vec<Deferred>) = list
                .into_iter()
                .partition(|d| d.reason == DeferredReason::Frozen);
            if !keep.is_empty() {
                self.deferred.insert(path, keep);
            }
            frozen.extend(take);
        }
        let candidates = self.classify_deferred(frozen);
        self.admit(now, candidates, true);
    }

    /// A full scan begins: start collecting the paths it reports.
    pub fn scan_started(&mut self) {
        self.scan = Some(BTreeSet::new());
    }

    /// A full scan ended: every live record it did not report is gone
    /// (§7.3), except paths in flight and paths being restored by `revert`.
    /// Returns the tombstones, or `Err` if no bracket was open.
    pub fn scan_finished(&mut self, now: Timestamp) -> Result<Vec<LocalChange>, FolderStatus> {
        let seen = self.scan.take().ok_or(FolderStatus::ScanNotOpen)?;
        let gone: Vec<RelPath> = self
            .index
            .live_records()
            .map(|r| r.entry.path.clone())
            .filter(|p| !seen.contains(p) && !self.wants.in_flight(p) && !self.wants.restoring(p))
            .collect();
        let mut changes = Vec::new();
        for path in &gone {
            if let Some(change) = self.index.observe_absent(path, now.as_unix_nanos()) {
                changes.push(change);
                self.reclassify_want(now, path);
            }
            // A deferred entry whose file vanished underneath is only ever
            // caught here: later scans never report a tombstoned path.
            self.reconsider(now, path, DeferredReason::ChangedUnderneath);
        }
        // A deferred entry at a path with no record and no file is in the
        // same position: nothing will ever report that path, and the bracket
        // has just observed it absent (§7.3).
        let unseen: Vec<RelPath> = self
            .deferred
            .keys()
            .filter(|p| {
                !seen.contains(*p) && self.index.live(p).is_none() && !self.wants.in_flight(p)
            })
            .cloned()
            .collect();
        for path in &unseen {
            self.reconsider(now, path, DeferredReason::ChangedUnderneath);
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
            self.index.announced_seq(),
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
                    self.index.announced_seq(),
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

    /// Deadlines that have passed return their wants to *wanted* (§7.5).
    pub fn expire(&mut self, now: Timestamp) -> Vec<RelPath> {
        self.wants.expire(now)
    }

    /// A peer told us the highest `seq` of ours it holds (§7.4). It replaces
    /// our memory of its acks and marks it for catch-up at the next tick.
    pub fn have_up_to(&mut self, peer: NodeId, seq: u64) {
        self.acked.insert(peer, seq);
        self.catchup.insert(peer);
    }

    /// Catch-up batches for every peer marked by `have_up_to` (§7.4):
    /// announced records above its `seq`, in `seq` order, through `form()`.
    /// Ids are `base` and its successors; returns how many were used.
    pub fn catchup_batches(
        &mut self,
        now: Timestamp,
        base: BatchId,
    ) -> (Vec<(NodeId, Vec<Batch>)>, u128) {
        let peers = std::mem::take(&mut self.catchup);
        let mut out = Vec::new();
        let mut used: u128 = 0;
        for peer in peers {
            let after = self.acked_by(peer);
            let records: Vec<(&IndexRecord, Option<crate::index::ChangeKind>)> = self
                .index
                .announced_since(after)
                .into_iter()
                .map(|r| (r, None))
                .collect();
            // The peer holds everything up to `after`, so the chain resumes
            // there and the receiver sees no gap.
            let batches = batch::form(
                base.successor(used),
                self.id,
                self.index.own(),
                now,
                &records,
                after,
            );
            used += batches.len() as u128;
            if !batches.is_empty() {
                out.push((peer, batches));
            }
        }
        (out, used)
    }

    /// A batch arrived (§7.4): apply set, quarantine joins, frozen paths,
    /// the brake, and the decision (§8.1, §8.2). Accepted items join the
    /// want-list; every entry the batch carries names its source as a
    /// holder of that version (§7.5).
    pub fn receive(&mut self, now: Timestamp, batch: &Batch) -> Received {
        self.index
            .received_range(batch.source, batch.seq_low, batch.seq_high);
        let mut set = batch::apply_set(&self.index, batch);
        self.winner_fallbacks += set.fallbacks as u64;

        // The raw incoming entry per path, last by seq (§7.4).
        let mut raw: BTreeMap<&RelPath, &Entry> = BTreeMap::new();
        for entry in &batch.entries {
            raw.insert(&entry.path, entry);
        }
        for (path, entry) in &raw {
            self.wants.note_announced(path, &entry.hash, batch.source);
        }

        let mut joined = 0;
        let mut frozen = 0;
        let mut already_wanted = 0;
        let mut kept = Vec::with_capacity(set.items.len());
        for item in std::mem::take(&mut set.items) {
            let Some(incoming) = raw.get(item.path()).copied() else {
                kept.push(item);
                continue;
            };
            // A version this machine already accepted went through the brake
            // when it was accepted; another member offering it (or something
            // older) only adds a source, and must not be held or quarantined
            // while its fetch is under way (§7.5).
            if let Some(want) = self.wants.get(item.path())
                && (item.incoming().version == *want.version()
                    || want.version().dominates(&item.incoming().version))
            {
                already_wanted += 1;
                continue;
            }
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
                    already_wanted,
                    held: Some(reason),
                }
            }
            Verdict::Pass => {
                for item in set.items.clone() {
                    self.want(item, batch.id, batch.source, batch.seq_high);
                }
                Received {
                    set,
                    decision: Decision::Accepted,
                    summary,
                    joined,
                    frozen,
                    already_wanted,
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
            let candidates = set
                .items
                .iter()
                .cloned()
                .map(|it| Candidate {
                    raw: item
                        .entries
                        .get(it.path())
                        .cloned()
                        .unwrap_or_else(|| it.incoming().clone()),
                    item: it,
                    batch: item.batch,
                    source: item.source,
                    seq_high: item.seq_high,
                })
                .collect();
            self.admit(now, candidates, false);
            return Approved::Released(Some(set));
        }
        match &self.paused {
            Some(paused) if paused.batch == batch => {
                let batches = self.form_batches(now, batch);
                self.paused = None;
                self.unfreeze(now);
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
            let over_stamp = self.quarantine.max_stamp_at(path);
            changes.push(
                self.index
                    .bump_over(path, &over, over_stamp, now.as_unix_nanos()),
            );
        }
        self.quarantine.release(batch);
        if !changes.is_empty() {
            self.touched(now);
        }
        // Every index write re-classifies the wants at its path.
        for path in &paths {
            self.reclassify_want(now, path);
        }
        Some(changes)
    }

    /// `revert` (§8.3) on a paused folder: discard the pending changes,
    /// restore every path's announced record, move the current files to
    /// trash, and want the restored live entries again as `restoring`.
    /// `None` if the folder is not paused.
    pub fn revert(&mut self, now: Timestamp) -> Option<RevertOutcome> {
        let paused = self.paused.take()?;
        let own = self.index.own();
        let others: BTreeSet<NodeId> = self.members.iter().copied().filter(|m| *m != own).collect();
        let mut trash = Vec::new();
        let mut refetch = 0;
        let reverted = self.index.revert_pending();
        for reverted in &reverted {
            if reverted.current.as_ref().is_some_and(|e| !e.deleted) {
                trash.push(reverted.path.clone());
            }
            if let Some(record) = &reverted.restored
                && !record.entry.deleted
            {
                let mode = if record.entry.kind == Kind::Dir {
                    ApplyMode::Direct
                } else {
                    ApplyMode::Fetch
                };
                refetch += 1;
                self.wants.insert_restoring(Want {
                    entry: record.entry.clone(),
                    mode,
                    conflict: None,
                    batch: paused.batch,
                    source: own,
                    seq_high: 0,
                    sources: others.clone(),
                    excluded: BTreeSet::new(),
                    mismatches: 0,
                    fetched: false,
                    restoring: true,
                    answered: BTreeSet::new(),
                    state: WantState::Wanted,
                });
            }
        }
        self.window = None;
        self.unfreeze(now);
        Some(RevertOutcome {
            batch: paused.batch,
            reverted,
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

    /// A peer's decision on one of our batches carries its contiguous
    /// watermark of our records (§7.4). It replaces what we knew, being the
    /// truth about what the peer holds; below what we have announced, the
    /// peer missed a batch (lost or still in flight), and catch-up at the
    /// next tick re-sends everything above the watermark, exactly as a
    /// reconnect does. A duplicate costs a batch the receiver drops as
    /// already held; a missing one would never converge.
    pub fn acknowledged(&mut self, peer: NodeId, watermark: u64) {
        self.acked.insert(peer, watermark);
        if watermark < self.index.announced_seq() {
            self.catchup.insert(peer);
        }
    }

    /// A peer disconnected: fetches from it are over (§7.5).
    pub fn peer_gone(&mut self, peer: NodeId) {
        self.wants.peer_gone(peer);
    }

    /// The host reported on a fetch (§7.5 steps 3 and 4). Returns the
    /// want's new state, if the report matched one, and the tombstone
    /// written if the report settled a restoring want as unrecoverable
    /// (§8.3 step 4): every other member has now answered `NotAvailable`,
    /// the restored record describes content that exists nowhere, so the
    /// deletion stands as a local change, announced like any other.
    pub fn fetched(
        &mut self,
        now: Timestamp,
        path: &RelPath,
        version: &Version,
        report: FetchReport,
    ) -> Fetched {
        let own = self.index.own();
        let unrecoverable = {
            let want = self.wants.fetched(path, version, report);
            want.is_some_and(|w| {
                w.restoring
                    && report == FetchReport::NotAvailable
                    && self
                        .members
                        .iter()
                        .all(|m| *m == own || w.answered.contains(m))
            })
        };
        let state = self.wants.get(path).map(|w| w.state);
        if !unrecoverable {
            return Fetched {
                state,
                unrecoverable: None,
            };
        }
        self.wants.remove(path);
        let change = self
            .index
            .observe_absent_unrecoverable(path, now.as_unix_nanos());
        if change.is_some() {
            self.touched(now);
        }
        Fetched {
            state: None,
            unrecoverable: change,
        }
    }

    /// The host reported bytes arriving for a fetch (§7.5).
    pub fn progress(&mut self, now: Timestamp, path: &RelPath, version: &Version) {
        self.wants.progress(now, path, version);
    }

    /// A want persisted by the host comes back after a restart (Phase 2).
    pub fn restore_want(&mut self, want: Want) {
        self.wants.restore(want);
    }

    /// The process restarted with this state (§11, §13): every want the
    /// host was fetching or committing is wanted again, the open scan
    /// bracket is gone, and peers will announce themselves afresh.
    pub fn restarted(&mut self) {
        self.wants.restarted();
        self.scan = None;
        self.catchup.clear();
        self.window = None;
    }

    /// Decide the next host steps for the want-list (§7.5) and adopt every
    /// index-only want. Returns the steps and the records adopted.
    pub fn dispatch(
        &mut self,
        now: Timestamp,
        peers: &BTreeMap<NodeId, Tier>,
    ) -> (Vec<HostStep>, Vec<IndexRecord>) {
        let steps = self.wants.dispatch(now, &self.rules, peers);
        let mut host = Vec::new();
        let mut adopted = Vec::new();
        for step in steps {
            match step {
                WantStep::Fetch {
                    path,
                    version,
                    from,
                } => {
                    let (hash, size) = self
                        .wants
                        .get(&path)
                        .map(|w| (w.entry.hash, w.entry.size))
                        .unwrap_or((crate::entry::ContentHash::EMPTY, 0));
                    host.push(HostStep::Fetch {
                        path,
                        version,
                        hash,
                        size,
                        from,
                    });
                }
                WantStep::Commit(want) => {
                    let want = *want;
                    let path = want.path().clone();
                    let expected = self.expected(&path);
                    host.push(match want.mode {
                        ApplyMode::MetadataOnly => HostStep::SetMeta {
                            path,
                            mtime_ns: want.entry.mtime_ns,
                            exec: want.entry.exec,
                        },
                        _ if want.entry.deleted => HostStep::Remove {
                            path,
                            expected,
                            displace: match &want.conflict {
                                Some(copy) => Displace::ConflictCopy(copy.path.clone()),
                                None => Displace::Trash,
                            },
                        },
                        _ => HostStep::Write {
                            path,
                            displace: match &want.conflict {
                                Some(copy) => Displace::ConflictCopy(copy.path.clone()),
                                None => Displace::Trash,
                            },
                            entry: want.entry,
                            expected,
                        },
                    });
                }
                WantStep::Adopt(want) => {
                    let path = want.path().clone();
                    match self.adopt(now, want.entry.clone()) {
                        Some(record) => adopted.push(record),
                        None => self.defer_changed_underneath(*want),
                    }
                    self.reconsider(now, &path, DeferredReason::ChangedUnderneath);
                }
            }
        }
        (host, adopted)
    }

    /// Want-list changes since the last call, for `WantChanged` actions.
    pub fn want_changes(&mut self) -> Vec<(RelPath, Option<Want>)> {
        self.wants.drain_changes()
    }

    /// The host finished committing (or failed to commit) a want (§7.5
    /// steps 6 to 9). On `Ok` the index adopts the entry and the window
    /// opens so the adoption is announced (§7.4); if the want carried a
    /// conflict copy, the displaced file is recorded at the conflict path
    /// as this machine's local add (§7.6). On `ChangedUnderneath` the entry
    /// is kept in the deferred set until the path is observed again. Either
    /// way the want ends. Returns every record written, in order; empty if
    /// nothing matched or the commit did not happen.
    pub fn applied(
        &mut self,
        now: Timestamp,
        path: &RelPath,
        version: &Version,
        outcome: ApplyOutcome,
    ) -> Vec<IndexRecord> {
        if self.wants.get(path).is_none_or(|w| w.version() != version) {
            return Vec::new();
        }
        let Some(want) = self.wants.remove(path) else {
            return Vec::new();
        };
        match outcome {
            ApplyOutcome::Ok => {
                let Some(record) = self.adopt(now, want.entry.clone()) else {
                    // The record moved on under the commit (release-build
                    // fallback of Index::adopt): keep the entry for the next
                    // observation rather than overwrite a local write.
                    self.defer_changed_underneath(want);
                    return Vec::new();
                };
                let mut written = vec![record];
                if let Some(copy) = want.conflict {
                    let copy_path = copy.path.clone();
                    let change = self.index.record_conflict_copy(copy.path, &copy.loser);
                    self.touched(now);
                    written.push(change.record);
                    // Every index write re-classifies the wants at its path: a
                    // want for a peer's copy of the same loser now meets this
                    // machine's own copy, an identical-content merge rather
                    // than a fetch that would land over it.
                    self.reclassify_want(now, &copy_path);
                }
                self.reconsider(now, path, DeferredReason::ChangedUnderneath);
                written
            }
            ApplyOutcome::ChangedUnderneath => {
                self.defer_changed_underneath(want);
                Vec::new()
            }
        }
    }

    /// Keep a want's entry in the deferred set until the next observation of
    /// its path (§7.5 step 6).
    fn defer_changed_underneath(&mut self, want: Want) {
        self.deferred
            .entry(want.path().clone())
            .or_default()
            .push(Deferred {
                entry: want.entry,
                batch: want.batch,
                source: want.source,
                seq_high: want.seq_high,
                reason: DeferredReason::ChangedUnderneath,
            });
    }

    /// Take a committed remote entry into the index and open the window so
    /// it is announced (§7.4 "adopted records are announced too"). `None` if
    /// the record no longer lets it (see [`Index::adopt`]).
    pub fn adopt(&mut self, now: Timestamp, entry: Entry) -> Option<IndexRecord> {
        let record = self.index.adopt(entry)?.clone();
        self.touched(now);
        Some(record)
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
        assert_eq!(b.wants().len(), 1);
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
        assert!(b.wants().is_empty(), "the want ended");
        assert_eq!(b.due(), Some(t(7.0)), "an adoption opens the window");
        let relayed = b.form_batches(t(7.0), batch_id().successor(5)).remove(0);
        assert_eq!(relayed.entries, vec![entry.clone()]);
        assert_eq!(relayed.summary, Default::default(), "adopted, not counted");

        // A second copy of the same batch is entirely ignored now.
        let again = b.receive(t(0.0), &batch).set;
        assert!(again.is_empty());
        assert_eq!(again.ignored, 1);
        assert!(b.wants().is_empty());
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
        assert!(b.wants().is_empty());
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
        assert_eq!(b.wants().len(), 1);
        let want = b.wants().get(&p("x")).unwrap();
        assert_eq!(
            (want.batch, want.source, want.seq_high),
            (batch.id, node(1), 1)
        );
        assert_eq!(want.mode, ApplyMode::Fetch);
        let m = &want.entry;
        assert_eq!(m.hash, hash(1), "M carries the winner's content");
        assert!(m.version.dominates(&entry.version));
        assert!(
            m.version
                .dominates(&b.index().get(&p("x")).unwrap().entry.version)
        );
        let copy = want.conflict.as_ref().unwrap();
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
        assert_eq!(b.wants().len(), 1);
        // Delete vs modify: the live incoming side wins (§7.6 rule 1). There is
        // no local file to displace, so no conflict copy; M carries A's
        // content under the merged vector.
        let want = b.wants().get(&p("x")).unwrap();
        assert_eq!(want.mode, ApplyMode::Fetch);
        assert!(want.conflict.is_none());
        let m = &want.entry;
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
        assert_eq!(b.wants().len(), 1);
        let want = b.wants().get(&p("x")).unwrap();
        assert_eq!(want.entry, entry);
        assert_eq!(want.mode, ApplyMode::Fetch);
        assert!(want.conflict.is_none());
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
    fn acknowledgements_replace_what_we_knew() {
        let mut f = folder();
        assert_eq!(f.acked_by(node(2)), 0);
        f.acknowledged(node(2), 7);
        f.acknowledged(node(2), 3);
        assert_eq!(f.acked_by(node(2)), 3, "the peer's watermark is the truth");
    }

    #[test]
    fn a_batch_lost_in_flight_is_caught_up_from_the_watermark() {
        // A announces f00 (seq 1) in one batch and f01 (seq 2) in the next;
        // only the second reaches B. B has a gap, acknowledges its watermark
        // 0, and A re-sends everything above 0 at the next tick (§7.4).
        let mut a = folder_with(Rules::default(), 1, "alpha");
        let mut b = folder_with(Rules::default(), 2, "bravo");
        a.scanned(t(1.0), p("f00"), file(1, 1));
        let first = a.form_batches(t(3.0), bid(1)).remove(0);
        a.scanned(t(4.0), p("f01"), file(2, 2));
        let second = a.form_batches(t(6.0), bid(2)).remove(0);
        assert_eq!((first.seq_low, first.seq_high), (0, 1));
        assert_eq!((second.seq_low, second.seq_high), (1, 2), "the chain");

        let r = b.receive(t(6.0), &second);
        assert_eq!(r.decision, Decision::Accepted, "processed all the same");
        assert_eq!(b.wants().len(), 1, "f01 is wanted");
        assert_eq!(
            b.index().peer_seq(node(1)),
            0,
            "acknowledged at the watermark"
        );
        assert!(b.index().peer_has_gap(node(1)));

        a.acknowledged(node(2), b.index().peer_seq(node(1)));
        assert!(a.wake_now(), "catch-up at the next tick");
        let (mut batches, _) = a.catchup_batches(t(7.0), bid(3));
        let (peer, mut sent) = batches.remove(0);
        assert_eq!(peer, node(2));
        let resend = sent.remove(0);
        assert_eq!((resend.seq_low, resend.seq_high), (0, 2));
        assert_eq!(resend.entries.len(), 2, "both records, f01 again");
        let r = b.receive(t(7.0), &resend);
        assert_eq!(
            r.already_wanted, 1,
            "f01's copy only names the source again"
        );
        assert_eq!(b.wants().len(), 2);
        assert_eq!(b.index().peer_seq(node(1)), 2, "the gap filled");
        assert!(!b.index().peer_has_gap(node(1)));
        a.acknowledged(node(2), 2);
        assert!(!a.wake_now(), "nothing left to catch up");
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

    fn file_obs(h: u8) -> ScanState {
        file(h, 1)
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

    /// Host shorthand: every member but this one is on the LAN, every fetch
    /// succeeds and every commit is `Ok`. Drives the want-list until it is
    /// empty and returns every record written.
    fn commit_all(f: &mut FolderState, now: Timestamp) -> Vec<IndexRecord> {
        let own = f.index().own();
        let peers: BTreeMap<NodeId, Tier> = f
            .members()
            .filter(|m| *m != own)
            .map(|m| (m, Tier::Lan))
            .collect();
        let mut out = Vec::new();
        for _ in 0..64 {
            // Settle whatever an earlier dispatch already asked the host for.
            let in_flight: Vec<(RelPath, Version, WantState)> = f
                .wants()
                .iter()
                .map(|w| (w.path().clone(), w.version().clone(), w.state))
                .collect();
            for (path, version, state) in in_flight {
                match state {
                    WantState::Fetching { .. } => {
                        f.fetched(now, &path, &version, FetchReport::Ok);
                    }
                    WantState::Committing { .. } => {
                        out.extend(f.applied(now, &path, &version, ApplyOutcome::Ok));
                    }
                    _ => {}
                }
            }
            let (steps, adopted) = f.dispatch(now, &peers);
            out.extend(adopted);
            for step in &steps {
                match step {
                    HostStep::Fetch { path, version, .. } => {
                        f.fetched(now, path, version, FetchReport::Ok);
                    }
                    HostStep::Write { path, entry, .. } => {
                        out.extend(f.applied(now, path, &entry.version, ApplyOutcome::Ok));
                    }
                    HostStep::Remove { path, .. } | HostStep::SetMeta { path, .. } => {
                        let version = f.wants().get(path).unwrap().version().clone();
                        out.extend(f.applied(now, path, &version, ApplyOutcome::Ok));
                    }
                }
            }
            if steps.is_empty() && f.wants().iter().all(|w| !w.in_flight()) {
                break;
            }
        }
        assert!(
            f.wants().iter().all(|w| !w.in_flight()),
            "commit_all left work in flight: {:?}",
            f.wants()
                .iter()
                .map(|w| (w.path().as_str(), w.state))
                .collect::<Vec<_>>()
        );
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
        assert!(b.wants().is_empty(), "nothing applied");
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
        assert!(b.wants().is_empty());
        let stored = &b.quarantine().get(bid(3)).unwrap().entries[&p("f00")];
        assert!(!stored.deleted);
        assert_eq!(stored.hash, hash(7));
        assert_eq!(b.quarantine().versions_at(&p("f00")).len(), 2);

        // Unrelated paths from the same source still sync.
        a.scanned(t(17.0), p("f09"), file(9, 9));
        let other = a.form_batches(t(19.0), bid(6)).remove(0);
        let r = b.receive(t(19.0), &other);
        assert_eq!((r.joined, r.decision), (0, Decision::Accepted));
        assert_eq!(b.wants().len(), 1);
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
        assert_eq!(b.wants().len(), 8, "one want per released path");
        assert_eq!(b.approve(t(22.0), bid(3)), Approved::Unknown);
    }

    #[test]
    fn a_version_already_wanted_is_not_rebraked_when_relayed() {
        // A announces a file; B accepts it and starts fetching. C relays the
        // same version inside a batch that trips B's brake: the relayed copy
        // must only add C as a source, not be quarantined under B's own fetch.
        let (mut a, mut b) = a_and_b(10, tight());
        a.scanned(t(10.0), p("new"), file(7, 7));
        let batch = a.form_batches(t(12.0), bid(3)).remove(0);
        let r = b.receive(t(12.0), &batch);
        assert_eq!(r.decision, Decision::Accepted);
        let wanted = b.wants().get(&p("new")).unwrap().version().clone();
        b.dispatch(t(12.0), &lan(&[1]));
        assert!(b.in_flight(&p("new")));

        // C's batch: the same version of "new" plus eight deletes.
        let mut c = folder_with(Rules::default(), 3, "charlie");
        for i in 0..10 {
            let e = b
                .index()
                .get(&p(&format!("f{i:02}")))
                .unwrap()
                .entry
                .clone();
            c.adopt(t(13.0), e);
        }
        c.adopt(t(13.0), batch.entries[0].clone());
        c.form_batches(t(14.0), bid(4));
        for i in 0..8 {
            c.scanned(t(15.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        let mut relay = c.form_batches(t(17.0), bid(5)).remove(0);
        relay.entries.push(batch.entries[0].clone()); // the relayed "new"
        relay.seq_high += 1;
        let r = b.receive(t(17.0), &relay);
        assert_eq!(r.already_wanted, 1);
        assert_eq!(
            r.held,
            Some(HoldReason::Count {
                destructive: 8,
                tracked: 10
            })
        );
        assert!(
            b.quarantine().versions_at(&p("new")).is_empty(),
            "the wanted version is not quarantined"
        );
        let want = b.wants().get(&p("new")).unwrap();
        assert_eq!(want.version(), &wanted);
        assert!(want.sources.contains(&node(3)), "C named as a source");
        assert!(b.in_flight(&p("new")), "the fetch goes on");
        // The commit lands without touching the quarantine.
        b.fetched(t(20.0), &p("new"), &wanted, FetchReport::Ok);
        b.dispatch(t(18.0), &lan(&[1]));
        assert_eq!(
            b.applied(t(19.0), &p("new"), &wanted, ApplyOutcome::Ok)
                .len(),
            1
        );
        assert_eq!(b.quarantine().len(), 1, "the deletes stay held");
    }

    #[test]
    fn frozen_edits_meet_the_brake_when_the_folder_unpauses() {
        // B edits eight files and pauses (8 mods of 10). A edits the same
        // eight while B is paused: frozen. At approve they resolve as eight
        // conflicts, eight mods against B's live copies: held, not applied.
        let (mut a, mut b) = a_and_b(10, tight());
        for i in 0..8 {
            b.scanned(t(10.0), p(&format!("f{i:02}")), file(2, 20));
        }
        assert!(matches!(
            b.tick(t(12.0), bid(5)),
            Ticked::Paused { first: true, .. }
        ));
        for i in 0..8 {
            a.scanned(t(13.0), p(&format!("f{i:02}")), file(3, 30));
        }
        let batch = a.form_batches(t(15.0), bid(6)).remove(0);
        let r = b.receive(t(15.0), &batch);
        assert_eq!(r.frozen, 8);
        assert!(b.wants().is_empty());
        assert!(matches!(b.approve(t(20.0), bid(5)), Approved::Sent(_)));
        assert!(b.deferred().next().is_none(), "unfrozen");
        assert!(b.wants().is_empty(), "held, not wanted");
        let held = b.quarantine().get(bid(6)).unwrap();
        assert_eq!(held.entries.len(), 8);
        assert_eq!(held.source, node(1));
        assert!(matches!(
            held.reason,
            HoldReason::Count {
                destructive: 8,
                tracked: 10
            }
        ));
        let statuses = b.take_statuses();
        assert!(
            matches!(statuses.as_slice(), [FolderStatus::Held { batch, paths: 8, .. }] if *batch == bid(6))
        );
        // Approving the held item applies the eight conflicts.
        match b.approve(t(21.0), bid(6)) {
            Approved::Released(Some(set)) => assert_eq!(set.items.len(), 8),
            other => panic!("expected a release, got {other:?}"),
        }
        assert_eq!(b.wants().len(), 8);
    }

    #[test]
    fn frozen_entries_wait_for_unpause_through_scans_and_commits() {
        // B edits eight files and pauses. A's edit of f00 arrives frozen.
        // Scans of f00, a full scan bracket and a commit elsewhere must not
        // thaw it: only the unpause does (§8.1).
        let (mut a, mut b) = a_and_b(10, tight());
        for i in 0..8 {
            b.scanned(t(10.0), p(&format!("f{i:02}")), file(2, 20));
        }
        assert!(matches!(b.tick(t(12.0), bid(5)), Ticked::Paused { .. }));
        a.scanned(t(13.0), p("f00"), file(3, 30));
        let batch = a.form_batches(t(15.0), bid(6)).remove(0);
        assert_eq!(b.receive(t(15.0), &batch).frozen, 1);

        b.scanned(t(16.0), p("f00"), ScanState::Unchanged);
        b.scan_started();
        for i in 0..10 {
            b.scanned(t(17.0), p(&format!("f{i:02}")), ScanState::Unchanged);
        }
        assert!(b.scan_finished(t(18.0)).unwrap().is_empty());
        let frozen: Vec<&Deferred> = b.deferred().collect();
        assert_eq!(frozen.len(), 1, "still frozen after the scans");
        assert_eq!(frozen[0].reason, DeferredReason::Frozen);
        assert!(b.wants().is_empty(), "nothing wanted while paused");

        assert!(matches!(b.approve(t(20.0), bid(5)), Approved::Sent(_)));
        assert!(b.deferred().next().is_none(), "the unpause thaws it");
        assert_eq!(b.wants().len(), 1, "one conflict, wanted");
    }

    #[test]
    fn frozen_entries_of_a_held_batch_join_it_at_unpause() {
        // B edits seven files and pauses. A's batch edits the same seven
        // (frozen) and deletes the other three (held: three of ten). At
        // unpause the seven belong with the held batch, not in front of the
        // brake on their own (§8.2, I5).
        let (mut a, mut b) = a_and_b(10, tight());
        for i in 0..7 {
            b.scanned(t(10.0), p(&format!("f{i:02}")), file(2, 20));
        }
        assert!(matches!(b.tick(t(12.0), bid(5)), Ticked::Paused { .. }));
        for i in 0..7 {
            a.scanned(t(13.0), p(&format!("f{i:02}")), file(3, 30));
        }
        for i in 7..10 {
            a.scanned(t(13.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        let batch = a.form_batches(t(15.0), bid(6)).remove(0);
        let r = b.receive(t(15.0), &batch);
        assert_eq!(r.frozen, 7);
        assert!(matches!(r.decision, Decision::Held { .. }));
        assert_eq!(b.quarantine().get(bid(6)).unwrap().entries.len(), 3);
        b.take_statuses();

        assert!(matches!(b.approve(t(20.0), bid(5)), Approved::Sent(_)));
        assert!(b.deferred().next().is_none(), "thawed");
        assert!(
            b.wants().is_empty(),
            "nothing wanted: the batch is under review"
        );
        let held = b.quarantine().get(bid(6)).unwrap();
        assert_eq!(held.entries.len(), 10, "the seven joined the three");
        assert!(
            held.entries.values().all(|e| e.modified_by == node(1)),
            "held as received from A, not as B's merge"
        );
        assert_eq!(
            b.take_statuses(),
            vec![FolderStatus::JoinedHeld {
                batch: bid(6),
                count: 7
            }]
        );
        match b.approve(t(21.0), bid(6)) {
            Approved::Released(Some(set)) => assert_eq!(set.items.len(), 10),
            other => panic!("expected a release, got {other:?}"),
        }
        assert_eq!(b.wants().len(), 10);
    }

    #[test]
    fn a_scan_bracket_reconsiders_a_deferred_entry_at_an_unrecorded_path() {
        // A adds z; B's commit of it fails ChangedUnderneath (say the host
        // lost the temp file). B has no record of z and no file at z, so no
        // scan will ever report z; the bracket's end must stand in for the
        // observation, or the entry waits forever.
        let (mut a, mut b) = a_and_b(10, tight());
        a.scanned(t(10.0), p("z"), file(3, 30));
        let batch = a.form_batches(t(12.0), bid(6)).remove(0);
        assert_eq!(b.receive(t(12.0), &batch).decision, Decision::Accepted);
        let v = b.wants().get(&p("z")).unwrap().version().clone();
        let (steps, _) = b.dispatch(t(13.0), &lan(&[1]));
        assert!(matches!(&steps[0], HostStep::Fetch { .. }));
        b.fetched(t(20.0), &p("z"), &v, FetchReport::Ok);
        let (steps, _) = b.dispatch(t(14.0), &lan(&[1]));
        assert!(matches!(&steps[0], HostStep::Write { .. }));
        assert!(
            b.applied(t(15.0), &p("z"), &v, ApplyOutcome::ChangedUnderneath)
                .is_empty()
        );
        assert_eq!(b.deferred().count(), 1);
        assert!(b.wants().is_empty());

        b.scan_started();
        for i in 0..10 {
            b.scanned(t(16.0), p(&format!("f{i:02}")), ScanState::Unchanged);
        }
        assert!(
            b.scan_finished(t(17.0)).unwrap().is_empty(),
            "nothing tombstoned"
        );
        assert!(
            b.deferred().next().is_none(),
            "reconsidered at the bracket's end"
        );
        let want = b.wants().get(&p("z")).unwrap();
        assert_eq!(want.version(), &v, "wanted again");
        assert!(!want.fetched, "a fresh want fetches again");
    }

    #[test]
    fn writing_our_own_conflict_copy_reclassifies_a_want_for_a_peers_copy() {
        // B holds L for "f". C, another L-holder, resolved the same conflict
        // first and announced its copy of L at the conflict path. B wants
        // that copy (a fetch), then B's own commit of W lands and writes B's
        // copy record at the very same path. The want must meet B's record
        // as an identical-content merge, not stay a fetch whose commit would
        // land over B's own write (I8).
        let (mut a, mut b) = a_and_b(3, Rules::default());
        b.scanned(t(10.0), p("f00"), file(5, 50)); // B's L
        let loser = b.index().get(&p("f00")).unwrap().entry.clone();
        let copy_path = crate::conflict::conflict_copy_name(&loser).unwrap();
        a.scanned(t(11.0), p("f00"), file(6, 60)); // A's W, newer
        let batch = a.form_batches(t(13.0), bid(6)).remove(0);
        assert_eq!(b.receive(t(13.0), &batch).decision, Decision::Accepted);
        let want = b.wants().get(&p("f00")).unwrap();
        let w_version = want.version().clone();
        assert_eq!(want.conflict.as_ref().map(|c| &c.path), Some(&copy_path));

        // C's copy of the same L arrives: same content as B's future copy,
        // C's own version.
        let mut c_copy = loser.clone();
        c_copy.path = copy_path.clone();
        c_copy.version = Version::empty().incremented(node(3));
        c_copy.modified_by = node(3);
        c_copy.prev_hash = ContentHash::EMPTY;
        let mut c_batch = batch.clone();
        c_batch.id = bid(7);
        c_batch.source = node(3);
        c_batch.entries = vec![c_copy.clone()];
        c_batch.seq_low = 0;
        c_batch.seq_high = 1;
        assert_eq!(b.receive(t(14.0), &c_batch).decision, Decision::Accepted);
        assert_eq!(
            b.wants().get(&copy_path).unwrap().mode,
            ApplyMode::Fetch,
            "C's copy is wanted as content to fetch"
        );

        // B fetches W and commits it, displacing L to the copy path.
        let (steps, _) = b.dispatch(t(15.0), &lan(&[1, 3]));
        assert!(
            steps
                .iter()
                .any(|s| matches!(s, HostStep::Fetch { path, .. } if path == &p("f00")))
        );
        b.fetched(t(20.0), &p("f00"), &w_version, FetchReport::Ok);
        let (steps, _) = b.dispatch(t(16.0), &lan(&[1, 3]));
        assert!(steps.iter().any(|s| matches!(s, HostStep::Write { path, displace: Displace::ConflictCopy(c), .. } if path == &p("f00") && c == &copy_path)));
        let written = b.applied(t(17.0), &p("f00"), &w_version, ApplyOutcome::Ok);
        assert_eq!(written.len(), 2, "W adopted and B's copy recorded");
        let own_copy = b.index().get(&copy_path).unwrap().entry.clone();
        assert_eq!(own_copy.modified_by, node(2));

        // The want at the copy path was re-classified against B's record:
        // identical content, so it is a merge that dominates B's copy, not
        // a fetch of C's bytes.
        let want = b.wants().get(&copy_path).expect("still wanted, as a merge");
        assert_ne!(want.mode, ApplyMode::Fetch);
        assert!(want.version().dominates(&own_copy.version));
        assert!(want.version().dominates(&c_copy.version));
        // A late report for the old fetch matches nothing.
        assert!(
            b.applied(t(18.0), &copy_path, &c_copy.version, ApplyOutcome::Ok)
                .is_empty()
        );
        // Dispatch adopts the merge index-only; the record dominates both.
        let (_, adopted) = b.dispatch(t(19.0), &lan(&[1, 3]));
        assert_eq!(adopted.len(), 1);
        let record = b.index().get(&copy_path).unwrap();
        assert!(record.entry.version.dominates(&c_copy.version));
        assert!(b.wants().get(&copy_path).is_none());
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
        let held_stamp = b.quarantine().max_stamp_at(&p("f03")).unwrap();
        assert_eq!(b.deny(t(13.0), bid(9)), None);
        let changes = b.deny(t(13.0), bid(3)).unwrap();
        assert_eq!(changes.len(), 8);
        for c in &changes {
            let e = &c.record.entry;
            assert!(!e.deleted, "B's copies win");
            assert!(
                e.stamp > held_stamp,
                "the bump ranks above the tombstones it dominates (§7.6)"
            );
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
        assert_eq!(out.reverted.len(), 10, "one write per pending path");
        let restored: Vec<&IndexRecord> = out
            .reverted
            .iter()
            .filter_map(|r| r.restored.as_ref())
            .collect();
        let untouched: Vec<&IndexRecord> = announced
            .iter()
            .filter(|r| r.entry.path != p("f09"))
            .collect();
        assert_eq!(restored, untouched, "f09 was never pending");
        assert!(
            out.reverted
                .iter()
                .any(|r| r.path == p("junk") && r.restored.is_none()),
            "a path peers never saw is reported removed"
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
        assert_eq!(b.wants().len(), 9);
        assert!(
            b.wants()
                .iter()
                .all(|w| w.mode == ApplyMode::Fetch && w.conflict.is_none() && w.restoring)
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
        // One want per path: the second version dominated the first and
        // replaced it.
        let want = b.wants().get(&p("f00")).unwrap();
        assert!(!want.entry.deleted);
        assert_eq!(want.entry.hash, hash(5));
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
        assert!(b.wants().is_empty());
    }

    // ---- §7.4 catch-up, §7.5 want-list, §8.3 in flight --------------------------

    fn lan(peers: &[u8]) -> BTreeMap<NodeId, Tier> {
        peers.iter().map(|n| (node(*n), Tier::Lan)).collect()
    }

    #[test]
    fn have_up_to_marks_catch_up_of_announced_records_only() {
        let mut a = folder_with(Rules::default(), 1, "alpha");
        for i in 0..5 {
            a.scanned(t(1.0), p(&format!("f{i}")), file(1, 1));
        }
        a.form_batches(t(3.0), bid(1));
        a.scanned(t(4.0), p("pending"), file(2, 2));
        assert!(!a.wake_now());
        a.acknowledged(node(2), 4);
        a.have_up_to(node(2), 2);
        assert_eq!(
            a.acked_by(node(2)),
            2,
            "have_up_to replaces the ack, even downwards"
        );
        assert!(a.wake_now());
        let (batches, used) = a.catchup_batches(t(5.0), bid(9));
        assert_eq!(used, 1);
        assert!(!a.wake_now());
        assert_eq!(batches.len(), 1);
        let (peer, list) = &batches[0];
        assert_eq!(*peer, node(2));
        assert_eq!(list.len(), 1);
        let b = &list[0];
        assert_eq!(b.id, bid(9));
        let paths: Vec<_> = b.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["f2", "f3", "f4"],
            "records above seq 2, announced only"
        );
        assert_eq!(b.seq_high, 5);
        assert_eq!(
            b.summary,
            Summary::default(),
            "already-announced records carry no counts"
        );
        assert_eq!(
            a.index().pending_count(),
            1,
            "the pending record waits for the live path"
        );
        // A peer that holds everything gets nothing.
        a.have_up_to(node(3), 5);
        let (batches, used) = a.catchup_batches(t(6.0), bid(10));
        assert!(batches.is_empty());
        assert_eq!(used, 0);
    }

    #[test]
    fn catch_up_splits_at_ten_thousand_on_seq() {
        let mut a = folder_with(Rules::default(), 1, "alpha");
        let n = crate::batch::MAX_BATCH_ENTRIES + 5;
        for i in 0..n {
            a.scanned(t(1.0), p(&format!("f{i:05}")), file(1, i as i64));
        }
        a.form_batches(t(3.0), bid(1));
        a.have_up_to(node(2), 0);
        let (batches, used) = a.catchup_batches(t(5.0), bid(9));
        assert_eq!(used, 2);
        let list = &batches[0].1;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, bid(9));
        assert_eq!(list[1].id, bid(9).successor(1));
        assert_eq!(list[0].entries.len(), crate::batch::MAX_BATCH_ENTRIES);
        assert_eq!(list[0].seq_high, crate::batch::MAX_BATCH_ENTRIES as u64);
        assert_eq!(list[1].entries.len(), 5);
        assert_eq!(list[1].seq_high, n as u64);
    }

    #[test]
    fn a_paused_pending_set_stays_out_of_catch_up() {
        let (_, mut b) = a_and_b(10, tight());
        for i in 0..8 {
            b.scanned(t(10.0), p(&format!("f{i:02}")), ScanState::Absent);
        }
        assert!(matches!(
            b.tick(t(12.0), bid(5)),
            Ticked::Paused { first: true, .. }
        ));
        b.have_up_to(node(1), 0);
        let (batches, _) = b.catchup_batches(t(13.0), bid(6));
        let entries = &batches[0].1[0].entries;
        assert_eq!(entries.len(), 10, "the announced adds only");
        assert!(
            entries.iter().all(|e| !e.deleted),
            "no pending tombstone leaks"
        );
    }

    #[test]
    fn accepted_items_become_wants_and_the_host_is_driven_to_completion() {
        let (mut a, mut b) = a_and_b(3, Rules::default());
        a.scanned(t(10.0), p("d"), observed(Kind::Dir, 0, 0, false));
        a.scanned(t(10.0), p("d/new"), file(7, 7));
        a.scanned(t(10.0), p("f00"), file(8, 8));
        a.scanned(t(10.0), p("f01"), file(1, 9)); // touch
        a.scanned(t(10.0), p("f02"), ScanState::Absent);
        let batch = a.form_batches(t(12.0), bid(3)).remove(0);
        let r = b.receive(t(12.0), &batch);
        assert_eq!(r.decision, Decision::Accepted);
        assert_eq!(b.wants().len(), 5);
        let (steps, adopted) = b.dispatch(t(12.0), &lan(&[1]));
        assert!(adopted.is_empty());
        let kinds: Vec<&str> = steps
            .iter()
            .map(|s| match s {
                HostStep::Fetch { .. } => "fetch",
                HostStep::Write { .. } => "write",
                HostStep::Remove { .. } => "remove",
                HostStep::SetMeta { .. } => "setmeta",
            })
            .collect();
        assert_eq!(
            kinds,
            ["fetch", "fetch", "write", "setmeta", "remove"],
            "fetches first, then commits: d, f01, f02"
        );
        assert!(
            matches!(&steps[2], HostStep::Write { path, expected: None, displace: Displace::Trash, .. } if path == &p("d"))
        );
        assert!(
            matches!(&steps[3], HostStep::SetMeta { path, mtime_ns: 9, exec: false } if path == &p("f01"))
        );
        assert!(
            matches!(&steps[4], HostStep::Remove { path, expected: Some(Expected { kind: Kind::File, size: 10, mtime_ns: 1 }), displace: Displace::Trash } if path == &p("f02"))
        );
        assert_eq!(
            b.wants().get(&p("d/new")).unwrap().state,
            WantState::Fetching {
                from: node(1),
                deadline: t(72.0)
            }
        );
        assert!(b.in_flight(&p("d/new")));
        commit_all(&mut b, t(13.0));
        assert!(b.wants().is_empty());
        assert_eq!(
            b.index().tracked_count(),
            3,
            "f00, f01 and d/new; f02 gone; d is a directory"
        );
        assert_eq!(b.index().get(&p("d/new")).unwrap().entry.hash, hash(7));
        assert!(b.index().get(&p("f02")).unwrap().entry.deleted);
    }

    #[test]
    fn a_deferred_want_is_observable_and_a_local_edit_reclassifies_it() {
        let rules = Rules {
            relay_limit: 5,
            ..Rules::default()
        };
        let (mut a, mut b) = a_and_b(1, rules);
        a.scanned(t(10.0), p("big"), file(7, 7)); // size 10 > relay limit 5
        let batch = a.form_batches(t(12.0), bid(3)).remove(0);
        b.receive(t(12.0), &batch);
        let relay: BTreeMap<NodeId, Tier> = [(node(1), Tier::Relay)].into_iter().collect();
        assert!(b.dispatch(t(12.0), &relay).0.is_empty());
        assert_eq!(
            b.wants().get(&p("big")).unwrap().state,
            WantState::Deferred { need: Tier::Direct }
        );
        assert!(!b.in_flight(&p("big")), "deferred is observable");
        // The user creates their own "big": a real local change, announced.
        let out = b.scanned(t(13.0), p("big"), file(9, 9));
        assert!(out.change.is_some());
        assert_eq!(b.index().pending_count(), 1);
        let want = b.wants().get(&p("big")).unwrap();
        assert!(
            want.entry.version.dominates(&batch.entries[0].version),
            "the want is now the conflict's M"
        );
        assert_eq!(want.entry.hash, hash(9), "the local edit is newer and wins");
        assert_eq!(want.mode, ApplyMode::IndexOnly, "so nothing to fetch");
        let (_, adopted) = b.dispatch(t(13.0), &relay);
        assert_eq!(adopted.len(), 1);
        assert!(b.wants().is_empty());
        let sent = b.form_batches(t(15.0), bid(4)).remove(0);
        assert!(
            sent.entries
                .iter()
                .any(|e| e.path == p("big") && e.hash == hash(9))
        );
    }

    #[test]
    fn a_restoring_want_ignores_absent_but_a_new_file_cancels_it() {
        let (_, mut b) = a_and_b(
            2,
            Rules {
                hold_count: 1,
                hold_pct: 0,
                ..Rules::default()
            },
        );
        b.scanned(t(10.0), p("f00"), ScanState::Absent);
        b.scanned(t(10.0), p("f01"), ScanState::Absent);
        assert!(matches!(b.tick(t(12.0), bid(5)), Ticked::Paused { .. }));
        b.revert(t(13.0)).unwrap();
        // Nobody connected: without source, observable, but Absent is the trash move.
        assert!(b.dispatch(t(13.0), &BTreeMap::new()).0.is_empty());
        assert_eq!(b.wants().get(&p("f00")).unwrap().state, WantState::NoSource);
        assert!(!b.in_flight(&p("f00")));
        assert_eq!(
            b.scanned(t(14.0), p("f00"), ScanState::Absent),
            Scanned::default()
        );
        b.scan_started();
        assert!(
            b.scan_finished(t(15.0)).unwrap().is_empty(),
            "restoring paths skip the deletion pass"
        );
        assert_eq!(b.index().tracked_count(), 2);
        // A real file appears at f01: a local change that cancels the want.
        let out = b.scanned(t(16.0), p("f01"), file(9, 9));
        assert!(out.change.is_some());
        assert!(b.wants().get(&p("f01")).is_none(), "cancelled");
        assert!(b.wants().get(&p("f00")).is_some());
        assert_eq!(b.index().get(&p("f01")).unwrap().entry.hash, hash(9));
    }

    #[test]
    fn a_restored_path_leaves_flight_only_when_its_commit_lands() {
        let (_, mut b) = a_and_b(
            1,
            Rules {
                hold_count: 1,
                hold_pct: 0,
                ..Rules::default()
            },
        );
        b.scanned(t(10.0), p("f00"), file(5, 5));
        assert!(matches!(b.tick(t(12.0), bid(5)), Ticked::Paused { .. }));
        b.revert(t(13.0)).unwrap();
        let v = b.wants().get(&p("f00")).unwrap().version().clone();
        assert_eq!(b.wants().get(&p("f00")).unwrap().state, WantState::Wanted);
        assert!(b.in_flight(&p("f00")));
        let (steps, _) = b.dispatch(t(13.0), &lan(&[1]));
        assert!(matches!(&steps[0], HostStep::Fetch { from, .. } if *from == node(1)));
        assert!(b.in_flight(&p("f00")));
        b.fetched(t(20.0), &p("f00"), &v, FetchReport::Ok);
        assert!(b.in_flight(&p("f00")));
        let (steps, _) = b.dispatch(t(14.0), &lan(&[1]));
        assert!(matches!(
            &steps[0],
            HostStep::Write {
                expected: Some(_),
                ..
            }
        ));
        assert!(b.in_flight(&p("f00")));
        assert_eq!(b.applied(t(15.0), &p("f00"), &v, ApplyOutcome::Ok).len(), 1);
        assert!(!b.in_flight(&p("f00")));
        assert!(b.wants().is_empty());
    }

    #[test]
    fn a_persisted_want_comes_back_wanted_and_fetches() {
        let (mut a, mut b) = a_and_b(1, Rules::default());
        a.scanned(t(10.0), p("n"), file(7, 7));
        let batch = a.form_batches(t(12.0), bid(3)).remove(0);
        b.receive(t(12.0), &batch);
        b.dispatch(t(12.0), &lan(&[1]));
        let persisted = b.wants().get(&p("n")).unwrap().clone();
        assert!(matches!(persisted.state, WantState::Fetching { .. }));
        // A fresh engine after a restart.
        let mut c = folder_with(Rules::default(), 2, "bravo");
        c.restore_want(persisted);
        assert_eq!(c.wants().get(&p("n")).unwrap().state, WantState::Wanted);
        let (steps, _) = c.dispatch(t(20.0), &lan(&[1]));
        assert!(matches!(&steps[0], HostStep::Fetch { path, .. } if path == &p("n")));
    }

    /// A scripted host for the want-list properties.
    #[derive(Clone, Debug)]
    struct Script {
        outcomes: Vec<FetchReport>,
    }

    impl Script {
        fn next(&mut self) -> FetchReport {
            if self.outcomes.is_empty() {
                FetchReport::Ok
            } else {
                self.outcomes.remove(0)
            }
        }
    }

    fn drive(
        f: &mut FolderState,
        peers: &BTreeMap<NodeId, Tier>,
        script: &mut Script,
        rules: &Rules,
    ) -> Vec<HostStep> {
        let mut all = Vec::new();
        for round in 0..200 {
            let now = t(100.0 + round as f64);
            let (steps, _) = f.dispatch(now, peers);
            // Never more than the limits in flight.
            let fetching = f.wants().fetching();
            assert!(fetching <= rules.max_fetches_per_folder as usize);
            let mut per_peer: BTreeMap<NodeId, usize> = BTreeMap::new();
            for w in f.wants().iter() {
                if let WantState::Fetching { from, .. } = w.state {
                    *per_peer.entry(from).or_insert(0) += 1;
                }
            }
            assert!(
                per_peer
                    .values()
                    .all(|n| *n <= rules.max_fetches_per_peer as usize)
            );
            if steps.is_empty() {
                break;
            }
            for step in &steps {
                match step {
                    HostStep::Fetch { path, version, .. } => {
                        let outcome = script.next();
                        f.fetched(now, path, version, outcome);
                    }
                    HostStep::Write { path, entry, .. } => {
                        f.applied(now, path, &entry.version, ApplyOutcome::Ok);
                    }
                    HostStep::Remove { path, .. } | HostStep::SetMeta { path, .. } => {
                        let version = f.wants().get(path).unwrap().version().clone();
                        f.applied(now, path, &version, ApplyOutcome::Ok);
                    }
                }
            }
            all.extend(steps);
        }
        all
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
                    prop_assert!(b.wants().is_empty(), "held: nothing applied");
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
                            prop_assert_eq!(b.wants().len(), expected_set.items.len());
                        }
                        other => prop_assert!(false, "expected a release, got {other:?}"),
                    }
                    prop_assert!(b.quarantine().is_empty());
                }
                Verdict::Pass => {
                    prop_assert_eq!(r.held, None);
                    prop_assert_eq!(r.decision, Decision::Accepted);
                    prop_assert!(b.quarantine().is_empty());
                    prop_assert_eq!(b.wants().len(), expected_set.items.len());
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

        /// §7.5: every accepted item ends as exactly one of applied, deferred,
        /// without source, or given up; no fetch is ever asked of a peer that
        /// did not announce the version; the fetch limits hold throughout.
        #[test]
        fn every_want_ends_in_exactly_one_place(
            files in prop::collection::vec((0u8..6, 1u64..60, any::<bool>()), 1..12),
            a_tier in prop::sample::select(vec![None, Some(Tier::Lan), Some(Tier::Direct), Some(Tier::Relay)]),
            c_connected: bool,
            outcomes in prop::collection::vec(
                prop::sample::select(vec![FetchReport::Ok, FetchReport::NotAvailable, FetchReport::HashMismatch]),
                0..12,
            ),
            per_peer in 1u32..4,
        ) {
            let rules = Rules {
                direct_limit: 40,
                relay_limit: 20,
                max_fetches_per_peer: per_peer,
                max_fetches_per_folder: 3,
                ..Rules::default()
            };
            let mut a = folder_with(Rules::default(), 1, "alpha");
            for (i, size, is_dir) in &files {
                let path = p(&format!("f{i}"));
                if *is_dir {
                    a.scanned(t(1.0), path, observed(Kind::Dir, 0, 0, false));
                } else {
                    a.scanned(t(1.0), path, ScanState::Observed(Observed {
                        kind: Kind::File,
                        size: *size,
                        mtime_ns: 1,
                        exec: false,
                        hash: hash(*i + 1),
                    }));
                }
            }
            let batch = a.form_batches(t(3.0), bid(1)).remove(0);
            let mut b = folder_with(rules.clone(), 2, "bravo");
            let r = b.receive(t(3.0), &batch);
            prop_assert_eq!(r.decision, Decision::Accepted);
            let wanted: Vec<Entry> = b.wants().iter().map(|w| w.entry.clone()).collect();
            let mut peers = BTreeMap::new();
            if let Some(tier) = a_tier { peers.insert(node(1), tier); }
            if c_connected { peers.insert(node(3), Tier::Lan); }
            let mut script = Script { outcomes };
            let steps = drive(&mut b, &peers, &mut script, &rules);
            for step in &steps {
                if let HostStep::Fetch { from, .. } = step {
                    prop_assert_eq!(*from, node(1), "only the announcer is asked");
                }
            }
            for e in &wanted {
                let applied = b.index().get(&e.path).is_some_and(|r| r.entry == *e);
                let want = b.wants().get(&e.path);
                let parked = want.is_some_and(|w| matches!(w.state, WantState::Deferred { .. } | WantState::NoSource | WantState::GaveUp));
                prop_assert!(applied != parked, "{}: applied={applied} parked={parked} state={:?}", e.path, want.map(|w| w.state));
                if let Some(w) = want {
                    prop_assert!(!w.in_flight(), "nothing left in flight when the host has answered everything");
                    match w.state {
                        WantState::NoSource => prop_assert!(
                            a_tier.is_none() || !w.sources.contains(&node(1)) || w.excluded.contains(&node(1)),
                            "no source only when the announcer moved on or served bad content"
                        ),
                        WantState::Deferred { need } => {
                            let tier = a_tier.unwrap();
                            prop_assert!(!tier.allows(&rules, e.size));
                            prop_assert!(need.allows(&rules, e.size));
                        }
                        WantState::GaveUp => prop_assert_eq!(w.mismatches, 2),
                        _ => {}
                    }
                }
            }
        }

        /// §7.5: every Write of a path comes after the Write of each ancestor
        /// directory in the same list, and every directory Remove after the
        /// Removes of everything under it.
        #[test]
        fn writes_respect_parent_order(
            tree in prop::collection::vec((0u8..3, 0u8..3, 0u8..3), 1..10),
            delete: bool,
        ) {
            let mut a = folder_with(Rules::default(), 1, "alpha");
            let mut paths = BTreeSet::new();
            for (x, y, z) in &tree {
                let d1 = format!("d{x}");
                let d2 = format!("{d1}/e{y}");
                let file = format!("{d2}/f{z}");
                for d in [&d1, &d2] {
                    if paths.insert(d.clone()) {
                        a.scanned(t(1.0), p(d), observed(Kind::Dir, 0, 0, false));
                    }
                }
                if paths.insert(file.clone()) {
                    a.scanned(t(1.0), p(&file), file_obs(*z + 1));
                }
            }
            let mut b = folder_with(Rules::default(), 2, "bravo");
            let batch = a.form_batches(t(3.0), bid(1)).remove(0);
            b.receive(t(3.0), &batch);
            let mut script = Script { outcomes: vec![] };
            let steps = drive(&mut b, &lan(&[1]), &mut script, &Rules::default());
            let writes: Vec<RelPath> = steps.iter().filter_map(|s| match s { HostStep::Write { path, .. } => Some(path.clone()), _ => None }).collect();
            for (i, w) in writes.iter().enumerate() {
                for anc in writes.iter().filter(|o| o.is_ancestor_of(w)) {
                    let j = writes.iter().position(|x| x == anc).unwrap();
                    prop_assert!(j < i, "{anc} must be written before {w}");
                }
            }
            prop_assert!(b.wants().is_empty());
            if delete {
                // A removes everything; B must delete children before parents.
                let all: Vec<RelPath> = a.index().live_records().map(|r| r.entry.path.clone()).collect();
                for path in &all {
                    a.scanned(t(10.0), path.clone(), ScanState::Absent);
                }
                let batch = a.form_batches(t(12.0), bid(2)).remove(0);
                let loose = Rules { hold_count: 0, ..Rules::default() };
                b.rules_changed(loose.clone());
                let r = b.receive(t(12.0), &batch);
                prop_assert_eq!(r.decision, Decision::Accepted);
                let steps = drive(&mut b, &lan(&[1]), &mut script, &loose);
                let removes: Vec<RelPath> = steps.iter().filter_map(|s| match s { HostStep::Remove { path, .. } => Some(path.clone()), _ => None }).collect();
                for (i, r) in removes.iter().enumerate() {
                    for desc in removes.iter().filter(|o| r.is_ancestor_of(o)) {
                        let j = removes.iter().position(|x| x == desc).unwrap();
                        prop_assert!(j < i, "{desc} must be removed before {r}");
                    }
                }
                prop_assert_eq!(b.index().tracked_count(), 0);
                prop_assert!(b.wants().is_empty());
            }
        }

        /// §7.5: a fetch with progress stays alive; the first 60 s gap expires
        /// it at exactly the tick after the deadline, and the want is fetched
        /// again from the same, un-excluded source.
        #[test]
        fn a_stalled_fetch_returns_within_one_tick(
            gaps in prop::collection::vec(1i64..90, 1..8),
        ) {
            let (mut a, mut b) = a_and_b(1, Rules::default());
            a.scanned(t(10.0), p("n"), file(7, 7));
            let batch = a.form_batches(t(12.0), bid(3)).remove(0);
            b.receive(t(12.0), &batch);
            let v = b.wants().get(&p("n")).unwrap().version().clone();
            let (steps, _) = b.dispatch(t(12.0), &lan(&[1]));
            prop_assert_eq!(steps.len(), 1);
            let mut now = t(12.0);
            let mut expired_at = None;
            for gap in gaps {
                let before = now;
                now = now.plus_nanos(gap * NANOS_PER_SECOND);
                let overdue = b.expire(now);
                if gap >= 60 {
                    prop_assert_eq!(overdue, vec![p("n")]);
                    prop_assert_eq!(b.wants().get(&p("n")).unwrap().state, WantState::Wanted);
                    prop_assert!(b.expire(before.plus_nanos(60 * NANOS_PER_SECOND - 1)).is_empty());
                    expired_at = Some(now);
                    break;
                }
                prop_assert!(overdue.is_empty(), "progress every {gap} s keeps it alive");
                b.progress(now, &p("n"), &v);
            }
            if let Some(now) = expired_at {
                let (steps, _) = b.dispatch(now, &lan(&[1]));
                let retried = matches!(&steps[0], HostStep::Fetch { from, .. } if *from == node(1));
                prop_assert!(retried, "retried from the same source, got {:?}", steps[0]);
                prop_assert!(b.wants().get(&p("n")).unwrap().excluded.is_empty());
            }
        }
    }
}
