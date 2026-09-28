//! [`Spy`], a folder for tests that logs every call made through it, by
//! whole path, and can meddle with a file while it is read (DESIGN.md
//! §7.3: the walk's opens, and the re-stat after hashing).

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::fs::{Dir, Folder, Fs, ReadFile, RealFolder, Stat, WriteFile};

/// A change to make to a file the first time it is read.
type Meddle = Box<dyn FnOnce() + Send>;

/// A real folder that logs every call. Calls on the `Fs` are logged as
/// `fs.<name>`, calls through a held directory as `dir.<name>`, and
/// opening the root as `open_root`, each with the whole path it reached.
pub struct Spy {
    inner: RealFolder,
    state: Arc<State>,
}

#[derive(Default)]
struct State {
    calls: Mutex<Vec<(&'static str, PathBuf)>>,
    /// A file, and what to do to it after its first read returns.
    meddle: Mutex<Option<(PathBuf, Meddle)>>,
    meddled: Mutex<bool>,
}

impl State {
    fn log(&self, op: &'static str, path: &Path) {
        self.calls.lock().unwrap().push((op, path.to_path_buf()));
    }
}

impl Spy {
    pub fn new(root: &Path) -> Self {
        Self {
            inner: RealFolder::new(root),
            state: Arc::default(),
        }
    }

    /// Every call so far, in order.
    pub fn all(&self) -> Vec<(&'static str, PathBuf)> {
        self.state.calls.lock().unwrap().clone()
    }

    /// The paths of every call of `op` so far, in order.
    pub fn calls(&self, op: &str) -> Vec<PathBuf> {
        self.all()
            .into_iter()
            .filter(|(o, _)| *o == op)
            .map(|(_, path)| path)
            .collect()
    }

    /// Run `change` once, after the first read of the file at `path`
    /// returns: in the middle of hashing it.
    pub fn meddle(&self, path: &str, change: impl FnOnce() + Send + 'static) {
        *self.state.meddle.lock().unwrap() = Some((path.into(), Box::new(change)));
    }

    /// Whether the change has been made.
    pub fn meddled(&self) -> bool {
        *self.state.meddled.lock().unwrap()
    }
}

impl Folder for Spy {
    fn open(&self) -> io::Result<Box<dyn Fs>> {
        self.state.log("open_root", Path::new(""));
        Ok(Box::new(SpyFs {
            inner: self.inner.open()?,
            state: Arc::clone(&self.state),
        }))
    }
}

struct SpyFs {
    inner: Box<dyn Fs>,
    state: Arc<State>,
}

impl Fs for SpyFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        self.state.log("fs.read_dir", dir);
        self.inner.read_dir(dir)
    }

    fn open_dir(&self, path: &Path) -> io::Result<Box<dyn Dir>> {
        self.state.log("fs.open_dir", path);
        Ok(Box::new(SpyDir {
            inner: self.inner.open_dir(path)?,
            path: path.to_path_buf(),
            state: Arc::clone(&self.state),
        }))
    }

    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        self.state.log("fs.lstat", path);
        self.inner.lstat(path)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.state.log("fs.read_link", path);
        self.inner.read_link(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadFile>> {
        self.state.log("fs.open_read", path);
        self.inner.open_read(path)
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        self.state.log("fs.create_new", path);
        self.inner.create_new(path)
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        self.state.log("fs.open_append", path);
        self.inner.open_append(path)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.state.log("fs.sync_dir", path);
        self.inner.sync_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.state.log("fs.rename", from);
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.state.log("fs.rename_noreplace", from);
        self.inner.rename_noreplace(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.state.log("fs.remove_file", path);
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.state.log("fs.remove_dir", path);
        self.inner.remove_dir(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.state.log("fs.create_dir", path);
        self.inner.create_dir(path)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        self.state.log("fs.symlink", link);
        self.inner.symlink(target, link)
    }

    fn set_mtime(&self, path: &Path, mtime_ns: i64) -> io::Result<()> {
        self.state.log("fs.set_mtime", path);
        self.inner.set_mtime(path, mtime_ns)
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.state.log("fs.set_mode", path);
        self.inner.set_mode(path, mode)
    }

    fn available_space(&self, path: &Path) -> io::Result<u64> {
        self.state.log("fs.available_space", path);
        self.inner.available_space(path)
    }
}

struct SpyDir {
    inner: Box<dyn Dir>,
    path: PathBuf,
    state: Arc<State>,
}

impl Dir for SpyDir {
    fn read_dir(&self) -> io::Result<Vec<OsString>> {
        self.state.log("dir.read_dir", &self.path);
        self.inner.read_dir()
    }

    fn lstat(&self, name: &OsStr) -> io::Result<Stat> {
        self.state.log("dir.lstat", &self.path.join(name));
        self.inner.lstat(name)
    }

    fn read_link(&self, name: &OsStr) -> io::Result<PathBuf> {
        self.state.log("dir.read_link", &self.path.join(name));
        self.inner.read_link(name)
    }

    fn open_read(&self, name: &OsStr) -> io::Result<Box<dyn ReadFile>> {
        let path = self.path.join(name);
        self.state.log("dir.open_read", &path);
        let inner = self.inner.open_read(name)?;
        let mut meddle = self.state.meddle.lock().unwrap();
        let change = match meddle.take() {
            Some((at, change)) if at == path => Some(change),
            other => {
                *meddle = other;
                None
            }
        };
        Ok(Box::new(SpyRead {
            inner,
            change,
            state: Arc::clone(&self.state),
        }))
    }

    fn open_dir(&self, name: &OsStr) -> io::Result<Box<dyn Dir>> {
        let path = self.path.join(name);
        self.state.log("dir.open_dir", &path);
        Ok(Box::new(SpyDir {
            inner: self.inner.open_dir(name)?,
            path,
            state: Arc::clone(&self.state),
        }))
    }
}

struct SpyRead {
    inner: Box<dyn ReadFile>,
    change: Option<Meddle>,
    state: Arc<State>,
}

impl ReadFile for SpyRead {}

impl Read for SpyRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some(change) = self.change.take() {
            change();
            *self.state.meddled.lock().unwrap() = true;
        }
        Ok(n)
    }
}

impl Seek for SpyRead {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.inner.seek(to)
    }
}
