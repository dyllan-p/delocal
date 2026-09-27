//! Writing the store (DESIGN.md §11): the table write each engine action
//! makes, the host's own writes, and the group that holds one event's
//! worth of both.
//!
//! **Every persistence hook maps to a write.** [`EngineWrite::sort`] is one
//! `match` over every [`Action`], with no wildcard: an action either makes
//! a write here or is an effect the daemon performs once the writes before
//! it are durable. A new action does not compile until someone puts it on
//! one side, so a hook cannot be added to the engine and silently left
//! unwritten.
//!
//! | Action | Write |
//! |---|---|
//! | `IndexChanged` | the record's row in `entries` |
//! | `IndexRemoved` | deletes the path's row from `entries` |
//! | `WantChanged` | the path's row in `want`, or deletes it for `None` |
//! | `PendingChanged` | the path's row in `pending`, or deletes it |
//! | `HeldChanged` | the item's row in `held_items`, keyed by batch, state and the deny's arrival number, or deletes it |
//! | `DeferredChanged` | the path's row in `deferred`, or deletes it |
//! | `RestChanged` | the folder's row in `folder_state` |
//! | `RecordBatch` | a new row in `batches` and its entries in `batch_entries` (history, §8.5) |
//! | `WakeAt`, `Send`, `Fetch`, `Write`, `Remove`, `SetMeta`, `MoveToTrash`, `StatusChanged` | none: effects, handed back to the daemon |
//!
//! A row is replaced whole, never patched: each hook reports the row as it
//! now stands.

use std::path::PathBuf;

use delocal_engine::{
    Action, Batch, BatchId, BatchRole, Decision, Deferred, FolderId, HeldRow, HeldState,
    IndexRecord, NodeId, Pending, RelPath, Rest, Timestamp, Want,
};
use rusqlite::Transaction;
use rusqlite::types::ToSql;

use super::codec::{EntryColumns, disk_bytes, entry_columns, int, json, time};
use super::host::{DiskName, FolderRow, JournalRow, Machine, Member, Mode, Shim, TrashRow};
use super::{Store, StoreError};

/// The writes of one event: every persistence hook the engine returned for
/// it, and the host's writes that belong with them (§11). A group is the
/// unit of atomicity: after a crash the store holds all of a group or none
/// of it, and holds the groups it has in the order they were submitted.
#[derive(Clone, Debug)]
pub struct Group {
    at: Timestamp,
    writes: Vec<Write>,
}

#[derive(Clone, Debug)]
enum Write {
    Engine(EngineWrite),
    Host(HostWrite),
}

impl Group {
    /// An empty group for an event handled at `at`, which history records
    /// as the time a batch was recorded.
    pub fn new(at: Timestamp) -> Self {
        Self {
            at,
            writes: Vec::new(),
        }
    }

    /// Keep the write `action` makes, if it makes one; an action that makes
    /// none is an effect, and comes back for the daemon to perform once this
    /// group is durable.
    pub fn push(&mut self, action: Action) -> Option<Action> {
        match EngineWrite::sort(action) {
            Sorted::Write(write) => {
                self.writes.push(Write::Engine(write));
                None
            }
            Sorted::Effect(effect) => Some(effect),
        }
    }

    /// A write to a table the host owns, in the same group as the engine
    /// writes it belongs to (§11).
    pub fn host(&mut self, write: HostWrite) {
        self.writes.push(Write::Host(write));
    }

    /// True if the group writes nothing. Its effects still wait for every
    /// group before it: they may depend on those writes.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

/// A write to a table the host owns (§11). `Put` replaces the row with the
/// same key; `Remove` deletes it, and removing a row that is not there is
/// not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostWrite {
    /// Record a folder, or change its name, rules or metadata version.
    PutFolder(FolderRow),
    PutMember {
        folder: FolderId,
        member: Member,
    },
    RemoveMember {
        folder: FolderId,
        node: NodeId,
    },
    PutTrash {
        folder: FolderId,
        row: TrashRow,
    },
    /// Pruning (§8.4), or `restore`.
    RemoveTrash {
        folder: FolderId,
        trashed_path: PathBuf,
    },
    PutShim {
        folder: FolderId,
        shim: Shim,
    },
    RemoveShim {
        folder: FolderId,
        path: RelPath,
    },
    PutDiskName {
        folder: FolderId,
        name: DiskName,
    },
    RemoveDiskName {
        folder: FolderId,
        path: RelPath,
    },
    /// Before a commit's displacement (§7.5).
    PutJournal {
        folder: FolderId,
        row: JournalRow,
    },
    /// Once the commit's rename is durable, or its undo is.
    RemoveJournal {
        folder: FolderId,
        path: RelPath,
    },
    PutMachine(Machine),
    RemoveMachine(NodeId),
}

/// The table write of an engine action (§11): one variant per action that
/// makes one, holding what it writes.
#[derive(Clone, Debug)]
enum EngineWrite {
    Record {
        folder: FolderId,
        record: IndexRecord,
    },
    RemoveRecord {
        folder: FolderId,
        path: RelPath,
    },
    Want {
        folder: FolderId,
        path: RelPath,
        want: Option<Box<Want>>,
    },
    Pending {
        folder: FolderId,
        path: RelPath,
        row: Option<Pending>,
    },
    Held {
        folder: FolderId,
        batch: BatchId,
        state: HeldState,
        row: Option<Box<HeldRow>>,
    },
    Deferred {
        folder: FolderId,
        path: RelPath,
        entries: Option<Vec<Deferred>>,
    },
    Rest {
        folder: FolderId,
        rest: Box<Rest>,
    },
    History {
        batch: Batch,
        role: BatchRole,
        decision: Option<Decision>,
    },
}

/// An engine action as the store sees it.
enum Sorted {
    /// A persistence hook, or history: it writes this.
    Write(EngineWrite),
    /// Something for the daemon to do once the writes before it are
    /// durable.
    Effect(Action),
}

impl EngineWrite {
    /// The write `action` makes, or `action` back if it is an effect. See
    /// the module docs: every action is named, so a new one is a compile
    /// error here until it is placed.
    fn sort(action: Action) -> Sorted {
        let write = match action {
            Action::IndexChanged { folder, record } => Self::Record { folder, record },
            Action::IndexRemoved { folder, path } => Self::RemoveRecord { folder, path },
            Action::WantChanged { folder, path, want } => Self::Want { folder, path, want },
            Action::PendingChanged { folder, path, row } => Self::Pending { folder, path, row },
            Action::HeldChanged {
                folder,
                batch,
                state,
                row,
            } => Self::Held {
                folder,
                batch,
                state,
                row,
            },
            Action::DeferredChanged {
                folder,
                path,
                entries,
            } => Self::Deferred {
                folder,
                path,
                entries,
            },
            Action::RestChanged { folder, rest } => Self::Rest { folder, rest },
            Action::RecordBatch {
                batch,
                role,
                decision,
            } => Self::History {
                batch,
                role,
                decision,
            },
            effect @ (Action::WakeAt(_)
            | Action::Send { .. }
            | Action::Fetch { .. }
            | Action::Write { .. }
            | Action::Remove { .. }
            | Action::SetMeta { .. }
            | Action::MoveToTrash { .. }
            | Action::StatusChanged { .. }) => return Sorted::Effect(effect),
        };
        Sorted::Write(write)
    }
}

impl Store {
    /// Apply every write of `groups`, in order, in one transaction, and
    /// commit it: when this returns `Ok` they are all durable (§11), and
    /// when it returns `Err` none of them is. The group-commit writer calls
    /// this for each transaction; before it starts, the daemon may call it
    /// directly.
    pub fn commit(&mut self, groups: &[Group]) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for group in groups {
            for write in &group.writes {
                match write {
                    Write::Engine(write) => engine_write(&tx, group.at, write)?,
                    Write::Host(write) => host_write(&tx, write)?,
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// Run `sql` once with `params`, through the connection's statement cache.
fn run(tx: &Transaction<'_>, sql: &str, params: &[&dyn ToSql]) -> Result<(), StoreError> {
    tx.prepare_cached(sql)?.execute(params)?;
    Ok(())
}

fn engine_write(
    tx: &Transaction<'_>,
    at: Timestamp,
    write: &EngineWrite,
) -> Result<(), StoreError> {
    match write {
        EngineWrite::Record { folder, record } => {
            let columns = EntryColumns::of(&record.entry);
            let seq = int(record.seq);
            let mut params: Vec<&dyn ToSql> = vec![folder.as_bytes()];
            params.extend(columns.params());
            params.push(&seq);
            run(
                tx,
                concat!(
                    "INSERT OR REPLACE INTO entries (folder, ",
                    entry_columns!(),
                    ", seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
                ),
                &params,
            )
        }
        EngineWrite::RemoveRecord { folder, path } => run(
            tx,
            "DELETE FROM entries WHERE folder = ?1 AND path = ?2",
            &[folder.as_bytes(), &path.as_str()],
        ),
        EngineWrite::Want { folder, path, want } => match want {
            Some(want) => run(
                tx,
                "INSERT OR REPLACE INTO want (folder, path, want_json) VALUES (?1, ?2, ?3)",
                &[folder.as_bytes(), &path.as_str(), &json("want", want)?],
            ),
            None => run(
                tx,
                "DELETE FROM want WHERE folder = ?1 AND path = ?2",
                &[folder.as_bytes(), &path.as_str()],
            ),
        },
        EngineWrite::Pending { folder, path, row } => match row {
            Some(row) => {
                let announced = match &row.announced_record {
                    Some(record) => Some(json("pending", record)?),
                    None => None,
                };
                run(
                    tx,
                    "INSERT OR REPLACE INTO pending (folder, path, announced_record_json, exempt) \
                     VALUES (?1, ?2, ?3, ?4)",
                    &[folder.as_bytes(), &path.as_str(), &announced, &row.exempt],
                )
            }
            None => run(
                tx,
                "DELETE FROM pending WHERE folder = ?1 AND path = ?2",
                &[folder.as_bytes(), &path.as_str()],
            ),
        },
        EngineWrite::Held {
            folder,
            batch,
            state,
            row,
        } => {
            let (state, denied_at) = match state {
                HeldState::Held => ("held", 0),
                HeldState::Denied { at } => ("denied", int(*at)),
            };
            match row {
                Some(row) => run(
                    tx,
                    "INSERT OR REPLACE INTO held_items (folder, batch, state, denied_at, row_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    &[
                        folder.as_bytes(),
                        batch.as_bytes(),
                        &state,
                        &denied_at,
                        &json("held_items", row)?,
                    ],
                ),
                None => run(
                    tx,
                    "DELETE FROM held_items \
                     WHERE folder = ?1 AND batch = ?2 AND state = ?3 AND denied_at = ?4",
                    &[folder.as_bytes(), batch.as_bytes(), &state, &denied_at],
                ),
            }
        }
        EngineWrite::Deferred {
            folder,
            path,
            entries,
        } => match entries {
            Some(entries) => run(
                tx,
                "INSERT OR REPLACE INTO deferred (folder, path, entries_json) VALUES (?1, ?2, ?3)",
                &[
                    folder.as_bytes(),
                    &path.as_str(),
                    &json("deferred", entries)?,
                ],
            ),
            None => run(
                tx,
                "DELETE FROM deferred WHERE folder = ?1 AND path = ?2",
                &[folder.as_bytes(), &path.as_str()],
            ),
        },
        EngineWrite::Rest { folder, rest } => run(
            tx,
            "INSERT OR REPLACE INTO folder_state (folder, small_rest_json) VALUES (?1, ?2)",
            &[folder.as_bytes(), &json("folder_state", rest)?],
        ),
        EngineWrite::History {
            batch,
            role,
            decision,
        } => history(tx, at, batch, *role, decision.as_ref()),
    }
}

/// One `RecordBatch` into history (§8.5): a row in `batches`, and one in
/// `batch_entries` per entry, in the batch's order.
fn history(
    tx: &Transaction<'_>,
    at: Timestamp,
    batch: &Batch,
    role: BatchRole,
    decision: Option<&Decision>,
) -> Result<(), StoreError> {
    let role = match role {
        BatchRole::Sent => "sent",
        BatchRole::Received => "received",
        BatchRole::Paused => "paused",
    };
    let (decision, held_reason) = match decision {
        None => (None, None),
        Some(Decision::Accepted) => (Some("accepted"), None),
        Some(Decision::Held { reason }) => (Some("held"), Some(reason.as_str())),
    };
    let s = &batch.summary;
    run(
        tx,
        "INSERT INTO batches (id, folder, source, role, created_at, seq_low, seq_high, adds, \
         mods, dels, bytes, decision, held_reason, recorded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        &[
            batch.id.as_bytes(),
            batch.folder.as_bytes(),
            batch.source.as_bytes(),
            &role,
            &time(batch.created_at),
            &int(batch.seq_low),
            &int(batch.seq_high),
            &int(s.adds),
            &int(s.mods),
            &int(s.dels),
            &int(s.bytes),
            &decision,
            &held_reason,
            &time(at),
        ],
    )?;
    let record = tx.last_insert_rowid();
    for (position, entry) in (0_i64..).zip(&batch.entries) {
        let columns = EntryColumns::of(entry);
        let mut params: Vec<&dyn ToSql> = vec![&record, &position];
        params.extend(columns.params());
        run(
            tx,
            concat!(
                "INSERT INTO batch_entries (record, position, ",
                entry_columns!(),
                ") VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            &params,
        )?;
    }
    Ok(())
}

fn host_write(tx: &Transaction<'_>, write: &HostWrite) -> Result<(), StoreError> {
    match write {
        // An upsert, not INSERT OR REPLACE: REPLACE deletes the old row
        // first, and every folder's rows reference this one.
        HostWrite::PutFolder(row) => run(
            tx,
            "INSERT INTO folders (id, name, created_by, rules_json, meta_version) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, \
             created_by = excluded.created_by, rules_json = excluded.rules_json, \
             meta_version = excluded.meta_version",
            &[
                row.id.as_bytes(),
                &row.name,
                row.created_by.as_bytes(),
                &json("folders", &row.rules)?,
                &int(row.meta_version),
            ],
        ),
        HostWrite::PutMember { folder, member } => {
            let mode = match member.mode {
                Mode::TwoWay => "two-way",
            };
            run(
                tx,
                "INSERT OR REPLACE INTO members (folder, node, path, mode, joined_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                &[
                    folder.as_bytes(),
                    member.node.as_bytes(),
                    &disk_bytes(&member.path),
                    &mode,
                    &time(member.joined_at),
                ],
            )
        }
        HostWrite::RemoveMember { folder, node } => run(
            tx,
            "DELETE FROM members WHERE folder = ?1 AND node = ?2",
            &[folder.as_bytes(), node.as_bytes()],
        ),
        HostWrite::PutTrash { folder, row } => run(
            tx,
            "INSERT OR REPLACE INTO trash \
             (folder, trashed_path, original_path, hash, trashed_at, size) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            &[
                folder.as_bytes(),
                &disk_bytes(&row.trashed_path),
                &row.original_path.as_str(),
                row.hash.as_bytes(),
                &time(row.trashed_at),
                &int(row.size),
            ],
        ),
        HostWrite::RemoveTrash {
            folder,
            trashed_path,
        } => run(
            tx,
            "DELETE FROM trash WHERE folder = ?1 AND trashed_path = ?2",
            &[folder.as_bytes(), &disk_bytes(trashed_path)],
        ),
        HostWrite::PutShim { folder, shim } => run(
            tx,
            "INSERT OR REPLACE INTO mtime_shim (folder, path, requested_ns, stored_ns) \
             VALUES (?1, ?2, ?3, ?4)",
            &[
                folder.as_bytes(),
                &shim.path.as_str(),
                &shim.requested_ns,
                &shim.stored_ns,
            ],
        ),
        HostWrite::RemoveShim { folder, path } => run(
            tx,
            "DELETE FROM mtime_shim WHERE folder = ?1 AND path = ?2",
            &[folder.as_bytes(), &path.as_str()],
        ),
        HostWrite::PutDiskName { folder, name } => run(
            tx,
            "INSERT OR REPLACE INTO disk_names (folder, path, bytes) VALUES (?1, ?2, ?3)",
            &[
                folder.as_bytes(),
                &name.path.as_str(),
                &disk_bytes(name.name.as_ref()),
            ],
        ),
        HostWrite::RemoveDiskName { folder, path } => run(
            tx,
            "DELETE FROM disk_names WHERE folder = ?1 AND path = ?2",
            &[folder.as_bytes(), &path.as_str()],
        ),
        HostWrite::PutJournal { folder, row } => run(
            tx,
            "INSERT OR REPLACE INTO commit_journal (folder, path, displaced_to, temp_file) \
             VALUES (?1, ?2, ?3, ?4)",
            &[
                folder.as_bytes(),
                &row.path.as_str(),
                &disk_bytes(&row.displaced_to),
                &row.temp_file.as_deref().map(disk_bytes),
            ],
        ),
        HostWrite::RemoveJournal { folder, path } => run(
            tx,
            "DELETE FROM commit_journal WHERE folder = ?1 AND path = ?2",
            &[folder.as_bytes(), &path.as_str()],
        ),
        HostWrite::PutMachine(m) => run(
            tx,
            "INSERT OR REPLACE INTO machines \
             (node, hostname, ts_stable_id, ts_user, trusted, last_seen, delocal_version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[
                m.node.as_bytes(),
                &m.hostname,
                &m.ts_stable_id,
                &m.ts_user,
                &m.trusted,
                &time(m.last_seen),
                &m.delocal_version,
            ],
        ),
        HostWrite::RemoveMachine(node) => run(
            tx,
            "DELETE FROM machines WHERE node = ?1",
            &[node.as_bytes()],
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use delocal_engine::{
        Action, BatchDecision, BatchRole, ContentHash, Decision, Displace, FolderParts,
        FolderStatus, HeldState, Kind, Observed, Outbound, Rules, Version,
    };

    use super::super::HistoryRow;
    use super::super::sample::*;
    use super::*;

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("delocal.db")).unwrap();
        (dir, store)
    }

    /// A group that records folder 1 with members 1 and 2.
    fn joined(f: FolderId) -> Group {
        let mut g = Group::new(at(1));
        g.host(HostWrite::PutFolder(folder_row(f)));
        for n in [1, 2] {
            g.host(HostWrite::PutMember {
                folder: f,
                member: member(n),
            });
        }
        g
    }

    fn push(g: &mut Group, action: Action) {
        assert_eq!(g.push(action), None, "a hook came back as an effect");
    }

    #[test]
    fn every_hook_writes_its_row_and_a_none_removes_it() {
        let (_dir, mut store) = open();
        let f = folder(1);
        let mut g = joined(f);
        for (p, seq) in [("a", 1), ("b", u64::MAX), ("c", 3)] {
            push(
                &mut g,
                Action::IndexChanged {
                    folder: f,
                    record: record(p, seq),
                },
            );
        }
        push(
            &mut g,
            Action::IndexRemoved {
                folder: f,
                path: path("c"),
            },
        );
        for p in ["a", "b"] {
            push(
                &mut g,
                Action::WantChanged {
                    folder: f,
                    path: path(p),
                    want: Some(Box::new(want(p))),
                },
            );
            push(
                &mut g,
                Action::DeferredChanged {
                    folder: f,
                    path: path(p),
                    entries: Some(deferred(p)),
                },
            );
        }
        for (p, announced) in [("a", true), ("b", false)] {
            push(
                &mut g,
                Action::PendingChanged {
                    folder: f,
                    path: path(p),
                    row: Some(pending(announced)),
                },
            );
        }
        // One batch held, denied, and held again: three rows.
        for state in [
            HeldState::Held,
            HeldState::Denied { at: 5 },
            HeldState::Denied { at: u64::MAX },
        ] {
            push(
                &mut g,
                Action::HeldChanged {
                    folder: f,
                    batch: batch_id(9),
                    state,
                    row: Some(Box::new(held_row(9))),
                },
            );
        }
        push(
            &mut g,
            Action::RestChanged {
                folder: f,
                rest: Box::new(rest()),
            },
        );
        store.commit(&[g]).unwrap();

        let mut expected = FolderParts {
            id: f,
            rules: folder_row(f).rules,
            members: [node(1), node(2)].into_iter().collect(),
            records: [("a", 1), ("b", u64::MAX)]
                .map(|(p, seq)| (path(p), record(p, seq)))
                .into_iter()
                .collect(),
            wants: ["a", "b"].map(|p| (path(p), want(p))).into_iter().collect(),
            pending: [("a", true), ("b", false)]
                .map(|(p, a)| (path(p), pending(a)))
                .into_iter()
                .collect(),
            held: [
                HeldState::Held,
                HeldState::Denied { at: 5 },
                HeldState::Denied { at: u64::MAX },
            ]
            .map(|s| ((batch_id(9), s), held_row(9)))
            .into_iter()
            .collect(),
            deferred: ["a", "b"]
                .map(|p| (path(p), deferred(p)))
                .into_iter()
                .collect(),
            rest: rest(),
        };
        assert_eq!(store.parts(f).unwrap().as_ref(), Some(&expected));
        assert_eq!(store.load().unwrap(), [expected.clone()]);

        // Every `None` removes its row, and only that row.
        let mut g = Group::new(at(2));
        push(
            &mut g,
            Action::IndexRemoved {
                folder: f,
                path: path("a"),
            },
        );
        push(
            &mut g,
            Action::WantChanged {
                folder: f,
                path: path("a"),
                want: None,
            },
        );
        push(
            &mut g,
            Action::PendingChanged {
                folder: f,
                path: path("b"),
                row: None,
            },
        );
        push(
            &mut g,
            Action::HeldChanged {
                folder: f,
                batch: batch_id(9),
                state: HeldState::Denied { at: 5 },
                row: None,
            },
        );
        push(
            &mut g,
            Action::DeferredChanged {
                folder: f,
                path: path("b"),
                entries: None,
            },
        );
        // Removing what is not there is not an error.
        push(
            &mut g,
            Action::WantChanged {
                folder: f,
                path: path("never"),
                want: None,
            },
        );
        let mut changed = rest();
        changed.seq = u64::MAX;
        push(
            &mut g,
            Action::RestChanged {
                folder: f,
                rest: Box::new(changed.clone()),
            },
        );
        store.commit(&[g]).unwrap();
        expected.records.remove(&path("a"));
        expected.wants.remove(&path("a"));
        expected.pending.remove(&path("b"));
        expected
            .held
            .remove(&(batch_id(9), HeldState::Denied { at: 5 }));
        expected.deferred.remove(&path("b"));
        expected.rest = changed;
        assert_eq!(store.parts(f).unwrap(), Some(expected));
    }

    #[test]
    fn a_denied_row_is_keyed_by_the_arrival_number_its_deny_took() {
        let (_dir, mut store) = open();
        let f = folder(1);
        let mut g = joined(f);
        for state in [HeldState::Held, HeldState::Denied { at: 7 }] {
            push(
                &mut g,
                Action::HeldChanged {
                    folder: f,
                    batch: batch_id(9),
                    state,
                    row: Some(Box::new(held_row(9))),
                },
            );
        }
        store.commit(&[g]).unwrap();
        let mut stmt = store
            .conn
            .prepare("SELECT state, denied_at FROM held_items ORDER BY state DESC")
            .unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, [("held".to_owned(), 0), ("denied".to_owned(), 7)]);
    }

    #[test]
    fn record_batch_appends_to_history_in_order() {
        let (_dir, mut store) = open();
        let f = folder(1);
        store.commit(&[joined(f)]).unwrap();
        let records = [
            (BatchRole::Paused, None, 10),
            (BatchRole::Sent, None, 11),
            (
                BatchRole::Received,
                Some(Decision::Held {
                    reason: "612 deletes = 51% of folder".to_owned(),
                }),
                12,
            ),
            // The same batch again, as a lossy transport can deliver it.
            (BatchRole::Received, Some(Decision::Accepted), 13),
        ];
        let groups: Vec<Group> = records
            .iter()
            .map(|(role, decision, t)| {
                let mut g = Group::new(at(*t));
                push(
                    &mut g,
                    Action::RecordBatch {
                        batch: batch(f, 4),
                        role: *role,
                        decision: decision.clone(),
                    },
                );
                g
            })
            .collect();
        store.commit(&groups).unwrap();
        let expected: Vec<HistoryRow> = records
            .into_iter()
            .map(|(role, decision, t)| HistoryRow {
                batch: batch(f, 4),
                role,
                decision,
                recorded_at: at(t),
            })
            .collect();
        assert_eq!(store.history(f).unwrap(), expected);
        assert!(store.history(folder(2)).unwrap().is_empty());
    }

    #[test]
    fn effects_come_back_and_write_nothing() {
        let f = folder(1);
        let observed = Observed {
            kind: Kind::File,
            size: 1,
            mtime_ns: 2,
            exec: false,
            hash: hash(3),
        };
        let effects = [
            Action::WakeAt(at(1)),
            Action::Send {
                to: node(2),
                payload: Outbound::Decision(BatchDecision {
                    batch: batch_id(1),
                    folder: f,
                    decision: Decision::Accepted,
                    seq_high: 4,
                }),
            },
            Action::Fetch {
                folder: f,
                path: path("a"),
                version: Version::empty(),
                hash: ContentHash::EMPTY,
                size: 5,
                from: node(2),
            },
            Action::Write {
                folder: f,
                path: path("a"),
                entry: entry("a", 1),
                expected: Some(observed.clone()),
                displace: Displace::ConflictCopy(path("a.conflict")),
            },
            Action::Remove {
                folder: f,
                path: path("a"),
                expected: Some(observed),
                displace: Displace::Trash,
            },
            Action::SetMeta {
                folder: f,
                path: path("a"),
                expected: None,
                mtime_ns: 6,
                exec: true,
            },
            Action::MoveToTrash {
                folder: f,
                path: path("a"),
            },
            Action::StatusChanged {
                folder: f,
                status: FolderStatus::AlreadyJoined,
            },
        ];
        let mut g = Group::new(at(0));
        for effect in effects {
            assert_eq!(g.push(effect.clone()), Some(effect));
        }
        assert!(g.is_empty());
    }

    #[test]
    fn host_rows_round_trip_are_replaced_by_key_and_removed() {
        let (_dir, mut store) = open();
        let f = folder(1);
        let mut g = joined(f);
        g.host(HostWrite::PutTrash {
            folder: f,
            row: trash_row("t1"),
        });
        g.host(HostWrite::PutTrash {
            folder: f,
            row: trash_row("t2"),
        });
        g.host(HostWrite::PutShim {
            folder: f,
            shim: shim("s"),
        });
        g.host(HostWrite::PutDiskName {
            folder: f,
            name: disk_name("d"),
        });
        g.host(HostWrite::PutJournal {
            folder: f,
            row: journal_row("j1", true),
        });
        g.host(HostWrite::PutJournal {
            folder: f,
            row: journal_row("j2", false),
        });
        g.host(HostWrite::PutMachine(machine(3)));
        g.host(HostWrite::PutMachine(machine(4)));
        store.commit(&[g]).unwrap();

        assert_eq!(store.folders().unwrap(), [folder_row(f)]);
        assert_eq!(store.members(f).unwrap(), [member(1), member(2)]);
        assert_eq!(store.trash(f).unwrap(), [trash_row("t1"), trash_row("t2")]);
        assert_eq!(store.shims(f).unwrap(), [shim("s")]);
        assert_eq!(store.disk_names(f).unwrap(), [disk_name("d")]);
        assert_eq!(
            store.journal(f).unwrap(),
            [journal_row("j1", true), journal_row("j2", false)]
        );
        assert_eq!(store.machines().unwrap(), [machine(3), machine(4)]);
        // No small rest yet: the engine never reported the folder.
        assert_eq!(store.parts(f).unwrap(), None);

        // A put with the same key replaces; a remove deletes one row.
        let mut renamed = folder_row(f);
        renamed.name = "Notes".to_owned();
        renamed.rules = Rules::default();
        let mut moved = member(2);
        moved.path = "/elsewhere".into();
        let mut resized = trash_row("t1");
        resized.size = 1;
        let mut g = Group::new(at(2));
        g.host(HostWrite::PutFolder(renamed.clone()));
        g.host(HostWrite::PutMember {
            folder: f,
            member: moved.clone(),
        });
        g.host(HostWrite::RemoveMember {
            folder: f,
            node: node(1),
        });
        g.host(HostWrite::PutTrash {
            folder: f,
            row: resized.clone(),
        });
        g.host(HostWrite::RemoveTrash {
            folder: f,
            trashed_path: trash_row("t2").trashed_path,
        });
        g.host(HostWrite::RemoveShim {
            folder: f,
            path: path("s"),
        });
        g.host(HostWrite::RemoveDiskName {
            folder: f,
            path: path("d"),
        });
        g.host(HostWrite::RemoveJournal {
            folder: f,
            path: path("j1"),
        });
        g.host(HostWrite::RemoveMachine(node(3)));
        store.commit(&[g]).unwrap();

        assert_eq!(store.folders().unwrap(), [renamed]);
        assert_eq!(store.members(f).unwrap(), [moved]);
        assert_eq!(store.trash(f).unwrap(), [resized]);
        assert!(store.shims(f).unwrap().is_empty());
        assert!(store.disk_names(f).unwrap().is_empty());
        assert_eq!(store.journal(f).unwrap(), [journal_row("j2", false)]);
        assert_eq!(store.machines().unwrap(), [machine(4)]);
    }

    #[test]
    fn a_rules_change_keeps_every_row_of_the_folder() {
        let (_dir, mut store) = open();
        let f = folder(1);
        let mut g = joined(f);
        push(
            &mut g,
            Action::IndexChanged {
                folder: f,
                record: record("a", 1),
            },
        );
        push(
            &mut g,
            Action::RestChanged {
                folder: f,
                rest: Box::new(rest()),
            },
        );
        store.commit(&[g]).unwrap();
        let mut row = folder_row(f);
        row.rules.hold_pct = 1;
        let mut g = Group::new(at(2));
        g.host(HostWrite::PutFolder(row.clone()));
        store.commit(&[g]).unwrap();
        let parts = store.parts(f).unwrap().unwrap();
        assert_eq!(parts.rules, row.rules);
        assert_eq!(parts.records, BTreeMap::from([(path("a"), record("a", 1))]));
    }

    #[test]
    fn a_write_for_a_folder_never_recorded_fails_the_whole_transaction() {
        let (_dir, mut store) = open();
        let mut first = Group::new(at(1));
        first.host(HostWrite::PutMachine(machine(3)));
        let mut second = Group::new(at(2));
        push(
            &mut second,
            Action::IndexChanged {
                folder: folder(1),
                record: record("a", 1),
            },
        );
        let err = store.commit(&[first, second]).unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)), "{err}");
        // The machine in the first group went too: a transaction is whole.
        assert!(store.machines().unwrap().is_empty());
    }

    /// Two engines, `a` and `b`; `b`'s hook actions go through groups into
    /// the store, and after every event `b` handles, the parts reloaded
    /// from the store must be `b`'s own.
    struct Pair {
        a: delocal_engine::Engine,
        b: delocal_engine::Engine,
        store: Store,
        folder: FolderId,
        seconds: i64,
        fresh: u8,
        /// The writing actions `b` has returned, by name.
        seen: std::collections::BTreeSet<&'static str>,
    }

    /// The name of an action that writes, or `None` for an effect.
    fn writes(action: &Action) -> Option<&'static str> {
        Some(match action {
            Action::IndexChanged { .. } => "IndexChanged",
            Action::IndexRemoved { .. } => "IndexRemoved",
            Action::WantChanged { .. } => "WantChanged",
            Action::PendingChanged { .. } => "PendingChanged",
            Action::HeldChanged { .. } => "HeldChanged",
            Action::DeferredChanged { .. } => "DeferredChanged",
            Action::RestChanged { .. } => "RestChanged",
            Action::RecordBatch { .. } => "RecordBatch",
            _ => return None,
        })
    }

    impl Pair {
        fn now(&self) -> Timestamp {
            at(self.seconds * 1_000_000_000)
        }

        fn fresh(&mut self) -> BatchId {
            self.fresh += 1;
            BatchId::from_bytes([self.fresh; 16])
        }

        fn a(&mut self, event: delocal_engine::Event) -> Vec<Action> {
            let now = self.now();
            self.a.handle(now, event)
        }

        /// `b` handles `event` with `host` written beside its hooks; the
        /// effects come back.
        fn b_with(&mut self, host: Vec<HostWrite>, event: delocal_engine::Event) -> Vec<Action> {
            let now = self.now();
            let mut group = Group::new(now);
            for write in host {
                group.host(write);
            }
            let effects: Vec<Action> = self
                .b
                .handle(now, event)
                .into_iter()
                .inspect(|action| self.seen.extend(writes(action)))
                .filter_map(|action| group.push(action))
                .collect();
            self.store.commit(&[group]).unwrap();
            let parts = self
                .b
                .folder(self.folder)
                .map(delocal_engine::FolderState::parts);
            assert_eq!(self.store.parts(self.folder).unwrap(), parts);
            effects
        }

        fn b(&mut self, event: delocal_engine::Event) -> Vec<Action> {
            self.b_with(Vec::new(), event)
        }

        /// Hand every batch `a` sent to `b`.
        fn deliver(&mut self, from_a: Vec<Action>) -> Vec<Action> {
            let mut effects = Vec::new();
            for action in from_a {
                if let Action::Send {
                    payload: Outbound::Batch(batch),
                    ..
                } = action
                {
                    effects.extend(self.b(delocal_engine::Event::BatchReceived {
                        from: node(1),
                        batch,
                    }));
                }
            }
            effects
        }

        /// A tick on `a` after the batch window, with a fresh batch id.
        fn tick_a(&mut self) -> Vec<Action> {
            self.seconds += 11;
            let fresh_batch_id = self.fresh();
            self.a(delocal_engine::Event::Tick { fresh_batch_id })
        }

        fn tick_b(&mut self) -> Vec<Action> {
            self.seconds += 11;
            let fresh_batch_id = self.fresh();
            self.b(delocal_engine::Event::Tick { fresh_batch_id })
        }
    }

    fn file(n: u8) -> Observed {
        Observed {
            kind: Kind::File,
            size: u64::from(n),
            mtime_ns: 1_000 + i64::from(n),
            exec: false,
            hash: hash(n),
        }
    }

    #[test]
    fn a_real_engines_hooks_reload_as_its_parts() {
        use delocal_engine::{
            ApplyOutcome, Engine, Event, FetchReport, HostName, NodeConfig, ScanState, Tier,
        };
        let (_dir, store) = open();
        let f = folder(1);
        let rules = Rules {
            hold_count: 2,
            ..Rules::default()
        };
        let config = |n: u8, host: &str| NodeConfig {
            node_id: node(n),
            author_host: HostName::new(host).unwrap(),
        };
        let mut pair = Pair {
            a: Engine::new(config(1, "one")),
            b: Engine::new(config(2, "two")),
            store,
            folder: f,
            seconds: 1,
            fresh: 0,
            seen: std::collections::BTreeSet::new(),
        };
        let joined = || Event::FolderJoined {
            folder: f,
            rules: rules.clone(),
            members: vec![node(1), node(2)],
        };
        let mut row = folder_row(f);
        row.rules = rules.clone();
        let host = vec![
            HostWrite::PutFolder(row),
            HostWrite::PutMember {
                folder: f,
                member: member(1),
            },
            HostWrite::PutMember {
                folder: f,
                member: member(2),
            },
        ];
        pair.a(joined());
        pair.b_with(host, joined());
        pair.a(Event::PeerConnected {
            peer: node(2),
            tier: Tier::Lan,
        });
        pair.b(Event::PeerConnected {
            peer: node(1),
            tier: Tier::Lan,
        });

        // `a` announces twelve files; `b` wants them and lands two.
        for n in 1..=12 {
            pair.a(Event::Scanned {
                folder: f,
                path: path(&format!("f{n:02}")),
                state: ScanState::Observed(file(n)),
            });
        }
        let sent = pair.tick_a();
        let effects = pair.deliver(sent);
        let fetches: Vec<(RelPath, delocal_engine::ContentHash, Version)> = effects
            .into_iter()
            .filter_map(|action| match action {
                Action::Fetch {
                    path,
                    hash,
                    version,
                    ..
                } => Some((path, hash, version)),
                _ => None,
            })
            .collect();
        // At most four at once from one peer (§7.5).
        assert_eq!(fetches.len(), 4);
        let landed: Vec<RelPath> = fetches.iter().take(2).map(|(p, ..)| p.clone()).collect();
        // The third finds its path changed underneath and is deferred.
        let outcomes = [
            ApplyOutcome::Ok,
            ApplyOutcome::Ok,
            ApplyOutcome::ChangedUnderneath,
        ];
        for ((path, hash, version), outcome) in fetches.into_iter().zip(outcomes) {
            pair.b(Event::Fetched {
                folder: f,
                path: path.clone(),
                hash,
                version: version.clone(),
                outcome: FetchReport::Ok,
            });
            pair.b(Event::Applied {
                folder: f,
                path,
                version,
                outcome,
            });
        }
        assert!(!pair.store.parts(f).unwrap().unwrap().deferred.is_empty());

        // A local change of `b`'s own, pending until its window closes.
        pair.b(Event::Scanned {
            folder: f,
            path: path("mine"),
            state: ScanState::Observed(file(9)),
        });
        pair.tick_b();

        // `a` deletes the two `b` has: 2 of 12 passes `a`'s own brake, but
        // 2 of `b`'s 3 does not, so `b` holds the batch, then denies it.
        for path in landed {
            pair.a(Event::Scanned {
                folder: f,
                path,
                state: ScanState::Absent,
            });
        }
        let sent = pair.tick_a();
        pair.deliver(sent);
        let parts = pair.store.parts(f).unwrap().unwrap();
        let held = parts
            .held
            .keys()
            .find(|(_, state)| *state == HeldState::Held)
            .map(|(batch, _)| *batch)
            .expect("b held the deletes");
        pair.b(Event::Deny {
            folder: f,
            batch: held,
        });
        let parts = pair.store.parts(f).unwrap().unwrap();
        assert!(
            parts
                .held
                .keys()
                .any(|(_, state)| matches!(state, HeldState::Denied { .. })),
            "the deny's row is kept until its bumps are announced"
        );
        assert!(!parts.pending.is_empty(), "the deny's bumps are pending");
        pair.tick_b();

        // `b` deletes its own file and one it landed, and adds another: its
        // own brake pauses the folder, and `revert` removes the add peers
        // never saw.
        for path in [path("mine"), path("f01")] {
            pair.b(Event::Scanned {
                folder: f,
                path,
                state: ScanState::Absent,
            });
        }
        pair.b(Event::Scanned {
            folder: f,
            path: path("new"),
            state: ScanState::Observed(file(10)),
        });
        pair.tick_b();
        assert!(pair.store.parts(f).unwrap().unwrap().rest.paused.is_some());
        pair.b(Event::Revert { folder: f });
        let parts = pair.store.parts(f).unwrap().unwrap();
        assert!(!parts.records.contains_key(&path("new")));

        // What a restart hands the engine is what the engine had.
        let loaded = pair.store.load().unwrap();
        let own = vec![pair.b.folder(f).unwrap().parts()];
        let now = pair.now();
        assert_eq!(
            Engine::restore(config(2, "two"), loaded, now),
            Engine::restore(config(2, "two"), own, now)
        );
        assert!(!pair.store.history(f).unwrap().is_empty());
        // The scenario reaches every action that writes.
        assert_eq!(
            pair.seen.into_iter().collect::<Vec<_>>(),
            [
                "DeferredChanged",
                "HeldChanged",
                "IndexChanged",
                "IndexRemoved",
                "PendingChanged",
                "RecordBatch",
                "RestChanged",
                "WantChanged",
            ]
        );
    }
}
