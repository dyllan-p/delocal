//! [`Spy`], a folder for tests that logs every call made through it, by
//! whole path, and can meddle with a file while it is read (DESIGN.md
//! §7.3: the walk's opens, and the re-stat after hashing). The scan's tests
//! and the commit path's use it. For the commit path it can also run
//! something just before a given call (a user writing a path between the
//! check and the rename, §7.5; a `kill -9` at a given step), make a
//! directory read as a mount point (§7.3), and skip fsyncs, for tests that
//! drive thousands of commits and test atomicity, not durability.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::{Dir, Folder, Fs, ReadFile, RealFolder, Stat, WriteFile};

/// A change to make to a file the first time it is read.
type Meddle = Box<dyn FnOnce() + Send>;

/// Something to run once, just before a call.
type Hook = Box<dyn FnOnce() + Send>;

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
    /// Every call with all of its paths: a rename's source and target.
    steps: Mutex<Vec<(&'static str, Vec<PathBuf>)>>,
    /// Calls so far, logged or not.
    count: AtomicUsize,
    /// Log nothing, only count.
    quiet: AtomicBool,
    /// Succeed at every fsync without making one.
    no_sync: AtomicBool,
    /// What to run before the call of an op at a path, by its first path.
    before: Mutex<Vec<(&'static str, PathBuf, Hook)>>,
    /// The same, by the start of its first path.
    before_prefix: Mutex<Vec<(&'static str, String, Hook)>>,
    /// What to run before the call with this index, counting from 0.
    at: Mutex<Vec<(usize, Hook)>>,
    /// Directories that read as the root of another device, with
    /// everything beneath them: mount points (§7.3).
    mounts: Mutex<Vec<PathBuf>>,
    /// A file, and what to do to it after its first read returns.
    meddle: Mutex<Option<(PathBuf, Meddle)>>,
    meddled: Mutex<bool>,
}

impl State {
    fn log(&self, op: &'static str, path: &Path) {
        self.call(op, &[path]);
    }

    /// Count and log a call with all its paths, then run what waits for it.
    fn call(&self, op: &'static str, paths: &[&Path]) {
        let index = self.count.fetch_add(1, Ordering::SeqCst);
        let first = paths.first().copied().unwrap_or(Path::new(""));
        if !self.quiet.load(Ordering::Relaxed) {
            self.calls.lock().unwrap().push((op, first.to_path_buf()));
            let all = paths.iter().map(|p| p.to_path_buf()).collect();
            self.steps.lock().unwrap().push((op, all));
        }
        let hook = {
            let mut at = self.at.lock().unwrap();
            at.iter()
                .position(|(i, _)| *i == index)
                .map(|i| at.remove(i).1)
        };
        if let Some(hook) = hook {
            hook();
        }
        let hook = {
            let mut before = self.before.lock().unwrap();
            before
                .iter()
                .position(|(o, p, _)| *o == op && p == first)
                .map(|i| before.remove(i).2)
        };
        if let Some(hook) = hook {
            hook();
        }
        let hook = {
            let mut before = self.before_prefix.lock().unwrap();
            let first = first.to_string_lossy();
            before
                .iter()
                .position(|(o, prefix, _)| *o == op && first.starts_with(prefix.as_str()))
                .map(|i| before.remove(i).2)
        };
        if let Some(hook) = hook {
            hook();
        }
    }

    /// `stat` of `path`, on another device if it is at or beneath a mount
    /// point.
    fn mounted(&self, path: &Path, stat: io::Result<Stat>) -> io::Result<Stat> {
        let mut stat = stat?;
        if self
            .mounts
            .lock()
            .unwrap()
            .iter()
            .any(|m| path.starts_with(m))
        {
            stat.dev = stat.dev.wrapping_add(1);
        }
        Ok(stat)
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

    /// Every call so far with all of its paths, in order: what a FaultyFs
    /// rule counts a call by.
    pub fn steps(&self) -> Vec<(&'static str, Vec<PathBuf>)> {
        self.state.steps.lock().unwrap().clone()
    }

    /// Stop logging calls; they are still counted, and hooks still run.
    pub fn quiet(&self) {
        self.state.quiet.store(true, Ordering::Relaxed);
    }

    /// Make every fsync succeed without syncing.
    pub fn no_sync(&self) {
        self.state.no_sync.store(true, Ordering::Relaxed);
    }

    /// Run `hook` once, just before the first call of `op` (as logged,
    /// `fs.<name>`) whose first path is `path`.
    pub fn before(
        &self,
        op: &'static str,
        path: impl Into<PathBuf>,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.state
            .before
            .lock()
            .unwrap()
            .push((op, path.into(), Box::new(hook)));
    }

    /// Run `hook` once, just before the first call of `op` whose first path
    /// starts with `prefix`: for a call on a temp whose name is fresh.
    pub fn before_prefix(
        &self,
        op: &'static str,
        prefix: &str,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.state
            .before_prefix
            .lock()
            .unwrap()
            .push((op, prefix.to_owned(), Box::new(hook)));
    }

    /// Count and log a step that is not a filesystem call (a journal write,
    /// say) as if it were one, named `op`, so that hooks can be set on it.
    pub fn mark(&self, op: &'static str) {
        self.state.call(op, &[]);
    }

    /// Run `hook` once, just before the call with index `index`, counting
    /// every call from 0.
    pub fn at_call(&self, index: usize, hook: impl FnOnce() + Send + 'static) {
        self.state.at.lock().unwrap().push((index, Box::new(hook)));
    }

    /// Make the directory at `path` read as a mount point: it and
    /// everything beneath it are stated on another device than the root.
    /// Nothing else about it changes.
    pub fn mount(&self, path: &str) {
        self.state.mounts.lock().unwrap().push(path.into());
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
        self.state.mounted(path, self.inner.lstat(path))
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

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.state.log("fs.sync_file", path);
        if self.state.no_sync.load(Ordering::Relaxed) {
            return self.inner.lstat(path).map(|_| ());
        }
        self.inner.sync_file(path)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.state.log("fs.sync_dir", path);
        if self.state.no_sync.load(Ordering::Relaxed) {
            return self.inner.lstat(path).map(|_| ());
        }
        self.inner.sync_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.state.call("fs.rename", &[from, to]);
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.state.call("fs.rename_noreplace", &[from, to]);
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
        let path = self.path.join(name);
        self.state.log("dir.lstat", &path);
        self.state.mounted(&path, self.inner.lstat(name))
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
