//! Tests of reaching an index path on disk (DESIGN.md §7.3, §7.5 step 6),
//! on real directories. Which names a filesystem folds is a property of the
//! disk the tests run on, so the tests that depend on it ask the probe and
//! check both answers against what `std` sees: CI runs them on ext4, which
//! folds nothing, and on APFS, which folds case and normalisation.

use std::os::unix::ffi::OsStrExt;

use super::*;
use crate::fs::spy::Spy;
use crate::fs::{Folder, RealFolder};

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

/// A folder with `.delocal/tmp/`, and one operation on it.
fn folder() -> (tempfile::TempDir, Box<dyn Fs>) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(TMP_DIR)).unwrap();
    let fs = RealFolder::new(tmp.path()).open().unwrap();
    (tmp, fs)
}

fn device(fs: &dyn Fs) -> u64 {
    fs.lstat(Path::new("")).unwrap().dev
}

#[test]
fn a_disk_path_takes_each_components_pair() {
    let mut names = BTreeMap::new();
    names.insert(p("caf\u{e9}"), OsString::from("cafe\u{301}"));
    names.insert(p("caf\u{e9}/x/caf\u{e9}"), OsString::from("CAFE\u{301}"));
    assert_eq!(disk_path(&p("a/b"), &names), Path::new("a/b"));
    assert_eq!(
        disk_path(&p("caf\u{e9}/x/caf\u{e9}"), &names),
        Path::new("cafe\u{301}/x/CAFE\u{301}")
    );
    // A pair belongs to its whole path, not to a name wherever it appears.
    assert_eq!(
        disk_path(&p("x/caf\u{e9}"), &names),
        Path::new("x/caf\u{e9}")
    );
}

#[test]
fn only_case_joins_two_index_names() {
    let exact = Sensitivity::EXACT;
    let case = Sensitivity {
        folds_case: true,
        folds_normalisation: true,
    };
    let normalisation = Sensitivity {
        folds_case: false,
        folds_normalisation: true,
    };
    assert!(exact.same_name("a", "a"));
    assert!(!exact.same_name("a", "A"));
    assert!(case.same_name("Report.TXT", "report.txt"));
    assert!(case.same_name("\u{c9}t\u{e9}", "\u{e9}t\u{c9}"));
    assert!(!case.same_name("a", "b"));
    assert!(!normalisation.same_name("a", "A"));
}

#[test]
fn a_collision_is_a_live_record_spelt_otherwise_at_the_path_or_above() {
    let case = Sensitivity {
        folds_case: true,
        folds_normalisation: true,
    };
    let live = [p("Docs"), p("Docs/Notes.txt"), p("music")];
    let at = |path: &str, s| collision(&p(path), &live, s);
    // The path itself, spelt otherwise.
    assert_eq!(at("docs/notes.TXT", case), Some(p("Docs")));
    assert_eq!(at("Docs/notes.txt", case), Some(p("Docs/Notes.txt")));
    assert_eq!(at("MUSIC", case), Some(p("music")));
    // Its own ancestors, and paths that only share a prefix, are not.
    assert_eq!(at("Docs/Other.txt", case), None);
    assert_eq!(at("Docs/Notes.txt", case), None);
    assert_eq!(at("musical", case), None);
    // A record deeper than the path does not collide with it: its own
    // ancestors are live records too, and those do.
    assert_eq!(at("DOCS", case), Some(p("Docs")));
    // A filesystem that keeps case has no collisions at all.
    assert_eq!(at("docs/notes.TXT", Sensitivity::EXACT), None);
}

#[test]
fn the_probe_agrees_with_what_std_sees() {
    let (tmp, fs) = folder();
    let found = Sensitivity::probe(&*fs).unwrap();
    let dir = tmp.path().join(TMP_DIR);
    // The probe leaves nothing behind.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    std::fs::write(dir.join("lower"), "x").unwrap();
    std::fs::write(dir.join("caf\u{e9}"), "x").unwrap();
    assert_eq!(found.folds_case, dir.join("LOWER").exists());
    assert_eq!(found.folds_normalisation, dir.join("cafe\u{301}").exists());
}

#[test]
fn a_walk_stops_where_a_parent_is_missing_not_a_directory_or_mounted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("a/file"), "x").unwrap();
    std::fs::create_dir_all(root.join("m/inner")).unwrap();
    let spy = Spy::new(root);
    spy.mount("m");
    let fs = spy.open().unwrap();
    let dev = device(&*fs);
    let walked = |path: &str| {
        let path = p(path);
        walk(&*fs, &path, Path::new(path.as_str()), dev).unwrap()
    };

    assert_eq!(walked("top"), Walk::Reached);
    assert_eq!(walked("a/b/c"), Walk::Reached);
    assert_eq!(
        walked("a/missing/deeper/x"),
        Walk::Missing {
            first: p("a/missing")
        }
    );
    assert_eq!(walked("a/file/x"), Walk::NotADirectory { at: p("a/file") });
    assert_eq!(walked("m/inner/x"), Walk::Crossed { at: p("m") });
    // The walk stops at the parent: the path itself is the look's.
    assert_eq!(walked("m"), Walk::Reached);
}

#[test]
fn a_look_sees_each_kind_as_the_fast_path_does() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    std::fs::write(root.join("f"), "four").unwrap();
    fs.set_mode(Path::new("f"), 0o755).unwrap();
    fs.set_mtime(Path::new("f"), 1_700_000_000_123_456_789)
        .unwrap();
    std::fs::create_dir(root.join("d")).unwrap();
    std::os::unix::fs::symlink("target", root.join("s")).unwrap();
    let sensitivity = Sensitivity::probe(&*fs).unwrap();
    let looked =
        |name: &str| look(&*fs, Path::new(""), &p(name), OsStr::new(name), sensitivity).unwrap();
    let seen = |look: Look| match look {
        Look::Present { seen, .. } => seen,
        other => panic!("expected an entry, got {other:?}"),
    };

    assert_eq!(looked("missing"), Look::Absent);
    assert_eq!(
        seen(looked("f")),
        Some(Observed {
            kind: Kind::File,
            size: 4,
            mtime_ns: 1_700_000_000_123_456_789,
            exec: true,
            hash: ContentHash::EMPTY,
        })
    );
    assert_eq!(seen(looked("d")).unwrap().kind, Kind::Dir);
    assert_eq!(
        seen(looked("s")),
        Some(Observed {
            kind: Kind::Symlink,
            size: 6,
            mtime_ns: 0,
            exec: false,
            hash: hash_target(b"target"),
        })
    );
    let _socket = std::os::unix::net::UnixListener::bind(root.join("sock")).unwrap();
    assert_eq!(seen(looked("sock")), None);
}

#[test]
fn a_look_takes_only_the_exact_name_for_the_path() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    let sensitivity = Sensitivity::probe(&*fs).unwrap();
    std::fs::write(root.join("Report.txt"), "x").unwrap();
    std::fs::write(root.join("cafe\u{301}"), "x").unwrap();
    let looked = |path: &str, name: &[u8]| {
        look(
            &*fs,
            Path::new(""),
            &p(path),
            OsStr::from_bytes(name),
            sensitivity,
        )
        .unwrap()
    };

    // Another case is another path: on a disk that folds case it is taken
    // by the file the user has, and on one that keeps case it is absent.
    let other_case = looked("report.txt", b"report.txt");
    if sensitivity.folds_case {
        assert_eq!(
            other_case,
            Look::Taken {
                name: "Report.txt".into()
            }
        );
    } else {
        assert_eq!(other_case, Look::Absent);
    }

    // The NFD name on disk is the NFC index path. Through its pair it is
    // found anywhere; by its index name only where the disk folds
    // normalisation, which then names it as the disk does.
    let nfd = "cafe\u{301}".as_bytes();
    match looked("caf\u{e9}", nfd) {
        Look::Present { name, .. } => assert_eq!(name.as_bytes(), nfd),
        other => panic!("expected the user's file, got {other:?}"),
    }
    let by_index = looked("caf\u{e9}", "caf\u{e9}".as_bytes());
    if sensitivity.folds_normalisation {
        match by_index {
            Look::Present { name, .. } => assert_eq!(name.as_bytes(), nfd),
            other => panic!("expected the user's file, got {other:?}"),
        }
    } else {
        assert_eq!(by_index, Look::Absent);
    }
}

#[test]
fn two_names_for_one_path_are_coinciding() {
    let (tmp, fs) = folder();
    let root = tmp.path();
    let sensitivity = Sensitivity {
        folds_case: false,
        folds_normalisation: true,
    };
    std::fs::write(root.join("caf\u{e9}"), "nfc").unwrap();
    // A disk that folds normalisation cannot hold the second name.
    if std::fs::write(root.join("cafe\u{301}"), "nfd").is_err()
        || std::fs::read_dir(root).unwrap().count() < 3
    {
        return;
    }
    let looked = look(
        &*fs,
        Path::new(""),
        &p("caf\u{e9}"),
        OsStr::new("caf\u{e9}"),
        sensitivity,
    )
    .unwrap();
    assert_eq!(looked, Look::Coinciding);
}
