//! `.delocal/tmp/` (DESIGN.md §7.5 "Fetching a file" step 3 and "Crash
//! safety", §11): where a transfer lands before its commit renames it in,
//! and where the host makes the other files it renames at once.
//!
//! **Names.** A transfer is `<hash-prefix>-<unique>`: the first
//! [`HASH_PREFIX`] hex digits of the content it is fetching, so a want can
//! find the temp file an earlier attempt left and resume from its size.
//! Anything else the host makes here (a symlink on its way in, the
//! filesystem probe's files) starts with a word, never with sixteen hex
//! digits and a dash, so no want claims it.
//!
//! **Unclaimed files go at start** ([`remove_unclaimed`]). A crash leaves
//! temp files behind; the commit journal's undo leaves the one its row
//! names for resumption (§7.5). After the undo, every file here that no
//! want claims is removed, so `tmp/` holds only transfers that will resume.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use delocal_engine::ContentHash;

use crate::fs::{FileKind, Fs};
use crate::reach::unique;

/// The directory, relative to the folder root.
pub const DIR: &str = ".delocal/tmp";

/// How many hex digits of its content's hash a transfer's name starts with.
pub const HASH_PREFIX: usize = 16;

/// Make `.delocal/tmp/` if it is missing. `.delocal/` itself is never made
/// here: it holds the root guard's marker (§7.3), and a commit into a folder
/// whose `.delocal/` is gone (a disk not mounted, say) must fail rather
/// than make one.
pub fn ensure(fs: &dyn Fs) -> io::Result<()> {
    match fs.lstat(Path::new(DIR)) {
        Ok(stat) if stat.kind == FileKind::Dir => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{DIR} is not a directory"),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match fs.create_dir(Path::new(DIR)) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        },
        Err(e) => Err(e),
    }
}

/// A fresh name for a transfer of `hash`, relative to the folder root.
pub fn transfer(hash: &ContentHash) -> PathBuf {
    let hex = hex(hash.as_bytes());
    Path::new(DIR).join(format!("{}-{}", &hex[..HASH_PREFIX], unique()))
}

/// A fresh name for something the host makes in `tmp/` and renames at
/// once, which no want ever claims: `<what>-<unique>`. `what` must start
/// with a letter that is not a hex digit, which every caller's does.
pub fn scratch(what: &str) -> PathBuf {
    Path::new(DIR).join(format!("{what}-{}", unique()))
}

/// Whether the name of a file in `tmp/` is a transfer of one of `hashes`.
pub fn claimed(name: &OsStr, hashes: &BTreeSet<ContentHash>) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some((prefix, _)) = name.split_once('-') else {
        return false;
    };
    prefix.len() == HASH_PREFIX
        && hashes
            .iter()
            .any(|hash| hex(hash.as_bytes()).starts_with(prefix))
}

/// Remove every entry of `tmp/` whose name `keep` does not keep (§7.5
/// "Crash safety"), at daemon start, after the commit journal's undo.
/// Returns the names removed. A directory there (nothing the host makes) is
/// removed only if empty. A `tmp/` that is not there has nothing to remove.
pub fn remove_unclaimed(fs: &dyn Fs, keep: &dyn Fn(&OsStr) -> bool) -> io::Result<Vec<OsString>> {
    let dir = Path::new(DIR);
    let names = match fs.read_dir(dir) {
        Ok(names) => names,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut removed = Vec::new();
    for name in names {
        if keep(&name) {
            continue;
        }
        let path = dir.join(&name);
        let gone = match fs.lstat(&path) {
            Ok(stat) if stat.kind == FileKind::Dir => fs.remove_dir(&path),
            Ok(_) => fs.remove_file(&path),
            Err(e) => Err(e),
        };
        match gone {
            Ok(()) => removed.push(name),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use delocal_engine::path::RESERVED_DIR;

    use super::*;
    use crate::fs::{Folder, RealFolder};

    fn hash(first: u8) -> ContentHash {
        let mut bytes = [0x11; 32];
        bytes[0] = first;
        ContentHash::from_bytes(bytes)
    }

    #[test]
    fn a_transfer_is_named_by_its_hash_and_claimed_by_it() {
        let a = transfer(&hash(0xab));
        let name = a.file_name().unwrap();
        assert!(a.starts_with(DIR));
        assert!(name.to_str().unwrap().starts_with("ab11111111111111-"));
        assert_ne!(a, transfer(&hash(0xab)), "every name is fresh");
        let wanted: BTreeSet<_> = [hash(0xab)].into();
        assert!(claimed(name, &wanted));
        assert!(!claimed(name, &[hash(0xac)].into()));
        let link = scratch("link");
        assert!(!claimed(link.file_name().unwrap(), &wanted));
        assert!(!claimed(OsStr::new("ab11111111111111"), &wanted));
    }

    #[test]
    fn only_unclaimed_entries_are_removed_at_start() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = RealFolder::new(tmp.path()).open().unwrap();
        // Nothing to do without a tmp/ (or a .delocal/, which is not made).
        assert!(remove_unclaimed(&*fs, &|_| false).unwrap().is_empty());
        assert!(ensure(&*fs).is_err());

        std::fs::create_dir(tmp.path().join(RESERVED_DIR)).unwrap();
        ensure(&*fs).unwrap();
        ensure(&*fs).unwrap();
        let wanted: BTreeSet<_> = [hash(0xab)].into();
        let kept = transfer(&hash(0xab));
        let stale = transfer(&hash(0xcd));
        let link = scratch("link");
        for file in [&kept, &stale] {
            std::fs::write(tmp.path().join(file), "partial").unwrap();
        }
        std::os::unix::fs::symlink("anywhere", tmp.path().join(&link)).unwrap();
        std::fs::create_dir_all(tmp.path().join(DIR).join("full/x")).unwrap();
        std::fs::create_dir(tmp.path().join(DIR).join("empty")).unwrap();

        let mut removed = remove_unclaimed(&*fs, &|name| claimed(name, &wanted)).unwrap();
        removed.sort();
        let mut expected: Vec<OsString> = vec![
            stale.file_name().unwrap().into(),
            link.file_name().unwrap().into(),
            "empty".into(),
        ];
        expected.sort();
        assert_eq!(removed, expected);
        let mut left: Vec<OsString> = fs.read_dir(Path::new(DIR)).unwrap();
        left.sort();
        let expected: [OsString; 2] = [kept.file_name().unwrap().into(), "full".into()];
        assert_eq!(left, expected);
    }
}
