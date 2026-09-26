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
//! (§11), which after every event must equal the engine's parts.
//!
//! Time is discrete-event: the loop always jumps to the earliest pending
//! thing (a wake, a delivery, an operation, a scan, a restart, a user
//! action) and processes it in a fixed order, so a run is a pure function
//! of the seed, the knobs and the steps.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use delocal_engine::batch::BatchRole;
use delocal_engine::folder::{ApplyOutcome, Displace, FolderStatus, ScanState};
use delocal_engine::want::{FetchReport, Tier, Want, WantState};
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
    online: bool,
    restart_at: Option<Timestamp>,
    fs: BTreeMap<RelPath, File>,
    trash: Vec<ContentHash>,
    persisted: Persisted,
    wake_at: Option<Timestamp>,
    next_scan_at: Timestamp,
    /// Fetched, verified content waiting for its commit, per path.
    temp: BTreeMap<RelPath, (Version, Vec<u8>)>,
    // ---- invariant tracking ----
    /// Every version this node has ever sent in a batch, per path.
    sent: BTreeMap<RelPath, Vec<Version>>,
    /// Content this node adopted through sync, with the path and when (I2).
    /// An entry follows its content when sync moves it to a conflict copy.
    synced: Vec<(ContentHash, RelPath, Timestamp)>,
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

/// The whole simulated world. See the module docs.
pub struct Sim {
    seed: u64,
    knobs: Knobs,
    rng: ChaCha8Rng,
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
                online: true,
                restart_at: None,
                fs: BTreeMap::new(),
                trash: Vec::new(),
                persisted: Persisted::default(),
                wake_at: None,
                next_scan_at: clock.plus_nanos(rng.random_range(30 * NANOS..600 * NANOS)),
                temp: BTreeMap::new(),
                sent: BTreeMap::new(),
                synced: Vec::new(),
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
        let mut sim = Self {
            seed,
            knobs,
            rng,
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
            for id in &ids {
                self.full_scan(*id, false)?;
            }
            let end = self.clock.plus_nanos(FINAL_ROUND_NANOS);
            self.run_until(end)?;
            if self.quiescent() {
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
                    " {}: window={:?} held={} paused={} deferred=[{}] wants=[{}];",
                    Self::short(n.id),
                    f.window().map(|w| w.due()),
                    f.quarantine().len(),
                    f.paused().is_some(),
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
    fn feed(&mut self, id: NodeId, event: Event) -> Result<(), Failure> {
        let now = self.now_for(id);
        let actions = {
            let Some(node) = self.nodes.get_mut(&id) else {
                return Ok(());
            };
            let Some(engine) = node.engine.as_mut() else {
                return Ok(());
            };
            engine.handle(now, event)
        };
        if let Ok(bytes) = postcard::to_stdvec(&actions) {
            self.log.update(&bytes);
        }
        for action in actions {
            self.act(id, action)?;
        }
        self.check_persisted(id)
    }

    /// After every event, the tables the hooks wrote must be the engine's
    /// parts (§11): a missing or wrong hook fails at the event that should
    /// have reported the change, not at some later restart.
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

    fn act(&mut self, id: NodeId, action: Action) -> Result<(), Failure> {
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
            Action::Send { to, payload } => self.send(id, to, payload)?,
            Action::Fetch {
                path,
                version,
                hash,
                size,
                from,
                ..
            } => {
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
            Action::Write {
                ref path,
                ref entry,
                ..
            } => {
                let done_at = self
                    .clock
                    .plus_nanos(self.rng.random_range(50_000_000..500_000_000));
                self.ops.push(Op::Commit {
                    node: id,
                    path: path.clone(),
                    version: entry.version.clone(),
                    action: Box::new(action),
                    done_at,
                    suspended: false,
                });
            }
            Action::Remove { ref path, .. } | Action::SetMeta { ref path, .. } => {
                let version = self
                    .nodes
                    .get(&id)
                    .and_then(|n| n.engine.as_ref())
                    .and_then(|e| e.folder(self.folder))
                    .and_then(|f| f.wants().get(path))
                    .map(|w| w.version().clone())
                    .unwrap_or_default();
                let done_at = self
                    .clock
                    .plus_nanos(self.rng.random_range(50_000_000..500_000_000));
                self.ops.push(Op::Commit {
                    node: id,
                    path: path.clone(),
                    version,
                    action: Box::new(action),
                    done_at,
                    suspended: false,
                });
            }
            Action::MoveToTrash { path, .. } => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    move_to_trash(n, &path);
                }
            }
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
                            let now_ev = self.stats.events;
                            let seen = self.seen_at.entry(path.clone()).or_default();
                            match seen.iter_mut().find(|(v, _)| *v == record.entry.version) {
                                Some(slot) => slot.1 = now_ev,
                                None => seen.push((record.entry.version.clone(), now_ev)),
                            }
                        }
                    }
                    None => {
                        list.push(record.entry.clone());
                        let now_ev = self.stats.events;
                        self.seen_at
                            .entry(path.clone())
                            .or_default()
                            .push((record.entry.version.clone(), now_ev));
                    }
                }
                let now = self.clock;
                let events = self.stats.events;
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
                        n.synced
                            .push((record.entry.hash, record.entry.path.clone(), now));
                    }
                    n.persisted
                        .records
                        .insert(record.entry.path.clone(), record);
                }
                if let Some(version) = restored {
                    self.reverted_at.insert((id, path), (events, Some(version)));
                }
            }
            Action::IndexRemoved { path, .. } => {
                // Only `revert` removes a record: a path peers never saw.
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.persisted.records.remove(&path);
                }
                self.reverted_at
                    .insert((id, path), (self.stats.events, None));
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
            // Not kept yet: the simulator persists the parts it restarts
            // from, which so far are the index and the wants.
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
        if let Some(n) = self.nodes.get_mut(&id) {
            n.engine = None;
            n.temp.clear();
            n.wake_at = None;
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
        let Some(node) = self.nodes.get_mut(&id) else {
            return Ok(());
        };
        node.restart_at = None;
        // §11: the persisted parts and nothing else.
        node.engine = Some(Engine::restore(config, vec![parts], now));
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
                // `path` if its live record there has that hash and the disk
                // still matches the record, failing that from any live file
                // with that hash, failing that not at all.
                let served = self.nodes.get(&from).and_then(|src| {
                    let engine = src.engine.as_ref()?;
                    let index = engine.folder(self.folder)?.index();
                    index.locate(&path, &hash).find_map(|at| {
                        let record = index.live(at)?;
                        let file = src.fs.get(at)?;
                        let matches = file.kind == record.entry.kind
                            && (file.kind != Kind::File
                                || (file.content.len() as u64 == record.entry.size
                                    && file.mtime_ns == record.entry.mtime_ns));
                        matches.then(|| file.content.clone())
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
                        if hash_bytes(&bytes) == hash {
                            if let Some(n) = self.nodes.get_mut(&node) {
                                n.temp.insert(path.clone(), (version.clone(), bytes));
                            }
                            FetchReport::Ok
                        } else {
                            self.stats.mismatches += 1;
                            FetchReport::HashMismatch
                        }
                    }
                };
                self.feed(
                    node,
                    Event::Fetched {
                        folder: self.folder,
                        path,
                        hash,
                        version,
                        outcome: report,
                    },
                )
            }
            Op::Commit {
                node,
                path,
                version,
                action,
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
                let (outcome, created) = self.commit(node, &path, &version, &action);
                // The host's mkdir of missing parents is a filesystem event
                // like any other: the watcher may report it, the scan will.
                for dir in created {
                    self.watch(node, dir)?;
                }
                if outcome == ApplyOutcome::ChangedUnderneath {
                    self.stats.changed_underneath += 1;
                }
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
                self.feed(
                    node,
                    Event::Applied {
                        folder: self.folder,
                        path,
                        version,
                        outcome,
                    },
                )
            }
        }
    }

    /// §7.5 steps 6 to 9 against the simulated filesystem. Also returns the
    /// parent directories the host had to create for a write.
    fn commit(
        &mut self,
        id: NodeId,
        path: &RelPath,
        version: &Version,
        action: &Action,
    ) -> (ApplyOutcome, Vec<RelPath>) {
        let now = self.clock;
        let Some(node) = self.nodes.get_mut(&id) else {
            return (ApplyOutcome::ChangedUnderneath, Vec::new());
        };
        // §7.5 step 6: the same guard before every commit, SetMeta included.
        let expected = match action {
            Action::Write { expected, .. }
            | Action::Remove { expected, .. }
            | Action::SetMeta { expected, .. } => expected.as_ref(),
            _ => None,
        };
        if !expected_matches(node.fs.get(path), expected) {
            return (ApplyOutcome::ChangedUnderneath, Vec::new());
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
                    Some(_) => return (ApplyOutcome::ChangedUnderneath, Vec::new()),
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
                if let Displace::ConflictCopy(target) = displace
                    && node.fs.contains_key(target)
                {
                    return (ApplyOutcome::ChangedUnderneath, created);
                }
                let content = match entry.kind {
                    Kind::Dir => Some(Vec::new()),
                    _ => match node.temp.get(path) {
                        Some((v, _)) if v == version => {
                            node.temp.remove(path).map(|(_, bytes)| bytes)
                        }
                        _ => None,
                    },
                };
                let Some(content) = content else {
                    // No verified temp file: the host cannot commit this.
                    return (ApplyOutcome::ChangedUnderneath, created);
                };
                if let Some(existing) = node.fs.remove(path) {
                    match displace {
                        Displace::Trash => trash_file(node, existing),
                        Displace::ConflictCopy(target) => {
                            follow(node, path, target, existing.hash(), now);
                            node.fs.insert(target.clone(), existing);
                        }
                    }
                }
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
                if let Some(existing) = node.fs.get(path)
                    && existing.kind == Kind::Dir
                    && node.fs.keys().any(|p| path.is_ancestor_of(p))
                {
                    return (ApplyOutcome::ChangedUnderneath, created); // not empty
                }
                if let Displace::ConflictCopy(target) = displace
                    && node.fs.contains_key(target)
                {
                    return (ApplyOutcome::ChangedUnderneath, created);
                }
                if let Some(existing) = node.fs.remove(path) {
                    match displace {
                        Displace::Trash => trash_file(node, existing),
                        Displace::ConflictCopy(target) => {
                            follow(node, path, target, existing.hash(), now);
                            node.fs.insert(target.clone(), existing);
                        }
                    }
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
        (outcome, created)
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

    /// A full scan (§7.3) with the fast path against the persisted records.
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
                let fast = node
                    .persisted
                    .records
                    .get(path)
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
        self.feed(
            id,
            Event::ScanStarted {
                folder: self.folder,
            },
        )?;
        for (i, (path, state)) in entries.into_iter().enumerate() {
            if abort_at == Some(i) {
                return self.feed(
                    id,
                    Event::ScanAborted {
                        folder: self.folder,
                    },
                );
            }
            self.feed(
                id,
                Event::Scanned {
                    folder: self.folder,
                    path,
                    state,
                },
            )?;
        }
        self.feed(
            id,
            Event::ScanFinished {
                folder: self.folder,
            },
        )
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
                self.write_file(id, &rel(&path_name(path)), content_bytes(content), None)
            }
            Step::Delete { node, path } => {
                let id = self.node_at(node);
                self.delete_path(id, &rel(&path_name(path)))
            }
            Step::Rename { node, from, to } => {
                let id = self.node_at(node);
                let from = rel(&path_name(from));
                let to = rel(&path_name(to));
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
                let path = rel(&path_name(path));
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
                let path = rel(&path_name(path));
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
                let path = rel(&path_name(path));
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
                let path = rel(&path_name(path));
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
                // The host keeps the rules with the folder (§11) and tells
                // the engine.
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.persisted.rules = rules.clone();
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

/// The commit guard (§7.5 step 6): what the engine believes is on disk
/// against what is, by the scan fast path's predicate.
fn expected_matches(file: Option<&File>, expected: Option<&Observed>) -> bool {
    match (file, expected) {
        (None, None) => true,
        (Some(f), Some(e)) => e.unchanged_by_stat(&f.observed()),
        _ => false,
    }
}

/// Sync moved the file with `hash` from `from` to the conflict-copy path
/// `to` (§7.6). The node's adoptions of that content follow it there, dated
/// now, so that its user's later edit or deletion of the copy counts as the
/// user's own change and not as sync's loss (I2, §14.1).
fn follow(node: &mut Node, from: &RelPath, to: &RelPath, hash: ContentHash, now: Timestamp) {
    for (h, path, at) in &mut node.synced {
        if *h == hash && path == from {
            *path = to.clone();
            *at = now;
        }
    }
}

fn trash_file(node: &mut Node, file: File) {
    if file.kind != Kind::Dir {
        node.trash.push(file.hash());
    }
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
