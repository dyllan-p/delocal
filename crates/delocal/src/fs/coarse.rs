//! [`Coarse`], a folder for tests whose filesystem keeps mtimes to 2 s, as
//! FAT does (DESIGN.md §7.3, the mtime precision shim). Every mtime it is
//! asked to set is stored rounded down to a multiple of 2 s, and every mtime
//! it reports is rounded the same way, so a file written past it reads back
//! as FAT would show it too. Linux's vfat rounds down; the direction does
//! not matter to the shim, which only compares what it asked for with what
//! it reads back.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Dir, Folder, Fs, ReadFile, Stat, WriteFile};

/// The granularity, in nanoseconds.
pub const GRAIN: i64 = 2_000_000_000;

/// `mtime_ns` as a filesystem that keeps it to [`GRAIN`] stores it.
pub fn coarse(mtime_ns: i64) -> i64 {
    mtime_ns - mtime_ns.rem_euclid(GRAIN)
}

/// A folder whose filesystem keeps mtimes to 2 s. See the module docs.
pub struct Coarse {
    inner: Arc<dyn Folder>,
}

impl Coarse {
    pub fn new(inner: Arc<dyn Folder>) -> Self {
        Self { inner }
    }
}

impl Folder for Coarse {
    fn open(&self) -> io::Result<Box<dyn Fs>> {
        Ok(Box::new(CoarseFs {
            inner: self.inner.open()?,
        }))
    }
}

fn rounded(stat: io::Result<Stat>) -> io::Result<Stat> {
    stat.map(|stat| Stat {
        mtime_ns: coarse(stat.mtime_ns),
        ..stat
    })
}

struct CoarseFs {
    inner: Box<dyn Fs>,
}

impl Fs for CoarseFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        self.inner.read_dir(dir)
    }

    fn open_dir(&self, path: &Path) -> io::Result<Box<dyn Dir>> {
        Ok(Box::new(CoarseDir {
            inner: self.inner.open_dir(path)?,
        }))
    }

    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        rounded(self.inner.lstat(path))
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.read_link(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadFile>> {
        self.inner.open_read(path)
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        self.inner.create_new(path)
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        self.inner.open_append(path)
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_file(path)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename_noreplace(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir(path)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        self.inner.symlink(target, link)
    }

    fn set_mtime(&self, path: &Path, mtime_ns: i64) -> io::Result<()> {
        self.inner.set_mtime(path, coarse(mtime_ns))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.inner.set_mode(path, mode)
    }

    fn available_space(&self, path: &Path) -> io::Result<u64> {
        self.inner.available_space(path)
    }
}

struct CoarseDir {
    inner: Box<dyn Dir>,
}

impl Dir for CoarseDir {
    fn read_dir(&self) -> io::Result<Vec<OsString>> {
        self.inner.read_dir()
    }

    fn lstat(&self, name: &OsStr) -> io::Result<Stat> {
        rounded(self.inner.lstat(name))
    }

    fn read_link(&self, name: &OsStr) -> io::Result<PathBuf> {
        self.inner.read_link(name)
    }

    fn open_read(&self, name: &OsStr) -> io::Result<Box<dyn ReadFile>> {
        self.inner.open_read(name)
    }

    fn open_dir(&self, name: &OsStr) -> io::Result<Box<dyn Dir>> {
        Ok(Box::new(CoarseDir {
            inner: self.inner.open_dir(name)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::RealFolder;

    #[test]
    fn rounding_is_down_to_a_multiple_of_two_seconds() {
        assert_eq!(coarse(1_700_000_001_999_999_999), 1_700_000_000_000_000_000);
        assert_eq!(coarse(1_700_000_002_000_000_000), 1_700_000_002_000_000_000);
        assert_eq!(coarse(1), 0);
        assert_eq!(coarse(0), 0);
        assert_eq!(coarse(-1), -2_000_000_000);
        assert_eq!(coarse(-2_000_000_000), -2_000_000_000);
    }

    /// An mtime set through it is stored rounded, and every mtime it
    /// reports, through the `Fs` or a held directory, is rounded too, a
    /// file written past it included.
    #[test]
    fn it_stores_and_reports_mtimes_to_two_seconds() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("set"), "x").unwrap();
        std::fs::write(tmp.path().join("written"), "y").unwrap();
        let folder = Coarse::new(Arc::new(RealFolder::new(tmp.path())));
        let fs = folder.open().unwrap();
        fs.set_mtime(Path::new("set"), 1_700_000_001_234_567_891)
            .unwrap();
        let real = RealFolder::new(tmp.path()).open().unwrap();
        assert_eq!(
            real.lstat(Path::new("set")).unwrap().mtime_ns,
            1_700_000_000_000_000_000
        );
        assert_eq!(
            fs.lstat(Path::new("set")).unwrap().mtime_ns,
            1_700_000_000_000_000_000
        );
        let written = real.lstat(Path::new("written")).unwrap().mtime_ns;
        assert_eq!(
            fs.lstat(Path::new("written")).unwrap().mtime_ns,
            coarse(written)
        );
        let root = fs.open_dir(Path::new("")).unwrap();
        assert_eq!(
            root.lstat(OsStr::new("set")).unwrap().mtime_ns,
            1_700_000_000_000_000_000
        );
        assert_eq!(
            root.lstat(OsStr::new("written")).unwrap().mtime_ns,
            coarse(written)
        );
    }
}
