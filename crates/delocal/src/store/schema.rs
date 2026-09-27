//! The SQLite schema (DESIGN.md §11) and its migrations.
//!
//! The schema follows the persisted parts: one table per engine part (the
//! index in `entries`, `want`, `pending`, `held_items`, `deferred`, and the
//! small rest in `folder_state`), and beside them the tables the host owns
//! (`folders` and `members`, history in `batches` and `batch_entries`,
//! `trash`, `mtime_shim`, `disk_names`, `commit_journal`, `machines`).
//! Nothing is ever stored as a whole-state blob.
//!
//! **Versions.** `PRAGMA user_version` holds the schema version, 0 for a new
//! or empty file. `MIGRATIONS[n]` takes a database from version `n` to
//! `n + 1`, and each one runs in its own transaction together with the
//! version bump, so a crash during a migration leaves the file at the old
//! version or the new one, never between. A file whose version is above
//! what this binary knows was written by a newer delocal: it is refused and
//! left untouched, because guessing at a schema we do not know is how a
//! store loses rows.
//!
//! **Types.** Every table is `STRICT`, so SQLite refuses a value of the
//! wrong type instead of storing it; the `CHECK`s catch the rest of what a
//! bug in the encoding could write (a 15-byte node id, a `kind` that is not
//! one). Identifiers and hashes are `BLOB`s of their bytes; index paths are
//! `TEXT` (they are UTF-8, §7.1); places on disk (a member's folder path, a
//! trash location, a temp file) are `BLOB`s of their bytes, since a name on
//! disk need not be UTF-8. Counters that are `u64` in Rust are stored as the
//! `i64` with the same bits, so every value round-trips, including the ones
//! a bad peer could send; SQL never compares or sums them. Engine values
//! with nested structure are JSON (`serde_json`, Appendix A), in columns
//! named `_json`.
//!
//! **Keys.** Every folder's rows reference `folders`, and foreign keys are
//! on, so a row for a folder the host never recorded is an error at the
//! write, not a row that silently fails to load. Nothing cascades: removing
//! a folder is Phase 4's, and a delete that took a folder's whole index
//! with it by accident is the kind of mistake this schema should not make
//! possible.

use rusqlite::{Connection, TransactionBehavior};

use super::StoreError;

/// Schema version 1: every table §11 names.
const V1: &str = r"
-- Folder metadata, which the host keeps (§9.1): where FolderParts gets its
-- id, rules and members.
CREATE TABLE folders (
    id           BLOB    NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    name         TEXT    NOT NULL,
    created_by   BLOB    NOT NULL CHECK (length(created_by) = 16),
    rules_json   TEXT    NOT NULL,
    meta_version INTEGER NOT NULL
) STRICT;

-- mode has no CHECK: §9.4 stores it from day one so that later modes need
-- no migration.
CREATE TABLE members (
    folder    BLOB    NOT NULL REFERENCES folders (id),
    node      BLOB    NOT NULL CHECK (length(node) = 16),
    path      BLOB    NOT NULL,
    mode      TEXT    NOT NULL,
    joined_at INTEGER NOT NULL,
    PRIMARY KEY (folder, node)
) STRICT;

-- The small rest (§11): one row per folder.
CREATE TABLE folder_state (
    folder          BLOB NOT NULL PRIMARY KEY REFERENCES folders (id),
    small_rest_json TEXT NOT NULL
) STRICT;

-- The index (§7.1): one row per entry, tombstones included.
CREATE TABLE entries (
    folder       BLOB    NOT NULL REFERENCES folders (id),
    path         TEXT    NOT NULL,
    kind         TEXT    NOT NULL CHECK (kind IN ('file', 'dir', 'symlink')),
    size         INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    exec         INTEGER NOT NULL CHECK (exec IN (0, 1)),
    hash         BLOB    NOT NULL CHECK (length(hash) = 32),
    prev_hash    BLOB    NOT NULL CHECK (length(prev_hash) = 32),
    stamp        INTEGER NOT NULL,
    version_blob BLOB    NOT NULL,
    deleted      INTEGER NOT NULL CHECK (deleted IN (0, 1)),
    modified_by  BLOB    NOT NULL CHECK (length(modified_by) = 16),
    author_host  TEXT    NOT NULL,
    seq          INTEGER NOT NULL,
    PRIMARY KEY (folder, path)
) STRICT;

-- The wants (§7.5): one row per path.
CREATE TABLE want (
    folder    BLOB NOT NULL REFERENCES folders (id),
    path      TEXT NOT NULL,
    want_json TEXT NOT NULL,
    PRIMARY KEY (folder, path)
) STRICT;

-- The pending set (§7.1, §8.1): one row per unannounced path, with the
-- record peers last saw there (NULL if they never saw one).
CREATE TABLE pending (
    folder                BLOB    NOT NULL REFERENCES folders (id),
    path                  TEXT    NOT NULL,
    announced_record_json TEXT,
    exempt                INTEGER NOT NULL CHECK (exempt IN (0, 1)),
    PRIMARY KEY (folder, path)
) STRICT;

-- The held items (§8.2, §8.3): one row per item, held or denied. A denied
-- row is also keyed by the arrival number its deny took (0 while held), so
-- one batch can be held, denied, held again and denied again. row_json
-- carries the item and every version quarantined for it, each with its
-- arrival number.
CREATE TABLE held_items (
    folder    BLOB    NOT NULL REFERENCES folders (id),
    batch     BLOB    NOT NULL CHECK (length(batch) = 16),
    state     TEXT    NOT NULL CHECK (state IN ('held', 'denied')),
    denied_at INTEGER NOT NULL CHECK (state = 'denied' OR denied_at = 0),
    row_json  TEXT    NOT NULL,
    PRIMARY KEY (folder, batch, state, denied_at)
) STRICT;

-- The deferred paths (§7.5, §8.1): one row per path, its entries in
-- arrival order.
CREATE TABLE deferred (
    folder       BLOB NOT NULL REFERENCES folders (id),
    path         TEXT NOT NULL,
    entries_json TEXT NOT NULL,
    PRIMARY KEY (folder, path)
) STRICT;

-- History (§8.5): one row per batch this machine recorded, in the order it
-- recorded them. A batch id can be recorded more than once (paused, then
-- sent on approve; received twice over a lossy transport), so the key is
-- the record, not the id.
CREATE TABLE batches (
    record      INTEGER NOT NULL PRIMARY KEY,
    id          BLOB    NOT NULL CHECK (length(id) = 16),
    folder      BLOB    NOT NULL REFERENCES folders (id),
    source      BLOB    NOT NULL CHECK (length(source) = 16),
    role        TEXT    NOT NULL CHECK (role IN ('sent', 'received', 'paused')),
    created_at  INTEGER NOT NULL,
    seq_low     INTEGER NOT NULL,
    seq_high    INTEGER NOT NULL,
    adds        INTEGER NOT NULL,
    mods        INTEGER NOT NULL,
    dels        INTEGER NOT NULL,
    bytes       INTEGER NOT NULL,
    decision    TEXT    CHECK (decision IN ('accepted', 'held')),
    held_reason TEXT    CHECK ((decision IS 'held') = (held_reason IS NOT NULL)),
    recorded_at INTEGER NOT NULL
) STRICT;

-- A recorded batch's entries, whole, in the batch's order (the sender's
-- seq order, §7.4).
CREATE TABLE batch_entries (
    record       INTEGER NOT NULL REFERENCES batches (record),
    position     INTEGER NOT NULL,
    path         TEXT    NOT NULL,
    kind         TEXT    NOT NULL CHECK (kind IN ('file', 'dir', 'symlink')),
    size         INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    exec         INTEGER NOT NULL CHECK (exec IN (0, 1)),
    hash         BLOB    NOT NULL CHECK (length(hash) = 32),
    prev_hash    BLOB    NOT NULL CHECK (length(prev_hash) = 32),
    stamp        INTEGER NOT NULL,
    version_blob BLOB    NOT NULL,
    deleted      INTEGER NOT NULL CHECK (deleted IN (0, 1)),
    modified_by  BLOB    NOT NULL CHECK (length(modified_by) = 16),
    author_host  TEXT    NOT NULL,
    PRIMARY KEY (record, position)
) STRICT;

-- The trash (§8.4): one row per file in it, by where it is under the
-- folder's .delocal/trash/.
CREATE TABLE trash (
    folder        BLOB    NOT NULL REFERENCES folders (id),
    trashed_path  BLOB    NOT NULL,
    original_path TEXT    NOT NULL,
    hash          BLOB    NOT NULL CHECK (length(hash) = 32),
    trashed_at    INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    PRIMARY KEY (folder, trashed_path)
) STRICT;

-- The mtime precision shim (§7.3).
CREATE TABLE mtime_shim (
    folder       BLOB    NOT NULL REFERENCES folders (id),
    path         TEXT    NOT NULL,
    requested_ns INTEGER NOT NULL,
    stored_ns    INTEGER NOT NULL,
    PRIMARY KEY (folder, path)
) STRICT;

-- Names on disk that are not their index path's bytes (§7.3): the name of
-- the path's last component as the directory lists it.
CREATE TABLE disk_names (
    folder BLOB NOT NULL REFERENCES folders (id),
    path   TEXT NOT NULL,
    bytes  BLOB NOT NULL,
    PRIMARY KEY (folder, path)
) STRICT;

-- The commit journal (§7.5): a commit between its displacement and its
-- rename. temp_file is NULL when nothing is renamed in (a directory).
CREATE TABLE commit_journal (
    folder       BLOB NOT NULL REFERENCES folders (id),
    path         TEXT NOT NULL,
    displaced_to BLOB NOT NULL,
    temp_file    BLOB,
    PRIMARY KEY (folder, path)
) STRICT;

-- This user's machines (§5, §10).
CREATE TABLE machines (
    node            BLOB    NOT NULL PRIMARY KEY CHECK (length(node) = 16),
    hostname        TEXT    NOT NULL,
    ts_stable_id    TEXT    NOT NULL,
    ts_user         TEXT    NOT NULL,
    trusted         INTEGER NOT NULL CHECK (trusted IN (0, 1)),
    last_seen       INTEGER NOT NULL,
    delocal_version TEXT    NOT NULL
) STRICT;
";

/// Every migration, oldest first: `MIGRATIONS[n]` takes a database at
/// `user_version` `n` to `n + 1`. Append only; a migration that has shipped
/// is never edited.
const MIGRATIONS: &[&str] = &[V1];

/// The schema version this binary writes, and the newest it can open.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// The database's `user_version`.
pub(super) fn user_version(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// Refuse a database this binary does not know, before anything is written
/// to it: not even the switch to WAL, which rewrites the file's header.
pub(super) fn check_version(conn: &Connection) -> Result<(), StoreError> {
    let found = user_version(conn)?;
    if (0..=SCHEMA_VERSION).contains(&found) {
        Ok(())
    } else {
        Err(StoreError::UnknownSchema {
            found,
            known: SCHEMA_VERSION,
        })
    }
}

/// Bring the database up to [`SCHEMA_VERSION`], one migration per
/// transaction. The version is read again inside each transaction, which
/// holds the write lock from its start, so two processes opening one file
/// cannot both run a step.
pub(super) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_version(&tx)?;
        let at = user_version(&tx)?;
        let Some(step) = usize::try_from(at).ok().and_then(|at| MIGRATIONS.get(at)) else {
            return Ok(());
        };
        tx.execute_batch(step)?;
        tx.pragma_update(None, "user_version", at + 1)?;
        tx.commit()?;
    }
}
