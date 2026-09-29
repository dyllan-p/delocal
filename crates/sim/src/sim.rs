//! The host and the driver (DESIGN.md §14.1).
//!
//! `Sim` owns the virtual clock, one seeded PRNG, the nodes (engine,
//! filesystem, trash, persisted store), the network (links with tier and
//! connectivity, messages in transit with delivery times), the host
//! operations in progress (fetches and commits), and the simulated user's
//! pending actions. It answers exactly the contract the engine defines:
//! `WakeAt` becomes a `Tick` with a fresh id, `Send` becomes a message,
//! `Fetch` becomes a transfer that completes with `Fetched`, `Write`,
//! `Remove` and `SetMeta` become commits that complete with `Applied`, and
//! every persistence hook is written to its table in the persisted store
//! (§11), which after every event must equal the engine's parts. A commit
//! that displaces a file goes through the host's commit journal (§7.5).
//!
//! Time is discrete-event: the loop always jumps to the earliest pending
//! thing (a wake, a delivery, an operation, a scan, a restart, a user
//! action) and processes it in a fixed order, so a run is a pure function
//! of the seed, the knobs and the steps.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use delocal_engine::batch::BatchRole;
use delocal_engine::folder::{ApplyOutcome, Displace, FolderStatus, ScanState, SkipReason};
use delocal_engine::want::{FetchReport, LocalError, Tier, Want, WantState};
use delocal_engine::{
    Action, BatchId, ContentHash, Deferred, Engine, Entry, Event, FolderId, FolderParts,
    FolderState, HeldRow, HeldState, HostName, IndexRecord, Kind, NodeConfig, NodeId, Observed,
    Outbound, Pending, RelPath, Rest, Rules, Timestamp, Version,
};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::Serialize;

use crate::invariants;
use crate::knobs::Knobs;
use crate::steps::{Step, UserAction, dir_name, path_name};

const NANOS: i64 = 1_000_000_000;
/// The final drain gives up after this much virtual time without quiescence.
const FINAL_ROUND_NANOS: i64 = 30 * 60 * NANOS;
const FINAL_ROUNDS: usize = 12;
/// Events processed in one `run_until` before the run is declared livelocked.
const EVENT_BUDGET: usize = 400_000;
/// The PRNG stream, beside the main one (stream 0), that the crash between
/// a commit's two renames draws from (§7.5). A model added after the main
/// stream's draws were fixed takes a stream of its own, so the main stream
/// makes the same draws in the same order whether the model is on or off,
/// and a seed's history changes only where the model does something.
const JOURNAL_STREAM: u64 = 1;
/// The PRNG stream group commit draws its lags from (§11).
const GROUP_STREAM: u64 = 2;
/// The PRNG stream the skip model draws from: which paths a scan cannot
/// inspect, why, and which tracked paths become ignored (§7.3).
const SKIP_STREAM: u64 = 3;
/// The PRNG stream the local failures draw from (§7.5): which fetches and
/// commits fail with an I/O error, whether a failed rename takes its temp
/// file with it, and when a disk fills and for how long.
const FAIL_STREAM: u64 = 4;
/// The PRNG stream that spells file names in upper case, for the
/// case-insensitive node (§7.6).
const CASE_STREAM: u64 = 5;
/// How often a host whose folder's inbound is paused checks for free
/// space (§7.5).
const SPACE_CHECK_NANOS: i64 = 30 * NANOS;

/// What a clean run reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub nodes: usize,
    pub steps_applied: usize,
    pub virtual_secs: i64,
    /// blake3 over the final engines, filesystems, trashes and action log.
    pub fingerprint: Vec<u8>,
    pub stats: Stats,
}

/// Counters worth printing so a run that finds nothing can be judged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub batches_sent: u64,
    pub held: u64,
    pub paused: u64,
    pub approvals: u64,
    pub denials: u64,
    pub reverts: u64,
    pub crashes: u64,
    pub fetches: u64,
    pub not_available: u64,
    pub mismatches: u64,
    pub changed_underneath: u64,
    pub stalled: u64,
    pub conflict_copies: u64,
    pub events: u64,
    /// Crashes between a commit's displacement and its rename (§7.5).
    pub crashes_between_renames: u64,
    /// Displacements the commit journal undid: at a restart, or because the
    /// rename after them failed (§7.5).
    pub displacements_undone: u64,
    /// Crashes that lost an open group of writes (§11).
    pub groups_lost: u64,
    /// Effects lost with those groups, never performed.
    pub effects_lost: u64,
    /// Non-empty directories displaced with their children (§7.6).
    pub subtrees_displaced: u64,
    /// Paths scans reported `Skipped`, ignored ones included (§7.3).
    pub skipped: u64,
    /// Tracked paths that became ignored for a while (§7.3).
    pub ignores: u64,
    /// Fetches and commits that failed with an I/O error (§7.5).
    pub io_failures: u64,
    /// Times a disk filled up.
    pub disk_fills: u64,
    /// Fetches and commits that failed with `DiskFull`.
    pub disk_full_reports: u64,
    /// `SpaceRecovered` reports.
    pub spaces_recovered: u64,
    /// File names a step spelled in upper case (§7.6).
    pub upper_spellings: u64,
    /// Commits the case-insensitive node's host did not attempt because of
    /// a case collision.
    pub case_collisions: u64,
    /// Temp files a failed rename took with it.
    pub temps_lost: u64,
}

/// Why a run failed, with everything needed to reproduce and read it.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub seed: u64,
    pub knobs: Knobs,
    pub steps: Vec<Step>,
    pub invariant: String,
    pub detail: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "FAILED {}: {}", self.invariant, self.detail)?;
        writeln!(f, "  seed:  {}", self.seed)?;
        writeln!(f, "  knobs: {}", self.knobs)?;
        writeln!(f, "  steps ({}):", self.steps.len())?;
        for step in &self.steps {
            writeln!(f, "    {step:?},")?;
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}

/// One entry of a node's simulated filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct File {
    pub kind: Kind,
    /// File content, or the symlink target; empty for a directory.
    pub content: Vec<u8>,
    pub mtime_ns: i64,
    pub exec: bool,
}

impl File {
    fn hash(&self) -> ContentHash {
        if self.kind == Kind::Dir {
            ContentHash::EMPTY
        } else {
            hash_bytes(&self.content)
        }
    }

    fn observed(&self) -> Observed {
        Observed {
            kind: self.kind,
            size: self.content.len() as u64,
            mtime_ns: if self.kind == Kind::File {
                self.mtime_ns
            } else {
                0
            },
            exec: self.exec && self.kind == Kind::File,
            hash: self.hash(),
        }
    }
}

/// blake3 of some bytes as the engine's hash type.
pub fn hash_bytes(bytes: &[u8]) -> ContentHash {
    ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
}

/// The bytes a content seed expands to: sizes 10 to 90, so the §6.5 tier
/// limits the simulation uses (30 relay, 60 direct) bite.
pub fn content_bytes(seed: u8) -> Vec<u8> {
    let len = 10 + (usize::from(seed) % 5) * 20;
    vec![seed; len]
}

/// What the daemon would have on disk for one node (§11): the folder's
/// rules, which the host keeps itself, and one table per engine part,
/// written by the persistence hooks and by nothing else. A crashed node
/// restarts from these and nothing else.
#[derive(Clone, Debug, Default, Serialize)]
struct Persisted {
    rules: Rules,
    records: BTreeMap<RelPath, IndexRecord>,
    wants: BTreeMap<RelPath, Want>,
    pending: BTreeMap<RelPath, Pending>,
    held: BTreeMap<(BatchId, HeldState), HeldRow>,
    deferred: BTreeMap<RelPath, Vec<Deferred>>,
    /// `None` until the folder reports its first row, at `FolderJoined`.
    rest: Option<Rest>,
}

impl Persisted {
    /// The tables as the parts a restart hands the engine; `None` if the
    /// folder never reported its rest.
    fn parts(&self, id: FolderId, members: &[NodeId]) -> Option<FolderParts> {
        Some(FolderParts {
            id,
            rules: self.rules.clone(),
            members: members.iter().copied().collect(),
            records: self.records.clone(),
            wants: self.wants.clone(),
            pending: self.pending.clone(),
            held: self.held.clone(),
            deferred: self.deferred.clone(),
            rest: self.rest.clone()?,
        })
    }

    /// The first table that differs from the engine's part, if any.
    fn mismatch(&self, state: &FolderState) -> Option<String> {
        let live = state.parts();
        let rows = |hook: &str, part: &str, stored: usize, engine: usize| {
            format!(
                "{hook} did not reproduce {part}; {stored} rows stored vs {engine} in the engine"
            )
        };
        if live.records != self.records {
            return Some(rows(
                "IndexChanged",
                "the index",
                self.records.len(),
                live.records.len(),
            ));
        }
        if live.wants != self.wants {
            return Some(rows(
                "WantChanged",
                "the want-list",
                self.wants.len(),
                live.wants.len(),
            ));
        }
        if live.pending != self.pending {
            return Some(rows(
                "PendingChanged",
                "the pending set",
                self.pending.len(),
                live.pending.len(),
            ));
        }
        if live.held != self.held {
            return Some(rows(
                "HeldChanged",
                "the held items",
                self.held.len(),
                live.held.len(),
            ));
        }
        if live.deferred != self.deferred {
            return Some(rows(
                "DeferredChanged",
                "the deferred paths",
                self.deferred.len(),
                live.deferred.len(),
            ));
        }
        if self.rest.as_ref() != Some(&live.rest) {
            return Some("RestChanged did not reproduce the small rest".to_owned());
        }
        if live.rules != self.rules {
            return Some("the rules the host stored are not the engine's".to_owned());
        }
        None
    }
}

#[derive(Serialize)]
struct Node {
    id: NodeId,
    host: HostName,
    skew_ns: i64,
    #[serde(skip)]
    engine: Option<Engine>,
    /// The folder as it stood when the node crashed, for the restart check
    /// (§11); `None` while the node runs.
    #[serde(skip)]
    crashed: Option<FolderState>,
    /// Writes not yet durable and the effects waiting for them (§11).
    #[serde(skip)]
    group: Option<Group>,
    /// With a group-commit lag, the folder as it stood when its last group
    /// became durable: what a crash now would restart into (§11).
    #[serde(skip)]
    durable: Option<FolderState>,
    online: bool,
    restart_at: Option<Timestamp>,
    fs: BTreeMap<RelPath, File>,
    trash: Vec<ContentHash>,
    persisted: Persisted,
    wake_at: Option<Timestamp>,
    next_scan_at: Timestamp,
    /// Fetched, verified content waiting for its commit: per path, one
    /// temp file for each version fetched there. A real host names each
    /// temp file after the content's hash with a random suffix (§7.5 step
    /// 3), so no fetch ever overwrites another's, a late one for an older
    /// version included. Versions have no order, so a list.
    temp: BTreeMap<RelPath, Vec<(Version, Vec<u8>)>>,
    /// The host's commit journal (§7.5, §11): one row per commit whose
    /// displacement has happened and whose rename has not. Durable, so a
    /// row a crash left open is still here at the restart.
    journal: Vec<JournalRow>,
    /// Paths a rule in `.delocalignore` ignores, with everything beneath
    /// them, and how many more of this node's full scans the rule lasts
    /// (§7.3). On disk, so a restart keeps them.
    ignored: BTreeMap<RelPath, u32>,
    /// The disk has no free space until then (§7.5).
    full_until: Option<Timestamp>,
    /// When the host next checks for free space, while the folder's inbound
    /// is paused (§7.5).
    space_check_at: Option<Timestamp>,
    /// The filesystem ignores case (§7.6): two names that differ only by
    /// case are one file.
    case_insensitive: bool,
    /// The engine has paused the folder's inbound on a full disk, as the
    /// host learned it (see `Sim::note_pause`), and the host has not yet
    /// reported `SpaceRecovered`: the engine must start nothing inbound
    /// meanwhile (§7.5).
    paused_inbound: bool,
    // ---- invariant tracking ----
    /// Every version this node has ever sent in a batch, per path.
    sent: BTreeMap<RelPath, Vec<Version>>,
    /// Content this node adopted through sync, with the path and when (I2).
    /// An entry follows its content when sync moves it to a conflict copy.
    synced: Vec<(ContentHash, RelPath, Timestamp)>,
    /// Every move sync made on this node of a file to a conflict-copy path
    /// or back, in order: content, from, to and when. An adoption joins
    /// `synced` only once its write is durable (§11), which may be after a
    /// commit already moved the content on, so it replays the moves made
    /// since its write.
    moves: Vec<(ContentHash, RelPath, RelPath, Timestamp)>,
    /// When this node's own user last edited or deleted each path (I2).
    local_edit_at: BTreeMap<RelPath, Timestamp>,
    /// The highest `seq` this node's index has written. Every write takes a
    /// new one except `revert`'s, which puts records back with theirs
    /// (§8.3), so an `IndexChanged` at or below it is a revert's.
    max_seq: u64,
    paused: bool,
}

impl Node {
    fn alive(&self) -> bool {
        self.engine.is_some() && self.online
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Link {
    up: bool,
    tier: Tier,
}

#[derive(Clone, Debug)]
struct Message {
    deliver_at: Timestamp,
    seq: u64,
    from: NodeId,
    to: NodeId,
    payload: Outbound,
}

#[derive(Clone, Debug)]
enum Op {
    Fetch {
        node: NodeId,
        from: NodeId,
        path: RelPath,
        version: Version,
        hash: ContentHash,
        done_at: Timestamp,
        next_progress: Timestamp,
        corrupt: bool,
    },
    Commit {
        node: NodeId,
        path: RelPath,
        version: Version,
        action: Box<Action>,
        commit: u64,
        done_at: Timestamp,
        /// It fell due while its node was offline. Offline stands for a
        /// suspended machine, whose disk operations finish when it resumes:
        /// the host reports every commit or restarts (§7.5).
        suspended: bool,
    },
}

impl Op {
    fn node(&self) -> NodeId {
        match self {
            Self::Fetch { node, .. } | Self::Commit { node, .. } => *node,
        }
    }

    fn next_at(&self) -> Timestamp {
        match self {
            Self::Fetch {
                done_at,
                next_progress,
                ..
            } => (*done_at).min(*next_progress),
            Self::Commit {
                suspended: true, ..
            } => Timestamp::from_unix_nanos(i64::MAX),
            Self::Commit { done_at, .. } => *done_at,
        }
    }
}

#[derive(Clone, Debug)]
struct UserDue {
    at: Timestamp,
    node: NodeId,
    action: UserAction,
}

/// A row of the commit journal (§7.5): what a commit moved aside from its
/// path, and where to. The host makes it durable before the displacement
/// and removes it once the rename has put the new content in place.
#[derive(Clone, Debug, Serialize)]
struct JournalRow {
    path: RelPath,
    to: Displace,
    /// Everything the displacement moved, at the path it had.
    moved: Vec<(RelPath, File)>,
    /// Where in the node's trash the moved files start, for `Displace::Trash`.
    trash_at: usize,
}

/// What one full scan reports under the skip model (§7.3).
#[derive(Default)]
struct Walk {
    /// Every path the walk found, in order, with what the scan reports for
    /// it: `None` beneath a path it skipped, whose contents it cannot see.
    reports: Vec<(RelPath, Option<ScanState>)>,
    /// Paths a rule ignores that the walk did not find on disk, reported
    /// `Skipped` after it.
    unreached: Vec<RelPath>,
    /// Skipped paths whose skip covers what lies beneath them: the
    /// directories the scan could not list, and every ignored path.
    beneath: BTreeSet<RelPath>,
}

/// What the skip checker holds one node's index writes against (§7.3).
struct SkipCheck {
    node: NodeId,
    /// The paths reported `Skipped` so far in the bracket, or the one path
    /// the watcher reported.
    skipped: BTreeSet<RelPath>,
    /// Those whose skip covers what lies beneath them (see [`Walk`]).
    beneath: BTreeSet<RelPath>,
    /// As the watched event began: the paths with a live record, each with
    /// this node's own counter in its version, and the index's `seq`, so a
    /// tombstone written above it is one the event wrote.
    live: BTreeMap<RelPath, u64>,
    seq: u64,
}

impl SkipCheck {
    fn new(node: NodeId) -> Self {
        Self {
            node,
            skipped: BTreeSet::new(),
            beneath: BTreeSet::new(),
            live: BTreeMap::new(),
            seq: 0,
        }
    }

    /// True if a skip covers `path`: it was reported `Skipped`, or lies
    /// beneath a skip that covers what is beneath it.
    fn covers(&self, path: &RelPath) -> bool {
        self.skipped.contains(path) || self.skipped_above(path).is_some()
    }

    /// The skipped path above `path` that covers it, if any.
    fn skipped_above(&self, path: &RelPath) -> Option<&RelPath> {
        self.beneath
            .iter()
            .find(|d| self.skipped.contains(*d) && d.is_ancestor_of(path))
    }
}

/// A commit an engine asked for and has not had reported (§7.5).
struct InFlight {
    /// Which of the host's commits it is, so that only its own report ends
    /// it: a commit the engine released may still report, later.
    commit: u64,
    version: Version,
    /// Its want carried the restoring mark (§8.3).
    restoring: bool,
    /// The conflict-copy path it moves the losing file to (§7.6), if any.
    copy_to: Option<RelPath>,
    /// A rule released its want early (§7.5): the commit still holds the
    /// path until its own report, and its want may be anything meanwhile.
    released: bool,
}

/// What the disk-full checker needs of the event a node is being fed
/// (§7.5): see [`Sim::starts_inbound_while_paused`].
struct Feeding {
    node: NodeId,
    /// A commit's report or a scan's observation, the two events that may
    /// put another node's version in the index while the inbound is
    /// paused: the report of a commit asked for before the pause, and a
    /// landing an observation finds (§8.3).
    may_adopt: bool,
    /// The index's `seq` as the event began; a write above it is new.
    seq: u64,
}

/// True if a rule on `node` ignores `path`: a rule for the path itself, or
/// for a directory above it (§7.3).
fn ignores(node: &Node, path: &RelPath) -> bool {
    node.ignored
        .keys()
        .any(|r| r == path || r.is_ancestor_of(path))
}

/// A group of writes on its way to durability, and the effects that wait
/// for it (§11 group commit).
struct Group {
    /// Table writes in the order they were made.
    writes: Vec<Staged>,
    /// Effects in the order the engine asked for them.
    effects: Vec<Held>,
    /// Events still to join the group before it becomes durable.
    left: u32,
}

/// A table write with the clock and event count it was made at, which the
/// invariant tracking records when the write becomes durable.
struct Staged {
    write: TableWrite,
    at: Timestamp,
    event: u64,
    /// How many moves sync had made on the node when the write was made
    /// (see `Node::moves`).
    moves: usize,
}

/// A write to the persisted store (§11).
enum TableWrite {
    /// A persistence hook's row.
    Hook(Box<Action>),
    /// The folder's rules, which the host keeps itself.
    Rules(Rules),
}

/// An effect: something that reaches the disk or the network (§11).
enum Held {
    Send {
        to: NodeId,
        payload: Outbound,
    },
    Fetch {
        path: RelPath,
        version: Version,
        hash: ContentHash,
        size: u64,
        from: NodeId,
    },
    /// `Write`, `Remove` or `SetMeta`, with the version of the want it
    /// commits as the engine asked for it, and its number.
    Commit {
        path: RelPath,
        version: Version,
        action: Box<Action>,
        commit: u64,
    },
    MoveToTrash {
        path: RelPath,
    },
}

/// A persistence hook: an action the host writes to its tables (§11).
fn is_table_write(action: &Action) -> bool {
    matches!(
        action,
        Action::IndexChanged { .. }
            | Action::IndexRemoved { .. }
            | Action::WantChanged { .. }
            | Action::PendingChanged { .. }
            | Action::HeldChanged { .. }
            | Action::DeferredChanged { .. }
            | Action::RestChanged { .. }
    )
}

/// What the host did with a commit.
enum Committed {
    /// Report this outcome to the engine.
    Report(ApplyOutcome),
    /// The node crashed after the displacement and before the rename, with
    /// the journal row still open (§7.5).
    CrashedBetweenRenames,
    /// A `Write` found no temp file of its version: the engine asked to
    /// commit content it did not have.
    NoTemp,
}

/// The whole simulated world. See the module docs.
pub struct Sim {
    seed: u64,
    knobs: Knobs,
    rng: ChaCha8Rng,
    /// Draws for the crash between two renames, on `JOURNAL_STREAM`.
    journal_rng: ChaCha8Rng,
    /// Draws for group commit's lags, on `GROUP_STREAM`.
    group_rng: ChaCha8Rng,
    /// Draws for the skip model, on `SKIP_STREAM`.
    skip_rng: ChaCha8Rng,
    /// Draws for the local failures, on `FAIL_STREAM`.
    fail_rng: ChaCha8Rng,
    /// Draws for upper-case spellings, on `CASE_STREAM`.
    case_rng: ChaCha8Rng,
    /// While a node is fed an event: which node, what kind of event, and
    /// its index's `seq` as the event began, for the disk-full checker.
    feeding: Option<Feeding>,
    /// While a node is fed a `Skipped` report or the end of a bracket that
    /// had one, what the skip checker holds its index writes against.
    checking: Option<SkipCheck>,
    /// Every commit an engine asked for whose report it has not been fed,
    /// by node and path: what the in-flight checker holds the engine to
    /// (§7.5).
    committing: BTreeMap<(NodeId, RelPath), InFlight>,
    /// The number the next commit a host is asked for gets.
    next_commit: u64,
    /// While a commit's report is fed, the conflict-copy path it displaced
    /// the losing file to, if it landed and did (§7.6).
    copy_recorded: Option<(NodeId, RelPath)>,
    /// While an observation is fed, the node and path it observes.
    observing: Option<(NodeId, RelPath)>,
    clock: Timestamp,
    folder: FolderId,
    rules: Rules,
    nodes: BTreeMap<NodeId, Node>,
    /// Node ids in creation order, for `Step` indices.
    order: Vec<NodeId>,
    /// When (in events) each node last reverted each path (§8.3), and the
    /// record the revert put back there (`None` if it removed the record). A
    /// revert discards every version the node wrote at the path since its
    /// last announcement, which is every version of the node's that
    /// dominates the restored one; nobody else ever saw those records, and
    /// their vectors can be reached again, by the node's next change or by a
    /// merge that includes its re-issued counter (I3, I4, I7). The restored
    /// record itself, and everything before it, was announced and stands.
    reverted_at: BTreeMap<(NodeId, RelPath), (u64, Option<Version>)>,
    /// When (in events) each version at each path was first seen, for the
    /// revert exemption above.
    seen_at: BTreeMap<RelPath, Vec<(Version, u64)>>,
    links: BTreeMap<(NodeId, NodeId), Link>,
    messages: Vec<Message>,
    msg_seq: u64,
    /// Batches each node's user approved (I5): their entries may be applied
    /// however late they reach the want-list, even if leftovers of the same
    /// batch are held again under its id.
    approved: BTreeSet<(NodeId, BatchId)>,
    ops: Vec<Op>,
    users: Vec<UserDue>,
    corruption_on: bool,
    /// False in the final phase, which skips nothing and ignores nothing.
    skips_on: bool,
    /// False in the final phase, in which no fetch or commit fails and no
    /// disk fills.
    failures_on: bool,
    steps: Vec<Step>,
    steps_applied: usize,
    stats: Stats,
    /// Every content hash that was ever in a batch some node sent (I2).
    announced: BTreeSet<ContentHash>,
    /// Every distinct version ever recorded at each path on any node (I3, I4).
    versions: BTreeMap<RelPath, Vec<Entry>>,
    /// Records a revert discarded whose vector was later reached again and
    /// replaced them in `versions` (§8.3). They lost conflicts before they
    /// were discarded, and a copy made from one keeps its name, so I4 still
    /// counts them as losing versions.
    superseded: BTreeMap<RelPath, Vec<Entry>>,
    log: blake3::Hasher,
}

impl Sim {
    /// Build the world for `seed`: nodes, folder, links, connections.
    pub fn new(seed: u64, knobs: Knobs) -> Self {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let n = knobs.nodes.unwrap_or_else(|| rng.random_range(2u8..=8)) as usize;
        let folder = FolderId::from_bytes(rng.random());
        let rules = Rules {
            hold_count: 3,
            hold_pct: 25,
            hold_size: 1_000_000,
            direct_limit: 60,
            relay_limit: 30,
            max_fetches_per_peer: 2,
            max_fetches_per_folder: 4,
        };
        let clock = Timestamp::from_unix_nanos(1_700_000_000 * NANOS);
        let mut nodes = BTreeMap::new();
        let mut order = Vec::new();
        for i in 0..n {
            let id = NodeId::from_bytes(rng.random());
            let host = HostName::new(format!("n{i}")).unwrap_or_else(|_| HostName::empty());
            let skew_ns = rng.random_range(-3 * NANOS..=3 * NANOS);
            let node = Node {
                id,
                host,
                skew_ns,
                engine: None,
                crashed: None,
                group: None,
                durable: None,
                online: true,
                restart_at: None,
                fs: BTreeMap::new(),
                trash: Vec::new(),
                persisted: Persisted::default(),
                wake_at: None,
                next_scan_at: clock.plus_nanos(rng.random_range(30 * NANOS..600 * NANOS)),
                temp: BTreeMap::new(),
                journal: Vec::new(),
                ignored: BTreeMap::new(),
                full_until: None,
                space_check_at: None,
                case_insensitive: i == 0 && knobs.case_variants > 0.0,
                paused_inbound: false,
                sent: BTreeMap::new(),
                synced: Vec::new(),
                moves: Vec::new(),
                local_edit_at: BTreeMap::new(),
                max_seq: 0,
                paused: false,
            };
            nodes.insert(id, node);
            order.push(id);
        }
        let mut links = BTreeMap::new();
        for (i, a) in order.iter().enumerate() {
            for b in &order[i + 1..] {
                let tier = match rng.random_range(0u8..3) {
                    0 => Tier::Lan,
                    1 => Tier::Direct,
                    _ => Tier::Relay,
                };
                links.insert(link_key(*a, *b), Link { up: true, tier });
            }
        }
        let mut journal_rng = ChaCha8Rng::seed_from_u64(seed);
        journal_rng.set_stream(JOURNAL_STREAM);
        let mut group_rng = ChaCha8Rng::seed_from_u64(seed);
        group_rng.set_stream(GROUP_STREAM);
        let mut skip_rng = ChaCha8Rng::seed_from_u64(seed);
        skip_rng.set_stream(SKIP_STREAM);
        let mut fail_rng = ChaCha8Rng::seed_from_u64(seed);
        fail_rng.set_stream(FAIL_STREAM);
        let mut case_rng = ChaCha8Rng::seed_from_u64(seed);
        case_rng.set_stream(CASE_STREAM);
        let mut sim = Self {
            seed,
            knobs,
            rng,
            journal_rng,
            group_rng,
            skip_rng,
            fail_rng,
            case_rng,
            feeding: None,
            checking: None,
            committing: BTreeMap::new(),
            next_commit: 0,
            copy_recorded: None,
            observing: None,
            clock,
            folder,
            rules,
            nodes,
            order,
            links,
            messages: Vec::new(),
            msg_seq: 0,
            reverted_at: BTreeMap::new(),
            seen_at: BTreeMap::new(),
            approved: BTreeSet::new(),
            ops: Vec::new(),
            users: Vec::new(),
            corruption_on: true,
            skips_on: true,
            failures_on: true,
            steps: Vec::new(),
            steps_applied: 0,
            stats: Stats::default(),
            announced: BTreeSet::new(),
            versions: BTreeMap::new(),
            superseded: BTreeMap::new(),
            log: blake3::Hasher::new(),
        };
        // Engines join the folder, then every pair connects.
        let ids = sim.order.clone();
        for id in &ids {
            let engine = Engine::new(sim.config_of(*id));
            if let Some(node) = sim.nodes.get_mut(id) {
                node.engine = Some(engine);
                node.persisted.rules = sim.rules.clone();
            }
            // Ignore failures during construction: nothing has happened yet.
            let _ = sim.feed(
                *id,
                Event::FolderJoined {
                    folder,
                    rules: sim.rules.clone(),
                    members: ids.clone(),
                },
            );
            // Joining a folder is durable before the daemon does anything
            // else, or a crash could leave it with no rest row at all.
            let _ = sim.flush(*id);
        }
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                let _ = sim.connect(*a, *b);
            }
        }
        sim
    }

    fn config_of(&self, id: NodeId) -> NodeConfig {
        NodeConfig {
            node_id: id,
            author_host: self
                .nodes
                .get(&id)
                .map(|n| n.host.clone())
                .unwrap_or_else(HostName::empty),
        }
    }

    fn now_for(&self, id: NodeId) -> Timestamp {
        let skew = self.nodes.get(&id).map_or(0, |n| n.skew_ns);
        self.clock.plus_nanos(skew)
    }

    fn node_at(&self, index: u8) -> NodeId {
        self.order[usize::from(index) % self.order.len()]
    }

    fn fail(&self, invariant: &str, detail: String) -> Failure {
        Failure {
            seed: self.seed,
            knobs: self.knobs.clone(),
            steps: self.steps.clone(),
            invariant: invariant.to_owned(),
            detail,
        }
    }

    fn short(id: NodeId) -> String {
        id.short().to_string()
    }

    // ---------------------------------------------------------------- driver

    /// Apply every step with a settle after each, run the final phase, and
    /// check the invariants.
    pub fn run(&mut self, steps: &[Step]) -> Result<Outcome, Failure> {
        self.steps = steps.to_vec();
        for step in steps {
            self.apply(step)?;
            self.steps_applied += 1;
            let settle = self.rng.random_range(NANOS..30 * NANOS);
            let end = self.clock.plus_nanos(settle);
            self.run_until(end)?;
        }
        self.final_phase()?;
        invariants::check_all(self)?;
        Ok(Outcome {
            nodes: self.order.len(),
            steps_applied: self.steps_applied,
            virtual_secs: (self.clock.as_unix_nanos() - 1_700_000_000 * NANOS) / NANOS,
            fingerprint: self.fingerprint(),
            stats: self.stats.clone(),
        })
    }

    /// Heal everything, approve everything, scan everything, drain.
    fn final_phase(&mut self) -> Result<(), Failure> {
        self.corruption_on = false;
        // No fetch or commit fails from now on, and every disk has room
        // again; a paused inbound resumes at its host's next check (§7.5).
        self.failures_on = false;
        for node in self.nodes.values_mut() {
            node.full_until = None;
        }
        // Every ignore rule goes, and from now on every scan can look
        // everywhere, so the final scans observe everything (§7.3).
        self.skips_on = false;
        for node in self.nodes.values_mut() {
            node.ignored.clear();
        }
        let ids = self.order.clone();
        for id in &ids {
            if self.nodes.get(id).is_some_and(|n| n.restart_at.is_some()) {
                self.restart(*id)?;
            }
            if self.nodes.get(id).is_some_and(|n| !n.online) {
                self.set_online(*id, true)?;
            }
        }
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                self.set_link(*a, *b, true)?;
                self.set_tier(*a, *b, Tier::Lan)?;
            }
        }
        self.users.clear();
        for round in 0..FINAL_ROUNDS {
            for id in &ids {
                self.user_act(*id, UserAction::ApproveAll)?;
            }
            self.resolve_case_pairs()?;
            for id in &ids {
                self.full_scan(*id, false)?;
            }
            let end = self.clock.plus_nanos(FINAL_ROUND_NANOS);
            self.run_until(end)?;
            if self.quiescent() {
                // An idle daemon commits what it has written. Only writes
                // are left (quiescence holds no effects), so this changes
                // nothing but the tables and what the invariants track.
                for id in &ids {
                    self.flush(*id)?;
                }
                return Ok(());
            }
            if round + 1 == FINAL_ROUNDS {
                return Err(self.fail("quiescence", self.describe_unquiet()));
            }
        }
        Ok(())
    }

    /// Nothing left to do anywhere (§14.1).
    fn quiescent(&self) -> bool {
        if !self.messages.is_empty() || !self.ops.is_empty() || !self.users.is_empty() {
            return false;
        }
        self.nodes.values().all(|n| {
            let Some(engine) = &n.engine else {
                return false;
            };
            let Some(f) = engine.folder(self.folder) else {
                return false;
            };
            n.online
                && n.restart_at.is_none()
                && n.group.as_ref().is_none_or(|g| g.effects.is_empty())
                && !f.disk_full()
                && f.window().is_none()
                && f.due().is_none()
                && f.quarantine().is_empty()
                && f.paused().is_none()
                && f.deferred().next().is_none()
                && f.wants()
                    .iter()
                    .all(|w| matches!(w.state, WantState::GaveUp | WantState::NoSource))
        })
    }

    fn describe_unquiet(&self) -> String {
        let mut out = format!(
            "{} messages, {} ops, {} user actions pending;",
            self.messages.len(),
            self.ops.len(),
            self.users.len()
        );
        for n in self.nodes.values() {
            let Some(engine) = &n.engine else {
                out.push_str(&format!(" {} crashed;", Self::short(n.id)));
                continue;
            };
            let Some(f) = engine.folder(self.folder) else {
                continue;
            };
            let wants: Vec<String> = f
                .wants()
                .iter()
                .filter(|w| !matches!(w.state, WantState::GaveUp | WantState::NoSource))
                .map(|w| format!("{}={:?}", w.path(), w.state))
                .collect();
            if !wants.is_empty()
                || f.window().is_some()
                || !f.quarantine().is_empty()
                || f.paused().is_some()
                || f.disk_full()
                || f.deferred().next().is_some()
            {
                let deferred: Vec<String> = f
                    .deferred()
                    .map(|d| {
                        format!(
                            "{}@{:?} {:?}{} (index: {:?})",
                            d.entry.path,
                            d.entry.version,
                            d.reason,
                            if d.entry.deleted { " tombstone" } else { "" },
                            f.index()
                                .get(&d.entry.path)
                                .map(|r| (r.entry.version.clone(), r.entry.deleted))
                        )
                    })
                    .collect();
                out.push_str(&format!(
                    " {}: window={:?} held={} paused={} disk_full={} deferred=[{}] wants=[{}];",
                    Self::short(n.id),
                    f.window().map(|w| w.due()),
                    f.quarantine().len(),
                    f.paused().is_some(),
                    f.disk_full(),
                    deferred.join(", "),
                    wants.join(", ")
                ));
            }
        }
        out
    }

    /// The discrete-event loop up to `end`.
    fn run_until(&mut self, end: Timestamp) -> Result<(), Failure> {
        let mut budget = EVENT_BUDGET;
        loop {
            let Some(next) = self.next_event_at() else {
                self.clock = end;
                return Ok(());
            };
            if next > end {
                self.clock = end;
                return Ok(());
            }
            if next < self.clock {
                return Err(self.fail(
                    "clock",
                    format!(
                        "an event at {next:?} was queued after the clock reached {:?}; {}",
                        self.clock,
                        self.describe_unquiet()
                    ),
                ));
            }
            self.clock = next;
            budget -= 1;
            if budget == 0 {
                return Err(self.fail(
                    "livelock",
                    format!(
                        "{EVENT_BUDGET} events without reaching {end:?}; {}",
                        self.describe_unquiet()
                    ),
                ));
            }
            self.stats.events += 1;
            self.process_one_due()?;
        }
    }

    fn next_event_at(&self) -> Option<Timestamp> {
        let mut best: Option<Timestamp> = None;
        let mut consider = |t: Option<Timestamp>| {
            if let Some(t) = t {
                best = Some(best.map_or(t, |b| b.min(t)));
            }
        };
        for n in self.nodes.values() {
            consider(n.restart_at);
            if n.alive() {
                consider(n.wake_at);
                consider(Some(n.next_scan_at));
                consider(n.space_check_at);
            }
        }
        consider(self.messages.iter().map(|m| m.deliver_at).min());
        consider(self.ops.iter().map(Op::next_at).min());
        consider(self.users.iter().map(|u| u.at).min());
        best
    }

    /// Process exactly one thing due at the clock, in a fixed order.
    fn process_one_due(&mut self) -> Result<(), Failure> {
        let now = self.clock;
        // 1. restarts
        if let Some(id) = self
            .nodes
            .values()
            .find(|n| n.restart_at.is_some_and(|t| t <= now))
            .map(|n| n.id)
        {
            return self.restart(id);
        }
        // 2. wakes
        if let Some(id) = self
            .nodes
            .values()
            .find(|n| n.alive() && n.wake_at.is_some_and(|t| t <= now))
            .map(|n| n.id)
        {
            if let Some(n) = self.nodes.get_mut(&id) {
                n.wake_at = None;
            }
            let fresh = BatchId::from_bytes(self.rng.random());
            return self.feed(
                id,
                Event::Tick {
                    fresh_batch_id: fresh,
                },
            );
        }
        // 3. messages, by (deliver_at, seq)
        if let Some(pos) = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.deliver_at <= now)
            .min_by_key(|(_, m)| (m.deliver_at, m.seq))
            .map(|(i, _)| i)
        {
            let msg = self.messages.remove(pos);
            return self.deliver(msg);
        }
        // 4. host operations
        if let Some(pos) = self.ops.iter().position(|op| op.next_at() <= now) {
            return self.progress_op(pos);
        }
        // 4b. checks for free space (§7.5)
        if let Some(id) = self
            .nodes
            .values()
            .find(|n| n.alive() && n.space_check_at.is_some_and(|t| t <= now))
            .map(|n| n.id)
        {
            return self.check_space(id);
        }
        // 5. scans
        if let Some(id) = self
            .nodes
            .values()
            .find(|n| n.alive() && n.next_scan_at <= now)
            .map(|n| n.id)
        {
            return self.full_scan(id, true);
        }
        // 6. user actions
        if let Some(pos) = self.users.iter().position(|u| u.at <= now) {
            let due = self.users.remove(pos);
            return self.user_act(due.node, due.action);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- engine I/O

    /// Hand an event to a node's engine and carry out its actions.
    ///
    /// Writes and effects follow §11's group commit. An event that writes
    /// anything joins the node's open group of writes, or opens one, and the
    /// group becomes durable a drawn number of events later (when the event
    /// ends, with `--group-commit-lag 0`). The effects of every event in the
    /// group, file operations and sends, wait for it and are performed in
    /// order once it is durable. An event that writes nothing while no group
    /// is open depends on nothing unwritten, and its effects happen at once.
    fn feed(&mut self, id: NodeId, event: Event) -> Result<(), Failure> {
        let now = self.now_for(id);
        let what = event_name(&event);
        let observing = match &event {
            Event::Scanned { path, .. } => Some((id, path.clone())),
            _ => None,
        };
        let folder = self.folder;
        let may_adopt = matches!(event, Event::Applied { .. } | Event::Scanned { .. });
        let (actions, seq) = {
            let Some(node) = self.nodes.get_mut(&id) else {
                return Ok(());
            };
            let Some(engine) = node.engine.as_mut() else {
                return Ok(());
            };
            let seq = engine.folder(folder).map_or(0, |f| f.index().seq());
            (engine.handle(now, event), seq)
        };
        if let Ok(bytes) = postcard::to_stdvec(&actions) {
            self.log.update(&bytes);
        }
        // Opened before any action is carried out: an event's effects wait
        // for its writes even when the engine lists the effect first.
        if actions.iter().any(is_table_write) {
            self.open_group(id);
        }
        self.note_pause(id);
        let outer = std::mem::replace(&mut self.observing, observing);
        let outer_feeding = self.feeding.replace(Feeding {
            node: id,
            may_adopt,
            seq,
        });
        let acted = actions
            .into_iter()
            .try_for_each(|action| self.act(id, action))
            .and_then(|()| self.holds_its_commits(id, &what));
        self.observing = outer;
        self.feeding = outer_feeding;
        acted?;
        self.event_done(id)
    }

    /// §7.5: at most one commit is in flight per path, and a commit holds
    /// its path until the host reports it or the process restarts. So after
    /// every event, each commit `id` asked for and has not had reported is
    /// still what its want waits for: the want is there, committing that
    /// version. Two rules may release the want early (see
    /// [`Sim::release_allowed`]), but not the path: the released commit is
    /// tracked until its own report like any other, and no second commit of
    /// the path may start meanwhile ([`Sim::commit_started`]).
    fn holds_its_commits(&mut self, id: NodeId, event: &str) -> Result<(), Failure> {
        let Some(folder) = self.engine(id).and_then(|e| e.folder(self.folder)) else {
            return Ok(());
        };
        let mut released = Vec::new();
        for ((node, path), c) in &self.committing {
            if *node != id || c.released {
                continue;
            }
            let held = folder.wants().get(path).is_some_and(|w| {
                w.version() == &c.version && matches!(w.state, WantState::Committing { .. })
            });
            if held {
                continue;
            }
            if self.release_allowed(id, path, c) {
                released.push(path.clone());
                continue;
            }
            let now = folder
                .wants()
                .get(path)
                .map(|w| format!("{:?} at {:?}", w.state, w.version()));
            return Err(self.fail(
                "commit in flight",
                format!(
                    "{} released its commit of {path} at {:?} on {event}, before the host reported it; the want is now {}",
                    Self::short(id),
                    c.version,
                    now.unwrap_or_else(|| "gone".to_owned())
                ),
            ));
        }
        for path in released {
            if let Some(c) = self.committing.get_mut(&(id, path)) {
                c.released = true;
            }
        }
        Ok(())
    }

    /// The two rules that may release the want of a commit in flight early
    /// (§7.5), which the in-flight checker lets through. They release the
    /// want, not the path:
    ///
    /// - §7.5, conflict-copy re-classification: the report of a commit that
    ///   moved the losing file to its conflict-copy path records that copy,
    ///   and the want at the copy path is re-classified against it,
    ///   releasing a commit in flight there. That commit was asked for
    ///   before the copy existed, so its guard almost always fails, and its
    ///   report is discarded whatever it says.
    /// - §7.5's revert exception, §8.3: an observation of an occupant at a
    ///   path carrying the restoring mark clears the mark and cancels the
    ///   want, whatever state it is in.
    fn release_allowed(&self, id: NodeId, path: &RelPath, c: &InFlight) -> bool {
        let at = Some((id, path.clone()));
        self.copy_recorded == at || (c.restoring && self.observing == at)
    }

    /// §7.5: the engine asks `id`'s host for a commit of `path` at
    /// `version`, which is in flight until the host reports it. Returns the
    /// commit's number. A second commit of the path while one is in flight
    /// is a failure, whether or not a rule released the first one's want.
    fn commit_started(
        &mut self,
        id: NodeId,
        path: &RelPath,
        version: &Version,
        action: &Action,
    ) -> Result<u64, Failure> {
        let key = (id, path.clone());
        if let Some(earlier) = self.committing.get(&key) {
            let detail = format!(
                "{} started a commit of {path} at {version:?} while its commit at {:?} was in flight{}",
                Self::short(id),
                earlier.version,
                if earlier.released {
                    ", released early"
                } else {
                    ""
                }
            );
            return Err(self.fail("commit in flight", detail));
        }
        let restoring = self
            .engine(id)
            .and_then(|e| e.folder(self.folder))
            .and_then(|f| f.wants().get(path))
            .is_some_and(|w| w.restoring);
        let copy_to = match action {
            Action::Write { displace, .. } | Action::Remove { displace, .. } => match displace {
                Displace::ConflictCopy(to) => Some(to.clone()),
                Displace::Trash => None,
            },
            _ => None,
        };
        let commit = self.next_commit;
        self.next_commit += 1;
        self.committing.insert(
            key,
            InFlight {
                commit,
                version: version.clone(),
                restoring,
                copy_to,
                released: false,
            },
        );
        Ok(commit)
    }

    /// Open a group of writes on `id` unless one is open (§11), and draw how
    /// many further events it waits for before it becomes durable.
    fn open_group(&mut self, id: NodeId) {
        let lag = self.knobs.group_commit_lag;
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if node.group.is_none() {
            let left = if lag == 0 {
                0
            } else {
                self.group_rng.random_range(0..=lag)
            };
            node.group = Some(Group {
                writes: Vec::new(),
                effects: Vec::new(),
                left,
            });
        }
    }

    /// After an event on `id`: the open group becomes durable if this event
    /// was its last. With no group open, nothing is waiting to be written,
    /// so the tables must already be the engine's parts.
    fn event_done(&mut self, id: NodeId) -> Result<(), Failure> {
        let durable = match self.nodes.get_mut(&id).map(|n| &mut n.group) {
            Some(Some(group)) if group.left > 0 => {
                group.left -= 1;
                return Ok(());
            }
            Some(Some(_)) => true,
            _ => false,
        };
        if durable {
            self.flush(id)
        } else {
            self.check_persisted(id)
        }
    }

    /// The open group on `id` becomes durable (§11): its writes reach the
    /// tables, which must then be the engine's parts, and the effects that
    /// waited for it are performed in order. With a lag the folder is kept
    /// as it stands now, for the check at the next restart: a crash before
    /// the next group is durable restarts from exactly these tables.
    fn flush(&mut self, id: NodeId) -> Result<(), Failure> {
        let Some(group) = self.nodes.get_mut(&id).and_then(|n| n.group.take()) else {
            return Ok(());
        };
        for staged in group.writes {
            self.record(id, staged)?;
        }
        self.check_persisted(id)?;
        if self.knobs.group_commit_lag > 0 {
            let folder = self.folder;
            if let Some(n) = self.nodes.get_mut(&id) {
                n.durable = n.engine.as_ref().and_then(|e| e.folder(folder)).cloned();
            }
        }
        for held in group.effects {
            self.perform(id, held)?;
        }
        Ok(())
    }

    /// Whenever a group becomes durable, and after every event while none is
    /// open, the tables the hooks wrote must be the engine's parts (§11): a
    /// missing or wrong hook fails at the first such point after the event
    /// that should have reported the change, not at some later restart.
    fn check_persisted(&mut self, id: NodeId) -> Result<(), Failure> {
        let folder = self.folder;
        let Some(node) = self.nodes.get_mut(&id) else {
            return Ok(());
        };
        let Some(engine) = &node.engine else {
            return Ok(());
        };
        let Some(state) = engine.folder(folder) else {
            return Ok(());
        };
        match node.persisted.mismatch(state) {
            None => Ok(()),
            Some(what) => {
                let detail = format!("{}: {what}", Self::short(id));
                Err(self.fail("persistence hooks", detail))
            }
        }
    }

    /// I5 (§14.1): no batch that tripped the brake is applied without an
    /// approve. Approve releases the item before admitting its entries, so
    /// a commit whose want still names a held batch is a hole in the brake.
    /// Versions are not the test: the merge of two sides is the same on
    /// every machine, so a version a held batch carries can also be reached
    /// through a batch that passed on its own apply set (§8.2 quarantines
    /// the incoming version, not the merge, on purpose).
    fn commits_held_item(&self, id: NodeId, action: &Action) -> Option<RelPath> {
        let path = match action {
            Action::Write { path, .. }
            | Action::Remove { path, .. }
            | Action::SetMeta { path, .. } => path,
            _ => return None,
        };
        let folder = self.engine(id)?.folder(self.folder)?;
        let want = folder.wants().get(path)?;
        if self.approved.contains(&(id, want.batch)) {
            return None;
        }
        // The item must hold this path: a batch id names a review item, and
        // entries of a batch that passed can be held later under its id
        // (§8.2 re-admission) while the accepted ones are still committing.
        folder
            .quarantine()
            .get(want.batch)
            .filter(|item| item.entries.contains_key(path))
            .map(|_| path.clone())
    }

    /// §7.3: the engine keeps the record of a path a bracket reports
    /// `Skipped` and of every tracked path beneath a directory it skipped,
    /// and a skip outside a bracket changes nothing. So while the checker
    /// watches, no index write may be a deletion this node observes, under
    /// a new `seq`, over a record that was live at a path a skip covers.
    /// An observed deletion raises the node's own counter above the live
    /// record's (§7.2); a tombstone that does not is no deletion of what
    /// was live. It is what a queued `revert` leaves when it runs at the
    /// bracket's end: it puts back the announced tombstone under its old
    /// `seq`, and the want it re-derives there merges a peer's tombstone
    /// with it, a new `seq` whose metadata, this node's own included, is
    /// the winner's.
    fn tombstones_a_skip(&self, id: NodeId, action: &Action) -> Option<String> {
        let check = self.checking.as_ref().filter(|c| c.node == id)?;
        let Action::IndexChanged { record, .. } = action else {
            return None;
        };
        let path = &record.entry.path;
        let own = check.live.get(path)?;
        if !record.entry.deleted
            || record.entry.modified_by != id
            || record.seq <= check.seq
            || record.entry.version.counter(id) <= *own
            || !check.covers(path)
        {
            return None;
        }
        Some(match check.skipped_above(path) {
            Some(dir) => format!(
                "{} tombstoned {path}, beneath {dir}, which its scan reported Skipped",
                Self::short(id)
            ),
            None => format!(
                "{} tombstoned {path}, which was reported Skipped",
                Self::short(id)
            ),
        })
    }

    /// §7.5: a released commit keeps its path in flight, and nothing may
    /// change the path's record until its report, neither a new commit
    /// ([`Sim::commit_started`]) nor an index-only adoption. The event that
    /// releases the want may still write there (the conflict copy it
    /// records, a landing an observation finds); from the next event on,
    /// any write of the record is a failure.
    fn changes_a_held_record(&self, id: NodeId, action: &Action) -> Option<String> {
        let path = match action {
            Action::IndexChanged { record, .. } => &record.entry.path,
            Action::IndexRemoved { path, .. } => path,
            _ => return None,
        };
        let held = self.committing.get(&(id, path.clone()))?;
        held.released.then(|| {
            format!(
                "{} changed the record of {path} while a released commit of {:?} held it",
                Self::short(id),
                held.version
            )
        })
    }

    /// The host learns that a full disk paused the folder's inbound from
    /// the engine's own state, as it persists it (§7.5, §11): from then on
    /// it checks for space every 30 s, and the disk-full checker holds the
    /// engine to the pause, from the actions of the event that paused it
    /// on, until the host reports space recovered.
    fn note_pause(&mut self, id: NodeId) {
        let folder = self.folder;
        let clock = self.clock;
        let Some(n) = self.nodes.get_mut(&id) else {
            return;
        };
        let paused = n
            .engine
            .as_ref()
            .and_then(|e| e.folder(folder))
            .is_some_and(FolderState::disk_full);
        if paused && !n.paused_inbound {
            n.paused_inbound = true;
            n.space_check_at = Some(clock.plus_nanos(SPACE_CHECK_NANOS));
        }
    }

    /// §7.5: from the engine's pause on a full disk until the host reports
    /// `SpaceRecovered`, the folder's inbound is paused. The engine asks for
    /// no fetch and no commit, and puts no other node's version in the
    /// index, except through the report of a commit it asked for before the
    /// pause or a landing an observation finds (§8.3). A revert's records
    /// keep their `seq` and are not new.
    fn starts_inbound_while_paused(&self, id: NodeId, action: &Action) -> Option<String> {
        if !self.nodes.get(&id)?.paused_inbound {
            return None;
        }
        match action {
            Action::Fetch { path, .. }
            | Action::Write { path, .. }
            | Action::Remove { path, .. }
            | Action::SetMeta { path, .. } => Some(format!(
                "{} asked the host to fetch or commit {path} while its disk was full",
                Self::short(id)
            )),
            Action::IndexChanged { record, .. } => {
                let feeding = self.feeding.as_ref().filter(|f| f.node == id)?;
                (!feeding.may_adopt && record.seq > feeding.seq && record.entry.modified_by != id)
                    .then(|| {
                        format!(
                            "{} adopted {}'s version of {} while its disk was full",
                            Self::short(id),
                            Self::short(record.entry.modified_by),
                            record.entry.path
                        )
                    })
            }
            _ => None,
        }
    }

    fn act(&mut self, id: NodeId, action: Action) -> Result<(), Failure> {
        if let Some(detail) = self.tombstones_a_skip(id, &action) {
            return Err(self.fail("Skipped", detail));
        }
        if let Some(detail) = self.changes_a_held_record(id, &action) {
            return Err(self.fail("commit in flight", detail));
        }
        if let Some(detail) = self.starts_inbound_while_paused(id, &action) {
            return Err(self.fail("disk full", detail));
        }
        if let Some(path) = self.commits_held_item(id, &action) {
            return Err(self.fail(
                "I5 brake",
                format!(
                    "{} committed {} from a batch that is still held",
                    Self::short(id),
                    path
                ),
            ));
        }
        match action {
            Action::WakeAt(t) => {
                // The engine speaks the node's skewed clock; the host keeps
                // global time. "No later than" a time already past means
                // now: a restored folder can carry a window that fell due
                // while the process was down.
                let clock = self.clock;
                if let Some(n) = self.nodes.get_mut(&id) {
                    let global = t.plus_nanos(-n.skew_ns).max(clock);
                    n.wake_at = Some(n.wake_at.map_or(global, |w| w.min(global)));
                }
            }
            // Effects wait for the open group, if any (§11).
            Action::Send { to, payload } => self.hold(id, Held::Send { to, payload })?,
            Action::Fetch {
                path,
                version,
                hash,
                size,
                from,
                ..
            } => self.hold(
                id,
                Held::Fetch {
                    path,
                    version,
                    hash,
                    size,
                    from,
                },
            )?,
            Action::Write {
                ref path,
                ref entry,
                ..
            } => {
                let (path, version) = (path.clone(), entry.version.clone());
                let commit = self.commit_started(id, &path, &version, &action)?;
                self.hold(
                    id,
                    Held::Commit {
                        path,
                        version,
                        action: Box::new(action),
                        commit,
                    },
                )?;
            }
            Action::Remove { ref path, .. } | Action::SetMeta { ref path, .. } => {
                // The version the commit is for is the want's as the engine
                // asks for it, not as it stands when the commit is performed.
                let path = path.clone();
                let version = self
                    .nodes
                    .get(&id)
                    .and_then(|n| n.engine.as_ref())
                    .and_then(|e| e.folder(self.folder))
                    .and_then(|f| f.wants().get(&path))
                    .map(|w| w.version().clone())
                    .unwrap_or_default();
                let commit = self.commit_started(id, &path, &version, &action)?;
                self.hold(
                    id,
                    Held::Commit {
                        path,
                        version,
                        action: Box::new(action),
                        commit,
                    },
                )?;
            }
            Action::MoveToTrash { path, .. } => self.hold(id, Held::MoveToTrash { path })?,
            Action::RecordBatch {
                batch,
                role,
                decision,
            } => match role {
                BatchRole::Sent => {
                    self.stats.batches_sent += 1;
                    let paused = self.nodes.get(&id).is_some_and(|n| n.paused);
                    for e in &batch.entries {
                        let seen_before = self.nodes.get(&id).is_some_and(|n| {
                            n.sent
                                .get(&e.path)
                                .is_some_and(|vs| vs.contains(&e.version))
                        });
                        // A paused folder may send announced records (catch-up)
                        // but never its pending, unannounced ones.
                        let leaks_pending = paused
                            && self
                                .engine(id)
                                .and_then(|eng| eng.folder(self.folder))
                                .is_some_and(|f| {
                                    f.index().is_pending(&e.path)
                                        && f.index()
                                            .get(&e.path)
                                            .is_some_and(|r| r.entry.version == e.version)
                                });
                        if leaks_pending {
                            return Err(self.fail(
                                "paused send",
                                format!(
                                    "{} sent its pending version {:?} of {} while paused",
                                    Self::short(id),
                                    e.version,
                                    e.path
                                ),
                            ));
                        }
                        if !e.deleted && e.kind != Kind::Dir {
                            self.announced.insert(e.hash);
                        }
                        if let Some(n) = self.nodes.get_mut(&id)
                            && !seen_before
                        {
                            n.sent
                                .entry(e.path.clone())
                                .or_default()
                                .push(e.version.clone());
                        }
                    }
                }
                BatchRole::Received => {
                    if matches!(decision, Some(delocal_engine::Decision::Held { .. })) {
                        self.stats.held += 1;
                    }
                }
                BatchRole::Paused => {
                    self.stats.paused += 1;
                }
            },
            // Table writes join the open group, which `feed` opened for any
            // event that writes.
            Action::IndexChanged { .. }
            | Action::IndexRemoved { .. }
            | Action::WantChanged { .. }
            | Action::PendingChanged { .. }
            | Action::HeldChanged { .. }
            | Action::DeferredChanged { .. }
            | Action::RestChanged { .. } => {
                let staged = Staged {
                    write: TableWrite::Hook(Box::new(action)),
                    at: self.clock,
                    event: self.stats.events,
                    moves: self.nodes.get(&id).map_or(0, |n| n.moves.len()),
                };
                match self.nodes.get_mut(&id).and_then(|n| n.group.as_mut()) {
                    Some(group) => group.writes.push(staged),
                    None => self.record(id, staged)?,
                }
            }
            Action::StatusChanged { status, .. } => match status {
                FolderStatus::Paused { .. } => {
                    if let Some(n) = self.nodes.get_mut(&id) {
                        n.paused = true;
                    }
                }
                FolderStatus::Unpaused { .. } | FolderStatus::Reverted { .. } => {
                    if matches!(status, FolderStatus::Reverted { .. }) {
                        self.stats.reverts += 1;
                    }
                    if let Some(n) = self.nodes.get_mut(&id) {
                        n.paused = false;
                    }
                }
                FolderStatus::Denied { .. } => {
                    self.stats.denials += 1;
                }
                FolderStatus::WinnerFallback { count } => {
                    return Err(self.fail(
                        "winner fallback",
                        format!(
                            "{}: rule 5 of the winner rule decided {count} conflict(s)",
                            Self::short(id)
                        ),
                    ));
                }
                FolderStatus::Stalled { .. } => self.stats.stalled += 1,
                FolderStatus::UnchangedUnknownPath { path } => {
                    return Err(self.fail(
                        "host bug",
                        format!("{}: Unchanged reported for {path} which the engine has no live record for", Self::short(id)),
                    ));
                }
                _ => {}
            },
        }
        Ok(())
    }

    /// A table write reaches the persisted store (§11), and the invariant
    /// tracking that follows index writes sees it, dated when it was made.
    /// Only durable writes are recorded: a write a crash lost was never
    /// seen by anyone, and its effects never happened.
    fn record(&mut self, id: NodeId, staged: Staged) -> Result<(), Failure> {
        let Staged {
            write,
            at: made_at,
            event,
            moves,
        } = staged;
        let action = match write {
            TableWrite::Hook(action) => *action,
            TableWrite::Rules(rules) => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.persisted.rules = rules;
                }
                return Ok(());
            }
        };
        match action {
            Action::IndexChanged { record, .. } => {
                if record.entry.path.file_name().contains(".conflict-")
                    && record.entry.modified_by == id
                    && !record.entry.deleted
                {
                    self.stats.conflict_copies += 1;
                }
                // I7 (§14.1): equal vectors at a path mean equal content and
                // deletion state, on every node, after every index write. The
                // one exemption is a node's own re-issue after `revert`
                // (§8.3): the reverted record's vector was never announced,
                // and the newer own write replaces it.
                let path = record.entry.path.clone();
                let list = self.versions.entry(path.clone()).or_default();
                match list.iter().position(|e| e.version == record.entry.version) {
                    Some(at) => {
                        let existing = list[at].clone();
                        let same = existing.kind == record.entry.kind
                            && existing.hash == record.entry.hash
                            && existing.exec == record.entry.exec
                            && existing.deleted == record.entry.deleted;
                        // The earlier record was discarded by its author's
                        // revert after it was written: nobody else ever saw
                        // it, and the vector is free to be reached again, by
                        // that author's next change or by any merge that
                        // includes it. The new record replaces it whatever the
                        // content, so that conflict-copy names (from the
                        // loser's mtime) are checked against what exists.
                        let discarded = self.discarded_by_revert(&existing);
                        if !same || discarded {
                            if !same && !discarded {
                                return Err(self.fail(
                                    "I7 version identity",
                                    format!(
                                        "{}: {} at version {:?} has {:?} {} exec={} deleted={} but the same vector was seen with {:?} {} exec={} deleted={}",
                                        Self::short(id),
                                        path,
                                        record.entry.version,
                                        record.entry.kind,
                                        record.entry.hash.short(),
                                        record.entry.exec,
                                        record.entry.deleted,
                                        existing.kind,
                                        existing.hash.short(),
                                        existing.exec,
                                        existing.deleted
                                    ),
                                ));
                            }
                            if existing != record.entry {
                                self.superseded
                                    .entry(path.clone())
                                    .or_default()
                                    .push(existing);
                            }
                            if let Some(list) = self.versions.get_mut(&path) {
                                list[at] = record.entry.clone();
                            }
                            // The replacement is a new record: it was first
                            // seen now, after the revert that discarded the
                            // old one, or the next revert check would set it
                            // aside too.
                            let now_ev = event;
                            let seen = self.seen_at.entry(path.clone()).or_default();
                            match seen.iter_mut().find(|(v, _)| *v == record.entry.version) {
                                Some(slot) => slot.1 = now_ev,
                                None => seen.push((record.entry.version.clone(), now_ev)),
                            }
                        }
                    }
                    None => {
                        list.push(record.entry.clone());
                        let now_ev = event;
                        self.seen_at
                            .entry(path.clone())
                            .or_default()
                            .push((record.entry.version.clone(), now_ev));
                    }
                }
                let now = made_at;
                let mut restored = None;
                if let Some(n) = self.nodes.get_mut(&id) {
                    // A record put back by `revert` keeps its old `seq`
                    // (§8.3); the revert runs whenever the folder settles,
                    // not necessarily at the user's step, so it is known by
                    // this. It restores what peers hold and lands nothing.
                    let reverted = record.seq <= n.max_seq;
                    n.max_seq = n.max_seq.max(record.seq);
                    if reverted {
                        restored = Some(record.entry.version.clone());
                    }
                    // Content arrived only if the record's hash is new at the
                    // path: a metadata-only apply (§7.5) adopts a version of
                    // what the node already holds and lands no bytes.
                    let landed = n
                        .persisted
                        .records
                        .get(&record.entry.path)
                        .is_none_or(|prev| {
                            prev.entry.deleted || prev.entry.hash != record.entry.hash
                        });
                    if record.entry.modified_by != id
                        && !record.entry.deleted
                        && record.entry.kind != Kind::Dir
                        && landed
                        && !reverted
                    {
                        // Where the content is now: a commit of this node's
                        // may have moved it to a conflict copy since the write
                        // was made, before the write was durable.
                        let (mut at, mut when) = (record.entry.path.clone(), now);
                        for (hash, from, to, moved_at) in n.moves.iter().skip(moves) {
                            if *hash == record.entry.hash && *from == at {
                                at = to.clone();
                                when = *moved_at;
                            }
                        }
                        n.synced.push((record.entry.hash, at, when));
                    }
                    n.persisted
                        .records
                        .insert(record.entry.path.clone(), record);
                }
                if let Some(version) = restored {
                    self.reverted_at.insert((id, path), (event, Some(version)));
                }
            }
            Action::IndexRemoved { path, .. } => {
                // Only `revert` removes a record: a path peers never saw.
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.persisted.records.remove(&path);
                }
                self.reverted_at.insert((id, path), (event, None));
            }
            Action::WantChanged { path, want, .. } => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    match want {
                        Some(w) => {
                            n.persisted.wants.insert(path, *w);
                        }
                        None => {
                            n.persisted.wants.remove(&path);
                        }
                    }
                }
            }
            // The other parts' tables (§11): each hook writes its row, and a
            // `None` removes it.
            Action::PendingChanged { path, row, .. } => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    match row {
                        Some(row) => n.persisted.pending.insert(path, row),
                        None => n.persisted.pending.remove(&path),
                    };
                }
            }
            Action::HeldChanged {
                batch, state, row, ..
            } => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    match row {
                        Some(row) => n.persisted.held.insert((batch, state), *row),
                        None => n.persisted.held.remove(&(batch, state)),
                    };
                }
            }
            Action::DeferredChanged { path, entries, .. } => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    match entries {
                        Some(entries) => n.persisted.deferred.insert(path, entries),
                        None => n.persisted.deferred.remove(&path),
                    };
                }
            }
            Action::RestChanged { rest, .. } => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.persisted.rest = Some(*rest);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// An effect waits for the open group on `id`, if there is one, and is
    /// performed once the group is durable; with none open it depends on
    /// nothing unwritten and is performed now (§11).
    fn hold(&mut self, id: NodeId, held: Held) -> Result<(), Failure> {
        match self.nodes.get_mut(&id).and_then(|n| n.group.as_mut()) {
            Some(group) => {
                group.effects.push(held);
                Ok(())
            }
            None => self.perform(id, held),
        }
    }

    /// Carry out an effect: a message goes on the wire, a fetch or a commit
    /// starts, a file goes to the trash.
    fn perform(&mut self, id: NodeId, held: Held) -> Result<(), Failure> {
        match held {
            Held::Send { to, payload } => self.send(id, to, payload)?,
            Held::Fetch {
                path,
                version,
                hash,
                size,
                from,
            } => {
                // A request cannot go out over a connection that dropped
                // while the fetch waited for its group; the engine has been
                // told of the drop and has put the want back.
                if !self.connected(id, from) {
                    return Ok(());
                }
                self.stats.fetches += 1;
                let tier = self.link(id, from).map_or(Tier::Relay, |l| l.tier);
                let per_byte = match tier {
                    Tier::Lan => 1_000_000,
                    Tier::Direct => 5_000_000,
                    Tier::Relay => 20_000_000,
                };
                let mut duration = 200_000_000 + size as i64 * per_byte;
                if self.rng.random_range(0u32..10) == 0 {
                    // A slow link now and then, so stalls and progress matter.
                    duration += self.rng.random_range(30 * NANOS..120 * NANOS);
                }
                let corrupt =
                    self.corruption_on && self.rng.random::<f64>() < self.knobs.corruption;
                self.ops.push(Op::Fetch {
                    node: id,
                    from,
                    path,
                    version,
                    hash,
                    done_at: self.clock.plus_nanos(duration),
                    next_progress: self.clock.plus_nanos(2 * NANOS),
                    corrupt,
                });
            }
            Held::Commit {
                path,
                version,
                action,
                commit,
            } => {
                let done_at = self
                    .clock
                    .plus_nanos(self.rng.random_range(50_000_000..500_000_000));
                self.ops.push(Op::Commit {
                    node: id,
                    path,
                    version,
                    action,
                    commit,
                    done_at,
                    suspended: false,
                });
            }
            Held::MoveToTrash { path } => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    move_to_trash(n, &path);
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- network

    fn link_key(a: NodeId, b: NodeId) -> (NodeId, NodeId) {
        link_key(a, b)
    }

    fn link(&self, a: NodeId, b: NodeId) -> Option<Link> {
        self.links.get(&Self::link_key(a, b)).copied()
    }

    fn connected(&self, a: NodeId, b: NodeId) -> bool {
        self.link(a, b).is_some_and(|l| l.up)
            && self.nodes.get(&a).is_some_and(Node::alive)
            && self.nodes.get(&b).is_some_and(Node::alive)
    }

    fn send(&mut self, from: NodeId, to: NodeId, payload: Outbound) -> Result<(), Failure> {
        if !self.connected(from, to) {
            return Ok(()); // dropped on the floor, as a dead link would
        }
        let (lo, hi) = self.knobs.delay_ms;
        let delay_ms = self.rng.random_range(lo..=hi.max(lo)) as i64;
        self.msg_seq += 1;
        self.messages.push(Message {
            deliver_at: self.clock.plus_nanos(delay_ms * 1_000_000),
            seq: self.msg_seq,
            from,
            to,
            payload,
        });
        Ok(())
    }

    fn deliver(&mut self, msg: Message) -> Result<(), Failure> {
        if !self.connected(msg.from, msg.to) {
            return Ok(());
        }
        let event = match msg.payload {
            Outbound::Batch(batch) => Event::BatchReceived {
                from: msg.from,
                batch,
            },
            Outbound::Decision(decision) => Event::DecisionReceived {
                from: msg.from,
                decision,
            },
            Outbound::HaveUpTo { folder, seq } => Event::HaveUpToReceived {
                from: msg.from,
                folder,
                seq,
            },
        };
        self.feed(msg.to, event)
    }

    fn connect(&mut self, a: NodeId, b: NodeId) -> Result<(), Failure> {
        if !self.connected(a, b) {
            return Ok(());
        }
        let tier = self.link(a, b).map_or(Tier::Relay, |l| l.tier);
        self.feed(a, Event::PeerConnected { peer: b, tier })?;
        self.feed(b, Event::PeerConnected { peer: a, tier })
    }

    fn disconnect(&mut self, a: NodeId, b: NodeId) -> Result<(), Failure> {
        self.messages
            .retain(|m| !((m.from == a && m.to == b) || (m.from == b && m.to == a)));
        self.ops.retain(|op| match op {
            Op::Fetch { node, from, .. } => {
                !((*node == a && *from == b) || (*node == b && *from == a))
            }
            Op::Commit { .. } => true,
        });
        self.feed(a, Event::PeerDisconnected { peer: b })?;
        self.feed(b, Event::PeerDisconnected { peer: a })
    }

    fn set_link(&mut self, a: NodeId, b: NodeId, up: bool) -> Result<(), Failure> {
        if a == b {
            return Ok(());
        }
        let key = Self::link_key(a, b);
        let Some(link) = self.links.get_mut(&key) else {
            return Ok(());
        };
        if link.up == up {
            return Ok(());
        }
        link.up = up;
        if up {
            self.connect(a, b)
        } else {
            self.disconnect(a, b)
        }
    }

    fn set_tier(&mut self, a: NodeId, b: NodeId, tier: Tier) -> Result<(), Failure> {
        if a == b {
            return Ok(());
        }
        let key = Self::link_key(a, b);
        let Some(link) = self.links.get_mut(&key) else {
            return Ok(());
        };
        if link.tier == tier {
            return Ok(());
        }
        link.tier = tier;
        if self.connected(a, b) {
            self.feed(a, Event::PeerTierChanged { peer: b, tier })?;
            self.feed(b, Event::PeerTierChanged { peer: a, tier })?;
        }
        Ok(())
    }

    fn set_online(&mut self, id: NodeId, online: bool) -> Result<(), Failure> {
        let clock = self.clock;
        let Some(node) = self.nodes.get(&id) else {
            return Ok(());
        };
        if node.online == online {
            return Ok(());
        }
        let peers: Vec<NodeId> = self.order.iter().copied().filter(|p| *p != id).collect();
        if online {
            if let Some(n) = self.nodes.get_mut(&id) {
                n.online = true;
                // Offline stands for a suspended machine: its timers did not
                // run, and whatever fell due while it slept fires now.
                n.wake_at = n.wake_at.map(|t| t.max(clock));
                n.space_check_at = n.space_check_at.map(|t| t.max(clock));
                n.next_scan_at = n.next_scan_at.max(clock);
            }
            for op in &mut self.ops {
                if let Op::Commit {
                    node,
                    done_at,
                    suspended,
                    ..
                } = op
                    && *node == id
                    && *suspended
                {
                    *suspended = false;
                    *done_at = clock;
                }
            }
            for p in peers {
                self.connect(id, p)?;
            }
        } else {
            for p in peers {
                if self.connected(id, p) {
                    self.disconnect(id, p)?;
                }
            }
            if let Some(n) = self.nodes.get_mut(&id) {
                n.online = false;
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- crashes

    fn crash(&mut self, id: NodeId, gap_ns: i64) -> Result<(), Failure> {
        let Some(node) = self.nodes.get(&id) else {
            return Ok(());
        };
        if node.engine.is_none() {
            return Ok(());
        }
        self.stats.crashes += 1;
        let peers: Vec<NodeId> = self.order.iter().copied().filter(|p| *p != id).collect();
        for p in peers {
            if self.connected(id, p) {
                // The peer notices the connection drop; the crashed side is gone.
                self.messages
                    .retain(|m| !((m.from == id && m.to == p) || (m.from == p && m.to == id)));
                self.feed(p, Event::PeerDisconnected { peer: id })?;
            }
        }
        self.ops
            .retain(|op| op.node() != id && !matches!(op, Op::Fetch { from, .. } if *from == id));
        // The node's commits die with it; the restart wants their paths again.
        self.committing.retain(|(node, _), _| *node != id);
        let folder = self.folder;
        let lag = self.knobs.group_commit_lag;
        if let Some(n) = self.nodes.get_mut(&id) {
            // §11: the open group's writes and the effects held for them are
            // lost together. The check at the restart compares the rebuild
            // with the folder as it stood when its last group became durable,
            // since the tables lag the engine by design.
            if let Some(group) = n.group.take() {
                self.stats.groups_lost += 1;
                self.stats.effects_lost += group.effects.len() as u64;
            }
            n.crashed = if lag > 0 {
                n.durable.take()
            } else {
                n.engine.as_ref().and_then(|e| e.folder(folder)).cloned()
            };
            n.engine = None;
            n.temp.clear();
            n.wake_at = None;
            // The host's own memory of a full disk died with it; at the
            // restart it learns the folder's state from its tables.
            n.space_check_at = None;
            n.paused_inbound = false;
            n.restart_at = Some(self.clock.plus_nanos(gap_ns));
        }
        Ok(())
    }

    fn restart(&mut self, id: NodeId) -> Result<(), Failure> {
        let now = self.now_for(id);
        let config = self.config_of(id);
        let Some(parts) = self
            .nodes
            .get(&id)
            .and_then(|n| n.persisted.parts(self.folder, &self.order))
        else {
            let detail = format!("{}: no rest row to restart from", Self::short(id));
            return Err(self.fail("persistence hooks", detail));
        };
        // §11: the folder rebuilt from the tables must be the folder that
        // crashed, once both have been through the restart. The per-event
        // check catches a hook that did not report a change; this catches a
        // part that `parts()` and `from_parts()` leave out, whether or not
        // an invariant would notice it was lost.
        let lost = self
            .nodes
            .get_mut(&id)
            .and_then(|n| n.crashed.take())
            .map(|crashed| restart_difference(&parts, crashed, &config, now));
        match lost {
            Some(None) => {}
            Some(Some(field)) => {
                let detail = format!(
                    "{}: the folder rebuilt from the tables differs from the one that crashed, first in {field}",
                    Self::short(id)
                );
                return Err(self.fail("persistence hooks", detail));
            }
            None => {
                let detail = format!("{}: restarted without having crashed", Self::short(id));
                return Err(self.fail("persistence hooks", detail));
            }
        }
        let clock = self.clock;
        let Some(node) = self.nodes.get_mut(&id) else {
            return Ok(());
        };
        node.restart_at = None;
        // §7.5: before the first scan, every row the crash left open in the
        // commit journal is undone. The temp files died with the process.
        let rows = std::mem::take(&mut node.journal);
        self.stats.displacements_undone += rows.len() as u64;
        for row in rows.into_iter().rev() {
            undo(node, row, clock);
        }
        // §11: the persisted parts and nothing else.
        let engine = Engine::restore(config, vec![parts], now);
        if self.knobs.group_commit_lag > 0 {
            node.durable = engine.folder(self.folder).cloned();
        }
        // §7.5: a restart frees no space. A folder whose inbound a full disk
        // paused is still paused, and the host goes on checking.
        if engine
            .folder(self.folder)
            .is_some_and(FolderState::disk_full)
        {
            node.paused_inbound = true;
            node.space_check_at = Some(clock.plus_nanos(SPACE_CHECK_NANOS));
        }
        node.engine = Some(engine);
        // §7.3: a full scan runs at daemon start, so at every restart; a
        // queued `revert` waits for it (§8.3). A node restarted while
        // offline scans as soon as it is back.
        if node.alive() {
            self.full_scan(id, true)?;
        } else {
            node.next_scan_at = self.clock;
        }
        let peers: Vec<NodeId> = self.order.iter().copied().filter(|p| *p != id).collect();
        for p in peers {
            self.connect(id, p)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- host ops

    fn progress_op(&mut self, pos: usize) -> Result<(), Failure> {
        let op = self.ops[pos].clone();
        match op {
            Op::Fetch {
                node,
                from,
                path,
                version,
                hash,
                done_at,
                next_progress,
                corrupt,
            } => {
                if next_progress <= self.clock && next_progress < done_at {
                    if let Op::Fetch { next_progress, .. } = &mut self.ops[pos] {
                        *next_progress = self.clock.plus_nanos(2 * NANOS);
                    }
                    return self.feed(
                        node,
                        Event::FetchProgress {
                            folder: self.folder,
                            path,
                            hash,
                            version,
                        },
                    );
                }
                self.ops.remove(pos);
                if !self.connected(node, from) {
                    return Ok(());
                }
                // §7.5 step 2: the source serves the requested content from
                // `path` if its live record there has that hash and what is
                // on disk still matches the record by the scan's fast-path
                // test (size, mtime and exec bit for a file, kind and target
                // for a symlink), failing that from any live file with that
                // hash, failing that not at all.
                let served = self.nodes.get(&from).and_then(|src| {
                    let engine = src.engine.as_ref()?;
                    let index = engine.folder(self.folder)?.index();
                    index.locate(&path, &hash).find_map(|at| {
                        let record = index.live(at)?;
                        let file = src.fs.get(at)?;
                        record
                            .entry
                            .unchanged_by_stat(&file.observed())
                            .then(|| file.content.clone())
                    })
                });
                let report = match served {
                    None => {
                        self.stats.not_available += 1;
                        FetchReport::NotAvailable
                    }
                    Some(mut bytes) => {
                        if corrupt {
                            bytes.push(0xff);
                        }
                        if hash_bytes(&bytes) != hash {
                            self.stats.mismatches += 1;
                            FetchReport::HashMismatch
                        } else if let Some(error) = self.local_failure(node, true) {
                            // The bytes arrived and could not be written here.
                            FetchReport::Failed { error }
                        } else {
                            if let Some(n) = self.nodes.get_mut(&node) {
                                put_temp(n, &path, &version, bytes);
                            }
                            FetchReport::Ok
                        }
                    }
                };
                let failed = match &report {
                    FetchReport::Failed { error } => Some(error.clone()),
                    _ => None,
                };
                // The fetch the want waits for, or one it has moved on from,
                // whose report the engine ignores (§7.5).
                let awaited = self
                    .engine(node)
                    .and_then(|e| e.folder(self.folder))
                    .and_then(|f| f.wants().get(&path))
                    .is_some_and(|w| {
                        w.version() == &version && matches!(w.state, WantState::Fetching { .. })
                    });
                let what = format!("fetch of {path}");
                self.feed(
                    node,
                    Event::Fetched {
                        folder: self.folder,
                        path,
                        hash,
                        version,
                        outcome: report,
                    },
                )?;
                match failed {
                    Some(error) => self.after_local_failure(node, &error, awaited, &what),
                    None => Ok(()),
                }
            }
            Op::Commit {
                node,
                path,
                version,
                action,
                commit,
                ..
            } => {
                // A suspended node's commit waits for it to resume; a crashed
                // node's is gone, and its restart wants the path again.
                if self
                    .nodes
                    .get(&node)
                    .is_some_and(|n| n.engine.is_some() && !n.online)
                {
                    if let Op::Commit { suspended, .. } = &mut self.ops[pos] {
                        *suspended = true;
                    }
                    return Ok(());
                }
                self.ops.remove(pos);
                if !self.nodes.get(&node).is_some_and(Node::alive) {
                    return Ok(());
                }
                // Drawn before the commit starts, and used where it would
                // strike: at the rename for a `Write`, before anything is
                // changed for a `Remove` or `SetMeta`.
                let fail = self.local_failure(node, matches!(*action, Action::Write { .. }));
                // What the disk held, for the check that a failed commit left
                // it so; only a commit that can fail needs it: one drawn to,
                // and one on a filesystem that ignores case.
                let before = self
                    .nodes
                    .get(&node)
                    .filter(|n| fail.is_some() || n.case_insensitive)
                    .map(|n| (n.fs.clone(), n.trash.clone()));
                let (committed, created) = self.commit(node, &path, &version, &action, fail);
                if let Committed::Report(ApplyOutcome::Failed { error }) = &committed {
                    self.left_as_found(node, &path, error, before)?;
                }
                let outcome = match committed {
                    Committed::Report(outcome) => outcome,
                    // The process died with the path displaced. Nothing is
                    // reported and no watcher is left to see the parents the
                    // host created; the restart undoes the displacement and
                    // its scan finds the rest (§7.5).
                    Committed::CrashedBetweenRenames => {
                        self.stats.crashes_between_renames += 1;
                        let gap = self.journal_rng.random_range(NANOS..60 * NANOS);
                        return self.crash(node, gap);
                    }
                    // §7.5: the engine commits fetched content only once the
                    // host has reported the fetch, and forgets it at every
                    // failure that may have taken the temp file and at every
                    // restart. A temp file goes only with those, so a `Write`
                    // without one is a commit of content the engine could not
                    // know it had.
                    Committed::NoTemp => {
                        return Err(self.fail(
                            "missing temp",
                            format!(
                                "{}: asked to commit {path} at {version:?} with no temp file of that version",
                                Self::short(node)
                            ),
                        ));
                    }
                };
                // The host's mkdir of missing parents is a filesystem event
                // like any other: the watcher may report it, the scan will.
                for dir in created {
                    self.watch(node, dir)?;
                }
                if outcome == ApplyOutcome::ChangedUnderneath {
                    self.stats.changed_underneath += 1;
                }
                let failed = match &outcome {
                    ApplyOutcome::Failed { error } => Some(error.clone()),
                    _ => None,
                };
                // §13: the rename happened but the report is lost to a crash.
                if outcome == ApplyOutcome::Ok
                    && self.rng.random::<f64>() < self.knobs.crash_after_rename
                {
                    let gap = self.rng.random_range(NANOS..60 * NANOS);
                    return self.crash(node, gap);
                }
                // I8: a commit is reported only for a want whose version
                // still dominates the record at its path. The engine adopts
                // what the host committed; if the record moved on meanwhile
                // (this node wrote its own conflict copy at the path while
                // fetching a peer's, say), adopting would overwrite a local
                // write the peers never saw. The engine asserts this in debug
                // builds; the check here is what the release sweep and the
                // shrinker see.
                if outcome == ApplyOutcome::Ok
                    && let Some(f) = self.engine(node).and_then(|e| e.folder(self.folder))
                    && let Some(want) = f.wants().get(&path)
                    && let Some(record) = f.index().get(&path)
                    && !want.version().dominates_or_equals(&record.entry.version)
                {
                    return Err(self.fail(
                        "I8 adopt dominance",
                        format!(
                            "{}: commit of {} at {:?} reported while the record there is {:?}",
                            Self::short(node),
                            path,
                            want.version(),
                            record.entry.version
                        ),
                    ));
                }
                // The report is in: the commit is no longer in flight, a
                // released one included. Only one its want still waits for
                // is taken as its want's report (§7.5).
                let key = (node, path.clone());
                let (copy_to, awaited) = match self.committing.get(&key) {
                    Some(c) if c.commit == commit => {
                        let awaited = !c.released;
                        let copy_to = self.committing.remove(&key).and_then(|c| c.copy_to);
                        (copy_to, awaited)
                    }
                    _ => (None, false),
                };
                let what = format!("commit of {path}");
                self.copy_recorded = copy_to
                    .filter(|_| outcome == ApplyOutcome::Ok)
                    .map(|to| (node, to));
                let fed = self.feed(
                    node,
                    Event::Applied {
                        folder: self.folder,
                        path,
                        version,
                        outcome,
                    },
                );
                self.copy_recorded = None;
                fed?;
                match failed {
                    Some(error) => self.after_local_failure(node, &error, awaited, &what),
                    None => Ok(()),
                }
            }
        }
    }

    /// Whether a fetch or a commit on `id` fails on this machine (§7.5
    /// "local failures"). One that `needs_space` (a fetch writing its bytes,
    /// a `Write`) may first fill the disk, which then stays full for 30 s to
    /// 10 min, and fails with `DiskFull` while it is. Otherwise it fails
    /// with an I/O error at the knob's rate. Every draw is on `FAIL_STREAM`,
    /// and none is made with the knobs at 0 or in the final phase.
    fn local_failure(&mut self, id: NodeId, needs_space: bool) -> Option<LocalError> {
        if !self.failures_on {
            return None;
        }
        let now = self.clock;
        if needs_space {
            if self.knobs.disk_fill > 0.0 && self.fail_rng.random::<f64>() < self.knobs.disk_fill {
                let until = now.plus_nanos(self.fail_rng.random_range(30 * NANOS..=600 * NANOS));
                if let Some(n) = self.nodes.get_mut(&id)
                    && n.full_until.is_none_or(|t| t <= now)
                {
                    n.full_until = Some(until);
                    self.stats.disk_fills += 1;
                }
            }
            if self
                .nodes
                .get(&id)
                .is_some_and(|n| n.full_until.is_some_and(|t| t > now))
            {
                self.stats.disk_full_reports += 1;
                return Some(LocalError::DiskFull);
            }
        }
        if self.knobs.io_failure > 0.0 && self.fail_rng.random::<f64>() < self.knobs.io_failure {
            self.stats.io_failures += 1;
            return Some(LocalError::Io);
        }
        None
    }

    /// §7.5: a failed commit leaves the disk as it found it. The host
    /// undid its displacement and removed the directories it made, so the
    /// node's folder and trash are exactly as they were before the commit
    /// (`before`), and no journal row is left open.
    fn left_as_found(
        &self,
        id: NodeId,
        path: &RelPath,
        error: &LocalError,
        before: Option<(BTreeMap<RelPath, File>, Vec<ContentHash>)>,
    ) -> Result<(), Failure> {
        let Some(node) = self.nodes.get(&id) else {
            return Ok(());
        };
        let as_found = before.is_some_and(|(fs, trash)| fs == node.fs && trash == node.trash);
        if as_found && node.journal.is_empty() {
            return Ok(());
        }
        Err(self.fail(
            "failed commit",
            format!(
                "{}: the commit of {path} failed with {error:?} but changed the disk",
                Self::short(id)
            ),
        ))
    }

    /// After a fetch or a commit on `id` failed (§7.5), I2 must still hold
    /// on that node: every content sync adopted there is in its folder or
    /// its trash, a failure having moved nothing anywhere. And a full disk
    /// reported by the operation its want waited for (`awaited`) must have
    /// paused the folder's inbound; the report of one the want had moved
    /// on from is ignored like any such report, and the next operation to
    /// find the disk full says so for itself.
    fn after_local_failure(
        &mut self,
        id: NodeId,
        error: &LocalError,
        awaited: bool,
        what: &str,
    ) -> Result<(), Failure> {
        if *error == LocalError::DiskFull
            && awaited
            && !self
                .engine(id)
                .and_then(|e| e.folder(self.folder))
                .is_some_and(FolderState::disk_full)
        {
            return Err(self.fail(
                "disk full",
                format!(
                    "{}: the {what} its want waited for found the disk full, and the folder's inbound did not pause",
                    Self::short(id)
                ),
            ));
        }
        invariants::i2_node(self, id)
    }

    /// The host of `id` checks for free space while its folder's inbound
    /// is paused (§7.5), every 30 s, and reports `SpaceRecovered` once the
    /// disk has room.
    fn check_space(&mut self, id: NodeId) -> Result<(), Failure> {
        let clock = self.clock;
        let paused = self
            .engine(id)
            .and_then(|e| e.folder(self.folder))
            .is_some_and(FolderState::disk_full);
        let Some(n) = self.nodes.get_mut(&id) else {
            return Ok(());
        };
        if !paused {
            n.space_check_at = None;
            n.paused_inbound = false;
            return Ok(());
        }
        if n.full_until.is_some_and(|t| t > clock) {
            n.space_check_at = Some(clock.plus_nanos(SPACE_CHECK_NANOS));
            return Ok(());
        }
        n.full_until = None;
        n.space_check_at = None;
        n.paused_inbound = false;
        self.stats.spaces_recovered += 1;
        let folder = self.folder;
        self.feed(id, Event::SpaceRecovered { folder })?;
        invariants::i2_node(self, id)
    }

    /// §7.5 steps 6 to 9 against the simulated filesystem. Also returns the
    /// parent directories the host had to create for a write. `fail` is the
    /// local failure drawn for this commit, if any: a `Write` meets it at
    /// its rename, a `Remove` or `SetMeta` before it changes anything.
    fn commit(
        &mut self,
        id: NodeId,
        path: &RelPath,
        version: &Version,
        action: &Action,
        fail: Option<LocalError>,
    ) -> (Committed, Vec<RelPath>) {
        let now = self.clock;
        let crash_between = self.knobs.crash_between_renames;
        let subtrees = self.knobs.displace_subtrees;
        let changed = || Committed::Report(ApplyOutcome::ChangedUnderneath);
        // A failure at the rename may take the temp file with it, which only
        // the host can tell (§7.5): half do, on `FAIL_STREAM`.
        let loses_temp = fail.is_some() && self.fail_rng.random::<bool>();
        let Some(node) = self.nodes.get_mut(&id) else {
            return (changed(), Vec::new());
        };
        // §7.5 step 6 on a filesystem that ignores case: a live record at a
        // path that is this one but for case names the same file, so the
        // commit is not attempted. The host asks its own tables, which is
        // all it knows of the index.
        if node.case_insensitive
            && let Action::Write { .. } = action
            && let Some(with) = collides(node, path)
        {
            self.stats.case_collisions += 1;
            let error = LocalError::CaseCollision { with };
            return (
                Committed::Report(ApplyOutcome::Failed { error }),
                Vec::new(),
            );
        }
        // §7.5 step 6: the same guard before every commit, SetMeta included.
        let expected = match action {
            Action::Write { expected, .. }
            | Action::Remove { expected, .. }
            | Action::SetMeta { expected, .. } => expected.as_ref(),
            _ => None,
        };
        if !expected_matches(node.fs.get(path), expected) {
            return (changed(), Vec::new());
        }
        // A `Remove` or `SetMeta` fails, if it does, before it has moved or
        // changed anything.
        if let Some(error) = fail.clone()
            && !matches!(action, Action::Write { .. })
        {
            return (
                Committed::Report(ApplyOutcome::Failed { error }),
                Vec::new(),
            );
        }
        let mut created = Vec::new();
        if let Action::Write { .. } = action {
            // A rename into a directory that does not exist fails, so a real
            // host creates the missing ancestors first (the entry's own
            // parent may be tombstoned here while a peer kept a child alive).
            // A file where a directory must be cannot be renamed into.
            let mut ancestor = path.parent();
            while let Some(dir) = ancestor {
                match node.fs.get(&dir) {
                    Some(f) if f.kind == Kind::Dir => break,
                    Some(_) => return (changed(), Vec::new()),
                    None => created.push(dir.clone()),
                }
                ancestor = dir.parent();
            }
            for dir in &created {
                node.fs.insert(
                    dir.clone(),
                    File {
                        kind: Kind::Dir,
                        content: Vec::new(),
                        mtime_ns: 0,
                        exec: false,
                    },
                );
            }
        }
        let outcome = match action {
            Action::Write {
                entry, displace, ..
            } => {
                // The rename in refuses to replace (§7.5), and on a
                // filesystem that ignores case another spelling is the same
                // name, so is the conflict-copy path's.
                if occupied_otherwise(node, path)
                    || matches!(displace, Displace::ConflictCopy(target) if occupied(node, target))
                {
                    return (changed(), created);
                }
                // Step 7, behind a journal row made durable first. The row
                // is this commit's until the rename below removes it: rows
                // left by a crash were all undone at the restart.
                if subtrees && has_children(node, path) {
                    self.stats.subtrees_displaced += 1;
                }
                if displace_journalled(node, path, displace, subtrees, now)
                    && crash_between > 0.0
                    && self.journal_rng.random::<f64>() < crash_between
                {
                    return (Committed::CrashedBetweenRenames, created);
                }
                let row = node.journal.pop();
                // Step 8. A drawn failure strikes the rename itself (§7.5):
                // the host undoes step 7 and removes the directories it made,
                // so the disk is as it found it, and reports the failure. The
                // temp file stays for resumption, unless the failure took it.
                if let Some(error) = fail {
                    if let Some(row) = row {
                        self.stats.displacements_undone += 1;
                        undo(node, row, now);
                    }
                    for dir in created.iter().rev() {
                        node.fs.remove(dir);
                    }
                    if loses_temp && take_temp(node, path, version).is_some() {
                        self.stats.temps_lost += 1;
                    }
                    return (
                        Committed::Report(ApplyOutcome::Failed { error }),
                        Vec::new(),
                    );
                }
                // The rename needs a verified temp file (a directory is made
                // in place instead).
                let content = match entry.kind {
                    Kind::Dir => Some(Vec::new()),
                    _ => take_temp(node, path, version),
                };
                let Some(content) = content else {
                    return (Committed::NoTemp, created);
                };
                node.fs.insert(
                    path.clone(),
                    File {
                        kind: entry.kind,
                        content,
                        mtime_ns: entry.mtime_ns,
                        exec: entry.exec,
                    },
                );
                ApplyOutcome::Ok
            }
            Action::Remove { displace, .. } => {
                // A directory is removed only when empty (§7.5, "Deletes").
                // Displaced to a conflict copy it is renamed instead, and a
                // rename takes its children (§7.6).
                let renamed = subtrees && matches!(displace, Displace::ConflictCopy(_));
                if !renamed && has_children(node, path) {
                    return (changed(), created); // not empty
                }
                if let Displace::ConflictCopy(target) = displace
                    && node.fs.contains_key(target)
                {
                    return (changed(), created);
                }
                if has_children(node, path) {
                    self.stats.subtrees_displaced += 1;
                }
                let moved = displace_path(node, path, displace, subtrees);
                if let Displace::ConflictCopy(target) = displace {
                    follow_moved(node, path, target, &moved, now);
                }
                ApplyOutcome::Ok
            }
            Action::SetMeta { mtime_ns, exec, .. } => match node.fs.get_mut(path) {
                Some(file) if file.kind == Kind::File => {
                    file.mtime_ns = *mtime_ns;
                    file.exec = *exec;
                    ApplyOutcome::Ok
                }
                _ => ApplyOutcome::ChangedUnderneath,
            },
            _ => ApplyOutcome::ChangedUnderneath,
        };
        (Committed::Report(outcome), created)
    }

    // ---------------------------------------------------------------- scanning

    /// A watcher event for `path` on `id`, unless the watcher drops it. Every
    /// user change to the filesystem comes through here, so this is also
    /// where the user's edits are remembered for I2.
    fn watch(&mut self, id: NodeId, path: RelPath) -> Result<(), Failure> {
        let now = self.clock;
        if let Some(n) = self.nodes.get_mut(&id) {
            n.local_edit_at.insert(path.clone(), now);
        }
        if self.rng.random::<f64>() < self.knobs.drop_watcher {
            return Ok(());
        }
        // The watcher sees a path a rule ignores, and the host reports it
        // `Skipped`, which outside a bracket changes nothing (§7.3).
        if self.nodes.get(&id).is_some_and(|n| ignores(n, &path)) {
            let mut check = SkipCheck::new(id);
            check.skipped.insert(path.clone());
            let event = Event::Scanned {
                folder: self.folder,
                path,
                state: ScanState::Skipped {
                    reason: SkipReason::Ignored,
                },
            };
            return self.checked(id, event, &mut check);
        }
        let state = self
            .nodes
            .get(&id)
            .and_then(|n| n.fs.get(&path))
            .map_or(ScanState::Absent, |f| ScanState::Observed(f.observed()));
        self.feed(
            id,
            Event::Scanned {
                folder: self.folder,
                path,
                state,
            },
        )
    }

    /// A full scan (§7.3) with the fast path against the index records the
    /// host has written, under the skip model (see [`Sim::skip_some`]). The
    /// skip checker watches every report of `Skipped` and, if there was
    /// one, the end of the bracket.
    fn full_scan(&mut self, id: NodeId, may_abort: bool) -> Result<(), Failure> {
        let Some(node) = self.nodes.get_mut(&id) else {
            return Ok(());
        };
        node.next_scan_at = self
            .clock
            .plus_nanos(self.rng.random_range(60 * NANOS..600 * NANOS));
        if !node.alive() {
            return Ok(());
        }
        let entries: Vec<(RelPath, ScanState)> = node
            .fs
            .iter()
            .map(|(path, file)| {
                // The §7.3 fast path, the same test a real scanner applies to
                // what stat returns.
                let fast = written_record(node, path)
                    .is_some_and(|r| r.entry.unchanged_by_stat(&file.observed()));
                let state = if fast {
                    ScanState::Unchanged
                } else {
                    ScanState::Observed(file.observed())
                };
                (path.clone(), state)
            })
            .collect();
        let abort_at = if may_abort && self.rng.random_range(0u32..20) == 0 {
            Some(self.rng.random_range(0..=entries.len()))
        } else {
            None
        };
        let walk = self.skip_some(id, entries);
        let mut check = SkipCheck::new(id);
        check.beneath = walk.beneath;
        let folder = self.folder;
        self.feed(id, Event::ScanStarted { folder })?;
        for (i, (path, state)) in walk.reports.into_iter().enumerate() {
            if abort_at == Some(i) {
                return self.bracket_end(id, Event::ScanAborted { folder }, &mut check);
            }
            if let Some(state) = state {
                self.report(id, path, state, &mut check)?;
            }
        }
        for path in walk.unreached {
            let state = ScanState::Skipped {
                reason: SkipReason::Ignored,
            };
            self.report(id, path, state, &mut check)?;
        }
        self.bracket_end(id, Event::ScanFinished { folder }, &mut check)
    }

    /// One report of a full scan. A `Skipped` one joins the bracket's skips
    /// and is fed with the skip checker watching.
    fn report(
        &mut self,
        id: NodeId,
        path: RelPath,
        state: ScanState,
        check: &mut SkipCheck,
    ) -> Result<(), Failure> {
        let skipped = matches!(state, ScanState::Skipped { .. });
        if skipped {
            check.skipped.insert(path.clone());
        }
        let folder = self.folder;
        let event = Event::Scanned {
            folder,
            path,
            state,
        };
        if skipped {
            self.checked(id, event, check)
        } else {
            self.feed(id, event)
        }
    }

    /// The skip model (§7.3) for one full scan of `id` that walked
    /// `entries`. Maybe a rule comes that ignores a tracked path for this
    /// and the next zero to three of the node's scans. Then each path is
    /// reported as the walk found it, or `Skipped`: because a rule ignores
    /// it, or because the scan cannot inspect it, with the knob's
    /// probability; a skipped directory's contents go unreported. Every
    /// draw is on `SKIP_STREAM`, and none is made with the knob at 0 or in
    /// the final phase.
    fn skip_some(&mut self, id: NodeId, entries: Vec<(RelPath, ScanState)>) -> Walk {
        let p = if self.skips_on { self.knobs.skip } else { 0.0 };
        if p > 0.0 && self.skip_rng.random::<f64>() < p {
            let tracked: Vec<RelPath> = self
                .engine(id)
                .and_then(|e| e.folder(self.folder))
                .map(|f| {
                    f.index()
                        .live_records()
                        .map(|r| r.entry.path.clone())
                        .collect()
                })
                .unwrap_or_default();
            if !tracked.is_empty() {
                let path = tracked[self.skip_rng.random_range(0..tracked.len())].clone();
                let scans = self.skip_rng.random_range(1..=4);
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.ignored.insert(path, scans);
                    self.stats.ignores += 1;
                }
            }
        }
        let Some(node) = self.nodes.get_mut(&id) else {
            return Walk::default();
        };
        // This scan meets the rules as they stand; each then lasts one scan
        // fewer.
        let rules: BTreeSet<RelPath> = node.ignored.keys().cloned().collect();
        node.ignored.retain(|_, left| {
            *left -= 1;
            *left > 0
        });
        // A rule covers what lies beneath its path, whatever the kind.
        let mut walk = Walk {
            beneath: rules.clone(),
            ..Walk::default()
        };
        let mut reached = BTreeSet::new();
        for (path, state) in entries {
            if walk.beneath.iter().any(|d| d.is_ancestor_of(&path)) {
                walk.reports.push((path, None));
                continue;
            }
            let dir = node.fs.get(&path).is_some_and(|f| f.kind == Kind::Dir);
            let state = if rules.contains(&path) {
                reached.insert(path.clone());
                ScanState::Skipped {
                    reason: SkipReason::Ignored,
                }
            } else if p > 0.0 && self.skip_rng.random::<f64>() < p {
                let reasons: &[SkipReason] = if dir {
                    &[SkipReason::PermissionDenied, SkipReason::Io]
                } else {
                    &[
                        SkipReason::PermissionDenied,
                        SkipReason::Io,
                        SkipReason::Unstable,
                    ]
                };
                if dir {
                    walk.beneath.insert(path.clone());
                }
                ScanState::Skipped {
                    reason: reasons[self.skip_rng.random_range(0..reasons.len())],
                }
            } else {
                state
            };
            if let ScanState::Skipped { .. } = state {
                self.stats.skipped += 1;
            }
            walk.reports.push((path, Some(state)));
        }
        // A tracked path a rule ignores is reported `Skipped` whether or not
        // it is on disk: while ignored its deletion must not read as one.
        walk.unreached = rules.into_iter().filter(|r| !reached.contains(r)).collect();
        self.stats.skipped += walk.unreached.len() as u64;
        walk
    }

    /// Feed `event` to `id` with the skip checker watching the index writes
    /// it causes, against the index as the event begins.
    fn checked(&mut self, id: NodeId, event: Event, check: &mut SkipCheck) -> Result<(), Failure> {
        if let Some(f) = self.engine(id).and_then(|e| e.folder(self.folder)) {
            check.seq = f.index().seq();
            check.live = f
                .index()
                .live_records()
                .map(|r| (r.entry.path.clone(), r.entry.version.counter(id)))
                .collect();
        }
        self.checking = Some(std::mem::replace(check, SkipCheck::new(id)));
        let fed = self.feed(id, event);
        if let Some(back) = self.checking.take() {
            *check = back;
        }
        fed
    }

    /// The end of a bracket, finished or aborted: watched by the skip
    /// checker if the bracket skipped anything.
    fn bracket_end(
        &mut self,
        id: NodeId,
        event: Event,
        check: &mut SkipCheck,
    ) -> Result<(), Failure> {
        if check.skipped.is_empty() {
            self.feed(id, event)
        } else {
            self.checked(id, event, check)
        }
    }

    // ---------------------------------------------------------------- steps

    fn apply(&mut self, step: &Step) -> Result<(), Failure> {
        match step.clone() {
            Step::Create {
                node,
                path,
                content,
            }
            | Step::Modify {
                node,
                path,
                content,
            } => {
                let id = self.node_at(node);
                let path = self.spell(path_name(path));
                self.write_file(id, &path, content_bytes(content), None)
            }
            Step::Delete { node, path } => {
                let id = self.node_at(node);
                let path = self.spell(path_name(path));
                self.delete_path(id, &path)
            }
            Step::Rename { node, from, to } => {
                let id = self.node_at(node);
                let from = self.spell(path_name(from));
                let from = self.on_disk(id, from);
                let to = self.spell(path_name(to));
                if from == to {
                    return Ok(());
                }
                let content = self
                    .nodes
                    .get(&id)
                    .and_then(|n| n.fs.get(&from))
                    .filter(|f| f.kind == Kind::File)
                    .map(|f| (f.content.clone(), f.exec));
                let Some((content, exec)) = content else {
                    return Ok(());
                };
                self.delete_path(id, &from)?;
                self.write_file(id, &to, content, Some(exec))
            }
            Step::Touch { node, path } => {
                let id = self.node_at(node);
                let path = self.spell(path_name(path));
                let path = self.on_disk(id, path);
                let now = self.now_for(id).as_unix_nanos();
                let touched = self
                    .nodes
                    .get_mut(&id)
                    .and_then(|n| n.fs.get_mut(&path))
                    .filter(|f| f.kind == Kind::File)
                    .map(|f| {
                        f.mtime_ns = now;
                    });
                if touched.is_some() {
                    self.watch(id, path)?;
                }
                Ok(())
            }
            Step::Chmod { node, path } => {
                let id = self.node_at(node);
                let path = self.spell(path_name(path));
                let path = self.on_disk(id, path);
                let flipped = self
                    .nodes
                    .get_mut(&id)
                    .and_then(|n| n.fs.get_mut(&path))
                    .filter(|f| f.kind == Kind::File)
                    .map(|f| {
                        f.exec = !f.exec;
                    });
                if flipped.is_some() {
                    self.watch(id, path)?;
                }
                Ok(())
            }
            Step::Mkdir { node, dir } => {
                let id = self.node_at(node);
                self.mkdir(id, &rel(&dir_name(dir)))
            }
            Step::Rmdir { node, dir } => {
                let id = self.node_at(node);
                self.delete_path(id, &rel(&dir_name(dir)))
            }
            Step::Symlink { node, path, target } => {
                let id = self.node_at(node);
                let path = self.spell(path_name(path));
                let path = self.on_disk(id, path);
                let target = path_name(target).into_bytes();
                let now = self.now_for(id).as_unix_nanos();
                self.ensure_parent(id, &path)?;
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.fs.insert(
                        path.clone(),
                        File {
                            kind: Kind::Symlink,
                            content: target,
                            mtime_ns: now,
                            exec: false,
                        },
                    );
                }
                self.watch(id, path)
            }
            Step::Everywhere { path, contents } => {
                let path = self.spell(path_name(path));
                let ids = self.order.clone();
                for (i, id) in ids.into_iter().enumerate() {
                    if !self.nodes.get(&id).is_some_and(Node::alive) {
                        continue;
                    }
                    match contents.get(i % contents.len().max(1)).copied().flatten() {
                        Some(c) => self.write_file(id, &path, content_bytes(c), None)?,
                        None => self.delete_path(id, &path)?,
                    }
                }
                Ok(())
            }
            Step::Partition { a, b } => {
                let (a, b) = (self.node_at(a), self.node_at(b));
                self.set_link(a, b, false)
            }
            Step::Heal { a, b } => {
                let (a, b) = (self.node_at(a), self.node_at(b));
                self.set_link(a, b, true)
            }
            Step::Tier { a, b, tier } => {
                let (a, b) = (self.node_at(a), self.node_at(b));
                let tier = match tier % 3 {
                    0 => Tier::Lan,
                    1 => Tier::Direct,
                    _ => Tier::Relay,
                };
                self.set_tier(a, b, tier)
            }
            Step::Offline { node } => {
                let id = self.node_at(node);
                self.set_online(id, false)
            }
            Step::Online { node } => {
                let id = self.node_at(node);
                self.set_online(id, true)
            }
            Step::Crash { node, gap_secs } => {
                let id = self.node_at(node);
                self.crash(id, i64::from(gap_secs) * NANOS)
            }
            Step::MassDelete { node, fraction } => {
                let id = self.node_at(node);
                let paths = self.files_of(id, fraction);
                for p in paths {
                    self.delete_path(id, &p)?;
                }
                Ok(())
            }
            Step::MassModify {
                node,
                fraction,
                content,
            } => {
                let id = self.node_at(node);
                let paths = self.files_of(id, fraction);
                for p in paths {
                    self.write_file(id, &p, content_bytes(content), None)?;
                }
                Ok(())
            }
            Step::User {
                node,
                action,
                delay_secs,
            } => {
                let id = self.node_at(node);
                self.users.push(UserDue {
                    at: self.clock.plus_nanos(i64::from(delay_secs) * NANOS),
                    node: id,
                    action,
                });
                Ok(())
            }
            Step::Settle { secs } => {
                let end = self.clock.plus_nanos(i64::from(secs) * NANOS);
                self.run_until(end)
            }
        }
    }

    /// A file path a step names, as the step spells it (§7.6): with the
    /// knob's probability its file name is in upper case, `F3` for `f3`.
    /// Only file names vary, never directories, so the paths of a pair
    /// differ in their last component. The draw is on `CASE_STREAM`, and
    /// none is made with the knob at 0.
    fn spell(&mut self, name: String) -> RelPath {
        let p = self.knobs.case_variants;
        if p > 0.0 && self.case_rng.random::<f64>() < p {
            self.stats.upper_spellings += 1;
            let leaf = name.rfind('/').map_or(0, |i| i + 1);
            let (dir, file) = name.split_at(leaf);
            return rel(&format!("{dir}{}", file.to_ascii_uppercase()));
        }
        rel(&name)
    }

    /// The name on `id`'s disk that its user reaches by `path`: `path`
    /// itself, or, on a filesystem that ignores case, the spelling already
    /// there (§7.6). Writing to `F3` where `f3` exists changes `f3`.
    fn on_disk(&self, id: NodeId, path: RelPath) -> RelPath {
        self.nodes
            .get(&id)
            .filter(|n| n.case_insensitive && !n.fs.contains_key(&path))
            .and_then(|n| spelled_otherwise(n, &path))
            .unwrap_or(path)
    }

    /// §7.6: two paths that differ only by case cannot both exist on a
    /// filesystem that ignores case, `status` names the pair, and the user
    /// resolves it on a case-sensitive machine. In every round of the final
    /// phase the user of the first case-sensitive node does. Of each set of
    /// names in its folder that differ only by case, it keeps the one that
    /// sorts last, which is the vocabulary's own lower case, and deletes
    /// the others. And for every pair a case-insensitive node's wants name
    /// as colliding, if its folder holds the colliding path but not the
    /// wanted one, it deletes the colliding path: the wanted one may exist
    /// nowhere it can see, only as a record a `revert` restored (§8.3).
    fn resolve_case_pairs(&mut self) -> Result<(), Failure> {
        if self.knobs.case_variants <= 0.0 {
            return Ok(());
        }
        let Some(id) = self
            .order
            .iter()
            .copied()
            .find(|id| self.nodes.get(id).is_some_and(|n| !n.case_insensitive))
        else {
            return Ok(());
        };
        let mut spellings: BTreeMap<String, Vec<RelPath>> = BTreeMap::new();
        for path in self
            .nodes
            .get(&id)
            .map(|n| n.fs.keys())
            .into_iter()
            .flatten()
        {
            spellings.entry(fold(path)).or_default().push(path.clone());
        }
        for (_, mut paths) in spellings {
            // In path order already; the last is kept.
            paths.pop();
            for path in paths {
                self.delete_path(id, &path)?;
            }
        }
        let named: Vec<(RelPath, RelPath)> = self
            .nodes
            .values()
            .filter(|n| n.case_insensitive)
            .filter_map(|n| n.engine.as_ref()?.folder(self.folder))
            .flat_map(|f| f.wants().iter())
            .filter_map(|w| match &w.state {
                WantState::Collides { with, .. } => Some((w.path().clone(), with.clone())),
                _ => None,
            })
            .collect();
        for (wanted, with) in named {
            self.resolve_named(id, &wanted, &with)?;
        }
        Ok(())
    }

    /// The user of case-sensitive `id` resolves the pair a case-insensitive
    /// node names, `wanted` colliding with `with`, if its folder holds
    /// `with` but not `wanted`: it deletes `with`, and `wanted` lands.
    fn resolve_named(
        &mut self,
        id: NodeId,
        wanted: &RelPath,
        with: &RelPath,
    ) -> Result<(), Failure> {
        let holds = |p: &RelPath| self.nodes.get(&id).is_some_and(|n| n.fs.contains_key(p));
        if holds(with) && !holds(wanted) {
            self.delete_path(id, with)?;
        }
        Ok(())
    }

    /// The first `fraction` percent of a node's files, in path order.
    fn files_of(&self, id: NodeId, fraction: u8) -> Vec<RelPath> {
        let Some(n) = self.nodes.get(&id) else {
            return Vec::new();
        };
        let files: Vec<RelPath> =
            n.fs.iter()
                .filter(|(_, f)| f.kind == Kind::File)
                .map(|(p, _)| p.clone())
                .collect();
        let count = files.len() * usize::from(fraction.min(100)) / 100;
        files.into_iter().take(count).collect()
    }

    fn ensure_parent(&mut self, id: NodeId, path: &RelPath) -> Result<(), Failure> {
        if let Some(parent) = path.parent() {
            let exists = self
                .nodes
                .get(&id)
                .is_some_and(|n| n.fs.contains_key(&parent));
            if !exists {
                self.mkdir(id, &parent)?;
            }
        }
        Ok(())
    }

    fn mkdir(&mut self, id: NodeId, dir: &RelPath) -> Result<(), Failure> {
        let exists = self
            .nodes
            .get(&id)
            .is_some_and(|n| n.fs.get(dir).is_some_and(|f| f.kind == Kind::Dir));
        if exists {
            return Ok(());
        }
        // Whatever was there gives way to the directory (a user's rm + mkdir).
        self.delete_path(id, dir)?;
        if let Some(n) = self.nodes.get_mut(&id) {
            n.fs.insert(
                dir.clone(),
                File {
                    kind: Kind::Dir,
                    content: Vec::new(),
                    mtime_ns: 0,
                    exec: false,
                },
            );
        }
        self.watch(id, dir.clone())
    }

    fn write_file(
        &mut self,
        id: NodeId,
        path: &RelPath,
        content: Vec<u8>,
        exec: Option<bool>,
    ) -> Result<(), Failure> {
        if !self.nodes.contains_key(&id) {
            return Ok(());
        }
        let path = &self.on_disk(id, path.clone());
        self.ensure_parent(id, path)?;
        let now = self.now_for(id).as_unix_nanos();
        if let Some(n) = self.nodes.get_mut(&id) {
            // A directory in the way is removed with its children first.
            if n.fs.get(path).is_some_and(|f| f.kind == Kind::Dir) {
                let under: Vec<RelPath> =
                    n.fs.keys()
                        .filter(|p| path.is_ancestor_of(p))
                        .cloned()
                        .collect();
                for p in under {
                    n.fs.remove(&p);
                }
            }
            let exec = exec.unwrap_or_else(|| n.fs.get(path).is_some_and(|f| f.exec));
            n.fs.insert(
                path.clone(),
                File {
                    kind: Kind::File,
                    content,
                    mtime_ns: now,
                    exec,
                },
            );
        }
        self.watch(id, path.clone())
    }

    fn delete_path(&mut self, id: NodeId, path: &RelPath) -> Result<(), Failure> {
        let path = &self.on_disk(id, path.clone());
        let removed: Vec<RelPath> = match self.nodes.get_mut(&id) {
            Some(n) => {
                let mut gone: Vec<RelPath> =
                    n.fs.keys()
                        .filter(|p| *p == path || path.is_ancestor_of(p))
                        .cloned()
                        .collect();
                gone.sort();
                for p in &gone {
                    n.fs.remove(p);
                }
                gone
            }
            None => Vec::new(),
        };
        for p in removed.into_iter().rev() {
            self.watch(id, p)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- the user

    fn user_act(&mut self, id: NodeId, action: UserAction) -> Result<(), Failure> {
        let Some(node) = self.nodes.get(&id) else {
            return Ok(());
        };
        let Some(engine) = &node.engine else {
            return Ok(());
        };
        let Some(f) = engine.folder(self.folder) else {
            return Ok(());
        };
        let held: Vec<BatchId> = f.quarantine().items().map(|i| i.batch).collect();
        let paused = f.paused().map(|p| p.batch);
        let folder = self.folder;
        match action {
            UserAction::ApproveAll => {
                for batch in held {
                    self.stats.approvals += 1;
                    self.approved.insert((id, batch));
                    self.feed(id, Event::Approve { folder, batch })?;
                }
                if let Some(batch) = paused {
                    self.stats.approvals += 1;
                    self.approved.insert((id, batch));
                    self.feed(id, Event::Approve { folder, batch })?;
                }
            }
            // `deny` and `revert` may be queued until the folder settles
            // (§8.3); their effects are tracked from the actions whenever
            // they run (see `act`).
            UserAction::DenyAll => {
                for batch in held {
                    self.feed(id, Event::Deny { folder, batch })?;
                }
            }
            UserAction::Revert => {
                if paused.is_some() {
                    self.feed(id, Event::Revert { folder })?;
                }
            }
            UserAction::Rules {
                hold_count,
                hold_pct,
            } => {
                let rules = Rules {
                    hold_count,
                    hold_pct,
                    ..self.rules.clone()
                };
                // The host keeps the rules with the folder (§11), written in
                // the same group as the engine's writes for the change.
                self.open_group(id);
                let staged = Staged {
                    write: TableWrite::Rules(rules.clone()),
                    at: self.clock,
                    event: self.stats.events,
                    moves: self.nodes.get(&id).map_or(0, |n| n.moves.len()),
                };
                if let Some(group) = self.nodes.get_mut(&id).and_then(|n| n.group.as_mut()) {
                    group.writes.push(staged);
                }
                self.feed(id, Event::RulesChanged { folder, rules })?;
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- views for invariants

    pub(crate) fn folder_id(&self) -> FolderId {
        self.folder
    }

    pub(crate) fn node_ids(&self) -> &[NodeId] {
        &self.order
    }

    pub(crate) fn engine(&self, id: NodeId) -> Option<&Engine> {
        self.nodes.get(&id).and_then(|n| n.engine.as_ref())
    }

    pub(crate) fn fs(&self, id: NodeId) -> Option<&BTreeMap<RelPath, File>> {
        self.nodes.get(&id).map(|n| &n.fs)
    }

    pub(crate) fn trash(&self, id: NodeId) -> &[ContentHash] {
        self.nodes.get(&id).map_or(&[], |n| n.trash.as_slice())
    }

    pub(crate) fn announced(&self) -> &BTreeSet<ContentHash> {
        &self.announced
    }

    /// True if `entry`'s author reverted its path (§8.3) after `entry` was
    /// first seen at event `seen`: the record was pending and never
    /// announced, so no other node ever held it. Such records are not
    /// evidence of anything the mesh saw (I3, I7).
    fn discarded_since(&self, entry: &Entry, seen: u64) -> bool {
        self.reverted_at
            .get(&(entry.modified_by, entry.path.clone()))
            .is_some_and(|(reverted, restored)| {
                *reverted >= seen
                    && restored
                        .as_ref()
                        .is_none_or(|kept| !kept.dominates_or_equals(&entry.version))
            })
    }

    /// True if `entry`, as recorded in the version table, was discarded by
    /// its author's revert before anyone else could see it.
    pub(crate) fn discarded_by_revert(&self, entry: &Entry) -> bool {
        let seen = self
            .seen_at
            .get(&entry.path)
            .and_then(|v| v.iter().find(|(ver, _)| *ver == entry.version))
            .map_or(u64::MAX, |(_, at)| *at);
        self.discarded_since(entry, seen)
    }

    /// Content `id` adopted through sync, with when, and when its user last
    /// touched each path (I2).
    pub(crate) fn synced(&self, id: NodeId) -> Synced<'_> {
        static EMPTY: BTreeMap<RelPath, Timestamp> = BTreeMap::new();
        match self.nodes.get(&id) {
            Some(n) => (n.synced.as_slice(), &n.local_edit_at),
            None => (&[], &EMPTY),
        }
    }

    pub(crate) fn versions(&self) -> &BTreeMap<RelPath, Vec<Entry>> {
        &self.versions
    }

    /// Records a revert discarded and a later write replaced in
    /// [`Sim::versions`]; still losing versions for I4.
    pub(crate) fn superseded(&self) -> &BTreeMap<RelPath, Vec<Entry>> {
        &self.superseded
    }

    pub(crate) fn failure(&self, invariant: &str, detail: String) -> Failure {
        self.fail(invariant, detail)
    }

    fn fingerprint(&self) -> Vec<u8> {
        let mut h = self.log.clone();
        for id in &self.order {
            if let Some(n) = self.nodes.get(id) {
                if let Some(state) = n.engine.as_ref().and_then(|e| e.folder(self.folder))
                    && let Ok(bytes) = postcard::to_stdvec(state)
                {
                    h.update(&bytes);
                }
                if let Ok(bytes) = postcard::to_stdvec(&n.fs) {
                    h.update(&bytes);
                }
                if let Ok(bytes) = postcard::to_stdvec(&n.trash) {
                    h.update(&bytes);
                }
            }
        }
        h.finalize().as_bytes().to_vec()
    }
}

/// What I2 reads per node: content adopted through sync with when, and when
/// the user last touched each path.
pub(crate) type Synced<'a> = (
    &'a [(ContentHash, RelPath, Timestamp)],
    &'a BTreeMap<RelPath, Timestamp>,
);

fn link_key(a: NodeId, b: NodeId) -> (NodeId, NodeId) {
    if a <= b { (a, b) } else { (b, a) }
}

fn rel(s: &str) -> RelPath {
    // The vocabulary is fixed and every name in it is valid; fall back to a
    // known-good path rather than panic if that ever changes.
    RelPath::new(s).unwrap_or_else(|_| {
        RelPath::new("f0").unwrap_or_else(|_| unreachable!("f0 is a valid path"))
    })
}

/// An event's kind and the path it names, if any, for a failure's detail.
fn event_name(event: &Event) -> String {
    match event {
        Event::Scanned { path, state, .. } => {
            let state = match state {
                ScanState::Absent => "Absent",
                ScanState::Unchanged => "Unchanged",
                ScanState::Observed(_) => "Observed",
                ScanState::Skipped { .. } => "Skipped",
            };
            format!("Scanned {path} {state}")
        }
        Event::Fetched { path, outcome, .. } => format!("Fetched {path} {outcome:?}"),
        Event::FetchProgress { path, .. } => format!("FetchProgress {path}"),
        Event::Applied { path, outcome, .. } => format!("Applied {path} {outcome:?}"),
        other => {
            let name = format!("{other:?}");
            name.split([' ', '{', '(']).next().unwrap_or("").to_owned()
        }
    }
}

/// A verified fetch of `version` at `path` lands in its own temp file,
/// replacing only an earlier fetch of the same version.
fn put_temp(node: &mut Node, path: &RelPath, version: &Version, bytes: Vec<u8>) {
    let files = node.temp.entry(path.clone()).or_default();
    files.retain(|(v, _)| v != version);
    files.push((version.clone(), bytes));
}

/// The commit of `version` at `path` takes its temp file, if there is one.
fn take_temp(node: &mut Node, path: &RelPath, version: &Version) -> Option<Vec<u8>> {
    let files = node.temp.get_mut(path)?;
    let at = files.iter().position(|(v, _)| v == version)?;
    let (_, bytes) = files.remove(at);
    if files.is_empty() {
        node.temp.remove(path);
    }
    Some(bytes)
}

/// The temp file for `version` at `path`, if there is one.
#[cfg(test)]
fn temp_of<'a>(node: &'a Node, path: &RelPath, version: &Version) -> Option<&'a Vec<u8>> {
    node.temp
        .get(path)?
        .iter()
        .find(|(v, _)| v == version)
        .map(|(_, bytes)| bytes)
}

/// A path as a filesystem that ignores case sees it (§7.6): the names in
/// the simulation are ASCII, so lower case is enough.
fn fold(path: &RelPath) -> String {
    path.as_str().to_ascii_lowercase()
}

/// True if a rename to `path` on `node` would find it taken: something is
/// there, under this spelling or, on a filesystem that ignores case, any
/// other.
fn occupied(node: &Node, path: &RelPath) -> bool {
    node.fs.contains_key(path) || occupied_otherwise(node, path)
}

/// True if `node` ignores case and holds `path` under another spelling.
fn occupied_otherwise(node: &Node, path: &RelPath) -> bool {
    node.case_insensitive && spelled_otherwise(node, path).is_some()
}

/// On a filesystem that ignores case, the other spelling of `path` that is
/// on disk, if any.
fn spelled_otherwise(node: &Node, path: &RelPath) -> Option<RelPath> {
    let folded = fold(path);
    node.fs
        .keys()
        .find(|p| *p != path && fold(p) == folded)
        .cloned()
}

/// The live record in `node`'s tables at a path that differs from `path`
/// only by case, if any (§7.5 step 6).
fn collides(node: &Node, path: &RelPath) -> Option<RelPath> {
    let folded = fold(path);
    node.persisted
        .records
        .values()
        .find(|r| !r.entry.deleted && r.entry.path != *path && fold(&r.entry.path) == folded)
        .map(|r| r.entry.path.clone())
}

/// The commit guard (§7.5 step 6): what the engine believes is on disk
/// against what is, by the scan fast path's predicate.
fn expected_matches(file: Option<&File>, expected: Option<&Observed>) -> bool {
    match (file, expected) {
        (None, None) => true,
        (Some(f), Some(e)) => e.unchanged_by_stat(&f.observed()),
        _ => false,
    }
}

/// Where the folder rebuilt from `parts` first differs from `crashed`, the
/// folder as it stood at the crash, once both have been through a restart
/// at `now` (§11); `None` if nowhere.
fn restart_difference(
    parts: &FolderParts,
    mut crashed: FolderState,
    config: &NodeConfig,
    now: Timestamp,
) -> Option<&'static str> {
    let mut rebuilt =
        FolderState::from_parts(parts.clone(), config.node_id, config.author_host.clone());
    rebuilt.restarted(now);
    crashed.restarted(now);
    // Both want-lists noted every want the restart touched: bookkeeping for
    // the hooks, not state, drained before comparing.
    rebuilt.want_changes();
    crashed.want_changes();
    rebuilt.first_difference(&crashed)
}

/// Sync moved the file with `hash` from `from` to `to`: to a conflict-copy
/// path (§7.6), or back from one when the journal undoes a displacement
/// (§7.5). The node's adoptions of that content follow it there, dated
/// now, so that its user's later edit or deletion of the copy counts as the
/// user's own change and not as sync's loss (I2, §14.1).
fn follow(node: &mut Node, from: &RelPath, to: &RelPath, hash: ContentHash, now: Timestamp) {
    node.moves.push((hash, from.clone(), to.clone(), now));
    for (h, path, at) in &mut node.synced {
        if *h == hash && path == from {
            *path = to.clone();
            *at = now;
        }
    }
}

/// The index record the host last wrote at `path`, durable or not. A real
/// host reads its own uncommitted writes, so its scan's fast path compares
/// against these (§7.3, §11), not against what a crash would leave.
fn written_record<'a>(node: &'a Node, path: &RelPath) -> Option<&'a IndexRecord> {
    let staged = node.group.iter().flat_map(|g| g.writes.iter().rev());
    for s in staged {
        let TableWrite::Hook(action) = &s.write else {
            continue;
        };
        match action.as_ref() {
            Action::IndexChanged { record, .. } if record.entry.path == *path => {
                return Some(record);
            }
            Action::IndexRemoved { path: p, .. } if p == path => return None,
            _ => {}
        }
    }
    node.persisted.records.get(path)
}

fn trash_file(node: &mut Node, file: File) {
    if file.kind != Kind::Dir {
        node.trash.push(file.hash());
    }
}

/// True if something lies under `path`: it is a non-empty directory.
fn has_children(node: &Node, path: &RelPath) -> bool {
    node.fs.keys().any(|p| path.is_ancestor_of(p))
}

/// §7.5 step 7: move what is at `path` aside, to the trash or to a
/// conflict-copy path, with one rename. With `subtree`, a directory takes
/// everything under it, as a real rename does (§7.6); without, only its own
/// entry moves. Returns what moved, at the paths it had.
fn displace_path(
    node: &mut Node,
    path: &RelPath,
    to: &Displace,
    subtree: bool,
) -> Vec<(RelPath, File)> {
    let moving: Vec<RelPath> = node
        .fs
        .keys()
        .filter(|p| *p == path || (subtree && path.is_ancestor_of(p)))
        .cloned()
        .collect();
    let mut moved = Vec::new();
    for from in moving {
        let at = match to {
            Displace::Trash => None,
            Displace::ConflictCopy(target) => match rebase(&from, path, target) {
                Some(at) => Some(at),
                None => continue,
            },
        };
        let Some(file) = node.fs.remove(&from) else {
            continue;
        };
        match at {
            None => trash_file(node, file.clone()),
            Some(at) => {
                node.fs.insert(at, file.clone());
            }
        }
        moved.push((from, file));
    }
    moved
}

/// §7.5 step 7 behind the commit journal: a row that names what is at
/// `path` and where it goes is made durable, then it moves there with one
/// rename (the row is written after the move here, which is the same thing
/// in a step no crash can split). False, and no row, if the path is empty.
///
/// Content moved to a conflict-copy path is followed there at once (I2,
/// see `follow`), not when the row goes: while it is open the node may
/// crash, and its user may edit or delete the moved file before the
/// restart undoes the row, which is then the user's own change.
fn displace_journalled(
    node: &mut Node,
    path: &RelPath,
    to: &Displace,
    subtree: bool,
    now: Timestamp,
) -> bool {
    if !node.fs.contains_key(path) {
        return false;
    }
    let trash_at = node.trash.len();
    let moved = displace_path(node, path, to, subtree);
    if let Displace::ConflictCopy(target) = to {
        follow_moved(node, path, target, &moved, now);
    }
    node.journal.push(JournalRow {
        path: path.clone(),
        to: to.clone(),
        moved,
        trash_at,
    });
    true
}

/// Undo a displacement whose rename never happened (§7.5): what the row
/// moved aside goes back to its path. A real host renames it back without
/// replacing anything, so if the path was taken meanwhile (a user can edit
/// a folder while its daemon is down) whatever moved stays where it went,
/// in the trash or at the conflict-copy path, and the next scan sees both.
/// What comes back from a conflict-copy path, as it now is, takes the
/// adoptions of its content back with it (I2).
fn undo(node: &mut Node, row: JournalRow, now: Timestamp) {
    if occupied(node, &row.path) {
        return;
    }
    match &row.to {
        Displace::Trash => {
            let hashes: Vec<ContentHash> = row
                .moved
                .iter()
                .filter(|(_, f)| f.kind != Kind::Dir)
                .map(|(_, f)| f.hash())
                .collect();
            let end = row.trash_at + hashes.len();
            // Nothing else writes to a node's trash between a displacement
            // and its undo, so the files are where the row says. Should that
            // ever not hold, the files stay in the trash rather than be
            // duplicated.
            if node.trash.get(row.trash_at..end) != Some(hashes.as_slice()) {
                return;
            }
            node.trash.drain(row.trash_at..end);
            for (from, file) in row.moved {
                node.fs.insert(from, file);
            }
        }
        Displace::ConflictCopy(target) => {
            let under: Vec<RelPath> = node
                .fs
                .keys()
                .filter(|p| *p == target || target.is_ancestor_of(p))
                .cloned()
                .collect();
            for at in under {
                let Some(back) = rebase(&at, target, &row.path) else {
                    continue;
                };
                if let Some(file) = node.fs.remove(&at) {
                    follow(node, &at, &back, file.hash(), now);
                    node.fs.insert(back, file);
                }
            }
        }
    }
}

/// `follow` for everything a rename of `path` to `target` moved.
fn follow_moved(
    node: &mut Node,
    path: &RelPath,
    target: &RelPath,
    moved: &[(RelPath, File)],
    now: Timestamp,
) {
    for (from, file) in moved {
        if let Some(to) = rebase(from, path, target) {
            follow(node, from, &to, file.hash(), now);
        }
    }
}

/// `p`, which is `from` or lies under it, after a rename of `from` to `to`.
fn rebase(p: &RelPath, from: &RelPath, to: &RelPath) -> Option<RelPath> {
    let rest = p.as_str().strip_prefix(from.as_str())?;
    RelPath::new(format!("{}{rest}", to.as_str())).ok()
}

fn move_to_trash(node: &mut Node, path: &RelPath) {
    let under: Vec<RelPath> = node
        .fs
        .keys()
        .filter(|p| *p == path || path.is_ancestor_of(p))
        .cloned()
        .collect();
    for p in under {
        if let Some(f) = node.fs.remove(&p) {
            trash_file(node, f);
        }
    }
}

/// For the checker's unit tests (§14.4: every fix has one, the checker's
/// own fixes included). A test builds the history an invariant checks
/// through the bookkeeping a run uses, instead of hoping a seed reaches it.
#[cfg(test)]
impl Sim {
    /// `id`'s table write `action` becomes durable at event `event`, as at
    /// the end of its group (§11). The node's tables then no longer match
    /// its engine, so a test makes these writes after its node's last event.
    pub(crate) fn durable(
        &mut self,
        id: NodeId,
        action: Action,
        event: u64,
    ) -> Result<(), Failure> {
        let at = self.clock;
        let moves = self.nodes.get(&id).map_or(0, |n| n.moves.len());
        self.record(
            id,
            Staged {
                write: TableWrite::Hook(Box::new(action)),
                at,
                event,
                moves,
            },
        )
    }

    /// `file` is at `path` on `id`'s disk, and `id`'s engine has scanned it.
    pub(crate) fn scanned(&mut self, id: NodeId, path: RelPath, file: File) -> Result<(), Failure> {
        let state = ScanState::Observed(file.observed());
        if let Some(n) = self.nodes.get_mut(&id) {
            n.fs.insert(path.clone(), file);
        }
        let folder = self.folder;
        self.feed(
            id,
            Event::Scanned {
                folder,
                path,
                state,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file the engine expects at a commit, as the scan last saw it.
    fn file(content: u8, exec: bool) -> File {
        File {
            kind: Kind::File,
            content: content_bytes(content),
            mtime_ns: 1_700_000_000 * NANOS,
            exec,
        }
    }

    /// A chmod changes neither size nor mtime, so a guard on those alone
    /// let a commit displace a file its user had just chmodded, and the
    /// conflict copy carried the user's exec bit under the loser's name
    /// (d12fec3). The guard is the scan fast path's predicate, which
    /// compares the exec bit (§7.5 step 6). The pins
    /// `a_chmod_under_a_pending_commit_is_changed_underneath` and
    /// `..._in_a_shorter_run` guard this only while their seeds reach it.
    #[test]
    fn the_commit_guard_refuses_a_file_whose_exec_bit_changed() {
        let expected = file(3, false).observed();
        assert!(expected_matches(Some(&file(3, false)), Some(&expected)));
        assert!(!expected_matches(Some(&file(3, true)), Some(&expected)));
        // The other way round too: expected executable, found not.
        let expected = file(3, true).observed();
        assert!(!expected_matches(Some(&file(3, false)), Some(&expected)));
    }

    /// The same for a retarget, which changes nothing about a symlink but
    /// its target (d12fec3, §7.5 step 6). The targets have the same length,
    /// so only the target itself tells them apart. The pins
    /// `a_retarget_under_a_pending_commit_is_changed_underneath` and
    /// `..._in_another_run` guard this only while their seeds reach it.
    #[test]
    fn the_commit_guard_refuses_a_symlink_whose_target_changed() {
        let link = |target: &[u8]| File {
            kind: Kind::Symlink,
            content: target.to_vec(),
            mtime_ns: 0,
            exec: false,
        };
        let expected = link(b"f1").observed();
        assert!(expected_matches(Some(&link(b"f1")), Some(&expected)));
        assert!(!expected_matches(Some(&link(b"f2")), Some(&expected)));
    }

    /// A world of two nodes, for tests that need a `Node` or the `Sim`'s
    /// bookkeeping and none of its history.
    fn world() -> Sim {
        Sim::new(
            0,
            Knobs {
                nodes: Some(2),
                ..Knobs::default()
            },
        )
    }

    /// When sync moves content a node adopted to a conflict copy, the
    /// adoption moves with it, dated at the move, so the user's later edit
    /// or deletion of the copy is the user's own change and not sync's loss
    /// (I2, 031e7aa). Here a directory is displaced with a file inside,
    /// which is the rename `follow` is called for, file by file. Only the
    /// adoption of the moved content at the moved path follows. The pins
    /// `a_conflict_copy_its_user_rewrote_is_not_lost`,
    /// `..._removed_with_its_directory_...` and `..._mass_deleted_...`
    /// guard this only while their seeds reach it.
    #[test]
    fn an_adoption_follows_its_content_to_the_conflict_copy() {
        let mut sim = world();
        let id = sim.order[0];
        let moved = file(4, false);
        let other = file(5, false).hash();
        let (dir, at, elsewhere) = (rel("d1"), rel("d1/f2"), rel("f3"));
        let copy = rel("d1.conflict-20231114-221320-n1");
        let before = Timestamp::from_unix_nanos(1_700_000_000 * NANOS);
        let now = before.plus_nanos(NANOS);
        let node = sim.nodes.get_mut(&id).unwrap();
        node.synced = vec![
            (moved.hash(), at.clone(), before),
            (other, at.clone(), before),
            (moved.hash(), elsewhere.clone(), before),
        ];
        follow_moved(node, &dir, &copy, &[(at.clone(), moved.clone())], now);
        let (synced, _) = sim.synced(id);
        assert_eq!(
            synced,
            [
                (moved.hash(), rel("d1.conflict-20231114-221320-n1/f2"), now),
                (other, at, before),
                (moved.hash(), elsewhere, before),
            ]
        );
    }

    /// A journalled displacement moves the content to its conflict-copy
    /// path at once, so the adoption follows it then (I2, §7.5): the node
    /// may crash with the row open, and its user may rewrite or delete the
    /// moved file before the restart undoes the row, which is the user's own
    /// change. What the undo brings back takes the adoption back with it.
    #[test]
    fn an_adoption_follows_a_journalled_displacement_and_its_undo() {
        let mut sim = world();
        let id = sim.order[0];
        let (at, copy) = (rel("d1/f2"), rel("d1/f2.conflict-20231114-221320-n1"));
        let adopted = file(4, false);
        let (before, displaced, undone) = (
            Timestamp::from_unix_nanos(1_700_000_000 * NANOS),
            Timestamp::from_unix_nanos(1_700_000_010 * NANOS),
            Timestamp::from_unix_nanos(1_700_000_020 * NANOS),
        );
        let to = Displace::ConflictCopy(copy.clone());
        let displace = |sim: &mut Sim| {
            let node = sim.nodes.get_mut(&id).unwrap();
            node.fs.insert(at.clone(), adopted.clone());
            node.synced = vec![(adopted.hash(), at.clone(), before)];
            assert!(displace_journalled(node, &at, &to, false, displaced));
            node.journal.pop().unwrap()
        };

        // Undone untouched: the content and its adoption are back.
        let row = displace(&mut sim);
        assert_eq!(
            sim.synced(id).0,
            [(adopted.hash(), copy.clone(), displaced)]
        );
        undo(sim.nodes.get_mut(&id).unwrap(), row, undone);
        assert_eq!(sim.synced(id).0, [(adopted.hash(), at.clone(), undone)]);
        crate::invariants::i2_node(&sim, id).unwrap();

        // Deleted by its user while the row was open: the user's change.
        let row = displace(&mut sim);
        let node = sim.nodes.get_mut(&id).unwrap();
        node.fs.remove(&copy);
        node.local_edit_at
            .insert(copy.clone(), displaced.plus_nanos(NANOS));
        undo(node, row, undone);
        assert_eq!(
            sim.synced(id).0,
            [(adopted.hash(), copy.clone(), displaced)]
        );
        crate::invariants::i2_node(&sim, id).unwrap_or_else(|f| panic!("{f}"));
    }

    /// An adoption joins `synced` once its write is durable (§11), which
    /// can be after a commit of the node's has already moved the content to
    /// a conflict copy with its directory (§7.6): here the adoption of `f2`
    /// is written, the displacement of `d1` moves it, and only then is the
    /// write durable. The entry starts where the content is, at the copy,
    /// dated by the move, so that its user's deletion there is the user's
    /// own change (I2). A move made before the write is not replayed, even
    /// of the same content from the same path: the write landed it again.
    #[test]
    fn an_adoption_durable_after_its_content_moved_starts_where_it_went() {
        let mut sim = world();
        let (id, peer) = (sim.order[0], sim.order[1]);
        let folder = sim.folder;
        let (dir, at) = (rel("d1"), rel("d1/f2"));
        let copy = rel("d1.conflict-20231114-221320-n1");
        let adopted = written(&at, peer, Version::from_iter([(peer, 1)]), 4, 0);
        let moved = File {
            mtime_ns: adopted.mtime_ns,
            ..file(4, false)
        };
        let now = Timestamp::from_unix_nanos(1_700_000_100 * NANOS);
        let node = sim.nodes.get_mut(&id).unwrap();
        // Earlier, the same content at the path went to a copy of its own.
        follow(node, &at, &rel("d1/f2.conflict-x"), adopted.hash, now);
        let staged = Staged {
            write: TableWrite::Hook(Box::new(changed(folder, adopted.clone(), 1))),
            at: now,
            event: 1,
            moves: node.moves.len(),
        };
        let later = now.plus_nanos(NANOS);
        follow_moved(node, &dir, &copy, &[(at.clone(), moved)], later);
        sim.record(id, staged).unwrap_or_else(|f| panic!("{f}"));
        let (synced, _) = sim.synced(id);
        assert_eq!(
            synced,
            [(
                adopted.hash,
                rel("d1.conflict-20231114-221320-n1/f2"),
                later
            )]
        );
    }

    /// A revert discards only the versions its node wrote above the record
    /// it puts back, which were pending; the restored record and those below
    /// it were announced and stand (§8.3, 878e417). This is the shape the
    /// pin `a_revert_discards_only_the_versions_above_the_restored_record`
    /// reaches: node e's revert puts back its live `d0` {b: 2, e: 3}, which
    /// dominates node b's tombstone {b: 2, e: 2}. Taking the restored record
    /// for a discarded one left the tombstone looking uncontradicted, and I3
    /// reported `d0` resurrected. Three seeds in 10,000 reached it when it
    /// was pinned, so the pin will likely go stale again; this test guards
    /// the checker whether or not a seed reaches it.
    #[test]
    fn a_revert_sets_aside_only_the_versions_above_the_record_it_restored() {
        let mut sim = world();
        let (b, e) = (sim.order[0], sim.order[1]);
        let d0 = rel("d0");
        let v = |at_b: u64, at_e: u64| Version::from_iter([(b, at_b), (e, at_e)]);
        let dir = |by: NodeId, version: Version, deleted: bool| Entry {
            path: d0.clone(),
            kind: Kind::Dir,
            size: 0,
            mtime_ns: 0,
            stamp: 1,
            exec: false,
            hash: ContentHash::EMPTY,
            prev_hash: ContentHash::EMPTY,
            version,
            deleted,
            modified_by: by,
            author_host: HostName::empty(),
        };
        let below = dir(e, v(1, 1), false);
        let tombstone = dir(b, v(2, 2), true);
        let restored = dir(e, v(2, 3), false);
        let pending = dir(e, v(2, 4), false);
        // Seen at events 1 to 4, in that order; e reverts at event 10.
        sim.seen_at.insert(
            d0.clone(),
            [&below, &tombstone, &restored, &pending]
                .iter()
                .zip(1..)
                .map(|(entry, at)| (entry.version.clone(), at))
                .collect(),
        );
        sim.reverted_at
            .insert((e, d0.clone()), (10, Some(restored.version.clone())));

        assert!(sim.discarded_by_revert(&pending));
        assert!(!sim.discarded_by_revert(&restored));
        assert!(!sim.discarded_by_revert(&below));
        // Another node's record is not e's revert's to discard.
        assert!(!sim.discarded_by_revert(&tombstone));
        // So the tombstone is contradicted by a record that stands, which
        // is what I3 asks.
        assert!(restored.version.dominates(&tombstone.version));
        // A record first seen after the revert is new, whatever it
        // dominates.
        let later = dir(e, v(2, 5), false);
        sim.seen_at
            .entry(d0.clone())
            .or_default()
            .push((later.version.clone(), 11));
        assert!(!sim.discarded_by_revert(&later));
        // A revert that removed the record, at a path peers never saw,
        // restored nothing, and everything e wrote there before it goes.
        sim.reverted_at.insert((e, d0), (10, None));
        assert!(sim.discarded_by_revert(&restored));
        assert!(sim.discarded_by_revert(&below));
        assert!(!sim.discarded_by_revert(&later));
    }

    /// A file record at `path` that `by` wrote, holding `file(content)`
    /// with an mtime `secs` seconds after it.
    fn written(path: &RelPath, by: NodeId, version: Version, content: u8, secs: i64) -> Entry {
        let f = File {
            mtime_ns: (1_700_000_000 + secs) * NANOS,
            ..file(content, false)
        };
        Entry {
            path: path.clone(),
            kind: Kind::File,
            size: f.content.len() as u64,
            mtime_ns: f.mtime_ns,
            stamp: f.mtime_ns,
            exec: false,
            hash: f.hash(),
            prev_hash: ContentHash::EMPTY,
            version,
            deleted: false,
            modified_by: by,
            author_host: HostName::empty(),
        }
    }

    /// The index write of `entry` under `seq`.
    fn changed(folder: FolderId, entry: Entry, seq: u64) -> Action {
        Action::IndexChanged {
            folder,
            record: IndexRecord { entry, seq },
        }
    }

    /// I7's exemption for a vector a revert discarded (§14.1, §8.3,
    /// 15d13ea): the discarded record was pending, so only its author ever
    /// held it, and its vector is free to be reached again with other
    /// content, by the author's next change or by any node's merge that
    /// includes it. Here e writes {b: 1, e: 1} over b's file and reverts to
    /// b's record; b's merge then reaches {b: 1, e: 1} with other content,
    /// and takes the discarded record's place. The pin
    /// `a_vector_a_revert_discarded_may_be_reached_again_by_a_merge` guards
    /// this only while its seed reaches it.
    #[test]
    fn a_merge_may_reach_a_vector_its_authors_revert_discarded() {
        let mut sim = world();
        let (b, e) = (sim.order[0], sim.order[1]);
        let folder = sim.folder;
        let f = rel("f");
        let base = written(&f, b, Version::from_iter([(b, 1)]), 1, 0);
        let pending = written(&f, e, Version::from_iter([(b, 1), (e, 1)]), 2, 1);
        let merged = written(&f, b, pending.version.clone(), 3, 2);
        // e holds b's file, writes over it, and reverts: the record it puts
        // back keeps its old seq.
        sim.durable(e, changed(folder, base.clone(), 1), 1).unwrap();
        sim.durable(e, changed(folder, pending.clone(), 2), 2)
            .unwrap();
        sim.durable(e, changed(folder, base.clone(), 1), 3).unwrap();
        assert!(sim.discarded_by_revert(&pending));
        sim.durable(b, changed(folder, merged.clone(), 1), 4)
            .unwrap_or_else(|f| panic!("{f}"));
        assert_eq!(sim.versions()[&f], [base, merged]);
        assert_eq!(sim.superseded()[&f], [pending]);
    }

    /// A vector a revert discarded and its author reached again replaces
    /// the discarded record in the version table even when the content is
    /// the same (§8.3, 8d4620d): the new record has its own mtime, and I4
    /// checks conflict-copy names, which carry the loser's mtime, against
    /// the records that exist. The pin
    /// `a_reissued_vector_replaces_the_discarded_record_whatever_its_content`
    /// guards this only while its seed reaches it.
    #[test]
    fn a_reissued_vector_replaces_the_discarded_record_with_the_same_content() {
        let mut sim = world();
        let e = sim.order[1];
        let folder = sim.folder;
        let f = rel("f");
        let v = Version::from_iter([(e, 1)]);
        let first = written(&f, e, v.clone(), 1, 0);
        let again = written(&f, e, v, 1, 5);
        assert!(first.same_content(&again));
        // e adds f, reverts before announcing it, and adds it again.
        sim.durable(e, changed(folder, first.clone(), 1), 1)
            .unwrap();
        let removed = Action::IndexRemoved {
            folder,
            path: f.clone(),
        };
        sim.durable(e, removed, 2).unwrap();
        sim.durable(e, changed(folder, again.clone(), 2), 3)
            .unwrap();
        assert_eq!(sim.versions()[&f], [again]);
        assert_eq!(sim.superseded()[&f], [first]);
    }

    /// Content is requested by hash (§7.5 steps 2 and 3, 5c20f3d), so a
    /// source serves it from its live file at the path whatever that
    /// record's version, and failing that from any live file that holds it.
    /// A conflict's merged version exists nowhere until someone merges, and
    /// the winner's holders hold its content under an older version. The
    /// pin `content_is_fetched_by_hash_from_whoever_holds_it` guards this
    /// only while its seed reaches it.
    #[test]
    fn a_source_serves_the_wanted_content_under_any_version_or_path() {
        let mut sim = world();
        let (asker, source) = (sim.order[0], sim.order[1]);
        let content = file(6, false);
        sim.scanned(source, rel("w"), content.clone()).unwrap();
        let held = sim
            .engine(source)
            .and_then(|e| e.folder(sim.folder))
            .and_then(|f| f.index().live(&rel("w")))
            .map(|r| r.entry.version.clone())
            .unwrap();
        let merged = held.merge(&Version::from_iter([(asker, 1)]));
        for path in [rel("w"), rel("elsewhere")] {
            sim.ops.push(Op::Fetch {
                node: asker,
                from: source,
                path: path.clone(),
                version: merged.clone(),
                hash: content.hash(),
                done_at: sim.clock,
                next_progress: sim.clock,
                corrupt: false,
            });
            let pos = sim.ops.len() - 1;
            sim.progress_op(pos).unwrap();
            assert_eq!(
                temp_of(&sim.nodes[&asker], &path, &merged),
                Some(&content.content),
                "{path} is served"
            );
        }
    }

    /// A directory entry, as the simulated disk holds one.
    fn dir() -> File {
        File {
            kind: Kind::Dir,
            content: Vec::new(),
            mtime_ns: 0,
            exec: false,
        }
    }

    /// The skip checker (§7.3). While it watches a node, a tombstone of
    /// that node's own under a new `seq`, raising its counter above the
    /// record that was live at a path reported `Skipped` or beneath a
    /// skipped directory, is a failure. Nothing else is: a path no skip
    /// covers, a record put back by a revert under its old `seq`, a merge
    /// after one that keeps the node's counter, a peer's tombstone, a live
    /// record, or another node's write.
    #[test]
    fn the_skip_checker_flags_a_tombstone_a_skip_covers() {
        let mut sim = world();
        let (id, other) = (sim.order[0], sim.order[1]);
        let mut check = SkipCheck::new(id);
        check.skipped = [rel("d0"), rel("f1"), rel("d1"), rel("f3")].into();
        check.beneath = [rel("d0"), rel("d2")].into();
        check.live = [
            (rel("d0/f4"), 0),
            (rel("f1"), 0),
            (rel("d1/f5"), 0),
            (rel("f2"), 0),
            (rel("f3"), 1),
        ]
        .into();
        check.seq = 10;
        sim.checking = Some(check);
        let folder = sim.folder;
        let record = |path: &str, seq: u64, by: NodeId, deleted: bool| Action::IndexChanged {
            folder,
            record: IndexRecord {
                entry: Entry {
                    path: rel(path),
                    kind: Kind::File,
                    size: 0,
                    mtime_ns: 0,
                    stamp: 1,
                    exec: false,
                    hash: ContentHash::EMPTY,
                    prev_hash: ContentHash::EMPTY,
                    version: Version::from_iter([(by, 1)]),
                    deleted,
                    modified_by: by,
                    author_host: HostName::empty(),
                },
                seq,
            },
        };
        let flagged = |sim: &Sim, action: Action| sim.tombstones_a_skip(id, &action);
        assert!(
            flagged(&sim, record("f1", 11, id, true)).is_some(),
            "skipped"
        );
        let beneath = flagged(&sim, record("d0/f4", 11, id, true)).unwrap();
        assert!(beneath.contains("beneath d0"), "{beneath}");
        for (action, why) in [
            (record("d1/f5", 11, id, true), "d1 skipped as a file"),
            (record("f2", 11, id, true), "no skip covers f2"),
            (record("d0/f6", 11, id, true), "d0/f6 was not live"),
            (record("f1", 10, id, true), "a revert's old seq"),
            (
                record("f3", 11, id, true),
                "a merge that keeps the node's counter",
            ),
            (record("f1", 11, other, true), "a peer's tombstone"),
            (record("f1", 11, id, false), "a live record"),
        ] {
            assert_eq!(flagged(&sim, action), None, "{why}");
        }
        let seen_by_other = record("f1", 11, id, true);
        assert_eq!(sim.tombstones_a_skip(other, &seen_by_other), None);
        sim.checking = None;
        assert_eq!(
            flagged(&sim, record("f1", 11, id, true)),
            None,
            "not watching"
        );
    }

    /// §7.5: from the engine's pause on a full disk until the host reports
    /// space recovered, the folder's inbound is paused. The checker flags
    /// every fetch and commit the engine asks for meanwhile, and any record
    /// of another node's version it writes, except in the report of a
    /// commit asked for before the pause or an observation (a landing,
    /// §8.3); its own versions, and a revert's records, which keep their
    /// `seq`, are not inbound. Nothing is flagged while not paused.
    #[test]
    fn the_disk_full_checker_holds_a_paused_engine_to_its_pause() {
        let mut sim = world();
        let (id, other) = (sim.order[0], sim.order[1]);
        let folder = sim.folder;
        let entry = written(&rel("n"), other, Version::from_iter([(other, 1)]), 1, 0);
        let fetch = Action::Fetch {
            folder,
            path: rel("n"),
            version: entry.version.clone(),
            hash: entry.hash,
            size: entry.size,
            from: other,
        };
        let commits = [
            Action::Write {
                folder,
                path: rel("n"),
                entry: entry.clone(),
                expected: None,
                displace: Displace::Trash,
            },
            Action::Remove {
                folder,
                path: rel("n"),
                expected: None,
                displace: Displace::Trash,
            },
            Action::SetMeta {
                folder,
                path: rel("n"),
                expected: None,
                mtime_ns: 1,
                exec: false,
            },
        ];
        assert_eq!(sim.starts_inbound_while_paused(id, &fetch), None);
        sim.nodes.get_mut(&id).unwrap().paused_inbound = true;
        for action in std::iter::once(&fetch).chain(&commits) {
            let detail = sim.starts_inbound_while_paused(id, action);
            assert!(
                detail
                    .as_ref()
                    .is_some_and(|d| d.contains("while its disk was full")),
                "{action:?}: {detail:?}"
            );
        }
        let adopt = changed(folder, entry.clone(), 5);
        let feeding = |may_adopt| Feeding {
            node: id,
            may_adopt,
            seq: 4,
        };
        sim.feeding = Some(feeding(false));
        assert!(sim.starts_inbound_while_paused(id, &adopt).is_some());
        let own = written(&rel("n"), id, Version::from_iter([(id, 1)]), 1, 0);
        assert_eq!(
            sim.starts_inbound_while_paused(id, &changed(folder, own, 5)),
            None
        );
        let reverted = changed(folder, entry, 4);
        assert_eq!(sim.starts_inbound_while_paused(id, &reverted), None);
        sim.feeding = Some(feeding(true));
        assert_eq!(sim.starts_inbound_while_paused(id, &adopt), None);
        assert_eq!(sim.starts_inbound_while_paused(other, &fetch), None);
    }

    /// §7.5: a full disk reported by the fetch or commit its want waited
    /// for pauses the folder's inbound, so after such a report the engine
    /// must be paused. A report the engine ignores (the want had moved on)
    /// need not pause it, nor need any other error.
    #[test]
    fn a_full_disk_the_want_waited_for_must_pause_the_inbound() {
        let mut sim = world();
        let id = sim.order[0];
        let full = LocalError::DiskFull;
        let failure = sim
            .after_local_failure(id, &full, true, "commit of n")
            .unwrap_err();
        assert_eq!(failure.invariant, "disk full");
        assert!(failure.detail.contains("commit of n"), "{}", failure.detail);
        sim.after_local_failure(id, &full, false, "commit of n")
            .unwrap();
        sim.after_local_failure(id, &LocalError::Io, true, "commit of n")
            .unwrap();
    }

    /// §7.5: a failed commit leaves the disk as it found it. The checker
    /// compares the folder and trash with what they were before the commit,
    /// and wants no journal row left open.
    #[test]
    fn the_failed_commit_checker_flags_a_disk_it_changed() {
        let mut sim = world();
        let id = sim.order[0];
        let snapshot = |sim: &Sim| {
            let n = &sim.nodes[&id];
            Some((n.fs.clone(), n.trash.clone()))
        };
        let path = rel("n");
        let io = LocalError::Io;
        let before = snapshot(&sim);
        sim.left_as_found(id, &path, &io, before.clone()).unwrap();
        sim.nodes
            .get_mut(&id)
            .unwrap()
            .fs
            .insert(path.clone(), file(1, false));
        let failure = sim.left_as_found(id, &path, &io, before).unwrap_err();
        assert_eq!(failure.invariant, "failed commit");
        let before = snapshot(&sim);
        sim.nodes
            .get_mut(&id)
            .unwrap()
            .trash
            .push(file(2, false).hash());
        assert!(sim.left_as_found(id, &path, &io, before).is_err());
        assert!(sim.left_as_found(id, &path, &io, None).is_err());
    }

    /// §7.6 on the simulated disk: a filesystem that ignores case holds one
    /// spelling of a name. Its user's write to another spelling changes the
    /// file there; a rename to another spelling finds the name taken; and a
    /// commit collides with a live record in its tables at another
    /// spelling, never with a tombstone. A case-sensitive disk has none of
    /// this.
    #[test]
    fn a_disk_that_ignores_case_holds_one_spelling_of_a_name() {
        let mut sim = world();
        let (id, other) = (sim.order[0], sim.order[1]);
        let (lower, upper) = (rel("d1/f3"), rel("d1/F3"));
        for n in [id, other] {
            sim.nodes
                .get_mut(&n)
                .unwrap()
                .fs
                .insert(lower.clone(), file(1, false));
        }
        sim.nodes.get_mut(&id).unwrap().case_insensitive = true;
        assert_eq!(sim.on_disk(id, upper.clone()), lower);
        assert_eq!(sim.on_disk(id, rel("d1/F4")), rel("d1/F4"));
        assert_eq!(sim.on_disk(other, upper.clone()), upper);
        assert!(occupied(&sim.nodes[&id], &upper));
        assert!(!occupied(&sim.nodes[&other], &upper));

        let record = |deleted| IndexRecord {
            entry: Entry {
                deleted,
                ..written(&lower, id, Version::from_iter([(id, 1)]), 1, 0)
            },
            seq: 1,
        };
        let node = sim.nodes.get_mut(&id).unwrap();
        node.persisted.records.insert(lower.clone(), record(false));
        assert_eq!(collides(node, &upper), Some(lower.clone()));
        assert_eq!(collides(node, &lower), None, "its own spelling");
        node.persisted.records.insert(lower.clone(), record(true));
        assert_eq!(collides(node, &upper), None, "a tombstone");
    }

    /// §7.6: the user resolves a pair a case-insensitive node names on a
    /// case-sensitive machine. Where that machine holds the colliding path
    /// but not the wanted one, which may exist only as a record a revert
    /// restored (§8.3), its user deletes the colliding path; where it holds
    /// both, or only the wanted one, this leaves them.
    #[test]
    fn a_pair_named_as_colliding_is_resolved_on_a_case_sensitive_machine() {
        let mut sim = world();
        let id = sim.order[1];
        let (wanted, with) = (rel("F3"), rel("f3"));
        let holds = |sim: &Sim, p: &RelPath| sim.nodes[&id].fs.contains_key(p);
        sim.nodes
            .get_mut(&id)
            .unwrap()
            .fs
            .insert(with.clone(), file(1, false));
        sim.nodes
            .get_mut(&id)
            .unwrap()
            .fs
            .insert(wanted.clone(), file(2, false));
        sim.resolve_named(id, &wanted, &with).unwrap();
        assert!(holds(&sim, &with) && holds(&sim, &wanted), "both: left");
        sim.nodes.get_mut(&id).unwrap().fs.remove(&wanted);
        sim.resolve_named(id, &wanted, &with).unwrap();
        assert!(!holds(&sim, &with), "only the colliding one: deleted");
    }

    /// §7.5: a commit is in flight from the moment the engine asks for it
    /// until the host reports it, and at most one is in flight per path. The
    /// checker flags a second commit of a path while one is in flight, and a
    /// commit whose want stopped committing before its report, unless one of
    /// the two rules that release a want early did it: the re-classification
    /// of a conflict-copy path while the commit that made the copy is
    /// reported (§7.5), or an observation of a path carrying the restoring
    /// mark (§7.5's revert exception, §8.3). They release the want, not the
    /// path: a second commit is flagged after them too. Another path or
    /// another node is no second commit.
    #[test]
    fn the_in_flight_checker_flags_a_second_commit_and_a_release() {
        let mut sim = world();
        let (id, other) = (sim.order[0], sim.order[1]);
        let folder = sim.folder;
        let write = |path: &str, n: u64| Action::Write {
            folder,
            path: rel(path),
            entry: written(&rel(path), other, Version::from_iter([(other, n)]), 1, 0),
            expected: None,
            displace: Displace::Trash,
        };
        let v = |n: u64| Version::from_iter([(other, n)]);
        for (node, path) in [(id, "n"), (id, "m"), (id, "k"), (other, "n")] {
            sim.commit_started(node, &rel(path), &v(1), &write(path, 1))
                .unwrap();
        }
        let twice = sim
            .commit_started(id, &rel("n"), &v(2), &write("n", 2))
            .unwrap_err();
        assert_eq!(twice.invariant, "commit in flight");
        assert!(
            twice.detail.contains("started a commit of n"),
            "{}",
            twice.detail
        );

        // The engine has no want at all, so every commit above is released.
        let released = sim.holds_its_commits(id, "Tick").unwrap_err();
        assert!(
            released.detail.contains("released its commit of k"),
            "{}",
            released.detail
        );
        sim.committing.remove(&(id, rel("k")));
        sim.committing.get_mut(&(id, rel("n"))).unwrap().restoring = true;
        sim.observing = Some((id, rel("n")));
        assert!(
            sim.holds_its_commits(id, "Scanned").is_err(),
            "m's want carried no mark"
        );
        sim.copy_recorded = Some((id, rel("m")));
        sim.holds_its_commits(id, "Scanned").unwrap();
        sim.copy_recorded = None;
        sim.observing = None;
        assert!(sim.committing[&(id, rel("n"))].released);
        assert!(sim.committing[&(id, rel("m"))].released);
        sim.holds_its_commits(id, "Tick").unwrap();
        let after = sim
            .commit_started(id, &rel("m"), &v(2), &write("m", 2))
            .unwrap_err();
        assert!(after.detail.contains("released early"), "{}", after.detail);
        assert!(
            !sim.committing[&(other, rel("n"))].released,
            "another node's"
        );

        // Nor may the record of a held path change, by an adoption or
        // anything else, until the released commit's report.
        let record = |path: &str| Action::IndexChanged {
            folder,
            record: IndexRecord {
                entry: written(&rel(path), other, v(3), 1, 0),
                seq: 9,
            },
        };
        let changed = sim.changes_a_held_record(id, &record("m")).unwrap();
        assert!(changed.contains("changed the record of m"), "{changed}");
        assert_eq!(
            sim.changes_a_held_record(other, &record("n")),
            None,
            "not released"
        );
        assert_eq!(
            sim.changes_a_held_record(id, &record("j")),
            None,
            "not held"
        );
    }

    /// §7.3 through the model: a rule ignores the directory d0 and its
    /// user deletes d0/f4 meanwhile. The scan reports d0 `Skipped` and
    /// nothing beneath it, the engine keeps d0/f4 live, and the checker,
    /// which watches that bracket, is satisfied. The first scan after the
    /// rule goes observes the deletion.
    #[test]
    fn an_ignored_directory_keeps_the_records_beneath_it_through_a_scan() {
        let mut sim = world();
        let id = sim.order[0];
        let (d0, f4) = (rel("d0"), rel("d0/f4"));
        sim.scanned(id, d0.clone(), dir()).unwrap();
        sim.scanned(id, f4.clone(), file(3, false)).unwrap();
        let live = |sim: &Sim| {
            sim.engine(id)
                .and_then(|e| e.folder(sim.folder))
                .and_then(|f| f.index().live(&f4))
                .is_some()
        };
        assert!(live(&sim));
        let node = sim.nodes.get_mut(&id).unwrap();
        node.ignored.insert(d0.clone(), 1);
        node.fs.remove(&f4);
        sim.full_scan(id, false).unwrap();
        assert!(live(&sim), "kept beneath the ignored directory");
        assert!(sim.nodes[&id].ignored.is_empty(), "the rule has gone");
        sim.full_scan(id, false).unwrap();
        assert!(!live(&sim), "observed gone once the rule went");
    }

    /// §7.5 step 2 (draft 46): a source serves its file only if what is on
    /// disk still matches the record by the scan's fast-path test, size,
    /// mtime and exec bit for a file and kind and target for a symlink. A
    /// symlink retargeted since its record was served on its kind alone and
    /// failed the requester's hash check, and a file chmodded since its
    /// record was served too. Both are refused now; a file that still
    /// matches is served.
    #[test]
    fn a_source_serves_only_what_still_matches_its_record_by_the_fast_path() {
        let mut sim = world();
        let (asker, source) = (sim.order[0], sim.order[1]);
        let link = |target: &[u8]| File {
            kind: Kind::Symlink,
            content: target.to_vec(),
            mtime_ns: 0,
            exec: false,
        };
        sim.scanned(source, rel("l"), link(b"f1")).unwrap();
        sim.scanned(source, rel("x"), file(6, false)).unwrap();
        sim.scanned(source, rel("w"), file(7, false)).unwrap();
        // Behind the engine's back: a retarget to a target of the same
        // length, and a chmod.
        let node = sim.nodes.get_mut(&source).unwrap();
        node.fs.insert(rel("l"), link(b"f2"));
        node.fs.get_mut(&rel("x")).unwrap().exec = true;
        let fetch = |sim: &mut Sim, path: &str, hash: ContentHash| {
            let version = sim
                .engine(source)
                .and_then(|e| e.folder(sim.folder))
                .and_then(|f| f.index().live(&rel(path)))
                .map(|r| r.entry.version.clone())
                .unwrap();
            sim.ops.push(Op::Fetch {
                node: asker,
                from: source,
                path: rel(path),
                version: version.clone(),
                hash,
                done_at: sim.clock,
                next_progress: sim.clock,
                corrupt: false,
            });
            let pos = sim.ops.len() - 1;
            sim.progress_op(pos).unwrap();
            temp_of(&sim.nodes[&asker], &rel(path), &version).is_some()
        };
        let before = sim.stats.clone();
        assert!(!fetch(&mut sim, "l", link(b"f1").hash()));
        assert_eq!(
            (sim.stats.not_available, sim.stats.mismatches),
            (before.not_available + 1, before.mismatches),
            "the retargeted symlink is refused, not served with its new target"
        );
        assert!(!fetch(&mut sim, "x", file(6, false).hash()), "chmodded");
        assert_eq!(sim.stats.not_available, before.not_available + 2);
        assert!(fetch(&mut sim, "w", file(7, false).hash()), "unchanged");
    }
}
