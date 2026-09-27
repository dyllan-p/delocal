//! Reading the store back (DESIGN.md §11): each folder's persisted parts,
//! which the daemon hands to `Engine::restore` at start-up, and the tables
//! the host owns.
//!
//! A folder's parts are rebuilt from its rows and nothing else. A folder
//! the host recorded but whose small rest is missing was never reported by
//! the engine (its `FolderJoined` did not become durable), so it has no
//! parts: [`Store::parts`] says `None`, and the daemon joins it afresh.

use std::collections::{BTreeMap, BTreeSet};

use delocal_engine::{
    Batch, BatchId, BatchRole, Decision, Deferred, FolderId, FolderParts, HeldRow, HeldState,
    IndexRecord, Pending, RelPath, Rest, Summary, Want,
};
use rusqlite::{Params, Row};

use super::codec::{
    batch_id, corrupt, disk_path, entry, entry_columns, folder_id, from_json, hash, node, rel_path,
    timestamp, uint,
};
use super::host::{
    DiskName, FolderRow, HistoryRow, JournalRow, Machine, Member, Mode, Shim, TrashRow,
};
use super::{Store, StoreError};

impl Store {
    /// Every row `sql` returns, each read by `read`.
    fn rows<T>(
        &self,
        sql: &str,
        params: impl Params,
        mut read: impl FnMut(&Row<'_>) -> Result<T, StoreError>,
    ) -> Result<Vec<T>, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let mut rows = stmt.query(params)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(read(row)?);
        }
        Ok(out)
    }

    /// Every folder the host has recorded, in id order.
    pub fn folders(&self) -> Result<Vec<FolderRow>, StoreError> {
        self.rows(
            "SELECT id, name, created_by, rules_json, meta_version FROM folders ORDER BY id",
            [],
            folder_row,
        )
    }

    /// The folder recorded as `id`, if there is one.
    pub fn folder(&self, id: FolderId) -> Result<Option<FolderRow>, StoreError> {
        let rows = self.rows(
            "SELECT id, name, created_by, rules_json, meta_version FROM folders WHERE id = ?1",
            [id.as_bytes()],
            folder_row,
        )?;
        Ok(rows.into_iter().next())
    }

    /// The members of `folder`, in node order.
    pub fn members(&self, folder: FolderId) -> Result<Vec<Member>, StoreError> {
        const T: &str = "members";
        self.rows(
            "SELECT node, path, mode, joined_at FROM members WHERE folder = ?1 ORDER BY node",
            [folder.as_bytes()],
            |row| {
                Ok(Member {
                    node: node(T, &row.get::<_, Vec<u8>>(0)?)?,
                    path: disk_path(row.get(1)?),
                    mode: match row.get::<_, String>(2)?.as_str() {
                        "two-way" => Mode::TwoWay,
                        other => return Err(corrupt(T, format!("mode {other:?}"))),
                    },
                    joined_at: timestamp(row.get(3)?),
                })
            },
        )
    }

    /// `folder`'s persisted parts (§11), rebuilt from its rows: `None` if
    /// the host never recorded the folder, or the engine never reported
    /// its small rest.
    pub fn parts(&self, folder: FolderId) -> Result<Option<FolderParts>, StoreError> {
        let Some(meta) = self.folder(folder)? else {
            return Ok(None);
        };
        let Some(rest) = self.rest(folder)? else {
            return Ok(None);
        };
        let members: BTreeSet<_> = self.members(folder)?.into_iter().map(|m| m.node).collect();
        Ok(Some(FolderParts {
            id: folder,
            rules: meta.rules,
            members,
            records: self.records(folder)?,
            wants: self.wants(folder)?,
            pending: self.pending(folder)?,
            held: self.held(folder)?,
            deferred: self.deferred(folder)?,
            rest,
        }))
    }

    /// The parts of every folder that has them, in folder id order: what
    /// `Engine::restore` takes.
    pub fn load(&self) -> Result<Vec<FolderParts>, StoreError> {
        let mut out = Vec::new();
        for folder in self.folders()? {
            out.extend(self.parts(folder.id)?);
        }
        Ok(out)
    }

    fn rest(&self, folder: FolderId) -> Result<Option<Rest>, StoreError> {
        const T: &str = "folder_state";
        let rows = self.rows(
            "SELECT small_rest_json FROM folder_state WHERE folder = ?1",
            [folder.as_bytes()],
            |row| from_json(T, &row.get::<_, String>(0)?),
        )?;
        Ok(rows.into_iter().next())
    }

    fn records(&self, folder: FolderId) -> Result<BTreeMap<RelPath, IndexRecord>, StoreError> {
        const T: &str = "entries";
        let sql = concat!(
            "SELECT ",
            entry_columns!(),
            ", seq FROM entries WHERE folder = ?1"
        );
        let rows = self.rows(sql, [folder.as_bytes()], |row| {
            Ok(IndexRecord {
                entry: entry(T, row, 0)?,
                seq: uint(row.get(12)?),
            })
        })?;
        Ok(rows
            .into_iter()
            .map(|r| (r.entry.path.clone(), r))
            .collect())
    }

    fn wants(&self, folder: FolderId) -> Result<BTreeMap<RelPath, Want>, StoreError> {
        const T: &str = "want";
        let rows = self.rows(
            "SELECT path, want_json FROM want WHERE folder = ?1",
            [folder.as_bytes()],
            |row| {
                let path = rel_path(T, row.get(0)?)?;
                Ok((path, from_json(T, &row.get::<_, String>(1)?)?))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    fn pending(&self, folder: FolderId) -> Result<BTreeMap<RelPath, Pending>, StoreError> {
        const T: &str = "pending";
        let rows = self.rows(
            "SELECT path, announced_record_json, exempt FROM pending WHERE folder = ?1",
            [folder.as_bytes()],
            |row| {
                let path = rel_path(T, row.get(0)?)?;
                let announced_record = match row.get::<_, Option<String>>(1)? {
                    Some(json) => Some(from_json(T, &json)?),
                    None => None,
                };
                let exempt = row.get(2)?;
                Ok((
                    path,
                    Pending {
                        announced_record,
                        exempt,
                    },
                ))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    fn held(
        &self,
        folder: FolderId,
    ) -> Result<BTreeMap<(BatchId, HeldState), HeldRow>, StoreError> {
        const T: &str = "held_items";
        let rows = self.rows(
            "SELECT batch, state, denied_at, row_json FROM held_items WHERE folder = ?1",
            [folder.as_bytes()],
            |row| {
                let batch = batch_id(T, &row.get::<_, Vec<u8>>(0)?)?;
                let state = match row.get::<_, String>(1)?.as_str() {
                    "held" => HeldState::Held,
                    "denied" => HeldState::Denied {
                        at: uint(row.get(2)?),
                    },
                    other => return Err(corrupt(T, format!("state {other:?}"))),
                };
                Ok(((batch, state), from_json(T, &row.get::<_, String>(3)?)?))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    fn deferred(&self, folder: FolderId) -> Result<BTreeMap<RelPath, Vec<Deferred>>, StoreError> {
        const T: &str = "deferred";
        let rows = self.rows(
            "SELECT path, entries_json FROM deferred WHERE folder = ?1",
            [folder.as_bytes()],
            |row| {
                let path = rel_path(T, row.get(0)?)?;
                Ok((path, from_json(T, &row.get::<_, String>(1)?)?))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    /// Every batch `folder`'s history holds (§8.5), in the order this
    /// machine recorded them.
    pub fn history(&self, folder: FolderId) -> Result<Vec<HistoryRow>, StoreError> {
        const T: &str = "batches";
        let heads = self.rows(
            "SELECT record, id, source, role, created_at, seq_low, seq_high, adds, mods, dels, \
             bytes, decision, held_reason, recorded_at \
             FROM batches WHERE folder = ?1 ORDER BY record",
            [folder.as_bytes()],
            |row| {
                let record: i64 = row.get(0)?;
                let role = match row.get::<_, String>(3)?.as_str() {
                    "sent" => BatchRole::Sent,
                    "received" => BatchRole::Received,
                    "paused" => BatchRole::Paused,
                    other => return Err(corrupt(T, format!("role {other:?}"))),
                };
                let decision = match (
                    row.get::<_, Option<String>>(11)?.as_deref(),
                    row.get::<_, Option<String>>(12)?,
                ) {
                    (None, None) => None,
                    (Some("accepted"), None) => Some(Decision::Accepted),
                    (Some("held"), Some(reason)) => Some(Decision::Held { reason }),
                    (decision, reason) => {
                        return Err(corrupt(T, format!("decision {decision:?} ({reason:?})")));
                    }
                };
                let batch = Batch {
                    id: batch_id(T, &row.get::<_, Vec<u8>>(1)?)?,
                    folder,
                    source: node(T, &row.get::<_, Vec<u8>>(2)?)?,
                    created_at: timestamp(row.get(4)?),
                    seq_low: uint(row.get(5)?),
                    seq_high: uint(row.get(6)?),
                    entries: Vec::new(),
                    summary: Summary {
                        adds: uint(row.get(7)?),
                        mods: uint(row.get(8)?),
                        dels: uint(row.get(9)?),
                        bytes: uint(row.get(10)?),
                    },
                };
                let history = HistoryRow {
                    batch,
                    role,
                    decision,
                    recorded_at: timestamp(row.get(13)?),
                };
                Ok((record, history))
            },
        )?;
        let sql = concat!(
            "SELECT ",
            entry_columns!(),
            " FROM batch_entries WHERE record = ?1 ORDER BY position"
        );
        let mut out = Vec::with_capacity(heads.len());
        for (record, mut history) in heads {
            history.batch.entries =
                self.rows(sql, [record], |row| entry("batch_entries", row, 0))?;
            out.push(history);
        }
        Ok(out)
    }

    /// Every file in `folder`'s trash (§8.4), by where it is in the trash.
    pub fn trash(&self, folder: FolderId) -> Result<Vec<TrashRow>, StoreError> {
        const T: &str = "trash";
        self.rows(
            "SELECT trashed_path, original_path, hash, trashed_at, size FROM trash \
             WHERE folder = ?1 ORDER BY trashed_path",
            [folder.as_bytes()],
            |row| {
                Ok(TrashRow {
                    trashed_path: disk_path(row.get(0)?),
                    original_path: rel_path(T, row.get(1)?)?,
                    hash: hash(T, &row.get::<_, Vec<u8>>(2)?)?,
                    trashed_at: timestamp(row.get(3)?),
                    size: uint(row.get(4)?),
                })
            },
        )
    }

    /// Every pair the mtime shim keeps for `folder` (§7.3), in path order.
    pub fn shims(&self, folder: FolderId) -> Result<Vec<Shim>, StoreError> {
        const T: &str = "mtime_shim";
        self.rows(
            "SELECT path, requested_ns, stored_ns FROM mtime_shim WHERE folder = ?1 ORDER BY path",
            [folder.as_bytes()],
            |row| {
                Ok(Shim {
                    path: rel_path(T, row.get(0)?)?,
                    requested_ns: row.get(1)?,
                    stored_ns: row.get(2)?,
                })
            },
        )
    }

    /// Every name on disk `folder` keeps (§7.3), in path order.
    pub fn disk_names(&self, folder: FolderId) -> Result<Vec<DiskName>, StoreError> {
        const T: &str = "disk_names";
        self.rows(
            "SELECT path, bytes FROM disk_names WHERE folder = ?1 ORDER BY path",
            [folder.as_bytes()],
            |row| {
                Ok(DiskName {
                    path: rel_path(T, row.get(0)?)?,
                    name: disk_path(row.get(1)?).into_os_string(),
                })
            },
        )
    }

    /// Every open row of `folder`'s commit journal (§7.5), in path order:
    /// what start-up undoes before the first scan.
    pub fn journal(&self, folder: FolderId) -> Result<Vec<JournalRow>, StoreError> {
        const T: &str = "commit_journal";
        self.rows(
            "SELECT path, displaced_to, temp_file FROM commit_journal WHERE folder = ?1 \
             ORDER BY path",
            [folder.as_bytes()],
            |row| {
                Ok(JournalRow {
                    path: rel_path(T, row.get(0)?)?,
                    displaced_to: disk_path(row.get(1)?),
                    temp_file: row.get::<_, Option<Vec<u8>>>(2)?.map(disk_path),
                })
            },
        )
    }

    /// Every machine (§5), in node order.
    pub fn machines(&self) -> Result<Vec<Machine>, StoreError> {
        const T: &str = "machines";
        self.rows(
            "SELECT node, hostname, ts_stable_id, ts_user, trusted, last_seen, delocal_version \
             FROM machines ORDER BY node",
            [],
            |row| {
                Ok(Machine {
                    node: node(T, &row.get::<_, Vec<u8>>(0)?)?,
                    hostname: row.get(1)?,
                    ts_stable_id: row.get(2)?,
                    ts_user: row.get(3)?,
                    trusted: row.get(4)?,
                    last_seen: timestamp(row.get(5)?),
                    delocal_version: row.get(6)?,
                })
            },
        )
    }
}

fn folder_row(row: &Row<'_>) -> Result<FolderRow, StoreError> {
    const T: &str = "folders";
    Ok(FolderRow {
        id: folder_id(T, &row.get::<_, Vec<u8>>(0)?)?,
        name: row.get(1)?,
        created_by: node(T, &row.get::<_, Vec<u8>>(2)?)?,
        rules: from_json(T, &row.get::<_, String>(3)?)?,
        meta_version: uint(row.get(4)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_store_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("delocal.db")).unwrap();
        assert!(store.folders().unwrap().is_empty());
        assert!(store.load().unwrap().is_empty());
        assert!(store.machines().unwrap().is_empty());
        let unknown = FolderId::from_bytes([7; 16]);
        assert_eq!(store.parts(unknown).unwrap(), None);
        assert!(store.history(unknown).unwrap().is_empty());
    }
}
