//! The store (DESIGN.md §11, §8.5): the daemon's SQLite database,
//! `delocal.db` in the state directory.
//!
//! It holds every folder's persisted parts, one table per engine part, and
//! the tables the host owns beside them; [`schema`] lists them. The file is
//! in WAL mode with `synchronous=FULL`, so a transaction is on disk when its
//! commit returns: in WAL mode `FULL` syncs the log at every commit, where
//! the default `NORMAL` would let the last commits before a power loss
//! vanish after the daemon had already acted on them.
//!
//! [`Store::open`] opens the file and brings its schema up to date, and
//! refuses one written by a newer delocal. A commit that finds another
//! connection holding the write lock waits up to [`BUSY_TIMEOUT`] for it.
//! [`Store::load`] reads back every folder's parts for `Engine::restore`;
//! the other readers return the host's tables. Writes arrive as
//! [`Group`]s, one per event, each holding the persistence hooks the
//! engine returned for it and the host's writes that go with them
//! ([`write`] maps each hook to its table), and the group-commit
//! [`Writer`] commits them, many groups to a transaction ([`writer`]).

use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

mod codec;
pub mod host;
mod load;
#[cfg(test)]
mod mirror;
#[cfg(test)]
mod sample;
pub mod schema;
pub mod write;
pub mod writer;

pub use host::{
    DiskName, FolderRow, HistoryRow, JournalRow, Machine, Member, Mode, Shim, TrashRow,
};
pub use schema::SCHEMA_VERSION;
pub use write::{Group, HostWrite};
pub use writer::{Durable, WriteError, Writer};

/// How long the store waits for another connection's write lock before
/// the statement that needs it fails. Long enough to wait out a lock held
/// for a moment (a `sqlite3` shell in the middle of a write, a backup);
/// short enough that the effects waiting on a commit (§11) do not stall
/// behind a lock that is not coming back. Past it the commit fails, and
/// the writer takes nothing more, as after any failed commit.
///
/// rusqlite already sets 5 s on every connection it opens; the store sets
/// it itself so that this does not rest on a library's default.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The daemon's database. See the module docs.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open the database at `path`, creating it if there is no file, and
    /// migrate it to [`SCHEMA_VERSION`]. A file with a schema version this
    /// binary does not know is refused with [`StoreError::UnknownSchema`]
    /// before anything is written to it.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        schema::check_version(&conn)?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::NotWal { mode });
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        schema::migrate(&mut conn)?;
        // After the migrations: SQLite's way of reshaping a table needs
        // foreign keys off while it runs.
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(Self { conn })
    }

    /// The file's schema version: [`SCHEMA_VERSION`] once it is open.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        schema::user_version(&self.conn)
    }
}

/// Why the store could not do what it was asked.
#[derive(Debug)]
pub enum StoreError {
    /// SQLite refused.
    Sqlite(rusqlite::Error),
    /// The file's schema version is not one this binary knows: a newer
    /// delocal wrote it. It was left untouched.
    UnknownSchema { found: i64, known: i64 },
    /// SQLite would not put the file in WAL mode, which the durability of a
    /// commit depends on; `mode` is the journal mode it kept.
    NotWal { mode: String },
    /// A row in `table` holds something the store could not have written.
    Corrupt { table: &'static str, detail: String },
    /// A value for `table` could not be encoded as JSON.
    Encode { table: &'static str, detail: String },
    /// The writer thread could not be started.
    Spawn(std::io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "database: {e}"),
            Self::UnknownSchema { found, known } => write!(
                f,
                "the database has schema version {found}, and this delocal knows versions up \
                 to {known}: a newer delocal wrote it, so it was left untouched"
            ),
            Self::NotWal { mode } => write!(
                f,
                "the database could not be put in WAL mode (it stayed in {mode} mode)"
            ),
            Self::Corrupt { table, detail } => write!(f, "a corrupt row in {table}: {detail}"),
            Self::Encode { table, detail } => {
                write!(f, "a value for {table} could not be encoded: {detail}")
            }
            Self::Spawn(e) => write!(f, "the store's writer could not start: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Spawn(e) => Some(e),
            Self::UnknownSchema { .. }
            | Self::NotWal { .. }
            | Self::Corrupt { .. }
            | Self::Encode { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables of schema version 1, in name order.
    const TABLES: [&str; 15] = [
        "batch_entries",
        "batches",
        "commit_journal",
        "deferred",
        "disk_names",
        "entries",
        "folder_state",
        "folders",
        "held_items",
        "machines",
        "members",
        "mtime_shim",
        "pending",
        "trash",
        "want",
    ];

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn pragma<T: rusqlite::types::FromSql>(conn: &Connection, name: &str) -> T {
        conn.pragma_query_value(None, name, |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn an_empty_file_migrates_to_the_current_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delocal.db");
        std::fs::write(&path, b"").unwrap();

        let store = Store::open(&path).unwrap();
        assert_eq!(SCHEMA_VERSION, 1);
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(tables(&store.conn), TABLES);
        assert_eq!(pragma::<String>(&store.conn, "journal_mode"), "wal");
        // 2 is FULL.
        assert_eq!(pragma::<i64>(&store.conn, "synchronous"), 2);
        assert_eq!(pragma::<i64>(&store.conn, "busy_timeout"), 5_000);
        assert_eq!(pragma::<i64>(&store.conn, "foreign_keys"), 1);
    }

    #[test]
    fn a_missing_file_is_created_and_reopening_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delocal.db");
        drop(Store::open(&path).unwrap());
        // Automatic indexes have no SQL, hence the Option.
        let schema = |conn: &Connection| -> Vec<(String, Option<String>)> {
            let mut stmt = conn
                .prepare("SELECT name, sql FROM sqlite_schema ORDER BY name")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let first = schema(&Store::open(&path).unwrap().conn);
        let again = Store::open(&path).unwrap();
        assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(schema(&again.conn), first);
    }

    #[test]
    fn a_newer_schema_is_refused_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delocal.db");
        // A database from a future delocal, in the default rollback-journal
        // mode, with a table this binary has never heard of.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE from_the_future (x INTEGER) STRICT;")
                .unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        match Store::open(&path) {
            Err(StoreError::UnknownSchema { found, known }) => {
                assert_eq!((found, known), (SCHEMA_VERSION + 1, SCHEMA_VERSION));
            }
            Err(e) => panic!("expected UnknownSchema, got {e}"),
            Ok(_) => panic!("a newer schema was opened"),
        }
        // Not a byte changed: not switched to WAL, nothing migrated.
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!dir.path().join("delocal.db-wal").exists());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(pragma::<String>(&conn, "journal_mode"), "delete");
        assert_eq!(tables(&conn), ["from_the_future"]);
    }

    #[test]
    fn a_negative_schema_version_is_refused_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delocal.db");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", -1)
            .unwrap();
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::UnknownSchema { found: -1, .. })
        ));
    }

    #[test]
    fn a_file_that_is_not_a_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delocal.db");
        std::fs::write(&path, [0x42; 4096]).unwrap();
        assert!(matches!(Store::open(&path), Err(StoreError::Sqlite(_))));
        assert_eq!(std::fs::read(&path).unwrap(), [0x42; 4096]);
    }
}
