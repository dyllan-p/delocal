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
//! Paths are host paths, the folder root joined with the names on disk.
//! Nothing here maps them to index paths (§7.3 does that from the names
//! `read_dir` returns). Errors are the operating system's own `io::Error`s,
//! so a caller maps a real `ENOSPC` and an injected one the same way.
//!
//! **Symlinks.** The calls that describe or change an entry (`lstat`,
//! `read_link`, `create_new`, `create_dir`, `symlink`, `rename`,
//! `remove_file`, `remove_dir`, `set_mtime`, `set_mode`) never follow a
//! symlink at the end of a path, since §7.3 syncs symlinks as symlinks. The
//! calls that open something (`read_dir`, `open_read`, `open_append`,
//! `sync_dir`) and `available_space` do follow one, as their system calls
//! do: `std` has no way to open without following. A caller opens what it
//! has just seen with `lstat`, so only a path swapped in between is at
//! risk, and §7.3's re-stat after hashing catches a swapped file; a FIFO
//! swapped in there would block the open until something writes to it.

use std::ffi::OsString;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

pub mod real;

#[cfg(test)]
mod conformance;

pub use real::RealFs;

/// Every filesystem operation on a folder. See the module docs for which
/// section each serves.
///
/// Object-safe and `Send + Sync`, so the daemon can hold an `Arc<dyn Fs>`
/// and choose the implementation at spawn.
pub trait Fs: Send + Sync {
    /// The names in `dir`, as raw bytes, without `.` and `..`, sorted by
    /// bytes. Sorting makes a scan's order the same on every run, whatever
    /// order the filesystem keeps; the names themselves are exactly what
    /// `readdir` returned, never normalised (§7.3). Follows a symlink at
    /// `dir` (see the module docs).
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<OsString>>;

    /// What is at `path`, without following a symlink there.
    fn lstat(&self, path: &Path) -> io::Result<Stat>;

    /// The target of the symlink at `path`, as raw bytes.
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;

    /// Open the file at `path` for reading, positioned at its start.
    /// Follows a symlink at `path` (see the module docs).
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadFile>>;

    /// Create a file at `path` for writing. Fails if anything is already
    /// there, so a temp file is never truncated or shared by accident.
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn WriteFile>>;

    /// Open the existing file at `path` for writing at its end. Fails if
    /// there is none: a resumed transfer whose temp file has gone starts
    /// over, it does not create one. Follows a symlink at `path` (see the
    /// module docs).
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn WriteFile>>;

    /// Make the entries of the directory at `path` durable: creations,
    /// removals and renames in it survive a power loss once this returns.
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
    /// (the low twelve bits are used). A symlink at `path` is refused with
    /// `InvalidInput` rather than followed.
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()>;

    /// Bytes an unprivileged process can still write on the filesystem that
    /// holds `path`.
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
