//! The invariants of DESIGN.md §14.1, checked at the end of a run once the
//! world is quiescent, plus the two extra checks: no winner-rule fallback
//! ever fired, and a paused folder never sent an unannounced record (that
//! one is checked as batches are sent, in `sim.rs`).

use std::collections::{BTreeMap, BTreeSet};

use delocal_engine::conflict::{Side, conflict_copy_name, winner};
use delocal_engine::{ContentHash, Entry, Kind, RelPath};

use crate::sim::{Failure, Sim};

/// Run every end-of-run invariant.
pub fn check_all(sim: &Sim) -> Result<(), Failure> {
    no_fallbacks(sim)?;
    i1_convergence(sim)?;
    i2_no_loss(sim)?;
    i3_no_resurrection(sim)?;
    i4_bounded_conflicts(sim)?;
    Ok(())
}

/// What a node holds at a path, for I1.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Live {
    kind: Kind,
    hash: ContentHash,
    exec: bool,
}

fn live_index(sim: &Sim, id: delocal_engine::NodeId) -> Option<BTreeMap<RelPath, Live>> {
    let f = sim.engine(id)?.folder(sim.folder_id())?;
    Some(
        f.index()
            .live_records()
            .map(|r| {
                (
                    r.entry.path.clone(),
                    Live {
                        kind: r.entry.kind,
                        hash: r.entry.hash,
                        exec: r.entry.exec && r.entry.kind == Kind::File,
                    },
                )
            })
            .collect(),
    )
}

fn live_fs(sim: &Sim, id: delocal_engine::NodeId) -> BTreeMap<RelPath, Live> {
    sim.fs(id)
        .map(|fs| {
            fs.iter()
                .map(|(p, f)| {
                    (
                        p.clone(),
                        Live {
                            kind: f.kind,
                            hash: if f.kind == Kind::Dir {
                                ContentHash::EMPTY
                            } else {
                                crate::sim::hash_bytes(&f.content)
                            },
                            exec: f.exec && f.kind == Kind::File,
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn diff(a: &BTreeMap<RelPath, Live>, b: &BTreeMap<RelPath, Live>) -> String {
    let mut out = Vec::new();
    for (p, l) in a {
        match b.get(p) {
            None => out.push(format!("{p}: {l:?} vs absent")),
            Some(r) if r != l => out.push(format!("{p}: {l:?} vs {r:?}")),
            _ => {}
        }
    }
    for p in b.keys() {
        if !a.contains_key(p) {
            out.push(format!("{p}: absent vs {:?}", b[p]));
        }
    }
    out.truncate(8);
    out.join("; ")
}

/// Extra: rule 5 of the winner rule never decided anything.
fn no_fallbacks(sim: &Sim) -> Result<(), Failure> {
    for id in sim.node_ids() {
        if let Some(f) = sim.engine(*id).and_then(|e| e.folder(sim.folder_id()))
            && f.winner_fallbacks() > 0
        {
            return Err(sim.failure(
                "winner fallback",
                format!(
                    "{}: {} conflict(s) decided by rule 5",
                    id.short(),
                    f.winner_fallbacks()
                ),
            ));
        }
    }
    Ok(())
}

/// I1: every node's live index is identical, and every filesystem matches
/// its own index.
fn i1_convergence(sim: &Sim) -> Result<(), Failure> {
    let ids = sim.node_ids();
    let Some(first) = ids.first() else {
        return Ok(());
    };
    let Some(reference) = live_index(sim, *first) else {
        return Err(sim.failure(
            "I1 convergence",
            format!("{} has no engine at the end", first.short()),
        ));
    };
    for id in ids {
        let Some(mine) = live_index(sim, *id) else {
            return Err(sim.failure(
                "I1 convergence",
                format!("{} has no engine at the end", id.short()),
            ));
        };
        if mine != reference {
            return Err(sim.failure(
                "I1 convergence",
                format!(
                    "index of {} differs from {}: {}",
                    id.short(),
                    first.short(),
                    diff(&mine, &reference)
                ),
            ));
        }
        let disk = live_fs(sim, *id);
        if disk != mine {
            return Err(sim.failure(
                "I1 convergence",
                format!(
                    "{}: filesystem differs from its index: {}",
                    id.short(),
                    diff(&disk, &mine)
                ),
            ));
        }
    }
    Ok(())
}

/// I2: sync never loses content. Every content a node adopted through
/// sync is still in that node's folder or trash at the end, unless the
/// node's own user edited or deleted the path afterwards, which §8.6 does
/// not protect. (Announced content the author overwrote before anyone
/// fetched it is likewise unprotected by design.)
fn i2_no_loss(sim: &Sim) -> Result<(), Failure> {
    let _ = sim.announced(); // kept for statistics
    for id in sim.node_ids() {
        let mut present: BTreeSet<ContentHash> = BTreeSet::new();
        if let Some(fs) = sim.fs(*id) {
            for f in fs.values() {
                if f.kind != Kind::Dir {
                    present.insert(crate::sim::hash_bytes(&f.content));
                }
            }
        }
        present.extend(sim.trash(*id).iter().copied());
        let (synced, local_edit_at) = sim.synced(*id);
        for (hash, path, at) in synced {
            if present.contains(hash) {
                continue;
            }
            if local_edit_at.get(path).is_some_and(|t| t > at) {
                continue; // the user's own later edit; not sync's loss
            }
            return Err(sim.failure(
                "I2 no loss",
                format!(
                    "{}: content {} adopted at {path} (at {:?}) is in neither its folder nor its trash and its user never touched the path since",
                    id.short(), hash.short(), at
                ),
            ));
        }
    }
    Ok(())
}

/// I3: a tombstone that nothing concurrent or newer ever contradicted means
/// the path is gone everywhere.
fn i3_no_resurrection(sim: &Sim) -> Result<(), Failure> {
    for (path, versions) in sim.versions() {
        for tomb in versions.iter().filter(|e| e.deleted) {
            let contradicted = versions.iter().any(|v| {
                v.version != tomb.version
                    && (v.version.dominates(&tomb.version)
                        || v.version.compare(&tomb.version) == delocal_engine::Relation::Concurrent)
            });
            if contradicted {
                continue;
            }
            for id in sim.node_ids() {
                let live = sim
                    .engine(*id)
                    .and_then(|e| e.folder(sim.folder_id()))
                    .is_some_and(|f| f.index().live(path).is_some());
                let on_disk = sim.fs(*id).is_some_and(|fs| fs.contains_key(path));
                if live || on_disk {
                    return Err(sim.failure(
                        "I3 no resurrection",
                        format!(
                            "{path} was deleted (tombstone {:?} by {}) with nothing concurrent or newer, but {} still has it (index live: {live}, on disk: {on_disk})",
                            tomb.version, tomb.modified_by.short(), id.short()
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// I4: every conflict copy present is the copy of a version that lost at
/// its path, with that version's content, and there is one copy path per
/// losing version by construction of the name. A copy the simulated user
/// edited afterwards (a mass modify picks any file) keeps the name check
/// but not the content check: it is the user's file from then on.
fn i4_bounded_conflicts(sim: &Sim) -> Result<(), Failure> {
    let mut user_edited: BTreeSet<RelPath> = BTreeSet::new();
    for id in sim.node_ids() {
        let (_, local_edit_at) = sim.synced(*id);
        user_edited.extend(local_edit_at.keys().cloned());
    }
    // Losing versions per original path: ranked below a concurrent,
    // content-differing version by the winner rule.
    let mut expected: BTreeMap<RelPath, Entry> = BTreeMap::new();
    for versions in sim.versions().values() {
        for (i, a) in versions.iter().enumerate() {
            for b in &versions[i + 1..] {
                if a.version.compare(&b.version) != delocal_engine::Relation::Concurrent
                    || a.same_content(b)
                {
                    continue;
                }
                let loser = match winner(a, b).side {
                    Side::First => b,
                    Side::Second => a,
                };
                if loser.deleted {
                    continue; // a losing tombstone has no file to copy
                }
                if let Some(name) = conflict_copy_name(loser) {
                    expected.entry(name).or_insert_with(|| loser.clone());
                }
            }
        }
    }
    for id in sim.node_ids() {
        let Some(index) = live_index(sim, *id) else {
            continue;
        };
        for (path, live) in &index {
            if !path.file_name().contains(".conflict-") {
                continue;
            }
            let Some(loser) = expected.get(path) else {
                return Err(sim.failure(
                    "I4 bounded conflicts",
                    format!(
                        "{}: conflict copy {path} matches no losing version at its original path",
                        id.short()
                    ),
                ));
            };
            if user_edited.contains(path) {
                continue; // the user's own edit of the copy; the name still checks out
            }
            let same = loser.kind == live.kind
                && loser.hash == live.hash
                && (loser.kind != Kind::File || loser.exec == live.exec);
            if !same {
                return Err(sim.failure(
                    "I4 bounded conflicts",
                    format!(
                        "{}: conflict copy {path} has content {} but the losing version {:?} had {}",
                        id.short(), live.hash.short(), loser.version, loser.hash.short()
                    ),
                ));
            }
        }
    }
    Ok(())
}
