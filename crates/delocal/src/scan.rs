//! The scanner (DESIGN.md §7.3): a full scan of a folder, reported to the
//! engine as one bracket of events.
//!
//! [`Scan::run`] reports `ScanStarted`, then one `Scanned` for each path it
//! can speak for, then `ScanFinished`, or `ScanAborted` if the root guard
//! stops it. It is one operation (§7.3): it opens the folder root afresh at
//! the start and holds it until the end, and nothing else.
//!
//! **The walk** is depth first, from the root, and opens each directory
//! once, from the descriptor of its parent ([`crate::fs::Dir`]). Every entry
//! is reached by one call relative to the directory it is in, so the walk
//! never asks the filesystem for a path by name (§7.3, names on disk). The
//! names `read_dir` lists become index paths through [`crate::names`]: NFC,
//! with the pairs whose name on disk differs kept for `disk_names` (§11). A
//! name with no index path (not UTF-8, or too long) is unobservable and
//! counted for `status`, never reported; two names whose index paths
//! coincide are reported `Skipped` once, at that path. The walk goes into
//! neither.
//!
//! **Each entry** is stated by name and then:
//!
//! - A directory is opened and listed, then reported: `Unchanged` if its
//!   record is a directory, `Observed` otherwise. The walk then goes in.
//!   One that cannot be opened or listed is reported `Skipped` instead, once,
//!   which covers every tracked path beneath it (§7.3), and the walk does
//!   not go in.
//! - A symlink is read, never followed; its content is its target, so the
//!   fast path compares the target's hash (§7.3).
//! - A file whose size, mtime and exec bit match its record is `Unchanged`,
//!   and is not read (the fast path, [`Entry::unchanged_by_stat`]). Any other
//!   file is hashed on one of the hashing threads, if its change time has
//!   settled ([`hash`]); one that changed within the last 2 s, or changes
//!   while it is hashed, is `Skipped { Unstable }`.
//! - Anything else (a FIFO, a socket, a device) is not an entry (§3.1) and
//!   is not reported, as if absent.
//!
//! Permission denied on any of those calls reports the path
//! `Skipped { PermissionDenied }`; an entry that turned into something else
//! between its stat and its open, `Skipped { Unstable }`; any other error,
//! `Skipped { Io }`. An entry that is gone by the time it is stated or
//! opened is not reported: it is absent. Nothing unobserved is ever left out
//! of a bracket for any other reason, since the bracket's end takes every
//! tracked path it was not told about for deleted.
//!
//! **Reports come back in walk order**, whichever hashing thread finishes
//! first ([`ordered`]), so the same tree always scans to the same events.
//!
//! **Ignored paths** ([`ignore_rules`]) are not reported and not walked
//! into. After the walk, every tracked path the rules ignore is reported
//! `Skipped { Ignored }`, whether or not it is on disk: the record stays,
//! frozen, and a rule that hides a file never reads as its deletion (§7.3).
//!
//! **The root guard** (§7.3). The scan aborts, before anything is observed,
//! if the root cannot be opened, if `.delocal/folder.json` is not a file,
//! if the root cannot be listed, or if `.delocalignore` is there but cannot
//! be read, since then the scan cannot tell what the user meant to leave
//! alone. The marker is checked again, through the same `.delocal`, before
//! `ScanFinished`: a folder deleted wholesale while it was being scanned
//! takes its marker with it, and must not read as a mass deletion. An
//! aborted scan closes the bracket with `ScanAborted`, which announces no
//! deletions (§7.3).
//!
//! **What the host keeps** of a scan is its [`ScanReport`]: the names on
//! disk, the unobservable names, the skips by reason, and the rules left out
//! of `.delocalignore`, all for `status` and the store.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use delocal_engine::path::RESERVED_DIR;
use delocal_engine::{
    ContentHash, Entry, Event, FolderId, Index, Kind, Observed, RelPath, ScanState, SkipReason,
    Timestamp,
};

use crate::fs::{Dir, FileKind, Folder, NotAFile, Stat};
use crate::names::{self, IndexName, Name, Unobservable};
use crate::store::DiskName;

pub mod hash;
pub mod ignore_rules;
pub mod ordered;

use hash::{Hashed, hash_file, hash_target, settled};
use ignore_rules::{IgnoreRules, InvalidRule};
use ordered::Ordered;

/// The root guard's marker, in `.delocal/` (§7.3, §11).
pub const MARKER: &str = "folder.json";

/// The permission bit that is a file's exec bit (§7.1): the owner's
/// execute bit, as git reads it.
pub const EXEC_BIT: u32 = 0o100;

/// Hashing threads at most, however many cores there are: past this the
/// disk, not the CPU, is what a scan waits on.
const MAX_HASHERS: NonZeroUsize = match NonZeroUsize::new(8) {
    Some(n) => n,
    None => NonZeroUsize::MIN,
};

/// The index records the host last wrote for a folder (§11): what the fast
/// path compares against, and which paths are tracked.
pub trait Records {
    /// The live record at `path`.
    fn live(&self, path: &RelPath) -> Option<&Entry>;
    /// Every live record.
    fn live_entries(&self) -> Box<dyn Iterator<Item = &Entry> + '_>;
}

impl Records for Index {
    fn live(&self, path: &RelPath) -> Option<&Entry> {
        Index::live(self, path).map(|record| &record.entry)
    }

    fn live_entries(&self) -> Box<dyn Iterator<Item = &Entry> + '_> {
        Box::new(self.live_records().map(|record| &record.entry))
    }
}

impl Records for BTreeMap<RelPath, Entry> {
    fn live(&self, path: &RelPath) -> Option<&Entry> {
        self.get(path).filter(|entry| !entry.deleted)
    }

    fn live_entries(&self) -> Box<dyn Iterator<Item = &Entry> + '_> {
        Box::new(self.values().filter(|entry| !entry.deleted))
    }
}

/// The time as the host tells it to the engine: the monotonic now of §7.8.
/// A scan reads it as it states each file, for the stability check.
pub trait Clock {
    fn now(&self) -> Timestamp;
}

impl<F: Fn() -> Timestamp> Clock for F {
    fn now(&self) -> Timestamp {
        self()
    }
}

/// How many threads hash for a scan by default: one per core, up to eight.
pub fn default_hashers() -> NonZeroUsize {
    std::thread::available_parallelism().map_or(NonZeroUsize::MIN, |n| n.min(MAX_HASHERS))
}

/// One full scan of one folder. See the module docs.
pub struct Scan<'a> {
    /// The folder the events are for.
    pub id: FolderId,
    /// Where it is on disk.
    pub folder: &'a dyn Folder,
    /// Its index records, as the host last wrote them.
    pub records: &'a dyn Records,
    pub clock: &'a dyn Clock,
    /// Threads to hash on.
    pub hashers: NonZeroUsize,
}

/// What a scan found, besides its events: what the host keeps for `status`
/// and the store.
#[derive(Debug, Default)]
pub struct ScanReport {
    /// Why the scan ended with `ScanAborted`, if it did.
    pub aborted: Option<Abort>,
    /// Every entry whose name on disk is not its index path's last
    /// component (§7.3, `disk_names` in §11).
    pub disk_names: Vec<DiskName>,
    /// Every unobservable name the scan met (§7.3), where it is on disk:
    /// `status` names them, and the user renames one.
    pub unobservable: Vec<(PathBuf, Unobservable)>,
    /// The paths reported `Skipped`, counted by reason (§7.3).
    pub skipped: BTreeMap<SkipReason, u64>,
    /// The lines of `.delocalignore` that were left out.
    pub invalid_rules: Vec<InvalidRule>,
}

/// Why a scan was aborted (§7.3): each is a case of the root guard, and
/// none observed anything.
#[derive(Debug)]
pub enum Abort {
    /// The folder root could not be opened: the folder, or the disk it is
    /// on, is not there.
    Root(io::Error),
    /// `.delocal/folder.json` is not a file at the root: missing, or not a
    /// file, or it could not be stated. Checked at the start and again
    /// before the scan finishes.
    Marker(io::Error),
    /// The folder root could not be listed.
    List(io::Error),
    /// `.delocalignore` is at the root but could not be read, or is not a
    /// file, so the scan cannot tell which paths are ignored.
    IgnoreFile(io::Error),
}

/// A report for one path, or nothing (the path is absent).
type Found = Option<(RelPath, ScanState)>;

impl Scan<'_> {
    /// Scan the folder, handing each event to `emit` as it is ready.
    pub fn run(&self, emit: &mut dyn FnMut(Event)) -> ScanReport {
        let folder = self.id;
        let mut report = ScanReport::default();
        emit(Event::ScanStarted { folder });
        match self.walk(&mut report, emit) {
            Ok(()) => emit(Event::ScanFinished { folder }),
            Err(abort) => {
                report.aborted = Some(abort);
                emit(Event::ScanAborted { folder });
            }
        }
        report
    }

    fn walk(&self, report: &mut ScanReport, emit: &mut dyn FnMut(Event)) -> Result<(), Abort> {
        let fs = self.folder.open().map_err(Abort::Root)?;
        let root: Arc<dyn Dir> = Arc::from(fs.open_dir(Path::new("")).map_err(Abort::Root)?);
        let delocal = marker(&*root)?;
        let rules = IgnoreRules::read(&*root).map_err(Abort::IgnoreFile)?;
        report.invalid_rules = rules.invalid().to_vec();
        let names = root.read_dir().map_err(Abort::List)?;

        std::thread::scope(|scope| {
            let mut walker = Walker {
                records: self.records,
                clock: self.clock,
                rules: &rules,
                disk_names: &mut report.disk_names,
                unobservable: &mut report.unobservable,
                pipe: Ordered::start(scope, self.hashers, &hash_job),
                out: Out {
                    folder: self.id,
                    emit,
                    skipped: &mut report.skipped,
                },
            };
            walker.walk(root, names);
            walker.ignored_records();
            let Walker { pipe, mut out, .. } = walker;
            pipe.finish(&mut |found| out.take(found));
        });
        // A hashing thread that panicked panics the scope above, so the
        // bracket is never finished, and its unreported path is never taken
        // for absent.
        check_marker(&*delocal)
    }
}

/// Open `.delocal` and check the marker in it. `.delocal` stays held, so
/// the end of the scan checks the same marker again.
fn marker(root: &dyn Dir) -> Result<Box<dyn Dir>, Abort> {
    let delocal = root
        .open_dir(OsStr::new(RESERVED_DIR))
        .map_err(Abort::Marker)?;
    check_marker(&*delocal)?;
    Ok(delocal)
}

fn check_marker(delocal: &dyn Dir) -> Result<(), Abort> {
    match delocal.lstat(OsStr::new(MARKER)) {
        Ok(stat) if stat.kind == FileKind::File => Ok(()),
        Ok(stat) => Err(Abort::Marker(NotAFile { kind: stat.kind }.into())),
        Err(e) => Err(Abort::Marker(e)),
    }
}

/// The walk's state: everything but the stack of directories it is in.
struct Walker<'a, 'e> {
    records: &'a dyn Records,
    clock: &'a dyn Clock,
    rules: &'a IgnoreRules,
    disk_names: &'a mut Vec<DiskName>,
    unobservable: &'a mut Vec<(PathBuf, Unobservable)>,
    pipe: Ordered<Job, Found>,
    out: Out<'a, 'e>,
}

/// Where reports go once it is their turn.
struct Out<'a, 'e> {
    folder: FolderId,
    emit: &'e mut dyn FnMut(Event),
    skipped: &'a mut BTreeMap<SkipReason, u64>,
}

impl Out<'_, '_> {
    /// Report what one item found. `None` is a job that panicked, whose
    /// path goes unreported: see the end of [`Scan::walk`].
    fn take(&mut self, found: Option<Found>) {
        let Some(Some((path, state))) = found else {
            return;
        };
        if let ScanState::Skipped { reason } = state {
            *self.skipped.entry(reason).or_default() += 1;
        }
        (self.emit)(Event::Scanned {
            folder: self.folder,
            path,
            state,
        });
    }
}

/// A directory the walk is in: held, with the names it has yet to look at.
struct Level {
    dir: Arc<dyn Dir>,
    /// Where it is on disk, relative to the root.
    disk: PathBuf,
    names: std::vec::IntoIter<(OsString, Name)>,
    /// Index paths already reported for two coinciding names here.
    coinciding: BTreeSet<RelPath>,
}

impl Level {
    fn new(dir: Arc<dyn Dir>, disk: PathBuf, index: Option<&RelPath>, names: &[OsString]) -> Self {
        Self {
            dir,
            disk,
            names: names::dir(index, names).into_iter(),
            coinciding: BTreeSet::new(),
        }
    }
}

/// A file for a hashing thread.
struct Job {
    dir: Arc<dyn Dir>,
    name: OsString,
    path: RelPath,
    /// What the walk stated.
    stat: Stat,
    /// What it saw, all but the hash.
    seen: Observed,
}

fn hash_job(job: Job) -> Found {
    let state = match hash_file(&*job.dir, &job.name, &job.stat) {
        Hashed::Stable(hash) => ScanState::Observed(Observed { hash, ..job.seen }),
        Hashed::Unstable => unstable(),
        Hashed::Vanished => return None,
        Hashed::Failed(e) => skipped(&e),
    };
    Some((job.path, state))
}

impl Walker<'_, '_> {
    /// Walk the tree below the root, held as `root`, whose names are
    /// `names`: depth first, each directory's entries in the order it lists
    /// them, a directory's report before its contents'. A stack, not
    /// recursion, so a deep tree costs heap, not the thread's stack.
    fn walk(&mut self, root: Arc<dyn Dir>, names: Vec<OsString>) {
        let mut stack = vec![Level::new(root, PathBuf::new(), None, &names)];
        while let Some(level) = stack.last_mut() {
            let Some((disk, name)) = level.names.next() else {
                stack.pop();
                continue;
            };
            let dir = Arc::clone(&level.dir);
            let place = level.disk.join(&disk);
            match name {
                Name::Reserved => {}
                Name::Unobservable(why) => {
                    if let Unobservable::Coincides { path } = &why
                        && let Some(reason) = why.skip_reason()
                        && level.coinciding.insert(path.clone())
                    {
                        self.ready(path.clone(), ScanState::Skipped { reason });
                    }
                    self.unobservable.push((place, why));
                }
                Name::Path(IndexName { path, differs }) => {
                    if let Some(entered) = self.entry(dir, &disk, path, differs) {
                        let (dir, path, names) = entered;
                        stack.push(Level::new(dir, place, Some(&path), &names));
                    }
                }
            }
        }
    }

    /// Look at the entry `disk` in `dir`, at index path `path`. Returns a
    /// directory to walk into, held, with its index path and names.
    fn entry(
        &mut self,
        dir: Arc<dyn Dir>,
        disk: &OsStr,
        path: RelPath,
        differs: bool,
    ) -> Option<(Arc<dyn Dir>, RelPath, Vec<OsString>)> {
        let stat = match dir.lstat(disk) {
            Ok(stat) => stat,
            Err(e) if gone(&e) => return None,
            Err(e) => {
                // Whatever it is, the rules may leave it alone.
                if !(self.rules.ignores_entry(&path, false)
                    && self.rules.ignores_entry(&path, true))
                {
                    self.keep_name(&path, disk, differs);
                    self.ready(path, skipped(&e));
                }
                return None;
            }
        };
        let kind = match stat.kind {
            FileKind::File => Kind::File,
            FileKind::Dir => Kind::Dir,
            FileKind::Symlink => Kind::Symlink,
            FileKind::Other => return None,
        };
        if self.rules.ignores_entry(&path, kind == Kind::Dir) {
            return None;
        }
        self.keep_name(&path, disk, differs);
        match kind {
            Kind::Dir => return self.directory(&*dir, disk, path),
            Kind::Symlink => self.symlink(&*dir, disk, path),
            Kind::File => self.file(dir, disk, path, stat),
        }
        None
    }

    /// A directory: opened and listed, then reported, then walked into.
    fn directory(
        &mut self,
        parent: &dyn Dir,
        disk: &OsStr,
        path: RelPath,
    ) -> Option<(Arc<dyn Dir>, RelPath, Vec<OsString>)> {
        let opened = parent.open_dir(disk).and_then(|dir| {
            let names = dir.read_dir()?;
            Ok((dir, names))
        });
        match opened {
            Ok((dir, names)) => {
                let seen = Observed {
                    kind: Kind::Dir,
                    size: 0,
                    mtime_ns: 0,
                    exec: false,
                    hash: ContentHash::EMPTY,
                };
                self.ready(path.clone(), self.fast(&path, seen));
                Some((Arc::from(dir), path, names))
            }
            Err(e) if gone(&e) => None,
            // Once, for everything beneath it (§7.3).
            Err(e) => {
                self.ready(path, skipped(&e));
                None
            }
        }
    }

    /// A symlink: its target is its content (§7.1).
    fn symlink(&mut self, dir: &dyn Dir, disk: &OsStr, path: RelPath) {
        let state = match dir.read_link(disk) {
            Ok(target) => {
                let target = target.as_os_str().as_bytes();
                let seen = Observed {
                    kind: Kind::Symlink,
                    size: target.len() as u64,
                    mtime_ns: 0,
                    exec: false,
                    hash: hash_target(target),
                };
                self.fast(&path, seen)
            }
            Err(e) if gone(&e) => return,
            // Not a symlink any more: it changed while the scan looked.
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => unstable(),
            Err(e) => skipped(&e),
        };
        self.ready(path, state);
    }

    /// A file: unchanged by the fast path, or hashed once it has settled.
    fn file(&mut self, dir: Arc<dyn Dir>, disk: &OsStr, path: RelPath, stat: Stat) {
        let seen = Observed {
            kind: Kind::File,
            size: stat.size,
            mtime_ns: stat.mtime_ns,
            exec: stat.mode & EXEC_BIT != 0,
            hash: ContentHash::EMPTY,
        };
        if self.unchanged(&path, &seen) {
            self.ready(path, ScanState::Unchanged);
        } else if !settled(stat.ctime_ns, self.clock.now()) {
            self.ready(path, unstable());
        } else {
            let job = Job {
                dir,
                name: disk.to_os_string(),
                path,
                stat,
                seen,
            };
            self.pipe.submit(job, &mut |found| self.out.take(found));
        }
    }

    /// Every tracked path the rules ignore, reported `Skipped`, on disk or
    /// not: frozen until the rule goes (§7.3).
    fn ignored_records(&mut self) {
        let records = self.records;
        for entry in records.live_entries() {
            if self.rules.ignores(&entry.path, entry.kind == Kind::Dir) {
                let reason = SkipReason::Ignored;
                self.ready(entry.path.clone(), ScanState::Skipped { reason });
            }
        }
    }

    /// The fast path (§7.3): whether the live record at `path` says `seen`
    /// is unchanged.
    fn unchanged(&self, path: &RelPath, seen: &Observed) -> bool {
        self.records
            .live(path)
            .is_some_and(|record| record.unchanged_by_stat(seen))
    }

    /// `Unchanged` by the fast path, or `seen`.
    fn fast(&self, path: &RelPath, seen: Observed) -> ScanState {
        if self.unchanged(path, &seen) {
            ScanState::Unchanged
        } else {
            ScanState::Observed(seen)
        }
    }

    /// Keep the pair if the name on disk is not the index path's (§7.3).
    fn keep_name(&mut self, path: &RelPath, disk: &OsStr, differs: bool) {
        if differs {
            self.disk_names.push(DiskName {
                path: path.clone(),
                name: disk.to_os_string(),
            });
        }
    }

    /// A report that needs no hashing, in its place.
    fn ready(&mut self, path: RelPath, state: ScanState) {
        self.pipe
            .ready(Some((path, state)), &mut |found| self.out.take(found));
    }
}

/// The entry is gone: it is absent, and not reported.
fn gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

fn unstable() -> ScanState {
    ScanState::Skipped {
        reason: SkipReason::Unstable,
    }
}

/// `Skipped`, for an error on the way to observing a path.
fn skipped(e: &io::Error) -> ScanState {
    let reason = if e.kind() == io::ErrorKind::PermissionDenied {
        SkipReason::PermissionDenied
    } else if NotAFile::of(e).is_some() || e.kind() == io::ErrorKind::NotADirectory {
        // It was stated as one kind and opened as another.
        SkipReason::Unstable
    } else {
        SkipReason::Io
    };
    ScanState::Skipped { reason }
}

#[cfg(test)]
mod tests;
