//! The commit journal (DESIGN.md §7.5, "Two renames, one commit"; drafts
//! 34 and 36): a row made durable before a commit displaces what is at its
//! path, and removed once its rename is durable.
//!
//! Between the displacement and the rename the path is absent. While the
//! daemon runs the in-flight rule covers that window; a crash inside it is
//! covered by the row. At the next start, before the first scan, [`undo`]
//! undoes every open row **into an empty path only**: if the path is empty,
//! what was displaced is moved back; if it is occupied (the rename
//! completed before the row went, or the user put something there while the
//! daemon was down), nothing moves, and the displaced file stays where the
//! commit put it, in the trash or at the conflict-copy path, for the first
//! scan to observe. An undo never replaces anything. The temp file is left
//! for the fetch to resume (§7.5); the engine's commit did not survive the
//! restart, so it fetches and commits again.
//!
//! It undoes only a commit whose rename never happened (draft 65), told by
//! whether the row's temp file is still there, since a rename consumes it.
//! A row whose temp file is gone belongs to a commit that landed, its
//! report or its sync lost: its directories are synced and the row dropped,
//! and the displaced file stays where the commit put it, rather than come
//! back over a newer version or over the user's deletion of it.
//!
//! A displacement to the trash makes its trash row durable with the journal
//! row ([`crate::trash`]), so a file that stays in the trash after an undo
//! keeps its row, and one that comes back loses it.

use std::io;
use std::path::{Path, PathBuf};

use delocal_engine::{FolderId, RelPath};

use crate::fs::{Fs, rename_refusing};
use crate::reach::{self, DiskNames, Walk};
use crate::scan::Clock;
use crate::store::{Group, HostWrite, JournalRow, Writer};
use crate::trash;

/// Where a commit makes its journal row durable before it displaces
/// anything (§7.5).
pub trait Journal {
    /// Make `writes` durable before returning: the journal row, and the
    /// trash row that goes with it. An error stops the commit before
    /// anything is moved.
    fn durable(&self, writes: Vec<HostWrite>) -> io::Result<()>;
}

/// The journal in the store (§11): each call is a group of its own,
/// submitted to the group-commit writer and waited for, so the row is on
/// disk before the displacement, as the journal requires. A displacement
/// therefore costs one synchronous transaction.
pub struct StoreJournal<'a> {
    pub writer: &'a Writer,
    pub clock: &'a dyn Clock,
}

impl Journal for StoreJournal<'_> {
    fn durable(&self, writes: Vec<HostWrite>) -> io::Result<()> {
        let mut group = Group::new(self.clock.now());
        for write in writes {
            group.host(write);
        }
        self.writer
            .submit(group)
            .wait()
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

/// What [`undo`] did with the open rows.
#[derive(Debug, Default)]
pub struct Undone {
    /// The writes to make durable before the first scan: each undone row's
    /// removal, and the trash row of a displaced file that came back.
    pub writes: Vec<HostWrite>,
    /// Paths whose displaced file was moved back.
    pub back: Vec<RelPath>,
    /// Paths that were occupied, or could not be reached, so their
    /// displaced file stayed where the commit put it.
    pub left: Vec<RelPath>,
    /// Paths whose commit had landed (its temp file was gone, draft 65):
    /// made durable and dropped, the displaced file left where it went.
    pub landed: Vec<RelPath>,
    /// Paths whose commit had landed but whose directories could not be
    /// synced, with them: the row stays, for the host to resolve later.
    pub unsynced: Vec<(RelPath, Vec<PathBuf>, io::Error)>,
    /// Rows that could not be undone for an error other than the path
    /// being occupied; they stay, and the next start tries again.
    pub failed: Vec<(RelPath, io::Error)>,
}

/// The directories a landed commit's renames changed: the path's parent
/// and where its displaced file went.
pub(crate) fn landed_dirs(disk: &Path, displaced_to: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![disk.parent().unwrap_or(Path::new("")).to_path_buf()];
    let to = displaced_to.parent().unwrap_or(Path::new("")).to_path_buf();
    if !dirs.contains(&to) {
        dirs.push(to);
    }
    dirs
}

/// Make a landed commit's directories durable. One that is gone, or is no
/// longer a directory, has nothing left to make durable.
pub(crate) fn sync_landed(fs: &dyn Fs, dirs: &[PathBuf]) -> io::Result<()> {
    for dir in dirs {
        match fs.sync_dir(dir) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Undo every open row of `folder`'s commit journal, `rows`, at daemon
/// start, before the first scan (see the module docs). `names` are the
/// folder's names on disk, which address the path as the commit did. A row
/// whose path could not be looked at, or lies beneath a mount point, is
/// left open and listed as failed, for the next start.
pub fn undo(fs: &dyn Fs, folder: FolderId, rows: &[JournalRow], names: &dyn DiskNames) -> Undone {
    let mut undone = Undone::default();
    let device = match fs.lstat(Path::new("")) {
        Ok(stat) => stat.dev,
        Err(e) => {
            for row in rows {
                undone
                    .failed
                    .push((row.path.clone(), io::Error::new(e.kind(), e.to_string())));
            }
            return undone;
        }
    };
    for row in rows {
        let gone = HostWrite::RemoveJournal {
            folder,
            path: row.path.clone(),
        };
        // The trash row a displacement to the trash made with this one.
        let trash_row = row
            .displaced_to
            .strip_prefix(trash::DIR)
            .ok()
            .map(|in_trash| HostWrite::RemoveTrash {
                folder,
                trashed_path: in_trash.to_path_buf(),
            });
        match fs.lstat(&row.displaced_to) {
            Ok(_) => {}
            // Nothing was displaced, or it is already back.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                undone.writes.push(gone);
                undone.writes.extend(trash_row);
                continue;
            }
            Err(e) => {
                undone.failed.push((row.path.clone(), e));
                continue;
            }
        }
        let disk = reach::disk_path(&row.path, names);
        // Draft 65: a rename consumes the row's temp file, so a temp file
        // that is gone means the commit landed, its report or its sync lost.
        // Only a rename that never happened is undone: a landed one is made
        // durable and its row dropped, its displaced file left where it
        // went. A row with no temp file (a directory made in place) cannot
        // be told this way, and is undone into an empty path as before.
        if let Some(temp) = &row.temp_file {
            match fs.lstat(temp) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let dirs = landed_dirs(&disk, &row.displaced_to);
                    match sync_landed(fs, &dirs) {
                        Ok(()) => {
                            undone.writes.push(gone);
                            undone.landed.push(row.path.clone());
                        }
                        Err(e) => undone.unsynced.push((row.path.clone(), dirs, e)),
                    }
                    continue;
                }
                Err(e) => {
                    undone.failed.push((row.path.clone(), e));
                    continue;
                }
            }
        }
        let moved = match reach::walk(fs, &row.path, &disk, device) {
            Ok(Walk::Reached) => rename_refusing(fs, &row.displaced_to, &disk),
            // The parent went, or is not a directory: there is no empty
            // path to go back into.
            Ok(Walk::Missing { .. } | Walk::NotADirectory { .. }) => {
                Err(io::ErrorKind::NotFound.into())
            }
            // A disk is mounted over the parent: what is beneath it is not
            // the folder's until it goes, so the row waits for it.
            Ok(Walk::Crossed { at }) => Err(io::Error::other(format!("{at} is a mount point"))),
            // It could not be looked at: the row stays for the next start.
            Err(e) => Err(e),
        };
        match moved {
            Ok(_) => {
                let parent = disk.parent().unwrap_or(Path::new(""));
                let from = row.displaced_to.parent().unwrap_or(Path::new(""));
                // Best effort: the row goes only with the writes below,
                // which the host makes durable before the first scan.
                let _ = fs.sync_dir(parent);
                let _ = fs.sync_dir(from);
                undone.writes.push(gone);
                undone.writes.extend(trash_row);
                undone.back.push(row.path.clone());
            }
            // Occupied, or its parent is gone or not a directory: the
            // displaced file stays where the commit put it.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                ) =>
            {
                undone.writes.push(gone);
                undone.left.push(row.path.clone());
            }
            Err(e) => undone.failed.push((row.path.clone(), e)),
        }
    }
    undone
}
