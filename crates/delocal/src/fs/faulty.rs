//! [`FaultyFs`]: the fault-injecting wrapper of the filesystem layer
//! (DESIGN.md §14.2), built only with the `faults` feature.
//!
//! It wraps another [`Fs`] and follows a [`Spec`]: scripted rules, each an
//! operation, a path pattern, a trigger and a fault (the [`spec`] module
//! says exactly what each means). A call no rule fires on goes to the
//! wrapped filesystem unchanged, so with no rules `FaultyFs` passes the same
//! conformance checks as what it wraps.
//!
//! **Deterministic.** Rules hold no randomness and no clock. Each call rule
//! counts the calls that match its operation and pattern, whether or not it
//! fires, and an offset rule looks only at the file's position. So the same
//! spec and the same sequence of calls give the same faults on every run,
//! and [`FaultyFs::injected`] lists them in the order they happened.
//!
//! When several rules fire on one call, of either kind, the first in the
//! spec decides the fault. A call no rule fires on but that would cross an
//! offset rule's byte moves only the bytes before the nearest such byte.

use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{Fs, ReadFile, Stat, WriteFile};

pub mod pattern;
pub mod spec;

use pattern::Pattern;
pub use spec::{Fault, Op, Rule, Spec, SpecError, Trigger};

/// One fault a [`FaultyFs`] injected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Injected {
    /// The rule that fired: its index in the spec, counting from 0.
    pub rule: usize,
    pub op: Op,
    /// The call's path, relative to the folder root: a rename's source, a
    /// symlink's link, the path an open file was opened with.
    pub path: PathBuf,
    pub fault: Fault,
}

/// A filesystem that injects faults into another. See the module docs.
pub struct FaultyFs<F> {
    inner: F,
    rules: Arc<Rules>,
}

/// The rules and their counters, shared with every open file so that reads,
/// writes and syncs are counted with everything else.
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

impl<F: Fs> FaultyFs<F> {
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

    /// Every fault injected so far, in order.
    pub fn injected(&self) -> Vec<Injected> {
        self.rules.state().injected.clone()
    }

    /// Count a call without a position, and fail it if a rule fires.
    fn check(&self, op: Op, paths: &[&Path]) -> io::Result<()> {
        match self.rules.check(op, paths, None) {
            Verdict::Fail(fault) => Err(fault.error()),
            Verdict::Proceed | Verdict::AtMost(_) => Ok(()),
        }
    }
}

impl Rules {
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

impl<F: Fs> Fs for FaultyFs<F> {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        self.check(Op::ReadDir, &[dir])?;
        self.inner.read_dir(dir)
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

/// A file from [`FaultyFs::open_read`].
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
    use crate::fs::RealFs;

    crate::fs::conformance::conformance_tests!(|root: &Path| {
        FaultyFs::new(RealFs::open(root).unwrap(), Spec::default()).unwrap()
    });

    fn rule(op: Op, path: &str, at: Trigger, fail: Fault) -> Rule {
        Rule {
            op,
            path: path.into(),
            at,
            fail,
        }
    }

    /// A `FaultyFs` with `rules` over the folder at `root`.
    fn faulty(root: &Path, rules: Vec<Rule>) -> FaultyFs<RealFs> {
        FaultyFs::new(RealFs::open(root).unwrap(), Spec { rules }).unwrap()
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
        let fs = FaultyFs::new(RealFs::open(root).unwrap(), spec.clone()).unwrap();
        let outcome = |result: io::Result<String>| match result {
            Ok(ok) => ok,
            Err(e) => match e.raw_os_error() {
                Some(5) => "EIO".into(),
                Some(13) => "EACCES".into(),
                Some(18) => "EXDEV".into(),
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
        for i in fs.injected() {
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
