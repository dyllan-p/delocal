//! The commit path (DESIGN.md §7.5 steps 6 to 9, "Renames never replace",
//! "Two renames, one commit", "Local failures", "Deletes", "Symlinks";
//! §8.3 step 2's move to the trash): the host side of the engine's
//! `Write`, `Remove`, `SetMeta` and `MoveToTrash`.
//!
//! **One operation** (§7.3). [`Commit::run`] opens the folder root afresh,
//! checks the root guard's marker, and reaches its path one component at a
//! time ([`crate::reach`]): through the names on disk, never onto another
//! device (a walk that would cross a mount point stops with
//! `Failed { Io }`, draft 48), and never through something that is not a
//! directory (`ChangedUnderneath`, except where nothing needs the parent:
//! a delete of a path that cannot exist has nothing to do).
//!
//! **The guard** (step 6) is the scan fast path's predicate,
//! [`Observed::unchanged_by_stat`], between what the engine expected and
//! what [`reach::look`] finds, with the exact-name check a filesystem that
//! folds names needs. Before it, a `Write` on such a filesystem looks for a
//! live record at another spelling of its path or of an ancestor
//! (`Failed { CaseCollision }`, §7.6).
//!
//! **A write** then readies what comes in before it touches anything at
//! the path: a file's exec bit (written like git's, §7.1: execute added
//! wherever read is set, or every execute bit cleared) and mtime are set on
//! its temp file and the temp file is fsynced; a symlink is made in
//! `.delocal/tmp/` from the target its temp file holds; a directory is made
//! empty there. Every write is thus a rename of a temp (draft 65), and its
//! journal row names that temp, whose absence later says the rename
//! happened. Missing parent directories are made. If something is at the
//! path, a journal row naming where it goes is made durable (with its trash
//! row, for the trash), and it is renamed aside (step 7), to the trash or
//! the conflict-copy path the engine chose; then what comes in is renamed
//! to the path (step 8). The directories whose entries changed are fsynced
//! before the report (step 9), and the journal row is removed with the
//! report: by then the rename is durable.
//!
//! **Renames never replace** ([`rename_refusing`]): something that
//! appears at a target between the check and the rename makes the rename
//! fail, and survives. A rename in that fails puts the displaced file back
//! only if the path is still empty, so a failure leaves the disk as it found
//! it and never replaces what the user made meanwhile. If putting it back
//! fails too, the file is stranded: the report carries `Skipped`, since the
//! path is empty only because of this commit and must not read as a local
//! deletion; the journal row stays open; and the next commit at the path,
//! before anything else, or else the next start's undo, puts it back. A filesystem that
//! cannot refuse to replace gets a fresh check and a plain rename, and
//! [`DiskState::narrower`] says so for `status`.
//!
//! **A delete** moves a file or symlink to the trash, or renames it to the
//! conflict-copy path, in one rename, and removes a directory only when it
//! is empty (`rmdir`, which refuses anything else atomically); a directory
//! displaced to a conflict copy is renamed whole (§7.6). A revert's move to
//! the trash is the same, reported as `Trashed` (draft 57), and finds an
//! absent path already where it would leave it.
//!
//! **A metadata-only apply** sets the exec bit and mtime of the file it
//! guarded, and fsyncs it before the report.
//!
//! **Reports** (drafts 52, 58). Every report carries the time its
//! operation ran: just after the rename or the change, or, for a failure,
//! when the host took the observation the report carries, which is what a
//! scan would report for the path now ([`Observer`]), after any undo. A
//! failure maps to `ChangedUnderneath` when the path was not what the
//! engine expected, a target was taken, a directory was not empty or a
//! parent was not a directory, and otherwise to `Failed { DiskFull }` for
//! `ENOSPC` and `EDQUOT` and `Failed { Io }` for anything else, the host's
//! own temp files included. A folder whose marker is gone reports what it
//! found as `Skipped`, never `Absent`: a disk that is not mounted must not
//! read as its files deleted (§7.3, the root guard). An fsync that fails
//! after the rename does not undo a commit that has landed: the report is
//! `Ok`, the error is returned for the host to log, and the journal row
//! stays open, since the rename is not known to be durable.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use delocal_engine::path::RESERVED_DIR;
use delocal_engine::{
    Action, ApplyOutcome, ContentHash, Displace, Entry, Event, FolderId, Kind, LocalError,
    Observed, RelPath, ScanState, SkipReason, Timestamp, Version,
};

use crate::fs::{
    FileKind, Folder, Fs, NotAFile, ParentNotADirectory, Refusing, Stat, rename_refusing,
};
use crate::names::{self, IndexName, Name};
use crate::reach::{self, DiskNames, Look, Sensitivity, Walk};
use crate::scan::ignore_rules::{IgnoreRules, RuleSource};
use crate::scan::{Clock, MARKER, Observer, OpenRows, Records};
use crate::store::{HostWrite, JournalRow, TrashRow};
use crate::tmp;
use crate::trash::{self, Moving, NotMoved};

pub mod journal;
pub mod start;

pub use journal::{Journal, StoreJournal, undo};
pub use start::{Start, Started};

/// What the host keeps about one folder's disk from one commit to the
/// next.
#[derive(Debug, Default)]
pub struct DiskState {
    sensitivity: OnceLock<Sensitivity>,
    narrower: AtomicBool,
    /// The paths whose journal row is open while the daemon runs, and what
    /// resolves each (§7.5, draft 64). The scan and every single
    /// observation report them `Skipped` ([`OpenRows`]).
    open: Mutex<BTreeMap<RelPath, Open>>,
}

/// Why a path's journal row is still open, and what resolves it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Open {
    /// What a commit displaced is not back: a write whose rename in failed
    /// could not put it back, or the start's undo could not run. Resolved
    /// by moving it back into the path if that is empty, or by finding the
    /// path taken.
    Displaced {
        /// Where it is now.
        at: PathBuf,
        /// Where it goes back to.
        back: PathBuf,
        /// Its place in the trash, relative to `.delocal/trash/`, if it is
        /// there: its row goes when it comes back.
        trash: Option<PathBuf>,
        /// The temp file the commit was to rename in: if it is gone, the
        /// rename happened after all, and nothing goes back (draft 65).
        temp: Option<PathBuf>,
    },
    /// A commit landed, but an fsync after it failed, so its rename is not
    /// known to be durable. Resolved once these directories are synced.
    Unsynced { dirs: Vec<PathBuf> },
}

impl OpenRows for DiskState {
    fn is_open(&self, path: &RelPath) -> bool {
        self.opened()
            .keys()
            .any(|open| open == path || open.is_ancestor_of(path))
    }

    fn open_paths(&self) -> Vec<RelPath> {
        self.opened().keys().cloned().collect()
    }
}

impl DiskState {
    /// Nothing known yet: the first commit probes the filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    /// A disk whose sensitivity is already known.
    pub fn knowing(sensitivity: Sensitivity) -> Self {
        let state = Self::default();
        let _ = state.sensitivity.set(sensitivity);
        state
    }

    /// Whether a rename had to fall back to a check and a plain rename,
    /// because the filesystem cannot refuse to replace (§7.5): `status`
    /// notes the folder as having the narrower guarantee.
    pub fn narrower(&self) -> bool {
        self.narrower.load(Ordering::Relaxed)
    }

    /// How the filesystem compares names, probed by the first commit.
    fn sensitivity(&self, fs: &dyn Fs) -> io::Result<Sensitivity> {
        if let Some(known) = self.sensitivity.get() {
            return Ok(*known);
        }
        tmp::ensure(fs)?;
        let probed = Sensitivity::probe(fs)?;
        Ok(*self.sensitivity.get_or_init(|| probed))
    }

    fn opened(&self) -> std::sync::MutexGuard<'_, BTreeMap<RelPath, Open>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keep `path` open: a journal row the start's undo could not resolve,
    /// whose displaced file is at `at` (relative to the folder root), goes
    /// back to `back` if its rename never happened, and whose commit was to
    /// rename in `temp`.
    pub(crate) fn keep_displaced(
        &self,
        path: RelPath,
        at: PathBuf,
        back: PathBuf,
        temp: Option<PathBuf>,
    ) {
        let trash = at.strip_prefix(trash::DIR).ok().map(Path::to_path_buf);
        let open = Open::Displaced {
            at,
            back,
            trash,
            temp,
        };
        self.opened().insert(path, open);
    }

    /// Keep `path` open: a commit that landed, whose directories `dirs`
    /// could not be synced.
    pub(crate) fn keep_unsynced(&self, path: RelPath, dirs: Vec<PathBuf>) {
        self.opened().insert(path, Open::Unsynced { dirs });
    }

    fn renamed(&self, how: Refusing) {
        if how == Refusing::Checked {
            self.narrower.store(true, Ordering::Relaxed);
        }
    }
}

/// One folder's commits: what they need from the host. See the module
/// docs.
pub struct Commit<'a> {
    pub id: FolderId,
    pub folder: &'a dyn Folder,
    /// The folder's index records as the host last wrote them: the fast
    /// path's reference for what a failed report carries, and the live
    /// paths a case collision is judged against.
    pub records: &'a dyn Records,
    /// Its names on disk that are not their index paths' (§7.3).
    pub names: &'a dyn DiskNames,
    /// Where a displacement's journal row is made durable (§7.5).
    pub journal: &'a dyn Journal,
    /// The engine's clock (§7.8): reports are dated by it.
    pub clock: &'a dyn Clock,
    pub disk: &'a DiskState,
}

/// What a commit reports, and what it leaves for the host to write.
#[derive(Debug)]
pub struct Committed {
    /// `Applied`, or `Trashed` for a `MoveToTrash`.
    pub event: Event,
    /// The host's writes that belong with the report (§11): the journal
    /// row's removal once the rename is durable, the trash row of a delete,
    /// the removal of a name on disk that no longer applies. They go in the
    /// report's group, which must be durable before the next commit at the
    /// same path runs: that commit may put rows of its own at the same keys
    /// (the path's journal row, a place in the trash). §11's ordering, with
    /// effects held until the groups before them are durable, gives this.
    pub writes: Vec<HostWrite>,
    /// The directories the commit made above its path (§7.5 step 8).
    pub created: Vec<RelPath>,
    /// Fsyncs that failed after the commit had landed: it is reported
    /// `Ok`, but whether it survives a power loss is not known.
    pub unsynced: Vec<io::Error>,
    /// The files a user rule ignores inside a directory this commit was to
    /// remove, which it therefore did not remove (§7.3, draft 64): `status`
    /// names them.
    pub holds_ignored: Vec<RelPath>,
}

/// The commit an action asks for, in the host's terms.
enum Op<'a> {
    Write {
        entry: &'a Entry,
        expected: Option<&'a Observed>,
        displace: &'a Displace,
    },
    Remove {
        expected: Option<&'a Observed>,
        displace: &'a Displace,
    },
    SetMeta {
        expected: Option<&'a Observed>,
        mtime_ns: i64,
        exec: bool,
    },
    MoveToTrash {
        expected: &'a Observed,
    },
}

/// How a commit ended, before its report.
enum Fail {
    /// `ChangedUnderneath`.
    Changed,
    /// `Failed`.
    Local(LocalError),
    /// A directory to be removed holds files a user rule ignores (§7.3,
    /// draft 64): nothing was removed, and these are the files, which
    /// `status` names. The engine has no want state for this yet, so it is
    /// reported as `ChangedUnderneath`, as any directory found not empty,
    /// and the delete waits for the next observation of the directory.
    HoldsIgnored(Vec<RelPath>),
    /// `Failed`, and what the host found is not to be trusted as an
    /// observation, so the report carries `Skipped` for `reason`: the root
    /// guard failed (§7.3), the path is beneath a mount point (draft 48), or
    /// the displaced file could not be put back, so the path is empty only
    /// because of this commit (§7.5: no failure may read as a deletion).
    Blind {
        error: LocalError,
        reason: SkipReason,
    },
}

impl Fail {
    /// `Failed { Io }`, carrying `Skipped` for `reason`.
    fn blind(reason: SkipReason) -> Self {
        Self::Blind {
            error: LocalError::Io,
            reason,
        }
    }
}

/// What a commit that did not fail leaves.
struct Done {
    at: Timestamp,
    writes: Vec<HostWrite>,
    created: Vec<RelPath>,
    unsynced: Vec<io::Error>,
}

/// A failure, with the writes and the directories it leaves for the host.
struct Failed {
    fail: Fail,
    writes: Vec<HostWrite>,
}

impl From<Fail> for Failed {
    fn from(fail: Fail) -> Self {
        Self {
            fail,
            writes: Vec::new(),
        }
    }
}

/// One commit in progress: the path, where it is on disk, and the
/// operation's view of the folder.
struct At<'f> {
    fs: &'f dyn Fs,
    path: &'f RelPath,
    /// The path on disk, through its names, relative to the root.
    disk: PathBuf,
    /// Its parent on disk.
    parent: PathBuf,
    /// The root's device.
    device: u64,
    sensitivity: Sensitivity,
}

impl Commit<'_> {
    /// Perform the commit `action` asks for (§7.5 steps 6 to 9, §8.3 step
    /// 2), and return its report: `Applied`, or `Trashed` for a
    /// `MoveToTrash`, under `version`. That is the `Write`'s entry's or the
    /// `MoveToTrash`'s own; a `Remove` or `SetMeta` names none, so the host
    /// passes the version of the path's want when the engine asked for the
    /// commit, as the simulator does. `temp` is the verified temp file of a
    /// `Write` of a file or symlink (§7.5 steps 3 to 5), relative to the
    /// folder root. `None` for anything that is not a commit.
    pub fn run(
        &self,
        action: &Action,
        version: &Version,
        temp: Option<&Path>,
    ) -> Option<Committed> {
        let (path, op) = match action {
            Action::Write {
                path,
                entry,
                expected,
                displace,
                ..
            } => (
                path,
                Op::Write {
                    entry,
                    expected: expected.as_ref(),
                    displace,
                },
            ),
            Action::Remove {
                path,
                expected,
                displace,
                ..
            } => (
                path,
                Op::Remove {
                    expected: expected.as_ref(),
                    displace,
                },
            ),
            Action::SetMeta {
                path,
                expected,
                mtime_ns,
                exec,
                ..
            } => (
                path,
                Op::SetMeta {
                    expected: expected.as_ref(),
                    mtime_ns: *mtime_ns,
                    exec: *exec,
                },
            ),
            Action::MoveToTrash { path, expected, .. } => (path, Op::MoveToTrash { expected }),
            _ => return None,
        };
        let trashed = matches!(op, Op::MoveToTrash { .. });
        let mut holds_ignored = Vec::new();
        let (outcome, at, done) = match self.attempt(path, &op, temp) {
            Ok(done) => (ApplyOutcome::Ok, done.at, done),
            Err(failed) => {
                let (found, at) = self.found(path, &failed.fail);
                let outcome = match failed.fail {
                    Fail::Changed => ApplyOutcome::ChangedUnderneath { found },
                    Fail::HoldsIgnored(files) => {
                        holds_ignored = files;
                        ApplyOutcome::ChangedUnderneath { found }
                    }
                    Fail::Local(error) => ApplyOutcome::Failed { error, found },
                    Fail::Blind { error, .. } => ApplyOutcome::Failed { error, found },
                };
                let done = Done {
                    at,
                    writes: failed.writes,
                    created: Vec::new(),
                    unsynced: Vec::new(),
                };
                (outcome, at, done)
            }
        };
        // A commit that removed the path's journal row has resolved it,
        // whatever kept it open before.
        let removed_row = done
            .writes
            .iter()
            .any(|w| matches!(w, HostWrite::RemoveJournal { path: p, .. } if p == path));
        if removed_row {
            self.disk.opened().remove(path);
        }
        let (folder, path, version) = (self.id, path.clone(), version.clone());
        let event = if trashed {
            Event::Trashed {
                folder,
                path,
                version,
                at,
                outcome,
            }
        } else {
            Event::Applied {
                folder,
                path,
                version,
                at,
                outcome,
            }
        };
        Some(Committed {
            event,
            writes: done.writes,
            created: done.created,
            unsynced: done.unsynced,
            holds_ignored,
        })
    }

    /// The observation a failed report carries, and its time: what a scan
    /// would report for the path now, unless the host cannot trust what it
    /// would see there.
    fn found(&self, path: &RelPath, fail: &Fail) -> (ScanState, Timestamp) {
        let skipped = |reason| (ScanState::Skipped { reason }, self.clock.now());
        if let Fail::Blind { reason, .. } = fail {
            return skipped(*reason);
        }
        let Ok(fs) = self.folder.open() else {
            return skipped(SkipReason::Io);
        };
        if marker(&*fs).is_err() {
            return skipped(SkipReason::Io);
        }
        let Ok(sensitivity) = self.disk.sensitivity(&*fs) else {
            return skipped(SkipReason::Io);
        };
        Observer {
            fs: &*fs,
            records: self.records,
            names: self.names,
            open: self.disk,
            clock: self.clock,
            sensitivity,
        }
        .observe(path)
    }

    fn attempt(&self, path: &RelPath, op: &Op<'_>, temp: Option<&Path>) -> Result<Done, Failed> {
        let fs = self
            .folder
            .open()
            .map_err(|_| Fail::blind(SkipReason::Io))?;
        let fs = &*fs;
        marker(fs).map_err(|_| Fail::blind(SkipReason::Io))?;
        let sensitivity = self
            .disk
            .sensitivity(fs)
            .map_err(|e| Fail::Local(local(&e)))?;
        let device = fs
            .lstat(Path::new(""))
            .map_err(|e| Fail::Local(local(&e)))?
            .dev;
        let disk = reach::disk_path(path, self.names);
        let parent = disk.parent().map(Path::to_path_buf).unwrap_or_default();
        let at = At {
            fs,
            path,
            disk,
            parent,
            device,
            sensitivity,
        };
        match self.resolve_one(fs, path) {
            Resolution::NotOpen | Resolution::Resolved => {}
            // A displaced file that cannot go back yet: the path is empty
            // only because of this host, and no commit there goes ahead.
            Resolution::Open {
                displaced: true,
                error,
            } => {
                return Err(Fail::Blind {
                    error: local(&error),
                    reason: SkipReason::Io,
                }
                .into());
            }
            // A landed commit still not known durable: the path holds what
            // the engine adopted, and this commit may go ahead.
            Resolution::Open {
                displaced: false, ..
            } => {}
            // Resolved on disk, but its rows still say otherwise: the
            // report carries their changes, and this commit stops here.
            Resolution::Unrecorded { error, writes } => {
                return Err(Failed {
                    fail: Fail::Local(local(&error)),
                    writes,
                });
            }
        }
        match *op {
            Op::Write {
                entry,
                expected,
                displace,
            } => self.write(&at, entry, expected, displace, temp),
            Op::Remove { expected, displace } => self.remove(&at, expected, displace),
            Op::SetMeta {
                expected,
                mtime_ns,
                exec,
            } => self.set_meta(&at, expected, mtime_ns, exec),
            Op::MoveToTrash { expected } => self.move_to_trash(&at, expected),
        }
    }

    /// Resolve the open journal row at `path`, if there is one (§7.5,
    /// draft 64): put a displaced file back if the path is still empty, or
    /// sync the directories of a commit whose fsync failed. The rows that
    /// change are made durable at once, before a commit can make rows of
    /// its own for the same places (it may displace into the very place a
    /// file just left).
    fn resolve_one(&self, fs: &dyn Fs, path: &RelPath) -> Resolution {
        let Some(open) = self.disk.opened().get(path).cloned() else {
            return Resolution::NotOpen;
        };
        let journal_gone = HostWrite::RemoveJournal {
            folder: self.id,
            path: path.clone(),
        };
        // Draft 65: a temp file that is gone means the rename happened, so
        // the commit landed and only its durability is left to make sure of.
        let open = match open {
            Open::Displaced { at, back, temp, .. }
                if temp.as_ref().is_none_or(
                    |temp| matches!(fs.lstat(temp), Err(e) if e.kind() == io::ErrorKind::NotFound),
                ) =>
            {
                let dirs = journal::landed_dirs(&back, &at);
                self.disk
                    .opened()
                    .insert(path.clone(), Open::Unsynced { dirs: dirs.clone() });
                Open::Unsynced { dirs }
            }
            open => open,
        };
        let writes = match open {
            Open::Displaced {
                at, back, trash, ..
            } => match rename_refusing(fs, &at, &back) {
                Ok(how) => {
                    self.disk.renamed(how);
                    let back_from_trash = trash.map(|trashed_path| HostWrite::RemoveTrash {
                        folder: self.id,
                        trashed_path,
                    });
                    let mut writes = vec![journal_gone];
                    writes.extend(back_from_trash);
                    writes
                }
                // The path was taken meanwhile, or the file went: nothing
                // is left to undo, and the file stays where it is.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                    ) =>
                {
                    vec![journal_gone]
                }
                Err(e) => {
                    return Resolution::Open {
                        displaced: true,
                        error: e,
                    };
                }
            },
            Open::Unsynced { dirs } => {
                if let Err(error) = journal::sync_landed(fs, &dirs) {
                    return Resolution::Open {
                        displaced: false,
                        error,
                    };
                }
                vec![journal_gone]
            }
        };
        self.disk.opened().remove(path);
        match self.journal.durable(writes.clone()) {
            Ok(()) => Resolution::Resolved,
            Err(e) => Resolution::Unrecorded { error: e, writes },
        }
    }

    /// Resolve every open journal row of the folder that can be resolved
    /// now (§7.5, draft 64). The host calls this before each scan, so a
    /// path stays `Skipped` no longer than it must; each commit resolves its
    /// own path's row first anyway.
    pub fn resolve(&self) -> Resolved {
        let mut resolved = Resolved::default();
        let paths = self.disk.open_paths();
        if paths.is_empty() {
            return resolved;
        }
        // Nothing is moved in a folder whose marker is gone (§7.3).
        let fs = match self.folder.open() {
            Ok(fs) if marker(&*fs).is_ok() => fs,
            _ => {
                resolved.open = paths;
                return resolved;
            }
        };
        for path in paths {
            match self.resolve_one(&*fs, &path) {
                Resolution::NotOpen | Resolution::Resolved => resolved.resolved.push(path),
                Resolution::Open { error, .. } => {
                    resolved.errors.push((path.clone(), error));
                    resolved.open.push(path);
                }
                Resolution::Unrecorded { error, writes } => {
                    resolved.errors.push((path.clone(), error));
                    resolved.unrecorded.extend(writes);
                    resolved.resolved.push(path);
                }
            }
        }
        resolved
    }

    /// Step 6's look at the path: the walk to its parent, then the path
    /// itself. A walk onto another device, or a path that is itself a mount
    /// point, stops the commit (draft 48).
    fn look(&self, at: &At<'_>) -> Result<Found, Fail> {
        match reach::walk(at.fs, at.path, &at.disk, at.device) {
            Ok(Walk::Reached) => {}
            Ok(Walk::Missing { first }) => return Ok(Found::Missing { first }),
            Ok(Walk::NotADirectory { .. }) => return Ok(Found::Unreachable),
            Ok(Walk::Crossed { .. }) => return Err(Fail::blind(SkipReason::Ignored)),
            Err(e) => return Err(Fail::Local(local(&e))),
        }
        let name = at.disk.file_name().unwrap_or_default();
        match reach::look(at.fs, &at.parent, at.path, name, at.sensitivity) {
            Ok(Look::Absent) => Ok(Found::Nothing),
            Ok(Look::Taken { .. } | Look::Present { seen: None, .. }) => Ok(Found::Taken),
            Ok(Look::Coinciding) => Err(Fail::Changed),
            Ok(Look::Present {
                name,
                stat,
                seen: Some(seen),
            }) => {
                // The path itself is a mount point.
                if seen.kind == Kind::Dir && stat.dev != at.device {
                    return Err(Fail::blind(SkipReason::Ignored));
                }
                Ok(Found::Entry {
                    disk: at.parent.join(name),
                    seen,
                    mode: stat.mode,
                })
            }
            Err(e) => Err(Fail::Local(local(&e))),
        }
    }

    /// `Write` (§7.5 steps 6 to 9). See the module docs.
    fn write(
        &self,
        at: &At<'_>,
        entry: &Entry,
        expected: Option<&Observed>,
        displace: &Displace,
        temp: Option<&Path>,
    ) -> Result<Done, Failed> {
        let fs = at.fs;
        let live = self.records.live_entries();
        if let Some(with) = reach::collision(at.path, live.map(|e| &e.path), at.sensitivity) {
            return Err(Fail::Local(LocalError::CaseCollision { with }).into());
        }
        let found = self.look(at)?;
        let missing = match &found {
            Found::Unreachable => return Err(Fail::Changed.into()),
            Found::Taken => return Err(Fail::Changed.into()),
            Found::Missing { first } => Some(first.clone()),
            Found::Nothing | Found::Entry { .. } => None,
        };
        guard(expected, found.seen())?;
        // The conflict-copy path must be free, as the engine found it.
        if let Displace::ConflictCopy(to) = displace {
            self.free(at, to)?;
        }
        let incoming = ready(fs, entry, temp)?;
        let created = make_parents(at, missing.as_ref()).inspect_err(|_| incoming.discard(fs))?;
        let undo_parents = |fs: &dyn Fs| {
            for dir in created.iter().rev() {
                let _ = fs.remove_dir(&reach::disk_path(dir, self.names));
            }
        };
        // Step 7, behind a durable journal row.
        let displaced = match &found {
            Found::Entry { disk, seen, .. } => {
                let hash = expected.map_or(seen.hash, |e| e.hash);
                match self.displace(at, disk, seen, hash, displace, Some(incoming.path())) {
                    Ok(displaced) => Some(displaced),
                    Err(failed) => {
                        incoming.discard(fs);
                        undo_parents(fs);
                        return Err(failed);
                    }
                }
            }
            _ => None,
        };
        let journal_rows = |displaced: &Option<Displaced>| -> Vec<HostWrite> {
            displaced
                .as_ref()
                .map(|_| HostWrite::RemoveJournal {
                    folder: self.id,
                    path: at.path.clone(),
                })
                .into_iter()
                .collect()
        };
        // Step 8: the rename in, never replacing; received paths are
        // written as they are (§7.3), so at the index name.
        let target = at.parent.join(at.path.file_name());
        let renamed =
            rename_refusing(fs, incoming.path(), &target).map(|how| self.disk.renamed(how));
        if let Err(e) = renamed {
            let mut fail = changed_or_local(&e);
            let writes = match (&displaced, &found) {
                (Some(displaced), Found::Entry { disk, .. }) => {
                    match self.put_back(at.path, fs, displaced, disk, Some(incoming.path())) {
                        Some(writes) => {
                            incoming.discard(fs);
                            writes
                        }
                        // The temp file stays, so that the row still says
                        // its rename never happened (draft 65).
                        None => {
                            // Stranded: the path is empty because of this
                            // commit, which must not read as a deletion.
                            fail = Fail::Blind {
                                error: match fail {
                                    Fail::Local(error) => error,
                                    _ => LocalError::Io,
                                },
                                reason: SkipReason::Io,
                            };
                            Vec::new()
                        }
                    }
                }
                _ => {
                    incoming.discard(fs);
                    Vec::new()
                }
            };
            undo_parents(fs);
            return Err(Failed { fail, writes });
        }
        let now = self.clock.now();
        // Step 9: what changed is durable before the report.
        let mut unsynced = Vec::new();
        let mut sync = |dir: &Path| {
            if let Err(e) = fs.sync_dir(dir) {
                unsynced.push(e);
            }
        };
        if let Some(first) = &missing {
            let first = reach::disk_path(first, self.names);
            sync(first.parent().unwrap_or(Path::new("")));
        }
        for dir in &created {
            sync(&reach::disk_path(dir, self.names));
        }
        sync(&at.parent);
        if let Some(displaced) = &displaced {
            sync(displaced.to.parent().unwrap_or(Path::new("")));
        }
        sync(Path::new(tmp::DIR));
        incoming.finish(fs);
        // The journal row goes once the rename is durable (§7.5). If an
        // fsync failed, that is not known, and the row stays for the next
        // start, which leaves an occupied path alone.
        let mut writes = if unsynced.is_empty() {
            journal_rows(&displaced)
        } else {
            if displaced.is_some() {
                let mut dirs = vec![at.parent.clone()];
                dirs.extend(
                    displaced
                        .as_ref()
                        .and_then(|d| d.to.parent())
                        .map(Path::to_path_buf),
                );
                self.disk
                    .opened()
                    .insert(at.path.clone(), Open::Unsynced { dirs });
            }
            Vec::new()
        };
        writes.extend(self.stale_name(at.path));
        Ok(Done {
            at: now,
            writes,
            created,
            unsynced,
        })
    }

    /// `ChangedUnderneath` unless nothing is at the conflict-copy path
    /// `to` (§7.6: a target that appears between classification and commit).
    fn free(&self, at: &At<'_>, to: &RelPath) -> Result<(), Fail> {
        let disk = reach::disk_path(to, self.names);
        let parent = disk.parent().unwrap_or(Path::new(""));
        let name = disk.file_name().unwrap_or_default();
        match reach::look(at.fs, parent, to, name, at.sensitivity) {
            Ok(Look::Absent) => Ok(()),
            Ok(_) => Err(Fail::Changed),
            Err(e) => Err(Fail::Local(local(&e))),
        }
    }

    /// Step 7: move what is at the path aside, after making its journal
    /// row durable, and its trash row with it for the trash.
    fn displace(
        &self,
        at: &At<'_>,
        disk: &Path,
        seen: &Observed,
        hash: ContentHash,
        displace: &Displace,
        temp: Option<&Path>,
    ) -> Result<Displaced, Failed> {
        let fs = at.fs;
        if seen.kind == Kind::Dir {
            refuse_mounts(at, disk)?;
        }
        let row = |to: &Path| JournalRow {
            path: at.path.clone(),
            displaced_to: to.to_path_buf(),
            temp_file: temp.map(Path::to_path_buf),
        };
        let journal_gone = |extra: Option<HostWrite>| -> Vec<HostWrite> {
            let mut writes = vec![HostWrite::RemoveJournal {
                folder: self.id,
                path: at.path.clone(),
            }];
            writes.extend(extra);
            writes
        };
        match displace {
            Displace::Trash => {
                let what = Moving {
                    original: at.path,
                    kind: seen.kind,
                    hash: if seen.kind == Kind::Dir {
                        ContentHash::EMPTY
                    } else {
                        hash
                    },
                    size: match seen.kind {
                        Kind::Dir => trash::size_of(fs, disk, at.device),
                        Kind::File | Kind::Symlink => seen.size,
                    },
                    at: self.clock.now(),
                };
                let mut last: Option<TrashRow> = None;
                let moved = trash::move_in(fs, disk, &what, &mut |place| {
                    let trash_row = what.row(place);
                    let mut writes = vec![
                        HostWrite::PutJournal {
                            folder: self.id,
                            row: row(&place.disk),
                        },
                        HostWrite::PutTrash {
                            folder: self.id,
                            row: trash_row.clone(),
                        },
                    ];
                    // A place taken in the window has another's file now:
                    // the row this commit wrote for it goes.
                    if let Some(old) = last.replace(trash_row) {
                        writes.push(remove_trash(self.id, &old));
                    }
                    self.journal.durable(writes)
                });
                let unrow =
                    |last: &Option<TrashRow>| last.as_ref().map(|r| remove_trash(self.id, r));
                match moved {
                    Ok(moved) => {
                        self.disk.renamed(moved.refusing);
                        Ok(Displaced {
                            to: moved.place.disk,
                            trash: last,
                        })
                    }
                    Err(NotMoved::Gone) => Err(Failed {
                        fail: Fail::Changed,
                        writes: if last.is_some() {
                            journal_gone(unrow(&last))
                        } else {
                            Vec::new()
                        },
                    }),
                    Err(NotMoved::Failed(e)) => Err(Failed {
                        fail: changed_or_local(&e),
                        writes: if last.is_some() {
                            journal_gone(unrow(&last))
                        } else {
                            Vec::new()
                        },
                    }),
                }
            }
            Displace::ConflictCopy(copy) => {
                let to = reach::disk_path(copy, self.names);
                self.journal
                    .durable(vec![HostWrite::PutJournal {
                        folder: self.id,
                        row: row(&to),
                    }])
                    .map_err(|e| Fail::Local(local(&e)))?;
                match rename_refusing(fs, disk, &to) {
                    Ok(how) => {
                        self.disk.renamed(how);
                        Ok(Displaced { to, trash: None })
                    }
                    Err(e) => Err(Failed {
                        fail: changed_or_local(&e),
                        writes: journal_gone(None),
                    }),
                }
            }
        }
    }

    /// Undo a displacement after a failed rename in: move it back to
    /// where it was found, `to`, only if that is still empty (§7.5), and
    /// return the writes that go with the report. A displaced file that came
    /// back out of the trash loses its trash row, and the journal row goes.
    /// One that cannot go back because the path was taken meanwhile stays
    /// where it went, with its trash row, and the journal row goes too:
    /// there is nothing left to undo. One that cannot go back for an I/O
    /// error is stranded: `None`. Its journal row stays open, so the next
    /// start's undo moves it back if the path is still empty, and so does
    /// the next commit at the path before anything else.
    fn put_back(
        &self,
        path: &RelPath,
        fs: &dyn Fs,
        displaced: &Displaced,
        to: &Path,
        temp: Option<&Path>,
    ) -> Option<Vec<HostWrite>> {
        let journal_gone = HostWrite::RemoveJournal {
            folder: self.id,
            path: path.clone(),
        };
        match rename_refusing(fs, &displaced.to, to) {
            Ok(how) => {
                self.disk.renamed(how);
                let mut writes = vec![journal_gone];
                writes.extend(
                    displaced
                        .trash
                        .as_ref()
                        .map(|row| remove_trash(self.id, row)),
                );
                Some(writes)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Some(vec![journal_gone]),
            Err(_) => {
                let stranded = Open::Displaced {
                    at: displaced.to.clone(),
                    back: to.to_path_buf(),
                    trash: displaced.trash.as_ref().map(|row| row.trashed_path.clone()),
                    temp: temp.map(Path::to_path_buf),
                };
                self.disk.opened().insert(path.clone(), stranded);
                None
            }
        }
    }

    /// The removal of `path`'s name on disk, if it had one other than its
    /// index name: what is there now was written by its index name, or is
    /// gone.
    fn stale_name(&self, path: &RelPath) -> Option<HostWrite> {
        self.names.name(path).map(|_| HostWrite::RemoveDiskName {
            folder: self.id,
            path: path.clone(),
        })
    }

    /// `Remove` (§7.5 "Deletes", step 7 for a conflict a tombstone won).
    fn remove(
        &self,
        at: &At<'_>,
        expected: Option<&Observed>,
        displace: &Displace,
    ) -> Result<Done, Failed> {
        let found = self.look(at)?;
        guard(expected, found.seen())?;
        let Found::Entry { disk, seen, .. } = found else {
            return Ok(self.done(Vec::new(), Vec::new()));
        };
        let hash = expected.map_or(seen.hash, |e| e.hash);
        let fs = at.fs;
        let mut writes = Vec::new();
        let to = match (seen.kind, displace) {
            (Kind::Dir, Displace::Trash) => {
                writes.extend(self.remove_dir(at, &disk)?);
                None
            }
            (_, Displace::ConflictCopy(copy)) => {
                if seen.kind == Kind::Dir {
                    refuse_mounts(at, &disk)?;
                }
                let to = reach::disk_path(copy, self.names);
                let how = rename_refusing(fs, &disk, &to).map_err(|e| changed_or_local(&e))?;
                self.disk.renamed(how);
                Some(to)
            }
            (Kind::File | Kind::Symlink, Displace::Trash) => {
                let place = self.trash(at, at.path, &disk, seen.kind, hash, seen.size)?;
                writes.push(HostWrite::PutTrash {
                    folder: self.id,
                    row: place.1,
                });
                Some(place.0)
            }
        };
        let now = self.clock.now();
        let mut unsynced = Vec::new();
        for dir in std::iter::once(at.parent.as_path()).chain(to.as_deref().and_then(Path::parent))
        {
            if let Err(e) = fs.sync_dir(dir) {
                unsynced.push(e);
            }
        }
        writes.extend(self.stale_name(at.path));
        Ok(Done {
            at: now,
            writes,
            created: Vec::new(),
            unsynced,
        })
    }

    /// Move what is at `disk`, the index path `original`, into the trash,
    /// unjournalled: one rename leaves the path absent, which is what the
    /// commit is for. Returns where it went and its row.
    fn trash(
        &self,
        at: &At<'_>,
        original: &RelPath,
        disk: &Path,
        kind: Kind,
        hash: ContentHash,
        size: u64,
    ) -> Result<(PathBuf, TrashRow), Fail> {
        let what = Moving {
            original,
            kind,
            hash,
            size,
            at: self.clock.now(),
        };
        match trash::move_in(at.fs, disk, &what, &mut |_| Ok(())) {
            Ok(moved) => {
                self.disk.renamed(moved.refusing);
                let row = what.row(&moved.place);
                Ok((moved.place.disk, row))
            }
            Err(NotMoved::Gone) => Err(Fail::Changed),
            Err(NotMoved::Failed(e)) => Err(changed_or_local(&e)),
        }
    }

    /// Remove the directory at `disk`, the commit's path, which is to be
    /// removed (§7.5 "Deletes", §8.3, drafts 61 and 64): only when it is
    /// empty, except that what only the default rules ignore inside (a
    /// `.DS_Store`, an editor's swap file) goes to the trash first. Anything
    /// else inside stops it: an entry no rule ignores is `ChangedUnderneath`,
    /// as before, and a file a user rule ignores is [`Fail::HoldsIgnored`],
    /// with nothing moved. Returns the trash rows of what went.
    fn remove_dir(&self, at: &At<'_>, disk: &Path) -> Result<Vec<HostWrite>, Failed> {
        let fs = at.fs;
        match fs.remove_dir(disk) {
            Ok(()) => return Ok(Vec::new()),
            Err(e) if not_empty(&e) => {}
            Err(e) => return Err(changed_or_local(&e).into()),
        }
        let mut writes = Vec::new();
        for (name, index, stat) in self.holds(at, disk)? {
            let child = disk.join(&name);
            let (kind, hash, size) = match stat.kind {
                FileKind::File => {
                    let hash = trash::hash_file(fs, &child).map_err(|e| changed_or_local(&e))?;
                    (Kind::File, hash, stat.size)
                }
                FileKind::Symlink => {
                    let target = fs.read_link(&child).map_err(|e| changed_or_local(&e))?;
                    let hash = crate::scan::hash::hash_target(target.as_os_str().as_bytes());
                    (Kind::Symlink, hash, stat.size)
                }
                FileKind::Dir => {
                    // Never a disk mounted there (draft 48).
                    if stat.dev != at.device {
                        return Err(Fail::Local(LocalError::Io).into());
                    }
                    refuse_mounts(at, &child)?;
                    let size = trash::size_of(fs, &child, at.device);
                    (Kind::Dir, ContentHash::EMPTY, size)
                }
                FileKind::Other => return Err(Fail::Changed.into()),
            };
            match self.trash(at, &index, &child, kind, hash, size) {
                Ok((_, row)) => writes.push(HostWrite::PutTrash {
                    folder: self.id,
                    row,
                }),
                Err(fail) => return Err(Failed { fail, writes }),
            }
        }
        match fs.remove_dir(disk) {
            Ok(()) => Ok(writes),
            Err(e) => Err(Failed {
                fail: changed_or_local(&e),
                writes,
            }),
        }
    }

    /// What the directory at `disk`, the commit's path, holds, when every
    /// entry in it is ignored by the default rules: those entries, by name
    /// on disk, index path and `stat`. A file a user rule ignores is
    /// [`Fail::HoldsIgnored`], listing every such file; an entry no rule
    /// ignores, or one with no index path, is `ChangedUnderneath`; rules
    /// that cannot be read leave nothing to tell by, and are too.
    fn holds(&self, at: &At<'_>, disk: &Path) -> Result<Vec<(OsString, RelPath, Stat)>, Fail> {
        let fs = at.fs;
        let rules = fs
            .open_dir(Path::new(""))
            .and_then(|root| IgnoreRules::read(&*root))
            .map_err(|_| Fail::Changed)?;
        let listed = fs.read_dir(disk).map_err(|e| changed_or_local(&e))?;
        let mut defaults = Vec::new();
        let mut users = Vec::new();
        for name in listed {
            let Name::Path(IndexName { path, .. }) = names::name(Some(at.path), &name) else {
                return Err(Fail::Changed);
            };
            let stat = match fs.lstat(&disk.join(&name)) {
                Ok(stat) => stat,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(changed_or_local(&e)),
            };
            match rules.source(&path, stat.kind == FileKind::Dir) {
                Some(RuleSource::Default) => defaults.push((name, path, stat)),
                Some(RuleSource::User) => users.push(path),
                None => return Err(Fail::Changed),
            }
        }
        if users.is_empty() {
            Ok(defaults)
        } else {
            Err(Fail::HoldsIgnored(users))
        }
    }

    /// `SetMeta` (§7.5 "Metadata-only applies").
    fn set_meta(
        &self,
        at: &At<'_>,
        expected: Option<&Observed>,
        mtime_ns: i64,
        exec: bool,
    ) -> Result<Done, Failed> {
        let found = self.look(at)?;
        guard(expected, found.seen())?;
        let Found::Entry {
            disk,
            seen: Observed {
                kind: Kind::File, ..
            },
            mode,
        } = found
        else {
            return Err(Fail::Changed.into());
        };
        let fs = at.fs;
        let wanted = exec_mode(mode, exec);
        if wanted != mode {
            fs.set_mode(&disk, wanted)
                .map_err(|e| changed_or_local(&e))?;
        }
        if let Err(e) = fs.set_mtime(&disk, mtime_ns) {
            // Put the mode back, so a failure leaves the file as it was.
            if wanted != mode {
                let _ = fs.set_mode(&disk, mode);
            }
            return Err(changed_or_local(&e).into());
        }
        let now = self.clock.now();
        let unsynced = fs.sync_file(&disk).err().into_iter().collect();
        Ok(Done {
            at: now,
            writes: Vec::new(),
            created: Vec::new(),
            unsynced,
        })
    }

    /// `MoveToTrash` (§8.3 step 2, drafts 57 and 61).
    fn move_to_trash(&self, at: &At<'_>, expected: &Observed) -> Result<Done, Failed> {
        let found = self.look(at)?;
        let Found::Entry { disk, seen, .. } = found else {
            // An absent path is already where the move would leave it.
            return Ok(self.done(Vec::new(), Vec::new()));
        };
        if !expected.unchanged_by_stat(&seen) {
            return Err(Fail::Changed.into());
        }
        let fs = at.fs;
        let mut writes = Vec::new();
        let mut dirs = vec![at.parent.clone()];
        if seen.kind == Kind::Dir {
            // Only an empty directory, but for what only the defaults
            // ignore (draft 61, draft 64).
            writes.extend(self.remove_dir(at, &disk)?);
        } else {
            let (to, row) = self.trash(at, at.path, &disk, seen.kind, expected.hash, seen.size)?;
            dirs.extend(to.parent().map(Path::to_path_buf));
            writes.push(HostWrite::PutTrash {
                folder: self.id,
                row,
            });
        }
        let now = self.clock.now();
        let unsynced = dirs
            .iter()
            .filter_map(|dir| fs.sync_dir(dir).err())
            .collect();
        writes.extend(self.stale_name(at.path));
        Ok(Done {
            at: now,
            writes,
            created: Vec::new(),
            unsynced,
        })
    }

    /// A commit that found nothing to do, dated now.
    fn done(&self, writes: Vec<HostWrite>, created: Vec<RelPath>) -> Done {
        Done {
            at: self.clock.now(),
            writes,
            created,
            unsynced: Vec::new(),
        }
    }
}

/// What resolving one path's open journal row came to.
enum Resolution {
    /// Its row was not open.
    NotOpen,
    /// Resolved, and its rows durable.
    Resolved,
    /// Still open: a displaced file that cannot go back yet, or
    /// directories that still cannot be synced.
    Open { displaced: bool, error: io::Error },
    /// Resolved on disk, but its rows could not be made durable: `writes`
    /// still have to be.
    Unrecorded {
        error: io::Error,
        writes: Vec<HostWrite>,
    },
}

/// What [`Commit::resolve`] did.
#[derive(Debug, Default)]
pub struct Resolved {
    /// Paths whose rows were resolved: no longer skipped.
    pub resolved: Vec<RelPath>,
    /// Paths still open, still skipped.
    pub open: Vec<RelPath>,
    /// Why, for the host to log.
    pub errors: Vec<(RelPath, io::Error)>,
    /// Writes resolved on disk that could not be made durable through the
    /// journal; the host writes them with its next group.
    pub unrecorded: Vec<HostWrite>,
}

/// What step 6 found at the path.
enum Found {
    /// Nothing, and nothing taking its name.
    Nothing,
    /// The ancestor `first` is missing, so the path is too.
    Missing { first: RelPath },
    /// An ancestor is not a directory: the path cannot be there.
    Unreachable,
    /// Something not the path's takes its name: a differently cased file on
    /// a filesystem that folds case, or a FIFO, a socket or a device.
    Taken,
    /// The entry at the path, where it is on disk, and its mode.
    Entry {
        disk: PathBuf,
        seen: Observed,
        mode: u32,
    },
}

impl Found {
    /// What the guard compares: the entry, or nothing.
    fn seen(&self) -> Option<&Observed> {
        match self {
            Self::Entry { seen, .. } => Some(seen),
            Self::Nothing | Self::Missing { .. } | Self::Unreachable | Self::Taken => None,
        }
    }
}

/// A displacement done: where it went, and its trash row if it went to
/// the trash.
struct Displaced {
    to: PathBuf,
    trash: Option<TrashRow>,
}

/// What a write renames in, readied before anything at the path is
/// touched.
enum Incoming {
    /// The fetched temp file, its mode and mtime set and its data durable.
    File(PathBuf),
    /// A symlink made in `tmp/`, and the temp file its target came from.
    Symlink { link: PathBuf, temp: PathBuf },
    /// An empty directory made in `tmp/` (draft 65: every write goes
    /// through a temp and a rename, so every journal row has a temp that
    /// says whether its rename happened).
    Dir(PathBuf),
}

impl Incoming {
    /// What is renamed to the path: the temp the journal row names.
    fn path(&self) -> &Path {
        match self {
            Self::File(temp) | Self::Dir(temp) => temp,
            Self::Symlink { link, .. } => link,
        }
    }

    /// After a failure: what the commit made for itself goes (a symlink, an
    /// empty directory), and a fetched temp file stays for the next attempt
    /// to resume (§7.5).
    fn discard(&self, fs: &dyn Fs) {
        match self {
            Self::Symlink { link, .. } => {
                let _ = fs.remove_file(link);
            }
            Self::Dir(dir) => {
                let _ = fs.remove_dir(dir);
            }
            Self::File(_) => {}
        }
    }

    /// After the rename in: the temp file a symlink's target came from
    /// has served its purpose.
    fn finish(&self, fs: &dyn Fs) {
        if let Self::Symlink { temp, .. } = self {
            let _ = fs.remove_file(temp);
        }
    }
}

/// Ready what a write renames in, without touching the path (§7.5 steps 5
/// and 8, "Symlinks"). A missing or unusable temp file is the host's own
/// failure, `Failed { Io }`: the want fetches again.
fn ready(fs: &dyn Fs, entry: &Entry, temp: Option<&Path>) -> Result<Incoming, Fail> {
    let io = |e: &io::Error| Fail::Local(local(e));
    match entry.kind {
        Kind::Dir => {
            tmp::ensure(fs).map_err(|e| io(&e))?;
            let dir = tmp::scratch("dir");
            fs.create_dir(&dir).map_err(|e| io(&e))?;
            Ok(Incoming::Dir(dir))
        }
        Kind::File => {
            let temp = temp.ok_or(Fail::Local(LocalError::Io))?;
            let mode = fs.lstat(temp).map_err(|e| io(&e))?.mode;
            let wanted = exec_mode(mode, entry.exec);
            if wanted != mode {
                fs.set_mode(temp, wanted).map_err(|e| io(&e))?;
            }
            fs.set_mtime(temp, entry.mtime_ns).map_err(|e| io(&e))?;
            fs.sync_file(temp).map_err(|e| io(&e))?;
            Ok(Incoming::File(temp.to_path_buf()))
        }
        Kind::Symlink => {
            let temp = temp.ok_or(Fail::Local(LocalError::Io))?;
            let mut target = Vec::new();
            fs.open_read(temp)
                .and_then(|mut file| file.read_to_end(&mut target))
                .map_err(|e| io(&e))?;
            // The fetch verified it; a temp file changed since is not what
            // the engine asked for.
            if crate::scan::hash::hash_target(&target) != entry.hash {
                return Err(Fail::Local(LocalError::Io));
            }
            tmp::ensure(fs).map_err(|e| io(&e))?;
            let link = tmp::scratch("link");
            fs.symlink(Path::new(OsStr::from_bytes(&target)), &link)
                .map_err(|e| io(&e))?;
            if let Err(e) = fs.sync_dir(Path::new(tmp::DIR)) {
                let _ = fs.remove_file(&link);
                return Err(io(&e));
            }
            Ok(Incoming::Symlink {
                link,
                temp: temp.to_path_buf(),
            })
        }
    }
}

/// Make the directories from `first` down to the path's parent (§7.5 step
/// 8). Returns them, highest first. A directory made by someone else a
/// moment before is used; anything else there is `ChangedUnderneath`, and
/// what this made is removed again.
fn make_parents(at: &At<'_>, first: Option<&RelPath>) -> Result<Vec<RelPath>, Failed> {
    let Some(first) = first else {
        return Ok(Vec::new());
    };
    let mut created: Vec<RelPath> = Vec::new();
    let mut disk = at.disk.parent().map(Path::to_path_buf).unwrap_or_default();
    // The directories to make, from the parent up to `first`.
    let mut todo = Vec::new();
    let mut dir = at.path.parent();
    while let Some(d) = dir {
        todo.push((d.clone(), disk.clone()));
        if d == *first {
            break;
        }
        dir = d.parent();
        disk = disk.parent().map(Path::to_path_buf).unwrap_or_default();
    }
    let undo = |fs: &dyn Fs, created: &[RelPath], disks: &[(RelPath, PathBuf)]| {
        for made in created.iter().rev() {
            if let Some((_, disk)) = disks.iter().find(|(d, _)| d == made) {
                let _ = fs.remove_dir(disk);
            }
        }
    };
    for (index, disk) in todo.iter().rev() {
        match at.fs.create_dir(disk) {
            Ok(()) => created.push(index.clone()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match at.fs.lstat(disk) {
                Ok(stat) if stat.kind == FileKind::Dir && stat.dev == at.device => {}
                _ => {
                    undo(at.fs, &created, &todo);
                    return Err(Fail::Changed.into());
                }
            },
            Err(e) => {
                undo(at.fs, &created, &todo);
                return Err(changed_or_local(&e).into());
            }
        }
    }
    Ok(created)
}

/// `Failed { Io }` if a disk is mounted anywhere inside the directory at
/// `disk`, which a commit would otherwise move whole (§7.3, draft 48): into
/// the trash, where pruning would delete what is on it, or to a conflict
/// copy. Nothing is moved until the mount goes. The directory itself is on
/// the root's device, so the report carries what is observed there.
fn refuse_mounts(at: &At<'_>, disk: &Path) -> Result<(), Fail> {
    match reach::mounted_beneath(at.fs, disk, at.device) {
        Ok(false) => Ok(()),
        Ok(true) => Err(Fail::Local(LocalError::Io)),
        Err(e) => Err(changed_or_local(&e)),
    }
}

/// Step 6's guard: what the engine expected at the path against what is
/// there, by the scan fast path's predicate (§7.3).
fn guard(expected: Option<&Observed>, seen: Option<&Observed>) -> Result<(), Fail> {
    let unchanged = match (expected, seen) {
        (None, None) => true,
        (Some(expected), Some(seen)) => expected.unchanged_by_stat(seen),
        _ => false,
    };
    if unchanged {
        Ok(())
    } else {
        Err(Fail::Changed)
    }
}

/// The root guard's marker, `.delocal/folder.json`, is a file (§7.3). A
/// commit into a folder without it (a disk not mounted, a folder deleted
/// wholesale) does nothing.
fn marker(fs: &dyn Fs) -> io::Result<()> {
    let stat = fs.lstat(&Path::new(RESERVED_DIR).join(MARKER))?;
    if stat.kind == FileKind::File {
        Ok(())
    } else {
        Err(NotAFile { kind: stat.kind }.into())
    }
}

/// The mode bits for a file whose exec bit is to be `exec`, written as git
/// writes it (§7.1, draft 47): execute added wherever read is set, or every
/// execute bit cleared, the other bits as they are.
pub fn exec_mode(mode: u32, exec: bool) -> u32 {
    if exec {
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    }
}

/// The local error an I/O error is (§7.5 "Local failures"): a full disk or
/// quota, or any other.
fn local(e: &io::Error) -> LocalError {
    let full = [rustix::io::Errno::NOSPC, rustix::io::Errno::DQUOT]
        .iter()
        .any(|errno| e.raw_os_error() == Some(errno.raw_os_error()));
    if full {
        LocalError::DiskFull
    } else {
        LocalError::Io
    }
}

/// An error at the path or a target the commit renames to: the path is
/// not what the commit expected (it went, something took a name, a
/// directory is not empty, a parent stopped being a directory, an entry
/// became something else), or a local failure.
fn changed_or_local(e: &io::Error) -> Fail {
    let changed = matches!(
        e.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::AlreadyExists
            | io::ErrorKind::DirectoryNotEmpty
            | io::ErrorKind::NotADirectory
    ) || ParentNotADirectory::of(e).is_some()
        || NotAFile::of(e).is_some();
    if changed {
        Fail::Changed
    } else {
        Fail::Local(local(e))
    }
}

/// Whether `rmdir` refused for a directory not being empty: `ENOTEMPTY`,
/// or `EEXIST`, which POSIX allows instead.
fn not_empty(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists
    )
}

/// The removal of a trash row.
fn remove_trash(folder: FolderId, row: &TrashRow) -> HostWrite {
    HostWrite::RemoveTrash {
        folder,
        trashed_path: row.trashed_path.clone(),
    }
}

#[cfg(test)]
mod tests;
