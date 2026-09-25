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
use crate::entry::{ContentHash, Entry, Observed};
use crate::folder::{
    ApplyOutcome, Approved, Displace, FolderState, FolderStatus, HostStep, ScanState, Ticked,
};
use crate::id::{BatchId, FolderId, HostName, NodeId};
use crate::index::IndexRecord;
use crate::path::RelPath;
use crate::rules::Rules;
use crate::time::Timestamp;
use crate::version::Version;
use crate::want::{FetchReport, Tier, Want};

/// What the host tells the engine about this machine at start-up.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeConfig {
    /// From `node.json` (§5).
    pub node_id: NodeId,
    /// This machine's Tailscale hostname, carried as `author_host` (§7.1).
    pub author_host: HostName,
}

/// A message for a peer. The binary crate wraps it in its wire envelope (§12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outbound {
    Batch(Batch),
    Decision(BatchDecision),
    /// The highest `seq` of the recipient's records we hold, per folder
    /// (§7.4, `FolderMeta.have_up_to` in §12). Sent on connect.
    HaveUpTo {
        folder: FolderId,
        seq: u64,
    },
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

    /// A peer told us the highest `seq` of ours it holds (§7.4). Replaces
    /// our memory of its acks; catch-up follows at the next tick.
    HaveUpToReceived {
        from: NodeId,
        folder: FolderId,
        seq: u64,
    },

    /// The host finished a fetch the engine asked for (§7.5). `hash` is
    /// what was requested from the source (a `RequestFile` names content,
    /// not a version); `version` is the want the report belongs to, so a
    /// late report for a want since replaced is told apart.
    Fetched {
        folder: FolderId,
        path: RelPath,
        hash: ContentHash,
        version: Version,
        outcome: FetchReport,
    },
    /// Bytes are arriving for a fetch; the host sends this at most every
    /// few seconds. Pushes the stall deadline out (§7.5). Fields as in
    /// [`Event::Fetched`].
    FetchProgress {
        folder: FolderId,
        path: RelPath,
        hash: ContentHash,
        version: Version,
    },
    /// A want the host persisted comes back after a restart (Phase 2).
    WantRestored {
        folder: FolderId,
        want: Box<Want>,
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

    /// Pull one file from one source (§7.5 steps 1 to 5): send
    /// `RequestFile { folder, path, hash, offset }`. The request names the
    /// content, so the source may serve it from `path` or from any live
    /// file with that hash. `version` is for the report back, not the wire.
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
    /// set mtime and exec. Report with [`Event::Applied`].
    Write {
        folder: FolderId,
        path: RelPath,
        entry: Entry,
        expected: Option<Observed>,
        displace: Displace,
    },
    /// Delete as one operation: check `expected`, move to trash, report.
    /// Directories only when empty.
    Remove {
        folder: FolderId,
        path: RelPath,
        expected: Option<Observed>,
        /// Trash, or the conflict-copy path when the removed file is the
        /// losing content of a conflict a tombstone won (§7.6).
        displace: Displace,
    },
    /// Metadata-only apply (§7.5): after the same guard as every commit
    /// (`expected` against what is on disk, `Observed::unchanged_by_stat`),
    /// set mtime and exec, no transfer. Report with [`Event::Applied`].
    SetMeta {
        folder: FolderId,
        path: RelPath,
        expected: Option<Observed>,
        mtime_ns: i64,
        exec: bool,
    },
    /// Move a local file aside with nothing to rename in: revert only (§8.3).
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
    /// A record left the index; persistence hook for Phase 2. Only `revert`
    /// removes records (§8.3: a pending add peers never saw has no announced
    /// version to fall back to). Every other end of a path is a tombstone,
    /// reported as [`Action::IndexChanged`].
    IndexRemoved { folder: FolderId, path: RelPath },
    /// A want changed or ended; persistence hook for Phase 2 (§7.5).
    WantChanged {
        folder: FolderId,
        path: RelPath,
        want: Option<Box<Want>>,
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
    /// The time of the event being handled, for an immediate wake-up.
    now_hint: Timestamp,
}

impl Engine {
    pub fn new(config: NodeConfig) -> Self {
        Self {
            config,
            folders: BTreeMap::new(),
            peers: BTreeMap::new(),
            woke: BTreeMap::new(),
            now_hint: Timestamp::default(),
        }
    }

    /// Rebuild an engine from persisted folder state after a restart (§11
    /// "persistence contract", §13). The folder state is the unit of
    /// restart: index, pending set, watermarks, peer seqs and acks,
    /// quarantine, wants, paused state and deferred entries all come back
    /// as they were. Peers are not restored; they reconnect and exchange
    /// `have_up_to`. Wants in transient states return to *wanted*, since
    /// the host's in-flight operations died with the process.
    pub fn restore(config: NodeConfig, folders: Vec<FolderState>) -> Self {
        let mut engine = Self::new(config);
        for mut folder in folders {
            folder.restarted();
            engine.folders.insert(folder.id(), folder);
        }
        engine
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
        self.now_hint = now;
        let mut out = Vec::new();
        match event {
            Event::Tick { fresh_batch_id } => self.tick(now, fresh_batch_id, &mut out),
            Event::FolderJoined {
                folder,
                rules,
                members,
            } => {
                if self.folders.contains_key(&folder) {
                    // Replacing the state would discard the index.
                    out.push(Action::StatusChanged {
                        folder,
                        status: FolderStatus::AlreadyJoined,
                    });
                } else {
                    let state = FolderState::new(
                        folder,
                        rules,
                        members,
                        self.config.node_id,
                        self.config.author_host.clone(),
                    );
                    self.folders.insert(folder, state);
                }
            }
            Event::RulesChanged { folder, rules } => match self.folders.get_mut(&folder) {
                Some(f) => out.extend(
                    f.rules_changed(rules)
                        .into_iter()
                        .map(|status| Action::StatusChanged { folder, status }),
                ),
                None => out.push(unknown_folder(folder)),
            },
            Event::Scanned {
                folder,
                path,
                state,
            } => match self.folders.get_mut(&folder) {
                Some(f) => {
                    let before = f.winner_fallbacks();
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
                    if f.winner_fallbacks() > before {
                        out.push(Action::StatusChanged {
                            folder,
                            status: FolderStatus::WinnerFallback {
                                count: f.winner_fallbacks(),
                            },
                        });
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
            Event::PeerConnected { peer, tier } => {
                self.peers.insert(peer, tier);
                // Tell the peer how far its records reach here (§7.4, §12).
                for (id, folder) in &self.folders {
                    if folder.is_member(peer) {
                        out.push(Action::Send {
                            to: peer,
                            payload: Outbound::HaveUpTo {
                                folder: *id,
                                seq: folder.index().peer_seq(peer),
                            },
                        });
                    }
                }
            }
            Event::PeerTierChanged { peer, tier } => {
                self.peers.insert(peer, tier);
            }
            Event::PeerDisconnected { peer } => {
                self.peers.remove(&peer);
                for folder in self.folders.values_mut() {
                    folder.peer_gone(peer);
                }
            }
            Event::HaveUpToReceived { from, folder, seq } => match self.folders.get_mut(&folder) {
                Some(f) => f.have_up_to(from, seq),
                None => out.push(unknown_folder(folder)),
            },
            Event::BatchReceived { from, batch } => self.receive(now, from, batch, &mut out),
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
                    for record in f.applied(now, &path, &version, outcome) {
                        out.push(Action::IndexChanged { folder, record });
                    }
                }
                None => out.push(unknown_folder(folder)),
            },
            Event::Fetched {
                folder,
                path,
                version,
                outcome,
                ..
            } => match self.folders.get_mut(&folder) {
                Some(f) => {
                    let fetched = f.fetched(now, &path, &version, outcome);
                    if fetched.state == Some(crate::want::WantState::GaveUp) {
                        out.push(Action::StatusChanged {
                            folder,
                            status: FolderStatus::GaveUp { path: path.clone() },
                        });
                    }
                    if let Some(change) = fetched.unrecoverable {
                        out.push(Action::IndexChanged {
                            folder,
                            record: change.record,
                        });
                        out.push(Action::StatusChanged {
                            folder,
                            status: FolderStatus::Unrecoverable { path },
                        });
                    }
                }
                None => out.push(unknown_folder(folder)),
            },
            Event::FetchProgress {
                folder,
                path,
                version,
                ..
            } => match self.folders.get_mut(&folder) {
                Some(f) => f.progress(now, &path, &version),
                None => out.push(unknown_folder(folder)),
            },
            Event::WantRestored { folder, want } => match self.folders.get_mut(&folder) {
                Some(f) => f.restore_want(*want),
                None => out.push(unknown_folder(folder)),
            },
            Event::Approve { folder, batch } => match self.folders.get_mut(&folder) {
                Some(f) => match f.approve(now, batch) {
                    Approved::Released(set) => out.push(Action::StatusChanged {
                        folder,
                        status: FolderStatus::Released {
                            batch,
                            items: set.map_or(0, |s| s.items.len()),
                        },
                    }),
                    Approved::Sent(batches) => {
                        self.send_batches(folder, batches, &mut out);
                        out.push(Action::StatusChanged {
                            folder,
                            status: FolderStatus::Unpaused { batch },
                        });
                    }
                    Approved::Unknown => out.push(Action::StatusChanged {
                        folder,
                        status: FolderStatus::UnknownBatch { batch },
                    }),
                },
                None => out.push(unknown_folder(folder)),
            },
            Event::Deny { folder, batch } => match self.folders.get_mut(&folder) {
                Some(f) => match f.deny(now, batch) {
                    Some(changes) => {
                        let paths = changes.len();
                        out.extend(changes.into_iter().map(|c| Action::IndexChanged {
                            folder,
                            record: c.record,
                        }));
                        out.push(Action::StatusChanged {
                            folder,
                            status: FolderStatus::Denied { batch, paths },
                        });
                    }
                    None => out.push(Action::StatusChanged {
                        folder,
                        status: FolderStatus::UnknownBatch { batch },
                    }),
                },
                None => out.push(unknown_folder(folder)),
            },
            Event::Revert { folder } => match self.folders.get_mut(&folder) {
                Some(f) => match f.revert(now) {
                    Some(reverted) => {
                        for write in reverted.reverted {
                            out.push(match write.restored {
                                Some(record) => Action::IndexChanged { folder, record },
                                None => Action::IndexRemoved {
                                    folder,
                                    path: write.path,
                                },
                            });
                        }
                        for path in &reverted.trash {
                            out.push(Action::MoveToTrash {
                                folder,
                                path: path.clone(),
                            });
                        }
                        out.push(Action::StatusChanged {
                            folder,
                            status: FolderStatus::Reverted {
                                batch: reverted.batch,
                                trashed: reverted.trash.len(),
                                refetch: reverted.refetch,
                            },
                        });
                    }
                    None => out.push(Action::StatusChanged {
                        folder,
                        status: FolderStatus::NotPaused,
                    }),
                },
                None => out.push(unknown_folder(folder)),
            },
        }
        self.pump(now, &mut out);
        self.schedule(&mut out);
        out
    }

    /// Drive every folder's want-list (§7.5): emit fetches and commits,
    /// report adoptions and want changes.
    fn pump(&mut self, now: Timestamp, out: &mut Vec<Action>) {
        let peers = self.peers.clone();
        for (id, folder) in &mut self.folders {
            let folder_id = *id;
            let (steps, adopted) = folder.dispatch(now, &peers);
            for step in steps {
                out.push(match step {
                    HostStep::Fetch {
                        path,
                        version,
                        hash,
                        size,
                        from,
                    } => Action::Fetch {
                        folder: folder_id,
                        path,
                        version,
                        hash,
                        size,
                        from,
                    },
                    HostStep::Write {
                        path,
                        entry,
                        expected,
                        displace,
                    } => Action::Write {
                        folder: folder_id,
                        path,
                        entry,
                        expected,
                        displace,
                    },
                    HostStep::Remove {
                        path,
                        expected,
                        displace,
                    } => Action::Remove {
                        folder: folder_id,
                        path,
                        expected,
                        displace,
                    },
                    HostStep::SetMeta {
                        path,
                        expected,
                        mtime_ns,
                        exec,
                    } => Action::SetMeta {
                        folder: folder_id,
                        path,
                        expected,
                        mtime_ns,
                        exec,
                    },
                });
            }
            for record in adopted {
                out.push(Action::IndexChanged {
                    folder: folder_id,
                    record,
                });
            }
            for (path, want) in folder.want_changes() {
                out.push(Action::WantChanged {
                    folder: folder_id,
                    path,
                    want: want.map(Box::new),
                });
            }
            for status in folder.take_statuses() {
                out.push(Action::StatusChanged {
                    folder: folder_id,
                    status,
                });
            }
        }
    }

    /// Form and send batches for every folder whose window is due (§7.4).
    ///
    /// The host's timer has fired, so nothing is scheduled any more: forget
    /// what was asked for, and `schedule` re-announces the due time of any
    /// folder that is not due yet (a tick that came early).
    fn tick(&mut self, now: Timestamp, fresh: BatchId, out: &mut Vec<Action>) {
        self.woke.clear();
        let mut used: u128 = 0;
        let ids: Vec<FolderId> = self.folders.keys().copied().collect();
        for id in ids {
            let Some(folder) = self.folders.get_mut(&id) else {
                continue;
            };
            for path in folder.expire(now) {
                out.push(Action::StatusChanged {
                    folder: id,
                    status: FolderStatus::Stalled { path },
                });
            }
            let (catchup, spent) = folder.catchup_batches(now, fresh.successor(used));
            used += spent;
            for (peer, batches) in catchup {
                for batch in batches {
                    out.push(Action::Send {
                        to: peer,
                        payload: Outbound::Batch(batch.clone()),
                    });
                    out.push(Action::RecordBatch {
                        batch,
                        role: BatchRole::Sent,
                        decision: None,
                    });
                }
            }
            match folder.tick(now, fresh.successor(used)) {
                Ticked::Nothing => {}
                Ticked::Sent(batches) => {
                    used += batches.len() as u128;
                    self.send_batches(id, batches, out);
                }
                Ticked::Paused {
                    first,
                    snapshot,
                    status,
                } => {
                    if first {
                        // The reserved id is spent even though nothing was sent.
                        used += 1;
                        for batch in snapshot {
                            out.push(Action::RecordBatch {
                                batch,
                                role: BatchRole::Paused,
                                decision: None,
                            });
                        }
                    }
                    out.push(Action::StatusChanged { folder: id, status });
                }
            }
        }
    }

    /// Send formed batches to every connected member and record them.
    fn send_batches(&self, folder: FolderId, batches: Vec<Batch>, out: &mut Vec<Action>) {
        let Some(state) = self.folders.get(&folder) else {
            return;
        };
        let recipients: Vec<NodeId> = state
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

    /// A batch arrived: apply set, quarantine, brake, decision, history
    /// (§7.4, §8.1, §8.2).
    fn receive(&mut self, now: Timestamp, from: NodeId, batch: Batch, out: &mut Vec<Action>) {
        let Some(folder) = self.folders.get_mut(&batch.folder) else {
            out.push(unknown_folder(batch.folder));
            return;
        };
        let received = folder.receive(now, &batch);
        let set: &ApplySet = &received.set;
        if set.fallbacks > 0 {
            out.push(Action::StatusChanged {
                folder: batch.folder,
                status: FolderStatus::WinnerFallback {
                    count: folder.winner_fallbacks(),
                },
            });
        }
        if set.duplicates > 0 {
            out.push(Action::StatusChanged {
                folder: batch.folder,
                status: FolderStatus::DuplicatePaths {
                    batch: batch.id,
                    count: set.duplicates,
                },
            });
        }
        if received.joined > 0 {
            out.push(Action::StatusChanged {
                folder: batch.folder,
                status: FolderStatus::JoinedHeld {
                    batch: batch.id,
                    count: received.joined,
                },
            });
        }
        if let Some(reason) = received.held {
            out.push(Action::StatusChanged {
                folder: batch.folder,
                status: FolderStatus::Held {
                    batch: batch.id,
                    source: batch.source,
                    reason,
                    summary: received.summary,
                    paths: set.items.len(),
                },
            });
        }
        let decision = received.decision;
        out.push(Action::Send {
            to: from,
            payload: Outbound::Decision(BatchDecision {
                batch: batch.id,
                folder: batch.folder,
                decision: decision.clone(),
                // The acknowledgement is our contiguous watermark of the
                // sender's records, not this batch's seq_high: below it
                // when an earlier batch never arrived (§7.4).
                seq_high: folder.index().peer_seq(from),
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
            if folder.wake_now() {
                out.push(Action::WakeAt(self.now_hint));
                self.woke.remove(id);
                continue;
            }
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
    use crate::want::WantState;
    use proptest::prelude::*;

    use crate::batch::Summary;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::batch::ApplyMode;
    use crate::entry::{Kind, Observed};
    use crate::index::IndexRecord;
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
                        Outbound::HaveUpTo { folder, seq } => {
                            Event::HaveUpToReceived { from, folder, seq }
                        }
                    };
                    let replies = engines.get_mut(&to).unwrap().handle(now, event);
                    rest.extend(deliver(now, to, replies, engines));
                }
                other => rest.push((from, other)),
            }
        }
        rest
    }

    /// Actions without the `WantChanged` persistence hooks, for tests that
    /// assert on exact positions.
    fn core(actions: &[Action]) -> Vec<Action> {
        actions
            .iter()
            .filter(|a| !matches!(a, Action::WantChanged { .. }))
            .cloned()
            .collect()
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

        // An early tick forms nothing and asks for the wake again, since the
        // host's timer has fired and is no longer pending.
        let out = a.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        assert_eq!(out, [Action::WakeAt(t(13.0))]);
        let out = a.handle(
            t(13.0).plus_nanos(-1),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        assert_eq!(
            out,
            [Action::WakeAt(t(13.0))],
            "1 ns early still re-announces"
        );

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
        let f = b.folder(folder()).unwrap();
        let want = f.wants().get(&p("x")).unwrap();
        assert_eq!(want.source, node(1));
        assert_eq!(f.wants().len(), 1);
        assert_eq!(want.mode, ApplyMode::Fetch);
        assert!(
            matches!(want.state, WantState::Fetching { from, .. } if from == node(1)),
            "A is connected and announced it, so the fetch started"
        );
        // A's watermark for B moved to the batch's seq_high.
        assert_eq!(
            engines[&node(1)]
                .folder(folder())
                .unwrap()
                .acked_by(node(2)),
            1
        );
        // Receiving is not writing: B has no window.
        assert_eq!(b.folder(folder()).unwrap().window(), None);
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
        let item = b
            .folder(folder())
            .unwrap()
            .wants()
            .iter()
            .next()
            .unwrap()
            .entry
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
        let out = core(&out);
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
        let want = c
            .folder(folder())
            .unwrap()
            .wants()
            .get(&p("dir/note.md"))
            .unwrap();
        assert_eq!(want.source, node(2), "arrived via B");
        let got = &want.entry;
        assert_eq!(got, &a_entry);
        assert_eq!(got.version, Version::empty().incremented(node(1)));
        assert_eq!(got.modified_by, node(1));
        assert_eq!(got.author_host.as_str(), "alpha");

        // A ignored the equal version and B's window closed; nothing loops.
        let a = &engines[&node(1)];
        assert!(a.folder(folder()).unwrap().wants().is_empty());
        assert_eq!(engines[&node(2)].folder(folder()).unwrap().due(), None);
    }

    #[test]
    fn a_conflict_resolves_to_m_on_both_sides_and_the_copy_is_announced() {
        let mut engines = BTreeMap::new();
        let mut a = engine(1, "alpha");
        let mut b = engine(2, "bravo");
        join(&mut a, &[1, 2]);
        join(&mut b, &[1, 2]);
        connect(&mut a, &mut b);
        // B creates the base and A commits it.
        b.handle(
            t(1.0),
            Event::Scanned {
                folder: folder(),
                path: p("x"),
                state: file(1, 1),
            },
        );
        let out = b.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        engines.insert(node(1), a);
        engines.insert(node(2), b);
        deliver(t(3.0), node(2), out, &mut engines);
        let base = engines[&node(1)]
            .folder(folder())
            .unwrap()
            .wants()
            .iter()
            .next()
            .unwrap()
            .entry
            .clone();
        engines.get_mut(&node(1)).unwrap().handle(
            t(4.0),
            Event::Applied {
                folder: folder(),
                path: p("x"),
                version: base.version.clone(),
                outcome: ApplyOutcome::Ok,
            },
        );
        engines.get_mut(&node(1)).unwrap().handle(
            t(6.0),
            Event::Tick {
                fresh_batch_id: fresh(2),
            },
        );

        // Concurrent edits: A at mtime 100 (loses), B at mtime 200 (wins).
        let out_a = engines.get_mut(&node(1)).unwrap().handle(
            t(10.0),
            Event::Scanned {
                folder: folder(),
                path: p("x"),
                state: file(5, 100),
            },
        );
        assert!(matches!(out_a[0], Action::IndexChanged { .. }));
        engines.get_mut(&node(2)).unwrap().handle(
            t(10.0),
            Event::Scanned {
                folder: folder(),
                path: p("x"),
                state: file(6, 200),
            },
        );
        let out_a = engines.get_mut(&node(1)).unwrap().handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        let out_b = engines.get_mut(&node(2)).unwrap().handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(4),
            },
        );
        deliver(t(12.0), node(1), out_a, &mut engines);
        deliver(t(12.0), node(2), out_b, &mut engines);

        // A holds L: fetch W's content, displace to the conflict copy.
        let a_want = engines[&node(1)]
            .folder(folder())
            .unwrap()
            .wants()
            .get(&p("x"))
            .unwrap()
            .clone();
        assert_eq!(a_want.mode, ApplyMode::Fetch);
        let copy = a_want.conflict.clone().unwrap();
        assert_eq!(copy.path.as_str(), "x.conflict-19700101-000000-alpha");
        assert_eq!(copy.loser.hash, hash(5));
        let m = a_want.entry.clone();
        assert_eq!(m.hash, hash(6));
        assert_eq!(m.modified_by, node(2));
        // B holds W: index-only, no copy, the same M, adopted at once with
        // nothing for the host to do.
        let b_folder = engines[&node(2)].folder(folder()).unwrap();
        assert!(b_folder.wants().is_empty());
        assert_eq!(b_folder.index().get(&p("x")).unwrap().entry, m);

        // Both hosts commit. A writes two records: M and the copy.
        let out = engines.get_mut(&node(1)).unwrap().handle(
            t(13.0),
            Event::Applied {
                folder: folder(),
                path: p("x"),
                version: m.version.clone(),
                outcome: ApplyOutcome::Ok,
            },
        );
        let changed: Vec<&IndexRecord> = out
            .iter()
            .filter_map(|a| match a {
                Action::IndexChanged { record, .. } => Some(record),
                _ => None,
            })
            .collect();
        assert_eq!(changed.len(), 2);
        assert_eq!(changed[0].entry, m);
        assert_eq!(changed[1].entry.path, copy.path);
        assert_eq!(changed[1].entry.hash, hash(5));
        assert_eq!(
            changed[1].entry.modified_by,
            node(1),
            "the copy is A's change"
        );
        assert_eq!(changed[1].entry.author_host.as_str(), "alpha");
        assert_eq!(changed[1].entry.prev_hash, ContentHash::EMPTY);
        let out = engines.get_mut(&node(2)).unwrap().handle(
            t(13.0),
            Event::Applied {
                folder: folder(),
                path: p("x"),
                version: m.version.clone(),
                outcome: ApplyOutcome::Ok,
            },
        );
        assert!(
            !out.iter().any(|a| matches!(a, Action::IndexChanged { .. })),
            "B already adopted M; a stray report changes nothing"
        );

        // A's next batch carries M and the copy; B ignores M and takes the copy.
        let out = engines.get_mut(&node(1)).unwrap().handle(
            t(15.0),
            Event::Tick {
                fresh_batch_id: fresh(5),
            },
        );
        let Outbound::Batch(batch) = sends(&out)[0].1.clone() else {
            panic!()
        };
        assert_eq!(batch.entries.len(), 2);
        assert_eq!(
            batch.summary.adds, 1,
            "the copy is A's local add; M is relayed"
        );
        deliver(t(15.0), node(1), out, &mut engines);
        let b_folder = engines[&node(2)].folder(folder()).unwrap();
        assert_eq!(b_folder.index().get(&p("x")).unwrap().entry, m);
        assert_eq!(
            b_folder.wants().len(),
            1,
            "M is equal on B; only the copy is wanted"
        );
        let want = b_folder.wants().get(&copy.path).unwrap();
        assert_eq!(want.mode, ApplyMode::Fetch);
        assert!(want.conflict.is_none());
        assert_eq!(b_folder.winner_fallbacks(), 0);
        assert_eq!(
            engines[&node(1)]
                .folder(folder())
                .unwrap()
                .winner_fallbacks(),
            0
        );
    }

    fn tight() -> Rules {
        Rules {
            hold_count: 3,
            hold_pct: 25,
            hold_size: 1_000,
            ..Rules::default()
        }
    }

    fn join_with(e: &mut Engine, rules: Rules, members: &[u8]) {
        e.handle(
            t(0.0),
            Event::FolderJoined {
                folder: folder(),
                rules,
                members: members.iter().map(|i| node(*i)).collect(),
            },
        );
    }

    /// A and B connected, both with tight rules, both holding ten files A made.
    /// §8.3 step 4: a reverted path whose content no member can serve.
    #[test]
    fn an_unrecoverable_revert_ends_in_a_tombstone_once_every_member_answered() {
        // Three members; C starts offline. B creates three files and
        // announces them; A wants them but never fetches (its fetches go
        // unanswered). B's user deletes all three (the folder pauses on the
        // deletes), then reverts. The restored records describe content only
        // B ever had, and B's user removed it.
        let mut a = engine(1, "alpha");
        let mut b = engine(2, "bravo");
        let mut c = engine(3, "charlie");
        for e in [&mut a, &mut b, &mut c] {
            join_with(e, tight(), &[1, 2, 3]);
        }
        connect(&mut a, &mut b);
        for i in 0..3 {
            b.handle(
                t(1.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i}")),
                    state: file(1, 1),
                },
            );
        }
        let out = b.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        let mut engines = BTreeMap::from([(node(1), a), (node(2), b), (node(3), c)]);
        deliver(t(3.0), node(2), out, &mut engines);
        let b = engines.get_mut(&node(2)).unwrap();
        for i in 0..3 {
            b.handle(
                t(10.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i}")),
                    state: ScanState::Absent,
                },
            );
        }
        let out = b.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(2),
            },
        );
        assert!(out.iter().any(|a| matches!(
            a,
            Action::StatusChanged {
                status: FolderStatus::Paused { .. },
                ..
            }
        )));
        let out = b.handle(t(13.0), Event::Revert { folder: folder() });
        let fetches: Vec<(RelPath, Version)> = out
            .iter()
            .filter_map(|a| match a {
                Action::Fetch {
                    path,
                    version,
                    from,
                    ..
                } if *from == node(1) => Some((path.clone(), version.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(fetches.len(), 3, "restoring wants ask the connected member");
        // A never had the bytes.
        for (path, version) in &fetches {
            let out = b.handle(
                t(14.0),
                Event::Fetched {
                    folder: folder(),
                    path: path.clone(),
                    hash: hash(1),
                    version: version.clone(),
                    outcome: FetchReport::NotAvailable,
                },
            );
            assert!(
                !out.iter().any(|a| matches!(
                    a,
                    Action::IndexChanged { .. }
                        | Action::StatusChanged {
                            status: FolderStatus::Unrecoverable { .. },
                            ..
                        }
                )),
                "C has not been asked yet"
            );
        }
        let f = b.folder(folder()).unwrap();
        assert_eq!(f.wants().len(), 3, "the wants wait for C");
        for w in f.wants().iter() {
            assert!(w.restoring);
            assert_eq!(w.answered, BTreeSet::from([node(1)]));
            assert!(
                !w.answered.contains(&node(3)),
                "C is the member not yet asked"
            );
        }
        // C connects and is asked; it never had the files either.
        let out = b.handle(
            t(20.0),
            Event::PeerConnected {
                peer: node(3),
                tier: Tier::Lan,
            },
        );
        let mut all = out;
        all.extend(b.handle(
            t(21.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        ));
        let fetches: Vec<(RelPath, Version)> = all
            .iter()
            .filter_map(|a| match a {
                Action::Fetch {
                    path,
                    version,
                    from,
                    ..
                } if *from == node(3) => Some((path.clone(), version.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            fetches.len(),
            3,
            "the late member is asked when it connects"
        );
        let mut tombstones = 0;
        let mut statuses = 0;
        for (path, version) in &fetches {
            let out = b.handle(
                t(22.0),
                Event::Fetched {
                    folder: folder(),
                    path: path.clone(),
                    hash: hash(1),
                    version: version.clone(),
                    outcome: FetchReport::NotAvailable,
                },
            );
            tombstones += out.iter().filter(|a| matches!(a, Action::IndexChanged { record, .. } if record.entry.deleted && record.entry.path == *path)).count();
            statuses += out.iter().filter(|a| matches!(a, Action::StatusChanged { status: FolderStatus::Unrecoverable { path: sp }, .. } if sp == path)).count();
        }
        assert_eq!((tombstones, statuses), (3, 3));
        let f = b.folder(folder()).unwrap();
        assert!(f.wants().is_empty(), "the wants are dropped");
        for i in 0..3 {
            let e = &f.index().get(&p(&format!("f{i}"))).unwrap().entry;
            assert!(e.deleted, "the deletion stands");
            assert_eq!(
                e.prev_hash,
                hash(1),
                "prev_hash is the restored record's hash"
            );
            assert_eq!(e.modified_by, node(2), "a local change");
        }
        // The tombstones go out at the next tick (§8.3 step 4) and are
        // exempt from the sender pre-check (§8.1): the same three deletions
        // paused the folder when the user made them, and pause nothing now,
        // since no member holds what they remove.
        let out = b.handle(
            t(30.0),
            Event::Tick {
                fresh_batch_id: fresh(4),
            },
        );
        assert!(
            !out.iter().any(|a| matches!(
                a,
                Action::StatusChanged {
                    status: FolderStatus::Paused { .. },
                    ..
                }
            )),
            "no pause on unrecoverable tombstones: {out:?}"
        );
        let batches: Vec<&Batch> = out
            .iter()
            .filter_map(|a| match a {
                Action::Send {
                    payload: Outbound::Batch(batch),
                    ..
                } => Some(batch),
                _ => None,
            })
            .collect();
        assert_eq!(batches.len(), 2, "one batch to each connected member");
        for batch in batches {
            assert_eq!(batch.entries.len(), 3);
            assert!(batch.entries.iter().all(|e| e.deleted));
            assert_eq!(batch.summary.dels, 0, "counted for nothing");
        }
    }

    fn two_with_ten_files() -> BTreeMap<NodeId, Engine> {
        let mut a = engine(1, "alpha");
        let mut b = engine(2, "bravo");
        join_with(&mut a, tight(), &[1, 2]);
        join_with(&mut b, tight(), &[1, 2]);
        connect(&mut a, &mut b);
        for i in 0..10 {
            a.handle(
                t(1.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i:02}")),
                    state: file(1, 1),
                },
            );
        }
        let out = a.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        let mut engines = BTreeMap::from([(node(1), a), (node(2), b)]);
        deliver(t(3.0), node(1), out, &mut engines);
        let b = engines.get_mut(&node(2)).unwrap();
        let items: Vec<(RelPath, Version)> = b
            .folder(folder())
            .unwrap()
            .wants()
            .iter()
            .map(|w| (w.path().clone(), w.version().clone()))
            .collect();
        for (path, version) in items {
            b.handle(
                t(4.0),
                Event::Applied {
                    folder: folder(),
                    path,
                    version,
                    outcome: ApplyOutcome::Ok,
                },
            );
        }
        let out = b.handle(
            t(6.0),
            Event::Tick {
                fresh_batch_id: fresh(2),
            },
        );
        deliver(t(6.0), node(2), out, &mut engines);
        engines
    }

    fn statuses(actions: &[Action]) -> Vec<&FolderStatus> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::StatusChanged { status, .. } => Some(status),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_mass_delete_pauses_the_sender_until_approve() {
        let mut engines = two_with_ten_files();
        let b = engines.get_mut(&node(2)).unwrap();
        for i in 0..8 {
            b.handle(
                t(10.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i:02}")),
                    state: ScanState::Absent,
                },
            );
        }
        let out = b.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        assert!(sends(&out).is_empty(), "nothing leaves");
        assert!(matches!(
            out[0],
            Action::RecordBatch {
                role: BatchRole::Paused,
                decision: None,
                ..
            }
        ));
        assert!(
            matches!(statuses(&out)[0], FolderStatus::Paused { batch, .. } if *batch == fresh(3))
        );
        // Later changes and ticks: still nothing leaves.
        b.handle(
            t(13.0),
            Event::Scanned {
                folder: folder(),
                path: p("f09"),
                state: file(2, 2),
            },
        );
        let out = b.handle(
            t(15.0),
            Event::Tick {
                fresh_batch_id: fresh(4),
            },
        );
        assert!(sends(&out).is_empty());
        assert!(
            matches!(statuses(&out)[0], FolderStatus::PausedRecheck { batch, would_pass: false, .. } if *batch == fresh(3))
        );
        assert!(
            b.handle(
                t(16.0),
                Event::Approve {
                    folder: folder(),
                    batch: fresh(9)
                }
            )
            .iter()
            .any(|a| matches!(
                a,
                Action::StatusChanged {
                    status: FolderStatus::UnknownBatch { .. },
                    ..
                }
            ))
        );
        let out = b.handle(
            t(16.0),
            Event::Approve {
                folder: folder(),
                batch: fresh(3),
            },
        );
        let Outbound::Batch(batch) = sends(&out)[0].1.clone() else {
            panic!()
        };
        assert_eq!(batch.id, fresh(3), "sent under the reserved id");
        assert_eq!((batch.summary.dels, batch.summary.mods), (8, 1));
        assert!(matches!(
            out.last(),
            Some(Action::StatusChanged {
                status: FolderStatus::Unpaused { .. },
                ..
            })
        ));
        // A holds it: 9 destructive of 10 tracked.
        let rest = deliver(t(16.0), node(2), out, &mut engines);
        assert!(rest.iter().any(|(who, a)| *who == node(1)
            && matches!(
                a,
                Action::StatusChanged {
                    status: FolderStatus::Held { .. },
                    ..
                }
            )));
        assert!(rest.iter().any(|(who, a)| *who == node(1)
            && matches!(
                a,
                Action::RecordBatch {
                    role: BatchRole::Received,
                    decision: Some(Decision::Held { .. }),
                    ..
                }
            )));
        assert!(
            engines[&node(1)]
                .folder(folder())
                .unwrap()
                .wants()
                .is_empty()
        );
        assert_eq!(
            engines[&node(2)]
                .folder(folder())
                .unwrap()
                .acked_by(node(1)),
            batch.seq_high,
            "held acknowledges"
        );
    }

    #[test]
    fn revert_emits_trash_moves_and_refetches() {
        let mut engines = two_with_ten_files();
        let b = engines.get_mut(&node(2)).unwrap();
        for i in 0..8 {
            b.handle(
                t(10.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i:02}")),
                    state: ScanState::Absent,
                },
            );
        }
        b.handle(
            t(10.0),
            Event::Scanned {
                folder: folder(),
                path: p("junk"),
                state: file(6, 6),
            },
        );
        let out = b.handle(t(11.0), Event::Revert { folder: folder() });
        assert_eq!(
            out,
            [Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::NotPaused
            }],
            "the wake for 12 s was already announced after the scans"
        );
        b.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        let out = b.handle(t(13.0), Event::Revert { folder: folder() });
        let fetches = out
            .iter()
            .filter(|a| matches!(a, Action::Fetch { .. }))
            .count();
        assert_eq!(
            fetches, 4,
            "eight restored wants, four fetch slots per peer"
        );
        let out = core(&out);
        // The index writes come first, one per pending path (§11: every
        // write is reported before the host acts on it).
        let restored = out
            .iter()
            .filter(|a| matches!(a, Action::IndexChanged { .. }))
            .count();
        assert_eq!(
            restored, 8,
            "one announced record put back per reverted path"
        );
        assert_eq!(
            out[8],
            Action::IndexRemoved {
                folder: folder(),
                path: p("junk")
            },
            "a pending add peers never saw is removed, not restored"
        );
        let out: Vec<Action> = out
            .into_iter()
            .filter(|a| !matches!(a, Action::IndexChanged { .. } | Action::IndexRemoved { .. }))
            .collect();
        assert_eq!(
            out[0],
            Action::MoveToTrash {
                folder: folder(),
                path: p("junk")
            }
        );
        assert_eq!(
            out[1],
            Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::Reverted {
                    batch: fresh(3),
                    trashed: 1,
                    refetch: 8
                }
            }
        );
        assert!(
            matches!(out.last(), Some(Action::WakeAt(_))),
            "a wake for the fetch deadlines"
        );
        let f = b.folder(folder()).unwrap();
        assert_eq!(f.index().tracked_count(), 10);
        assert_eq!(f.index().pending_count(), 0);
        assert!(f.in_flight(&p("f00")));
        let out = b.handle(
            t(15.0),
            Event::Tick {
                fresh_batch_id: fresh(4),
            },
        );
        assert!(sends(&out).is_empty(), "nothing to announce after revert");
    }

    /// §8.3: revert re-derives the wants at reverted paths from their
    /// received entries. The sequence the simulator found: a crash after
    /// the rename leaves a fetched file the index never learnt of; the
    /// folder pauses on other changes; the next scan makes the file a local
    /// add, so the newer remote version already wanted becomes a conflict's
    /// `M` folding that add in; a still newer version is deferred behind
    /// `M`; `revert` discards the add. `M` described a merge that could
    /// never happen and content no peer held. The received entry
    /// re-classifies to a plain add, the version kept while paused is
    /// re-admitted at unpause, dominates and replaces it, and its content
    /// is fetched.
    #[test]
    fn revert_rederives_a_conflict_want_and_the_newer_version_is_fetched() {
        let mut engines = two_with_ten_files();
        // A adds n; B fetches it and the rename lands.
        let a = engines.get_mut(&node(1)).unwrap();
        a.handle(
            t(10.0),
            Event::Scanned {
                folder: folder(),
                path: p("n"),
                state: file(7, 7),
            },
        );
        let out = a.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        let rest = deliver(t(12.0), node(1), out, &mut engines);
        let fetch_of = |rest: &[(NodeId, Action)], h: ContentHash| {
            rest.iter().find_map(|(who, a)| match a {
                Action::Fetch {
                    path,
                    version,
                    hash: got,
                    from,
                    ..
                } if *who == node(2) && path == &p("n") && *got == h => {
                    Some((version.clone(), *from))
                }
                _ => None,
            })
        };
        let tagged = |out: Vec<Action>| -> Vec<(NodeId, Action)> {
            out.into_iter().map(|a| (node(2), a)).collect()
        };
        let (v1, _) = fetch_of(&rest, hash(7)).unwrap();
        let b = engines.get_mut(&node(2)).unwrap();
        let out = b.handle(
            t(13.0),
            Event::Fetched {
                folder: folder(),
                path: p("n"),
                hash: hash(7),
                version: v1.clone(),
                outcome: FetchReport::Ok,
            },
        );
        assert!(out.iter().any(|a| matches!(a, Action::Write { .. })));
        // B crashes after the rename: the file is on disk, the report is
        // lost, and the process comes back with the persisted state.
        let snapshot = b.folder(folder()).unwrap().clone();
        let restored = Engine::restore(b.config().clone(), vec![snapshot]);
        engines.insert(node(2), restored);
        let b = engines.get_mut(&node(2)).unwrap();
        let out = b.handle(
            t(20.0),
            Event::PeerConnected {
                peer: node(1),
                tier: Tier::Lan,
            },
        );
        assert!(
            fetch_of(&tagged(out.clone()), hash(7)).is_some(),
            "asked again"
        );
        deliver(t(20.0), node(2), out, &mut engines);
        let b = engines.get_mut(&node(2)).unwrap();
        b.handle(
            t(21.0),
            Event::Fetched {
                folder: folder(),
                path: p("n"),
                hash: hash(7),
                version: v1,
                outcome: FetchReport::NotAvailable,
            },
        );
        // A edits n (stamp 70): B wants v2, and A has moved on again, so
        // the want is without source and the path observable.
        let a = engines.get_mut(&node(1)).unwrap();
        a.handle(
            t(22.0),
            Event::Scanned {
                folder: folder(),
                path: p("n"),
                state: file(8, 70),
            },
        );
        let out = a.handle(
            t(24.0),
            Event::Tick {
                fresh_batch_id: fresh(4),
            },
        );
        let rest = deliver(t(24.0), node(1), out, &mut engines);
        let (v2, _) = fetch_of(&rest, hash(8)).unwrap();
        let b = engines.get_mut(&node(2)).unwrap();
        b.handle(
            t(25.0),
            Event::Fetched {
                folder: folder(),
                path: p("n"),
                hash: hash(8),
                version: v2.clone(),
                outcome: FetchReport::NotAvailable,
            },
        );
        // The watcher reports three deletes; B pauses at the tick.
        for i in 0..3 {
            b.handle(
                t(26.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i:02}")),
                    state: ScanState::Absent,
                },
            );
        }
        let out = b.handle(
            t(28.0),
            Event::Tick {
                fresh_batch_id: fresh(5),
            },
        );
        assert!(
            statuses(&out)
                .iter()
                .any(|s| matches!(s, FolderStatus::Paused { .. }))
        );
        // The next full scan finds the file the crash left: a local add
        // while paused, concurrent with v2, so the want becomes M with A's
        // content.
        b.handle(t(30.0), Event::ScanStarted { folder: folder() });
        b.handle(
            t(30.0),
            Event::Scanned {
                folder: folder(),
                path: p("n"),
                state: file(7, 7),
            },
        );
        for i in 3..10 {
            b.handle(
                t(30.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i:02}")),
                    state: ScanState::Unchanged,
                },
            );
        }
        b.handle(t(30.0), Event::ScanFinished { folder: folder() });
        let m = b
            .folder(folder())
            .unwrap()
            .wants()
            .get(&p("n"))
            .unwrap()
            .clone();
        assert!(m.entry.version.dominates(&v2), "M folds the local add in");
        assert_eq!(m.entry.hash, hash(8));
        assert!(m.conflict.is_some());
        assert_eq!(m.received, {
            let mut e = m.entry.clone();
            e.version = v2.clone();
            e
        });
        // A edits n once more; v3 is kept while paused, behind M.
        let a = engines.get_mut(&node(1)).unwrap();
        a.handle(
            t(32.0),
            Event::Scanned {
                folder: folder(),
                path: p("n"),
                state: file(9, 80),
            },
        );
        let out = a.handle(
            t(34.0),
            Event::Tick {
                fresh_batch_id: fresh(6),
            },
        );
        let rest = deliver(t(34.0), node(1), out, &mut engines);
        assert!(fetch_of(&rest, hash(9)).is_none(), "deferred, not wanted");
        let v3 = engines[&node(1)]
            .folder(folder())
            .unwrap()
            .index()
            .get(&p("n"))
            .unwrap()
            .entry
            .version
            .clone();
        let b = engines.get_mut(&node(2)).unwrap();
        let f = b.folder(folder()).unwrap();
        assert_eq!(f.wants().get(&p("n")).unwrap().entry, m.entry);
        assert_eq!(f.deferred().filter(|d| d.entry.path == p("n")).count(), 1);
        // Revert: the add peers never saw is removed and trashed; the want
        // is re-derived from v2 as received, a plain add; the version kept
        // while paused is re-admitted at unpause, dominates it, replaces
        // it, and its content is fetched from A. Had M stayed, v3 would
        // have been re-deferred as concurrent with it.
        let out = b.handle(t(36.0), Event::Revert { folder: folder() });
        assert!(
            out.iter()
                .any(|a| matches!(a, Action::IndexRemoved { path, .. } if path == &p("n")))
        );
        assert!(
            out.iter()
                .any(|a| matches!(a, Action::MoveToTrash { path, .. } if path == &p("n")))
        );
        let (wanted, from) = fetch_of(&tagged(out), hash(9)).expect("v3's content is fetched");
        assert_eq!((wanted, from), (v3.clone(), node(1)));
        let f = b.folder(folder()).unwrap();
        let want = f.wants().get(&p("n")).unwrap();
        assert_eq!(want.version(), &v3);
        assert!(want.conflict.is_none(), "nothing local to displace");
        assert!(!want.restoring, "no announced record was restored at n");
        assert_eq!(f.deferred().count(), 0);
        let out = b.handle(
            t(39.0),
            Event::Fetched {
                folder: folder(),
                path: p("n"),
                hash: hash(9),
                version: v3.clone(),
                outcome: FetchReport::Ok,
            },
        );
        assert!(
            out.iter().any(
                |a| matches!(a, Action::Write { path, expected: None, .. } if path == &p("n"))
            )
        );
        let out = b.handle(
            t(40.0),
            Event::Applied {
                folder: folder(),
                path: p("n"),
                version: v3.clone(),
                outcome: ApplyOutcome::Ok,
            },
        );
        assert!(
            matches!(&out[0], Action::IndexChanged { record, .. } if record.entry.hash == hash(9) && record.entry.version == v3)
        );
        assert!(b.folder(folder()).unwrap().wants().get(&p("n")).is_none());
    }

    #[test]
    fn deny_sends_bumps_and_rule_changes_only_report() {
        let mut engines = two_with_ten_files();
        let a = engines.get_mut(&node(1)).unwrap();
        for i in 0..8 {
            a.handle(
                t(10.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i:02}")),
                    state: ScanState::Absent,
                },
            );
        }
        let out = a.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        assert!(sends(&out).is_empty(), "A's own pre-check pauses it first");
        let out = a.handle(
            t(12.0),
            Event::Approve {
                folder: folder(),
                batch: fresh(3),
            },
        );
        assert_eq!(sends(&out).len(), 1, "approved: sent under the reserved id");
        let rest = deliver(t(12.0), node(1), out, &mut engines);
        assert!(rest.iter().any(|(who, a)| *who == node(2) && matches!(a, Action::StatusChanged { status: FolderStatus::Held { batch, paths: 8, .. }, .. } if *batch == fresh(3))));
        let b = engines.get_mut(&node(2)).unwrap();
        let out = b.handle(
            t(13.0),
            Event::RulesChanged {
                folder: folder(),
                rules: Rules {
                    hold_count: 0,
                    ..Rules::default()
                },
            },
        );
        assert_eq!(
            out,
            [Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::RulesRecheck {
                    batch: fresh(3),
                    would_pass: true
                }
            }]
        );
        assert_eq!(
            b.folder(folder()).unwrap().quarantine().len(),
            1,
            "reported, not released"
        );
        let out = b.handle(
            t(14.0),
            Event::Deny {
                folder: folder(),
                batch: fresh(3),
            },
        );
        assert_eq!(
            out.iter()
                .filter(|a| matches!(a, Action::IndexChanged { .. }))
                .count(),
            8
        );
        assert!(out.iter().any(|a| matches!(
            a,
            Action::StatusChanged {
                status: FolderStatus::Denied { paths: 8, .. },
                ..
            }
        )));
        assert_eq!(out.last(), Some(&Action::WakeAt(t(16.0))));
        let out = b.handle(
            t(16.0),
            Event::Tick {
                fresh_batch_id: fresh(4),
            },
        );
        let Outbound::Batch(batch) = sends(&out)[0].1.clone() else {
            panic!()
        };
        assert_eq!(batch.entries.len(), 8);
        assert_eq!(batch.summary, Summary::default(), "touches");
        let rest = deliver(t(16.0), node(2), out, &mut engines);
        assert!(!rest.iter().any(|(_, a)| matches!(
            a,
            Action::StatusChanged {
                status: FolderStatus::Held { .. },
                ..
            }
        )));
        let a = engines[&node(1)].folder(folder()).unwrap();
        assert_eq!(a.wants().len(), 8, "A fetches its files back");
    }

    #[test]
    fn connecting_exchanges_have_up_to_and_catch_up_follows_at_the_next_tick() {
        let mut a = engine(1, "alpha");
        let mut b = engine(2, "bravo");
        join(&mut a, &[1, 2]);
        join(&mut b, &[1, 2]);
        // A announces three files while B is not connected.
        for i in 0..3 {
            a.handle(
                t(1.0),
                Event::Scanned {
                    folder: folder(),
                    path: p(&format!("f{i}")),
                    state: file(1, 1),
                },
            );
        }
        let out = a.handle(
            t(3.0),
            Event::Tick {
                fresh_batch_id: fresh(1),
            },
        );
        assert!(sends(&out).is_empty(), "nobody connected");
        // Both sides connect: each tells the other what it holds.
        let out_a = a.handle(
            t(5.0),
            Event::PeerConnected {
                peer: node(2),
                tier: Tier::Lan,
            },
        );
        assert_eq!(
            sends(&out_a),
            [(
                node(2),
                &Outbound::HaveUpTo {
                    folder: folder(),
                    seq: 0
                }
            )]
        );
        let out_b = b.handle(
            t(5.0),
            Event::PeerConnected {
                peer: node(1),
                tier: Tier::Lan,
            },
        );
        assert_eq!(
            sends(&out_b),
            [(
                node(1),
                &Outbound::HaveUpTo {
                    folder: folder(),
                    seq: 0
                }
            )]
        );
        let mut engines = BTreeMap::from([(node(1), a), (node(2), b)]);
        let rest = deliver(t(5.0), node(1), out_a, &mut engines);
        // B received A's have_up_to: it has nothing above 0 of A's... it asks
        // for an immediate tick to check.
        assert!(rest.iter().any(|(who, a)| *who == node(2) && matches!(a, Action::WakeAt(ts) if *ts == t(5.0))));
        let rest = deliver(t(5.0), node(2), out_b, &mut engines);
        assert!(rest.iter().any(|(who, a)| *who == node(1) && matches!(a, Action::WakeAt(ts) if *ts == t(5.0))));
        // A's tick sends the catch-up batch; B's tick has nothing to send.
        let out = engines.get_mut(&node(1)).unwrap().handle(
            t(5.0),
            Event::Tick {
                fresh_batch_id: fresh(2),
            },
        );
        let Outbound::Batch(batch) = sends(&out)[0].1.clone() else {
            panic!()
        };
        assert_eq!(batch.id, fresh(2));
        assert_eq!(batch.entries.len(), 3);
        assert_eq!(batch.seq_high, 3);
        assert!(out.iter().any(|a| matches!(
            a,
            Action::RecordBatch {
                role: BatchRole::Sent,
                ..
            }
        )));
        deliver(t(5.0), node(1), out, &mut engines);
        let out = engines.get_mut(&node(2)).unwrap().handle(
            t(5.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        assert!(sends(&out).is_empty());
        let b = engines[&node(2)].folder(folder()).unwrap();
        assert_eq!(b.wants().len(), 3);
        assert_eq!(
            engines[&node(1)]
                .folder(folder())
                .unwrap()
                .acked_by(node(2)),
            3,
            "the decision acknowledged it"
        );
    }

    #[test]
    fn the_fetch_flow_through_the_engine() {
        let mut engines = two_with_ten_files();
        let a = engines.get_mut(&node(1)).unwrap();
        a.handle(
            t(10.0),
            Event::Scanned {
                folder: folder(),
                path: p("n"),
                state: file(7, 7),
            },
        );
        let out = a.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        let rest = deliver(t(12.0), node(1), out, &mut engines);
        let fetch = rest
            .iter()
            .find_map(|(who, a)| match a {
                Action::Fetch {
                    path,
                    version,
                    hash: h,
                    size,
                    from,
                    ..
                } if *who == node(2) => Some((path.clone(), version.clone(), *h, *size, *from)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (fetch.0.as_str(), fetch.2, fetch.3, fetch.4),
            ("n", hash(7), 10, node(1))
        );
        assert!(
            rest.iter().any(|(who, a)| *who == node(2)
                && matches!(a, Action::WantChanged { want: Some(_), .. }))
        );
        let b = engines.get_mut(&node(2)).unwrap();
        // Progress pushes the deadline; the wake follows it.
        let out = b.handle(
            t(40.0),
            Event::FetchProgress {
                folder: folder(),
                path: p("n"),
                hash: fetch.2,
                version: fetch.1.clone(),
            },
        );
        assert_eq!(out.last(), Some(&Action::WakeAt(t(100.0))));
        let out = b.handle(
            t(50.0),
            Event::Fetched {
                folder: folder(),
                path: p("n"),
                hash: fetch.2,
                version: fetch.1.clone(),
                outcome: FetchReport::Ok,
            },
        );
        let write = out
            .iter()
            .find(|a| matches!(a, Action::Write { .. }))
            .unwrap();
        assert!(
            matches!(write, Action::Write { path, expected: None, displace: Displace::Trash, entry, .. } if path == &p("n") && entry.hash == hash(7))
        );
        assert_eq!(
            out.last(),
            Some(&Action::WakeAt(t(80.0))),
            "the commit deadline"
        );
        let out = b.handle(
            t(51.0),
            Event::Applied {
                folder: folder(),
                path: p("n"),
                version: fetch.1.clone(),
                outcome: ApplyOutcome::Ok,
            },
        );
        assert!(
            matches!(&out[0], Action::IndexChanged { record, .. } if record.entry.hash == hash(7))
        );
        assert!(
            out.iter().any(
                |a| matches!(a, Action::WantChanged { want: None, path, .. } if path == &p("n"))
            )
        );
        assert!(b.folder(folder()).unwrap().wants().is_empty());
    }

    #[test]
    fn a_stalled_fetch_is_reported_and_retried_at_the_next_tick() {
        let mut engines = two_with_ten_files();
        let a = engines.get_mut(&node(1)).unwrap();
        a.handle(
            t(10.0),
            Event::Scanned {
                folder: folder(),
                path: p("n"),
                state: file(7, 7),
            },
        );
        let out = a.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        deliver(t(12.0), node(1), out, &mut engines);
        let b = engines.get_mut(&node(2)).unwrap();
        let out = b.handle(
            t(71.0),
            Event::Tick {
                fresh_batch_id: fresh(4),
            },
        );
        assert!(
            !out.iter().any(|a| matches!(a, Action::Fetch { .. })),
            "1 s before the deadline"
        );
        let out = b.handle(
            t(72.0),
            Event::Tick {
                fresh_batch_id: fresh(5),
            },
        );
        assert!(out.iter().any(|a| matches!(a, Action::StatusChanged { status: FolderStatus::Stalled { path }, .. } if path == &p("n"))));
        assert!(
            out.iter()
                .any(|a| matches!(a, Action::Fetch { from, .. } if *from == node(1))),
            "retried, same source"
        );
    }

    #[test]
    fn restore_rebuilds_folders_and_re_wants_in_flight_work() {
        let mut engines = two_with_ten_files();
        let a = engines.get_mut(&node(1)).unwrap();
        a.handle(
            t(10.0),
            Event::Scanned {
                folder: folder(),
                path: p("n"),
                state: file(7, 7),
            },
        );
        let out = a.handle(
            t(12.0),
            Event::Tick {
                fresh_batch_id: fresh(3),
            },
        );
        deliver(t(12.0), node(1), out, &mut engines);
        let b = &engines[&node(2)];
        let before = b.folder(folder()).unwrap().clone();
        assert!(matches!(
            before.wants().get(&p("n")).unwrap().state,
            WantState::Fetching { .. }
        ));
        // The process dies and comes back with the persisted folder state.
        let mut restored = Engine::restore(b.config().clone(), vec![before.clone()]);
        let f = restored.folder(folder()).unwrap();
        assert_eq!(f.index(), before.index());
        assert_eq!(
            f.wants().get(&p("n")).unwrap().state,
            WantState::Wanted,
            "the fetch died with the process"
        );
        assert_eq!(restored.peers().count(), 0, "peers reconnect");
        assert_eq!(f.window(), None);
        // On reconnect it exchanges have_up_to and the fetch starts again.
        let out = restored.handle(
            t(20.0),
            Event::PeerConnected {
                peer: node(1),
                tier: Tier::Lan,
            },
        );
        assert!(sends(&out).iter().any(|(to, o)| *to == node(1) && matches!(o, Outbound::HaveUpTo { seq, .. } if *seq == before.index().peer_seq(node(1)))));
        assert!(out.iter().any(
            |a| matches!(a, Action::Fetch { path, from, .. } if path == &p("n") && *from == node(1))
        ));
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
    fn joining_an_already_joined_folder_keeps_the_index() {
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
        let out = e.handle(
            t(2.0),
            Event::FolderJoined {
                folder: folder(),
                rules: Rules::default(),
                members: vec![node(1)],
            },
        );
        assert_eq!(
            out,
            [Action::StatusChanged {
                folder: folder(),
                status: FolderStatus::AlreadyJoined
            }]
        );
        let f = e.folder(folder()).unwrap();
        assert_eq!(f.index().len(), 1, "index kept");
        assert_eq!(
            f.members().collect::<Vec<_>>(),
            [node(1), node(2)],
            "members kept"
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
            expected: Some(Observed {
                kind: Kind::File,
                size: 1,
                mtime_ns: 2,
                exec: false,
                hash: hash(1),
            }),
            displace: Displace::ConflictCopy(p("x.conflict")),
        };
        let json = serde_json::to_string(&act).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), act);
    }

    proptest! {
        /// §8.1: once paused, no batch leaves the folder whatever local
        /// changes or ticks follow, until Approve.
        #[test]
        fn a_paused_folder_never_sends_until_approve(
            steps in prop::collection::vec((0u8..12, 0u8..3, any::<bool>(), any::<bool>()), 1..20),
        ) {
            let mut engines = two_with_ten_files();
            let b = engines.get_mut(&node(2)).unwrap();
            for i in 0..8 {
                b.handle(t(10.0), Event::Scanned { folder: folder(), path: p(&format!("f{i:02}")), state: ScanState::Absent });
            }
            let out = b.handle(t(12.0), Event::Tick { fresh_batch_id: fresh(3) });
            prop_assert!(sends(&out).is_empty());
            let mut now = t(12.0);
            for (k, (i, h, gone, tick)) in steps.into_iter().enumerate() {
                now = now.plus_nanos(1_500_000_000);
                let path = p(&format!("f{:02}", i));
                let state = if gone { ScanState::Absent } else { file(h + 1, k as i64 + 20) };
                let out = b.handle(now, Event::Scanned { folder: folder(), path, state });
                prop_assert!(sends(&out).is_empty());
                if tick {
                    let out = b.handle(now.plus_nanos(12_000_000_000), Event::Tick { fresh_batch_id: fresh(100 + k as u8) });
                    prop_assert!(sends(&out).is_empty(), "{out:?}");
                    now = now.plus_nanos(12_000_000_000);
                }
            }
            prop_assert!(b.folder(folder()).unwrap().paused().is_some());
            let out = b.handle(now.plus_nanos(1), Event::Approve { folder: folder(), batch: fresh(3) });
            prop_assert_eq!(sends(&out).len(), 1);
        }

        /// I6 groundwork: two engines fed the same events, FetchProgress
        /// timings included, emit byte-identical actions.
        #[test]
        fn progress_timings_do_not_break_determinism(
            progress in prop::collection::vec(1i64..70, 0..6),
            connect_c: bool,
        ) {
            let run = |seed: u8| -> Vec<u8> {
                let mut a = engine(1, "alpha");
                let mut b = engine(2, "bravo");
                join(&mut a, &[1, 2, 3]);
                join(&mut b, &[1, 2, 3]);
                connect(&mut a, &mut b);
                if connect_c {
                    b.handle(t(0.0), Event::PeerConnected { peer: node(3), tier: Tier::Relay });
                }
                a.handle(t(1.0), Event::Scanned { folder: folder(), path: p("n"), state: file(seed, 1) });
                let out = a.handle(t(3.0), Event::Tick { fresh_batch_id: fresh(1) });
                let mut engines = BTreeMap::from([(node(1), a), (node(2), b)]);
                let mut all = deliver(t(3.0), node(1), out, &mut engines);
                let v = engines[&node(2)].folder(folder()).unwrap().wants().get(&p("n")).map(|w| w.version().clone());
                let mut now = t(3.0);
                for gap in &progress {
                    now = now.plus_nanos(gap * NANOS_PER_SECOND);
                    let b = engines.get_mut(&node(2)).unwrap();
                    if let Some(v) = &v {
                        all.extend(b.handle(now, Event::FetchProgress { folder: folder(), path: p("n"), hash: hash(seed), version: v.clone() }).into_iter().map(|a| (node(2), a)));
                    }
                    all.extend(b.handle(now.plus_nanos(1), Event::Tick { fresh_batch_id: fresh(9) }).into_iter().map(|a| (node(2), a)));
                }
                let actions: Vec<Action> = all.into_iter().map(|(_, a)| a).collect();
                let mut bytes = postcard::to_stdvec(&actions).unwrap();
                bytes.extend(postcard::to_stdvec(&engines[&node(2)]).unwrap());
                bytes
            };
            prop_assert_eq!(run(7), run(7));
        }
    }
}
