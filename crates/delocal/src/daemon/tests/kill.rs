//! `kill -9` (DESIGN.md §11 "persistence contract", §13): one daemon with
//! random edits survives fifty kills, and after each restart its store
//! holds exactly the engine's parts and a full scan once idle changes
//! nothing.
//!
//! The test binary runs the daemon in a child process, [`child`], which
//! starts it on the folder (restoring from whatever the last child left),
//! waits until it is idle, checks, prints `checked <round>`, and then edits
//! the folder at random and feeds the engine batches from a peer that never
//! connects: directories made and files replaced by directories (commits
//! with a displacement, so the journal), tombstones (trash moves), touches
//! (metadata-only applies) and files no one can serve (wants that wait).
//! The parent kills it with `SIGKILL` at a random moment and starts the
//! next round on the same state directory and folder.
//!
//! The two checks are the persistence contract seen from outside. The
//! store's parts equal the engine's: every hook was written, and nothing a
//! crash took is in one but not the other. And a scan of a folder nothing
//! is changing makes no index change: the index the restart rebuilt
//! describes the disk, so no write or commit was lost or half-recorded.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use delocal_engine::{Entry, FolderParts, IndexRecord, Kind, RelPath, Timestamp};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use super::super::clock::Clock;
use super::super::{Daemon, NoTransport, Setup, lock};
use super::{Fixture, Ids, batch, from_peer, tombstone, touch};
use crate::fs::RealFolder;
use crate::store::Store;

const STATE: &str = "DELOCAL_KILL_STATE";
const ROOT: &str = "DELOCAL_KILL_ROOT";
const SEED: &str = "DELOCAL_KILL_SEED";
const ROUND: &str = "DELOCAL_KILL_ROUND";

/// How many times the daemon is killed.
const KILLS: u64 = 50;

/// The seed for the kill times and every child's edits.
const SEED_VALUE: u64 = 0x6b69_6c6c;

/// The directories the edits use, beside the root and the peer's.
const DIRS: [&str; 5] = ["", "a", "a/b", "c", "c/d"];

/// The first difference between two folders' parts, by part.
fn difference(stored: &[FolderParts], engine: &[FolderParts]) -> Option<String> {
    if stored.len() != engine.len() {
        return Some(format!(
            "{} folders stored, {} in the engine",
            stored.len(),
            engine.len()
        ));
    }
    for (s, e) in stored.iter().zip(engine) {
        let parts: [(&str, bool); 7] = [
            (
                "id, rules, members",
                s.id == e.id && s.rules == e.rules && s.members == e.members,
            ),
            ("records", s.records == e.records),
            ("wants", s.wants == e.wants),
            ("pending", s.pending == e.pending),
            ("held", s.held == e.held),
            ("deferred", s.deferred == e.deferred),
            ("rest", s.rest == e.rest),
        ];
        if let Some((name, _)) = parts.iter().find(|(_, same)| !same) {
            let detail = match *name {
                "records" => {
                    let keys = |p: &FolderParts| -> Vec<String> {
                        p.records.keys().map(ToString::to_string).collect()
                    };
                    let diff: Vec<String> = s
                        .records
                        .iter()
                        .filter(|(k, v)| e.records.get(*k) != Some(*v))
                        .map(|(k, _)| k.to_string())
                        .chain(
                            e.records
                                .keys()
                                .filter(|k| !s.records.contains_key(*k))
                                .map(ToString::to_string),
                        )
                        .collect();
                    format!(
                        "differ at {diff:?} ({} vs {})",
                        keys(s).len(),
                        keys(e).len()
                    )
                }
                "wants" => format!("{:?} vs {:?}", s.wants, e.wants),
                "rest" => format!("{:?} vs {:?}", s.rest, e.rest),
                _ => String::new(),
            };
            return Some(format!("{name} differ: {detail}"));
        }
    }
    None
}

/// Run by [`one_daemon_with_random_edits_survives_fifty_kill_9s`] as its
/// child, once per round, and killed by it.
#[test]
#[ignore = "run by one_daemon_with_random_edits_survives_fifty_kill_9s as its child"]
fn child() {
    let (Ok(state), Ok(root), Ok(seed), Ok(round)) = (
        std::env::var(STATE),
        std::env::var(ROOT),
        std::env::var(SEED),
        std::env::var(ROUND),
    ) else {
        return;
    };
    let (state, root) = (PathBuf::from(state), PathBuf::from(root));
    let seed: u64 = seed.parse().unwrap();
    let round: u64 = round.parse().unwrap();
    let out = |line: String| {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{line}").unwrap();
        stdout.flush().unwrap();
    };
    let _logs = super::super::logs::init(&state);
    let ids = Fixture::ids();
    let root_folder = root.clone();
    let setup = Setup {
        state_dir: state.clone(),
        lock: lock::lock(&state).unwrap(),
        node: ids.node.clone(),
        clock: std::sync::Arc::new(Clock::start()),
        realtime: std::sync::Arc::new(crate::scan::stability::realtime as fn() -> Timestamp),
        folder: Box::new(move |_| std::sync::Arc::new(RealFolder::new(&root_folder))),
        outbox: Box::new(NoTransport),
        rescan: super::super::RESCAN,
        space_check: super::super::SPACE_CHECK,
        hashers: std::num::NonZeroUsize::new(2).unwrap(),
    };
    let daemon = Daemon::start(setup).unwrap();
    if let Err(e) = daemon.settled(Duration::from_secs(60)) {
        out(format!("failed: round {round}: {e}"));
        return;
    }
    let (stored, engine) = daemon.parts();
    if let Some(difference) = difference(&stored, &engine) {
        out(format!(
            "failed: round {round}: the store's parts and the engine's: {difference}"
        ));
        return;
    }
    let changes = daemon.scan(ids.folder);
    if changes != 0 {
        out(format!(
            "failed: round {round}: a scan once idle made {changes} index changes"
        ));
        return;
    }
    if let Err(e) = daemon.settled(Duration::from_secs(60)) {
        out(format!("failed: round {round}: after the check: {e}"));
        return;
    }
    out(format!("checked {round}"));

    let mut driver = Driver {
        root,
        rng: StdRng::seed_from_u64(seed ^ round.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
        round,
        made: 0,
    };
    loop {
        driver.step(&daemon, &ids);
    }
}

/// The child's random edits and the peer's batches.
struct Driver {
    root: PathBuf,
    rng: StdRng,
    round: u64,
    made: u64,
}

impl Driver {
    fn pick(&mut self) -> PathBuf {
        let dir = DIRS[self.rng.random_range(0..DIRS.len())];
        let name = format!("f{}", self.rng.random_range(0..12));
        Path::new(dir).join(name)
    }

    fn step(&mut self, daemon: &Daemon, ids: &Ids) {
        let path = self.pick();
        let at = self.root.join(&path);
        let roll = self.rng.random_range(0..100);
        // Failures are the folder being as it is: a file that is not there,
        // a directory in the way. Nothing here needs to succeed.
        let _ = match roll {
            0..30 => std::fs::create_dir_all(at.parent().unwrap_or(&self.root)).and_then(|()| {
                let len = self.rng.random_range(0..3000);
                let bytes: Vec<u8> = (0..len).map(|_| self.rng.random()).collect();
                std::fs::write(&at, bytes)
            }),
            30..40 => std::fs::remove_file(&at),
            40..47 => std::fs::metadata(&at).and_then(|meta| {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() ^ 0o111;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode))
            }),
            47..55 => {
                let dir = DIRS[self.rng.random_range(1..DIRS.len())];
                std::fs::create_dir_all(self.root.join(dir))
            }
            55..60 => {
                let dir = DIRS[self.rng.random_range(1..DIRS.len())];
                std::fs::remove_dir(self.root.join(dir))
            }
            60..66 => {
                std::os::unix::fs::symlink(format!("target{}", self.rng.random_range(0..3)), &at)
            }
            66..80 => {
                self.peer(daemon, ids);
                Ok(())
            }
            _ => Ok(()),
        };
        std::thread::sleep(Duration::from_millis(self.rng.random_range(5..40)));
    }

    /// A batch from the peer, built on the engine's records as they stand:
    /// a few entries, so the brake never holds it.
    fn peer(&mut self, daemon: &Daemon, ids: &Ids) {
        let folder = ids.folder;
        let peer = ids.peer;
        let (records, seq): (Vec<IndexRecord>, u64) = daemon.engine(move |engine| {
            let Some(f) = engine.folder(folder) else {
                return (Vec::new(), 0);
            };
            (
                f.index().live_records().cloned().collect(),
                f.index().peer_seq(peer),
            )
        });
        let files: Vec<&IndexRecord> = records
            .iter()
            .filter(|r| r.entry.kind == Kind::File && !r.entry.path.as_str().starts_with("peer"))
            .collect();
        let mut entries: Vec<Entry> = Vec::new();
        let mut taken: Vec<RelPath> = Vec::new();
        let peer_dir = records.iter().find(|r| r.entry.path.as_str() == "peer");
        if peer_dir.is_none() {
            entries.push(from_peer(peer, "peer", None));
        }
        for _ in 0..self.rng.random_range(1..4) {
            self.made += 1;
            let name = format!("peer/r{}-{}", self.round, self.made);
            let pick = (!files.is_empty()).then(|| files[self.rng.random_range(0..files.len())]);
            let entry = match (self.rng.random_range(0..5), pick) {
                (0, _) | (_, None) => from_peer(peer, &name, None),
                (1, Some(file)) => tombstone(peer, file, Timestamp::from_unix_nanos(1)),
                (2, Some(file)) => touch(peer, file),
                // A directory where the file is: a commit that displaces.
                (3, Some(file)) => from_peer(peer, file.entry.path.as_str(), Some(file)),
                // Content no one can serve: a want that waits.
                _ => Entry {
                    kind: Kind::File,
                    size: 10,
                    mtime_ns: 1_700_000_000_000_000_000,
                    hash: delocal_engine::ContentHash::from_bytes(rand::random()),
                    ..from_peer(peer, &name, None)
                },
            };
            if taken.contains(&entry.path) {
                continue;
            }
            taken.push(entry.path.clone());
            entries.push(entry);
        }
        daemon.inject(delocal_engine::Event::BatchReceived {
            from: peer,
            batch: batch(peer, folder, seq, entries),
        });
    }
}

/// §11, §13: fifty `kill -9`s of a daemon at work, and after each restart
/// the store's parts are the engine's and a scan once idle changes nothing.
#[test]
fn one_daemon_with_random_edits_survives_fifty_kill_9s() {
    let fixture = Fixture::new();
    for dir in DIRS {
        std::fs::create_dir_all(fixture.at(dir)).unwrap();
    }
    for n in 0..8 {
        std::fs::write(fixture.at(&format!("a/f{n}")), format!("file {n}")).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let mut rng = StdRng::seed_from_u64(SEED_VALUE);
    let mut kills = 0;
    for round in 0..=KILLS {
        let stderr =
            std::fs::File::create(fixture.dir.path().join(format!("child-{round}.err"))).unwrap();
        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "daemon::tests::kill::child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(STATE, fixture.state())
            .env(ROOT, fixture.root())
            .env(SEED, SEED_VALUE.to_string())
            .env(ROUND, round.to_string())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (lines, said) = mpsc::channel();
        std::thread::spawn(move || {
            // libtest prints `test ... ` and no newline before the test's
            // own output, so a line can start with that.
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(at) = line.find("checked ").or_else(|| line.find("failed: ")) {
                    let _ = lines.send(line[at..].to_owned());
                }
            }
        });
        let line = said.recv_timeout(Duration::from_secs(120));
        let checked = format!("checked {round}");
        if line.as_deref() != Ok(checked.as_str()) {
            let _ = child.kill();
            let _ = child.wait();
            let logs = std::fs::read_dir(fixture.state().join("logs"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .collect::<Vec<_>>()
                .join("");
            let tail: String = logs.lines().rev().take(40).collect::<Vec<_>>().join("\n");
            let err =
                std::fs::read_to_string(fixture.dir.path().join(format!("child-{round}.err")))
                    .unwrap_or_default();
            panic!(
                "round {round} after {kills} kills: {line:?}\nchild's stderr:\n{err}\n\
                 last log lines, newest first:\n{tail}"
            );
        }
        if round == KILLS {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(rng.random_range(50..1000)));
        // `Child::kill` is SIGKILL: `kill -9`.
        child.kill().unwrap();
        child.wait().unwrap();
        kills += 1;
    }
    assert_eq!(kills, KILLS);

    // The rounds did what they are for: the peer's batches were received
    // and committed, files went to the trash, and the journal had rows to
    // resolve at some start.
    let store = Store::open(&fixture.state().join(super::super::STORE)).unwrap();
    let received = store
        .history(fixture.folder)
        .unwrap()
        .iter()
        .filter(|row| row.role == delocal_engine::BatchRole::Received)
        .count();
    let trashed = store.trash(fixture.folder).unwrap().len();
    let parts = store.parts(fixture.folder).unwrap().unwrap();
    let peer_dirs = parts
        .records
        .values()
        .filter(|r| r.entry.path.as_str().starts_with("peer/") && r.entry.kind == Kind::Dir)
        .count();
    let logs = std::fs::read_dir(fixture.state().join("logs"))
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect::<String>();
    let resolved = logs
        .lines()
        .filter(|l| l.contains("journal resolved"))
        .count();
    eprintln!(
        "{received} batches received, {trashed} in the trash, {peer_dirs} of the peer's \
         directories, the journal resolved at {resolved} starts"
    );
    assert!(received >= 20, "{received} batches received");
    assert!(trashed >= 5, "{trashed} in the trash");
    assert!(peer_dirs >= 5, "{peer_dirs} directories from the peer");
}
