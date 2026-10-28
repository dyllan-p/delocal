//! `node.json` (DESIGN.md §5, §11): this installation's node ID.
//!
//! The node ID is 16 random bytes, made once and kept for the life of the
//! installation: it keys every version vector this machine writes, so a
//! second one would read to every peer as a different machine, and two
//! machines sharing one would corrupt the vectors. The file is therefore
//! made once, durably, and never rewritten here. A file that cannot be read
//! or does not parse stops the daemon rather than being replaced: making a
//! new ID would be exactly the loss the file exists to prevent.
//!
//! `delocal up` is meant to make the file on first run (§10); until Phase 4
//! brings `up`, the daemon makes it if there is none. The Tailscale binding
//! §11 also lists here is Phase 3's, and arrives as a field this version
//! ignores.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use delocal_engine::{NodeId, Timestamp};
use serde::{Deserialize, Serialize};

/// The file's name in the state directory.
pub const FILE: &str = "node.json";

/// What `node.json` holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub node_id: NodeId,
    /// When the ID was made, in Unix nanoseconds.
    pub created_at: Timestamp,
}

/// Why `node.json` could not be read or made.
#[derive(Debug)]
pub enum NodeError {
    /// It could not be read, or made.
    Io { path: PathBuf, error: io::Error },
    /// It is there but is not a node file.
    Parse {
        path: PathBuf,
        error: serde_json::Error,
    },
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
            Self::Parse { path, error } => write!(
                f,
                "{} is not a node file ({error}); it holds this machine's identity, so it \
                 was left as it is",
                path.display()
            ),
        }
    }
}

impl std::error::Error for NodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            Self::Parse { error, .. } => Some(error),
        }
    }
}

/// The node in `dir`'s `node.json`, made with a fresh random ID at `now`
/// if there is no file yet. The new file is written to a temp name,
/// synced, and renamed into place, and the directory is synced, so a crash
/// leaves either no file or the whole of it.
pub fn load_or_create(dir: &Path, now: Timestamp) -> Result<Node, NodeError> {
    let path = dir.join(FILE);
    match fs::read(&path) {
        Ok(bytes) => {
            return serde_json::from_slice(&bytes)
                .map_err(|error| NodeError::Parse { path, error });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(NodeError::Io { path, error }),
    }
    let node = Node {
        node_id: NodeId::from_bytes(rand::random()),
        created_at: now,
    };
    create(dir, &path, &node).map_err(|error| NodeError::Io { path, error })?;
    Ok(node)
}

fn create(dir: &Path, path: &Path, node: &Node) -> io::Result<()> {
    let temp = dir.join(format!("{FILE}.tmp"));
    let mut bytes = serde_json::to_vec_pretty(node).map_err(io::Error::other)?;
    bytes.push(b'\n');
    // A temp left by a crash before the rename is only ever ours.
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)?;
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_unix_nanos(1_761_000_000_000_000_000)
    }

    #[test]
    fn the_first_start_makes_an_id_and_every_later_one_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let made = load_or_create(dir.path(), now()).unwrap();
        assert_eq!(made.created_at, now());
        let text = fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.contains(&made.node_id.to_string()), "{text}");
        let later = Timestamp::from_unix_nanos(now().as_unix_nanos() + 1);
        assert_eq!(load_or_create(dir.path(), later).unwrap(), made);
        assert!(!dir.path().join("node.json.tmp").exists());
        // Another installation gets another ID.
        let other = tempfile::tempdir().unwrap();
        assert_ne!(
            load_or_create(other.path(), now()).unwrap().node_id,
            made.node_id
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        fs::write(&path, b"{\"node_id\": \"not hex\"}").unwrap();
        assert!(matches!(
            load_or_create(dir.path(), now()),
            Err(NodeError::Parse { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), b"{\"node_id\": \"not hex\"}");
    }

    #[test]
    fn fields_a_later_version_adds_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let id = "00112233445566778899aabbccddeeff";
        fs::write(
            dir.path().join(FILE),
            format!("{{\"node_id\": \"{id}\", \"created_at\": 5, \"tailscale\": {{}}}}"),
        )
        .unwrap();
        let node = load_or_create(dir.path(), now()).unwrap();
        assert_eq!(node.node_id.to_string(), id);
        assert_eq!(node.created_at, Timestamp::from_unix_nanos(5));
    }
}
