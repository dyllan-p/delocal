//! Fixed values of every type the store writes, for its unit tests
//! (DESIGN.md §11). Each sample fills every field with something other
//! than its default, so a column or JSON field that did not round-trip
//! shows up as a difference.

use std::collections::BTreeMap;
use std::path::PathBuf;

use delocal_engine::{
    ApplyMode, Batch, BatchId, ConflictCopy, ContentHash, Deferred, DeferredReason, Entry,
    FolderId, HeldItem, HeldRow, HoldReason, HostName, Index, IndexRecord, Kind, NodeId, Observed,
    Paused, Pending, Quarantined, Queued, RelPath, Rest, Rules, Summary, Timestamp, UserDecision,
    Version, WaitReason, Want, WantState,
};

use super::host::{DiskName, FolderRow, JournalRow, Machine, Member, Mode, Shim, TrashRow};

pub fn node(n: u8) -> NodeId {
    NodeId::from_bytes([n; 16])
}

pub fn folder(n: u8) -> FolderId {
    FolderId::from_bytes([n; 16])
}

pub fn batch_id(n: u8) -> BatchId {
    BatchId::from_bytes([n; 16])
}

pub fn hash(n: u8) -> ContentHash {
    ContentHash::from_bytes([n; 32])
}

pub fn path(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

pub fn at(nanos: i64) -> Timestamp {
    Timestamp::from_unix_nanos(nanos)
}

pub fn entry(p: &str, n: u8) -> Entry {
    Entry {
        path: path(p),
        kind: Kind::Symlink,
        size: u64::MAX - u64::from(n),
        mtime_ns: -1_000_000_007,
        stamp: 1_790_000_000_000_000_000 + i64::from(n),
        exec: true,
        hash: hash(n),
        prev_hash: hash(n.wrapping_add(1)),
        version: [(node(1), u64::from(n) + 1), (node(9), u64::MAX)]
            .into_iter()
            .collect(),
        deleted: true,
        modified_by: node(9),
        author_host: HostName::new("laptop-2").unwrap(),
    }
}

pub fn record(p: &str, seq: u64) -> IndexRecord {
    IndexRecord {
        entry: entry(p, 3),
        seq,
    }
}

pub fn want(p: &str) -> Want {
    Want {
        entry: entry(p, 4),
        received: entry(p, 5),
        mode: ApplyMode::MetadataOnly,
        conflict: Some(ConflictCopy {
            path: path("a.conflict-20260927-101010-laptop-2.txt"),
            loser: entry(p, 6),
        }),
        batch: batch_id(7),
        source: node(2),
        seq_high: 41,
        sources: [node(2), node(3)].into_iter().collect(),
        excluded: [(node(3), at(99))].into_iter().collect(),
        strikes: [(node(3), 2)].into_iter().collect(),
        mismatches: 1,
        fetched: true,
        restoring: true,
        answered: [node(4)].into_iter().collect(),
        reset: Some(Observed {
            kind: Kind::File,
            size: 12,
            mtime_ns: 13,
            exec: true,
            hash: hash(14),
        }),
        state: WantState::Committing {
            deadline: at(15),
            overdue: true,
        },
    }
}

pub fn pending(announced: bool) -> Pending {
    Pending {
        announced_record: announced.then(|| record("a/b", 17)),
        exempt: true,
    }
}

pub fn held_row(batch: u8) -> HeldRow {
    let mut entries = BTreeMap::new();
    entries.insert(path("x"), entry("x", 20));
    let versions = [(
        path("x"),
        vec![
            Quarantined {
                version: entry("x", 21).version,
                stamp: 22,
                arrival: 23,
            },
            Quarantined {
                version: Version::empty(),
                stamp: -1,
                arrival: u64::MAX,
            },
        ],
    )]
    .into_iter()
    .collect();
    HeldRow {
        item: HeldItem {
            batch: batch_id(batch),
            source: node(5),
            seq_high: 24,
            held_at: at(25),
            reason: HoldReason::Count {
                destructive: 26,
                tracked: 27,
            },
            entries,
        },
        versions,
    }
}

pub fn deferred(p: &str) -> Vec<Deferred> {
    vec![
        Deferred {
            entry: entry(p, 30),
            batch: batch_id(31),
            source: node(6),
            seq_high: 32,
            reason: DeferredReason::Frozen,
            restoring: Some(entry(p, 33)),
        },
        Deferred {
            entry: entry(p, 34),
            batch: batch_id(35),
            source: node(7),
            seq_high: 36,
            reason: DeferredReason::ChangedUnderneath,
            restoring: None,
        },
    ]
}

pub fn rest() -> Rest {
    // Watermarks have private fields; an index builds one from ranges, as
    // the engine does. Peer 2 is contiguous to 10; peer 3 has a gap below
    // (20, 30].
    let mut index = Index::new(node(1), HostName::new("laptop-2").unwrap());
    index.received_range(node(2), 0, 10);
    index.received_range(node(3), 20, 30);
    Rest {
        seq: 40,
        announced_seq: 39,
        announced_tracked: 38,
        watermarks: index.watermarks().clone(),
        acked: [(node(2), 37)].into_iter().collect(),
        paused: Some(Paused {
            batch: batch_id(41),
            since: at(42),
            reason: HoldReason::Size { bytes: u64::MAX },
            summary: Summary {
                adds: 43,
                mods: 44,
                dels: 45,
                bytes: 46,
            },
            would_pass: true,
        }),
        queued: vec![
            Queued {
                decision: UserDecision::Deny {
                    batch: batch_id(47),
                },
                reason: WaitReason::Restoring { path: path("r") },
            },
            Queued {
                decision: UserDecision::Revert {
                    batch: batch_id(48),
                },
                reason: WaitReason::StartupScan,
            },
        ],
        arrivals: 49,
        winner_fallbacks: 50,
    }
}

pub fn batch(f: FolderId, n: u8) -> Batch {
    Batch {
        id: batch_id(n),
        folder: f,
        source: node(8),
        created_at: at(-5),
        seq_low: 51,
        seq_high: u64::MAX,
        entries: vec![entry("z", 52), entry("a", 53), entry("m/n", 54)],
        summary: Summary {
            adds: 1,
            mods: 2,
            dels: 3,
            bytes: u64::MAX,
        },
    }
}

pub fn folder_row(f: FolderId) -> FolderRow {
    FolderRow {
        id: f,
        name: "Sync".to_owned(),
        created_by: node(1),
        rules: Rules {
            hold_count: 0,
            max_fetches_per_peer: 1,
            ..Rules::default()
        },
        meta_version: u64::MAX,
    }
}

pub fn member(n: u8) -> Member {
    Member {
        node: node(n),
        path: PathBuf::from(format!("/home/me/Sync-{n}")),
        mode: Mode::TwoWay,
        joined_at: at(60 + i64::from(n)),
    }
}

pub fn trash_row(name: &str) -> TrashRow {
    TrashRow {
        trashed_path: PathBuf::from(format!("2026-09-27/{name}")),
        original_path: path(name),
        hash: hash(70),
        trashed_at: at(71),
        size: u64::MAX,
    }
}

pub fn shim(p: &str) -> Shim {
    Shim {
        path: path(p),
        requested_ns: 1_790_000_000_123_456_789,
        stored_ns: 1_790_000_000_000_000_000,
    }
}

pub fn disk_name(p: &str) -> DiskName {
    DiskName {
        path: path(p),
        // "é" decomposed: what macOS or a Linux user may have on disk.
        name: "e\u{301}.txt".into(),
    }
}

pub fn journal_row(p: &str, temp: bool) -> JournalRow {
    JournalRow {
        path: path(p),
        displaced_to: PathBuf::from(format!(".delocal/trash/2026-09-27/{p}")),
        temp_file: temp.then(|| PathBuf::from(".delocal/tmp/abcd1234-5678")),
    }
}

pub fn machine(n: u8) -> Machine {
    Machine {
        node: node(n),
        hostname: format!("host-{n}"),
        ts_stable_id: format!("nStable{n}CNTRL"),
        ts_user: "me@example.com".to_owned(),
        trusted: true,
        last_seen: at(80),
        delocal_version: "0.1.0".to_owned(),
    }
}
