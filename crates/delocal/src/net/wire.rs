//! The protocol's messages (DESIGN.md §12): everything one machine says to
//! another, as [`Message`], and the envelope the engine's messages travel
//! in.
//!
//! **The envelope.** The engine says three things to peers
//! ([`Outbound`]): a batch, a decision on one, and how much of a peer's
//! folder it holds. Each travels as its §12 message: `Batch`,
//! `BatchDecision`, and `FolderMeta`, whose `have_up_to` is the engine's
//! `HaveUpTo` and whose other fields are the host's (§9.1)
//! ([`Message::envelope`]). Received, the same three become the engine's
//! events ([`Message::event`]). Everything else is the host's own: the
//! handshake and keepalive (§6.3) and file transfer (§7.5), whose
//! `RequestFile` names its request with a [`RequestId`] that the source's
//! answers and the requester's `CancelRequest` carry (draft 34).
//!
//! **What two versions say to each other.** A machine sends `Hello` first
//! and reads the other's before anything else, and refuses another protocol
//! version with `Goodbye` (§6.3). So `Hello` and `Goodbye` must be read
//! alike by every version: they are the first two variants of [`Message`],
//! their fields and [`Reason`]'s variants never change or move, and
//! anything new goes after them. The test
//! `hello_and_goodbye_keep_their_bytes` holds them to it.
//!
//! **Optional fields.** Within one protocol version a message may gain
//! optional fields (§12). postcard writes a struct's fields one after the
//! other, with no names, so a new field can only go at the end of a
//! message. A frame's length says where the message ends, and the frame
//! decoder ignores whatever follows the fields it knows (§6.3), so an
//! older machine reads a newer one's messages.
//! The other half, a newer machine reading a message without the field, is
//! for whoever adds the first one.

use delocal_engine::{
    Batch, BatchDecision, ContentHash, Event, FolderId, HostName, NodeId, Outbound, RelPath, Rules,
    Timestamp,
};
use serde::{Deserialize, Serialize};

/// The protocol version (§12): one integer, bumped on any incompatible
/// change. Machines with different versions do not sync (§6.3).
pub const PROTOCOL: u32 = 1;

/// One message between two machines (§12). See the module docs for why
/// the order of the variants matters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// The first message in each direction (§6.3).
    Hello(Hello),
    /// The connection is about to close, and why.
    Goodbye {
        reason: Reason,
    },
    /// A folder as the sender holds it, sent on connect (§7.4, §9.1).
    FolderMeta(FolderMeta),
    /// A batch of entries (§7.4), with its `seq_low` and `seq_high`.
    Batch(Batch),
    /// The receiver's decision on a batch, which doubles as its
    /// acknowledgement (§7.4).
    BatchDecision(BatchDecision),
    /// Send the file with content `hash`, from `offset` on (§7.5). The
    /// request names content, not a version: the source may serve it from
    /// `path` or from any live file with that hash.
    RequestFile {
        req: RequestId,
        folder: FolderId,
        path: RelPath,
        hash: ContentHash,
        offset: u64,
    },
    /// The next bytes of a requested file, at `offset`: 1 MiB at most
    /// (§12).
    FileData {
        req: RequestId,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// The requested file is complete.
    FileDone {
        req: RequestId,
    },
    /// The source cannot serve the request (§7.5).
    NotAvailable {
        req: RequestId,
    },
    /// The requester has moved on (a stall, §7.5): the source stops
    /// streaming, and data for a request it no longer knows is dropped.
    CancelRequest {
        req: RequestId,
    },
    /// Keepalive (§6.3).
    Ping,
    Pong,
}

/// `Hello` (§6.3, §12): who is at the other end, and what it speaks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Its protocol version, [`PROTOCOL`].
    pub proto: u32,
    pub node: NodeId,
    pub hostname: HostName,
    /// Its delocal version, for `status` to name the machine that needs
    /// updating (§13).
    pub version: String,
    /// Its clock when it sent this: for `status`'s clock skew warning
    /// (§13). Nanoseconds since the Unix epoch, §12's `now: i64`.
    pub now: Timestamp,
}

/// Why a machine said `Goodbye`. See the module docs: these variants never
/// change or move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    /// The other machine speaks another protocol version (§6.3).
    Protocol,
    /// The machines are connected twice, and this connection is the one
    /// the dial rule drops (§6.3).
    Duplicate,
}

/// `FolderMeta` (§12): a folder as the sender holds it (§9.1), and the
/// highest `seq` of the recipient's records it holds (§7.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderMeta {
    pub folder: FolderId,
    pub name: String,
    pub members: Vec<NodeId>,
    pub rules: Rules,
    /// The version of the folder's metadata, which is synced separately
    /// from its entries (§9.1).
    pub meta_version: u64,
    pub have_up_to: u64,
}

/// Names one `RequestFile` among the requester's open requests to one
/// source (§12, draft 34). The requester chooses it; the source's
/// `FileData`, `FileDone` and `NotAvailable`, and the requester's
/// `CancelRequest`, carry it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

impl Message {
    /// The message that carries `payload`, what the engine asked to send
    /// (§12). `HaveUpTo` travels as `FolderMeta`: `meta` gives the folder
    /// as this machine holds it, and the engine's `seq` replaces its
    /// `have_up_to`. A folder `meta` does not know sends nothing.
    pub fn envelope(
        payload: Outbound,
        meta: impl FnOnce(FolderId) -> Option<FolderMeta>,
    ) -> Option<Self> {
        Some(match payload {
            Outbound::Batch(batch) => Self::Batch(batch),
            Outbound::Decision(decision) => Self::BatchDecision(decision),
            Outbound::HaveUpTo { folder, seq } => Self::FolderMeta(FolderMeta {
                folder,
                have_up_to: seq,
                ..meta(folder)?
            }),
        })
    }

    /// The engine's event for this message from the peer `from`, if it is
    /// one of the engine's: the other side of [`Message::envelope`].
    pub fn event(self, from: NodeId) -> Option<Event> {
        match self {
            Self::Batch(batch) => Some(Event::BatchReceived { from, batch }),
            Self::BatchDecision(decision) => Some(Event::DecisionReceived { from, decision }),
            Self::FolderMeta(meta) => Some(Event::HaveUpToReceived {
                from,
                folder: meta.folder,
                seq: meta.have_up_to,
            }),
            Self::Hello(_)
            | Self::Goodbye { .. }
            | Self::RequestFile { .. }
            | Self::FileData { .. }
            | Self::FileDone { .. }
            | Self::NotAvailable { .. }
            | Self::CancelRequest { .. }
            | Self::Ping
            | Self::Pong => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use delocal_engine::Decision;
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;
    use crate::store::{mirror, sample};

    /// Every message, each field over everything its type allows.
    pub(crate) fn message() -> BoxedStrategy<Message> {
        let req = || any::<u64>().prop_map(RequestId);
        let decision = prop_oneof![
            Just(Decision::Accepted),
            any::<String>().prop_map(|reason| Decision::Held { reason }),
        ];
        prop_oneof![
            hello().prop_map(Message::Hello),
            prop_oneof![Just(Reason::Protocol), Just(Reason::Duplicate)]
                .prop_map(|reason| Message::Goodbye { reason }),
            (
                mirror::folder(),
                any::<String>(),
                vec(mirror::node(), 0..4),
                mirror::rules(),
                any::<u64>(),
                any::<u64>(),
            )
                .prop_map(
                    |(folder, name, members, rules, meta_version, have_up_to)| {
                        Message::FolderMeta(FolderMeta {
                            folder,
                            name,
                            members,
                            rules,
                            meta_version,
                            have_up_to,
                        })
                    }
                ),
            mirror::folder()
                .prop_flat_map(mirror::batch)
                .prop_map(Message::Batch),
            (mirror::batch_id(), mirror::folder(), decision, any::<u64>()).prop_map(
                |(batch, folder, decision, seq_high)| Message::BatchDecision(BatchDecision {
                    batch,
                    folder,
                    decision,
                    seq_high,
                })
            ),
            (
                req(),
                mirror::folder(),
                mirror::path(),
                mirror::hash(),
                any::<u64>()
            )
                .prop_map(|(req, folder, path, hash, offset)| Message::RequestFile {
                    req,
                    folder,
                    path,
                    hash,
                    offset,
                }),
            (req(), any::<u64>(), vec(any::<u8>(), 0..64))
                .prop_map(|(req, offset, bytes)| Message::FileData { req, offset, bytes }),
            req().prop_map(|req| Message::FileDone { req }),
            req().prop_map(|req| Message::NotAvailable { req }),
            req().prop_map(|req| Message::CancelRequest { req }),
            Just(Message::Ping),
            Just(Message::Pong),
        ]
        .boxed()
    }

    fn hello() -> impl Strategy<Value = Hello> {
        (
            any::<u32>(),
            mirror::node(),
            mirror::host_name(),
            any::<String>(),
            mirror::at(),
        )
            .prop_map(|(proto, node, hostname, version, now)| Hello {
                proto,
                node,
                hostname,
                version,
                now,
            })
    }

    proptest! {
        /// Every §12 message comes back from postcard as it went in.
        #[test]
        fn every_message_round_trips(message in message()) {
            let bytes = postcard::to_stdvec(&message).unwrap();
            prop_assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), message);
        }
    }

    /// The test above draws messages at random; this names each variant
    /// once, so a variant the strategy misses fails here instead of going
    /// untested. The match has no wildcard: a new variant does not compile
    /// until it is listed.
    #[test]
    fn the_strategy_draws_every_variant() {
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        let mut seen = [false; 12];
        for _ in 0..2000 {
            let message = message().new_tree(&mut runner).unwrap().current();
            let index = match message {
                Message::Hello(_) => 0,
                Message::Goodbye { .. } => 1,
                Message::FolderMeta(_) => 2,
                Message::Batch(_) => 3,
                Message::BatchDecision(_) => 4,
                Message::RequestFile { .. } => 5,
                Message::FileData { .. } => 6,
                Message::FileDone { .. } => 7,
                Message::NotAvailable { .. } => 8,
                Message::CancelRequest { .. } => 9,
                Message::Ping => 10,
                Message::Pong => 11,
            };
            seen[index] = true;
        }
        assert_eq!(seen, [true; 12]);
    }

    /// `Hello` and `Goodbye` as every protocol version must read them (see
    /// the module docs): a change here strands every machine on another
    /// version without even a `Goodbye`.
    #[test]
    fn hello_and_goodbye_keep_their_bytes() {
        let hello = Message::Hello(Hello {
            proto: 1,
            node: sample::node(0xab),
            hostname: HostName::new("pi").unwrap(),
            version: "0.1.0".to_owned(),
            now: sample::at(-2),
        });
        let mut want = vec![0, 1, 16];
        want.extend([0xab; 16]);
        want.extend([2, b'p', b'i', 5, b'0', b'.', b'1', b'.', b'0', 3]);
        assert_eq!(postcard::to_stdvec(&hello).unwrap(), want);

        let goodbye = |reason| postcard::to_stdvec(&Message::Goodbye { reason }).unwrap();
        assert_eq!(goodbye(Reason::Protocol), [1, 0]);
        assert_eq!(goodbye(Reason::Duplicate), [1, 1]);
    }

    /// Each engine message travels as its §12 message and comes back as
    /// the engine's event from the sender; `HaveUpTo` takes the rest of
    /// `FolderMeta` from the host, and a folder the host does not know
    /// sends nothing.
    #[test]
    fn the_engines_messages_travel_in_their_envelopes() {
        let peer = sample::node(7);
        let folder = sample::folder(1);
        let held = FolderMeta {
            folder,
            name: "Sync".to_owned(),
            members: vec![sample::node(1), peer],
            rules: Rules::default(),
            meta_version: 3,
            have_up_to: 0,
        };
        let meta = |id| (id == folder).then(|| held.clone());

        let batch = sample::batch(folder, 2);
        let message = Message::envelope(Outbound::Batch(batch.clone()), meta).unwrap();
        assert_eq!(message, Message::Batch(batch.clone()));
        assert_eq!(
            message.event(peer),
            Some(Event::BatchReceived { from: peer, batch })
        );

        let decision = BatchDecision {
            batch: sample::batch_id(2),
            folder,
            decision: Decision::Held {
                reason: "80% deleted".to_owned(),
            },
            seq_high: 9,
        };
        let message = Message::envelope(Outbound::Decision(decision.clone()), meta).unwrap();
        assert_eq!(message, Message::BatchDecision(decision.clone()));
        assert_eq!(
            message.event(peer),
            Some(Event::DecisionReceived {
                from: peer,
                decision
            })
        );

        let message = Message::envelope(Outbound::HaveUpTo { folder, seq: 41 }, meta).unwrap();
        assert_eq!(
            message,
            Message::FolderMeta(FolderMeta {
                have_up_to: 41,
                ..held.clone()
            })
        );
        assert_eq!(
            message.event(peer),
            Some(Event::HaveUpToReceived {
                from: peer,
                folder,
                seq: 41
            })
        );

        let unknown = Outbound::HaveUpTo {
            folder: sample::folder(2),
            seq: 41,
        };
        assert_eq!(Message::envelope(unknown, meta), None);
    }

    /// The host's own messages are not the engine's.
    #[test]
    fn the_hosts_messages_are_no_events() {
        let req = RequestId(5);
        let host = [
            Message::Hello(Hello {
                proto: PROTOCOL,
                node: sample::node(7),
                hostname: HostName::new("pi").unwrap(),
                version: "0.1.0".to_owned(),
                now: sample::at(1),
            }),
            Message::Goodbye {
                reason: Reason::Duplicate,
            },
            Message::RequestFile {
                req,
                folder: sample::folder(1),
                path: sample::path("a"),
                hash: sample::hash(1),
                offset: 0,
            },
            Message::FileData {
                req,
                offset: 0,
                bytes: vec![1, 2],
            },
            Message::FileDone { req },
            Message::NotAvailable { req },
            Message::CancelRequest { req },
            Message::Ping,
            Message::Pong,
        ];
        for message in host {
            assert_eq!(message.clone().event(sample::node(7)), None, "{message:?}");
        }
    }
}
