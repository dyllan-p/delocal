//! Names on disk (DESIGN.md §7.1, §7.3): how a name that `readdir` returns
//! becomes an index path, or why it cannot.
//!
//! The scan works from the names a directory lists and never asks the
//! filesystem for an index path by name, because a normalisation-insensitive
//! or case-insensitive filesystem would answer for a different file. A name
//! becomes the last component of an index path by NFC normalisation. It is
//! **unobservable** instead (nothing is synced for it, `status` names it,
//! and a scan reports it `Skipped`) if it is not valid UTF-8, if its NFC form
//! is over 255 bytes, if the index path would be over 4,096 bytes, or if
//! another name in the same directory maps to the same index path.
//!
//! Where a name and its NFC form differ, [`IndexName::differs`] says so: the
//! store keeps those pairs in `disk_names` (§11) so that commits and the
//! guard address the file the user has.
//!
//! Everything here is a pure function of names: nothing touches the disk,
//! and nothing stats an index path.
//!
//! Not here: two casings of one name on a case-sensitive disk, which §7.3
//! makes unobservable only for a folder that also lives on macOS. That needs
//! to know the other members' platforms, which the host learns in Phase 3.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use delocal_engine::RelPath;
use delocal_engine::path::{MAX_COMPONENT_LEN, RelPathError};
use unicode_normalization::{UnicodeNormalization, is_nfc};

/// The longest index path, in bytes (§7.1).
pub const MAX_PATH_LEN: usize = 4096;

/// What one name in a directory is to the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Name {
    /// The name is an entry at this index path.
    Path(IndexName),
    /// `.delocal` at the folder root, which §7.3 always ignores. It is not
    /// an entry, and not unobservable either, so `status` never names it.
    Reserved,
    /// Nothing is synced for the name, and `status` names it (§7.3).
    Unobservable(Unobservable),
}

/// A name that is an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexName {
    /// The index path: the parent's, joined with the name's NFC form.
    pub path: RelPath,
    /// The name on disk is not the NFC form's bytes, so the store keeps the
    /// pair in `disk_names` (§11, §7.3).
    pub differs: bool,
}

/// Why a name is unobservable (§7.3). `status` counts skipped paths by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unobservable {
    /// The name is not valid UTF-8, so it has no index path at all.
    NotUtf8,
    /// The name's NFC form is `len` bytes, over 255 (§7.1). NFC can lengthen
    /// a name, so no peer could create it even when the name on disk fits.
    NameTooLong { len: usize },
    /// The index path would be `len` bytes, over [`MAX_PATH_LEN`] (§7.1).
    PathTooLong { len: usize },
    /// Another name in the same directory has the same index path: two
    /// normalisation forms of one name, on a disk that keeps both.
    Coincides { path: RelPath },
    /// The name breaks another rule of an index path (§7.1): it is empty,
    /// `.` or `..`, or holds a `/` or a NUL. `readdir` returns none of these;
    /// this keeps [`name`] total rather than panicking.
    NotAName(RelPathError),
}

/// Map one name from `readdir` in the directory at `parent` (`None` for the
/// folder root) to what it is to the index. Coincidence with the directory's
/// other names is [`dir`]'s job; on its own a name never coincides.
pub fn name(parent: Option<&RelPath>, disk: &OsStr) -> Name {
    let Some(disk) = disk.to_str() else {
        return Name::Unobservable(Unobservable::NotUtf8);
    };
    // One component, or joining it would make a deeper path; `RelPath::join`
    // refuses a slash the same way.
    if disk.contains('/') {
        return Name::Unobservable(Unobservable::NotAName(RelPathError::EmptyComponent));
    }
    let nfc: Cow<'_, str> = if is_nfc(disk) {
        Cow::Borrowed(disk)
    } else {
        Cow::Owned(disk.nfc().collect())
    };
    if nfc.len() > MAX_COMPONENT_LEN {
        return Name::Unobservable(Unobservable::NameTooLong { len: nfc.len() });
    }
    let path = match parent {
        Some(parent) => format!("{parent}/{nfc}"),
        None => nfc.to_string(),
    };
    if path.len() > MAX_PATH_LEN {
        return Name::Unobservable(Unobservable::PathTooLong { len: path.len() });
    }
    match RelPath::new(path) {
        Ok(path) => Name::Path(IndexName {
            path,
            differs: nfc != disk,
        }),
        Err(RelPathError::Reserved) => Name::Reserved,
        Err(e) => Name::Unobservable(Unobservable::NotAName(e)),
    }
}

/// Map every name of one directory, as [`Fs::read_dir`] listed them, and
/// mark the names whose index paths coincide as unobservable. The result
/// keeps the order of `names`.
///
/// [`Fs::read_dir`]: crate::fs::Fs::read_dir
pub fn dir(parent: Option<&RelPath>, names: &[OsString]) -> Vec<(OsString, Name)> {
    let mut mapped: Vec<(OsString, Name)> = names
        .iter()
        .map(|disk| (disk.clone(), name(parent, disk)))
        .collect();
    let coinciding = coinciding(&mapped);
    for (_, name) in &mut mapped {
        if let Name::Path(index) = name
            && coinciding.contains_key(&index.path)
        {
            let path = index.path.clone();
            *name = Name::Unobservable(Unobservable::Coincides { path });
        }
    }
    mapped
}

/// The index paths that two or more names of one directory map to, each
/// with those names in the order given. Every name listed is unobservable
/// (§7.3). A name that maps to no path, or to a path no other name maps
/// to, is not listed.
pub fn coinciding(names: &[(OsString, Name)]) -> BTreeMap<RelPath, Vec<OsString>> {
    let mut by_path: BTreeMap<RelPath, Vec<OsString>> = BTreeMap::new();
    for (disk, name) in names {
        if let Name::Path(index) = name {
            by_path
                .entry(index.path.clone())
                .or_default()
                .push(disk.clone());
        }
    }
    by_path.retain(|_, disks| disks.len() > 1);
    by_path
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn path(s: &str, differs: bool) -> Name {
        Name::Path(IndexName {
            path: p(s),
            differs,
        })
    }

    fn unobservable(reason: Unobservable) -> Name {
        Name::Unobservable(reason)
    }

    /// A parent directory whose index path is `len` bytes: 255-byte
    /// components, each taking 256 with its slash, then the rest.
    fn parent_of_len(len: usize) -> RelPath {
        let mut components = vec!["p".repeat(255); len / 256];
        components.push("p".repeat(len % 256));
        let parent = RelPath::new(components.join("/")).unwrap();
        assert_eq!(parent.as_str().len(), len);
        parent
    }

    #[test]
    fn table() {
        let docs = p("docs");
        let long_parent = parent_of_len(4090);
        // U+0344 is two bytes and its NFC form, U+0308 U+0301, is four:
        // 127 of them are 254 bytes on disk and 508 as an index path.
        let lengthened = "\u{344}".repeat(127);
        let name_255 = "n".repeat(255);
        let name_256 = "n".repeat(256);
        let cases: Vec<(Option<&RelPath>, &[u8], Name)> = vec![
            (None, b"report.txt", path("report.txt", false)),
            (Some(&docs), b"report.txt", path("docs/report.txt", false)),
            // NFC on disk is its own index path.
            (None, "café".as_bytes(), path("café", false)),
            // NFD on disk maps to the NFC path, and the pair differs.
            (None, "cafe\u{301}".as_bytes(), path("café", true)),
            (
                Some(&docs),
                "cafe\u{301}".as_bytes(),
                path("docs/café", true),
            ),
            // Combining marks out of canonical order are reordered.
            (
                None,
                "e\u{301}\u{323}".as_bytes(),
                path("\u{1eb9}\u{301}", true),
            ),
            // Hangul jamo compose into one syllable.
            (
                None,
                "\u{1112}\u{1161}\u{11ab}".as_bytes(),
                path("\u{d55c}", true),
            ),
            // Latin-1 é is not UTF-8.
            (None, b"caf\xe9", unobservable(Unobservable::NotUtf8)),
            (
                Some(&docs),
                b"\xff\xfe",
                unobservable(Unobservable::NotUtf8),
            ),
            (None, name_255.as_bytes(), path(&name_255, false)),
            (
                None,
                name_256.as_bytes(),
                unobservable(Unobservable::NameTooLong { len: 256 }),
            ),
            (
                None,
                lengthened.as_bytes(),
                unobservable(Unobservable::NameTooLong { len: 508 }),
            ),
            // 4,090 + "/" + 5 bytes is exactly the limit; one more is over.
            (
                Some(&long_parent),
                b"12345",
                path(&format!("{long_parent}/12345"), false),
            ),
            (
                Some(&long_parent),
                b"123456",
                unobservable(Unobservable::PathTooLong { len: 4097 }),
            ),
            // The limit is on the index path. "café" in NFD is 6 bytes and
            // 5 composed, so here it is over the limit on disk (4,097) and
            // at it as an index path (4,096): observable.
            (
                Some(&long_parent),
                "cafe\u{301}".as_bytes(),
                path(&format!("{long_parent}/café"), true),
            ),
            (None, b".delocal", Name::Reserved),
            (Some(&docs), b".delocal", path("docs/.delocal", false)),
            (
                None,
                b"..",
                unobservable(Unobservable::NotAName(RelPathError::DotDotComponent)),
            ),
            (
                None,
                b"a/b",
                unobservable(Unobservable::NotAName(RelPathError::EmptyComponent)),
            ),
        ];
        for (parent, disk, expected) in cases {
            assert_eq!(
                name(parent, OsStr::from_bytes(disk)),
                expected,
                "{:?} in {parent:?}",
                String::from_utf8_lossy(disk)
            );
        }
    }

    #[test]
    fn a_coinciding_pair_is_unobservable_and_the_rest_are_not() {
        let names: Vec<OsString> = [
            "cafe\u{301}".as_bytes(),
            "café".as_bytes(),
            b"plain",
            b"caf\xe9",
            // Two forms, neither of them NFC, of the same third name.
            "e\u{301}\u{323}".as_bytes(),
            "e\u{323}\u{301}".as_bytes(),
        ]
        .into_iter()
        .map(|b| OsStr::from_bytes(b).to_os_string())
        .collect();
        let cafe = Unobservable::Coincides { path: p("café") };
        let dotted = Unobservable::Coincides {
            path: p("\u{1eb9}\u{301}"),
        };
        let mapped = dir(None, &names);
        let expected = vec![
            unobservable(cafe.clone()),
            unobservable(cafe),
            path("plain", false),
            unobservable(Unobservable::NotUtf8),
            unobservable(dotted.clone()),
            unobservable(dotted),
        ];
        assert_eq!(
            mapped.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            mapped.iter().map(|(d, _)| d.clone()).collect::<Vec<_>>(),
            names,
            "the names on disk come back as they were, in order"
        );

        let groups = coinciding(
            &names
                .iter()
                .map(|d| (d.clone(), name(None, d)))
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            groups.into_iter().collect::<Vec<_>>(),
            [
                (p("café"), vec![names[0].clone(), names[1].clone()]),
                (
                    p("\u{1eb9}\u{301}"),
                    vec![names[4].clone(), names[5].clone()]
                ),
            ]
        );
    }

    /// The names the real filesystem lists, mapped. Linux filesystems keep
    /// both normalisation forms of a name, and bytes that are not UTF-8
    /// (except in a case-folded directory, which a temp dir is not); APFS
    /// (macOS) refuses a second form of a name it already has, and any name
    /// that is not UTF-8.
    #[test]
    fn names_as_the_real_filesystem_lists_them() {
        use crate::fs::{Folder, RealFolder};

        let tmp = tempfile::tempdir().unwrap();
        let fs = RealFolder::new(tmp.path()).open().unwrap();
        let create = |name: &[u8]| fs.create_new(std::path::Path::new(OsStr::from_bytes(name)));
        create(b"plain").unwrap();
        create("café".as_bytes()).unwrap();
        let nfd = create("cafe\u{301}".as_bytes());
        let latin1 = create(b"caf\xe9");

        let listed = fs.read_dir(std::path::Path::new("")).unwrap();
        let mapped: Vec<Name> = dir(None, &listed).into_iter().map(|(_, n)| n).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(
                nfd.map(|_| ()).unwrap_err().kind(),
                std::io::ErrorKind::AlreadyExists
            );
            assert!(latin1.is_err());
            assert_eq!(mapped, [path("café", false), path("plain", false)]);
        } else {
            nfd.unwrap();
            latin1.unwrap();
            let cafe = Unobservable::Coincides { path: p("café") };
            // Sorted by bytes, the fourth deciding: "cafe\u{301}" (0x65),
            // "café" (0xC3), "caf\xe9" (0xE9), then "plain".
            assert_eq!(
                mapped,
                [
                    unobservable(cafe.clone()),
                    unobservable(cafe),
                    unobservable(Unobservable::NotUtf8),
                    path("plain", false),
                ]
            );
        }
    }
}
