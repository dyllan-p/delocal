//! How engine values become columns and back (DESIGN.md §11), for the
//! encodings the module docs of [`super::schema`] describe.
//!
//! Every decoder is strict: a value this module did not write (a version
//! blob with a zero counter, a node id of the wrong length, a `kind` that
//! is not one) is an error naming the table, never a guess. The encoders
//! are total, so no value the engine hands the store can fail to be
//! written for its shape.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use delocal_engine::{
    BatchId, ContentHash, Entry, FolderId, HostName, Kind, NodeId, RelPath, Timestamp, Version,
};
use rusqlite::Row;
use rusqlite::types::ToSql;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::StoreError;

/// A `u64` as the `i64` with the same bits (the module docs of
/// [`super::schema`] say why).
pub(super) fn int(n: u64) -> i64 {
    n.cast_signed()
}

/// The `u64` whose bits an [`int`] column holds.
pub(super) fn uint(n: i64) -> u64 {
    n.cast_unsigned()
}

pub(super) fn time(t: Timestamp) -> i64 {
    t.as_unix_nanos()
}

pub(super) fn timestamp(n: i64) -> Timestamp {
    Timestamp::from_unix_nanos(n)
}

/// A corrupt row in `table`.
pub(super) fn corrupt(table: &'static str, detail: impl Into<String>) -> StoreError {
    StoreError::Corrupt {
        table,
        detail: detail.into(),
    }
}

/// Bytes of exactly `N`, from a `BLOB` column of `table`.
fn exact<const N: usize>(
    table: &'static str,
    what: &str,
    blob: &[u8],
) -> Result<[u8; N], StoreError> {
    blob.try_into()
        .map_err(|_| corrupt(table, format!("{what} is {} bytes, not {N}", blob.len())))
}

pub(super) fn node(table: &'static str, blob: &[u8]) -> Result<NodeId, StoreError> {
    exact(table, "a node id", blob).map(NodeId::from_bytes)
}

pub(super) fn folder_id(table: &'static str, blob: &[u8]) -> Result<FolderId, StoreError> {
    exact(table, "a folder id", blob).map(FolderId::from_bytes)
}

pub(super) fn batch_id(table: &'static str, blob: &[u8]) -> Result<BatchId, StoreError> {
    exact(table, "a batch id", blob).map(BatchId::from_bytes)
}

pub(super) fn hash(table: &'static str, blob: &[u8]) -> Result<ContentHash, StoreError> {
    exact(table, "a hash", blob).map(ContentHash::from_bytes)
}

pub(super) fn rel_path(table: &'static str, text: String) -> Result<RelPath, StoreError> {
    RelPath::new(text.clone()).map_err(|e| corrupt(table, format!("path {text:?}: {e}")))
}

pub(super) fn host_name(table: &'static str, text: String) -> Result<HostName, StoreError> {
    HostName::new(text.clone()).map_err(|e| corrupt(table, format!("host name {text:?}: {e}")))
}

/// A place on disk as the bytes of its name (Unix paths are bytes).
pub(super) fn disk_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

pub(super) fn disk_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

pub(super) fn kind_text(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Dir => "dir",
        Kind::Symlink => "symlink",
    }
}

pub(super) fn kind(table: &'static str, text: &str) -> Result<Kind, StoreError> {
    match text {
        "file" => Ok(Kind::File),
        "dir" => Ok(Kind::Dir),
        "symlink" => Ok(Kind::Symlink),
        other => Err(corrupt(table, format!("kind {other:?}"))),
    }
}

/// Bytes per component of a version blob: the node id, then the counter as
/// 8 big-endian bytes.
const COMPONENT: usize = NodeId::LEN + 8;

/// A version vector as a blob: its components in node order, each the node
/// id's 16 bytes and the counter's 8, big-endian. Fixed-width and canonical
/// (one blob per version), so two blobs are equal exactly when the versions
/// are, and it needs no crate.
pub(super) fn version_blob(version: &Version) -> Vec<u8> {
    let mut blob = Vec::with_capacity(COMPONENT * version.iter().count());
    for (node, counter) in version.iter() {
        blob.extend_from_slice(node.as_bytes());
        blob.extend_from_slice(&counter.to_be_bytes());
    }
    blob
}

/// The version a [`version_blob`] holds. Refuses anything that blob could
/// not have been: a length that is not whole components, nodes out of
/// order or repeated, a zero counter (a version never stores one).
pub(super) fn version(table: &'static str, blob: &[u8]) -> Result<Version, StoreError> {
    let (chunks, remainder) = blob.as_chunks::<COMPONENT>();
    if !remainder.is_empty() {
        return Err(corrupt(
            table,
            format!("a version blob of {} bytes", blob.len()),
        ));
    }
    let mut components = Vec::with_capacity(chunks.len());
    let mut last: Option<NodeId> = None;
    for chunk in chunks {
        let (id, counter) = chunk.split_at(NodeId::LEN);
        let id = node(table, id)?;
        let counter = u64::from_be_bytes(exact(table, "a counter", counter)?);
        if last.is_some_and(|last| last >= id) || counter == 0 {
            return Err(corrupt(table, "a version blob that is not canonical"));
        }
        last = Some(id);
        components.push((id, counter));
    }
    Ok(components.into_iter().collect())
}

/// An engine value as JSON.
pub(super) fn json<T: Serialize>(table: &'static str, value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|e| StoreError::Encode {
        table,
        detail: e.to_string(),
    })
}

/// The engine value a `_json` column holds.
pub(super) fn from_json<T: DeserializeOwned>(
    table: &'static str,
    text: &str,
) -> Result<T, StoreError> {
    serde_json::from_str(text).map_err(|e| corrupt(table, format!("JSON: {e}")))
}

/// The column names of an entry, in the order [`EntryColumns::params`]
/// binds them and [`entry`] reads them: shared by `entries` (which adds
/// `seq`) and `batch_entries`. A macro rather than a `const` so that
/// `concat!` can put it into whole SQL statements.
macro_rules! entry_columns {
    () => {
        "path, kind, size, mtime_ns, exec, hash, prev_hash, stamp, version_blob, deleted, \
         modified_by, author_host"
    };
}
pub(super) use entry_columns;

/// An entry's columns, encoded, in the order of [`entry_columns!`].
pub(super) struct EntryColumns {
    path: String,
    kind: &'static str,
    size: i64,
    mtime_ns: i64,
    exec: bool,
    hash: [u8; ContentHash::LEN],
    prev_hash: [u8; ContentHash::LEN],
    stamp: i64,
    version: Vec<u8>,
    deleted: bool,
    modified_by: [u8; NodeId::LEN],
    author_host: String,
}

impl EntryColumns {
    pub fn of(entry: &Entry) -> Self {
        Self {
            path: entry.path.as_str().to_owned(),
            kind: kind_text(entry.kind),
            size: int(entry.size),
            mtime_ns: entry.mtime_ns,
            exec: entry.exec,
            hash: *entry.hash.as_bytes(),
            prev_hash: *entry.prev_hash.as_bytes(),
            stamp: entry.stamp,
            version: version_blob(&entry.version),
            deleted: entry.deleted,
            modified_by: *entry.modified_by.as_bytes(),
            author_host: entry.author_host.as_str().to_owned(),
        }
    }

    /// The values to bind, in the order of [`entry_columns!`].
    pub fn params(&self) -> [&dyn ToSql; 12] {
        [
            &self.path,
            &self.kind,
            &self.size,
            &self.mtime_ns,
            &self.exec,
            &self.hash,
            &self.prev_hash,
            &self.stamp,
            &self.version,
            &self.deleted,
            &self.modified_by,
            &self.author_host,
        ]
    }
}

/// The entry in `row`'s columns `at..at + 12`, laid out as
/// [`entry_columns!`].
pub(super) fn entry(table: &'static str, row: &Row<'_>, at: usize) -> Result<Entry, StoreError> {
    let text = |i: usize| row.get::<_, String>(at + i);
    let blob = |i: usize| row.get::<_, Vec<u8>>(at + i);
    Ok(Entry {
        path: rel_path(table, text(0)?)?,
        kind: kind(table, &text(1)?)?,
        size: uint(row.get(at + 2)?),
        mtime_ns: row.get(at + 3)?,
        exec: row.get(at + 4)?,
        hash: hash(table, &blob(5)?)?,
        prev_hash: hash(table, &blob(6)?)?,
        stamp: row.get(at + 7)?,
        version: version(table, &blob(8)?)?,
        deleted: row.get(at + 9)?,
        modified_by: node(table, &blob(10)?)?,
        author_host: host_name(table, text(11)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> NodeId {
        NodeId::from_bytes([n; 16])
    }

    #[test]
    fn a_version_blob_is_its_components_in_node_order() {
        let v: Version = [(id(2), 7), (id(1), u64::MAX)].into_iter().collect();
        let blob = version_blob(&v);
        let mut expected = Vec::new();
        expected.extend_from_slice(&[1; 16]);
        expected.extend_from_slice(&u64::MAX.to_be_bytes());
        expected.extend_from_slice(&[2; 16]);
        expected.extend_from_slice(&7u64.to_be_bytes());
        assert_eq!(blob, expected);
        assert_eq!(version("t", &blob).unwrap(), v);
        assert_eq!(version_blob(&Version::empty()), Vec::<u8>::new());
        assert_eq!(version("t", &[]).unwrap(), Version::empty());
    }

    #[test]
    fn a_version_blob_it_could_not_have_written_is_corrupt() {
        let component = |n: u8, counter: u64| {
            let mut c = vec![n; 16];
            c.extend_from_slice(&counter.to_be_bytes());
            c
        };
        let refused = [
            // Not whole components.
            vec![0; 23],
            // A zero counter.
            component(1, 0),
            // Nodes out of order, and repeated.
            [component(2, 1), component(1, 1)].concat(),
            [component(1, 1), component(1, 2)].concat(),
        ];
        for blob in refused {
            assert!(
                matches!(
                    version("t", &blob),
                    Err(StoreError::Corrupt { table: "t", .. })
                ),
                "{blob:?}"
            );
        }
    }

    #[test]
    fn every_u64_survives_an_integer_column() {
        for n in [0, 1, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
            assert_eq!(uint(int(n)), n);
        }
        assert_eq!(int(u64::MAX), -1);
    }

    #[test]
    fn ids_of_the_wrong_length_are_corrupt() {
        assert!(node("t", &[0; 15]).is_err());
        assert!(hash("t", &[0; 33]).is_err());
        assert_eq!(node("t", &[3; 16]).unwrap(), id(3));
    }

    #[test]
    fn a_place_on_disk_keeps_bytes_that_are_not_utf8() {
        let bytes = b"trash/2026-09-27/\xff\xfe.txt".to_vec();
        let path = disk_path(bytes.clone());
        assert_eq!(disk_bytes(&path), bytes);
    }
}
