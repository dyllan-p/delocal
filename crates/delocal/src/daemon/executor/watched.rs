//! The executor's half of the watcher (DESIGN.md §7.3, draft 67): the
//! paths its events named, once quiet, observed as a scan would observe
//! them.
//!
//! Each path is observed by [`Commit::observe`], the scanner's single-path
//! observation behind the root guard, and reported with the time of its
//! own `stat` (draft 56). Around that:
//!
//! - **An open journal row** (draft 64): a path with one, or beneath one,
//!   is not looked at and nothing is reported for it, since its disk is in
//!   a state no record describes.
//! - **Another device** (draft 48): a path a rule ignores, or one at or
//!   beneath a mount point, is reported `Skipped { Ignored }` by the
//!   observation, and the watcher reports nothing for it and never walks
//!   into it.
//! - **Another spelling.** Before a path is reported absent, its parent is
//!   listed: if a name there maps to the path but is not its index name
//!   (§7.3, names on disk), the path is observed again through that name.
//!   An event may name a path by one spelling while the file has another.
//! - **The root guard** (§7.3). Nothing is observed in a folder whose
//!   marker is gone, and before anything absent is reported the marker is
//!   checked again. A folder that fails either check reports nothing, and
//!   the engine task treats it as after an aborted scan.
//! - **Directories, whole.** No event names what moves with a directory:
//!   inotify reports a directory renamed in or out, or removed whole, and
//!   nothing beneath it. So a directory that is gone takes every tracked
//!   path beneath it with it, each observed too, children before parents;
//!   and a directory that is there is walked, whatever the event said: each
//!   name in it observed, parents before children, then every tracked path
//!   beneath it that the walk did not reach. A directory moved in brings its
//!   contents, and one moved away and made again at the same path loses the
//!   ones the index knew.
//!
//! A path named twice in one job is observed once. An empty job checks the
//! root guard and nothing else: an event named the root itself.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use delocal_engine::{Entry, Event, FolderId, Kind, RelPath, ScanState, SkipReason, Timestamp};

use super::super::clock::Clock;
use super::super::watch::Seen;
use super::Kept;
use crate::commit::{self, Commit, StoreJournal};
use crate::names::{self, IndexName, Name};
use crate::reach::{self, DiskNames};
use crate::scan::OpenRows;

/// The folder's names on disk that are not their index paths', and those
/// this job has learned since: from the events' paths, and from the
/// directories it listed.
struct Overlay<'a> {
    base: &'a BTreeMap<RelPath, OsString>,
    learned: BTreeMap<RelPath, OsString>,
}

impl DiskNames for Overlay<'_> {
    fn name(&self, path: &RelPath) -> Option<&OsStr> {
        self.learned
            .get(path)
            .or_else(|| self.base.get(path))
            .map(OsString::as_os_str)
    }
}

/// The root guard tripped: the folder's marker is gone.
pub(super) struct Gone;

/// One observation to report, with the live record it was compared
/// against, for the engine task's check of an `Unchanged` (§7.3).
pub(super) type Observation = (Event, Option<Entry>);

/// One job's worth of the watcher's paths.
struct Watched<'a> {
    id: FolderId,
    kept: &'a Kept,
    journal: &'a StoreJournal<'a>,
    clock: &'a Clock,
    names: Overlay<'a>,
    /// Paths already observed in this job.
    done: BTreeSet<RelPath>,
    /// Directory listings made in this job, by the directory on disk.
    listings: BTreeMap<PathBuf, Option<Vec<(OsString, Name)>>>,
    out: Vec<Observation>,
}

/// Observe what `seen` names in `id` (see the module docs), or `Gone` if
/// the root guard tripped, in which case nothing is to be reported.
pub(super) fn observe(
    id: FolderId,
    kept: &Kept,
    journal: &StoreJournal<'_>,
    clock: &Clock,
    seen: Vec<Seen>,
) -> Result<Vec<Observation>, Gone> {
    if !commit::marked(&*kept.folder) {
        return Err(Gone);
    }
    let mut watched = Watched {
        id,
        kept,
        journal,
        clock,
        names: Overlay {
            base: &kept.names,
            learned: BTreeMap::new(),
        },
        done: BTreeSet::new(),
        listings: BTreeMap::new(),
        out: Vec::new(),
    };
    let mut paths = Vec::with_capacity(seen.len());
    for one in seen {
        for (path, name) in one.names {
            watched.names.learned.insert(path, name);
        }
        paths.push(one.path);
    }
    for path in paths {
        watched.path(&path)?;
    }
    // Before anything absent goes to the engine, the marker once more: a
    // folder deleted wholesale while this job ran is the guard's (§7.3).
    let absent = watched.out.iter().any(|(event, _)| {
        matches!(
            event,
            Event::Scanned {
                state: ScanState::Absent,
                ..
            }
        )
    });
    if absent && !commit::marked(&*kept.folder) {
        return Err(Gone);
    }
    Ok(watched.out)
}

impl Watched<'_> {
    fn commit(&self) -> Commit<'_> {
        Commit {
            id: self.id,
            folder: &*self.kept.folder,
            records: &*self.kept.records,
            names: &self.names,
            journal: self.journal,
            clock: self.clock,
            stability: &self.kept.stability,
            disk: &self.kept.disk,
        }
    }

    fn push(&mut self, path: &RelPath, state: ScanState, at: Timestamp) {
        let compared = self.kept.records.get(path).cloned();
        let event = Event::Scanned {
            folder: self.id,
            path: path.clone(),
            state,
            at,
        };
        self.out.push((event, compared));
    }

    /// Observe `path`, which an event named, and what came or went with it.
    fn path(&mut self, path: &RelPath) -> Result<(), Gone> {
        if !self.done.insert(path.clone()) || self.kept.disk.is_open(path) {
            return Ok(());
        }
        let (mut state, mut at) = self.commit().observe(path);
        if state == ScanState::Absent
            && let Some(name) = self.other_spelling(path)
        {
            self.names.learned.insert(path.clone(), name);
            (state, at) = self.commit().observe(path);
        }
        let tracked_dir = self
            .kept
            .records
            .get(path)
            .is_some_and(|record| record.kind == Kind::Dir);
        match &state {
            ScanState::Skipped {
                reason: SkipReason::Ignored,
            } => Ok(()),
            // An open row that came between the check above and the look.
            ScanState::Skipped { .. } if self.kept.disk.is_open(path) => Ok(()),
            ScanState::Absent => {
                if !commit::marked(&*self.kept.folder) {
                    return Err(Gone);
                }
                if tracked_dir {
                    for below in beneath(&self.kept.records, path).into_iter().rev() {
                        self.went_with(&below);
                    }
                }
                self.push(path, state, at);
                Ok(())
            }
            ScanState::Observed(seen) if seen.kind == Kind::Dir => {
                self.push(path, state, at);
                self.subtree(path)
            }
            ScanState::Unchanged if tracked_dir => {
                self.push(path, state, at);
                self.subtree(path)
            }
            _ => {
                self.push(path, state, at);
                Ok(())
            }
        }
    }

    /// Observe `path`, tracked beneath a directory that is gone.
    fn went_with(&mut self, path: &RelPath) {
        if !self.done.insert(path.clone()) || self.kept.disk.is_open(path) {
            return;
        }
        let (state, at) = self.commit().observe(path);
        if !matches!(
            state,
            ScanState::Skipped {
                reason: SkipReason::Ignored
            }
        ) {
            self.push(path, state, at);
        }
    }

    /// Observe what is beneath the directory `dir`, which may have been
    /// replaced: every name in it now, then every tracked path beneath it
    /// that those did not reach, children before parents.
    fn subtree(&mut self, dir: &RelPath) -> Result<(), Gone> {
        self.contents(dir)?;
        for below in beneath(&self.kept.records, dir).into_iter().rev() {
            self.went_with(&below);
        }
        Ok(())
    }

    /// Observe every name in the directory `dir`, which may have come with
    /// contents no event named.
    fn contents(&mut self, dir: &RelPath) -> Result<(), Gone> {
        let disk = reach::disk_path(dir, &self.names);
        let Some(listed) = self.listing(disk, Some(dir)) else {
            return Ok(());
        };
        for (name, mapped) in listed {
            if let Name::Path(IndexName { path, differs }) = mapped {
                if differs {
                    self.names.learned.insert(path.clone(), name);
                }
                self.path(&path)?;
            }
        }
        Ok(())
    }

    /// The name on disk, if it is not the index name, that `path`'s parent
    /// lists for it.
    fn other_spelling(&mut self, path: &RelPath) -> Option<OsString> {
        let parent = path.parent();
        let disk = parent
            .as_ref()
            .map(|p| reach::disk_path(p, &self.names))
            .unwrap_or_default();
        self.listing(disk, parent.as_ref())?
            .into_iter()
            .find_map(|(name, mapped)| match mapped {
                Name::Path(IndexName {
                    path: index,
                    differs: true,
                }) if index == *path => Some(name),
                _ => None,
            })
    }

    /// The names in the directory at `disk`, the index path `dir` (`None`
    /// for the root), as [`names::dir`] maps them; `None` if it cannot be
    /// listed. Each directory is listed once per job.
    fn listing(&mut self, disk: PathBuf, dir: Option<&RelPath>) -> Option<Vec<(OsString, Name)>> {
        if let Some(listed) = self.listings.get(&disk) {
            return listed.clone();
        }
        let listed = self
            .kept
            .folder
            .open()
            .and_then(|fs| fs.read_dir(&disk))
            .ok()
            .map(|names| names::dir(dir, &names));
        self.listings.insert(disk, listed.clone());
        listed
    }
}

/// Every record beneath the directory `dir`, in order. They lie between
/// `dir` and `dir` followed by `0`, the character after `/`.
fn beneath(records: &BTreeMap<RelPath, Entry>, dir: &RelPath) -> Vec<RelPath> {
    let end = format!("{dir}0");
    records
        .range(dir.clone()..)
        .map(|(path, _)| path)
        .take_while(|path| path.as_str() < end.as_str())
        .filter(|path| dir.is_ancestor_of(path))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn beneath_a_directory_is_everything_under_it_and_nothing_beside_it() {
        let node = delocal_engine::NodeId::from_bytes([1; 16]);
        let entry = |path: &str| {
            let e = Entry {
                path: p(path),
                kind: Kind::Dir,
                size: 0,
                mtime_ns: 0,
                stamp: 1,
                exec: false,
                hash: delocal_engine::ContentHash::EMPTY,
                prev_hash: delocal_engine::ContentHash::EMPTY,
                version: delocal_engine::Version::empty().incremented(node),
                deleted: false,
                modified_by: node,
                author_host: delocal_engine::HostName::empty(),
            };
            (p(path), e)
        };
        let records: BTreeMap<RelPath, Entry> =
            ["a", "a-b", "a.txt", "a/x", "a/x/y", "a/z", "a0", "b"]
                .into_iter()
                .map(entry)
                .collect();
        assert_eq!(beneath(&records, &p("a")), [p("a/x"), p("a/x/y"), p("a/z")]);
        assert_eq!(beneath(&records, &p("a/x")), [p("a/x/y")]);
        assert!(beneath(&records, &p("b")).is_empty());
    }
}
