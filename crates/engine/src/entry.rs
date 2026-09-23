//! Index entries (DESIGN.md §7.1) and content equality (§7.6).
//!
//! [`Entry`] is the §7.1 record without `seq`: what travels in a batch
//! (§7.4) and what a scanner observation becomes once the engine has
//! versioned it. [`ContentHash`] is opaque here: the host computes BLAKE3,
//! the engine only compares.

use serde::{Deserialize, Serialize};

use crate::id::{HostName, NodeId, bytes_newtype};
use crate::path::RelPath;
use crate::version::Version;

bytes_newtype! {
    /// A BLAKE3 hash (§7.1): of the content for files, of the target string
    /// for symlinks. Directories and tombstones carry [`ContentHash::EMPTY`].
    /// The engine never computes one.
    ContentHash, 32
}

impl ContentHash {
    /// The all-zero sentinel for directories and tombstones (§7.1). Never
    /// the hash of a file: BLAKE3 of empty input is not all zeros.
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
    /// Files only; 0 for directories and symlinks (§7.1).
    pub mtime_ns: i64,
    /// The executable bit, files only.
    pub exec: bool,
    /// Content hash; [`ContentHash::EMPTY`] for directories.
    pub hash: ContentHash,
}

impl Observed {
    /// Apply the §7.1 field rules for the kind: directories have no size,
    /// hash, exec bit or mtime; symlinks have no exec bit or mtime. A
    /// directory's mtime changes whenever a child is created or removed, so
    /// syncing it would turn every file change into a directory touch on
    /// every machine, forever. The host should already report them this
    /// way; this makes it impossible to depend on it.
    pub fn normalised(mut self) -> Self {
        match self.kind {
            Kind::Dir => {
                self.size = 0;
                self.hash = ContentHash::EMPTY;
                self.exec = false;
                self.mtime_ns = 0;
            }
            Kind::Symlink => {
                self.exec = false;
                self.mtime_ns = 0;
            }
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
    /// Files only; 0 for directories and symlinks (§7.1). For a tombstone,
    /// when the deletion was observed.
    pub mtime_ns: i64,
    /// A Lamport timestamp seeded by the modification time (§7.1): a content
    /// change stamps `max(mtime_ns, replaced record's stamp + 1)`; a
    /// metadata-only change, a tombstone, a directory or a symlink stamps
    /// `replaced record's stamp + 1`; a path with no record stamps
    /// `mtime_ns`, or 1 for a kind without one. Set by the author, carried
    /// with the entry, inherited by a merged record from the winner. It
    /// strictly increases along every machine's chain of versions for a
    /// path, so as the first key of the winner rule (§7.6) it makes the
    /// merged content a function of the vector. Never compared to a clock.
    pub stamp: i64,
    pub exec: bool,
    /// Content hash; [`ContentHash::EMPTY`] for directories and tombstones.
    pub hash: ContentHash,
    /// The `hash` of the version this one replaced, as it was when the
    /// change was made; [`ContentHash::EMPTY`] if the path did not exist.
    /// Set by the author and carried with the entry. `hash == prev_hash`
    /// means a metadata-only change (§7.1), which loses conflicts to real
    /// edits (§7.6) and is invisible to the brake (§8.1).
    pub prev_hash: ContentHash,
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

    /// The scan fast path (§7.3): true if what `stat` reports at the path
    /// means this live entry is unchanged, so the scanner need not hash it.
    /// A file is unchanged only if kind, size, mtime and the exec bit all
    /// match; a directory or symlink if its kind matches. The exec bit is
    /// part of the test because `chmod` changes neither size nor mtime: a
    /// fast path on those two alone would never notice a `chmod` whose
    /// watcher event was lost, and the index would disagree with the disk
    /// forever. A tombstone matches nothing: whatever is on disk is new.
    pub fn unchanged_by_stat(&self, kind: Kind, size: u64, mtime_ns: i64, exec: bool) -> bool {
        if self.deleted || self.kind != kind {
            return false;
        }
        match kind {
            Kind::File => self.size == size && self.mtime_ns == mtime_ns && self.exec == exec,
            Kind::Dir | Kind::Symlink => true,
        }
    }

    /// A metadata-only change (§7.1): the content is what it was before.
    /// Touches, and tombstones for paths peers never saw.
    pub fn is_metadata_only(&self) -> bool {
        self.hash == self.prev_hash
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

    fn hash(i: u8) -> ContentHash {
        let mut b = [0u8; 32];
        b[0] = i;
        ContentHash::from_bytes(b)
    }

    fn file(h: u8, exec: bool) -> Entry {
        Entry {
            path: RelPath::new("a.txt").unwrap(),
            kind: Kind::File,
            size: 10,
            mtime_ns: 1_000,
            stamp: 1_000,
            exec,
            hash: hash(h),
            prev_hash: ContentHash::EMPTY,
            version: Version::empty().incremented(node(1)),
            deleted: false,
            modified_by: node(1),
            author_host: HostName::new("laptop").unwrap(),
        }
    }

    #[test]
    fn hash_is_64_hex_and_has_an_empty_value() {
        assert_eq!(ContentHash::EMPTY.to_string(), "0".repeat(64));
        assert_eq!(ContentHash::LEN, 32);
        let h: ContentHash = "ff".repeat(32).parse().unwrap();
        assert_eq!(*h.as_bytes(), [0xff; 32]);
        assert_eq!(h.short().to_string(), "ffffffff");
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            format!("\"{}\"", "ff".repeat(32))
        );
        let bytes = postcard::to_stdvec(&h).unwrap();
        assert!(bytes.len() <= 33);
        assert_eq!(postcard::from_bytes::<ContentHash>(&bytes).unwrap(), h);
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
            (0, ContentHash::EMPTY, false, 0),
            "directories carry no mtime"
        );
        let link = Observed {
            kind: Kind::Symlink,
            size: 7,
            mtime_ns: 5,
            exec: true,
            hash: hash(9),
        }
        .normalised();
        assert_eq!(
            (link.size, link.hash, link.exec, link.mtime_ns),
            (7, hash(9), false, 0),
            "symlinks carry no mtime"
        );
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
        dir_a.hash = ContentHash::EMPTY;
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
    fn the_fast_path_compares_kind_size_mtime_and_exec() {
        let e = file(1, false);
        assert!(e.unchanged_by_stat(Kind::File, e.size, e.mtime_ns, false));
        assert!(
            !e.unchanged_by_stat(Kind::File, e.size + 1, e.mtime_ns, false),
            "size"
        );
        assert!(
            !e.unchanged_by_stat(Kind::File, e.size, e.mtime_ns + 1, false),
            "mtime"
        );
        assert!(
            !e.unchanged_by_stat(Kind::File, e.size, e.mtime_ns, true),
            "a chmod is a change"
        );
        assert!(
            !e.unchanged_by_stat(Kind::Symlink, e.size, e.mtime_ns, false),
            "kind"
        );
        let mut dir = file(1, false);
        dir.kind = Kind::Dir;
        assert!(
            dir.unchanged_by_stat(Kind::Dir, 4096, 99, true),
            "directories match by kind alone"
        );
        assert!(!dir.unchanged_by_stat(Kind::File, 0, 0, false));
        let mut dead = file(1, false);
        dead.deleted = true;
        assert!(
            !dead.unchanged_by_stat(Kind::File, dead.size, dead.mtime_ns, false),
            "a tombstone matches nothing"
        );
    }

    #[test]
    fn metadata_only_means_hash_equals_prev_hash() {
        let mut e = file(1, false);
        assert!(!e.is_metadata_only(), "a first add replaces nothing");
        e.prev_hash = hash(1);
        assert!(e.is_metadata_only(), "a touch");
        e.prev_hash = hash(2);
        assert!(!e.is_metadata_only(), "a real edit");
    }

    #[test]
    fn entry_round_trips_through_both_formats() {
        let e = file(3, true);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"file\""));
        assert!(json.contains("\"author_host\":\"laptop\""));
        assert!(json.contains(&format!("\"prev_hash\":\"{}\"", "0".repeat(64))));
        assert_eq!(serde_json::from_str::<Entry>(&json).unwrap(), e);
        let bytes = postcard::to_stdvec(&e).unwrap();
        assert_eq!(postcard::from_bytes::<Entry>(&bytes).unwrap(), e);
    }
}
