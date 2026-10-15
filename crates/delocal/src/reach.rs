//! Reaching an index path on disk (DESIGN.md §7.3, "Names on disk",
//! "Reaching a path" and "Mount points"; §7.5 step 6): what a commit, and a
//! single observation, need between the index path the engine names and the
//! entry the user has.
//!
//! **Names on disk.** An index path is NFC; the name on disk may not be. The
//! scan keeps every pair whose name differs ([`DiskNames`], `disk_names` in
//! §11), and [`disk_path`] builds a path from them, component by component,
//! so a commit addresses the file the user has. A path with no pair is
//! written as it is, which on Linux creates an NFC name.
//!
//! **Filesystems that fold names.** A case-insensitive or
//! normalisation-insensitive filesystem answers a `stat` by name for a
//! differently spelt file, so the guard confirms the exact name through the
//! parent's listing ([`look`]): an entry counts as the path only if its
//! name maps to the path's index name. Which kind of filesystem holds a
//! folder is [`Sensitivity::probe`]d once, in `.delocal/tmp/`, since APFS is
//! normally both and ext4 neither, and a folder can be on either. A live
//! record at another path the filesystem would treat as this one, or as one
//! of its ancestors, is a case collision ([`collision`], §7.6).
//!
//! **Never onto another device.** [`walk`] states every directory from the
//! root down to a path's parent, one component at a time through the
//! filesystem layer, and stops at the first one on another device than the
//! root, a mount point (draft 48), or that is not a directory. A commit
//! never writes beneath a mount point.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use delocal_engine::{ContentHash, Kind, Observed, RelPath};
use unicode_normalization::UnicodeNormalization;

use crate::fs::{FileKind, Fs, ParentNotADirectory, Stat};
use crate::names::{self, IndexName, Name};
use crate::scan::EXEC_BIT;
use crate::scan::hash::hash_target;

/// The names on disk that are not their index path's bytes (§7.3): the
/// last component of each path, as its directory lists it. The host keeps
/// them in `disk_names` (§11).
pub trait DiskNames: Send + Sync {
    /// The name on disk of `path`'s last component, if it is not the index
    /// path's own.
    fn name(&self, path: &RelPath) -> Option<&OsStr>;
}

impl DiskNames for BTreeMap<RelPath, OsString> {
    fn name(&self, path: &RelPath) -> Option<&OsStr> {
        self.get(path).map(OsString::as_os_str)
    }
}

/// The path on disk of the index path `path`, relative to the folder root:
/// each component's name on disk, or its index name where no pair says
/// otherwise.
pub fn disk_path(path: &RelPath, names: &dyn DiskNames) -> PathBuf {
    let mut disk = PathBuf::new();
    let mut prefix: Option<RelPath> = None;
    for component in path.components() {
        let index = match &prefix {
            Some(parent) => parent.join(component),
            None => RelPath::new(component),
        };
        let Ok(index) = index else {
            // A component of a valid path is a valid path; this keeps the
            // function total.
            disk.push(component);
            continue;
        };
        match names.name(&index) {
            Some(name) => disk.push(name),
            None => disk.push(component),
        }
        prefix = Some(index);
    }
    disk
}

/// How the filesystem that holds a folder compares names (§7.3, §7.5 step
/// 6): whether it treats two spellings that differ by case, or by Unicode
/// normalisation, as one name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sensitivity {
    pub folds_case: bool,
    pub folds_normalisation: bool,
}

impl Sensitivity {
    /// A filesystem that compares names byte for byte, as Linux's usually do.
    pub const EXACT: Self = Self {
        folds_case: false,
        folds_normalisation: false,
    };

    /// Whether a `stat` by name can answer for a differently spelt file, so
    /// the guard must confirm the name through the parent's listing.
    pub fn folds(self) -> bool {
        self.folds_case || self.folds_normalisation
    }

    /// Find out by asking the filesystem: in `.delocal/tmp/`, make a file
    /// with a lowercase name and another with an NFC name, and see whether
    /// the uppercase and the NFD spellings find them. Both files are
    /// removed again, and one left by a crash is an unclaimed temp file,
    /// which the next start removes (§7.5).
    pub fn probe(fs: &dyn Fs) -> io::Result<Self> {
        let tmp = Path::new(crate::tmp::DIR);
        let id = unique();
        let lower = tmp.join(format!("probe-{id}-case"));
        let upper = tmp.join(format!("PROBE-{}-CASE", id.to_uppercase()));
        let nfc = tmp.join(format!("probe-{id}-\u{e9}"));
        let nfd = tmp.join(format!("probe-{id}-e\u{301}"));
        let folds = |made: &Path, other: &Path| -> io::Result<bool> {
            drop(fs.create_new(made)?);
            let found = match fs.lstat(other) {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            };
            fs.remove_file(made)?;
            found
        };
        Ok(Self {
            folds_case: folds(&lower, &upper)?,
            folds_normalisation: folds(&nfc, &nfd)?,
        })
    }

    /// Whether this filesystem treats the index names `a` and `b`, single
    /// components, as one name. Index names are NFC, so two different ones
    /// are never the same name by normalisation alone; only case can join
    /// them.
    pub fn same_name(self, a: &str, b: &str) -> bool {
        a == b || (self.folds_case && fold(a) == fold(b))
    }
}

/// A name as a filesystem that ignores case compares it: lower case, in
/// NFC. Rust's lower case is Unicode's default, close to what APFS folds.
fn fold(name: &str) -> String {
    name.to_lowercase().nfc().collect()
}

/// A name no other operation of this process has used: the process id, the
/// clock, and a counter. For files this host makes in `.delocal/tmp/`,
/// which it creates refusing anything already there, so uniqueness is a
/// convenience, not what safety rests on.
pub fn unique() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{:x}{nanos:08x}{n:x}", std::process::id())
}

/// The live record at another path that this filesystem would treat as
/// `path` or as one of its ancestors (§7.5 step 6, §7.6), if there is one:
/// writing `path` would then land in, or on, that record's file. Only a
/// filesystem that folds case can have one, since index paths are NFC.
pub fn collision<'a>(
    path: &RelPath,
    live: impl IntoIterator<Item = &'a RelPath>,
    sensitivity: Sensitivity,
) -> Option<RelPath> {
    if !sensitivity.folds_case {
        return None;
    }
    let ours: Vec<&str> = path.components().collect();
    live.into_iter()
        .find(|other| {
            let theirs: Vec<&str> = other.components().collect();
            theirs.len() <= ours.len()
                && theirs != ours[..theirs.len()]
                && theirs
                    .iter()
                    .zip(&ours)
                    .all(|(a, b)| sensitivity.same_name(a, b))
        })
        .cloned()
}

/// Where the walk from the root to a path's parent stopped (§7.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Walk {
    /// Every directory above the path is on disk, on the root's device.
    Reached,
    /// `first` is the highest ancestor not on disk: the path is absent, and
    /// a write creates `first` and everything between it and the path.
    Missing { first: RelPath },
    /// The ancestor `at` is not a directory: the path cannot be there, and
    /// cannot be written (§7.3, a commit whose parent is not a directory).
    NotADirectory { at: RelPath },
    /// The ancestor `at` is on another device than the root: a mount point,
    /// beneath which nothing is observed or written (§7.3, draft 48).
    Crossed { at: RelPath },
}

/// Walk from the root to the parent of `path`, whose path on disk is
/// `disk`, stating each directory on the way: see [`Walk`]. `device` is the
/// root's. An error other than a missing ancestor or one that is not a
/// directory (permission denied, an I/O error) is returned as it is.
pub fn walk(fs: &dyn Fs, path: &RelPath, disk: &Path, device: u64) -> io::Result<Walk> {
    let mut index: Option<RelPath> = None;
    let mut on_disk = PathBuf::new();
    let ancestors = path.depth().saturating_sub(1);
    for (component, name) in path.components().zip(disk.iter()).take(ancestors) {
        let here = match &index {
            Some(parent) => parent.join(component),
            None => RelPath::new(component),
        }
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        on_disk.push(name);
        match fs.lstat(&on_disk) {
            Ok(stat) if stat.kind != FileKind::Dir => {
                return Ok(Walk::NotADirectory { at: here });
            }
            Ok(stat) if stat.dev != device => return Ok(Walk::Crossed { at: here }),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Walk::Missing { first: here });
            }
            Err(e) if ParentNotADirectory::of(&e).is_some() => {
                return Ok(Walk::NotADirectory { at: here });
            }
            Err(e) => return Err(e),
        }
        index = Some(here);
    }
    Ok(Walk::Reached)
}

/// What is at a path, as the guard and the fast path see it (§7.5 step 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Look {
    /// Nothing at the path, and nothing answering for its name either.
    Absent,
    /// The entry at the path: its name on disk, its `stat`, and what the
    /// fast path sees of it ([`Observed`] with a symlink's target hashed and
    /// a file's hash not computed). `None` for a FIFO, a socket or a
    /// device, which is not an entry (§3.1).
    Present {
        name: OsString,
        stat: Stat,
        seen: Option<Observed>,
    },
    /// No entry at the path, but the filesystem answers for its name with
    /// `name`, which is another path's: a differently cased file on a
    /// filesystem that folds case. A rename to the path would find it taken.
    Taken { name: OsString },
    /// Two names in the parent map to this path (§7.3): neither can be
    /// observed at it.
    Coinciding,
}

/// Look at `path`, whose parent is at `parent` on disk and whose own name on
/// disk is `name` as far as the host knows (a `disk_names` pair, or the
/// index name). On a filesystem that folds names (`sensitivity`), a `stat`
/// that finds something is confirmed through the parent's listing (§7.5 step
/// 6): the entry is the path only if its name maps to the path's index name,
/// and it is then addressed by that name. The parent must be on disk; see
/// [`walk`].
pub fn look(
    fs: &dyn Fs,
    parent: &Path,
    path: &RelPath,
    name: &OsStr,
    sensitivity: Sensitivity,
) -> io::Result<Look> {
    let stat = match fs.lstat(&parent.join(name)) {
        Ok(stat) => stat,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Look::Absent),
        Err(e) if ParentNotADirectory::of(&e).is_some() => return Ok(Look::Absent),
        Err(e) => return Err(e),
    };
    let name = if sensitivity.folds() {
        match exact(fs, parent, path)? {
            Exact::One(found) => found,
            Exact::Coinciding => return Ok(Look::Coinciding),
            Exact::None => {
                return Ok(Look::Taken {
                    name: taken_by(fs, parent, path, sensitivity)?.unwrap_or(name.to_owned()),
                });
            }
        }
    } else {
        name.to_owned()
    };
    // On a filesystem that folds names the `stat` above answered for the
    // spelling asked, which is the same entry; state it by its own name so
    // that what follows reads the entry found.
    let stat = if sensitivity.folds() {
        match fs.lstat(&parent.join(&name)) {
            Ok(stat) => stat,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Look::Absent),
            Err(e) => return Err(e),
        }
    } else {
        stat
    };
    let seen = match stat.kind {
        FileKind::File => Some(Observed {
            kind: Kind::File,
            size: stat.size,
            mtime_ns: stat.mtime_ns,
            exec: stat.mode & EXEC_BIT != 0,
            hash: ContentHash::EMPTY,
        }),
        FileKind::Dir => Some(Observed {
            kind: Kind::Dir,
            size: 0,
            mtime_ns: 0,
            exec: false,
            hash: ContentHash::EMPTY,
        }),
        FileKind::Symlink => {
            let target = match fs.read_link(&parent.join(&name)) {
                Ok(target) => target,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Look::Absent),
                Err(e) => return Err(e),
            };
            let target = target.as_os_str().as_bytes();
            Some(Observed {
                kind: Kind::Symlink,
                size: target.len() as u64,
                mtime_ns: 0,
                exec: false,
                hash: hash_target(target),
            })
        }
        FileKind::Other => None,
    };
    Ok(Look::Present { name, stat, seen })
}

/// What the parent's listing says about a path's name.
enum Exact {
    /// One name in the parent maps to the path: this one.
    One(OsString),
    /// None does.
    None,
    /// Two or more do (§7.3, coinciding names).
    Coinciding,
}

/// The names in `parent` that map to `path`'s index name (§7.3).
fn exact(fs: &dyn Fs, parent: &Path, path: &RelPath) -> io::Result<Exact> {
    let listed = fs.read_dir(parent)?;
    let mut found = names::dir(path.parent().as_ref(), &listed)
        .into_iter()
        .filter_map(|(disk, name)| match name {
            Name::Path(IndexName { path: index, .. }) if index == *path => Some(Ok(disk)),
            Name::Unobservable(names::Unobservable::Coincides { path: index })
                if index == *path =>
            {
                Some(Err(()))
            }
            _ => None,
        });
    Ok(match (found.next(), found.next()) {
        (None, _) => Exact::None,
        (Some(Ok(one)), None) => Exact::One(one),
        _ => Exact::Coinciding,
    })
}

/// The name in `parent` that this filesystem treats as `path`'s, which is
/// not `path`'s own, if it can be found in the listing.
fn taken_by(
    fs: &dyn Fs,
    parent: &Path,
    path: &RelPath,
    sensitivity: Sensitivity,
) -> io::Result<Option<OsString>> {
    let ours = path.file_name();
    Ok(fs.read_dir(parent)?.into_iter().find(|disk| {
        disk.to_str()
            .is_some_and(|name| sensitivity.same_name(&name.nfc().collect::<String>(), ours))
    }))
}

#[cfg(test)]
mod tests;
