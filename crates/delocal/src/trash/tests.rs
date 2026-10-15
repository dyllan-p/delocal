//! Tests of the trash (DESIGN.md §8.4) on real directories.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::fs::{Folder, RealFolder};

const SECOND: i64 = 1_000_000_000;
const DAY: i64 = 86_400 * SECOND;
/// 2026-10-15T12:00:00Z.
const NOON: i64 = 1_792_065_600 * SECOND;

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

fn at(nanos: i64) -> Timestamp {
    Timestamp::from_unix_nanos(nanos)
}

fn folder_id() -> FolderId {
    FolderId::from_bytes([7; 16])
}

fn blake3_of(bytes: &[u8]) -> ContentHash {
    ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
}

/// A folder with `.delocal/`, and one operation on it.
fn folder() -> (tempfile::TempDir, Box<dyn Fs>) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".delocal")).unwrap();
    let fs = RealFolder::new(tmp.path()).open().unwrap();
    (tmp, fs)
}

/// Everything in the trash: each file's place under the trash and content.
fn trash(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let rel = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(&entry.path(), &rel, out);
            } else if kind.is_symlink() {
                let target = std::fs::read_link(entry.path()).unwrap();
                out.insert(rel, target.into_os_string().into_encoded_bytes());
            } else {
                out.insert(rel, std::fs::read(entry.path()).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&root.join(DIR), "", &mut out);
    out
}

fn moving<'a>(original: &'a RelPath, bytes: &[u8], at_nanos: i64) -> Moving<'a> {
    Moving {
        original,
        kind: Kind::File,
        hash: blake3_of(bytes),
        size: bytes.len() as u64,
        at: at(at_nanos),
    }
}

#[test]
fn the_day_is_the_utc_day_of_the_move() {
    assert_eq!(day(at(0)), "1970-01-01");
    assert_eq!(day(at(NOON)), "2026-10-15");
    let midnight = NOON + 12 * 3600 * SECOND;
    assert_eq!(day(at(midnight - 1)), "2026-10-15");
    assert_eq!(day(at(midnight)), "2026-10-16");
    // 2024-02-29T00:00:00Z
    assert_eq!(day(at(1_709_164_800 * SECOND)), "2024-02-29");
}

#[test]
fn a_name_taken_that_day_is_numbered_before_its_extension() {
    assert_eq!(numbered("report.xlsx", Kind::File, 0), "report.xlsx");
    assert_eq!(numbered("report.xlsx", Kind::File, 1), "report~1.xlsx");
    assert_eq!(
        numbered("archive.tar.gz", Kind::File, 2),
        "archive.tar~2.gz"
    );
    assert_eq!(numbered(".bashrc", Kind::File, 1), ".bashrc~1");
    assert_eq!(numbered("Makefile", Kind::Symlink, 3), "Makefile~3");
    assert_eq!(numbered("photos.d", Kind::Dir, 1), "photos.d~1");
    // Too long: the stem gives way, then the extension, never the number.
    let long = format!("{}.txt", "a".repeat(253));
    let cut = numbered(&long, Kind::File, 12);
    assert_eq!(cut.len(), 255);
    assert_eq!(cut, format!("{}~12.txt", "a".repeat(248)));
    let ext = format!("x.{}", "e".repeat(253));
    let cut = numbered(&ext, Kind::File, 1);
    assert_eq!(cut.len(), 255);
    assert!(cut.starts_with("~1."), "{cut}");
}

proptest! {
    /// §8.4: whatever the name, a numbered one fits a component, keeps
    /// `~n`, and cuts only at character boundaries.
    #[test]
    fn a_numbered_name_always_fits_and_keeps_its_number(
        stem in "[a-zé😀.]{0,140}",
        ext in "[a-zé😀]{0,140}",
        n in 1u32..100_000,
        dir in any::<bool>(),
    ) {
        let name = format!("{stem}.{ext}");
        let name = cut(&name, MAX_COMPONENT_LEN).to_owned();
        prop_assume!(!name.is_empty());
        let kind = if dir { Kind::Dir } else { Kind::File };
        let out = numbered(&name, kind, n);
        prop_assert!(out.len() <= MAX_COMPONENT_LEN);
        let number = format!("~{n}");
        prop_assert!(out.contains(&number));
        // Nothing is cut from a name that fits with its number.
        if name.len() + number.len() <= MAX_COMPONENT_LEN {
            prop_assert_eq!(out.replacen(&number, "", 1), name);
        }
    }
}

#[test]
fn entries_go_under_their_day_and_path_and_never_replace() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    let original = p("docs/report.txt");
    let mut places = Vec::new();
    for (i, content) in [&b"one"[..], b"two", b"three"].into_iter().enumerate() {
        std::fs::write(root.join("victim"), content).unwrap();
        let what = moving(
            &original,
            content,
            NOON + i64::try_from(i).unwrap() * SECOND,
        );
        let moved = move_in(&*fs, Path::new("victim"), &what, &mut |place| {
            places.push(place.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(moved.refusing, Refusing::Atomic);
    }
    // The next day starts again without a number.
    std::fs::write(root.join("victim"), "four").unwrap();
    let what = moving(&original, b"four", NOON + DAY);
    move_in(&*fs, Path::new("victim"), &what, &mut |_| Ok(())).unwrap();

    assert_eq!(
        places.iter().map(|pl| pl.row.clone()).collect::<Vec<_>>(),
        [
            PathBuf::from("2026-10-15/docs/report.txt"),
            PathBuf::from("2026-10-15/docs/report~1.txt"),
            PathBuf::from("2026-10-15/docs/report~2.txt"),
        ]
    );
    assert_eq!(places[0].disk, Path::new(DIR).join(&places[0].row));
    let expected: BTreeMap<String, Vec<u8>> = [
        ("2026-10-15/docs/report.txt", &b"one"[..]),
        ("2026-10-15/docs/report~1.txt", b"two"),
        ("2026-10-15/docs/report~2.txt", b"three"),
        ("2026-10-16/docs/report.txt", b"four"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_vec()))
    .collect();
    assert_eq!(trash(root), expected);
}

#[test]
fn a_parent_taken_in_the_trash_is_numbered_too() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    // `a` was trashed as a file today; now `a/b` follows it.
    std::fs::write(root.join("x"), "a as a file").unwrap();
    let a = p("a");
    move_in(
        &*fs,
        Path::new("x"),
        &moving(&a, b"a as a file", NOON),
        &mut |_| Ok(()),
    )
    .unwrap();
    std::fs::write(root.join("x"), "b").unwrap();
    let b = p("a/b");
    let moved = move_in(&*fs, Path::new("x"), &moving(&b, b"b", NOON), &mut |_| {
        Ok(())
    })
    .unwrap();
    assert_eq!(moved.place.row, Path::new("2026-10-15/a~1/b"));
}

#[test]
fn a_place_taken_in_the_window_is_skipped_and_kept() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    std::fs::write(root.join("victim"), "ours").unwrap();
    let original = p("f.txt");
    let mut calls = 0;
    let moved = move_in(
        &*fs,
        Path::new("victim"),
        &moving(&original, b"ours", NOON),
        &mut |place| {
            calls += 1;
            if calls == 1 {
                // Something lands on the place between its check and the
                // rename.
                std::fs::write(root.join(&place.disk), "theirs").unwrap();
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(moved.place.row, Path::new("2026-10-15/f~1.txt"));
    let in_trash = trash(root);
    assert_eq!(in_trash["2026-10-15/f.txt"], b"theirs");
    assert_eq!(in_trash["2026-10-15/f~1.txt"], b"ours");
}

#[test]
fn a_failed_before_or_a_vanished_entry_moves_nothing() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    std::fs::write(root.join("victim"), "ours").unwrap();
    let original = p("f");
    let what = moving(&original, b"ours", NOON);
    let refused = move_in(&*fs, Path::new("victim"), &what, &mut |_| {
        Err(io::Error::other("journal"))
    });
    assert!(matches!(refused, Err(NotMoved::Failed(_))));
    assert_eq!(std::fs::read(root.join("victim")).unwrap(), b"ours");
    assert!(trash(root).is_empty());

    let gone = move_in(&*fs, Path::new("missing"), &what, &mut |_| Ok(()));
    assert!(matches!(gone, Err(NotMoved::Gone)), "{gone:?}");

    // Without `.delocal/` the trash is not made.
    std::fs::remove_dir_all(root.join(".delocal")).unwrap();
    let no_folder = move_in(&*fs, Path::new("victim"), &what, &mut |_| Ok(()));
    assert!(matches!(no_folder, Err(NotMoved::Failed(_))));
    assert!(!root.join(".delocal").exists());
}

#[test]
fn a_directory_moves_whole_and_counts_everything_in_it() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("d/sub")).unwrap();
    std::fs::write(root.join("d/a"), "12345").unwrap();
    std::fs::write(root.join("d/sub/b"), "678").unwrap();
    std::os::unix::fs::symlink("ab", root.join("d/s")).unwrap();
    assert_eq!(size_of(&*fs, Path::new("d")), 5 + 3 + 2);
    let original = p("d");
    let what = Moving {
        original: &original,
        kind: Kind::Dir,
        hash: ContentHash::EMPTY,
        size: 10,
        at: at(NOON),
    };
    let moved = move_in(&*fs, Path::new("d"), &what, &mut |_| Ok(())).unwrap();
    assert_eq!(moved.place.row, Path::new("2026-10-15/d"));
    assert_eq!(trash(root)["2026-10-15/d/sub/b"], b"678");
}

fn row(path: &str, trashed_at: i64, size: u64) -> TrashRow {
    TrashRow {
        trashed_path: path.into(),
        original_path: p("x"),
        hash: blake3_of(b"x"),
        trashed_at: at(trashed_at),
        size,
    }
}

fn removed(writes: &[HostWrite]) -> Vec<PathBuf> {
    writes
        .iter()
        .map(|w| match w {
            HostWrite::RemoveTrash { trashed_path, .. } => trashed_path.clone(),
            other => panic!("not a removal: {other:?}"),
        })
        .collect()
}

#[test]
fn pruning_takes_what_is_old_then_the_oldest_past_the_cap() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    let now = NOON + 40 * DAY;
    let rows = [
        row("2026-10-15/old", NOON, 1),
        row("2026-11-10/a/mid", NOON + 26 * DAY, 400),
        row("2026-11-11/new", NOON + 27 * DAY, 400),
        row("2026-11-12/newest", NOON + 28 * DAY, 400),
    ];
    for r in &rows {
        let at = root.join(DIR).join(&r.trashed_path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, vec![0; usize::try_from(r.size).unwrap()]).unwrap();
    }
    let policy = Policy {
        max_age_nanos: 30 * DAY,
        max_bytes: 800,
    };
    let pruned = prune(&*fs, folder_id(), &rows, at(now), policy);
    assert!(pruned.failed.is_empty());
    // The old one by age, then the oldest of the rest until 800 bytes.
    assert_eq!(
        removed(&pruned.writes),
        [
            PathBuf::from("2026-10-15/old"),
            PathBuf::from("2026-11-10/a/mid")
        ]
    );
    let left: Vec<String> = trash(root).into_keys().collect();
    assert_eq!(left, ["2026-11-11/new", "2026-11-12/newest"]);
    // The directories they left empty go too.
    assert!(!root.join(DIR).join("2026-10-15").exists());
    assert!(!root.join(DIR).join("2026-11-10").exists());
    assert!(root.join(DIR).exists());

    // Within the policy nothing goes.
    let pruned = prune(&*fs, folder_id(), &rows[2..], at(now), policy);
    assert!(pruned.writes.is_empty());
}

#[test]
fn reconciling_adopts_orphans_and_drops_rows_whose_entry_is_gone() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    let day = root.join(DIR).join("2026-10-15");
    std::fs::create_dir_all(day.join("docs")).unwrap();
    std::fs::write(day.join("docs/kept.txt"), "kept").unwrap();
    std::fs::write(day.join("docs/orphan~1.txt"), "orphan").unwrap();
    std::os::unix::fs::symlink("target", day.join("link")).unwrap();
    std::fs::create_dir_all(day.join("whole/inner")).unwrap();
    std::fs::write(day.join("whole/inner/f"), "covered by its row").unwrap();
    let rows = [
        row("2026-10-15/docs/kept.txt", NOON, 4),
        row("2026-10-15/whole", NOON, 18),
        row("2026-10-15/vanished", NOON, 1),
    ];
    let now = at(NOON + DAY);
    let mut writes = reconcile(&*fs, folder_id(), &rows, now);
    writes.sort_by_key(|w| format!("{w:?}"));
    let expected = {
        let mut e = vec![
            HostWrite::RemoveTrash {
                folder: folder_id(),
                trashed_path: "2026-10-15/vanished".into(),
            },
            HostWrite::PutTrash {
                folder: folder_id(),
                row: TrashRow {
                    trashed_path: "2026-10-15/docs/orphan~1.txt".into(),
                    original_path: p("docs/orphan~1.txt"),
                    hash: blake3_of(b"orphan"),
                    trashed_at: now,
                    size: 6,
                },
            },
            HostWrite::PutTrash {
                folder: folder_id(),
                row: TrashRow {
                    trashed_path: "2026-10-15/link".into(),
                    original_path: p("link"),
                    hash: hash_target(b"target"),
                    trashed_at: now,
                    size: 6,
                },
            },
        ];
        e.sort_by_key(|w| format!("{w:?}"));
        e
    };
    assert_eq!(writes, expected);
}
