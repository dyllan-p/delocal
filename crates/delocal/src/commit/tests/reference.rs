//! The reference table: the simulator's host commit
//! (`crates/sim/src/sim.rs`, `Sim::commit` and its helpers), ported to a
//! model of a disk, and a property test that the real commit path leaves
//! every random disk state, under every random action, exactly as the model
//! says, with the same report.
//!
//! The simulator's host is the one every engine rule was checked against
//! (§14.1), so where the real host could do something else, this is where
//! it would show. The port keeps the simulator's order of checks and its
//! outcomes, with the simulator's knobs at their defaults (a displaced
//! directory takes its children) and no failure drawn. It differs from the
//! simulator in two places, each a fact about real disks the simulator does
//! not model:
//!
//! - **What a failed report carries.** The simulator always reports the
//!   whole observation; the real host reports what a scan would, which is
//!   `Unchanged` where the live record's fast path says so. The engine
//!   reads the two the same way.
//! - **Case.** The simulator folds whole paths in ASCII; a real filesystem
//!   that folds case folds each name. The paths drawn here differ in case
//!   only in their last component, where the two agree, and the model
//!   folds only when the disk the test runs on does (APFS in CI's macOS
//!   job, nothing on ext4).

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;

use proptest::prelude::*;

use super::*;

/// A file in the model, as the simulator's `File`: kind, content (a
/// symlink's target), mtime and exec bit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MFile {
    pub kind: Kind,
    pub content: Vec<u8>,
    pub mtime_ns: i64,
    pub exec: bool,
}

impl MFile {
    fn hash(&self) -> ContentHash {
        match self.kind {
            Kind::Dir => ContentHash::EMPTY,
            Kind::File | Kind::Symlink => blake3_of(&self.content),
        }
    }

    /// `File::observed` in the simulator.
    pub fn observed(&self) -> Observed {
        Observed {
            kind: self.kind,
            size: self.content.len() as u64,
            mtime_ns: if self.kind == Kind::File {
                self.mtime_ns
            } else {
                0
            },
            exec: self.exec && self.kind == Kind::File,
            hash: self.hash(),
        }
    }

    fn dir() -> Self {
        Self {
            kind: Kind::Dir,
            content: Vec::new(),
            mtime_ns: 0,
            exec: false,
        }
    }
}

/// A disk in the model: what is at each path, and the trash's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub fs: BTreeMap<RelPath, MFile>,
    /// Every file and symlink moved to the trash, as (kind, content).
    pub trash: Vec<(Kind, Vec<u8>)>,
    pub folds_case: bool,
}

/// What the model's commit did: the outcome, and the directories it made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Modelled {
    pub outcome: ApplyOutcome,
    pub created: Vec<RelPath>,
}

impl Model {
    fn fold(path: &RelPath) -> String {
        path.as_str().to_ascii_lowercase()
    }

    fn occupied_otherwise(&self, path: &RelPath) -> bool {
        self.folds_case
            && self
                .fs
                .keys()
                .any(|p| p != path && Self::fold(p) == Self::fold(path))
    }

    fn occupied(&self, path: &RelPath) -> bool {
        self.fs.contains_key(path) || self.occupied_otherwise(path)
    }

    fn has_children(&self, path: &RelPath) -> bool {
        self.fs.keys().any(|p| path.is_ancestor_of(p))
    }

    /// `collides` in the simulator: a live record spelt otherwise.
    fn collides(&self, path: &RelPath, records: &BTreeMap<RelPath, Entry>) -> Option<RelPath> {
        if !self.folds_case {
            return None;
        }
        records
            .values()
            .find(|r| !r.deleted && r.path != *path && Self::fold(&r.path) == Self::fold(path))
            .map(|r| r.path.clone())
    }

    /// `found` in the simulator, with the real host's `Unchanged`.
    fn found(&self, path: &RelPath, records: &BTreeMap<RelPath, Entry>) -> ScanState {
        match self.fs.get(path) {
            None => ScanState::Absent,
            Some(file) => {
                let seen = file.observed();
                let unchanged = records
                    .get(path)
                    .is_some_and(|r| !r.deleted && r.unchanged_by_stat(&seen));
                if unchanged {
                    ScanState::Unchanged
                } else {
                    ScanState::Observed(seen)
                }
            }
        }
    }

    fn trash_file(&mut self, file: MFile) {
        if file.kind != Kind::Dir {
            self.trash.push((file.kind, file.content));
        }
    }

    /// `displace_path` in the simulator, with subtrees on.
    fn displace(&mut self, path: &RelPath, to: &Displace) {
        let moving: Vec<RelPath> = self
            .fs
            .keys()
            .filter(|p| *p == path || path.is_ancestor_of(p))
            .cloned()
            .collect();
        for from in moving {
            let Some(file) = self.fs.remove(&from) else {
                continue;
            };
            match to {
                Displace::Trash => self.trash_file(file),
                Displace::ConflictCopy(target) => {
                    let rest = &from.as_str()[path.as_str().len()..];
                    let at = RelPath::new(format!("{target}{rest}")).unwrap();
                    self.fs.insert(at, file);
                }
            }
        }
    }

    /// `Sim::commit`, for the four commit actions, with no failure drawn.
    /// `content` is what the write's temp file holds.
    pub fn commit(
        &mut self,
        action: &Action,
        content: &[u8],
        records: &BTreeMap<RelPath, Entry>,
    ) -> Modelled {
        let path = match action {
            Action::Write { path, .. }
            | Action::Remove { path, .. }
            | Action::SetMeta { path, .. }
            | Action::MoveToTrash { path, .. } => path.clone(),
            _ => unreachable!(),
        };
        let report = |m: &Self, outcome: fn(ScanState) -> ApplyOutcome| Modelled {
            outcome: outcome(m.found(&path, records)),
            created: Vec::new(),
        };
        let changed = |m: &Self| report(m, |found| ApplyOutcome::ChangedUnderneath { found });
        let ok = Modelled {
            outcome: ApplyOutcome::Ok,
            created: Vec::new(),
        };
        if let Action::Write { .. } = action
            && let Some(with) = self.collides(&path, records)
        {
            return Modelled {
                outcome: ApplyOutcome::Failed {
                    error: LocalError::CaseCollision { with },
                    found: self.found(&path, records),
                },
                created: Vec::new(),
            };
        }
        if let Action::MoveToTrash { expected, .. } = action {
            if !self.fs.contains_key(&path) {
                return ok;
            }
            if !matches(self.fs.get(&path), Some(expected)) {
                return changed(self);
            }
            if self.has_children(&path) {
                return changed(self);
            }
            self.displace(&path, &Displace::Trash);
            return ok;
        }
        let expected = match action {
            Action::Write { expected, .. }
            | Action::Remove { expected, .. }
            | Action::SetMeta { expected, .. } => expected.as_ref(),
            _ => None,
        };
        if !matches(self.fs.get(&path), expected) {
            return changed(self);
        }
        let mut created = Vec::new();
        if let Action::Write { .. } = action {
            let mut ancestor = path.parent();
            while let Some(dir) = ancestor {
                match self.fs.get(&dir) {
                    Some(f) if f.kind == Kind::Dir => break,
                    Some(_) => return changed(self),
                    None => created.push(dir.clone()),
                }
                ancestor = dir.parent();
            }
            created.reverse();
        }
        match action {
            Action::Write {
                entry, displace, ..
            } => {
                if self.occupied_otherwise(&path)
                    || matches!(displace, Displace::ConflictCopy(t) if self.occupied(t))
                {
                    return changed(self);
                }
                for dir in &created {
                    self.fs.insert(dir.clone(), MFile::dir());
                }
                if self.fs.contains_key(&path) {
                    self.displace(&path, displace);
                }
                self.fs.insert(
                    path.clone(),
                    MFile {
                        kind: entry.kind,
                        content: match entry.kind {
                            Kind::Dir => Vec::new(),
                            _ => content.to_vec(),
                        },
                        mtime_ns: if entry.kind == Kind::File {
                            entry.mtime_ns
                        } else {
                            0
                        },
                        exec: entry.exec && entry.kind == Kind::File,
                    },
                );
                Modelled {
                    outcome: ApplyOutcome::Ok,
                    created,
                }
            }
            Action::Remove { displace, .. } => {
                if !self.fs.contains_key(&path) {
                    return ok;
                }
                let renamed = matches!(displace, Displace::ConflictCopy(_));
                if !renamed && self.has_children(&path) {
                    return changed(self);
                }
                if let Displace::ConflictCopy(target) = displace
                    && self.occupied(target)
                {
                    return changed(self);
                }
                self.displace(&path, displace);
                ok
            }
            Action::SetMeta { mtime_ns, exec, .. } => match self.fs.get_mut(&path) {
                Some(file) if file.kind == Kind::File => {
                    file.mtime_ns = *mtime_ns;
                    file.exec = *exec;
                    ok
                }
                _ => changed(self),
            },
            _ => unreachable!(),
        }
    }
}

/// `expected_matches` in the simulator.
fn matches(file: Option<&MFile>, expected: Option<&Observed>) -> bool {
    match (file, expected) {
        (None, None) => true,
        (Some(f), Some(e)) => e.unchanged_by_stat(&f.observed()),
        _ => false,
    }
}

/// The paths drawn: case differs only in last components that are never
/// parents, and each name has a sibling for its conflict copy.
const PATHS: [&str; 12] = [
    "a", "A", "a.c", "b", "b.c", "d", "d/x", "d/X", "d/x.c", "d/y", "e/z", "e/z.c",
];

fn arb_file() -> impl Strategy<Value = MFile> {
    let content = proptest::collection::vec(b'a'..=b'e', 0..6);
    prop_oneof![
        3 => (content.clone(), 0i64..4, any::<bool>()).prop_map(|(content, m, exec)| MFile {
            kind: Kind::File,
            content,
            mtime_ns: OLD + m,
            exec,
        }),
        1 => Just(MFile::dir()),
        1 => proptest::collection::vec(b'a'..=b'e', 1..6).prop_map(|content| MFile {
            kind: Kind::Symlink,
            content,
            mtime_ns: 0,
            exec: false,
        }),
    ]
}

/// A disk state: some of the paths, each a file, a directory or a symlink,
/// made consistent (a parent of something is a directory) and, on a disk
/// that folds case, holding one spelling of each name.
fn arb_state(folds_case: bool) -> impl Strategy<Value = BTreeMap<RelPath, MFile>> {
    proptest::collection::vec((0..PATHS.len(), arb_file()), 0..8).prop_map(move |drawn| {
        let mut fs: BTreeMap<RelPath, MFile> = BTreeMap::new();
        for (i, file) in drawn {
            let path = p(PATHS[i]);
            let clash = fs.keys().any(|q| {
                folds_case && q != &path && q.as_str().eq_ignore_ascii_case(path.as_str())
            });
            if !clash {
                fs.insert(path, file);
            }
        }
        // Parents of anything are directories.
        let parents: Vec<RelPath> = fs.keys().filter_map(RelPath::parent).collect();
        for parent in parents {
            fs.insert(parent, MFile::dir());
        }
        fs
    })
}

/// An observation the engine might expect at a path: what is there, or
/// something else, or nothing.
fn arb_expected(truth: Option<Observed>) -> impl Strategy<Value = Option<Observed>> {
    let other = arb_file().prop_map(|f| Some(f.observed()));
    prop_oneof![
        3 => Just(truth),
        1 => Just(None),
        1 => other,
    ]
}

/// One of the four commit actions at a drawn path, with what its temp file
/// holds.
fn arb_action(state: BTreeMap<RelPath, MFile>) -> impl Strategy<Value = (Action, Vec<u8>)> {
    // Mostly a path that holds something, so every way a commit can end
    // is drawn often.
    let held: Vec<&'static str> = PATHS
        .into_iter()
        .filter(|path| state.contains_key(&p(path)))
        .collect();
    let path = if held.is_empty() {
        proptest::sample::select(PATHS.to_vec()).boxed()
    } else {
        prop_oneof![
            2 => proptest::sample::select(held),
            1 => proptest::sample::select(PATHS.to_vec()),
        ]
        .boxed()
    };
    (path, 0..4u8, arb_file(), any::<bool>(), any::<u8>()).prop_flat_map(
        move |(path, which, incoming, conflict, seed)| {
            let truth = state.get(&p(path)).map(MFile::observed);
            let copy = format!("{path}.c");
            let displace = if conflict && PATHS.contains(&copy.as_str()) {
                Displace::ConflictCopy(p(&copy))
            } else {
                Displace::Trash
            };
            arb_expected(truth.clone()).prop_map(move |expected| {
                let action = match which {
                    0 | 1 => {
                        let mut e = entry(
                            path,
                            incoming.kind,
                            &incoming.content,
                            incoming.mtime_ns,
                            incoming.exec,
                        );
                        if incoming.kind == Kind::Symlink {
                            e.hash = hash_target(&incoming.content);
                        }
                        write(path, e, expected.clone(), displace.clone())
                    }
                    2 => remove(path, expected.clone(), displace.clone()),
                    _ => match (seed % 2, &truth) {
                        (0, Some(t)) => move_to_trash(path, t.clone()),
                        (_, _) => set_meta(
                            path,
                            expected.clone(),
                            OLD + i64::from(seed % 5),
                            seed % 3 == 0,
                        ),
                    },
                };
                (action, incoming.content.clone())
            })
        },
    )
}

/// Make `state` on disk.
fn materialise(disk: &Disk, state: &BTreeMap<RelPath, MFile>) {
    // Parents sort before their children.
    for (path, file) in state {
        match file.kind {
            Kind::Dir => disk.dir(path.as_str()),
            Kind::File => {
                disk.file(path.as_str(), &file.content, file.mtime_ns, file.exec);
            }
            Kind::Symlink => {
                std::os::unix::fs::symlink(
                    std::ffi::OsStr::from_bytes(&file.content),
                    disk.at(path.as_str()),
                )
                .unwrap();
            }
        }
    }
}

/// What is on disk, outside `.delocal`, as the model holds it.
fn on_disk(disk: &Disk) -> BTreeMap<RelPath, MFile> {
    fn walk(root: &Path, rel: Option<RelPath>, out: &mut BTreeMap<RelPath, MFile>) {
        let dir = rel
            .as_ref()
            .map_or(root.to_path_buf(), |r| root.join(r.as_str()));
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if rel.is_none() && name == RESERVED_DIR {
                continue;
            }
            let path = match &rel {
                Some(r) => r.join(&name).unwrap(),
                None => p(&name),
            };
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            let file = if meta.is_dir() {
                walk(root, Some(path.clone()), out);
                MFile::dir()
            } else if meta.file_type().is_symlink() {
                MFile {
                    kind: Kind::Symlink,
                    content: std::fs::read_link(entry.path())
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes(),
                    mtime_ns: 0,
                    exec: false,
                }
            } else {
                MFile {
                    kind: Kind::File,
                    content: std::fs::read(entry.path()).unwrap(),
                    mtime_ns: meta.mtime() * SECOND + meta.mtime_nsec(),
                    exec: meta.mode() & 0o100 != 0,
                }
            };
            out.insert(path, file);
        }
    }
    let mut out = BTreeMap::new();
    walk(disk.root(), None, &mut out);
    out
}

/// Every file and symlink in the trash, as (kind, content), sorted.
fn in_trash(disk: &Disk) -> Vec<(Kind, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(Kind, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(&entry.path(), out);
            } else if kind.is_symlink() {
                let target = std::fs::read_link(entry.path()).unwrap();
                out.push((Kind::Symlink, target.into_os_string().into_encoded_bytes()));
            } else {
                out.push((Kind::File, std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(&disk.at(trash::DIR), &mut out);
    out.sort();
    out
}

/// The probe's answer for the disk the tests run on.
fn folds_case() -> bool {
    let disk = Disk::new();
    let fs = disk.folder().open().unwrap();
    Sensitivity::probe(&*fs).unwrap().folds_case
}

/// One drawn case: a disk state, the paths the engine holds records for,
/// and an action with what its temp file holds.
type Case = (BTreeMap<RelPath, MFile>, Vec<RelPath>, (Action, Vec<u8>));

fn arb_case() -> impl Strategy<Value = Case> {
    let folds = folds_case();
    arb_state(folds).prop_flat_map(|state| {
        // Records: any of what is on disk, as the engine last saw it.
        let paths: Vec<RelPath> = state.keys().cloned().collect();
        let tracked = proptest::sample::subsequence(paths.clone(), 0..=paths.len());
        let extra = proptest::sample::subsequence(PATHS.map(p).to_vec(), 0..3);
        (
            Just(state.clone()),
            (tracked, extra).prop_map(|(mut t, e)| {
                t.extend(e);
                t
            }),
            arb_action(state),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn the_real_commit_matches_the_simulators_host(
        (state, tracked, (action, content)) in arb_case(),
    ) {
        let mut disk = Disk::new();
        let folds = folds_case();
        materialise(&disk, &state);
        // A record for each tracked path: what is there, or a file for a
        // path with nothing (a record the engine holds for a path the user
        // has since changed).
        for path in tracked {
            let file = state.get(&path).cloned().unwrap_or(MFile {
                kind: Kind::File,
                content: b"recorded".to_vec(),
                mtime_ns: OLD,
                exec: false,
            });
            let mut record = entry(path.as_str(), file.kind, &file.content, file.mtime_ns, file.exec);
            record.hash = file.hash();
            record.size = file.content.len() as u64;
            disk.records.insert(path, record);
        }
        prop_assume!(on_disk(&disk) == state, "the disk could not hold the state");

        let mut model = Model { fs: state.clone(), trash: Vec::new(), folds_case: folds };
        let expected = model.commit(&action, &content, &disk.records);
        let temp = disk.temp(&content);
        let done = disk.commit(&action, Some(&temp));

        prop_assert_eq!(outcome(&done), &expected.outcome, "{:?} on {:?}", action, state);
        prop_assert_eq!(&done.created, &expected.created);
        prop_assert_eq!(on_disk(&disk), model.fs);
        let mut trashed = model.trash;
        trashed.sort();
        prop_assert_eq!(in_trash(&disk), trashed);
    }
}
