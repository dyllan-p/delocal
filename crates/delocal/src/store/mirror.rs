//! A model of the store for its tests (DESIGN.md §11): random streams of
//! writes, and the tables they should leave behind, kept in maps.
//!
//! [`Op`] is one write: an engine action that is a persistence hook or
//! `RecordBatch`, or a host write. [`stream`] generates groups of them
//! whose keys come from small pools, so rows are replaced and removed as
//! often as they are made, and whose values range over everything the
//! types allow. [`Mirror`] applies the same ops to plain maps the way §11
//! describes (a hook's row replaces the one with its key, `None` deletes
//! it, history appends) and says what each of the store's readers should
//! return.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use delocal_engine::{
    Action, ApplyMode, Batch, BatchId, BatchRole, ConflictCopy, ContentHash, Decision, Deferred,
    DeferredReason, Entry, FolderId, FolderParts, HeldItem, HeldRow, HeldState, HoldReason,
    HostName, Index, IndexRecord, Kind, NodeId, Observed, Paused, Pending, Quarantined, Queued,
    RelPath, Rest, Rules, Summary, Tier, Timestamp, UserDecision, Version, WaitReason, Want,
    WantState,
};
use proptest::collection::{btree_map, btree_set, vec};
use proptest::option;
use proptest::prelude::*;
use proptest::sample::select;

use super::host::{
    DiskName, FolderRow, HistoryRow, JournalRow, Machine, Member, Mode, Shim, TrashRow,
};
use super::{Group, HostWrite, Store, StoreError};

/// One write in a stream.
#[derive(Clone, Debug)]
pub enum Op {
    Hook(Action),
    Host(HostWrite),
}

/// One event's writes, handled at `at`.
#[derive(Clone, Debug)]
pub struct Ops {
    pub at: Timestamp,
    pub ops: Vec<Op>,
}

impl Ops {
    /// The same writes as a [`Group`] for the store.
    pub fn group(&self) -> Group {
        let mut group = Group::new(self.at);
        for op in &self.ops {
            match op {
                Op::Hook(action) => {
                    // Every op the strategies make writes something.
                    assert!(
                        group.push(action.clone()).is_none(),
                        "{action:?} is an effect"
                    );
                }
                Op::Host(write) => group.host(write.clone()),
            }
        }
        group
    }
}

// Pools: keys come from these, so a stream revisits them.

/// The folders every stream records first.
pub const FOLDERS: [u8; 2] = [1, 2];

fn folder() -> BoxedStrategy<FolderId> {
    select(FOLDERS.to_vec())
        .prop_map(|n| FolderId::from_bytes([n; 16]))
        .boxed()
}

fn pooled_node() -> BoxedStrategy<NodeId> {
    (1u8..=4).prop_map(|n| NodeId::from_bytes([n; 16])).boxed()
}

/// A node from the pool, mostly, or any node at all.
fn node() -> BoxedStrategy<NodeId> {
    prop_oneof![
        3 => pooled_node(),
        1 => any::<[u8; 16]>().prop_map(NodeId::from_bytes),
    ]
    .boxed()
}

fn batch_id() -> BoxedStrategy<BatchId> {
    prop_oneof![
        3 => (1u8..=3).prop_map(|n| BatchId::from_bytes([n; 16])),
        1 => any::<[u8; 16]>().prop_map(BatchId::from_bytes),
    ]
    .boxed()
}

fn hash() -> BoxedStrategy<ContentHash> {
    prop_oneof![
        1 => Just(ContentHash::EMPTY),
        3 => any::<[u8; 32]>().prop_map(ContentHash::from_bytes),
    ]
    .boxed()
}

/// Index paths: plain, nested, NFC and NFD forms of one name, a space, a
/// component of 255 bytes.
fn path() -> BoxedStrategy<RelPath> {
    let long = "l".repeat(255);
    select(vec![
        "a".to_owned(),
        "a/b".to_owned(),
        "a/b/c.txt".to_owned(),
        "\u{e9}".to_owned(),
        "e\u{301}".to_owned(),
        "日本/語.md".to_owned(),
        "with space".to_owned(),
        format!("d/{long}"),
    ])
    .prop_map(|p| RelPath::new(p).unwrap())
    .boxed()
}

fn host_name() -> BoxedStrategy<HostName> {
    select(vec![
        "",
        "laptop",
        "pi-4",
        "a0-b1-c2-d3-e4-f5-g6-h7-i8-j9-k0-l1-m2-n3-o4-p5-q6-r7-s8-t9-uv",
    ])
    .prop_map(|h| HostName::new(h).unwrap())
    .boxed()
}

fn at() -> BoxedStrategy<Timestamp> {
    any::<i64>().prop_map(Timestamp::from_unix_nanos).boxed()
}

fn kind() -> BoxedStrategy<Kind> {
    select(vec![Kind::File, Kind::Dir, Kind::Symlink]).boxed()
}

/// Bytes of a place on disk: anything, NUL and invalid UTF-8 included.
fn disk_bytes() -> BoxedStrategy<Vec<u8>> {
    vec(any::<u8>(), 0..24).boxed()
}

fn version() -> BoxedStrategy<Version> {
    btree_map(node(), 1..=u64::MAX, 0..4)
        .prop_map(|m| m.into_iter().collect())
        .boxed()
}

fn entry() -> BoxedStrategy<Entry> {
    (
        (
            path(),
            kind(),
            any::<u64>(),
            any::<i64>(),
            any::<i64>(),
            any::<bool>(),
        ),
        (
            hash(),
            hash(),
            version(),
            any::<bool>(),
            node(),
            host_name(),
        ),
    )
        .prop_map(
            |(
                (path, kind, size, mtime_ns, stamp, exec),
                (hash, prev_hash, version, deleted, modified_by, author_host),
            )| Entry {
                path,
                kind,
                size,
                mtime_ns,
                stamp,
                exec,
                hash,
                prev_hash,
                version,
                deleted,
                modified_by,
                author_host,
            },
        )
        .boxed()
}

fn record() -> BoxedStrategy<IndexRecord> {
    (entry(), any::<u64>())
        .prop_map(|(entry, seq)| IndexRecord { entry, seq })
        .boxed()
}

fn observed() -> BoxedStrategy<Observed> {
    (kind(), any::<u64>(), any::<i64>(), any::<bool>(), hash())
        .prop_map(|(kind, size, mtime_ns, exec, hash)| Observed {
            kind,
            size,
            mtime_ns,
            exec,
            hash,
        })
        .boxed()
}

fn tier() -> BoxedStrategy<Tier> {
    select(vec![Tier::Lan, Tier::Direct, Tier::Relay]).boxed()
}

fn want_state() -> BoxedStrategy<WantState> {
    prop_oneof![
        Just(WantState::Wanted),
        Just(WantState::Blocked),
        tier().prop_map(|need| WantState::Deferred { need }),
        Just(WantState::NoSource),
        (node(), at()).prop_map(|(from, deadline)| WantState::Fetching { from, deadline }),
        (at(), any::<bool>())
            .prop_map(|(deadline, overdue)| WantState::Committing { deadline, overdue }),
        Just(WantState::GaveUp),
        at().prop_map(|until| WantState::LocalRetry { until }),
        Just(WantState::DiskFull),
        (path(), any::<u64>()).prop_map(|(with, seq)| WantState::Collides { with, seq }),
        path().prop_map(|path| WantState::WaitingFor { path }),
    ]
    .boxed()
}

fn want() -> BoxedStrategy<Want> {
    (
        (
            entry(),
            entry(),
            select(vec![
                ApplyMode::Fetch,
                ApplyMode::Direct,
                ApplyMode::MetadataOnly,
                ApplyMode::IndexOnly,
            ]),
            option::of((path(), entry()).prop_map(|(path, loser)| ConflictCopy { path, loser })),
            batch_id(),
            node(),
            any::<u64>(),
        ),
        (
            btree_set(node(), 0..3),
            btree_map(node(), at(), 0..3),
            btree_map(node(), at(), 0..3),
            btree_map(node(), any::<u32>(), 0..3),
            any::<u8>(),
            any::<u32>(),
            any::<bool>(),
            any::<bool>(),
            btree_set(node(), 0..3),
            option::of(observed()),
            want_state(),
        ),
    )
        .prop_map(
            |(
                (entry, received, mode, conflict, batch, source, seq_high),
                (
                    sources,
                    refused,
                    excluded,
                    strikes,
                    mismatches,
                    local_retries,
                    fetched,
                    restoring,
                    answered,
                    reset,
                    state,
                ),
            )| Want {
                entry,
                received,
                mode,
                conflict,
                batch,
                source,
                seq_high,
                sources,
                refused,
                excluded,
                strikes,
                mismatches,
                local_retries,
                fetched,
                restoring,
                answered,
                reset,
                state,
            },
        )
        .boxed()
}

fn pending() -> BoxedStrategy<Pending> {
    (option::of(record()), any::<bool>())
        .prop_map(|(announced_record, exempt)| Pending {
            announced_record,
            exempt,
        })
        .boxed()
}

fn hold_reason() -> BoxedStrategy<HoldReason> {
    prop_oneof![
        (any::<u64>(), any::<usize>()).prop_map(|(destructive, tracked)| HoldReason::Count {
            destructive,
            tracked
        }),
        any::<u64>().prop_map(|bytes| HoldReason::Size { bytes }),
    ]
    .boxed()
}

fn held_state() -> BoxedStrategy<HeldState> {
    prop_oneof![
        Just(HeldState::Held),
        prop_oneof![1u64..4, any::<u64>()].prop_map(|at| HeldState::Denied { at }),
    ]
    .boxed()
}

fn held_row() -> BoxedStrategy<HeldRow> {
    let quarantined =
        (version(), any::<i64>(), any::<u64>()).prop_map(|(version, stamp, arrival)| Quarantined {
            version,
            stamp,
            arrival,
        });
    (
        batch_id(),
        node(),
        any::<u64>(),
        at(),
        hold_reason(),
        btree_map(path(), entry(), 0..3),
        btree_map(path(), vec(quarantined, 0..3), 0..3),
    )
        .prop_map(
            |(batch, source, seq_high, held_at, reason, entries, versions)| HeldRow {
                item: HeldItem {
                    batch,
                    source,
                    seq_high,
                    held_at,
                    reason,
                    entries,
                },
                versions,
            },
        )
        .boxed()
}

fn deferred() -> BoxedStrategy<Vec<Deferred>> {
    let one = (
        entry(),
        batch_id(),
        node(),
        any::<u64>(),
        select(vec![
            DeferredReason::ChangedUnderneath,
            DeferredReason::Frozen,
        ]),
        option::of(entry()),
    )
        .prop_map(
            |(entry, batch, source, seq_high, reason, restoring)| Deferred {
                entry,
                batch,
                source,
                seq_high,
                reason,
                restoring,
            },
        );
    vec(one, 1..3).boxed()
}

fn summary() -> BoxedStrategy<Summary> {
    (any::<u64>(), any::<u64>(), any::<u64>(), any::<u64>())
        .prop_map(|(adds, mods, dels, bytes)| Summary {
            adds,
            mods,
            dels,
            bytes,
        })
        .boxed()
}

fn queued() -> BoxedStrategy<Queued> {
    let decision = prop_oneof![
        batch_id().prop_map(|batch| UserDecision::Deny { batch }),
        batch_id().prop_map(|batch| UserDecision::Revert { batch }),
    ];
    let reason = prop_oneof![
        batch_id().prop_map(|batch| WaitReason::Paused { batch }),
        path().prop_map(|path| WaitReason::Restoring { path }),
        path().prop_map(|path| WaitReason::Committing { path }),
        Just(WaitReason::StartupScan),
        path().prop_map(|path| WaitReason::Unobserved { path }),
        Just(WaitReason::ScanOpen),
    ];
    (decision, reason)
        .prop_map(|(decision, reason)| Queued { decision, reason })
        .boxed()
}

fn rest() -> BoxedStrategy<Rest> {
    let paused = (batch_id(), at(), hold_reason(), summary(), any::<bool>()).prop_map(
        |(batch, since, reason, summary, would_pass)| Paused {
            batch,
            since,
            reason,
            summary,
            would_pass,
        },
    );
    (
        (any::<u64>(), any::<u64>(), any::<usize>()),
        // Watermarks have private fields: build them from received ranges,
        // as the engine does, gaps included.
        vec((node(), any::<u64>(), any::<u64>()), 0..5),
        btree_map(node(), any::<u64>(), 0..3),
        option::of(paused),
        vec(queued(), 0..3),
        (any::<u64>(), any::<u64>(), any::<bool>()),
    )
        .prop_map(
            |((seq, announced_seq, announced_tracked), ranges, acked, paused, queued, counts)| {
                let mut index =
                    Index::new(NodeId::from_bytes([0; 16]), HostName::new("x").unwrap());
                for (peer, low, high) in ranges {
                    index.received_range(peer, low.min(high), low.max(high));
                }
                Rest {
                    seq,
                    announced_seq,
                    announced_tracked,
                    watermarks: index.watermarks().clone(),
                    acked,
                    paused,
                    queued,
                    arrivals: counts.0,
                    winner_fallbacks: counts.1,
                    disk_full: counts.2,
                }
            },
        )
        .boxed()
}

fn batch(folder: FolderId) -> BoxedStrategy<Batch> {
    (
        batch_id(),
        node(),
        at(),
        any::<u64>(),
        any::<u64>(),
        vec(entry(), 0..4),
        summary(),
    )
        .prop_map(
            move |(id, source, created_at, seq_low, seq_high, entries, summary)| Batch {
                id,
                folder,
                source,
                created_at,
                seq_low,
                seq_high,
                entries,
                summary,
            },
        )
        .boxed()
}

fn decision() -> BoxedStrategy<Option<Decision>> {
    prop_oneof![
        Just(None),
        Just(Some(Decision::Accepted)),
        any::<String>().prop_map(|reason| Some(Decision::Held { reason })),
    ]
    .boxed()
}

fn role() -> BoxedStrategy<BatchRole> {
    select(vec![
        BatchRole::Sent,
        BatchRole::Received,
        BatchRole::Paused,
    ])
    .boxed()
}

/// Any engine action that writes: every persistence hook, and history.
fn hook() -> BoxedStrategy<Action> {
    folder().prop_flat_map(|folder| {
        prop_oneof![
            4 => record().prop_map(move |record| Action::IndexChanged { folder, record }),
            1 => path().prop_map(move |path| Action::IndexRemoved { folder, path }),
            3 => (path(), option::of(want().prop_map(Box::new)))
                .prop_map(move |(path, want)| Action::WantChanged { folder, path, want }),
            2 => (path(), option::of(pending()))
                .prop_map(move |(path, row)| Action::PendingChanged { folder, path, row }),
            2 => (batch_id(), held_state(), option::of(held_row().prop_map(Box::new))).prop_map(
                move |(batch, state, row)| Action::HeldChanged { folder, batch, state, row }
            ),
            2 => (path(), option::of(deferred())).prop_map(move |(path, entries)| {
                Action::DeferredChanged { folder, path, entries }
            }),
            1 => rest().prop_map(move |rest| Action::RestChanged { folder, rest: Box::new(rest) }),
            1 => (batch(folder), role(), decision())
                .prop_map(|(batch, role, decision)| Action::RecordBatch { batch, role, decision }),
        ]
    })
    .boxed()
}

fn rules() -> BoxedStrategy<Rules> {
    (
        any::<u64>(),
        any::<u8>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(
            |(
                hold_count,
                hold_pct,
                hold_size,
                direct_limit,
                relay_limit,
                max_fetches_per_peer,
                max_fetches_per_folder,
            )| Rules {
                hold_count,
                hold_pct,
                hold_size,
                direct_limit,
                relay_limit,
                max_fetches_per_peer,
                max_fetches_per_folder,
            },
        )
        .boxed()
}

fn folder_row(id: FolderId) -> BoxedStrategy<FolderRow> {
    (any::<String>(), node(), rules(), any::<u64>())
        .prop_map(move |(name, created_by, rules, meta_version)| FolderRow {
            id,
            name,
            created_by,
            rules,
            meta_version,
        })
        .boxed()
}

/// Trash locations are keys: mostly a few, so that a remove finds a row
/// an earlier put made (with half of them random, a stream seldom did).
fn trashed_path() -> BoxedStrategy<PathBuf> {
    prop_oneof![
        4 => select(vec![
            b"2026-09-27/a".to_vec(),
            b"2026-09-27/a~1".to_vec(),
            b"x/\xff".to_vec()
        ]),
        1 => disk_bytes(),
    ]
    .prop_map(|b| PathBuf::from(OsString::from_vec(b)))
    .boxed()
}

/// Any write to a table the host owns.
fn host_write() -> BoxedStrategy<HostWrite> {
    let path_buf = || disk_bytes().prop_map(|b| PathBuf::from(OsString::from_vec(b)));
    folder().prop_flat_map(move |folder| {
        prop_oneof![
            1 => folder_row(folder).prop_map(HostWrite::PutFolder),
            2 => (pooled_node(), path_buf(), at()).prop_map(move |(node, path, joined_at)| {
                HostWrite::PutMember {
                    folder,
                    member: Member { node, path, mode: Mode::TwoWay, joined_at },
                }
            }),
            1 => pooled_node().prop_map(move |node| HostWrite::RemoveMember { folder, node }),
            2 => (trashed_path(), path(), hash(), at(), any::<u64>()).prop_map(
                move |(trashed_path, original_path, hash, trashed_at, size)| HostWrite::PutTrash {
                    folder,
                    row: TrashRow { trashed_path, original_path, hash, trashed_at, size },
                }
            ),
            1 => trashed_path()
                .prop_map(move |trashed_path| HostWrite::RemoveTrash { folder, trashed_path }),
            2 => (path(), any::<i64>(), any::<i64>()).prop_map(
                move |(path, requested_ns, stored_ns)| HostWrite::PutShim {
                    folder,
                    shim: Shim { path, requested_ns, stored_ns },
                }
            ),
            1 => path().prop_map(move |path| HostWrite::RemoveShim { folder, path }),
            2 => (path(), disk_bytes()).prop_map(move |(path, name)| HostWrite::PutDiskName {
                folder,
                name: DiskName { path, name: OsString::from_vec(name) },
            }),
            1 => path().prop_map(move |path| HostWrite::RemoveDiskName { folder, path }),
            2 => (path(), path_buf(), option::of(path_buf())).prop_map(
                move |(path, displaced_to, temp_file)| HostWrite::PutJournal {
                    folder,
                    row: JournalRow { path, displaced_to, temp_file },
                }
            ),
            1 => path().prop_map(move |path| HostWrite::RemoveJournal { folder, path }),
            2 => machine().prop_map(HostWrite::PutMachine),
            1 => pooled_node().prop_map(HostWrite::RemoveMachine),
        ]
    })
    .boxed()
}

fn machine() -> BoxedStrategy<Machine> {
    (
        pooled_node(),
        any::<String>(),
        any::<String>(),
        any::<String>(),
        any::<bool>(),
        at(),
        any::<String>(),
    )
        .prop_map(
            |(node, hostname, ts_stable_id, ts_user, trusted, last_seen, delocal_version)| {
                Machine {
                    node,
                    hostname,
                    ts_stable_id,
                    ts_user,
                    trusted,
                    last_seen,
                    delocal_version,
                }
            },
        )
        .boxed()
}

/// One event's writes: hooks and host writes mixed, possibly none.
pub fn ops() -> BoxedStrategy<Ops> {
    let op = prop_oneof![
        3 => hook().prop_map(Op::Hook),
        2 => host_write().prop_map(Op::Host),
    ];
    (at(), vec(op, 0..8))
        .prop_map(|(at, ops)| Ops { at, ops })
        .boxed()
}

/// The group every stream starts with: both folders recorded, so every
/// later row has its folder.
pub fn setup() -> BoxedStrategy<Ops> {
    (at(), folder_row(folder_id(1)), folder_row(folder_id(2)))
        .prop_map(|(at, one, two)| Ops {
            at,
            ops: vec![
                Op::Host(HostWrite::PutFolder(one)),
                Op::Host(HostWrite::PutFolder(two)),
            ],
        })
        .boxed()
}

/// A stream: the setup group, then up to `len` more.
pub fn stream(len: usize) -> BoxedStrategy<Vec<Ops>> {
    (setup(), vec(ops(), 0..=len))
        .prop_map(|(first, rest)| {
            let mut all = vec![first];
            all.extend(rest);
            all
        })
        .boxed()
}

pub fn folder_id(n: u8) -> FolderId {
    FolderId::from_bytes([n; 16])
}

/// The tables as maps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mirror {
    folders: BTreeMap<FolderId, Tables>,
    machines: BTreeMap<NodeId, Machine>,
}

/// One folder's rows.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tables {
    row: FolderRow,
    members: BTreeMap<NodeId, Member>,
    records: BTreeMap<RelPath, IndexRecord>,
    wants: BTreeMap<RelPath, Want>,
    pending: BTreeMap<RelPath, Pending>,
    held: BTreeMap<(BatchId, HeldState), HeldRow>,
    deferred: BTreeMap<RelPath, Vec<Deferred>>,
    rest: Option<Rest>,
    history: Vec<HistoryRow>,
    /// By the bytes of the trash location, as SQLite orders a BLOB.
    trash: BTreeMap<Vec<u8>, TrashRow>,
    shims: BTreeMap<RelPath, Shim>,
    disk_names: BTreeMap<RelPath, DiskName>,
    journal: BTreeMap<RelPath, JournalRow>,
}

impl Tables {
    fn new(row: FolderRow) -> Self {
        Self {
            row,
            members: BTreeMap::new(),
            records: BTreeMap::new(),
            wants: BTreeMap::new(),
            pending: BTreeMap::new(),
            held: BTreeMap::new(),
            deferred: BTreeMap::new(),
            rest: None,
            history: Vec::new(),
            trash: BTreeMap::new(),
            shims: BTreeMap::new(),
            disk_names: BTreeMap::new(),
            journal: BTreeMap::new(),
        }
    }
}

/// Put `value` at `key`, or remove the key for `None`.
fn set<K: Ord, V>(map: &mut BTreeMap<K, V>, key: K, value: Option<V>) {
    match value {
        Some(value) => map.insert(key, value),
        None => map.remove(&key),
    };
}

fn bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

impl Mirror {
    /// Apply one event's writes, in order.
    pub fn apply(&mut self, ops: &Ops) {
        for op in &ops.ops {
            match op {
                Op::Hook(action) => self.hook(ops.at, action),
                Op::Host(write) => self.host(write),
            }
        }
    }

    fn tables(&mut self, folder: FolderId) -> &mut Tables {
        self.folders
            .get_mut(&folder)
            .unwrap_or_else(|| panic!("a write for {folder:?} before its folder"))
    }

    fn hook(&mut self, at: Timestamp, action: &Action) {
        match action.clone() {
            Action::IndexChanged { folder, record } => {
                let path = record.entry.path.clone();
                self.tables(folder).records.insert(path, record);
            }
            Action::IndexRemoved { folder, path } => {
                self.tables(folder).records.remove(&path);
            }
            Action::WantChanged { folder, path, want } => {
                set(&mut self.tables(folder).wants, path, want.map(|w| *w));
            }
            Action::PendingChanged { folder, path, row } => {
                set(&mut self.tables(folder).pending, path, row);
            }
            Action::HeldChanged {
                folder,
                batch,
                state,
                row,
            } => set(
                &mut self.tables(folder).held,
                (batch, state),
                row.map(|r| *r),
            ),
            Action::DeferredChanged {
                folder,
                path,
                entries,
            } => set(&mut self.tables(folder).deferred, path, entries),
            Action::RestChanged { folder, rest } => self.tables(folder).rest = Some(*rest),
            Action::RecordBatch {
                batch,
                role,
                decision,
            } => self.tables(batch.folder).history.push(HistoryRow {
                batch,
                role,
                decision,
                recorded_at: at,
            }),
            other => panic!("{other:?} writes nothing"),
        }
    }

    fn host(&mut self, write: &HostWrite) {
        match write.clone() {
            HostWrite::PutFolder(row) => match self.folders.get_mut(&row.id) {
                Some(tables) => tables.row = row,
                None => {
                    self.folders.insert(row.id, Tables::new(row));
                }
            },
            HostWrite::PutMember { folder, member } => {
                self.tables(folder).members.insert(member.node, member);
            }
            HostWrite::RemoveMember { folder, node } => {
                self.tables(folder).members.remove(&node);
            }
            HostWrite::PutTrash { folder, row } => {
                self.tables(folder)
                    .trash
                    .insert(bytes(&row.trashed_path), row);
            }
            HostWrite::RemoveTrash {
                folder,
                trashed_path,
            } => {
                self.tables(folder).trash.remove(&bytes(&trashed_path));
            }
            HostWrite::PutShim { folder, shim } => {
                self.tables(folder).shims.insert(shim.path.clone(), shim);
            }
            HostWrite::RemoveShim { folder, path } => {
                self.tables(folder).shims.remove(&path);
            }
            HostWrite::PutDiskName { folder, name } => {
                self.tables(folder)
                    .disk_names
                    .insert(name.path.clone(), name);
            }
            HostWrite::RemoveDiskName { folder, path } => {
                self.tables(folder).disk_names.remove(&path);
            }
            HostWrite::PutJournal { folder, row } => {
                self.tables(folder).journal.insert(row.path.clone(), row);
            }
            HostWrite::RemoveJournal { folder, path } => {
                self.tables(folder).journal.remove(&path);
            }
            HostWrite::PutMachine(machine) => {
                self.machines.insert(machine.node, machine);
            }
            HostWrite::RemoveMachine(node) => {
                self.machines.remove(&node);
            }
        }
    }

    /// The parts of `folder` as §11 has them: `None` until the engine
    /// reported its small rest.
    pub fn parts(&self, folder: FolderId) -> Option<FolderParts> {
        let t = self.folders.get(&folder)?;
        Some(FolderParts {
            id: folder,
            rules: t.row.rules.clone(),
            members: t.members.keys().copied().collect(),
            records: t.records.clone(),
            wants: t.wants.clone(),
            pending: t.pending.clone(),
            held: t.held.clone(),
            deferred: t.deferred.clone(),
            rest: t.rest.clone()?,
        })
    }

    /// The folders, in id order.
    pub fn folder_ids(&self) -> Vec<FolderId> {
        self.folders.keys().copied().collect()
    }

    /// The first reader of `store` whose answer is not this mirror's, if
    /// any, with both answers.
    pub fn difference(&self, store: &Store) -> Option<String> {
        let rows: Vec<FolderRow> = self.folders.values().map(|t| t.row.clone()).collect();
        let loaded: Vec<FolderParts> = self.folders.keys().filter_map(|f| self.parts(*f)).collect();
        let mut found = compare("machines", store.machines(), values(&self.machines))
            .or_else(|| compare("folders", store.folders(), rows))
            .or_else(|| compare("load", store.load(), loaded));
        for (&f, t) in &self.folders {
            found = found
                .or_else(|| compare("members", store.members(f), values(&t.members)))
                .or_else(|| compare("history", store.history(f), t.history.clone()))
                .or_else(|| compare("trash", store.trash(f), values(&t.trash)))
                .or_else(|| compare("shims", store.shims(f), values(&t.shims)))
                .or_else(|| compare("disk names", store.disk_names(f), values(&t.disk_names)))
                .or_else(|| compare("journal", store.journal(f), values(&t.journal)));
        }
        found
    }
}

/// A map's values, in key order.
fn values<K, V: Clone>(map: &BTreeMap<K, V>) -> Vec<V> {
    map.values().cloned().collect()
}

/// `None` if a reader returned what the mirror holds; otherwise both.
fn compare<T: PartialEq + Debug>(
    what: &str,
    stored: Result<T, StoreError>,
    mirror: T,
) -> Option<String> {
    let stored = stored.unwrap_or_else(|e| panic!("reading {what}: {e}"));
    (stored != mirror).then(|| format!("{what}:\n  stored {stored:?}\n  mirror {mirror:?}"))
}

/// A stream of groups from a seed, the same in every process: the crash
/// test's child writes it and the parent rebuilds it. Every group is
/// bracketed by two marker rows naming its number, one written first and
/// one last, so a store holding part of a group matches no prefix.
pub struct Seeded {
    runner: proptest::test_runner::TestRunner,
    next: u64,
}

/// The machines the brackets write; the strategies never make these.
pub const FIRST: NodeId = NodeId::from_bytes([0xaa; 16]);
pub const LAST: NodeId = NodeId::from_bytes([0xbb; 16]);

impl Seeded {
    pub fn new(seed: u64) -> Self {
        use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&seed.to_le_bytes());
        let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &key);
        Self {
            runner: TestRunner::new_with_rng(Config::default(), rng),
            next: 0,
        }
    }

    /// Group number `self.next`: the setup group first, then random ones.
    pub fn group(&mut self) -> Ops {
        let n = self.next;
        self.next += 1;
        let strategy = if n == 0 { setup() } else { ops() };
        let mut group = strategy.new_tree(&mut self.runner).unwrap().current();
        let marker = |node| {
            Op::Host(HostWrite::PutMachine(Machine {
                node,
                hostname: format!("group {n}"),
                ts_stable_id: String::new(),
                ts_user: String::new(),
                trusted: false,
                last_seen: Timestamp::from_unix_nanos(n.cast_signed()),
                delocal_version: String::new(),
            }))
        };
        group.ops.insert(0, marker(FIRST));
        group.ops.push(marker(LAST));
        group
    }
}
