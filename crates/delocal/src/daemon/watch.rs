//! The watcher (DESIGN.md §7.3): every folder root watched recursively,
//! its events debounced, and each path they name observed as a scan would
//! observe it.
//!
//! **Events.** `notify` watches each root (inotify on Linux, FSEvents on
//! macOS), never following a symlink, and `notify-debouncer-full` holds each
//! event for 2 s before it passes it on, merging what came for the same
//! path meanwhile. An event says only that a path changed, never what it
//! now is: [`sort`] turns each into the index path it names, and the daemon
//! observes that path, with [`crate::scan::Observer`] behind the root guard
//! ([`crate::commit::Commit::observe`]), and reports what it finds then.
//! Each observation therefore carries the time of its own `stat` (§7.3,
//! draft 56), not the event's, and whatever the event said was coalesced or
//! reordered on the way (FSEvents merges a path's changes into one record
//! of flags) cannot make it report something that is not on disk.
//!
//! **What an event becomes** ([`Seen`]):
//! - A path under the folder becomes its index path, component by
//!   component through [`crate::names`], with the names on disk that are
//!   not their index paths' kept beside it so the path can be reached by
//!   them. A name with no index path (not UTF-8, too long) is
//!   unobservable, and the event is dropped: the scan names it for
//!   `status`. So is anything in `.delocal/`.
//! - A path that came (created, renamed to) or went (removed, renamed
//!   from) is marked so: a directory that does may have brought contents
//!   with it, or taken them, and no event names those; and an observation
//!   that finds the opposite of what the event said is taken again later,
//!   since the debouncer drops a create and a removal that follow it as a
//!   pair, and the observation may have fallen between them.
//! - A removal or rename of the root itself is the root guard's case
//!   (§7.3): [`Input::Guard`], and nothing is observed.
//! - An event that says events were lost (an inotify queue overflow,
//!   FSEvents' "must scan subdirectories") asks for a full scan of the
//!   folder: [`Input::Rescan`].
//!
//! Errors from the watcher are logged and nothing more: the hourly scan is
//! the ground truth for whatever a watcher misses (§7.3).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use delocal_engine::{FolderId, RelPath};
use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind, RemoveKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer_opt,
};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, warn};

use super::core::Input;
use crate::names::{self, IndexName, Name};

/// How long the debouncer holds an event before passing it on (§7.3).
pub const QUIET: Duration = Duration::from_secs(2);

/// One path an event named, as the index knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub path: RelPath,
    /// The path came: it was created, or renamed to.
    pub came: bool,
    /// The path went: it was removed, or renamed from.
    pub went: bool,
    /// A second look, at a path whose first look found the opposite of
    /// what its event said: taken whole, and never asking for a third.
    pub again: bool,
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

/// The watcher. See the module docs.
pub struct Watcher {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    roots: Arc<Mutex<Vec<Root>>>,
}

impl Watcher {
    /// A watcher that sends what its events become to `inputs`, watching
    /// nothing yet.
    pub(crate) fn new(inputs: tokio_mpsc::Sender<Input>) -> notify::Result<Self> {
        let roots: Arc<Mutex<Vec<Root>>> = Arc::default();
        let known = Arc::clone(&roots);
        let handler = move |result: DebounceEventResult| {
            let sorted = {
                let roots = known.lock().unwrap_or_else(PoisonError::into_inner);
                sort(result, &roots)
            };
            for input in sorted {
                // Fails only once the engine task has stopped.
                let _ = inputs.blocking_send(input);
            }
        };
        let config = notify::Config::default().with_follow_symlinks(false);
        let debouncer = new_debouncer_opt::<_, RecommendedWatcher, RecommendedCache>(
            QUIET,
            None,
            handler,
            RecommendedCache::new(),
            config,
        )?;
        Ok(Self { debouncer, roots })
    }

    /// Watch `folder`'s root at `root`, recursively, in place of any watch
    /// it had.
    pub fn watch(&mut self, folder: FolderId, root: &Path) -> notify::Result<()> {
        self.unwatch(folder);
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        self.debouncer.watch(root, RecursiveMode::Recursive)?;
        self.locked().push(Root {
            folder,
            configured: root.to_path_buf(),
            canonical,
        });
        Ok(())
    }

    /// Stop watching `folder`, if it is watched.
    pub fn unwatch(&mut self, folder: FolderId) {
        let gone: Vec<Root> = {
            let mut roots = self.locked();
            let (gone, kept) = roots.drain(..).partition(|r| r.folder == folder);
            *roots = kept;
            gone
        };
        for root in gone {
            // A root that is gone has no watch left to remove.
            let _ = self.debouncer.unwatch(&root.configured);
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<Root>> {
        self.roots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What a batch of debounced events becomes for the engine task (see the
/// module docs): per folder, the paths to observe, in the order the events
/// named them; a guard for a folder whose root went; a full scan for one
/// whose events were lost.
fn sort(result: DebounceEventResult, roots: &[Root]) -> Vec<Input> {
    let events = match result {
        Ok(events) => events,
        Err(errors) => {
            for error in errors {
                warn!(%error, "watcher error");
            }
            return Vec::new();
        }
    };
    let mut inputs = Vec::new();
    let mut seen: BTreeMap<FolderId, Vec<Seen>> = BTreeMap::new();
    for event in &events {
        if event.need_rescan() {
            let folders: Vec<FolderId> = if event.paths.is_empty() {
                roots.iter().map(|r| r.folder).collect()
            } else {
                event
                    .paths
                    .iter()
                    .filter_map(|p| place(p, roots).map(|(f, _)| f))
                    .collect()
            };
            for folder in folders {
                inputs.push(Input::Rescan { folder });
            }
            continue;
        }
        for (path, came, went) in named(event) {
            let Some((folder, rel)) = place(path, roots) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                if root_went(&event.kind) {
                    inputs.push(Input::Guard {
                        folder,
                        root_gone: true,
                    });
                }
                continue;
            }
            match index_path(&rel) {
                Some((path, names)) => seen.entry(folder).or_default().push(Seen {
                    path,
                    came,
                    went,
                    again: false,
                    names,
                }),
                None => debug!(path = %rel.display(), "unobservable or reserved; not watched"),
            }
        }
    }
    for (folder, paths) in seen {
        inputs.push(Input::Watched { folder, paths });
    }
    inputs
}

/// The paths an event names, each with whether it came and whether it
/// went. Opening and reading a file change nothing; closing one written to
/// is a change. A rename that does not say which way (FSEvents') could be
/// either.
fn named(event: &DebouncedEvent) -> Vec<(&Path, bool, bool)> {
    let paths = event.paths.iter().map(PathBuf::as_path);
    let (came, went) = match &event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => (false, false),
        EventKind::Access(_) | EventKind::Other => return Vec::new(),
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => (true, false),
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            (false, true)
        }
        // From one path to another: the first went, the second came.
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            return paths.enumerate().map(|(i, p)| (p, i > 0, i == 0)).collect();
        }
        EventKind::Modify(ModifyKind::Name(_)) => (true, true),
        _ => (false, false),
    };
    paths.map(|p| (p, came, went)).collect()
}

/// Whether an event at a folder's root says the root itself went: removed,
/// or renamed away (FSEvents reports a root that changed as a rename).
fn root_went(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Remove(RemoveKind::Any | RemoveKind::Folder | RemoveKind::Other)
            | EventKind::Modify(ModifyKind::Name(
                RenameMode::From | RenameMode::Any | RenameMode::Both
            ))
    )
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
    use std::time::Instant;

    use notify::Event;
    use notify::event::{CreateKind, DataChange, Flag};

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

    fn event(kind: EventKind, paths: &[&str]) -> DebouncedEvent {
        let mut event = Event::new(kind);
        for path in paths {
            event = event.add_path(PathBuf::from(path));
        }
        DebouncedEvent::new(event, Instant::now())
    }

    fn seen(path: &str, came: bool, went: bool) -> Seen {
        Seen {
            path: p(path),
            came,
            went,
            again: false,
            names: Vec::new(),
        }
    }

    #[test]
    fn events_become_the_index_paths_they_name_per_folder() {
        let events = vec![
            event(EventKind::Create(CreateKind::File), &["/var/sync/a/f"]),
            // FSEvents spells the root canonically.
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &["/private/var/sync/g"],
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/home/me/notes/old", "/home/me/notes/new"],
            ),
            event(EventKind::Remove(RemoveKind::File), &["/home/me/notes/x"]),
            // Elsewhere, reading, and `.delocal/` are not the folder's.
            event(EventKind::Create(CreateKind::File), &["/tmp/elsewhere"]),
            event(EventKind::Access(AccessKind::Read), &["/var/sync/read"]),
            event(
                EventKind::Create(CreateKind::File),
                &["/var/sync/.delocal/tmp/t"],
            ),
        ];
        let inputs = sort(Ok(events), &roots());
        let watched: Vec<(FolderId, Vec<Seen>)> = inputs
            .into_iter()
            .map(|input| match input {
                Input::Watched { folder, paths } => (folder, paths),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            watched,
            [
                (A, vec![seen("a/f", true, false), seen("g", false, false)]),
                (
                    B,
                    vec![
                        seen("old", false, true),
                        seen("new", true, false),
                        seen("x", false, true)
                    ]
                ),
            ]
        );
    }

    #[test]
    fn a_name_that_is_not_its_index_path_s_travels_with_the_path() {
        let nfd = "/var/sync/cafe\u{301}/menu";
        let inputs = sort(
            Ok(vec![event(EventKind::Create(CreateKind::File), &[nfd])]),
            &roots(),
        );
        let [Input::Watched { folder: A, paths }] = &inputs[..] else {
            panic!("{inputs:?}")
        };
        assert_eq!(paths[0].path, p("caf\u{e9}/menu"));
        assert_eq!(
            paths[0].names,
            [(p("caf\u{e9}"), OsString::from("cafe\u{301}"))]
        );
    }

    #[test]
    fn the_root_going_is_the_guard_s_and_lost_events_ask_for_a_scan() {
        let events = vec![
            event(EventKind::Remove(RemoveKind::Folder), &["/var/sync"]),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/private/var/sync"],
            ),
            // A change to the root's own metadata is neither.
            event(
                EventKind::Modify(ModifyKind::Metadata(notify::event::MetadataKind::Any)),
                &["/home/me/notes"],
            ),
            DebouncedEvent::new(
                Event::new(EventKind::Other).set_flag(Flag::Rescan),
                Instant::now(),
            ),
        ];
        let inputs = sort(Ok(events), &roots());
        let kinds: Vec<String> = inputs
            .iter()
            .map(|input| match input {
                Input::Guard { folder, root_gone } => {
                    format!("guard {} {root_gone}", folder.short())
                }
                Input::Rescan { folder } => format!("rescan {}", folder.short()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "guard 01010101 true",
                "guard 01010101 true",
                "rescan 01010101",
                "rescan 02020202"
            ]
        );
    }

    #[test]
    fn errors_become_nothing_but_a_log_line() {
        let error = notify::Error::generic("lost");
        assert!(sort(Err(vec![error]), &roots()).is_empty());
    }
}
