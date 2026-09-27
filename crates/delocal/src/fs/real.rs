//! [`RealFolder`] and [`RealFs`]: the filesystem layer on the real filesystem
//! (DESIGN.md §14.2), reaching every path from the folder root (§7.3).
//!
//! A `RealFolder` is a configured path and nothing else. Its
//! [`open`](Folder::open) opens the root for one operation and returns a
//! `RealFs`, which holds the root until it is dropped. Every call on it walks
//! its path one component at a time from there, opening each parent with
//! `O_DIRECTORY | O_NOFOLLOW`, and then acts on the last component with a
//! directory-relative call (`openat`, `fstatat`, `renameat`, `unlinkat`,
//! `mkdirat`, `symlinkat`, `readlinkat`, `utimensat`). The kernel
//! never sees a whole path, so `PATH_MAX` (4,096 bytes on Linux, 1,024 on
//! macOS) never applies, and a parent that is not a directory stops the walk
//! with [`ParentNotADirectory`] before anything outside the folder is read
//! or written.
//!
//! Files are opened with `O_NOFOLLOW | O_NONBLOCK` and checked with `fstat`
//! on the descriptor, so a symlink is never followed, a FIFO never waited
//! on, and either is refused with [`NotAFile`]. `set_mode` opens the same
//! way and uses `fchmod` on the descriptor, never `fchmodat`, which on Linux
//! cannot refuse a symlink (§7.3); a file its owner made unreadable
//! therefore cannot have its mode set.
//!
//! The system calls come from `rustix` (Appendix A), which wraps them
//! safely; `std` has no directory-relative calls at all. A walk costs one
//! `openat` per parent, a few microseconds each.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, Dir, Mode, OFlags, Timespec, Timestamps};
use rustix::io::Errno;

use super::{FileKind, Folder, Fs, NotAFile, ParentNotADirectory, ReadFile, Stat, WriteFile};

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// The flags every walk opens a directory with: a directory, and never
/// through a symlink.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A folder on the real filesystem, by its configured path. It holds
/// nothing open (§7.3); each operation calls [`open`](Folder::open).
#[derive(Clone, Debug)]
pub struct RealFolder {
    root: PathBuf,
}

impl RealFolder {
    /// The folder whose root is at `root`. Nothing is opened, or even
    /// checked, until an operation calls [`open`](Folder::open).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The configured path.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Folder for RealFolder {
    fn open(&self) -> io::Result<Box<dyn Fs>> {
        Ok(Box::new(RealFs::open(&self.root)?))
    }
}

/// One operation on a [`RealFolder`]: the folder root, held open until this
/// is dropped. Paths are relative to that root (see [`Fs`]).
#[derive(Debug)]
pub struct RealFs {
    root: OwnedFd,
}

/// Where an operation acts: the directory its entry lives in (`None` for the
/// root) and the entry's name there (`None` when the path is the root).
type Walked<'p> = (Option<OwnedFd>, Option<&'p OsStr>);

impl RealFs {
    /// Open the folder at `root`, which must be a directory. `root` itself
    /// is the path the user gave, so symlinks in it are followed; nothing
    /// below it ever is. Private: [`RealFolder::open`] is the way in, so a
    /// root is only ever held for one operation.
    fn open(root: &Path) -> io::Result<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        Ok(Self {
            root: rustix::fs::open(root, flags, Mode::empty())?,
        })
    }

    /// Open the parents of `path` one at a time from the root, and return the
    /// last one with the name of the entry in it.
    fn walk<'p>(&self, path: &'p Path) -> io::Result<Walked<'p>> {
        let names = components(path)?;
        let Some((last, parents)) = names.split_last() else {
            return Ok((None, None));
        };
        let mut dir: Option<OwnedFd> = None;
        for (depth, name) in parents.iter().enumerate() {
            match rustix::fs::openat(self.at(&dir), *name, DIR_FLAGS, Mode::empty()) {
                Ok(next) => dir = Some(next),
                // One component at a time, so either error is about this one:
                // a file or something else (ENOTDIR), or a symlink that
                // O_NOFOLLOW refused (ELOOP).
                Err(Errno::NOTDIR | Errno::LOOP) => {
                    let parent: PathBuf = parents[..=depth].iter().collect();
                    return Err(ParentNotADirectory { parent }.into());
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok((dir, Some(*last)))
    }

    /// The directory an entry lives in and its name there. Refuses the root,
    /// which has no name to act on.
    fn entry<'p>(&self, path: &'p Path) -> io::Result<(Option<OwnedFd>, &'p OsStr)> {
        match self.walk(path)? {
            (dir, Some(name)) => Ok((dir, name)),
            (_, None) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the folder root has no name to act on",
            )),
        }
    }

    /// `dir`, or the root when there is none.
    fn at<'a>(&'a self, dir: &'a Option<OwnedFd>) -> BorrowedFd<'a> {
        dir.as_ref().map_or(self.root.as_fd(), AsFd::as_fd)
    }

    /// Open the last component of `path` with `flags`, never following a
    /// symlink or waiting on a FIFO, and return the descriptor with what it
    /// is. What the open refuses outright is [`NotAFile`] when it is not a
    /// file.
    fn open_entry(&self, path: &Path, flags: OFlags) -> io::Result<(OwnedFd, FileKind)> {
        let (dir, name) = self.entry(path)?;
        let at = self.at(&dir);
        let flags = flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        match rustix::fs::openat(at, name, flags, Mode::empty()) {
            Ok(fd) => {
                let kind = kind(&rustix::fs::fstat(&fd)?);
                Ok((fd, kind))
            }
            // Refused without following or waiting: a symlink (ELOOP under
            // O_NOFOLLOW), a directory opened for writing (EISDIR), a FIFO
            // opened for writing with no reader, or a socket (ENXIO, or
            // EOPNOTSUPP for a socket on macOS). Say what is there instead.
            Err(e @ (Errno::LOOP | Errno::ISDIR | Errno::NXIO | Errno::OPNOTSUPP)) => {
                match kind(&rustix::fs::statat(at, name, AtFlags::SYMLINK_NOFOLLOW)?) {
                    FileKind::File => Err(e.into()),
                    kind => Err(NotAFile { kind }.into()),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Open the file at `path` with `flags`: anything else is [`NotAFile`].
    /// The descriptor keeps `O_NONBLOCK`, which changes nothing about reading
    /// or writing a file. At the open it does one thing more on Linux: a file
    /// under a conflicting lease (a Samba or NFS server's) fails with
    /// `EWOULDBLOCK` instead of waiting for the lease to break, an I/O error
    /// the caller retries.
    fn open_file(&self, path: &Path, flags: OFlags) -> io::Result<File> {
        match self.open_entry(path, flags)? {
            (fd, FileKind::File) => Ok(File::from(fd)),
            (_, kind) => Err(NotAFile { kind }.into()),
        }
    }

    /// Open the directory at `path` without following a symlink there:
    /// anything but a directory is `NotADirectory`.
    fn open_dir(&self, path: &Path) -> io::Result<OwnedFd> {
        let (dir, name) = self.walk(path)?;
        let name = name.unwrap_or(OsStr::new("."));
        match rustix::fs::openat(self.at(&dir), name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => Ok(fd),
            Err(Errno::NOTDIR | Errno::LOOP) => Err(Errno::NOTDIR.into()),
            Err(e) => Err(e.into()),
        }
    }
}

/// The components of `path`: names only, with `.` dropped, empty for the
/// root. An absolute path or a `..` would leave the folder, so it is refused.
fn components(path: &Path) -> io::Result<Vec<&OsStr>> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(Ok(name)),
            Component::CurDir => None,
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                Some(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a path inside the folder", path.display()),
                )))
            }
        })
        .collect()
}

impl ReadFile for File {}

impl WriteFile for File {
    fn sync(&mut self) -> io::Result<()> {
        // On macOS `std` implements this with `F_FULLFSYNC`, which also
        // flushes the drive's cache; a plain `fsync` there does not.
        self.sync_all()
    }
}

impl Fs for RealFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in Dir::new(self.open_dir(dir)?)? {
            let name = entry?.file_name().to_bytes().to_vec();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name));
            }
        }
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        Ok(names)
    }

    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        let st = match self.walk(path)? {
            (dir, Some(name)) => {
                rustix::fs::statat(self.at(&dir), name, AtFlags::SYMLINK_NOFOLLOW)?
            }
            (_, None) => rustix::fs::fstat(&self.root)?,
        };
        Ok(stat(&st))
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        let (dir, name) = self.entry(path)?;
        let target = rustix::fs::readlinkat(self.at(&dir), name, Vec::new())?;
        Ok(PathBuf::from(OsString::from_vec(target.into_bytes())))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadFile>> {
        Ok(Box::new(self.open_file(path, OFlags::RDONLY)?))
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        let (dir, name) = self.entry(path)?;
        // O_EXCL fails on anything already there, a symlink included.
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        let mode = Mode::from_raw_mode(0o666);
        let fd = rustix::fs::openat(self.at(&dir), name, flags, mode)?;
        Ok(Box::new(File::from(fd)))
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        Ok(Box::new(
            self.open_file(path, OFlags::WRONLY | OFlags::APPEND)?,
        ))
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        // Through `File` for its `F_FULLFSYNC` on macOS, as `WriteFile::sync`.
        File::from(self.open_dir(path)?).sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from_dir, from_name) = self.entry(from)?;
        let (to_dir, to_name) = self.entry(to)?;
        let (from_at, to_at) = (self.at(&from_dir), self.at(&to_dir));
        Ok(rustix::fs::renameat(from_at, from_name, to_at, to_name)?)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let (dir, name) = self.entry(path)?;
        Ok(rustix::fs::unlinkat(self.at(&dir), name, AtFlags::empty())?)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        let (dir, name) = self.entry(path)?;
        Ok(rustix::fs::unlinkat(
            self.at(&dir),
            name,
            AtFlags::REMOVEDIR,
        )?)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let (dir, name) = self.entry(path)?;
        let mode = Mode::from_raw_mode(0o777);
        Ok(rustix::fs::mkdirat(self.at(&dir), name, mode)?)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        let (dir, name) = self.entry(link)?;
        Ok(rustix::fs::symlinkat(target, self.at(&dir), name)?)
    }

    fn set_mtime(&self, path: &Path, mtime_ns: i64) -> io::Result<()> {
        let times = Timestamps {
            // UTIME_OMIT leaves the access time as it is.
            last_access: Timespec {
                tv_sec: 0,
                tv_nsec: rustix::fs::UTIME_OMIT,
            },
            last_modification: Timespec {
                tv_sec: mtime_ns.div_euclid(NANOS_PER_SEC),
                // `rem_euclid` of a positive divisor is in 0..1e9.
                tv_nsec: mtime_ns.rem_euclid(NANOS_PER_SEC),
            },
        };
        match self.walk(path)? {
            (dir, Some(name)) => {
                let flags = AtFlags::SYMLINK_NOFOLLOW;
                Ok(rustix::fs::utimensat(self.at(&dir), name, &times, flags)?)
            }
            (_, None) => Ok(rustix::fs::futimens(&self.root, &times)?),
        }
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        let mode = Mode::from_raw_mode(narrow(mode & 0o7777));
        if components(path)?.is_empty() {
            return Ok(rustix::fs::fchmod(&self.root, mode)?);
        }
        // On the descriptor, not by name (see the module docs). Opening for
        // reading needs read permission, which chmod by name would not.
        match self.open_entry(path, OFlags::RDONLY)? {
            (fd, FileKind::File | FileKind::Dir) => Ok(rustix::fs::fchmod(&fd, mode)?),
            (_, kind) => Err(NotAFile { kind }.into()),
        }
    }

    fn available_space(&self, path: &Path) -> io::Result<u64> {
        let vfs = rustix::fs::fstatvfs(self.open_dir(path)?)?;
        // POSIX counts `f_bavail` in units of `f_frsize`, not `f_bsize`.
        Ok(vfs.f_bavail.saturating_mul(vfs.f_frsize))
    }
}

/// Our [`Stat`] from the system's. The field types differ between Linux and
/// macOS (`st_mode` is 16 bits on macOS, `st_dev` signed), so each field goes
/// through `TryInto`, which is exact for every value either platform stores,
/// except a negative device number, which [`device`] sign-extends.
fn stat(st: &rustix::fs::Stat) -> Stat {
    Stat {
        kind: kind(st),
        size: to_u64(st.st_size),
        mtime_ns: mtime_ns(to_i64(st.st_mtime), to_i64(st.st_mtime_nsec)),
        mode: to_u32(st.st_mode) & 0o7777,
        dev: device(st.st_dev),
        ino: to_u64(st.st_ino),
    }
}

fn kind(st: &rustix::fs::Stat) -> FileKind {
    use rustix::fs::FileType;
    match FileType::from_raw_mode(st.st_mode) {
        FileType::RegularFile => FileKind::File,
        FileType::Directory => FileKind::Dir,
        FileType::Symlink => FileKind::Symlink,
        _ => FileKind::Other,
    }
}

// Generic, so one body serves both platforms' field types without a cast
// that clippy would call unnecessary on one of them.
fn to_u64<T: TryInto<u64>>(value: T) -> u64 {
    value.try_into().unwrap_or(0)
}

fn to_i64<T: TryInto<i64>>(value: T) -> i64 {
    value.try_into().unwrap_or(0)
}

fn to_u32<T: TryInto<u32>>(value: T) -> u32 {
    value.try_into().unwrap_or(0)
}

/// A device number as `std`'s `MetadataExt::dev` gives it. `dev_t` is signed
/// on macOS and `std` sign-extends it, so this does too: a device read here
/// compares equal to the same device read through `std`, and two negative
/// device numbers never both become 0.
fn device<T: Copy + TryInto<u64> + TryInto<i64>>(value: T) -> u64 {
    match TryInto::<u64>::try_into(value) {
        Ok(dev) => dev,
        Err(_) => TryInto::<i64>::try_into(value).map_or(0, i64::cast_unsigned),
    }
}

/// A `u32` as a narrower or equal type: the permission bits as `mode_t`,
/// which is 16 bits on macOS. Twelve bits fit either.
fn narrow<T: TryFrom<u32> + Default>(value: u32) -> T {
    T::try_from(value).unwrap_or_default()
}

/// Seconds and nanoseconds as `stat` reports them, in nanoseconds. Before
/// the epoch the seconds are negative and the nanoseconds still count up
/// from them, so the sum is right either way.
fn mtime_ns(secs: i64, nanos: i64) -> i64 {
    secs.saturating_mul(NANOS_PER_SEC).saturating_add(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::fs::conformance::conformance_tests!(|root: &Path| RealFolder::new(root));

    #[test]
    fn mtime_ns_handles_the_epoch_and_saturates() {
        assert_eq!(mtime_ns(0, 0), 0);
        assert_eq!(mtime_ns(1, 5), 1_000_000_005);
        // 1969-12-31T23:59:59.25 is one second before the epoch plus 0.25.
        assert_eq!(mtime_ns(-1, 250_000_000), -750_000_000);
        assert_eq!(mtime_ns(i64::MAX / 2, 0), i64::MAX);
        assert_eq!(mtime_ns(i64::MIN / 2, 0), i64::MIN);
    }

    #[test]
    fn a_negative_device_number_is_sign_extended_as_std_does() {
        assert_eq!(device(5_u64), 5);
        assert_eq!(device(5_i32), 5);
        assert_eq!(device(-2_i32), u64::MAX - 1);
        assert_eq!(device(-2_i32), (-2_i64).cast_unsigned());
    }

    #[test]
    fn the_root_must_be_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), "x").unwrap();
        let kind = |root: &Path| match RealFolder::new(root).open() {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.kind(),
        };
        assert_eq!(kind(&tmp.path().join("file")), io::ErrorKind::NotADirectory);
        assert_eq!(kind(&tmp.path().join("missing")), io::ErrorKind::NotFound);
    }
}
