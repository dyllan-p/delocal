//! A user writing at the path while the host commits there (DESIGN.md
//! §7.5, "Renames never replace"): a file created between the check and the
//! rename survives, and a thread rewriting the target through 10,000
//! commits loses none of what it wrote.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::sync::Arc;

use super::*;

/// What the fast path sees at `path` now, as the engine would expect it.
fn seen(disk: &Disk, path: &str) -> Option<Observed> {
    let meta = std::fs::symlink_metadata(disk.at(path)).ok()?;
    Some(Observed {
        kind: Kind::File,
        size: meta.len(),
        mtime_ns: meta.mtime() * SECOND + meta.mtime_nsec(),
        exec: meta.mode() & 0o100 != 0,
        hash: ContentHash::EMPTY,
    })
}

#[test]
fn a_file_created_at_the_path_before_the_rename_in_survives() {
    // The path was empty when the commit looked.
    let disk = Disk::new();
    let spy = Spy::new(disk.root());
    let temp = disk.temp(b"received");
    let target = disk.at("f");
    spy.before("fs.rename_noreplace", temp.clone(), move || {
        std::fs::write(target, "the user's").unwrap();
    });
    let done = disk.commit_with(
        &spy,
        &write(
            "f",
            entry("f", Kind::File, b"received", OLD, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert!(
        matches!(outcome(&done), ApplyOutcome::ChangedUnderneath { found: ScanState::Observed(o) } if o.hash == blake3_of(b"the user's"))
    );
    assert_eq!(disk.read("f"), b"the user's");
    assert!(
        disk.exists(temp.to_str().unwrap()),
        "the temp file stays to resume"
    );
    assert!(done.writes.is_empty());

    // Something was there, and was displaced; the user writes the empty
    // path before the rename in. Both survive: the user's file at the
    // path, the displaced one in the trash with its row, since the path it
    // would go back to is taken.
    let disk = Disk::new();
    let spy = Spy::new(disk.root());
    let old = disk.file("f", b"old", OLD, false);
    let temp = disk.temp(b"received");
    let target = disk.at("f");
    spy.before("fs.rename_noreplace", temp.clone(), move || {
        std::fs::write(target, "the user's").unwrap();
    });
    let done = disk.commit_with(
        &spy,
        &write(
            "f",
            entry("f", Kind::File, b"received", OLD, false),
            Some(old),
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::ChangedUnderneath { .. }
    ));
    assert_eq!(disk.read("f"), b"the user's");
    assert_eq!(disk.trash(), [b"old".to_vec()]);
    assert_eq!(
        done.writes,
        [HostWrite::RemoveJournal {
            folder: folder_id(),
            path: p("f")
        }],
        "the journal row goes, the trash row stays"
    );
}

#[test]
fn a_file_created_at_a_target_before_any_rename_survives() {
    // At the conflict-copy path, before the displacement.
    let disk = Disk::new();
    let spy = Spy::new(disk.root());
    let old = disk.file("a.txt", b"mine", OLD, false);
    let temp = disk.temp(b"theirs");
    let copy = disk.at("a.conflict.txt");
    spy.before("fs.rename_noreplace", "a.txt", move || {
        std::fs::write(copy, "the user's").unwrap();
    });
    let action = write(
        "a.txt",
        entry("a.txt", Kind::File, b"theirs", OLD, false),
        Some(old),
        Displace::ConflictCopy(p("a.conflict.txt")),
    );
    let done = disk.commit_with(&spy, &action, Some(&temp));
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::ChangedUnderneath { .. }
    ));
    assert_eq!(disk.read("a.txt"), b"mine");
    assert_eq!(disk.read("a.conflict.txt"), b"the user's");

    // Where a directory is to be made.
    let disk = Disk::new();
    let spy = Spy::new(disk.root());
    let target = disk.at("d");
    spy.before("fs.create_dir", "d", move || {
        std::fs::write(target, "the user's").unwrap();
    });
    let done = disk.commit_with(
        &spy,
        &write(
            "d",
            entry("d", Kind::Dir, b"", 0, false),
            None,
            Displace::Trash,
        ),
        None,
    );
    assert!(matches!(
        outcome(&done),
        ApplyOutcome::ChangedUnderneath { .. }
    ));
    assert_eq!(disk.read("d"), b"the user's");

    // Where a missing parent is to be made: what the user made is left,
    // and nothing is written beneath a file.
    let disk = Disk::new();
    let spy = Spy::new(disk.root());
    let temp = disk.temp(b"x");
    let parent = disk.at("p");
    spy.before("fs.create_dir", "p", move || {
        std::fs::write(parent, "the user's").unwrap();
    });
    let done = disk.commit_with(
        &spy,
        &write(
            "p/x",
            entry("p/x", Kind::File, b"x", OLD, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(outcome(&done), &changed(ScanState::Absent));
    assert_eq!(disk.read("p"), b"the user's");

    // In the trash, the place taken between its check and the move is
    // skipped for the next (see the trash's own tests); here, through a
    // whole commit.
    let disk = Disk::new();
    let spy = Spy::new(disk.root());
    let old = disk.file("g", b"old", OLD, false);
    let trash_dir = disk.at(trash::DIR);
    spy.before("fs.rename_noreplace", "g", move || {
        // The commit has made today's directory: take the place in it.
        let day = std::fs::read_dir(&trash_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        std::fs::write(day.path().join("g"), "in the trash already").unwrap();
    });
    let done = disk.commit_with(&spy, &remove("g", Some(old), Displace::Trash), None);
    assert_eq!(outcome(&done), &ApplyOutcome::Ok);
    assert_eq!(
        disk.trash(),
        [b"in the trash already".to_vec(), b"old".to_vec()]
    );
}

/// §7.5: a thread rewriting the target, by creating it whenever it is
/// absent and writing it in two halves, while the host commits there
/// 10,000 times, alternately writing new content over it and deleting it.
/// Every file the thread wrote, and every one the host wrote, ends whole
/// in the folder or the trash. The commits skip fsyncs: this tests what
/// renames do, not what survives a power loss.
#[test]
fn a_user_rewriting_the_target_through_10000_commits_loses_nothing() {
    const COMMITS: usize = 10_000;
    let disk = Disk::new();
    let spy = Spy::new(disk.root());
    spy.quiet();
    spy.no_sync();
    let stop = Arc::new(AtomicBool::new(false));
    let user = {
        let (stop, target) = (Arc::clone(&stop), disk.at("target"));
        std::thread::spawn(move || {
            let mut written = Vec::new();
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                {
                    Ok(mut file) => {
                        let half = format!("user-{i:08}|");
                        file.write_all(half.as_bytes()).unwrap();
                        std::thread::yield_now();
                        file.write_all(half.as_bytes()).unwrap();
                        written.push(half.repeat(2).into_bytes());
                        i += 1;
                        // A moment's pause, so the host's commits find a
                        // settled file to write over as often as a file
                        // being written.
                        std::thread::sleep(std::time::Duration::from_micros(50));
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        std::thread::yield_now();
                    }
                    Err(e) => panic!("the user could not write: {e}"),
                }
            }
            written
        })
    };

    let mut ours = Vec::new();
    let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
    for n in 0..COMMITS {
        let expected = seen(&disk, "target");
        let done = if n % 2 == 0 {
            let content = format!("sync-{n:08}|").into_bytes();
            let temp = disk.temp(&content);
            let incoming = entry("target", Kind::File, &content, OLD, false);
            let done = disk.commit_with(
                &spy,
                &write("target", incoming, expected, Displace::Trash),
                Some(&temp),
            );
            if *outcome(&done) == ApplyOutcome::Ok {
                ours.push(content);
            } else {
                let _ = std::fs::remove_file(disk.at(temp.to_str().unwrap()));
            }
            done
        } else {
            disk.commit_with(&spy, &remove("target", expected, Displace::Trash), None)
        };
        let kind = match outcome(&done) {
            ApplyOutcome::Ok => "ok",
            ApplyOutcome::ChangedUnderneath { .. } => "changed",
            ApplyOutcome::Failed { .. } => "failed",
        };
        *outcomes.entry(format!("{} {kind}", n % 2)).or_default() += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let written = user.join().unwrap();

    assert_eq!(
        outcomes.keys().filter(|k| k.ends_with("failed")).count(),
        0,
        "{outcomes:?}"
    );
    let mut everywhere = disk.trash();
    if let Ok(bytes) = std::fs::read(disk.at("target")) {
        everywhere.push(bytes);
    }
    everywhere.sort();
    let missing = |wanted: &[Vec<u8>]| -> Vec<String> {
        wanted
            .iter()
            .filter(|w| everywhere.binary_search(w).is_err())
            .map(|w| String::from_utf8_lossy(w).into_owned())
            .collect()
    };
    // Both sides wrote, so the interleavings were exercised; how often
    // each wins depends on the machine's timing.
    assert!(
        written.len() >= 10,
        "the user wrote only {} files: {outcomes:?}",
        written.len()
    );
    assert!(ours.len() >= 10, "{outcomes:?}");
    assert_eq!(missing(&written), Vec::<String>::new(), "{outcomes:?}");
    assert_eq!(missing(&ours), Vec::<String>::new(), "{outcomes:?}");
    // Nothing else was made anywhere: the folder holds the target or
    // nothing, and the trash holds exactly what was written.
    assert_eq!(everywhere.len(), written.len() + ours.len(), "{outcomes:?}");
}

/// §7.3, the root guard: a folder whose marker goes while a commit runs
/// (its disk unmounted, say) reports what it found as `Skipped`, never as
/// what a stat of the now empty mount point would say.
#[test]
fn a_marker_gone_during_a_commit_makes_its_report_skipped() {
    let disk = Disk::new();
    let spy = Spy::new(disk.root());
    let temp = disk.temp(b"received");
    let (marker, target) = (disk.at(RESERVED_DIR).join(MARKER), disk.at("f"));
    spy.before("fs.rename_noreplace", temp.clone(), move || {
        std::fs::remove_file(marker).unwrap();
        std::fs::write(target, "the user's").unwrap();
    });
    let done = disk.commit_with(
        &spy,
        &write(
            "f",
            entry("f", Kind::File, b"received", OLD, false),
            None,
            Displace::Trash,
        ),
        Some(&temp),
    );
    assert_eq!(
        outcome(&done),
        &changed(ScanState::Skipped {
            reason: SkipReason::Io
        })
    );
}
