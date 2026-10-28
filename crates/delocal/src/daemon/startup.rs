//! The daemon's start (DESIGN.md §11 "persistence contract", §7.5 "Two
//! renames, one commit" and "Crash safety", §8.4, §13), in this order:
//!
//! 1. **Open the store**, refusing a schema newer than this binary knows
//!    and leaving that file untouched ([`Store::open`]).
//! 2. **Restore the engine** from each folder's persisted parts and nothing
//!    else ([`Engine::restore`]). A folder the host recorded whose small
//!    rest is missing was never reported by the engine, so it has no parts
//!    and is joined afresh, by its first event.
//! 3. **Each folder's start** ([`Start::run`]): the commit journal's open
//!    rows are resolved (draft 65: undone only where the rename never
//!    happened), the trash's rows reconciled and pruned, and `tmp/` cleared
//!    of every temp file no want claims and no open row names. A folder
//!    whose root or marker is not there has nothing done to it: its rows
//!    stay open, so its paths are skipped until a resolve finds the folder
//!    back (§7.3, the root guard).
//! 4. The writes those make are **durable before the first scan**, which
//!    the engine task starts as soon as it runs.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::Arc;

use delocal_engine::path::RESERVED_DIR;
use delocal_engine::{ContentHash, Engine, Entry, Event, FolderId, Kind, NodeConfig, RelPath};
use tracing::{info, warn};

use super::clock::Clock;
use super::{DaemonError, STORE, Setup};
use crate::commit::{DiskState, Start};
use crate::fs::{FileKind, Folder, Fs};
use crate::reach;
use crate::scan::MARKER;
use crate::scan::stability::Stability;
use crate::store::{Group, Store, Writer};
use crate::tmp;
use crate::trash::Policy;

/// One folder, started.
pub(crate) struct Opened {
    pub id: FolderId,
    pub folder: Arc<dyn Folder>,
    pub records: BTreeMap<RelPath, Entry>,
    pub names: BTreeMap<RelPath, OsString>,
    pub disk: Arc<DiskState>,
    pub stability: Arc<Stability>,
}

/// What the start leaves for the engine task and the executor.
pub(crate) struct Started {
    pub engine: Engine,
    pub writer: Writer,
    pub folders: Vec<Opened>,
    /// `FolderJoined` for each folder with no parts.
    pub joins: Vec<Event>,
}

/// Steps 1 to 4 (see the module docs).
pub(crate) fn start(
    setup: &Setup,
    node: &NodeConfig,
    clock: &Clock,
) -> Result<Started, DaemonError> {
    let store = Store::open(&setup.state_dir.join(STORE))?;
    let mut parts = store.load()?;
    let mut folders = Vec::new();
    let mut joins = Vec::new();
    let mut writes = Vec::new();
    for row in store.folders()? {
        let members = store.members(row.id)?;
        let Some(me) = members.iter().find(|m| m.node == node.node_id) else {
            warn!(folder = %row.id.short(), "this machine is not a member; left alone");
            parts.retain(|p| p.id != row.id);
            continue;
        };
        let folder = (setup.folder)(&me.path);
        let disk = Arc::new(DiskState::new());
        let names: BTreeMap<RelPath, OsString> = store
            .disk_names(row.id)?
            .into_iter()
            .map(|n| (n.path, n.name))
            .collect();
        let journal = store.journal(row.id)?;
        let trash = store.trash(row.id)?;
        let restored = parts.iter().find(|p| p.id == row.id);
        // A temp file a want may resume from: one named for a file's hash.
        let hashes: BTreeSet<ContentHash> = restored
            .into_iter()
            .flat_map(|p| p.wants.values())
            .filter(|want| want.entry.kind == Kind::File && !want.entry.deleted)
            .map(|want| want.entry.hash)
            .collect();
        match open_marked(&*folder) {
            Ok(fs) => {
                let started = Start {
                    folder: row.id,
                    journal: &journal,
                    trash: &trash,
                    names: &names,
                    disk: &disk,
                    claimed: &|name| tmp::claimed(name, &hashes),
                    now: clock.now(),
                    policy: Policy::default(),
                }
                .run(&*fs);
                let folder = row.id.short();
                let undone = &started.undone;
                if !journal.is_empty() {
                    info!(
                        %folder, back = ?undone.back, left = ?undone.left, landed = ?undone.landed,
                        "journal resolved"
                    );
                }
                for (path, error) in &undone.failed {
                    warn!(%folder, %path, %error, "journal row not undone; skipped until resolved");
                }
                for (path, _, error) in &undone.unsynced {
                    warn!(%folder, %path, %error, "landed commit not synced; skipped until resolved");
                }
                for (path, error) in &started.unpruned {
                    warn!(%folder, path = %path.display(), %error, "trash entry not pruned");
                }
                if let Some(error) = &started.tmp_error {
                    warn!(%folder, %error, "tmp/ not cleared");
                }
                writes.extend(started.writes);
            }
            Err(error) => {
                warn!(
                    folder = %row.id.short(), path = %me.path.display(), %error,
                    "folder not there; its journal waits for it"
                );
                for open in &journal {
                    disk.keep_displaced(
                        open.path.clone(),
                        open.displaced_to.clone(),
                        reach::disk_path(&open.path, &names),
                        open.temp_file.clone(),
                    );
                }
            }
        }
        if restored.is_none() {
            joins.push(Event::FolderJoined {
                folder: row.id,
                rules: row.rules.clone(),
                members: members.iter().map(|m| m.node).collect(),
            });
        }
        folders.push(Opened {
            id: row.id,
            folder,
            records: BTreeMap::new(),
            names,
            disk,
            stability: Arc::new(Stability::new(Arc::clone(&setup.realtime))),
        });
    }
    let engine = Engine::restore(node.clone(), parts, clock.now());
    for opened in &mut folders {
        if let Some(state) = engine.folder(opened.id) {
            opened.records = state
                .index()
                .live_records()
                .map(|record| (record.entry.path.clone(), record.entry.clone()))
                .collect();
        }
    }
    let writer = store.writer()?;
    if !writes.is_empty() {
        let mut group = Group::new(clock.now());
        for write in writes {
            group.host(write);
        }
        writer.submit(group).wait()?;
    }
    Ok(Started {
        engine,
        writer,
        folders,
        joins,
    })
}

/// The folder's root, opened, if its marker is there (§7.3, the root
/// guard): a folder whose disk is not mounted must not have its journal
/// undone or its `tmp/` cleared into whatever is at its path instead.
fn open_marked(folder: &dyn Folder) -> io::Result<Box<dyn Fs>> {
    let fs = folder.open()?;
    let marker = Path::new(RESERVED_DIR).join(MARKER);
    match fs.lstat(&marker)?.kind {
        FileKind::File => Ok(fs),
        kind => Err(io::Error::other(format!(
            "{} is a {kind:?}, not a file",
            marker.display()
        ))),
    }
}
