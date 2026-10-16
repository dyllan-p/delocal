//! What the host does for a folder at daemon start, before the first scan
//! (DESIGN.md §7.5 "Two renames, one commit" and "Crash safety", §8.4), in
//! this order:
//!
//! 1. Undo every open row of the commit journal, into an empty path only
//!    ([`undo`]). The temp file a row names is left for the fetch to resume.
//!    A row whose temp file is gone belongs to a commit that landed, and is
//!    synced and dropped instead (draft 65). A row the undo could not
//!    resolve, or a landed one whose directories could not be synced, stays
//!    open in the folder's [`DiskState`], so the scan reports its path
//!    `Skipped` until a later resolve or commit there settles it (draft 64).
//! 2. Bring the trash's rows into line with the trash ([`trash::reconcile`]):
//!    an entry a crash left without its row gets one, and a row whose entry
//!    is gone goes. This runs after the undo, which moves entries out of the
//!    trash and drops their rows.
//! 3. Prune the trash ([`trash::prune`]), with the rows as reconciled.
//! 4. Remove every temp file no want claims ([`tmp::remove_unclaimed`]),
//!    after the undo, which may name a temp file a want still claims.
//!
//! The host makes the writes durable before the first scan, which
//! observes whatever is at the paths now.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;

use delocal_engine::{FolderId, Timestamp};

use super::DiskState;
use super::journal::{Undone, undo};
use crate::fs::Fs;
use crate::reach::DiskNames;
use crate::store::{HostWrite, JournalRow, TrashRow};
use crate::tmp;
use crate::trash::{self, Policy};

/// What a folder's start found in the store.
pub struct Start<'a> {
    pub folder: FolderId,
    /// The open rows of its commit journal.
    pub journal: &'a [JournalRow],
    /// Its trash's rows.
    pub trash: &'a [TrashRow],
    /// Its names on disk that are not their index paths'.
    pub names: &'a dyn DiskNames,
    /// What the host keeps about its disk: rows the undo leaves open go
    /// here.
    pub disk: &'a DiskState,
    /// Whether a want claims a temp file, by its name (see
    /// [`tmp::claimed`]).
    pub claimed: &'a dyn Fn(&OsStr) -> bool,
    pub now: Timestamp,
    pub policy: Policy,
}

/// What a folder's start did.
#[derive(Debug, Default)]
pub struct Started {
    /// The host's writes, in order, to make durable before the first scan.
    pub writes: Vec<HostWrite>,
    pub undone: Undone,
    /// Trash entries pruning could not remove; they keep their rows.
    pub unpruned: Vec<(PathBuf, io::Error)>,
    /// The temp files removed.
    pub temps: Vec<OsString>,
    /// Why `tmp/` could not be cleared, if it could not.
    pub tmp_error: Option<io::Error>,
}

impl Start<'_> {
    /// Run the four steps on `fs`, one operation on the folder (§7.3).
    pub fn run(&self, fs: &dyn Fs) -> Started {
        let mut started = Started {
            undone: undo(fs, self.folder, self.journal, self.names),
            ..Started::default()
        };
        started.writes.extend(started.undone.writes.iter().cloned());
        for (path, _) in &started.undone.failed {
            if let Some(row) = self.journal.iter().find(|row| row.path == *path) {
                let back = crate::reach::disk_path(path, self.names);
                let (at, temp) = (row.displaced_to.clone(), row.temp_file.clone());
                self.disk.keep_displaced(path.clone(), at, back, temp);
            }
        }
        for (path, dirs, _) in &started.undone.unsynced {
            self.disk.keep_unsynced(path.clone(), dirs.clone());
        }
        // The rows as the undo left them, then as reconciled.
        let mut rows: Vec<TrashRow> = self.trash.to_vec();
        apply(&mut rows, &started.undone.writes);
        let reconciled = trash::reconcile(fs, self.folder, &rows, self.now);
        apply(&mut rows, &reconciled);
        started.writes.extend(reconciled);
        let pruned = trash::prune(fs, self.folder, &rows, self.now, self.policy);
        started.writes.extend(pruned.writes);
        started.unpruned = pruned.failed;
        match tmp::remove_unclaimed(fs, self.claimed) {
            Ok(removed) => started.temps = removed,
            Err(e) => started.tmp_error = Some(e),
        }
        started
    }
}

/// `writes`' changes to the trash's rows, applied to `rows`.
fn apply(rows: &mut Vec<TrashRow>, writes: &[HostWrite]) {
    for write in writes {
        match write {
            HostWrite::PutTrash { row, .. } => {
                rows.retain(|r| r.trashed_path != row.trashed_path);
                rows.push(row.clone());
            }
            HostWrite::RemoveTrash { trashed_path, .. } => {
                rows.retain(|r| r.trashed_path != *trashed_path);
            }
            _ => {}
        }
    }
}
