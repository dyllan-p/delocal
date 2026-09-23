//! Index entries (DESIGN.md §7.1) and content equality (§7.6).
//!
//! [`Entry`] is the §7.1 record without `seq`: what travels in a batch
//! (§7.4) and what a scanner observation becomes once the engine has
//! versioned it. [`struct@Hash`] is opaque here: the host computes BLAKE3, the
//! engine only compares.

use serde::{Deserialize, Serialize};

use crate::id::{HostName, NodeId, bytes_newtype};
use crate::path::RelPath;
use crate::version::Version;

bytes_newtype! {
    /// A BLAKE3 hash (§7.1): of the content for files, of the target string
    /// for symlinks. Directories carry [`Hash::EMPTY`]. The engine never
    /// computes one.
    Hash, 32
}

impl Hash {
    /// The "empty" hash directories carry. All zeros; no real BLAKE3 output
    /// will ever equal it in practice.
    pub const EMPTY: Self = Self([0u8; 32]);
}

/// What kind of filesystem object an entry is (§7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// What the scanner or watcher saw at a path (§7.3), after hashing and
/// after the 2 s stability check. The host produces this; the engine turns
/// it into an [`Entry`] by versioning it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Observed {
    pub kind: Kind,
    /// Bytes for files, target length for symlinks, 0 for directories.
    pub size: u64,
    /// Nanoseconds since the Unix epoch, as the filesystem reports it.
    pub mtime_ns: i64,
    /// The executable bit, files only.
    pub exec: bool,
    /// Content hash; [`Hash::EMPTY`] for directories.
    pub hash: Hash,
}

impl Observed {
    /// Apply the §7.1 field rules for the kind: directories have no size,
    /// hash or exec bit; symlinks have no exec bit. The host should already
    /// report them this way; this makes it impossible to depend on it.
    pub fn normalised(mut self) -> Self {
        match self.kind {
            Kind::Dir => {
                self.size = 0;
                self.hash = Hash::EMPTY;
                self.exec = false;
            }
            Kind::Symlink => self.exec = false,
            Kind::File => {}
        }
        self
    }
}

/// One entry as announced in a batch and stored in the index (§7.1, minus
/// `seq`, which is local to each machine).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Entry {
    pub path: RelPath,
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i64,
    pub exec: bool,
    pub hash: Hash,
    pub version: Version,
    /// This record is a tombstone (§7.7).
    pub deleted: bool,
    /// The node that produced this version.
    pub modified_by: NodeId,
    /// Hostname of `modified_by` when it made the change. Display and
    /// conflict names only, never comparison.
    pub author_host: HostName,
}

impl Entry {
    /// What the scanner sees of this entry, for the fast-path comparison.
    pub fn observed(&self) -> Observed {
        Observed {
            kind: self.kind,
            size: self.size,
            mtime_ns: self.mtime_ns,
            exec: self.exec,
            hash: self.hash,
        }
    }

    /// Content equality per §7.6: kind, hash, and for files the exec bit.
    /// `mtime_ns` and `size` are not compared (size is implied by the hash).
    /// Two tombstones have equal content; a tombstone and a live entry never do.
    pub fn same_content(&self, other: &Self) -> bool {
        if self.deleted || other.deleted {
            return self.deleted && other.deleted;
        }
        self.kind == other.kind
            && self.hash == other.hash
            && (self.kind != Kind::File || self.exec == other.exec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(i: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = i;
        NodeId::from_bytes(b)
    }

    fn hash(i: u8) -> Hash {
        let mut b = [0u8; 32];
        b[0] = i;
        Hash::from_bytes(b)
    }

    fn file(h: u8, exec: bool) -> Entry {
        Entry {
            path: RelPath::new("a.txt").unwrap(),
            kind: Kind::File,
            size: 10,
            mtime_ns: 1_000,
            exec,
            hash: hash(h),
            version: Version::empty().incremented(node(1)),
            deleted: false,
            modified_by: node(1),
            author_host: HostName::new("laptop").unwrap(),
        }
    }

    #[test]
    fn hash_is_64_hex_and_has_an_empty_value() {
        assert_eq!(Hash::EMPTY.to_string(), "0".repeat(64));
        assert_eq!(Hash::LEN, 32);
        let h: Hash = "ff".repeat(32).parse().unwrap();
        assert_eq!(*h.as_bytes(), [0xff; 32]);
        assert_eq!(h.short().to_string(), "ffffffff");
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            format!("\"{}\"", "ff".repeat(32))
        );
        let bytes = postcard::to_stdvec(&h).unwrap();
        assert!(bytes.len() <= 33);
        assert_eq!(postcard::from_bytes::<Hash>(&bytes).unwrap(), h);
    }

    #[test]
    fn kind_serialises_lowercase() {
        assert_eq!(
            serde_json::to_string(&Kind::Symlink).unwrap(),
            "\"symlink\""
        );
        assert_eq!(serde_json::from_str::<Kind>("\"dir\"").unwrap(), Kind::Dir);
    }

    #[test]
    fn normalised_strips_fields_the_kind_cannot_have() {
        let dir = Observed {
            kind: Kind::Dir,
            size: 4096,
            mtime_ns: 5,
            exec: true,
            hash: hash(9),
        }
        .normalised();
        assert_eq!(
            (dir.size, dir.hash, dir.exec, dir.mtime_ns),
            (0, Hash::EMPTY, false, 5)
        );
        let link = Observed {
            kind: Kind::Symlink,
            size: 7,
            mtime_ns: 5,
            exec: true,
            hash: hash(9),
        }
        .normalised();
        assert_eq!((link.size, link.hash, link.exec), (7, hash(9), false));
        let file = Observed {
            kind: Kind::File,
            size: 7,
            mtime_ns: 5,
            exec: true,
            hash: hash(9),
        };
        assert_eq!(file.clone().normalised(), file);
    }

    #[test]
    fn same_content_ignores_mtime_and_size_but_not_exec_or_hash() {
        let a = file(1, false);
        let mut b = a.clone();
        b.mtime_ns += 1;
        b.size += 1;
        b.version = b.version.incremented(node(2));
        b.modified_by = node(2);
        assert!(a.same_content(&b));

        assert!(!a.same_content(&file(2, false)), "different hash");
        assert!(!a.same_content(&file(1, true)), "different exec");

        let mut dir_a = a.clone();
        dir_a.kind = Kind::Dir;
        dir_a.hash = Hash::EMPTY;
        let mut dir_b = dir_a.clone();
        dir_b.exec = true; // not meaningful for dirs, must not matter
        assert!(dir_a.same_content(&dir_b));
        assert!(!dir_a.same_content(&a), "file vs dir");
    }

    #[test]
    fn tombstones_equal_each_other_and_nothing_else() {
        let live = file(1, false);
        let mut dead = live.clone();
        dead.deleted = true;
        let mut dead2 = dead.clone();
        dead2.hash = hash(5);
        dead2.kind = Kind::Dir;
        assert!(dead.same_content(&dead2));
        assert!(!dead.same_content(&live));
        assert!(!live.same_content(&dead));
    }

    #[test]
    fn entry_round_trips_through_both_formats() {
        let e = file(3, true);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"file\""));
        assert!(json.contains("\"author_host\":\"laptop\""));
        assert_eq!(serde_json::from_str::<Entry>(&json).unwrap(), e);
        let bytes = postcard::to_stdvec(&e).unwrap();
        assert_eq!(postcard::from_bytes::<Entry>(&bytes).unwrap(), e);
    }
}
