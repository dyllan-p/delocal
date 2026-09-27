//! Rows of the tables the host owns (DESIGN.md §11): folder metadata and
//! membership (§9.1), history (§8.5), the trash (§8.4), the mtime shim and
//! names on disk (§7.3), the commit journal (§7.5) and machines (§5). None
//! of them is an engine part; the host writes them in the same group as the
//! engine writes they belong to ([`super::Group::host`]).

use std::ffi::OsString;
use std::path::PathBuf;

use delocal_engine::{
    Batch, BatchRole, ContentHash, Decision, FolderId, NodeId, RelPath, Rules, Timestamp,
};

/// A folder as the host records it (§9.1): where a folder's
/// [`FolderParts`](delocal_engine::FolderParts) take their id and rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderRow {
    pub id: FolderId,
    /// The basename of the path it was first shared from.
    pub name: String,
    pub created_by: NodeId,
    pub rules: Rules,
    /// The version of the folder's metadata, which is synced separately
    /// from its entries (§9.1).
    pub meta_version: u64,
}

/// A member of a folder (§9.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub node: NodeId,
    /// Where the folder is on that machine.
    pub path: PathBuf,
    pub mode: Mode,
    pub joined_at: Timestamp,
}

/// How a member takes part in a folder (§9.4). v1 has one mode; the column
/// exists so the others need no migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    TwoWay,
}

/// One batch in history (§8.5): a `RecordBatch` action as it was reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    pub batch: Batch,
    pub role: BatchRole,
    pub decision: Option<Decision>,
    /// When this machine recorded it: the time of the event whose actions
    /// held the `RecordBatch`. For a received batch that is when it was
    /// decided.
    pub recorded_at: Timestamp,
}

/// A file in the trash (§8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashRow {
    /// Where it is, relative to the folder's `.delocal/trash/`.
    pub trashed_path: PathBuf,
    /// The index path it was moved away from.
    pub original_path: RelPath,
    /// Its content, so that a fetch of that hash can be filled from the
    /// trash without asking a peer (§7.5 step 2).
    pub hash: ContentHash,
    pub trashed_at: Timestamp,
    pub size: u64,
}

/// A pair the mtime precision shim keeps (§7.3): the host asked for
/// `requested_ns` at `path`, and the filesystem kept `stored_ns`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shim {
    pub path: RelPath,
    pub requested_ns: i64,
    pub stored_ns: i64,
}

/// A name on disk that is not the bytes of its index path (§7.3): the last
/// component of `path` as the directory lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskName {
    pub path: RelPath,
    pub name: OsString,
}

/// An open row of the commit journal (§7.5): a commit at `path` between
/// its displacement and its rename.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalRow {
    pub path: RelPath,
    /// Where step 7 moves the file at `path`, relative to the folder root:
    /// into the trash, or to a conflict path.
    pub displaced_to: PathBuf,
    /// The temp file step 8 renames in, relative to the folder root; `None`
    /// when nothing is renamed in (a directory is created instead).
    pub temp_file: Option<PathBuf>,
}

/// One of this user's machines (§5, §10 `machines`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub node: NodeId,
    pub hostname: String,
    /// The Tailscale stable node id the node id is bound to (§5).
    pub ts_stable_id: String,
    pub ts_user: String,
    /// `machines trust` for a tagged device (§5).
    pub trusted: bool,
    pub last_seen: Timestamp,
    pub delocal_version: String,
}
