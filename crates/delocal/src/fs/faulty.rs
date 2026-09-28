//! [`FaultyFolder`] and [`FaultyFs`]: the fault-injecting wrapper of the
//! filesystem layer (DESIGN.md §14.2), built only with the `faults` feature.
//!
//! A `FaultyFolder` wraps another [`Folder`] and follows a [`Spec`]: scripted
//! rules, each an operation, a path pattern, a trigger and a fault (the
//! [`spec`] module says exactly what each means). Each operation's
//! [`open`](Folder::open) gives a `FaultyFs` over what the wrapped folder
//! opened, and every `FaultyFs` shares the folder's rules and counters, so a
//! rule's nth call counts across operations, as a spec read once at spawn
//! means it to. A call no rule fires on goes to the wrapped filesystem
//! unchanged, so with no rules a `FaultyFs` passes the same conformance
//! checks as what it wraps.
//!
//! **Deterministic.** Rules hold no randomness and no clock. Each call rule
//! counts the calls that match its operation and pattern, whether or not it
//! fires, and an offset rule looks only at the file's position. So the same
//! spec and the same sequence of calls give the same faults on every run,
//! and [`FaultyFolder::injected`] lists them in the order they happened.
//!
//! When several rules fire on one call, of either kind, the first in the
//! spec decides the fault. A call no rule fires on but that would cross an
//! offset rule's byte moves only the bytes before the nearest such byte.
//!
//! **Every call, however it is reached.** A [`Dir`] from
//! [`open_dir`](Fs::open_dir) is wrapped too, and its calls are checked
//! against the same rules as the [`Fs`] call of the same name, under the
//! entry's whole path: a rule for `lstat` on `**/private/*` fires on
//! `Fs::lstat("a/private/x")` and on `lstat("x")` through the held `a/private`
//! alike, and counts both. So a scan that walks with held directories
//! (§7.3) meets exactly the faults a spec written in paths describes.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{Dir, Folder, Fs, ReadFile, Stat, WriteFile};

pub mod pattern;
pub mod spec;

use pattern::Pattern;
pub use spec::{Fault, Op, Rule, Spec, SpecError, Trigger};

/// One fault a [`FaultyFolder`] injected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Injected {
    /// The rule that fired: its index in the spec, counting from 0.
    pub rule: usize,
    pub op: Op,
    /// The call's path, relative to the folder root: a rename's source, a
    /// symlink's link, the path an open file was opened with, the empty path
    /// for opening the root.
    pub path: PathBuf,
    pub fault: Fault,
}

/// A folder that injects faults into another's operations. See the module
/// docs.
pub struct FaultyFolder<F> {
    inner: F,
    rules: Arc<Rules>,
}

/// One operation on a [`FaultyFolder`]: what the wrapped folder opened, with
/// the folder's rules.
pub struct FaultyFs {
    inner: Box<dyn Fs>,
    rules: Arc<Rules>,
}

/// The rules and their counters, shared with every operation and every open
/// file, so that everything is counted together.
struct Rules {
    compiled: Vec<(Rule, Pattern)>,
    state: Mutex<State>,
}

struct State {
    /// Matching calls so far, per rule.
    calls: Vec<u64>,
    injected: Vec<Injected>,
}

/// What one call may do.
enum Verdict {
    Proceed,
    Fail(Fault),
    /// A read or write may move at most this many bytes, stopping short of
    /// an offset rule's byte.
    AtMost(u64),
}

impl<F: Folder> FaultyFolder<F> {
    /// Wrap `inner` with the rules of `spec`, which is validated again here
    /// in case it was built in code rather than parsed.
    pub fn new(inner: F, spec: Spec) -> Result<Self, SpecError> {
        spec.validate()?;
        let calls = vec![0; spec.rules.len()];
        let compiled = spec
            .rules
            .into_iter()
            .map(|rule| {
                let pattern = Pattern::new(&rule.path);
                (rule, pattern)
            })
            .collect();
        Ok(Self {
            inner,
            rules: Arc::new(Rules {
                compiled,
                state: Mutex::new(State {
                    calls,
                    injected: Vec::new(),
                }),
            }),
        })
    }

    /// Every fault injected so far, by every operation, in order.
    pub fn injected(&self) -> Vec<Injected> {
        self.rules.state().injected.clone()
    }

    /// [`Folder::open`], unboxed.
    fn open_faulty(&self) -> io::Result<FaultyFs> {
        self.rules.fail(Op::OpenRoot, &[Path::new("")])?;
        Ok(FaultyFs {
            inner: self.inner.open()?,
            rules: Arc::clone(&self.rules),
        })
    }
}

impl<F: Folder> Folder for FaultyFolder<F> {
    fn open(&self) -> io::Result<Box<dyn Fs>> {
        Ok(Box::new(self.open_faulty()?))
    }
}

impl FaultyFs {
    /// Every fault injected so far, by every operation on this folder.
    pub fn injected(&self) -> Vec<Injected> {
        self.rules.state().injected.clone()
    }

    /// Count a call without a position, and fail it if a rule fires.
    fn check(&self, op: Op, paths: &[&Path]) -> io::Result<()> {
        self.rules.fail(op, paths)
    }
}

impl Rules {
    /// Count a call without a position, and fail it if a rule fires.
    fn fail(&self, op: Op, paths: &[&Path]) -> io::Result<()> {
        match self.check(op, paths, None) {
            Verdict::Fail(fault) => Err(fault.error()),
            Verdict::Proceed | Verdict::AtMost(_) => Ok(()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while the lock was held (a failing test) leaves the counters
        // as they were; they are still the truth, so carry on with them.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Count a call against every rule it matches, and decide it. `pos` is a
    /// read's or write's position in its file, `None` for other calls.
    fn check(&self, op: Op, paths: &[&Path], pos: Option<u64>) -> Verdict {
        let mut state = self.state();
        let mut fired = None;
        let mut nearest: Option<u64> = None;
        for (index, (rule, pattern)) in self.compiled.iter().enumerate() {
            let matched = rule.op == op
                && paths
                    .iter()
                    .any(|p| pattern.matches(p.as_os_str().as_bytes()));
            if !matched {
                continue;
            }
            let fires = match (rule.at, pos) {
                (Trigger::Call(n), _) => count(&mut state.calls[index]) == n,
                (Trigger::FromCall(n), _) => count(&mut state.calls[index]) >= n,
                (Trigger::Offset(offset), Some(pos)) if pos >= offset => true,
                (Trigger::Offset(offset), Some(_)) => {
                    nearest = Some(nearest.map_or(offset, |o| o.min(offset)));
                    false
                }
                (Trigger::Offset(_), None) => false,
            };
            if fires && fired.is_none() {
                fired = Some(index);
            }
        }
        let path = paths.first().map(|p| p.to_path_buf()).unwrap_or_default();
        let mut inject = |index: usize| {
            let fault = self.compiled[index].0.fail;
            state.injected.push(Injected {
                rule: index,
                op,
                path: path.clone(),
                fault,
            });
            Verdict::Fail(fault)
        };
        match (fired, pos, nearest) {
            (Some(index), _, _) => inject(index),
            (None, Some(pos), Some(offset)) => Verdict::AtMost(offset - pos),
            _ => Verdict::Proceed,
        }
    }
}

/// Add one matching call to a rule's counter and return the new count.
fn count(calls: &mut u64) -> u64 {
    *calls = calls.saturating_add(1);
    *calls
}

/// A buffer of `len` bytes cut to at most `limit`.
fn at_most(len: usize, limit: u64) -> usize {
    usize::try_from(limit).map_or(len, |limit| len.min(limit))
}

impl Fs for FaultyFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        self.check(Op::ReadDir, &[dir])?;
        self.inner.read_dir(dir)
    }

    fn open_dir(&self, path: &Path) -> io::Result<Box<dyn Dir>> {
        self.check(Op::OpenDir, &[path])?;
        Ok(Box::new(FaultyDir {
            inner: self.inner.open_dir(path)?,
            path: path.to_path_buf(),
            rules: Arc::clone(&self.rules),
        }))
    }

    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        self.check(Op::Lstat, &[path])?;
        self.inner.lstat(path)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.check(Op::ReadLink, &[path])?;
        self.inner.read_link(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadFile>> {
        self.check(Op::OpenRead, &[path])?;
        Ok(Box::new(FaultyRead {
            inner: self.inner.open_read(path)?,
            path: path.to_path_buf(),
            pos: 0,
            rules: Arc::clone(&self.rules),
        }))
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        self.check(Op::CreateNew, &[path])?;
        Ok(Box::new(FaultyWrite {
            inner: self.inner.create_new(path)?,
            path: path.to_path_buf(),
            pos: 0,
            rules: Arc::clone(&self.rules),
        }))
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn WriteFile>> {
        self.check(Op::OpenAppend, &[path])?;
        let inner = self.inner.open_append(path)?;
        // Offsets count from the start of the file, and an append handle
        // writes at its end. Asking the wrapped filesystem is not a call of
        // this one, so no rule counts it.
        let pos = self.inner.lstat(path)?.size;
        Ok(Box::new(FaultyWrite {
            inner,
            path: path.to_path_buf(),
            pos,
            rules: Arc::clone(&self.rules),
        }))
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.check(Op::SyncDir, &[path])?;
        self.inner.sync_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check(Op::Rename, &[from, to])?;
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check(Op::RenameNoreplace, &[from, to])?;
        self.inner.rename_noreplace(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.check(Op::RemoveFile, &[path])?;
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.check(Op::RemoveDir, &[path])?;
        self.inner.remove_dir(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.check(Op::CreateDir, &[path])?;
        self.inner.create_dir(path)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        self.check(Op::Symlink, &[link])?;
        self.inner.symlink(target, link)
    }

    fn set_mtime(&self, path: &Path, mtime_ns: i64) -> io::Result<()> {
        self.check(Op::SetMtime, &[path])?;
        self.inner.set_mtime(path, mtime_ns)
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.check(Op::SetMode, &[path])?;
        self.inner.set_mode(path, mode)
    }

    fn available_space(&self, path: &Path) -> io::Result<u64> {
        match self.rules.check(Op::AvailableSpace, &[path], None) {
            Verdict::Fail(Fault::Enospc) => Ok(0),
            Verdict::Fail(fault) => Err(fault.error()),
            Verdict::Proceed | Verdict::AtMost(_) => self.inner.available_space(path),
        }
    }
}

/// A directory from [`FaultyFs::open_dir`] or [`FaultyDir::open_dir`]. Its
/// calls are checked under the entry's whole path (see the module docs).
struct FaultyDir {
    inner: Box<dyn Dir>,
    /// The directory's path, relative to the folder root.
    path: PathBuf,
    rules: Arc<Rules>,
}

impl Dir for FaultyDir {
    fn read_dir(&self) -> io::Result<Vec<OsString>> {
        self.rules.fail(Op::ReadDir, &[&self.path])?;
        self.inner.read_dir()
    }

    fn lstat(&self, name: &OsStr) -> io::Result<Stat> {
        self.rules.fail(Op::Lstat, &[&self.path.join(name)])?;
        self.inner.lstat(name)
    }

    fn read_link(&self, name: &OsStr) -> io::Result<PathBuf> {
        self.rules.fail(Op::ReadLink, &[&self.path.join(name)])?;
        self.inner.read_link(name)
    }

    fn open_read(&self, name: &OsStr) -> io::Result<Box<dyn ReadFile>> {
        let path = self.path.join(name);
        self.rules.fail(Op::OpenRead, &[&path])?;
        Ok(Box::new(FaultyRead {
            inner: self.inner.open_read(name)?,
            path,
            pos: 0,
            rules: Arc::clone(&self.rules),
        }))
    }

    fn open_dir(&self, name: &OsStr) -> io::Result<Box<dyn Dir>> {
        let path = self.path.join(name);
        self.rules.fail(Op::OpenDir, &[&path])?;
        Ok(Box::new(FaultyDir {
            inner: self.inner.open_dir(name)?,
            path,
            rules: Arc::clone(&self.rules),
        }))
    }
}

/// A file from [`FaultyFs::open_read`] or [`FaultyDir::open_read`].
struct FaultyRead {
    inner: Box<dyn ReadFile>,
    path: PathBuf,
    /// The position in the file, for offset rules.
    pos: u64,
    rules: Arc<Rules>,
}

impl ReadFile for FaultyRead {}

impl Read for FaultyRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = match self.rules.check(Op::Read, &[&self.path], Some(self.pos)) {
            Verdict::Fail(fault) => return Err(fault.error()),
            Verdict::AtMost(limit) => at_most(buf.len(), limit),
            Verdict::Proceed => buf.len(),
        };
        let n = self.inner.read(&mut buf[..len])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for FaultyRead {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(to)?;
        Ok(self.pos)
    }
}

/// A file from [`FaultyFs::create_new`] or [`FaultyFs::open_append`].
struct FaultyWrite {
    inner: Box<dyn WriteFile>,
    path: PathBuf,
    /// The position in the file, for offset rules.
    pos: u64,
    rules: Arc<Rules>,
}

impl Write for FaultyWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = match self.rules.check(Op::Write, &[&self.path], Some(self.pos)) {
            Verdict::Fail(fault) => return Err(fault.error()),
            Verdict::AtMost(limit) => at_most(buf.len(), limit),
            Verdict::Proceed => buf.len(),
        };
        let n = self.inner.write(&buf[..len])?;
        self.pos += n as u64;
        Ok(n)
    }

    /// Not an operation: a `File` buffers nothing, so `flush` does nothing
    /// that could fail. Durability is `sync`.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl WriteFile for FaultyWrite {
    fn sync(&mut self) -> io::Result<()> {
        if let Verdict::Fail(fault) = self.rules.check(Op::Sync, &[&self.path], None) {
            return Err(fault.error());
        }
        self.inner.sync()
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use proptest::prelude::*;

    use super::*;
    use crate::fs::{NoReplaceUnsupported, RealFolder};

    crate::fs::conformance::conformance_tests!(|root: &Path| {
        FaultyFolder::new(RealFolder::new(root), Spec::default()).unwrap()
    });

    fn rule(op: Op, path: &str, at: Trigger, fail: Fault) -> Rule {
        Rule {
            op,
            path: path.into(),
            at,
            fail,
        }
    }

    /// One operation on a `FaultyFolder` with `rules` over the folder at
    /// `root`. The `FaultyFs` shares the folder's rules, so `injected` on it
    /// sees every operation's faults.
    fn faulty(root: &Path, rules: Vec<Rule>) -> FaultyFs {
        faulty_folder(root, rules).open_faulty().unwrap()
    }

    fn faulty_folder(root: &Path, rules: Vec<Rule>) -> FaultyFolder<RealFolder> {
        FaultyFolder::new(RealFolder::new(root), Spec { rules }).unwrap()
    }

    fn p(path: &str) -> &Path {
        Path::new(path)
    }

    fn errno<T>(result: io::Result<T>) -> Option<i32> {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.raw_os_error(),
        }
    }

    fn injected(rule: usize, op: Op, path: &str, fault: Fault) -> Injected {
        Injected {
            rule,
            op,
            path: path.into(),
            fault,
        }
    }

    #[test]
    fn call_fails_only_the_nth_matching_call() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a"), "x").unwrap();
        std::fs::write(root.join("b"), "x").unwrap();
        let fs = faulty(
            root,
            vec![rule(Op::Lstat, "**/a", Trigger::Call(2), Fault::Eio)],
        );
        fs.lstat(p("a")).unwrap();
        fs.lstat(p("b")).unwrap(); // does not match, so not counted
        assert_eq!(errno(fs.lstat(p("a"))), Some(5));
        fs.lstat(p("a")).unwrap();
        fs.read_dir(p("")).unwrap(); // another op, not counted
        assert_eq!(fs.injected(), [injected(0, Op::Lstat, "a", Fault::Eio)]);
    }

    #[test]
    fn from_call_fails_every_call_from_the_nth_and_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let fs = faulty(
            dir.path(),
            vec![rule(
                Op::CreateDir,
                "**",
                Trigger::FromCall(2),
                Fault::Eacces,
            )],
        );
        fs.create_dir(p("d1")).unwrap();
        for name in ["d2", "d3", "d4"] {
            assert_eq!(errno(fs.create_dir(p(name))), Some(13));
        }
        assert_eq!(
            fs.read_dir(p("")).unwrap(),
            ["d1"],
            "a failed call does nothing"
        );
        assert_eq!(fs.injected().len(), 3);
    }

    #[test]
    fn offset_moves_the_bytes_before_it_then_fails_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("tmp")).unwrap();
        let fs = faulty(
            root,
            vec![rule(
                Op::Write,
                "**/tmp/*",
                Trigger::Offset(5),
                Fault::Enospc,
            )],
        );

        let mut file = fs.create_new(p("tmp/t")).unwrap();
        let err = file.write_all(b"0123456789").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::StorageFull);
        drop(file);
        assert_eq!(std::fs::read(root.join("tmp/t")).unwrap(), b"01234");

        // Resuming at the end of the file is already past the offset.
        let mut file = fs.open_append(p("tmp/t")).unwrap();
        assert_eq!(errno(file.write(b"x")), Some(28));
        assert_eq!(std::fs::read(root.join("tmp/t")).unwrap(), b"01234");

        let mut file = fs.create_new(p("other")).unwrap();
        file.write_all(b"0123456789").unwrap();
        assert_eq!(std::fs::read(root.join("other")).unwrap(), b"0123456789");
        assert_eq!(
            fs.injected(),
            [
                injected(0, Op::Write, "tmp/t", Fault::Enospc),
                injected(0, Op::Write, "tmp/t", Fault::Enospc),
            ]
        );
    }

    #[test]
    fn offset_cuts_a_read_short_then_fails_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "hello world").unwrap();
        let fs = faulty(
            dir.path(),
            vec![rule(Op::Read, "**/f", Trigger::Offset(5), Fault::Eio)],
        );
        let mut file = fs.open_read(p("f")).unwrap();
        let mut bytes = Vec::new();
        assert_eq!(errno(file.read_to_end(&mut bytes)), Some(5));
        assert_eq!(bytes, b"hello", "the bytes before the offset arrive");

        // Seeking back before the offset reads again; past it fails.
        file.seek(SeekFrom::Start(1)).unwrap();
        let mut four = [0; 4];
        file.read_exact(&mut four).unwrap();
        assert_eq!(&four, b"ello");
        file.seek(SeekFrom::Start(6)).unwrap();
        assert_eq!(errno(file.read(&mut four)), Some(5));
    }

    #[test]
    fn a_rename_matches_on_either_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("trash")).unwrap();
        std::fs::write(root.join("a"), "a").unwrap();
        std::fs::write(root.join("trash/b"), "b").unwrap();
        let fs = faulty(
            root,
            vec![rule(
                Op::Rename,
                "**/trash/*",
                Trigger::FromCall(1),
                Fault::Exdev,
            )],
        );
        let err = fs.rename(p("a"), p("trash/a")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::CrossesDevices);
        assert_eq!(errno(fs.rename(p("trash/b"), p("b"))), Some(18));
        assert_eq!(fs.read_dir(p("")).unwrap(), ["a", "trash"], "nothing moved");
        assert_eq!(fs.read_dir(p("trash")).unwrap(), ["b"]);
        fs.rename(p("a"), p("c")).unwrap();
    }

    #[test]
    fn every_matching_rule_counts_and_the_first_to_fire_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "x").unwrap();
        let fs = faulty(
            dir.path(),
            vec![
                rule(Op::Lstat, "**", Trigger::Call(2), Fault::Eio),
                rule(Op::Lstat, "**/a", Trigger::Call(2), Fault::Eacces),
                rule(Op::Lstat, "**/a", Trigger::Call(3), Fault::Enospc),
            ],
        );
        fs.lstat(p("a")).unwrap();
        // Rules 0 and 1 both fire on the second call; rule 0 is first.
        assert_eq!(errno(fs.lstat(p("a"))), Some(5));
        // Rule 1 counted that call, so it does not fire on the third.
        assert_eq!(errno(fs.lstat(p("a"))), Some(28));
        fs.lstat(p("a")).unwrap();
        assert_eq!(
            fs.injected(),
            [
                injected(0, Op::Lstat, "a", Fault::Eio),
                injected(2, Op::Lstat, "a", Fault::Enospc),
            ]
        );
    }

    #[test]
    fn the_first_rule_to_fire_wins_whatever_its_trigger() {
        let dir = tempfile::tempdir().unwrap();
        // An offset rule listed first beats a call rule firing on the same
        // write.
        let fs = faulty(
            dir.path(),
            vec![
                rule(Op::Write, "**", Trigger::Offset(0), Fault::Eio),
                rule(Op::Write, "**", Trigger::Call(1), Fault::Enospc),
            ],
        );
        assert_eq!(errno(fs.create_new(p("t")).unwrap().write(b"x")), Some(5));

        // Past two offsets, the first rule wins, not the smaller offset.
        std::fs::write(dir.path().join("u"), [0; 12]).unwrap();
        let fs = faulty(
            dir.path(),
            vec![
                rule(Op::Write, "**", Trigger::Offset(10), Fault::Eio),
                rule(Op::Write, "**", Trigger::Offset(5), Fault::Enospc),
            ],
        );
        assert_eq!(errno(fs.open_append(p("u")).unwrap().write(b"x")), Some(5));

        // Before both, a write stops short of the nearer one, whichever is
        // listed first, and the next write fails there.
        let mut file = fs.create_new(p("t.new")).unwrap();
        assert_eq!(file.write(b"0123456789abcdef").unwrap(), 5);
        assert_eq!(errno(file.write(b"5")), Some(28));
        assert_eq!(
            fs.injected().iter().map(|i| i.rule).collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn enospc_on_available_space_reports_nothing_free() {
        let dir = tempfile::tempdir().unwrap();
        let fs = faulty(
            dir.path(),
            vec![
                rule(Op::AvailableSpace, "**", Trigger::Call(1), Fault::Enospc),
                rule(Op::AvailableSpace, "**", Trigger::Call(2), Fault::Eio),
            ],
        );
        assert_eq!(fs.available_space(p("")).unwrap(), 0);
        assert_eq!(errno(fs.available_space(p(""))), Some(5));
        assert!(fs.available_space(p("")).unwrap() > 0);
    }

    #[test]
    fn opening_and_using_a_file_are_separate_ops() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "x").unwrap();
        let fs = faulty(
            dir.path(),
            vec![
                rule(Op::OpenRead, "**/f", Trigger::Call(1), Fault::Eacces),
                rule(Op::CreateNew, "**/t", Trigger::Call(1), Fault::Enospc),
                rule(Op::Sync, "**/t", Trigger::Call(1), Fault::Eio),
            ],
        );
        assert_eq!(errno(fs.open_read(p("f"))), Some(13));
        fs.open_read(p("f")).unwrap();
        assert_eq!(errno(fs.create_new(p("t"))), Some(28));
        assert!(
            !dir.path().join("t").exists(),
            "a failed create creates nothing"
        );
        let mut file = fs.create_new(p("t")).unwrap();
        file.write_all(b"data").unwrap();
        assert_eq!(errno(file.sync()), Some(5));
        file.sync().unwrap();
    }

    #[test]
    fn rename_noreplace_takes_eexist_exdev_and_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "a").unwrap();
        let fs = faulty(
            dir.path(),
            vec![
                rule(Op::RenameNoreplace, "b", Trigger::Call(1), Fault::Eexist),
                rule(Op::RenameNoreplace, "b", Trigger::Call(2), Fault::Exdev),
                rule(
                    Op::RenameNoreplace,
                    "b",
                    Trigger::Call(3),
                    Fault::Unsupported,
                ),
            ],
        );
        let exists = fs.rename_noreplace(p("a"), p("b")).unwrap_err();
        assert_eq!(exists.kind(), ErrorKind::AlreadyExists);
        assert_eq!(errno(fs.rename_noreplace(p("a"), p("b"))), Some(18));
        let unsupported = fs.rename_noreplace(p("a"), p("b")).unwrap_err();
        assert!(NoReplaceUnsupported::of(&unsupported));
        assert_eq!(fs.read_dir(p("")).unwrap(), ["a"], "nothing moved");
        // A plain rename is another operation, and none of these rules is
        // about it.
        fs.rename(p("a"), p("c")).unwrap();
        fs.rename_noreplace(p("c"), p("b")).unwrap();
        assert_eq!(fs.read_dir(p("")).unwrap(), ["b"]);
    }

    #[test]
    fn opening_the_root_is_an_operation_and_counts_carry_across_operations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "x").unwrap();
        let folder = faulty_folder(
            dir.path(),
            vec![
                rule(Op::OpenRoot, "", Trigger::Call(2), Fault::Eacces),
                rule(Op::Lstat, "a", Trigger::Call(3), Fault::Eio),
            ],
        );
        // Operation one: two lstats.
        let fs = folder.open().unwrap();
        fs.lstat(p("a")).unwrap();
        fs.lstat(p("a")).unwrap();
        drop(fs);
        // The second open of the root fails, and opens nothing.
        assert_eq!(errno(folder.open()), Some(13));
        // Operation three: its first lstat is the rule's third call.
        let fs = folder.open().unwrap();
        assert_eq!(errno(fs.lstat(p("a"))), Some(5));
        assert_eq!(
            folder.injected(),
            [
                injected(0, Op::OpenRoot, "", Fault::Eacces),
                injected(1, Op::Lstat, "a", Fault::Eio),
            ]
        );
    }

    /// A held directory's calls meet the rules written for whole paths, and
    /// are counted with the calls made by path.
    #[test]
    fn rules_reach_a_held_directory_by_whole_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/private/sub")).unwrap();
        std::fs::write(root.join("a/private/x"), "xyz").unwrap();
        std::os::unix::fs::symlink("x", root.join("a/private/l")).unwrap();
        let fs = faulty(
            root,
            vec![
                rule(Op::Lstat, "**/private/*", Trigger::Call(2), Fault::Eio),
                rule(Op::ReadLink, "a/private/l", Trigger::Call(1), Fault::Eacces),
                rule(Op::OpenRead, "a/private/x", Trigger::Call(1), Fault::Eio),
                rule(Op::Read, "a/private/x", Trigger::Offset(1), Fault::Eio),
                rule(Op::ReadDir, "a/private", Trigger::Call(1), Fault::Eio),
                rule(
                    Op::OpenDir,
                    "a/private/sub",
                    Trigger::Call(1),
                    Fault::Eacces,
                ),
                rule(Op::OpenDir, "a", Trigger::Call(2), Fault::Eio),
            ],
        );
        let name = std::ffi::OsStr::new;

        fs.lstat(p("a/private/x")).unwrap();
        let a = fs.open_dir(p("a")).unwrap();
        let private = a.open_dir(name("private")).unwrap();
        // The second lstat under `private`, the first by path.
        assert_eq!(errno(private.lstat(name("x"))), Some(5));
        private.lstat(name("x")).unwrap();
        assert_eq!(errno(private.read_link(name("l"))), Some(13));
        assert_eq!(private.read_link(name("l")).unwrap(), p("x"));
        assert_eq!(errno(private.open_read(name("x"))), Some(5));
        let mut file = private.open_read(name("x")).unwrap();
        let mut bytes = Vec::new();
        assert_eq!(errno(file.read_to_end(&mut bytes)), Some(5));
        assert_eq!(bytes, b"x", "the offset rule counts from the file's start");
        assert_eq!(errno(private.read_dir()), Some(5));
        assert_eq!(private.read_dir().unwrap(), ["l", "sub", "x"]);
        assert_eq!(errno(private.open_dir(name("sub"))), Some(13));
        private.open_dir(name("sub")).unwrap();
        assert_eq!(errno(fs.open_dir(p("a"))), Some(5));

        assert_eq!(
            fs.injected(),
            [
                injected(0, Op::Lstat, "a/private/x", Fault::Eio),
                injected(1, Op::ReadLink, "a/private/l", Fault::Eacces),
                injected(2, Op::OpenRead, "a/private/x", Fault::Eio),
                injected(3, Op::Read, "a/private/x", Fault::Eio),
                injected(4, Op::ReadDir, "a/private", Fault::Eio),
                injected(5, Op::OpenDir, "a/private/sub", Fault::Eacces),
                injected(6, Op::OpenDir, "a", Fault::Eio),
            ]
        );
    }

    /// One step of a scripted run over a fresh directory, for the
    /// determinism tests.
    #[derive(Clone, Debug)]
    enum Step {
        Lstat(&'static str),
        Mkdir(&'static str),
        /// Create, write this many bytes, sync.
        Write(&'static str, usize),
        Read(&'static str),
        Rename(&'static str, &'static str),
        Remove(&'static str),
        List(&'static str),
    }

    /// Run `steps` through a `FaultyFs` over a fresh temporary directory and
    /// describe everything that happened: each step's result, then the
    /// injected faults.
    fn trace(spec: &Spec, steps: &[Step]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let folder = FaultyFolder::new(RealFolder::new(root), spec.clone()).unwrap();
        let fs = folder.open().unwrap();
        let outcome = |result: io::Result<String>| match result {
            Ok(ok) => ok,
            Err(e) => match e.raw_os_error() {
                Some(5) => "EIO".into(),
                Some(13) => "EACCES".into(),
                Some(18) => "EXDEV".into(),
                Some(17) => "EEXIST".into(),
                Some(28) => "ENOSPC".into(),
                _ => format!("{:?}", e.kind()),
            },
        };
        let mut lines = Vec::new();
        for step in steps {
            let (what, result) = match *step {
                Step::Lstat(path) => (
                    format!("lstat {path}"),
                    fs.lstat(p(path))
                        .map(|s| format!("{:?} {}", s.kind, s.size)),
                ),
                Step::Mkdir(path) => (
                    format!("mkdir {path}"),
                    fs.create_dir(p(path)).map(|()| "ok".into()),
                ),
                Step::Write(path, len) => {
                    let result = fs.create_new(p(path)).and_then(|mut file| {
                        file.write_all(&vec![b'x'; len])?;
                        file.sync()
                    });
                    let on_disk = std::fs::metadata(root.join(path)).map_or(0, |m| m.len());
                    let result = match result {
                        Ok(()) => Ok("ok".into()),
                        Err(e) => Ok(format!("{}, {on_disk} bytes on disk", outcome(Err(e)))),
                    };
                    (format!("write {path} {len}"), result)
                }
                Step::Read(path) => (
                    format!("read {path}"),
                    fs.open_read(p(path)).map(|mut file| {
                        let mut bytes = Vec::new();
                        match file.read_to_end(&mut bytes) {
                            Ok(n) => format!("{n} bytes"),
                            Err(e) => format!("{} after {} bytes", outcome(Err(e)), bytes.len()),
                        }
                    }),
                ),
                Step::Rename(from, to) => (
                    format!("rename {from} {to}"),
                    fs.rename(p(from), p(to)).map(|()| "ok".into()),
                ),
                Step::Remove(path) => (
                    format!("remove {path}"),
                    fs.remove_file(p(path)).map(|()| "ok".into()),
                ),
                Step::List(path) => (
                    format!("list {path}"),
                    fs.read_dir(p(path)).map(|names| format!("{names:?}")),
                ),
            };
            lines.push(format!("{what}: {}", outcome(result)));
        }
        for i in folder.injected() {
            lines.push(format!(
                "injected by rule {}: {:?} {} {:?}",
                i.rule,
                i.op,
                i.path.display(),
                i.fault
            ));
        }
        lines
    }

    /// The spec and script of the golden trace below.
    fn golden() -> (Spec, Vec<Step>) {
        let spec = Spec::parse(
            r#"{ "rules": [
                { "op": "write",      "path": "**/t/*",  "at": { "offset": 4 },    "fail": "ENOSPC" },
                { "op": "lstat",      "path": "**/a",    "at": { "call": 2 },      "fail": "EIO" },
                { "op": "rename",     "path": "**/t/**", "at": { "from_call": 2 }, "fail": "EXDEV" },
                { "op": "create_dir", "path": "**",      "at": { "call": 3 },      "fail": "EACCES" },
                { "op": "read",       "path": "**/a",    "at": { "offset": 2 },    "fail": "EIO" }
            ] }"#,
        )
        .unwrap();
        let steps = vec![
            Step::Mkdir("t"),
            Step::Write("a", 6),
            Step::Lstat("a"),
            Step::Lstat("a"),
            Step::Write("t/x", 10),
            Step::Rename("a", "t/a"),
            Step::Rename("t/a", "a"),
            Step::Mkdir("d"),
            Step::Mkdir("e"),
            Step::Read("t/a"),
            Step::List("t"),
            Step::Lstat("a"),
            Step::Remove("t/x"),
            Step::List("."),
        ];
        (spec, steps)
    }

    /// The same spec gives the same faults on every run: the trace is
    /// written out, so a run on another day, machine or platform that
    /// injected anything differently would fail here.
    #[test]
    fn the_same_spec_gives_the_same_faults_on_every_run() {
        let (spec, steps) = golden();
        let expected = [
            "mkdir t: ok",
            "write a 6: ok",
            "lstat a: File 6",
            "lstat a: EIO",
            "write t/x 10: ENOSPC, 4 bytes on disk",
            "rename a t/a: ok",
            "rename t/a a: EXDEV",
            "mkdir d: ok",
            "mkdir e: EACCES",
            "read t/a: EIO after 2 bytes",
            r#"list t: ["a", "x"]"#,
            "lstat a: NotFound",
            "remove t/x: ok",
            r#"list .: ["d", "t"]"#,
            "injected by rule 1: Lstat a Eio",
            "injected by rule 0: Write t/x Enospc",
            "injected by rule 2: Rename t/a Exdev",
            "injected by rule 3: CreateDir e Eacces",
            "injected by rule 4: Read t/a Eio",
        ];
        assert_eq!(trace(&spec, &steps), expected);
        assert_eq!(trace(&spec, &steps), expected, "and again");
    }

    fn arb_rule() -> impl Strategy<Value = Rule> {
        let ops = prop::sample::select(vec![
            Op::Lstat,
            Op::ReadDir,
            Op::CreateDir,
            Op::CreateNew,
            Op::Write,
            Op::Sync,
            Op::OpenRead,
            Op::Read,
            Op::Rename,
            Op::RemoveFile,
        ]);
        let paths = prop::sample::select(vec!["**", "**/a", "**/b*", "**/d/*", "**/d/**"]);
        let at = prop_oneof![
            (1..4u64).prop_map(Trigger::Call),
            (1..4u64).prop_map(Trigger::FromCall),
            (0..8u64).prop_map(Trigger::Offset),
        ];
        let fail =
            prop::sample::select(vec![Fault::Enospc, Fault::Eio, Fault::Eacces, Fault::Exdev]);
        (ops, paths, at, fail).prop_map(|(op, path, mut at, mut fail)| {
            // Bend what validation would refuse into what it accepts.
            if let Trigger::Offset(n) = at
                && !matches!(op, Op::Read | Op::Write)
            {
                at = Trigger::Call(n + 1);
            }
            if fail == Fault::Exdev && op != Op::Rename {
                fail = Fault::Eio;
            }
            rule(op, path, at, fail)
        })
    }

    fn arb_step() -> impl Strategy<Value = Step> {
        let name = || prop::sample::select(vec!["a", "b", "bb", "d", "d/a", "d/b"]);
        prop_oneof![
            name().prop_map(Step::Lstat),
            name().prop_map(Step::Mkdir),
            (name(), 0..12usize).prop_map(|(p, len)| Step::Write(p, len)),
            name().prop_map(Step::Read),
            (name(), name()).prop_map(|(from, to)| Step::Rename(from, to)),
            name().prop_map(Step::Remove),
            prop::sample::select(vec![".", "d"]).prop_map(Step::List),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Any spec and any script: two runs over fresh directories
        /// describe the same history, faults included.
        #[test]
        fn any_spec_gives_the_same_faults_on_every_run(
            rules in prop::collection::vec(arb_rule(), 0..6),
            steps in prop::collection::vec(arb_step(), 1..30),
        ) {
            let spec = Spec { rules };
            prop_assert_eq!(trace(&spec, &steps), trace(&spec, &steps));
        }
    }
}
