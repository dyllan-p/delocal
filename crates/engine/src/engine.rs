//! The engine (DESIGN.md §7 intro): one `Engine` per node that consumes
//! [`Event`]s and returns [`Action`]s, and does nothing else.
//!
//! The host stamps every call with the time and generates every fresh
//! identifier; the engine never reads a clock or a random number generator.
//! Everything it returns is a pure function of its state and the event, and
//! every iteration order is fixed (folders by `FolderId`, members and peers
//! by `NodeId`, batch entries by `seq`, apply sets by path), so the same
//! inputs give byte-identical outputs (§14.1, I6).
//!
//! Phase 1 fills this in one step at a time. Variants marked with a PR
//! number below are defined now, so the interface is reviewed once, and
//! handled or emitted from that PR on.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::batch::{ApplySet, Batch, BatchDecision, BatchRole, Decision};
use crate::entry::{ContentHash, Entry};
use crate::folder::{ApplyOutcome, FolderState, FolderStatus, ScanState};
use crate::id::{BatchId, FolderId, HostName, NodeId};
use crate::index::IndexRecord;
use crate::path::RelPath;
use crate::rules::Rules;
use crate::time::Timestamp;
use crate::version::Version;

/// What the host tells the engine about this machine at start-up.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeConfig {
    /// From `node.json` (§5).
    pub node_id: NodeId,
    /// This machine's Tailscale hostname, carried as `author_host` (§7.1).
    pub author_host: HostName,
}

/// How a peer is currently reached (§6.4). Input only in Phase 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    Lan,
    Direct,
    Relay,
}

/// The result of a host's fetch (§7.5 steps 1 to 5). PR 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FetchOutcome {
    Ok,
    /// The source no longer has that exact version.
    NotAvailable,
    /// Content did not verify against the expected hash.
    HashMismatch,
}

/// What the index believes is at a path when a commit is ordered (§7.5
/// step 6). `None` means absent. PR 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Expected {
    pub size: u64,
    pub mtime_ns: i64,
}

/// Where a commit moves the file it displaces (§7.5 step 7). PR 6.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Displace {
    /// To `.delocal/trash/` (§8.4).
    Trash,
    /// To the conflict-copy path: the displaced file is the losing content (§7.6).
    ConflictCopy(RelPath),
}

/// A message for a peer. The binary crate wraps it in its wire envelope (§12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outbound {
    Batch(Batch),
    Decision(BatchDecision),
}

/// Everything the host can tell the engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// The timer the engine asked for with [`Action::WakeAt`] fired. Batches
    /// form only here (§7.4). Several batches in one tick use successors of
    /// the fresh id.
    Tick {
        fresh_batch_id: BatchId,
    },

    /// This machine is a member of `folder`. Fixed for Phase 1; §9
    /// membership sync comes later.
    FolderJoined {
        folder: FolderId,
        rules: Rules,
        members: Vec<NodeId>,
    },
    RulesChanged {
        folder: FolderId,
        rules: Rules,
    },

    /// The scanner or watcher reports one path (§7.3). Inside a bracket it
    /// also marks the path seen.
    Scanned {
        folder: FolderId,
        path: RelPath,
        state: ScanState,
    },
    /// A full scan begins.
    ScanStarted {
        folder: FolderId,
    },
    /// A full scan ended; live records it did not report are deleted.
    ScanFinished {
        folder: FolderId,
    },
    /// The host stopped a scan (root guard, read error); nothing is announced.
    ScanAborted {
        folder: FolderId,
    },

    PeerConnected {
        peer: NodeId,
        tier: Tier,
    },
    PeerTierChanged {
        peer: NodeId,
        tier: Tier,
    },
    PeerDisconnected {
        peer: NodeId,
    },

    /// A batch arrived from a peer (§7.4).
    BatchReceived {
        from: NodeId,
        batch: Batch,
    },
    /// A peer decided on one of our batches; also its acknowledgement (§7.4).
    DecisionReceived {
        from: NodeId,
        decision: BatchDecision,
    },

    /// The host finished a fetch the engine asked for (§7.5). PR 6.
    Fetched {
        folder: FolderId,
        path: RelPath,
        version: Version,
        outcome: FetchOutcome,
    },
    /// The host finished a commit the engine asked for (§7.5). On `Ok` the
    /// index adopts the entry.
    Applied {
        folder: FolderId,
        path: RelPath,
        version: Version,
        outcome: ApplyOutcome,
    },

    /// User commands (§8.2, §8.3). PR 5.
    Approve {
        folder: FolderId,
        batch: BatchId,
    },
    Deny {
        folder: FolderId,
        batch: BatchId,
    },
    Revert {
        folder: FolderId,
    },
}

/// Everything the engine can ask the host to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Deliver a [`Event::Tick`] no later than this.
    WakeAt(Timestamp),
    /// Send a message to a connected peer.
    Send { to: NodeId, payload: Outbound },

    /// Pull one file from one source (§7.5 steps 1 to 5). PR 6.
    Fetch {
        folder: FolderId,
        path: RelPath,
        version: Version,
        hash: ContentHash,
        size: u64,
        from: NodeId,
    },
    /// Commit as one operation (§7.5 steps 6 to 9): check `expected`
    /// against the path, displace any existing file, rename the content in,
    /// set mtime and exec. Report with [`Event::Applied`]. PR 6.
    Write {
        folder: FolderId,
        path: RelPath,
        entry: Entry,
        expected: Option<Expected>,
        displace: Displace,
    },
    /// Delete as one operation: check `expected`, move to trash, report.
    /// Directories only when empty. PR 6.
    Remove {
        folder: FolderId,
        path: RelPath,
        expected: Option<Expected>,
    },
    /// Metadata-only apply (§7.5): set mtime and exec, no transfer. PR 6.
    SetMeta {
        folder: FolderId,
        path: RelPath,
        mtime_ns: i64,
        exec: bool,
    },
    /// Move a local file aside with nothing to rename in: revert only (§8.3). PR 5.
    MoveToTrash { folder: FolderId, path: RelPath },

    /// History (§8.5).
    RecordBatch {
        batch: Batch,
        role: BatchRole,
        decision: Option<Decision>,
    },
    /// The index changed; persistence hook for Phase 2. Every mutation, in order.
    IndexChanged {
        folder: FolderId,
        record: IndexRecord,
    },
    /// Something for `status`.
    StatusChanged {
        folder: FolderId,
        status: FolderStatus,
    },
}

/// The sync engine for one node. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Engine {
    config: NodeConfig,
    folders: BTreeMap<FolderId, FolderState>,
    peers: BTreeMap<NodeId, Tier>,
    /// The last `WakeAt` emitted per folder, so it is not repeated.
    woke: BTreeMap<FolderId, Timestamp>,
}

impl Engine {
    pub fn new(config: NodeConfig) -> Self {
        Self {
            config,
            folders: BTreeMap::new(),
            peers: BTreeMap::new(),
            woke: BTreeMap::new(),
        }
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// Read-only view of one folder, for `status` and the simulator.
    pub fn folder(&self, id: FolderId) -> Option<&FolderState> {
        self.folders.get(&id)
    }

    /// Every folder in `FolderId` order.
    pub fn folders(&self) -> impl Iterator<Item = &FolderState> {
        self.folders.values()
    }

    /// Connected peers in `NodeId` order with their tiers.
    pub fn peers(&self) -> impl Iterator<Item = (NodeId, Tier)> + '_ {
        self.peers.iter().map(|(n, t)| (*n, *t))
    }

    /// Handle one event at time `now` and return what the host should do.
    pub fn handle(&mut self, now: Timestamp, event: Event) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::Tick { fresh_batch_id } => self.tick(now, fresh_batch_id, &mut out),
            Event::FolderJoined {
                folder,
                rules,
                members,
            } => {
                let state = FolderState::new(
                    folder,
                    rules,
                    members,
                    self.config.node_id,
                    self.config.author_host.clone(),
                );
                self.folders.insert(folder, state);
            }
            Event::RulesChanged { folder, rules } => match self.folders.get_mut(&folder) {
                Some(f) => f.set_rules(rules),
                None => out.push(unknown_folder(folder)),
            },
            Event::Scanned {
                folder,
                path,
                state,
            } => match self.folders.get_mut(&folder) {
                Some(f) => {
                    let scanned = f.scanned(now, path, state);
                    if let Some(change) = scanned.change {
                        out.push(Action::IndexChanged {
                            folder,
                            record: change.record,
                        });
                    }
                    if let Some(status) = scanned.status {
                        out.push(Action::StatusChanged { folder, status });
                    }
                }
                None => out.push(unknown_folder(folder)),
            },
            Event::ScanStarted { folder } => match self.folders.get_mut(&folder) {
                Some(f) => f.scan_started(),
                None => out.push(unknown_folder(folder)),
            },
            Event::ScanFinished { folder } => match self.folders.get_mut(&folder) {
                Some(f) => match f.scan_finished(now) {
                    Ok(changes) => out.extend(changes.into_iter().map(|c| Action::IndexChanged {
                        folder,
                        record: c.record,
                    })),
                    Err(status) => out.push(Action::StatusChanged { folder, status }),
                },
                None => out.push(unknown_folder(folder)),
            },
            Event::ScanAborted { folder } => match self.folders.get_mut(&folder) {
                Some(f) => {
                    if let Err(status) = f.scan_aborted() {
                        out.push(Action::StatusChanged { folder, status });
                    }
                }
                None => out.push(unknown_folder(folder)),
            },
            Event::PeerConnected { peer, tier } | Event::PeerTierChanged { peer, tier } => {
                self.peers.insert(peer, tier);
            }
            Event::PeerDisconnected { peer } => {
                self.peers.remove(&peer);
            }
            Event::BatchReceived { from, batch } => self.receive(from, batch, &mut out),
            Event::DecisionReceived { from, decision } => {
                match self.folders.get_mut(&decision.folder) {
                    Some(f) => f.acknowledged(from, decision.seq_high),
                    None => out.push(unknown_folder(decision.folder)),
                }
            }
            Event::Applied {
                folder,
                path,
                version,
                outcome,
            } => match self.folders.get_mut(&folder) {
                Some(f) => {
                    if let Some(record) = f.applied(now, &path, &version, outcome) {
                        out.push(Action::IndexChanged { folder, record });
                    }
                }
                None => out.push(unknown_folder(folder)),
            },
            // PR 6: fetch results feed the want-list.
            Event::Fetched { .. } => {}
            // PR 5: approve, deny and revert.
            Event::Approve { .. } | Event::Deny { .. } | Event::Revert { .. } => {}
        }
        self.schedule(&mut out);
        out
    }

    /// Form and send batches for every folder whose window is due (§7.4).
    fn tick(&mut self, now: Timestamp, fresh: BatchId, out: &mut Vec<Action>) {
        let mut used: u128 = 0;
        for folder in self.folders.values_mut() {
            if !folder.is_due(now) {
                continue;
            }
            let batches = folder.form_batches(now, fresh.successor(used));
            used += batches.len() as u128;
            let recipients: Vec<NodeId> = folder
                .members()
                .filter(|m| *m != self.config.node_id && self.peers.contains_key(m))
                .collect();
            for batch in batches {
                for to in &recipients {
                    out.push(Action::Send {
                        to: *to,
                        payload: Outbound::Batch(batch.clone()),
                    });
                }
                out.push(Action::RecordBatch {
                    batch,
                    role: BatchRole::Sent,
                    decision: None,
                });
            }
        }
    }

    /// A batch arrived: apply set, decision, history (§7.4). The brake (PR 5)
    /// will decide between `Accepted` and `Held` here.
    fn receive(&mut self, from: NodeId, batch: Batch, out: &mut Vec<Action>) {
        let Some(folder) = self.folders.get_mut(&batch.folder) else {
            out.push(unknown_folder(batch.folder));
            return;
        };
        let set: ApplySet = folder.receive(&batch);
        if set.duplicates > 0 {
            out.push(Action::StatusChanged {
                folder: batch.folder,
                status: FolderStatus::DuplicatePaths {
                    batch: batch.id,
                    count: set.duplicates,
                },
            });
        }
        let decision = Decision::Accepted;
        out.push(Action::Send {
            to: from,
            payload: Outbound::Decision(BatchDecision {
                batch: batch.id,
                folder: batch.folder,
                decision: decision.clone(),
                seq_high: batch.seq_high,
            }),
        });
        out.push(Action::RecordBatch {
            batch,
            role: BatchRole::Received,
            decision: Some(decision),
        });
    }

    /// Ask for a tick at each folder's due time, once per change of it.
    fn schedule(&mut self, out: &mut Vec<Action>) {
        for (id, folder) in &self.folders {
            match folder.due() {
                Some(due) => {
                    if self.woke.get(id) != Some(&due) {
                        self.woke.insert(*id, due);
                        out.push(Action::WakeAt(due));
                    }
                }
                None => {
                    self.woke.remove(id);
                }
            }
        }
    }
}

fn unknown_folder(folder: FolderId) -> Action {
    Action::StatusChanged {
        folder,
        status: FolderStatus::UnknownFolder,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::batch::{ApplyItem, ApplyMode};
    use crate::entry::{Kind, Observed};
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

    fn folder() -> FolderId {
        FolderId::from_bytes([7; 16])
    }

    fn fresh(i: u8) -> BatchId {
        let mut b = [0u8; 16];
        b[0] = 0xb0;
        b[15] = i;
        BatchId::from_bytes(b)
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

    fn engine(i: u8, host: &str) -> Engine {
        Engine::new(NodeConfig {
            node_id: node(i),
            author_host: HostName::new(host).unwrap(),
        })
    }

    fn join(e: &mut Engine, members: &[u8]) {
        let out = e.handle(
            t(0.0),
            Event::FolderJoined {
                folder: folder(),
                rules: Rules::default(),
                members: members.iter().map(|i| node(*i)).collect(),
            },
        );
        assert!(out.is_empty());
    }

    fn connect(a: &mut Engine, b: &mut Engine) {
        a.handle(
            t(0.0),
            Event::PeerConnected {
                peer: b.config().node_id,
                tier: Tier::Lan,
            },
        );
        b.handle(
            t(0.0),
            Event::PeerConnected {
                peer: a.config().node_id,
                tier: Tier::Lan,
            },
        );
    }

    /// A test host: routes `Send` actions to the addressed engine at `now`
    /// and returns every non-network action, tagged with the engine that
    /// produced it.
    fn deliver(
        now: Timestamp,
        from: NodeId,
        actions: Vec<Action>,
        engines: &mut BTreeMap<NodeId, Engine>,
    ) -> Vec<(NodeId, Action)> {
        let mut rest = Vec::new();
        for action in actions {
            match action {
                Action::Send { to, payload } => {
                    let event = match payload {
                        Outbound::Batch(batch) => Event::BatchReceived { from, batch },
                        Outbound::Decision(decision) => Event::DecisionReceived { from, decision },
                    };
                    let replies = engines.get_mut(&to).unwrap().handle(now, event);
                    rest.extend(deliver(now, to, replies, engines));
                }
                other => rest.push((from, other)),
            }
        }
        rest
    }

    fn sends(actions: &[Action]) -> Vec<(NodeId, &Outbound)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send { to, payload } => Some((*to, payload)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn scan_then_tick_forms_a_batch_for_connected_members_only() {
        let mut a = engine(1, "a");
        let mut b = engine(2, "b");
        join(&mut a, &[1, 2, 3]); // 3 is a member but never connects
        connect(&mut a, &mut b);

        let out = a.handle(
            t(10.0),
            Event::Scanned {
                folder: folder(),
                path: p("doc.txt"),
                state: file(1, 5),
            },
        );
        assert!(matches!(out[0], Action::IndexChanged { .. }));
        assert_eq!(out[1], Action::WakeAt(t(12.0)));
        assert_eq!(out.len(), 2);

        // Another change 1 s later extends the quiet period.
        let out = a.handle(
            t(11.0),
            Event::Scanned {
                folder: folder(),
                path: p("other.txt"),
                state: file(2, 5),
            },
        );
        assert_eq!(out[1], Action::WakeAt(t(13.0)));

        // An early tick does nothing and does not repeat the wake.
        let out = a.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        assert!(out.is_empty(), "{out:?}");

        let out = a.handle(
            t(13.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        let sent = sends(&out);
        assert_eq!(sent.len(), 1, "only the connected member gets it");
        let Outbound::Batch(batch) = sent[0].1 else {
            panic!()
        };
        assert_eq!(sent[0].0, node(2));
        assert_eq!(batch.id, fresh(1));
        assert_eq!(batch.source, node(1));
        assert_eq!(batch.seq_high, 2);
        assert_eq!(batch.entries.len(), 2);
        assert_eq!(batch.summary.adds, 2);
        assert!(matches!(
            out.last(),
            Some(Action::RecordBatch {
                role: BatchRole::Sent,
                decision: None,
                ..
            })
        ));
        assert!(
            !out.iter().any(|a| matches!(a, Action::WakeAt(_))),
            "window closed"
        );
        assert_eq!(a.folder(folder()).unwrap().due(), None);
    }

    #[test]
    fn receiver_replies_with_an_accepting_decision_that_acks_seq_high() {
        let mut engines = BTreeMap::new();
        let mut a = engine(1, "a");
        let mut b = engine(2, "b");
        join(&mut a, &[1, 2]);
        join(&mut b, &[1, 2]);
        connect(&mut a, &mut b);
        a.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("x"),
                state: file(1, 1),
            },
        );
        let out = a.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        engines.insert(node(1), a);
        engines.insert(node(2), b);
        let rest = deliver(t(3.0), node(1), out, &mut engines);

        // B recorded the batch as received and accepted; A recorded it as sent.
        let b_records: Vec<_> = rest
            .iter()
            .filter(|(who, a)| *who == node(2) && matches!(a, Action::RecordBatch { .. }))
            .collect();
        assert_eq!(b_records.len(), 1);
        assert!(matches!(
            b_records[0].1,
            Action::RecordBatch {
                role: BatchRole::Received,
                decision: Some(Decision::Accepted),
                ..
            }
        ));
        let b = &engines[&node(2)];
        let set = &b.folder(folder()).unwrap().accepted()[0];
        assert_eq!(set.source, node(1));
        assert_eq!(set.items.len(), 1);
        assert!(matches!(
            set.items[0],
            ApplyItem::Apply {
                mode: ApplyMode::Fetch,
                ..
            }
        ));
        // A's watermark for B moved to the batch's seq_high.
        assert_eq!(
            engines[&node(1)]
                .folder(folder())
                .unwrap()
                .acked_by(node(2)),
            1
        );
        // Receiving is not writing: B has no window.
        assert_eq!(b.folder(folder()).unwrap().due(), None);
    }

    #[test]
    fn a_record_relays_from_a_to_c_through_b_intact() {
        // A - B - C, with A and C unable to see each other.
        let mut a = engine(1, "alpha");
        let mut b = engine(2, "bravo");
        let mut c = engine(3, "charlie");
        join(&mut a, &[1, 2]);
        join(&mut b, &[1, 2, 3]);
        join(&mut c, &[2, 3]);
        connect(&mut a, &mut b);
        connect(&mut b, &mut c);

        a.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("dir/note.md"),
                state: file(4, 77),
            },
        );
        let out = a.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        let a_entry = a
            .folder(folder())
            .unwrap()
            .index()
            .get(&p("dir/note.md"))
            .unwrap()
            .entry
            .clone();

        let mut engines = BTreeMap::from([(node(1), a), (node(2), b), (node(3), c)]);
        deliver(t(3.0), node(1), out, &mut engines);

        // B's host "fetches and commits" the item and reports back.
        let b = engines.get_mut(&node(2)).unwrap();
        let item = b.folder(folder()).unwrap().accepted()[0].items[0]
            .incoming()
            .clone();
        let out = b.handle(
            t(4.0),
            Event::Applied {
                folder: folder(),
                path: item.path.clone(),
                version: item.version.clone(),
                outcome: ApplyOutcome::Ok,
            },
        );
        assert!(matches!(out[0], Action::IndexChanged { .. }));
        assert_eq!(
            out[1],
            Action::WakeAt(t(6.0)),
            "the adoption opened B's window"
        );

        // B's next batch carries the adopted record to both neighbours.
        let out = b.handle(
            t(6.0),
            Event::Tick {
                fresh_batch_id: fresh(2),
            },
        );
        let recipients: Vec<NodeId> = sends(&out).iter().map(|(to, _)| *to).collect();
        assert_eq!(recipients, [node(1), node(3)], "member order");
        let Outbound::Batch(relayed) = sends(&out)[0].1.clone() else {
            panic!()
        };
        assert_eq!(relayed.source, node(2));
        assert_eq!(relayed.summary, Default::default(), "relayed, not counted");
        assert_eq!(relayed.entries, vec![a_entry.clone()]);
        deliver(t(6.0), node(2), out, &mut engines);

        // C has A's record with A's version, modified_by and author_host.
        let c = &engines[&node(3)];
        let set = &c.folder(folder()).unwrap().accepted()[0];
        assert_eq!(set.source, node(2), "arrived via B");
        let got = set.items[0].incoming();
        assert_eq!(got, &a_entry);
        assert_eq!(got.version, Version::empty().incremented(node(1)));
        assert_eq!(got.modified_by, node(1));
        assert_eq!(got.author_host.as_str(), "alpha");

        // A ignored the equal version and B's window closed; nothing loops.
        let a = &engines[&node(1)];
        assert!(a.folder(folder()).unwrap().accepted().is_empty());
        assert_eq!(engines[&node(2)].folder(folder()).unwrap().due(), None);
    }

    #[test]
    fn scan_bracket_events_flow_through_the_engine() {
        let mut e = engine(1, "a");
        join(&mut e, &[1]);
        e.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("a"),
                state: file(1, 1),
            },
        );
        e.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("b"),
                state: file(2, 1),
            },
        );
        e.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );

        // Aborted: nothing announced.
        e.handle(t(5.0), Event::ScanStarted { folder: folder() });
        e.handle(
            t(5.0),
            Event::Scanned {
                folder: folder(),
                path: p("a"),
                state: ScanState::Unchanged,
            },
        );
        let out = e.handle(t(5.5), Event::ScanAborted { folder: folder() });
        assert!(out.is_empty());
        assert_eq!(e.folder(folder()).unwrap().index().tracked_count(), 2);

        // Finished: b was not seen, so it is deleted.
        e.handle(t(6.0), Event::ScanStarted { folder: folder() });
        e.handle(
            t(6.0),
            Event::Scanned {
                folder: folder(),
                path: p("a"),
                state: ScanState::Unchanged,
            },
        );
        let out = e.handle(t(7.0), Event::ScanFinished { folder: folder() });
        assert!(
            matches!(&out[0], Action::IndexChanged { record, .. } if record.entry.deleted && record.entry.path == p("b"))
        );
        assert_eq!(out[1], Action::WakeAt(t(9.0)));
        assert_eq!(out.len(), 2);

        // Bracket bookkeeping bugs are reported, not panicked on.
        let out = e.handle(t(8.0), Event::ScanFinished { folder: folder() });
        assert_eq!(
            out,
            [Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::ScanNotOpen
            }]
        );
        let out = e.handle(
            t(8.0),
            Event::Scanned {
                folder: folder(),
                path: p("ghost"),
                state: ScanState::Unchanged,
            },
        );
        assert_eq!(
            out,
            [Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::UnchangedUnknownPath { path: p("ghost") }
            }]
        );
    }

    #[test]
    fn unknown_folders_are_reported_not_ignored_silently() {
        let mut e = engine(1, "a");
        let out = e.handle(t(1.0), Event::ScanStarted { folder: folder() });
        assert_eq!(
            out,
            [Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::UnknownFolder
            }]
        );
        let mut other = engine(2, "b");
        join(&mut other, &[1, 2]);
        other.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("x"),
                state: file(1, 1),
            },
        );
        other.handle(
            t(0.0),
            Event::PeerConnected {
                peer: node(1),
                tier: Tier::Relay,
            },
        );
        let out = other.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        let Outbound::Batch(batch) = sends(&out)[0].1.clone() else {
            panic!()
        };
        let out = e.handle(
            t(3.0),
            Event::BatchReceived {
                from: node(2),
                batch,
            },
        );
        assert_eq!(
            out,
            [Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::UnknownFolder
            }]
        );
        assert!(
            sends(&out).is_empty(),
            "no decision for a folder we are not in"
        );
    }

    #[test]
    fn duplicate_paths_in_a_received_batch_are_reported() {
        let mut e = engine(1, "a");
        join(&mut e, &[1, 2]);
        let mut other = engine(2, "b");
        join(&mut other, &[1, 2]);
        other.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("x"),
                state: file(1, 1),
            },
        );
        let mut batch = other
            .folder(folder())
            .unwrap()
            .clone()
            .form_batches(t(3.0), fresh(1))
            .remove(0);
        let dup = batch.entries[0].clone();
        batch.entries.push(dup);
        let out = e.handle(
            t(3.0),
            Event::BatchReceived {
                from: node(2),
                batch: batch.clone(),
            },
        );
        assert_eq!(
            out[0],
            Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::DuplicatePaths {
                    batch: batch.id,
                    count: 1
                }
            }
        );
        assert!(
            matches!(&out[1], Action::Send { to, payload: Outbound::Decision(d) } if *to == node(2) && d.decision == Decision::Accepted && d.seq_high == 1)
        );
    }

    #[test]
    fn two_folders_due_in_one_tick_use_successor_ids() {
        let f2 = FolderId::from_bytes([8; 16]);
        let mut e = engine(1, "a");
        join(&mut e, &[1, 2]);
        e.handle(
            t(0.0),
            Event::FolderJoined {
                folder: f2,
                rules: Rules::default(),
                members: vec![node(1), node(2)],
            },
        );
        e.handle(
            t(0.0),
            Event::PeerConnected {
                peer: node(2),
                tier: Tier::Lan,
            },
        );
        e.handle(
            t(1.0),
            Event::Scanned {
                folder: f2,
                path: p("x"),
                state: file(1, 1),
            },
        );
        e.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("y"),
                state: file(1, 1),
            },
        );
        let out = e.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        let ids: Vec<(FolderId, BatchId)> = sends(&out)
            .iter()
            .filter_map(|(_, o)| match o {
                Outbound::Batch(b) => Some((b.folder, b.id)),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            [(folder(), fresh(1)), (f2, fresh(1).successor(1))],
            "FolderId order"
        );
    }

    #[test]
    fn the_same_events_give_byte_identical_actions() {
        let events = || {
            vec![
                (
                    t(0.0),
                    Event::FolderJoined {
                        folder: folder(),
                        rules: Rules::default(),
                        members: vec![node(1), node(2), node(3)],
                    },
                ),
                (
                    t(0.0),
                    Event::PeerConnected {
                        peer: node(3),
                        tier: Tier::Direct,
                    },
                ),
                (
                    t(0.0),
                    Event::PeerConnected {
                        peer: node(2),
                        tier: Tier::Lan,
                    },
                ),
                (
                    t(1.0),
                    Event::Scanned {
                        folder: folder(),
                        path: p("b"),
                        state: file(1, 1),
                    },
                ),
                (
                    t(1.5),
                    Event::Scanned {
                        folder: folder(),
                        path: p("a"),
                        state: file(2, 1),
                    },
                ),
                (
                    t(2.0),
                    Event::Scanned {
                        folder: folder(),
                        path: p("b"),
                        state: ScanState::Absent,
                    },
                ),
                (
                    t(4.0),
                    Event::Tick {
                        fresh_batch_id: fresh(1),
                    },
                ),
            ]
        };
        let script = |e: &mut Engine| -> Vec<Vec<Action>> {
            events()
                .into_iter()
                .map(|(now, event)| e.handle(now, event))
                .collect()
        };
        let mut x = engine(1, "a");
        let mut y = engine(1, "a");
        let ax = script(&mut x);
        let ay = script(&mut y);
        assert_eq!(
            postcard::to_stdvec(&ax).unwrap(),
            postcard::to_stdvec(&ay).unwrap()
        );
        assert_eq!(
            postcard::to_stdvec(&x).unwrap(),
            postcard::to_stdvec(&y).unwrap()
        );
    }

    #[test]
    fn engine_and_events_round_trip_through_serde() {
        let mut e = engine(1, "a");
        join(&mut e, &[1, 2]);
        e.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("x"),
                state: file(1, 1),
            },
        );
        let bytes = postcard::to_stdvec(&e).unwrap();
        assert_eq!(postcard::from_bytes::<Engine>(&bytes).unwrap(), e);
        let ev = Event::Applied {
            folder: folder(),
            path: p("x"),
            version: Version::empty(),
            outcome: ApplyOutcome::ChangedUnderneath,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
        let act = Action::Write {
            folder: folder(),
            path: p("x"),
            entry: e
                .folder(folder())
                .unwrap()
                .index()
                .get(&p("x"))
                .unwrap()
                .entry
                .clone(),
            expected: Some(Expected {
                size: 1,
                mtime_ns: 2,
            }),
            displace: Displace::ConflictCopy(p("x.conflict")),
        };
        let json = serde_json::to_string(&act).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), act);
    }
}
