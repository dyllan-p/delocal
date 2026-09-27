//! The filesystem layer (DESIGN.md §14.2): every filesystem operation the
//! daemon performs on a folder goes through [`Fs`], a trait with a real
//! implementation, [`RealFs`], and a fault-injecting wrapper for tests. The
//! wrapper is what makes disk-full, I/O-error and failing-rename tests
//! deterministic and unprivileged.
//!
//! The operations are the ones §7.3, §7.5, §8.4 and §13 need, and nothing
//! else:
//!
//! | Operation | Serves |
//! |---|---|
//! | [`read_dir`](Fs::read_dir) | §7.3 the scan walk and names on disk; §7.5 step 6 the guard's exact-name check through the parent; §7.5 removing unclaimed `tmp/` files at start; §8.4 listing and pruning the trash |
//! | [`lstat`](Fs::lstat) | §7.3 the fast path, the stability re-stat after hashing, the root guard's marker, the shim's read-back; §7.5 step 2 a source's size and mtime check and the resume offset; §7.5 step 6 the guard; §8.4 the trash's size |
//! | [`read_link`](Fs::read_link) | §7.3 a symlink's fast path and hash; §7.5 step 6 the guard for a symlink; §7.5 step 2 serving a symlink |
//! | [`open_read`](Fs::open_read) | §7.3 hashing, `.delocalignore`, `folder.json`; §7.5 step 2 serving from an offset and filling a fetch from this machine's folder or trash; §8.4 restore |
//! | [`create_new`](Fs::create_new) | §7.5 step 3 a new temp file in `.delocal/tmp/`; §8.4 restore's copy |
//! | [`open_append`](Fs::open_append) | §7.5 steps 2 and 3 resuming a temp file; §13 crash during a transfer |
//! | [`WriteFile::sync`] | §7.5 the data is durable before the rename (step 8) |
//! | [`sync_dir`](Fs::sync_dir) | §7.5 step 9 the parent directory is durable before the report, and after the displacement of step 7 |
//! | [`rename`](Fs::rename) | §7.5 steps 7 and 8, the journal's undo, symlinks via a temp, deletes to the trash; §7.6 displacement to the conflict path; §8.4 |
//! | [`remove_file`](Fs::remove_file) | §7.5 step 4 discarding a temp file whose hash did not match; §7.5 unclaimed `tmp/` files at start; §8.4 pruning |
//! | [`remove_dir`](Fs::remove_dir) | §7.5 deletes, a directory only when empty; §8.4 pruning a day's empty directory |
//! | [`create_dir`](Fs::create_dir) | §7.5 step 8 missing parents and directory entries; §8.4 a day's directory; §11 `.delocal/tmp/` and `.delocal/trash/` |
//! | [`symlink`](Fs::symlink) | §7.5 symlinks: `symlink(target, tmp)` then rename |
//! | [`set_mtime`](Fs::set_mtime) | §7.5 step 5 the temp file's mtime; §7.5 metadata-only applies; §8.3 revert's `SetMeta` |
//! | [`set_mode`](Fs::set_mode) | §7.5 step 5 the temp file's exec bit; §7.5 exec-only applies; §8.3 revert's `SetMeta` |
//! | [`available_space`](Fs::available_space) | §7.5 local failures: `SpaceRecovered` after `DiskFull`; §13 disk full |
//!
//! **Paths** are relative to the folder root the `Fs` was opened on
//! ([`RealFs::open`]): the names on disk, joined with `/`. The empty path is
//! the root itself. An absolute path or a `..` would leave the folder, so it
//! is refused with `InvalidInput`. Nothing here maps paths to index paths
//! ([`crate::names`] does that from the names `read_dir` returns, §7.3).
//! Errors are the operating system's own `io::Error`s, so a caller maps a
//! real `ENOSPC` and an injected one the same way.
//!
//! **Reaching a path** (§7.3). A path is reached one component at a time
//! from the open root, never handed to the kernel whole, so `PATH_MAX`
//! never applies: every index path within §7.1's limits works on every
//! platform. Each parent is opened as a directory without following a
//! symlink, and a parent that is a symlink, a file or anything else stops
//! the operation with [`ParentNotADirectory`], so nothing outside the folder
//! is ever read or written. The calls that describe or change an entry
//! never follow a symlink at the last component either, and `read_dir`,
//! `sync_dir` and `available_space` refuse one there with `NotADirectory`.
//!
//! **Opening a file.** `open_read`, `open_append` and `set_mode` open the
//! last component without following a symlink and without waiting on a
//! FIFO, then check what the descriptor is. Anything but a file (or, for
//! `set_mode`, a directory) is refused with [`NotAFile`], which says what
//! was there: a file swapped for a symlink or a FIFO between a caller's
//! `lstat` and the open is observed as what it has become.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod conformance;
#[cfg(feature = "faults")]
pub mod faulty;
pub mod real;

#[cfg(feature = "faults")]
pub use faulty::FaultyFs;
pub use real::RealFs;

/// Every filesystem operation on one folder, with paths relative to its root.
/// See the module docs for which section each serves.
///
/// Object-safe and `Send + Sync`, so the daemon can hold an `Arc<dyn Fs>`
/// per folder and choose the implementation at spawn.
pub trait Fs: Send + Sync {
    /// The names in `dir`, as raw bytes, without `.` and `..`, sorted by
    /// bytes. Sorting makes a scan's order the same on every run, whatever
    /// order the filesystem keeps; the names themselves are exactly what
    /// `readdir` returned, never normalised (§7.3). A symlink at `dir` is
    /// not followed: it is `NotADirectory`.
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<OsString>>;

    /// What is at `path`, without following a symlink there.
    fn lstat(&self, path: &Path) -> io::Result<Stat>;

    /// The target of the symlink at `path`, as raw bytes.
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;

    /// Open the file at `path` for reading, positioned at its start.
    /// Anything else there is refused with [`NotAFile`], neither followed nor
    /// waited on.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadFile>>;

    /// Create a file at `path` for writing. Fails if anything is already
    /// there, so a temp file is never truncated or shared by accident.
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn WriteFile>>;

    /// Open the existing file at `path` for writing at its end. Fails if
    /// there is none: a resumed transfer whose temp file has gone starts
    /// over, it does not create one. Anything else there is refused with
    /// [`NotAFile`], neither followed nor waited on.
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn WriteFile>>;

    /// Make the entries of the directory at `path` durable: creations,
    /// removals and renames in it survive a power loss once this returns. A
    /// symlink at `path` is not followed: it is `NotADirectory`.
    fn sync_dir(&self, path: &Path) -> io::Result<()>;

    /// Rename `from` to `to` atomically, replacing a file at `to`. Both must
    /// be on one filesystem.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Remove the file or symlink at `path` (never a symlink's target).
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Remove the directory at `path`, which must be empty.
    fn remove_dir(&self, path: &Path) -> io::Result<()>;

    /// Create one directory at `path`. Its parent must exist, and nothing
    /// may be at `path` already.
    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Create a symlink at `link` pointing to `target`, which need not exist.
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;

    /// Set the modification time of what is at `path` to `mtime_ns`,
    /// nanoseconds since the Unix epoch, without following a symlink. The
    /// access time is left as it was.
    fn set_mtime(&self, path: &Path, mtime_ns: i64) -> io::Result<()>;

    /// Set the permission bits of the file or directory at `path` to `mode`
    /// (the low twelve bits are used). Anything else there, a symlink
    /// included, is refused with [`NotAFile`] rather than followed.
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()>;

    /// Bytes an unprivileged process can still write on the filesystem that
    /// holds the directory at `path`.
    fn available_space(&self, path: &Path) -> io::Result<u64>;
}

/// A file open for reading. `Seek` serves a request from an offset (§7.5
/// step 2).
pub trait ReadFile: Read + Seek + Send {}

/// A file open for writing.
pub trait WriteFile: Write + Send {
    /// Make everything written so far durable, data and metadata (`fsync`).
    fn sync(&mut self) -> io::Result<()>;
}

/// What [`Fs::lstat`] reports for one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: FileKind,
    /// Bytes: a file's length, a symlink's target length.
    pub size: u64,
    /// Nanoseconds since the Unix epoch. Saturates at the ends of `i64`,
    /// about the years 1677 and 2262.
    pub mtime_ns: i64,
    /// The permission bits (`st_mode & 0o7777`).
    pub mode: u32,
    /// The device that holds the entry. A directory on another device than
    /// the folder root is a mount point, and a rename out of it into the
    /// trash would cross filesystems (§8.4).
    pub dev: u64,
    /// The inode. A file replaced by a rename during hashing can keep its
    /// size and mtime but not its inode, which the stability re-stat checks
    /// (§7.3).
    pub ino: u64,
}

/// The kind of what is at a path. `Other` is a FIFO, a socket or a device:
/// §3.1 syncs files, directories and symlinks only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// A parent on the way to a path is not a directory: a symlink, a file or
/// anything else. The operation stopped there and followed nothing (§7.3); a
/// commit reports `ChangedUnderneath` for it (§7.5 step 6).
///
/// It travels inside an `io::Error` of kind `NotADirectory`;
/// [`ParentNotADirectory::of`] finds it there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentNotADirectory {
    /// The parent that is not a directory, relative to the folder root.
    pub parent: PathBuf,
}

impl ParentNotADirectory {
    /// The `ParentNotADirectory` inside `error`, if that is what it is.
    pub fn of(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }
}

impl fmt::Display for ParentNotADirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not a directory", self.parent.display())
    }
}

impl std::error::Error for ParentNotADirectory {}

impl From<ParentNotADirectory> for io::Error {
    fn from(error: ParentNotADirectory) -> Self {
        io::Error::new(io::ErrorKind::NotADirectory, error)
    }
}

/// What an open found at a path instead of a file: a symlink it did not
/// follow, a FIFO it did not wait on, a directory, a socket or a device
/// (§7.3). `set_mode` also accepts a directory, and refuses the rest.
///
/// It travels inside an `io::Error` of kind `InvalidInput`;
/// [`NotAFile::of`] finds it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotAFile {
    /// What was there.
    pub kind: FileKind,
}

impl NotAFile {
    /// The `NotAFile` inside `error`, if that is what it is.
    pub fn of(error: &io::Error) -> Option<Self> {
        error.get_ref()?.downcast_ref().copied()
    }
}

impl fmt::Display for NotAFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            FileKind::File => "a file",
            FileKind::Dir => "a directory",
            FileKind::Symlink => "a symlink",
            FileKind::Other => "a FIFO, socket or device",
        };
        write!(f, "not a file but {what}")
    }
}

impl std::error::Error for NotAFile {}

impl From<NotAFile> for io::Error {
    fn from(error: NotAFile) -> Self {
        io::Error::new(io::ErrorKind::InvalidInput, error)
    }
}
