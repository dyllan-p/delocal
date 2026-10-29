//! The mtime precision shim in the commit path and the scan (DESIGN.md
//! §7.3), on a filesystem that keeps mtimes to 2 s ([`Coarse`]): the mtime
//! a commit asked for is what every later look reports, through the pairs
//! the commit leaves, and so it stays once the pairs have been through the
//! store, as at a restart. A real change still reads as one.

use std::sync::Arc;

use super::*;
use crate::fs::coarse::{Coarse, coarse};
use crate::store::{FolderRow, Group, Shim, Store};

/// Mtimes a 2 s filesystem cannot keep: a fraction of a second, all but a
/// nanosecond of one, and an odd second.
const ASKED: [i64; 3] = [
    OLD + 123_456_789,
    OLD + SECOND + 999_999_999,
    OLD + 3 * SECOND,
];

fn coarse_folder(disk: &Disk) -> Coarse {
    Coarse::new(Arc::new(disk.folder()))
}

fn shim(path: &str, requested_ns: i64) -> Shim {
    Shim {
        path: p(path),
        requested_ns,
        stored_ns: coarse(requested_ns),
    }
}

fn put(path: &str, requested_ns: i64) -> HostWrite {
    HostWrite::PutShim {
        folder: folder_id(),
        shim: shim(path, requested_ns),
    }
}

fn removed(path: &str) -> HostWrite {
    HostWrite::RemoveShim {
        folder: folder_id(),
        path: p(path),
    }
}

/// The shim's writes among `writes`.
fn shim_writes(writes: &[HostWrite]) -> Vec<HostWrite> {
    writes
        .iter()
        .filter(|w| matches!(w, HostWrite::PutShim { .. } | HostWrite::RemoveShim { .. }))
        .cloned()
        .collect()
}

impl Disk {
    /// Keep what a commit's writes do to the pairs, as the host does.
    fn keep(&mut self, writes: &[HostWrite]) {
        for write in writes {
            match write {
                HostWrite::PutShim { shim, .. } => {
                    self.shims.insert(shim.path.clone(), shim.clone());
                }
                HostWrite::RemoveShim { path, .. } => {
                    self.shims.remove(path);
                }
                _ => {}
            }
        }
    }

    /// What a scan through `folder` reports for each path.
    fn scanned_through(&self, folder: &dyn Folder) -> BTreeMap<RelPath, ScanState> {
        let clock = settled_now;
        let mut out = BTreeMap::new();
        crate::scan::Scan {
            id: folder_id(),
            folder,
            records: &self.records,
            open: &self.state,
            shims: &self.shims,
            clock: &clock,
            stability: &settled(),
            hashers: std::num::NonZeroUsize::MIN,
        }
        .run(&mut |event| {
            if let Event::Scanned { path, state, .. } = event {
                out.insert(path, state);
            }
        });
        out
    }

    /// What a single observation through `folder` reports for `path`.
    fn observed_through(&self, folder: &dyn Folder, path: &str) -> ScanState {
        let clock = settled_now;
        Commit {
            id: folder_id(),
            folder,
            records: &self.records,
            names: &self.names,
            shims: &self.shims,
            journal: &self.journal,
            clock: &clock,
            stability: &settled(),
            disk: &self.state,
        }
        .observe(&p(path))
        .0
    }

    /// A received file: written from a temp holding `bytes`, at `mtime_ns`,
    /// and adopted as the engine would. Returns the commit's shim writes.
    fn receive(
        &mut self,
        folder: &dyn Folder,
        path: &str,
        bytes: &[u8],
        mtime_ns: i64,
    ) -> Vec<HostWrite> {
        let temp = self.temp(bytes);
        let entry = entry(path, Kind::File, bytes, mtime_ns, false);
        let action = write(path, entry.clone(), None, Displace::Trash);
        let committed = self.commit_with(folder, &action, Some(&temp));
        assert!(applied_ok(&committed), "{:?}", committed.event);
        self.keep(&committed.writes);
        self.records.insert(p(path), entry);
        shim_writes(&committed.writes)
    }
}

fn applied_ok(committed: &Committed) -> bool {
    matches!(
        committed.event,
        Event::Applied {
            outcome: ApplyOutcome::Ok,
            ..
        } | Event::Trashed {
            outcome: ApplyOutcome::Ok,
            ..
        }
    )
}

/// §7.3: files received on a 2 s filesystem keep their pairs, and a
/// hundred scans report each of them `Unchanged`, as does a single
/// observation; without the pairs the same scan would report a touch.
/// Once the pairs have been written to the store and read back, as at a
/// restart, the scans still report nothing. A real edit and a user's
/// touch each read as a change, and the other files stay unchanged.
#[test]
fn received_files_survive_a_hundred_scans_and_a_restart_and_a_real_edit_still_counts() {
    let mut disk = Disk::new();
    let folder = coarse_folder(&disk);
    let names: Vec<String> = (0..ASKED.len()).map(|i| format!("f{i}")).collect();
    for (name, asked) in names.iter().zip(ASKED) {
        let writes = disk.receive(&folder, name, name.as_bytes(), asked);
        assert_eq!(writes, [put(name, asked)]);
    }
    assert_eq!(disk.shims.len(), ASKED.len());

    let unchanged = |scanned: &BTreeMap<RelPath, ScanState>| {
        names
            .iter()
            .all(|name| scanned.get(&p(name)) == Some(&ScanState::Unchanged))
    };
    for round in 0..100 {
        let scanned = disk.scanned_through(&folder);
        assert!(unchanged(&scanned), "scan {round}: {scanned:?}");
    }
    for name in &names {
        assert_eq!(disk.observed_through(&folder, name), ScanState::Unchanged);
    }
    // What the pairs prevent: each file read as touched to what the
    // filesystem kept.
    let kept = std::mem::take(&mut disk.shims);
    let scanned = disk.scanned_through(&folder);
    for (name, asked) in names.iter().zip(ASKED) {
        match scanned.get(&p(name)) {
            Some(ScanState::Observed(seen)) => assert_eq!(seen.mtime_ns, coarse(asked)),
            other => panic!("{name}: {other:?}"),
        }
    }

    // The restart: the pairs through the store and back.
    let state = tempfile::tempdir().unwrap();
    let mut store = Store::open(&state.path().join("delocal.db")).unwrap();
    let mut group = Group::new(Timestamp::from_unix_nanos(1));
    group.host(HostWrite::PutFolder(FolderRow {
        id: folder_id(),
        name: "folder".to_owned(),
        created_by: NodeId::from_bytes([1; 16]),
        rules: delocal_engine::Rules::default(),
        meta_version: 0,
    }));
    for shim in kept.values() {
        group.host(HostWrite::PutShim {
            folder: folder_id(),
            shim: shim.clone(),
        });
    }
    store.commit(&[group]).unwrap();
    drop(store);
    let store = Store::open(&state.path().join("delocal.db")).unwrap();
    disk.shims = store
        .shims(folder_id())
        .unwrap()
        .into_iter()
        .map(|shim| (shim.path.clone(), shim))
        .collect();
    assert_eq!(disk.shims, kept);
    for round in 0..100 {
        let scanned = disk.scanned_through(&folder);
        assert!(
            unchanged(&scanned),
            "after the restart, scan {round}: {scanned:?}"
        );
    }

    // A real edit is a change, and so is a user's touch to another mtime.
    std::fs::write(disk.at("f0"), "edited").unwrap();
    let real = disk.folder().open().unwrap();
    real.set_mtime(Path::new("f1"), OLD + 10 * SECOND).unwrap();
    let scanned = disk.scanned_through(&folder);
    match scanned.get(&p("f0")) {
        Some(ScanState::Observed(seen)) => assert_eq!(seen.hash, blake3_of(b"edited")),
        other => panic!("f0: {other:?}"),
    }
    match scanned.get(&p("f1")) {
        Some(ScanState::Observed(seen)) => {
            assert_eq!(seen.mtime_ns, OLD + 10 * SECOND);
            assert_eq!(seen.hash, blake3_of(b"f1"));
        }
        other => panic!("f1: {other:?}"),
    }
    assert_eq!(scanned.get(&p("f2")), Some(&ScanState::Unchanged));
}

/// §7.3: a metadata-only apply reads back what it set, makes the pair
/// durable before it reports, and reports it too; the next commit's guard
/// sees the mtime that was asked for. Setting an mtime the filesystem can
/// keep removes the pair the same way.
#[test]
fn a_set_meta_makes_its_pair_durable_before_it_reports() {
    let mut disk = Disk::new();
    let folder = coarse_folder(&disk);
    // An mtime the filesystem keeps as it is.
    let mut seen = disk.file("f", b"same", OLD, false);
    disk.records
        .insert(p("f"), entry("f", Kind::File, b"same", OLD, false));

    let asked = ASKED[0];
    let committed = disk.commit_with(
        &folder,
        &set_meta("f", Some(seen.clone()), asked, false),
        None,
    );
    assert!(applied_ok(&committed), "{:?}", committed.event);
    assert_eq!(disk.journal.taken(), [put("f", asked)]);
    assert_eq!(shim_writes(&committed.writes), [put("f", asked)]);
    disk.keep(&committed.writes);
    seen.mtime_ns = asked;
    disk.records.get_mut(&p("f")).unwrap().mtime_ns = asked;
    assert_eq!(
        disk.scanned_through(&folder).get(&p("f")),
        Some(&ScanState::Unchanged)
    );

    // The guard sees what was asked for: another apply at the path goes
    // ahead. The filesystem keeps the same mtime for it, so only the
    // requested one changes.
    let again = asked + 1;
    let committed = disk.commit_with(
        &folder,
        &set_meta("f", Some(seen.clone()), again, false),
        None,
    );
    assert!(applied_ok(&committed), "{:?}", committed.event);
    assert_eq!(disk.journal.taken(), [put("f", again)]);
    disk.keep(&committed.writes);
    seen.mtime_ns = again;

    // An mtime the filesystem can keep: the pair goes, durably first.
    let even = OLD + 4 * SECOND;
    let committed = disk.commit_with(
        &folder,
        &set_meta("f", Some(seen.clone()), even, false),
        None,
    );
    assert!(applied_ok(&committed), "{:?}", committed.event);
    assert_eq!(disk.journal.taken(), [removed("f")]);
    assert_eq!(shim_writes(&committed.writes), [removed("f")]);
    disk.keep(&committed.writes);
    assert!(disk.shims.is_empty());

    // The same again changes nothing, and writes nothing.
    seen.mtime_ns = even;
    let committed = disk.commit_with(
        &folder,
        &set_meta("f", Some(seen.clone()), even, false),
        None,
    );
    assert!(applied_ok(&committed), "{:?}", committed.event);
    assert!(disk.journal.taken().is_empty());
    assert!(shim_writes(&committed.writes).is_empty());
}

/// §7.3: where the filesystem keeps what it is asked, a write keeps no
/// pair, and removes one the path had.
#[test]
fn on_an_exact_filesystem_a_write_keeps_no_pair_and_drops_an_old_one() {
    let mut disk = Disk::new();
    let folder = disk.folder();
    assert!(disk.receive(&folder, "f", b"one", ASKED[0]).is_empty());
    disk.shims.insert(p("g"), shim("g", ASKED[1]));
    assert_eq!(disk.receive(&folder, "g", b"two", ASKED[1]), [removed("g")]);
}

/// §7.3, §7.6: a commit that leaves the path empty removes its pair (a
/// delete to the trash, a revert's move), and a file displaced to a
/// conflict copy takes its pair there, where the copy's record carries the
/// mtime the pair stands for. A directory written over a file drops the
/// file's pair.
#[test]
fn a_path_left_empty_loses_its_pair_and_a_conflict_copy_takes_it() {
    let mut disk = Disk::new();
    let folder = coarse_folder(&disk);
    for (name, asked) in [
        ("deleted", ASKED[0]),
        ("reverted", ASKED[1]),
        ("lost", ASKED[2]),
    ] {
        disk.receive(&folder, name, name.as_bytes(), asked);
    }
    disk.receive(&folder, "replaced", b"replaced", ASKED[0]);
    let observed = |disk: &Disk, path: &str| -> Observed {
        let e = &disk.records[&p(path)];
        Observed {
            kind: e.kind,
            size: e.size,
            mtime_ns: e.mtime_ns,
            exec: e.exec,
            hash: e.hash,
        }
    };

    let deleted = observed(&disk, "deleted");
    let committed = disk.commit_with(
        &folder,
        &remove("deleted", Some(deleted), Displace::Trash),
        None,
    );
    assert!(applied_ok(&committed), "{:?}", committed.event);
    assert_eq!(shim_writes(&committed.writes), [removed("deleted")]);

    let reverted = observed(&disk, "reverted");
    let action = Action::MoveToTrash {
        folder: folder_id(),
        path: p("reverted"),
        expected: reverted,
        version: version(),
    };
    let committed = disk.commit_with(&folder, &action, None);
    assert!(applied_ok(&committed), "{:?}", committed.event);
    assert_eq!(shim_writes(&committed.writes), [removed("reverted")]);

    // The winner's content in, the loser to its copy, with its pair.
    let lost = observed(&disk, "lost");
    let temp = disk.temp(b"winner");
    let winner = entry("lost", Kind::File, b"winner", OLD, false);
    let copy = p("lost.conflict");
    let action = write(
        "lost",
        winner,
        Some(lost),
        Displace::ConflictCopy(copy.clone()),
    );
    let committed = disk.commit_with(&folder, &action, Some(&temp));
    assert!(applied_ok(&committed), "{:?}", committed.event);
    assert_eq!(
        shim_writes(&committed.writes),
        [
            HostWrite::PutShim {
                folder: folder_id(),
                shim: Shim {
                    path: copy.clone(),
                    ..shim("lost", ASKED[2])
                },
            },
            removed("lost"),
        ]
    );
    disk.keep(&committed.writes);
    // The copy's record, as the engine makes it: the loser's mtime.
    let mut copy_entry = disk.records[&p("lost")].clone();
    copy_entry.path = copy.clone();
    disk.records.insert(copy.clone(), copy_entry);
    disk.records
        .insert(p("lost"), entry("lost", Kind::File, b"winner", OLD, false));
    assert_eq!(
        disk.scanned_through(&folder).get(&copy),
        Some(&ScanState::Unchanged)
    );

    let replaced = observed(&disk, "replaced");
    let action = write(
        "replaced",
        entry("replaced", Kind::Dir, b"", 0, false),
        Some(replaced),
        Displace::Trash,
    );
    let committed = disk.commit_with(&folder, &action, None);
    assert!(applied_ok(&committed), "{:?}", committed.event);
    assert_eq!(shim_writes(&committed.writes), [removed("replaced")]);
}

/// §7.3, draft 65's reasoning applied to the shim: a write reads back its
/// temp file, not the path, so a user's write at the path just after the
/// rename is never taken for what the filesystem kept.
#[test]
fn a_write_reads_back_its_temp_file_not_the_path() {
    let disk = Disk::new();
    let spy = crate::fs::spy::Spy::new(disk.root());
    let temp = disk.temp(b"in");
    let action = write(
        "f",
        entry("f", Kind::File, b"in", ASKED[0], false),
        None,
        Displace::Trash,
    );
    let committed = disk.commit_with(&spy, &action, Some(&temp));
    assert!(applied_ok(&committed), "{:?}", committed.event);
    let steps = spy.all();
    let set = steps
        .iter()
        .position(|(op, path)| *op == "fs.set_mtime" && *path == temp)
        .unwrap();
    let rename = steps
        .iter()
        .position(|(op, _)| *op == "fs.rename_noreplace")
        .unwrap();
    let read_back = steps[set..rename]
        .iter()
        .any(|(op, path)| *op == "fs.lstat" && *path == temp);
    assert!(read_back, "{:?}", &steps[set..]);
    assert!(
        !steps[rename..]
            .iter()
            .any(|(op, path)| *op == "fs.lstat" && path == Path::new("f")),
        "{:?}",
        &steps[rename..]
    );
}
