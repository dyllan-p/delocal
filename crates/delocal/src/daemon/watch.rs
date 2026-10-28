//! The watcher (DESIGN.md §7.3, draft 67): every folder root watched
//! recursively, `notify`'s raw events debounced here, and each path they
//! name observed as it is on disk.
//!
//! **Raw events, debounced here.** `notify` watches each root (inotify on
//! Linux, FSEvents on macOS), never following a symlink. Every path any of
//! its events names is held in [`Quiet`] until it has had 2 s of quiet, no
//! event having named it for 2 s, and is then passed on to be observed.
//! That covers both ends of a rename, and every flag of a coalesced
//! FSEvents record, since notify gives each flag an event of its own naming
//! the record's path. Nothing is merged and nothing is dropped.
//!
//! **An event's kind is never trusted,** only the path it names. FSEvents'
//! kinds are a record's flags accumulated over time and say little about
//! what happened last, and whatever happened, what is on disk once the
//! path is quiet is what the engine needs. One kind is used: an event that
//! says only that something was opened or read names no change. The
//! daemon's own scans and observations open everything they look at, and a
//! watcher that held those would watch itself for ever.
//!
//! **What a path becomes** ([`sort`]):
//! - A path under the folder becomes its index path, component by
//!   component through [`crate::names`], with the names on disk along it
//!   that are not their index names', so the path can be reached by them.
//!   A name with no index path (not UTF-8, too long) is unobservable, and
//!   the event is dropped: the scan names it for `status`. So is anything
//!   in `.delocal/`.
//! - The root itself becomes a check of the root guard (§7.3), with
//!   nothing to observe: the executor finds whether the marker is there.
//!
//! **Lost events.** An event that says a subtree must be rescanned
//! (FSEvents' "must scan subdirectories") brings a walk of that subtree:
//! its path is passed on at once, and a directory is always observed whole
//! ([`super::executor`]). One that names no path, or names the root
//! (inotify's queue overflow), brings a full scan: [`Input::Rescan`].
//!
//! Errors from the watcher are logged and nothing more: the hourly scan is
//! the ground truth for whatever a watcher misses (§7.3).

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use delocal_engine::{FolderId, RelPath};
use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, warn};

use super::core::Input;
use crate::names::{self, IndexName, Name};

/// How long a path must go without an event before it is observed (§7.3).
pub const QUIET: Duration = Duration::from_secs(2);

/// How often the held paths are looked through for those that are quiet.
const TICK: Duration = Duration::from_millis(250);

/// One path the events named, as the index knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub path: RelPath,
    /// The names on disk along the path that are not their index paths',
    /// by the index path each names, as the event spelt them.
    pub names: Vec<(RelPath, OsString)>,
}

/// A folder the watcher watches: where it is, and where the platform may
/// say it is (FSEvents reports the canonical path, `/private/var` for
/// `/var` on macOS).
#[derive(Clone, Debug)]
struct Root {
    folder: FolderId,
    configured: PathBuf,
    canonical: PathBuf,
}

/// What the raw events named and is not yet passed on.
#[derive(Debug, Default)]
struct Quiet {
    /// Every path an event named, with when the last one did.
    last: HashMap<PathBuf, Instant>,
    /// Subtrees an event said must be rescanned: passed on at once.
    rescan: Vec<PathBuf>,
    /// An event said events were lost and named nothing: everything is
    /// scanned.
    everything: bool,
}

/// What is due to be passed on.
#[derive(Debug, Default, PartialEq, Eq)]
struct Due {
    quiet: Vec<PathBuf>,
    rescan: Vec<PathBuf>,
    everything: bool,
}

impl Quiet {
    /// Hold what one raw event names, at `now`.
    fn hold(&mut self, event: Event, now: Instant) {
        if event.need_rescan() {
            if event.paths.is_empty() {
                self.everything = true;
            } else {
                self.rescan.extend(event.paths);
            }
            return;
        }
        if opened_or_read(&event.kind) {
            return;
        }
        for path in event.paths {
            self.last.insert(path, now);
        }
    }

    /// Take every path that has had [`QUIET`] by `now`, and the rescans.
    fn take(&mut self, now: Instant) -> Due {
        let mut quiet: Vec<PathBuf> = Vec::new();
        self.last.retain(|path, at| {
            let due = now.saturating_duration_since(*at) >= QUIET;
            if due {
                quiet.push(path.clone());
            }
            !due
        });
        quiet.sort();
        Due {
            quiet,
            rescan: std::mem::take(&mut self.rescan),
            everything: std::mem::take(&mut self.everything),
        }
    }
}

/// Whether an event says only that something was opened or read: no
/// change. A file closed after it was written is a change.
fn opened_or_read(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(
            AccessKind::Open(_)
                | AccessKind::Read
                | AccessKind::Close(AccessMode::Read | AccessMode::Execute)
        )
    )
}

/// The watcher. See the module docs.
pub struct Watcher {
    watcher: RecommendedWatcher,
    roots: Arc<Mutex<Vec<Root>>>,
    #[cfg(test)]
    quiet: Arc<Mutex<Quiet>>,
    /// Set while the thread that passes quiet paths on is passing some.
    #[cfg(test)]
    passing: Arc<AtomicBool>,
    /// Tells that thread to stop.
    stop: Arc<AtomicBool>,
}

impl Watcher {
    /// A watcher that sends what its events become to `inputs`, watching
    /// nothing yet.
    pub(crate) fn new(inputs: tokio_mpsc::Sender<Input>) -> notify::Result<Self> {
        let quiet: Arc<Mutex<Quiet>> = Arc::default();
        let roots: Arc<Mutex<Vec<Root>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&quiet);
        let handler = move |result: notify::Result<Event>| match result {
            Ok(event) => locked(&held).hold(event, Instant::now()),
            Err(error) => warn!(%error, "watcher error"),
        };
        let config = notify::Config::default().with_follow_symlinks(false);
        let watcher = RecommendedWatcher::new(handler, config)?;
        let passing = Arc::new(AtomicBool::new(false));
        let (known, stopping) = (Arc::clone(&roots), Arc::clone(&stop));
        let (holding, sending) = (Arc::clone(&quiet), Arc::clone(&passing));
        thread::Builder::new()
            .name("delocal-watch".to_owned())
            .spawn(move || {
                // Ends when the watcher goes, or when the engine task does.
                while !stopping.load(Ordering::SeqCst) {
                    thread::sleep(TICK);
                    sending.store(true, Ordering::SeqCst);
                    let due = locked(&holding).take(Instant::now());
                    let sorted = sort(due, &locked(&known));
                    for input in sorted {
                        if inputs.blocking_send(input).is_err() {
                            return;
                        }
                    }
                    sending.store(false, Ordering::SeqCst);
                }
            })
            .map_err(notify::Error::io)?;
        Ok(Self {
            watcher,
            roots,
            #[cfg(test)]
            quiet,
            #[cfg(test)]
            passing,
            stop,
        })
    }

    /// Watch `folder`'s root at `root`, recursively, in place of any watch
    /// it had.
    pub fn watch(&mut self, folder: FolderId, root: &Path) -> notify::Result<()> {
        self.unwatch(folder);
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        self.watcher.watch(root, RecursiveMode::Recursive)?;
        locked(&self.roots).push(Root {
            folder,
            configured: root.to_path_buf(),
            canonical,
        });
        Ok(())
    }

    /// Whether a path an event named is still held, or on its way to the
    /// engine task: for tests that wait for the daemon to be idle.
    #[cfg(test)]
    pub fn holding(&self) -> bool {
        let quiet = locked(&self.quiet);
        self.passing.load(Ordering::SeqCst)
            || !quiet.last.is_empty()
            || !quiet.rescan.is_empty()
            || quiet.everything
    }

    /// Stop watching `folder`, if it is watched.
    pub fn unwatch(&mut self, folder: FolderId) {
        let gone: Vec<Root> = {
            let mut roots = locked(&self.roots);
            let (gone, kept) = roots.drain(..).partition(|r| r.folder == folder);
            *roots = kept;
            gone
        };
        for root in gone {
            // A root that is gone has no watch left to remove.
            let _ = self.watcher.unwatch(&root.configured);
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the due paths become for the engine task (see the module docs):
/// per folder, the paths to observe, in path order; a full scan for each
/// folder whose events were lost.
fn sort(due: Due, roots: &[Root]) -> Vec<Input> {
    let mut inputs = Vec::new();
    let mut rescans: Vec<FolderId> = Vec::new();
    if due.everything {
        rescans.extend(roots.iter().map(|r| r.folder));
    }
    // A folder with an entry here is passed on, even with nothing to
    // observe: an event named its root, so its guard is checked.
    let mut seen: BTreeMap<FolderId, Vec<Seen>> = BTreeMap::new();
    for path in &due.rescan {
        let Some((folder, rel)) = place(path, roots) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            rescans.push(folder);
        } else if let Some((path, names)) = index_path(&rel) {
            seen.entry(folder).or_default().push(Seen { path, names });
        }
    }
    for path in &due.quiet {
        let Some((folder, rel)) = place(path, roots) else {
            continue;
        };
        let paths = seen.entry(folder).or_default();
        if rel.as_os_str().is_empty() {
            continue;
        }
        match index_path(&rel) {
            Some((path, names)) => paths.push(Seen { path, names }),
            None => debug!(path = %rel.display(), "unobservable or reserved; not watched"),
        }
    }
    rescans.sort();
    rescans.dedup();
    for folder in rescans {
        seen.remove(&folder);
        inputs.push(Input::Rescan { folder });
    }
    for (folder, paths) in seen {
        inputs.push(Input::Watched { folder, paths });
    }
    inputs
}

/// The folder `path` is in, and the path relative to its root.
fn place(path: &Path, roots: &[Root]) -> Option<(FolderId, PathBuf)> {
    roots.iter().find_map(|root| {
        [&root.canonical, &root.configured]
            .into_iter()
            .find_map(|base| path.strip_prefix(base).ok())
            .map(|rel| (root.folder, rel.to_path_buf()))
    })
}

/// The index path of `rel`, a path relative to a folder root, with the
/// names on disk along it that are not their index paths'. `None` for a
/// path in `.delocal/`, or one with a name that has no index path.
fn index_path(rel: &Path) -> Option<(RelPath, Vec<(RelPath, OsString)>)> {
    let mut path: Option<RelPath> = None;
    let mut names = Vec::new();
    for component in rel.components() {
        let Component::Normal(disk) = component else {
            return None;
        };
        match names::name(path.as_ref(), disk) {
            Name::Path(IndexName {
                path: index,
                differs,
            }) => {
                if differs {
                    names.push((index.clone(), disk.to_os_string()));
                }
                path = Some(index);
            }
            Name::Reserved | Name::Unobservable(_) => return None,
        }
    }
    path.map(|path| (path, names))
}

#[cfg(test)]
mod tests {
    use notify::event::{CreateKind, DataChange, Flag, ModifyKind, RemoveKind, RenameMode};

    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    const A: FolderId = FolderId::from_bytes([1; 16]);
    const B: FolderId = FolderId::from_bytes([2; 16]);

    fn roots() -> Vec<Root> {
        vec![
            Root {
                folder: A,
                configured: "/var/sync".into(),
                canonical: "/private/var/sync".into(),
            },
            Root {
                folder: B,
                configured: "/home/me/notes".into(),
                canonical: "/home/me/notes".into(),
            },
        ]
    }

    fn event(kind: EventKind, paths: &[&str]) -> Event {
        let mut event = Event::new(kind);
        for path in paths {
            event = event.add_path(PathBuf::from(path));
        }
        event
    }

    fn seen(path: &str) -> Seen {
        Seen {
            path: p(path),
            names: Vec::new(),
        }
    }

    /// §7.3, draft 67: a path is passed on once it has had 2 s of quiet,
    /// and every event that names it in the meantime starts the 2 s again.
    #[test]
    fn a_path_is_passed_on_after_two_seconds_without_an_event() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut quiet = Quiet::default();
        let write = || {
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &["/x"],
            )
        };
        quiet.hold(write(), at(0));
        quiet.hold(write(), at(1_500));
        quiet.hold(
            event(EventKind::Create(CreateKind::File), &["/y"]),
            at(1_000),
        );
        assert!(quiet.take(at(2_999)).quiet.is_empty());
        assert_eq!(quiet.take(at(3_000)).quiet, [PathBuf::from("/y")]);
        assert!(quiet.take(at(3_499)).quiet.is_empty());
        assert_eq!(quiet.take(at(3_500)).quiet, [PathBuf::from("/x")]);
        assert!(quiet.take(at(60_000)).quiet.is_empty());
    }

    /// Every path any event names is held, whatever its kind says: both
    /// ends of a rename, a removal, every flag of a coalesced record. Only
    /// opening and reading are no change.
    #[test]
    fn every_path_of_every_event_is_held_whatever_its_kind() {
        let now = Instant::now();
        let mut quiet = Quiet::default();
        for e in [
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/r/a", "/r/b"],
            ),
            event(EventKind::Remove(RemoveKind::Any), &["/r/c"]),
            // FSEvents: one record's flags, each its own event.
            event(EventKind::Create(CreateKind::File), &["/r/d"]),
            event(EventKind::Remove(RemoveKind::File), &["/r/d"]),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
                &["/r/e"],
            ),
            event(EventKind::Any, &["/r/f"]),
            event(EventKind::Other, &["/r/g"]),
            event(
                EventKind::Access(AccessKind::Close(AccessMode::Write)),
                &["/r/h"],
            ),
            event(
                EventKind::Access(AccessKind::Open(AccessMode::Any)),
                &["/r/opened"],
            ),
            event(EventKind::Access(AccessKind::Read), &["/r/read"]),
            event(
                EventKind::Access(AccessKind::Close(AccessMode::Read)),
                &["/r/closed"],
            ),
        ] {
            quiet.hold(e, now);
        }
        let due = quiet.take(now + QUIET);
        let names: Vec<&str> = due.quiet.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "/r/a", "/r/b", "/r/c", "/r/d", "/r/e", "/r/f", "/r/g", "/r/h"
            ]
        );
    }

    #[test]
    fn quiet_paths_become_the_index_paths_they_name_per_folder() {
        let due = Due {
            quiet: [
                "/var/sync/a/f",
                // FSEvents spells the root canonically.
                "/private/var/sync/g",
                "/home/me/notes/x",
                "/tmp/elsewhere",
                "/var/sync/.delocal/tmp/t",
            ]
            .map(PathBuf::from)
            .to_vec(),
            ..Due::default()
        };
        let watched: Vec<(FolderId, Vec<Seen>)> = sort(due, &roots())
            .into_iter()
            .map(|input| match input {
                Input::Watched { folder, paths } => (folder, paths),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            watched,
            [(A, vec![seen("a/f"), seen("g")]), (B, vec![seen("x")])]
        );
    }

    #[test]
    fn a_name_that_is_not_its_index_path_s_travels_with_the_path() {
        let due = Due {
            quiet: vec![PathBuf::from("/var/sync/cafe\u{301}/menu")],
            ..Due::default()
        };
        let inputs = sort(due, &roots());
        let [Input::Watched { folder: A, paths }] = &inputs[..] else {
            panic!("{inputs:?}")
        };
        assert_eq!(paths[0].path, p("caf\u{e9}/menu"));
        assert_eq!(
            paths[0].names,
            [(p("caf\u{e9}"), OsString::from("cafe\u{301}"))]
        );
    }

    /// An event at the root itself checks the guard, observing nothing.
    #[test]
    fn the_root_itself_is_a_check_of_the_guard() {
        let due = Due {
            quiet: vec![PathBuf::from("/private/var/sync")],
            ..Due::default()
        };
        let inputs = sort(due, &roots());
        assert!(
            matches!(&inputs[..], [Input::Watched { folder: A, paths }] if paths.is_empty()),
            "{inputs:?}"
        );
    }

    /// Lost events: a subtree to rescan is walked at once, and events that
    /// name nothing, or the root, bring a full scan.
    #[test]
    fn lost_events_bring_a_walk_of_the_subtree_or_a_full_scan() {
        let now = Instant::now();
        let mut quiet = Quiet::default();
        let rescan = |paths: &[&str]| event(EventKind::Other, paths).set_flag(Flag::Rescan);
        quiet.hold(rescan(&["/var/sync/deep/tree"]), now);
        quiet.hold(rescan(&["/home/me/notes"]), now);
        // Passed on at once, not after 2 s.
        let due = quiet.take(now);
        assert!(due.quiet.is_empty() && !due.everything);
        let inputs = sort(due, &roots());
        let kinds: Vec<String> = inputs
            .iter()
            .map(|input| match input {
                Input::Rescan { folder } => format!("scan {}", folder.short()),
                Input::Watched { folder, paths } => format!(
                    "walk {} {:?}",
                    folder.short(),
                    paths.iter().map(|s| s.path.to_string()).collect::<Vec<_>>()
                ),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["scan 02020202", "walk 01010101 [\"deep/tree\"]"]);

        quiet.hold(rescan(&[]), now);
        let inputs = sort(quiet.take(now), &roots());
        assert_eq!(inputs.len(), 2);
        assert!(inputs.iter().all(|i| matches!(i, Input::Rescan { .. })));
    }
}
