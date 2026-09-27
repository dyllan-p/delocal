//! [`RealFs`]: the filesystem layer on the real filesystem (DESIGN.md §14.2),
//! the implementation the daemon ships with.
//!
//! Almost every operation is one call into `std`. Two are not: `set_mtime`
//! uses `filetime`, because `std` can only set times through an open file,
//! and opening a FIFO the user swapped in would block the daemon; and
//! `available_space` uses `rustix`'s `statvfs`, which `std` does not expose
//! at all (Appendix A).

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use filetime::FileTime;

use super::{FileKind, Fs, ReadFile, Stat, WriteFile};

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// The real filesystem. Holds nothing: every path is absolute or relative to
/// the process's working directory, as `std::fs` takes it.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealFs;

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
        let mut names = fs::read_dir(dir)?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        Ok(names)
    }

    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        let meta = fs::symlink_metadata(path)?;
        let file_type = meta.file_type();
        let kind = if file_type.is_file() {
            FileKind::File
        } else if file_type.is_dir() {
            FileKind::Dir
        } else if file_type.is_symlink() {
            FileKind::Symlink
        } else {
            FileKind::Other
        };
        Ok(Stat {
            kind,
            size: meta.len(),
            mtime_ns: mtime_ns(meta.mtime(), meta.mtime_nsec()),
            mode: meta.mode() & 0o7777,
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        fs::read_link(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadFile>> {
        Ok(Box::new(File::open(path)?))
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Box::new(file))
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        Ok(Box::new(OpenOptions::new().append(true).open(path)?))
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        // Linux and macOS both accept fsync on a directory opened read-only.
        File::open(path)?.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    fn set_mtime(&self, path: &Path, mtime_ns: i64) -> io::Result<()> {
        // `filetime` can leave the access time alone only on a call that
        // follows symlinks, so read it and write it back unchanged. Both
        // calls are on the entry itself, never a symlink's target.
        let meta = fs::symlink_metadata(path)?;
        let atime = FileTime::from_last_access_time(&meta);
        let secs = mtime_ns.div_euclid(NANOS_PER_SEC);
        // `rem_euclid` of a positive divisor is in 0..1e9, so it fits.
        let nanos = u32::try_from(mtime_ns.rem_euclid(NANOS_PER_SEC)).unwrap_or(0);
        filetime::set_symlink_file_times(path, atime, FileTime::from_unix_time(secs, nanos))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        // chmod(2) follows a symlink and Linux has no lchmod, so refuse one
        // here. A symlink swapped in between this check and the chmod is
        // followed; the caller has just guarded the path (§7.5 step 6), so
        // that window is the same one every commit has.
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "set_mode on a symlink",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
    }

    fn available_space(&self, path: &Path) -> io::Result<u64> {
        let vfs = rustix::fs::statvfs(path)?;
        // POSIX counts `f_bavail` in units of `f_frsize`, not `f_bsize`.
        Ok(vfs.f_bavail.saturating_mul(vfs.f_frsize))
    }
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

    crate::fs::conformance::conformance_tests!(RealFs);

    #[test]
    fn mtime_ns_handles_the_epoch_and_saturates() {
        assert_eq!(mtime_ns(0, 0), 0);
        assert_eq!(mtime_ns(1, 5), 1_000_000_005);
        // 1969-12-31T23:59:59.25 is one second before the epoch plus 0.25.
        assert_eq!(mtime_ns(-1, 250_000_000), -750_000_000);
        assert_eq!(mtime_ns(i64::MAX / 2, 0), i64::MAX);
        assert_eq!(mtime_ns(i64::MIN / 2, 0), i64::MIN);
    }
}
