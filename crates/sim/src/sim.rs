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
//! `IndexChanged` and `WantChanged` are written to the persisted store.
//!
//! Time is discrete-event: the loop always jumps to the earliest pending
//! thing (a wake, a delivery, an operation, a scan, a restart, a user
//! action) and processes it in a fixed order, so a run is a pure function
//! of the seed, the knobs and the steps.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use delocal_engine::batch::BatchRole;
use delocal_engine::folder::{ApplyOutcome, Displace, Expected, FolderStatus, ScanState};
use delocal_engine::want::{FetchReport, Tier, Want, WantState};
use delocal_engine::{
    Action, BatchId, ContentHash, Engine, Entry, Event, FolderId, FolderState, HostName,
    IndexRecord, Kind, NodeConfig, NodeId, Observed, Outbound, RelPath, Rules, Timestamp, Version,
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

/// What the daemon would have on disk for one node (§11).
#[derive(Clone, Debug, Default, Serialize)]
struct Persisted {
    records: BTreeMap<RelPath, IndexRecord>,
    wants: BTreeMap<RelPath, Want>,
    snapshot: Option<FolderState>,
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
    synced: Vec<(ContentHash, RelPath, Timestamp)>,
    /// When this node's own user last edited or deleted each path (I2).
    local_edit_at: BTreeMap<RelPath, Timestamp>,
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
    links: BTreeMap<(NodeId, NodeId), Link>,
    messages: Vec<Message>,
    msg_seq: u64,
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
            ops: Vec::new(),
            users: Vec::new(),
            corruption_on: true,
            steps: Vec::new(),
            steps_applied: 0,
            stats: Stats::default(),
            announced: BTreeSet::new(),
            versions: BTreeMap::new(),
            log: blake3::Hasher::new(),
        };
        // Engines join the folder, then every pair connects.
        let ids = sim.order.clone();
        for id in &ids {
            let config = sim.config_of(*id);
            let mut engine = Engine::new(config);
            let _ = engine.handle(
                sim.now_for(*id),
                Event::FolderJoined {
                    folder,
                    rules: sim.rules.clone(),
                    members: ids.clone(),
                },
            );
            if let Some(node) = sim.nodes.get_mut(id) {
                node.engine = Some(engine);
            }
            // Ignore failures during construction: nothing has happened yet.
            let _ = sim.snapshot(*id);
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
                out.push_str(&format!(
                    " {}: window={:?} held={} paused={} deferred={} wants=[{}];",
                    Self::short(n.id),
                    f.window().map(|w| w.due()),
                    f.quarantine().len(),
                    f.paused().is_some(),
                    f.deferred().count(),
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
        self.snapshot(id)
    }

    /// Persist the folder state and check the incremental hooks against it.
    fn snapshot(&mut self, id: NodeId) -> Result<(), Failure> {
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
        let records: BTreeMap<RelPath, IndexRecord> = state
            .index()
            .records()
            .map(|r| (r.entry.path.clone(), r.clone()))
            .collect();
        let wants: BTreeMap<RelPath, Want> = state
            .wants()
            .iter()
            .map(|w| (w.path().clone(), w.clone()))
            .collect();
        let records_ok = records == node.persisted.records;
        let wants_ok = wants == node.persisted.wants;
        let persisted_records = node.persisted.records.len();
        if records_ok && wants_ok {
            node.persisted.snapshot = Some(state.clone());
            return Ok(());
        }
        let short = Self::short(id);
        if !records_ok {
            return Err(self.fail(
                "persistence hooks",
                format!(
                    "{short}: IndexChanged did not reproduce the index; {persisted_records} persisted vs {} in the engine",
                    records.len()
                ),
            ));
        }
        Err(self.fail(
            "persistence hooks",
            format!("{short}: WantChanged did not reproduce the want-list"),
        ))
    }

    fn act(&mut self, id: NodeId, action: Action) -> Result<(), Failure> {
        match action {
            Action::WakeAt(t) => {
                // The engine speaks the node's skewed clock; the host keeps
                // global time.
                if let Some(n) = self.nodes.get_mut(&id) {
                    let global = t.plus_nanos(-n.skew_ns);
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
                // I5: a version still in quarantine is never adopted. Approve
                // releases the item first, so after an approval this is clear.
                if record.entry.modified_by != id
                    && self
                        .engine(id)
                        .and_then(|e| e.folder(self.folder))
                        .is_some_and(|f| {
                            f.quarantine()
                                .versions_at(&record.entry.path)
                                .contains(&record.entry.version)
                        })
                {
                    return Err(self.fail(
                        "I5 brake",
                        format!(
                            "{} adopted {} version {:?} while that version was quarantined",
                            Self::short(id),
                            record.entry.path,
                            record.entry.version
                        ),
                    ));
                }
                if record.entry.path.file_name().contains(".conflict-")
                    && record.entry.modified_by == id
                    && !record.entry.deleted
                {
                    self.stats.conflict_copies += 1;
                }
                let list = self.versions.entry(record.entry.path.clone()).or_default();
                if !list.iter().any(|e| e.version == record.entry.version) {
                    list.push(record.entry.clone());
                }
                let now = self.clock;
                if let Some(n) = self.nodes.get_mut(&id) {
                    if record.entry.modified_by != id
                        && !record.entry.deleted
                        && record.entry.kind != Kind::Dir
                    {
                        n.synced
                            .push((record.entry.hash, record.entry.path.clone(), now));
                    }
                    n.persisted
                        .records
                        .insert(record.entry.path.clone(), record);
                }
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
            Action::StatusChanged { status, .. } => match status {
                FolderStatus::Paused { .. } => {
                    if let Some(n) = self.nodes.get_mut(&id) {
                        n.paused = true;
                    }
                }
                FolderStatus::Unpaused { .. } | FolderStatus::Reverted { .. } => {
                    if let Some(n) = self.nodes.get_mut(&id) {
                        n.paused = false;
                    }
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
        let config = self.config_of(id);
        let folder = self.folder;
        let rules = self.rules.clone();
        let members = self.order.clone();
        let Some(node) = self.nodes.get_mut(&id) else {
            return Ok(());
        };
        node.restart_at = None;
        let engine = match node.persisted.snapshot.clone() {
            Some(state) => Engine::restore(config, vec![state]),
            None => {
                let mut e = Engine::new(config);
                let _ = e.handle(
                    self.clock,
                    Event::FolderJoined {
                        folder,
                        rules,
                        members,
                    },
                );
                e
            }
        };
        node.engine = Some(engine);
        node.next_scan_at = self
            .clock
            .plus_nanos(self.rng.random_range(NANOS..10 * NANOS));
        // Restoring may have changed want states (transient ones return to
        // wanted); the persisted want table must follow.
        if let Some(state) = node.engine.as_ref().and_then(|e| e.folder(folder)) {
            node.persisted.wants = state
                .wants()
                .iter()
                .map(|w| (w.path().clone(), w.clone()))
                .collect();
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
                            version,
                        },
                    );
                }
                self.ops.remove(pos);
                if !self.connected(node, from) {
                    return Ok(());
                }
                let served = self.nodes.get(&from).and_then(|src| {
                    let engine = src.engine.as_ref()?;
                    let f = engine.folder(self.folder)?;
                    if !f.index().has_exact(&path, &version) {
                        return None;
                    }
                    let file = src.fs.get(&path)?;
                    (file.kind != Kind::Dir).then(|| file.content.clone())
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
                self.ops.remove(pos);
                if !self.nodes.get(&node).is_some_and(Node::alive) {
                    return Ok(());
                }
                let outcome = self.commit(node, &path, &version, &action);
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

    /// §7.5 steps 6 to 9 against the simulated filesystem.
    fn commit(
        &mut self,
        id: NodeId,
        path: &RelPath,
        version: &Version,
        action: &Action,
    ) -> ApplyOutcome {
        let Some(node) = self.nodes.get_mut(&id) else {
            return ApplyOutcome::ChangedUnderneath;
        };
        let expected = match action {
            Action::Write { expected, .. } | Action::Remove { expected, .. } => *expected,
            _ => None,
        };
        if !matches!(action, Action::SetMeta { .. })
            && !expected_matches(node.fs.get(path), expected)
        {
            return ApplyOutcome::ChangedUnderneath;
        }
        match action {
            Action::Write {
                entry, displace, ..
            } => {
                if let Displace::ConflictCopy(target) = displace
                    && node.fs.contains_key(target)
                {
                    return ApplyOutcome::ChangedUnderneath;
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
                    return ApplyOutcome::ChangedUnderneath;
                };
                if let Some(existing) = node.fs.remove(path) {
                    match displace {
                        Displace::Trash => trash_file(node, existing),
                        Displace::ConflictCopy(target) => {
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
            Action::Remove { .. } => {
                if let Some(existing) = node.fs.get(path)
                    && existing.kind == Kind::Dir
                    && node.fs.keys().any(|p| path.is_ancestor_of(p))
                {
                    return ApplyOutcome::ChangedUnderneath; // not empty
                }
                if let Some(existing) = node.fs.remove(path) {
                    trash_file(node, existing);
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
        }
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
                let fast = node.persisted.records.get(path).is_some_and(|r| {
                    !r.entry.deleted
                        && r.entry.kind == file.kind
                        && (file.kind != Kind::File
                            || (r.entry.size == file.content.len() as u64
                                && r.entry.mtime_ns == file.mtime_ns))
                });
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
                    self.feed(id, Event::Approve { folder, batch })?;
                }
                if let Some(batch) = paused {
                    self.stats.approvals += 1;
                    self.feed(id, Event::Approve { folder, batch })?;
                }
            }
            UserAction::DenyAll => {
                for batch in held {
                    self.stats.denials += 1;
                    self.feed(id, Event::Deny { folder, batch })?;
                }
            }
            UserAction::Revert => {
                if paused.is_some() {
                    self.stats.reverts += 1;
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

fn expected_matches(file: Option<&File>, expected: Option<Expected>) -> bool {
    match (file, expected) {
        (None, None) => true,
        (Some(f), Some(e)) => {
            f.kind == e.kind
                && (f.kind != Kind::File
                    || (f.content.len() as u64 == e.size && f.mtime_ns == e.mtime_ns))
        }
        _ => false,
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
