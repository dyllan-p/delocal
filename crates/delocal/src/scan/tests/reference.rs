//! A generated tree scans to exactly its reference observations (DESIGN.md
//! §7.3). The tree is made on disk from a model, and what a scan must
//! report (each path's state, the names on disk, the unobservable names,
//! the skips) is worked out here from the model alone, never from the disk
//! or the code under test. That includes which paths the few rules used
//! ignore: each has a hand-written matcher below.

use std::collections::BTreeSet;

use proptest::prelude::*;

use super::*;

/// One entry of a generated tree, by its name on disk.
#[derive(Clone, Debug)]
enum Node {
    File {
        content: Vec<u8>,
        exec: bool,
        age: Age,
    },
    Link {
        target: &'static str,
    },
    Dir {
        children: BTreeMap<Vec<u8>, Node>,
    },
}

/// A file's mtime, against [`NOW`].
#[derive(Clone, Copy, Debug)]
enum Age {
    /// This many seconds before [`OLD`]: long settled.
    Settled(i64),
    /// Half a second ago: not settled.
    Recent,
    /// Half a minute from now: not settled either.
    Ahead,
}

impl Age {
    fn mtime_ns(self) -> i64 {
        match self {
            Self::Settled(secs) => OLD - secs * SECOND,
            Self::Recent => NOW - SECOND / 2,
            Self::Ahead => NOW + 30 * SECOND,
        }
    }

    fn settled(self) -> bool {
        matches!(self, Self::Settled(_))
    }
}

const NFD: &str = "e\u{301}";
const NFC: &str = "é";

/// The names a tree is made of, as bytes on disk: plain ones, ones the
/// defaults or the user's rules may ignore, "é" decomposed and composed,
/// and on Linux a name that is not UTF-8. None differ only by case, which
/// APFS would refuse.
fn names() -> Vec<&'static [u8]> {
    let mut names: Vec<&'static [u8]> = vec![
        b"a",
        b"b.txt",
        b"sub",
        b".DS_Store",
        b"x.swp",
        b"x.log",
        b"keep.log",
        b"build",
        b"top",
        NFD.as_bytes(),
        NFC.as_bytes(),
    ];
    if cfg!(target_os = "linux") {
        names.push(b"\xff");
    }
    names
}

/// The user's rules a tree's `.delocalignore` is drawn from, in order.
const USER_RULES: [&str; 4] = ["*.log", "!keep.log", "build/", "/top"];

/// Records at paths that are never on disk, with their kinds.
const ABSENT: [(&str, Kind); 5] = [
    ("gone", Kind::File),
    ("x.log", Kind::File),
    ("build/x", Kind::File),
    ("sub/deep", Kind::Dir),
    ("top", Kind::Dir),
];

#[derive(Clone, Debug)]
struct Model {
    root: BTreeMap<Vec<u8>, Node>,
    /// Which of [`USER_RULES`] `.delocalignore` holds; none means no file.
    rules: Vec<&'static str>,
    /// Decides which paths have a record, and what kind.
    seed: u64,
    /// Which of [`ABSENT`] have a record.
    absent: Vec<bool>,
}

fn arb_node() -> impl Strategy<Value = Node> {
    let age = prop_oneof![
        6 => (0..100i64).prop_map(Age::Settled),
        1 => Just(Age::Recent),
        1 => Just(Age::Ahead),
    ];
    let file = (
        prop::collection::vec(any::<u8>(), 0..40),
        any::<bool>(),
        age,
    )
        .prop_map(|(content, exec, age)| Node::File { content, exec, age });
    let link = prop::sample::select(vec!["a", "../b.txt", "sub/x", "abc"])
        .prop_map(|target| Node::Link { target });
    let leaf = prop_oneof![4 => file, 1 => link];
    leaf.prop_recursive(3, 32, 5, |inner| {
        prop::collection::btree_map(arb_name(), inner, 0..5)
            .prop_map(|children| Node::Dir { children })
    })
}

fn arb_name() -> impl Strategy<Value = Vec<u8>> {
    prop::sample::select(names()).prop_map(<[u8]>::to_vec)
}

fn arb_model() -> impl Strategy<Value = Model> {
    (
        prop::collection::btree_map(arb_name(), arb_node(), 0..6),
        prop::sample::subsequence(USER_RULES.to_vec(), 0..=USER_RULES.len()),
        any::<u64>(),
        prop::collection::vec(any::<bool>(), ABSENT.len()),
    )
        .prop_map(|(root, rules, seed, absent)| {
            let mut model = Model {
                root,
                rules,
                seed,
                absent,
            };
            model.for_this_platform();
            if !model.rules.is_empty() {
                model.root.insert(
                    ignore_rules::IGNORE_FILE.as_bytes().to_vec(),
                    Node::File {
                        content: model.ignore_file(),
                        exec: false,
                        age: Age::Settled(0),
                    },
                );
            }
            model
        })
}

/// An index path joined from a parent's and a last component.
fn join(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(parent) => format!("{parent}/{name}"),
        None => name.to_string(),
    }
}

/// What a name from [`names`] is to the index: its NFC form, and whether
/// the name on disk differs from it. `None` for a name that is not UTF-8.
fn index_name(name: &[u8]) -> Option<(String, bool)> {
    if name == NFD.as_bytes() {
        return Some((NFC.into(), true));
    }
    std::str::from_utf8(name)
        .ok()
        .map(|s| (s.to_string(), false))
}

/// Whether a directory holds both forms of "é", whose index paths
/// coincide.
fn coinciding(children: &BTreeMap<Vec<u8>, Node>) -> bool {
    children.contains_key(NFD.as_bytes()) && children.contains_key(NFC.as_bytes())
}

/// One rule's verdict on a path, if it matches it: `Some(true)` ignores,
/// `Some(false)` re-includes. By hand, for the rules the model uses.
fn verdict(rule: &str, path: &str, is_dir: bool) -> Option<bool> {
    let base = path.rsplit('/').next().unwrap_or(path);
    let matches = match rule {
        ".DS_Store" => base == ".DS_Store",
        "._*" => base.starts_with("._"),
        "*.swp" => base.ends_with(".swp"),
        "*~" => base.ends_with('~'),
        ".#*" => base.starts_with(".#"),
        ".Trash*" => base.starts_with(".Trash"),
        "*.log" => base.ends_with(".log"),
        "!keep.log" => return (base == "keep.log").then_some(false),
        "build/" => base == "build" && is_dir,
        "/top" => path == "top",
        other => panic!("no reference for the rule {other}"),
    };
    matches.then_some(true)
}

impl Model {
    /// Leave out what this platform's filesystem cannot hold: APFS keeps
    /// one of two names that coincide, and no name that is not UTF-8
    /// (never generated there).
    fn for_this_platform(&mut self) {
        fn prune(children: &mut BTreeMap<Vec<u8>, Node>) {
            if cfg!(target_os = "macos") && coinciding(children) {
                children.remove(NFD.as_bytes());
            }
            for node in children.values_mut() {
                if let Node::Dir { children } = node {
                    prune(children);
                }
            }
        }
        prune(&mut self.root);
    }

    fn ignore_file(&self) -> Vec<u8> {
        self.rules
            .iter()
            .map(|r| format!("{r}\n"))
            .collect::<String>()
            .into_bytes()
    }

    /// Whether the rules alone ignore the entry at `path`.
    fn ignores_entry(&self, path: &str, is_dir: bool) -> bool {
        let rules = ignore_rules::DEFAULTS.iter().chain(&self.rules);
        // The last rule to match decides, as in git.
        rules
            .filter_map(|rule| verdict(rule, path, is_dir))
            .next_back()
            == Some(true)
    }

    /// Whether `path` is ignored, or any directory above it.
    fn ignores(&self, path: &str, is_dir: bool) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        (1..parts.len()).any(|n| self.ignores_entry(&parts[..n].join("/"), true))
            || self.ignores_entry(path, is_dir)
    }

    /// Make the tree on disk.
    fn build(&self, disk: &Disk) {
        fn make(disk: &Disk, dir: &Path, children: &BTreeMap<Vec<u8>, Node>) {
            let fs = disk.folder().open().unwrap();
            for (name, node) in children {
                let path = dir.join(OsStr::from_bytes(name));
                let on_disk = disk.root().join(&path);
                match node {
                    Node::File { content, exec, age } => {
                        std::fs::write(&on_disk, content).unwrap();
                        fs.set_mode(&path, if *exec { 0o755 } else { 0o644 })
                            .unwrap();
                        fs.set_mtime(&path, age.mtime_ns()).unwrap();
                    }
                    Node::Link { target } => {
                        std::os::unix::fs::symlink(target, &on_disk).unwrap();
                    }
                    Node::Dir { children } => {
                        std::fs::create_dir(&on_disk).unwrap();
                        make(disk, &path, children);
                    }
                }
            }
        }
        make(disk, Path::new(""), &self.root);
    }

    /// Every entry with an index path, ignored or not, beneath directories
    /// that have one.
    fn paths(&self) -> Vec<(String, &Node)> {
        fn visit<'m>(
            children: &'m BTreeMap<Vec<u8>, Node>,
            parent: Option<&str>,
            out: &mut Vec<(String, &'m Node)>,
        ) {
            let both = coinciding(children);
            for (name, node) in children {
                let Some((last, _)) = index_name(name) else {
                    continue;
                };
                if both && last == NFC {
                    continue;
                }
                let path = join(parent, &last);
                if let Node::Dir { children } = node {
                    visit(children, Some(&path), out);
                }
                out.push((path, node));
            }
        }
        let mut out = Vec::new();
        visit(&self.root, None, &mut out);
        out
    }

    /// The host's records: for each path on disk, by the seed, none, one
    /// the fast path finds unchanged, or one it does not; and records at
    /// paths that are not on disk at all.
    fn records(&self) -> BTreeMap<RelPath, Entry> {
        let mut records = BTreeMap::new();
        let on_disk: BTreeSet<String> = self.paths().into_iter().map(|(p, _)| p).collect();
        for (path, node) in self.paths() {
            let choice = blake3::hash(&[&self.seed.to_le_bytes()[..], path.as_bytes()].concat())
                .as_bytes()[0]
                % 3;
            let (kind, size, mtime_ns, exec, hash) = match (node, choice) {
                (_, 0) => continue,
                // Unchanged by the fast path, with a hash that proves the
                // file was not read.
                (Node::File { content, exec, age }, 1) => (
                    Kind::File,
                    content.len() as u64,
                    age.mtime_ns(),
                    *exec,
                    blake3_of(b"not what is on disk"),
                ),
                (Node::File { content, exec, age }, _) => (
                    Kind::File,
                    content.len() as u64,
                    age.mtime_ns() - 1,
                    *exec,
                    blake3_of(content),
                ),
                (Node::Link { target }, 1) => (
                    Kind::Symlink,
                    target.len() as u64,
                    0,
                    false,
                    blake3_of(target.as_bytes()),
                ),
                (Node::Link { target }, _) => (
                    Kind::Symlink,
                    target.len() as u64,
                    0,
                    false,
                    blake3_of(b"elsewhere"),
                ),
                (Node::Dir { .. }, 1) => (Kind::Dir, 0, 0, false, ContentHash::EMPTY),
                // A directory where a file was.
                (Node::Dir { .. }, _) => (Kind::File, 1, OLD, false, blake3_of(b"f")),
            };
            records.insert(p(&path), record(&path, kind, size, mtime_ns, exec, hash));
        }
        for ((path, kind), &on) in ABSENT.iter().zip(&self.absent) {
            if on && !on_disk.contains(*path) {
                let record = record(path, *kind, 1, OLD, false, blake3_of(b"gone"));
                records.insert(p(path), record);
            }
        }
        records
    }

    /// What a scan of the tree against `records` must report.
    fn expect(&self, records: &BTreeMap<RelPath, Entry>) -> Expected {
        let mut expected = Expected::default();
        self.expect_dir(&self.root, None, Path::new(""), records, &mut expected);
        for entry in records.values() {
            if self.ignores(entry.path.as_str(), entry.kind == Kind::Dir) {
                expected
                    .reports
                    .push((entry.path.clone(), skip(SkipReason::Ignored)));
            }
        }
        expected
    }

    fn expect_dir(
        &self,
        children: &BTreeMap<Vec<u8>, Node>,
        parent: Option<&str>,
        disk: &Path,
        records: &BTreeMap<RelPath, Entry>,
        out: &mut Expected,
    ) {
        let both = coinciding(children);
        if both {
            let path = p(&join(parent, NFC));
            out.reports
                .push((path.clone(), skip(SkipReason::CoincidingNames)));
            for name in [NFD, NFC] {
                let why = Unobservable::Coincides { path: path.clone() };
                out.unobservable.push((disk.join(name), why));
            }
        }
        for (name, node) in children {
            let place = disk.join(OsStr::from_bytes(name));
            let Some((last, differs)) = index_name(name) else {
                out.unobservable.push((place, Unobservable::NotUtf8));
                continue;
            };
            if both && last == NFC {
                continue;
            }
            let path = join(parent, &last);
            let is_dir = matches!(node, Node::Dir { .. });
            if self.ignores_entry(&path, is_dir) {
                continue;
            }
            if differs {
                out.disk_names.push(DiskName {
                    path: p(&path),
                    name: OsStr::from_bytes(name).to_os_string(),
                });
            }
            let record = records.get(&p(&path));
            let state = match node {
                Node::File { content, exec, age } => {
                    let unchanged = record.is_some_and(|r| {
                        r.kind == Kind::File
                            && r.size == content.len() as u64
                            && r.mtime_ns == age.mtime_ns()
                            && r.exec == *exec
                    });
                    if unchanged {
                        ScanState::Unchanged
                    } else if !age.settled() {
                        skip(SkipReason::Unstable)
                    } else {
                        observed_file(content, age.mtime_ns(), *exec)
                    }
                }
                Node::Link { target } => {
                    let hash = blake3_of(target.as_bytes());
                    if record.is_some_and(|r| r.kind == Kind::Symlink && r.hash == hash) {
                        ScanState::Unchanged
                    } else {
                        observed_link(target)
                    }
                }
                Node::Dir { .. } => {
                    if record.is_some_and(|r| r.kind == Kind::Dir) {
                        ScanState::Unchanged
                    } else {
                        observed_dir()
                    }
                }
            };
            out.reports.push((p(&path), state));
            if let Node::Dir { children } = node {
                self.expect_dir(children, Some(&path), &place, records, out);
            }
        }
    }
}

fn record(
    path: &str,
    kind: Kind,
    size: u64,
    mtime_ns: i64,
    exec: bool,
    hash: ContentHash,
) -> Entry {
    Entry {
        path: p(path),
        kind,
        size,
        mtime_ns,
        stamp: 1,
        exec,
        hash,
        prev_hash: ContentHash::EMPTY,
        version: Version::default(),
        deleted: false,
        modified_by: NodeId::from_bytes([1; 16]),
        author_host: HostName::new("laptop").unwrap(),
    }
}

#[derive(Debug, Default)]
struct Expected {
    reports: Vec<(RelPath, ScanState)>,
    disk_names: Vec<DiskName>,
    unobservable: Vec<(PathBuf, Unobservable)>,
}

/// Anything, in an order that does not depend on the walk's.
fn sorted<T: std::fmt::Debug>(mut items: Vec<T>) -> Vec<T> {
    items.sort_by_key(|item| format!("{item:?}"));
    items
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn a_generated_tree_scans_to_exactly_its_reference_observations(
        model in arb_model(),
        hashers in 1..5usize,
    ) {
        let disk = Disk::new();
        model.build(&disk);
        let records = model.records();
        let (events, report) = scan_at(&disk.folder(), &records, NOW, hashers);
        prop_assert!(finished(&events), "{:?}", report.aborted);
        let expected = model.expect(&records);

        let found = reports(&events);
        let mut skipped: BTreeMap<SkipReason, u64> = BTreeMap::new();
        for (_, state) in &expected.reports {
            if let ScanState::Skipped { reason } = state {
                *skipped.entry(*reason).or_default() += 1;
            }
        }
        prop_assert_eq!(sorted(found), sorted(expected.reports));
        prop_assert_eq!(sorted(report.disk_names), sorted(expected.disk_names));
        prop_assert_eq!(sorted(report.unobservable), sorted(expected.unobservable));
        prop_assert_eq!(report.skipped, skipped);
        prop_assert!(report.invalid_rules.is_empty());

        // The same events, in the same order, on one thread.
        prop_assert_eq!(scan_at(&disk.folder(), &records, NOW, 1).0, events);
    }
}
