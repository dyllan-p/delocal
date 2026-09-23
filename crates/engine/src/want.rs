//! The want-list (DESIGN.md §7.5, §6.5): what this machine still has to
//! fetch or commit, and how it chooses where from.
//!
//! One [`Want`] per path: the entry to end up with (`M` for a conflict), its
//! apply mode, the members that announced exactly that version, and a
//! state. The engine turns wants into `Fetch`, `Write`, `Remove` and
//! `SetMeta` actions and back into index records when the host reports.
//!
//! **Selection** (§7.5): among connected, non-excluded sources the best tier
//! wins (`lan`, then `direct`, then `relay`), ties by smaller node ID; a
//! source whose §6.5 tier limit the file exceeds is skipped. No candidate is
//! *without source*; all skipped is *deferred*. At most
//! `max_fetches_per_peer` and `max_fetches_per_folder` fetches run at once,
//! assigned in `(tier, node, path)` order; a want whose best source has no
//! free slot takes the next allowed source instead.
//!
//! **Ordering gate** (§7.5): a create or modify commits only after every
//! ancestor directory's create in the list has committed; a directory
//! delete commits only after every descendant's delete. Fetches are not
//! gated, only commits.
//!
//! **In flight** (§7.5): a path whose want is *wanted*, *blocked*,
//! *fetching* or *committing* is in flight; *deferred*, *without source* and
//! *given up* are observable, because they can last for days. A want made
//! by `revert` (§8.3) is `restoring`: an `Absent` observation is the trash
//! move and is ignored in every state.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::batch::{ApplyItem, ApplyMode};
use crate::conflict::ConflictCopy;
use crate::entry::{Entry, Kind};
use crate::id::{BatchId, NodeId};
use crate::path::RelPath;
use crate::rules::Rules;
use crate::time::{COMMIT_DEADLINE_NANOS, FETCH_STALL_NANOS, Timestamp};
use crate::version::Version;

/// How a peer is currently reached (§6.4). Ordered best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    Lan,
    Direct,
    Relay,
}

impl Tier {
    /// The largest file this tier carries under `rules` (§6.5); `None` for
    /// no limit.
    pub fn limit(self, rules: &Rules) -> Option<u64> {
        match self {
            Self::Lan => None,
            Self::Direct => Some(rules.direct_limit),
            Self::Relay => Some(rules.relay_limit),
        }
    }

    /// True if a file of `size` may be fetched on this tier.
    pub fn allows(self, rules: &Rules, size: u64) -> bool {
        self.limit(rules).is_none_or(|limit| size <= limit)
    }
}

/// Where a want is in its life (§7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WantState {
    /// Ready to fetch (or, with content in hand, to commit) as soon as a
    /// source, a slot and the ordering gate allow.
    Wanted,
    /// Content in hand or not needed, waiting on the ordering gate.
    Blocked,
    /// Every available source is on a tier whose §6.5 limit the file
    /// exceeds; `need` is the least demanding tier that would allow it.
    Deferred { need: Tier },
    /// No connected member announced this version.
    NoSource,
    /// The host is fetching from `from`; stalls at `deadline`.
    Fetching { from: NodeId, deadline: Timestamp },
    /// The host is committing; overdue at `deadline`.
    Committing { deadline: Timestamp },
    /// Two hash mismatches; waits for the index to change (§7.5 step 4).
    GaveUp,
}

impl WantState {
    /// Short-lived states hide observations of the path (§7.5 "in flight").
    pub fn in_flight(self) -> bool {
        matches!(
            self,
            Self::Wanted | Self::Blocked | Self::Fetching { .. } | Self::Committing { .. }
        )
    }
}

/// One path this machine still has to bring up to date (§7.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Want {
    /// What to end up with: the incoming entry, or `M` for a conflict.
    pub entry: Entry,
    pub mode: ApplyMode,
    /// Where the commit displaces the losing local file, if this is a
    /// conflict resolution (§7.6).
    pub conflict: Option<ConflictCopy>,
    /// The batch the version arrived in, for history.
    pub batch: BatchId,
    pub source: NodeId,
    pub seq_high: u64,
    /// Members that announced exactly this version: the batch's source, the
    /// version's author, and later batches carrying the same version. One
    /// that answers `NotAvailable` has moved on and is removed (§7.5 step
    /// 3); announcing the version again puts it back, since it now says it
    /// holds it. A conflict's `M` exists nowhere until someone merges, so
    /// its first announcer is often the winner's holder, which can answer
    /// `NotAvailable` until it has seen the loser too.
    pub sources: BTreeSet<NodeId>,
    /// Sources that served a hash mismatch: never asked again for this
    /// want, whatever they announce (§7.5 step 4 retries from another).
    pub excluded: BTreeSet<NodeId>,
    pub mismatches: u8,
    /// Content has been fetched and verified; only the commit remains.
    pub fetched: bool,
    /// Made by `revert` (§8.3): `Absent` observations are the trash move.
    pub restoring: bool,
    pub state: WantState,
}

impl Want {
    pub fn path(&self) -> &RelPath {
        &self.entry.path
    }

    pub fn version(&self) -> &Version {
        &self.entry.version
    }

    /// True if the path is in flight (§7.5).
    pub fn in_flight(&self) -> bool {
        self.state.in_flight()
    }

    /// True if content must be fetched before the commit.
    pub fn needs_fetch(&self) -> bool {
        self.mode == ApplyMode::Fetch && !self.fetched
    }

    /// True if this want removes the entry.
    pub fn is_delete(&self) -> bool {
        self.entry.deleted
    }

    /// True if this want creates or replaces a directory.
    pub fn is_dir_create(&self) -> bool {
        !self.entry.deleted && self.entry.kind == Kind::Dir
    }

    fn from_item(item: ApplyItem, batch: BatchId, source: NodeId, seq_high: u64) -> Self {
        let ApplyItem::Apply {
            entry,
            mode,
            conflict,
        } = item;
        let sources = [source, entry.modified_by].into_iter().collect();
        Self {
            entry,
            mode,
            conflict,
            batch,
            source,
            seq_high,
            sources,
            excluded: BTreeSet::new(),
            mismatches: 0,
            fetched: false,
            restoring: false,
            state: WantState::Wanted,
        }
    }
}

/// What the host should be asked to do for a want, decided by
/// [`WantList::dispatch`]. The engine turns each into an `Action`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WantStep {
    Fetch {
        path: RelPath,
        version: Version,
        from: NodeId,
    },
    /// Commit (`Write`, `Remove` or `SetMeta` by mode and entry).
    Commit(Box<Want>),
    /// An index-only apply: adopt without a host action.
    Adopt(Entry),
}

/// The per-folder want-list. Every mutation is recorded in `changes` for
/// the engine to report (`WantChanged`), so Phase 2 can persist the table.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WantList {
    wants: BTreeMap<RelPath, Want>,
    /// Paths whose want changed or vanished since the last drain, in order.
    #[serde(skip)]
    changed: Vec<RelPath>,
}

impl WantList {
    pub fn is_empty(&self) -> bool {
        self.wants.is_empty()
    }

    pub fn len(&self) -> usize {
        self.wants.len()
    }

    pub fn get(&self, path: &RelPath) -> Option<&Want> {
        self.wants.get(path)
    }

    /// Every want in path order.
    pub fn iter(&self) -> impl Iterator<Item = &Want> {
        self.wants.values()
    }

    /// True if the path has a want in a short-lived state (§7.5).
    pub fn in_flight(&self, path: &RelPath) -> bool {
        self.wants.get(path).is_some_and(Want::in_flight)
    }

    /// True if the path has a `revert`-made want (§8.3).
    pub fn restoring(&self, path: &RelPath) -> bool {
        self.wants.get(path).is_some_and(|w| w.restoring)
    }

    /// Number of fetches in progress.
    pub fn fetching(&self) -> usize {
        self.wants
            .values()
            .filter(|w| matches!(w.state, WantState::Fetching { .. }))
            .count()
    }

    fn note(&mut self, path: &RelPath) {
        if self.changed.last() != Some(path) {
            self.changed.push(path.clone());
        }
    }

    /// Paths whose want changed since the last drain, with the want as it
    /// stands (`None` if removed), in order of first change.
    pub fn drain_changes(&mut self) -> Vec<(RelPath, Option<Want>)> {
        let mut seen = BTreeSet::new();
        std::mem::take(&mut self.changed)
            .into_iter()
            .filter(|p| seen.insert(p.clone()))
            .map(|p| {
                let want = self.wants.get(&p).cloned();
                (p, want)
            })
            .collect()
    }

    /// An accepted item arrives (§7.4). A want for the same version gains
    /// the source; a dominating version replaces the want; a dominated or
    /// concurrent version is refused and handed back for the caller to
    /// defer.
    pub fn insert(
        &mut self,
        item: ApplyItem,
        batch: BatchId,
        source: NodeId,
        seq_high: u64,
    ) -> Option<ApplyItem> {
        let path = item.path().clone();
        let incoming = item.incoming().version.clone();
        if let Some(existing) = self.wants.get_mut(&path) {
            if incoming == *existing.version() {
                existing.sources.insert(source);
                self.note(&path);
                return None;
            }
            if !incoming.dominates(existing.version()) {
                return Some(item);
            }
        }
        self.wants
            .insert(path.clone(), Want::from_item(item, batch, source, seq_high));
        self.note(&path);
        None
    }

    /// A batch carried `(path, version)`, whether or not it produced an
    /// item: if that is exactly what a want is after, the batch's source
    /// has it (§7.5).
    pub fn note_announced(&mut self, path: &RelPath, version: &Version, by: NodeId) {
        if let Some(want) = self.wants.get_mut(path)
            && want.version() == version
            && want.sources.insert(by)
        {
            self.note(path);
        }
    }

    /// A want made by `revert` (§8.3): fetch the restored entry again.
    pub fn insert_restoring(&mut self, want: Want) {
        let path = want.path().clone();
        self.wants.insert(path.clone(), want);
        self.note(&path);
    }

    /// A want persisted by the host comes back after a restart. Transient
    /// states become `Wanted`.
    pub fn restore(&mut self, mut want: Want) {
        if matches!(
            want.state,
            WantState::Fetching { .. } | WantState::Committing { .. } | WantState::Blocked
        ) {
            want.state = WantState::Wanted;
        }
        let path = want.path().clone();
        self.wants.insert(path.clone(), want);
        self.note(&path);
    }

    /// The process restarted: every transient state is *wanted* again, the
    /// host's operations having died with it.
    pub fn restarted(&mut self) {
        let paths: Vec<RelPath> = self.wants.keys().cloned().collect();
        for path in paths {
            if let Some(w) = self.wants.remove(&path) {
                self.restore(w);
            }
        }
    }

    /// Drop the want at `path` (committed, superseded or cancelled).
    pub fn remove(&mut self, path: &RelPath) -> Option<Want> {
        let want = self.wants.remove(path);
        if want.is_some() {
            self.note(path);
        }
        want
    }

    fn set_state(&mut self, path: &RelPath, state: WantState) {
        if let Some(want) = self.wants.get_mut(path)
            && want.state != state
        {
            want.state = state;
            self.note(path);
        }
    }

    /// The host reports on a fetch (§7.5 steps 3 and 4).
    pub fn fetched(
        &mut self,
        path: &RelPath,
        version: &Version,
        outcome: FetchReport,
    ) -> Option<&Want> {
        let want = self.wants.get_mut(path)?;
        if want.version() != version {
            return None;
        }
        let from = match want.state {
            WantState::Fetching { from, .. } => Some(from),
            _ => None,
        };
        match outcome {
            FetchReport::Ok => {
                want.fetched = true;
                want.state = WantState::Wanted;
            }
            FetchReport::NotAvailable => {
                if let Some(from) = from {
                    want.sources.remove(&from);
                }
                want.state = WantState::Wanted;
            }
            FetchReport::HashMismatch => {
                if let Some(from) = from {
                    want.excluded.insert(from);
                }
                want.mismatches = want.mismatches.saturating_add(1);
                want.state = if want.mismatches >= 2 {
                    WantState::GaveUp
                } else {
                    WantState::Wanted
                };
            }
        }
        self.note(path);
        self.wants.get(path)
    }

    /// The host reported bytes arriving: push the stall deadline out.
    pub fn progress(&mut self, now: Timestamp, path: &RelPath, version: &Version) {
        if let Some(want) = self.wants.get_mut(path)
            && want.version() == version
            && let WantState::Fetching { from, .. } = want.state
        {
            want.state = WantState::Fetching {
                from,
                deadline: now.plus_nanos(FETCH_STALL_NANOS),
            };
            self.note(path);
        }
    }

    /// Deadlines that have passed return the want to `Wanted` (§7.5); the
    /// source is not excluded, the stall may have been ours.
    pub fn expire(&mut self, now: Timestamp) -> Vec<RelPath> {
        let overdue: Vec<RelPath> = self
            .wants
            .iter()
            .filter(|(_, w)| match w.state {
                WantState::Fetching { deadline, .. } | WantState::Committing { deadline } => {
                    deadline <= now
                }
                _ => false,
            })
            .map(|(p, _)| p.clone())
            .collect();
        for path in &overdue {
            self.set_state(path, WantState::Wanted);
        }
        overdue
    }

    /// A peer went away: fetches from it are over.
    pub fn peer_gone(&mut self, peer: NodeId) {
        let paths: Vec<RelPath> = self
            .wants
            .iter()
            .filter(|(_, w)| matches!(w.state, WantState::Fetching { from, .. } if from == peer))
            .map(|(p, _)| p.clone())
            .collect();
        for path in paths {
            self.set_state(&path, WantState::Wanted);
        }
    }

    /// The earliest fetch or commit deadline, for the engine's wake-up.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        self.wants
            .values()
            .filter_map(|w| match w.state {
                WantState::Fetching { deadline, .. } | WantState::Committing { deadline } => {
                    Some(deadline)
                }
                _ => None,
            })
            .min()
    }

    /// True if committing `path` must wait (§7.5 ordering): a create with an
    /// ancestor directory still to be created, or a directory delete with a
    /// descendant still to be deleted.
    fn gated(&self, want: &Want) -> bool {
        if want.is_delete() {
            want.entry.kind == Kind::Dir
                && self
                    .wants
                    .values()
                    .any(|other| other.is_delete() && want.path().is_ancestor_of(other.path()))
        } else {
            self.wants
                .values()
                .any(|other| other.is_dir_create() && other.path().is_ancestor_of(want.path()))
        }
    }

    /// The sources a want may fetch from under the peers' tiers (§7.5,
    /// §6.5), best first: connected, not excluded, and on a tier whose
    /// limit the file fits.
    fn select(&self, want: &Want, rules: &Rules, peers: &BTreeMap<NodeId, Tier>) -> Selection {
        let mut candidates: Vec<(Tier, NodeId)> = want
            .sources
            .iter()
            .filter(|n| !want.excluded.contains(n))
            .filter_map(|n| peers.get(n).map(|t| (*t, *n)))
            .collect();
        if candidates.is_empty() {
            return Selection::NoSource;
        }
        candidates.sort();
        let size = want.entry.size;
        let allowed: Vec<(Tier, NodeId)> = candidates
            .into_iter()
            .filter(|(tier, _)| tier.allows(rules, size))
            .collect();
        if allowed.is_empty() {
            let need = [Tier::Relay, Tier::Direct, Tier::Lan]
                .into_iter()
                .find(|t| t.allows(rules, size))
                .unwrap_or(Tier::Lan);
            return Selection::Deferred(need);
        }
        Selection::Sources(allowed)
    }

    /// Decide what to do next for every want (§7.5), deterministically:
    /// expire nothing here (see [`WantList::expire`]), assign fetch slots in
    /// `(tier, node, path)` order within the limits, and commit whatever has
    /// its content and passes the ordering gate.
    pub fn dispatch(
        &mut self,
        now: Timestamp,
        rules: &Rules,
        peers: &BTreeMap<NodeId, Tier>,
    ) -> Vec<WantStep> {
        let mut steps = Vec::new();

        // 1. Fetches: candidates sorted by (tier, node, path), slots by peer and folder.
        let mut per_peer: BTreeMap<NodeId, u32> = BTreeMap::new();
        let mut total: u32 = 0;
        for w in self.wants.values() {
            if let WantState::Fetching { from, .. } = w.state {
                *per_peer.entry(from).or_insert(0) += 1;
                total += 1;
            }
        }
        // Wants ordered by their best (tier, node), then path; each carries
        // every allowed source so a full peer falls through to the next.
        let mut fetchable: Vec<Fetchable> = Vec::new();
        let mut reselected: Vec<(RelPath, WantState)> = Vec::new();
        for (path, want) in &self.wants {
            if !want.needs_fetch() {
                continue;
            }
            match want.state {
                WantState::Wanted | WantState::Deferred { .. } | WantState::NoSource => {}
                _ => continue,
            }
            match self.select(want, rules, peers) {
                Selection::Sources(allowed) => {
                    let (tier, node) = allowed[0];
                    fetchable.push((tier, node, path.clone(), allowed));
                }
                Selection::NoSource => reselected.push((path.clone(), WantState::NoSource)),
                Selection::Deferred(need) => {
                    reselected.push((path.clone(), WantState::Deferred { need }));
                }
            }
        }
        for (path, state) in reselected {
            self.set_state(&path, state);
        }
        fetchable.sort();
        for (_, _, path, allowed) in fetchable {
            if total >= rules.max_fetches_per_folder {
                self.set_state(&path, WantState::Wanted);
                continue;
            }
            let Some(node) = allowed
                .iter()
                .map(|(_, node)| *node)
                .find(|node| per_peer.get(node).copied().unwrap_or(0) < rules.max_fetches_per_peer)
            else {
                self.set_state(&path, WantState::Wanted);
                continue;
            };
            *per_peer.entry(node).or_insert(0) += 1;
            total += 1;
            let Some(want) = self.wants.get(&path) else {
                continue;
            };
            steps.push(WantStep::Fetch {
                path: path.clone(),
                version: want.version().clone(),
                from: node,
            });
            self.set_state(
                &path,
                WantState::Fetching {
                    from: node,
                    deadline: now.plus_nanos(FETCH_STALL_NANOS),
                },
            );
        }

        // 2. Commits: content in hand or not needed, gate permitting.
        let ready: Vec<RelPath> = self
            .wants
            .iter()
            .filter(|(_, w)| {
                !w.needs_fetch()
                    && matches!(
                        w.state,
                        WantState::Wanted
                            | WantState::Blocked
                            | WantState::Deferred { .. }
                            | WantState::NoSource
                    )
            })
            .map(|(p, _)| p.clone())
            .collect();
        // Creates parents first, deletes children first (§7.5).
        let mut creates: Vec<RelPath> = ready
            .iter()
            .filter(|p| !self.wants[*p].is_delete())
            .cloned()
            .collect();
        creates.sort();
        let mut deletes: Vec<RelPath> = ready
            .iter()
            .filter(|p| self.wants[*p].is_delete())
            .cloned()
            .collect();
        deletes.sort();
        deletes.reverse();
        for path in creates.into_iter().chain(deletes) {
            let Some(want) = self.wants.get(&path) else {
                continue;
            };
            if self.gated(want) {
                self.set_state(&path, WantState::Blocked);
                continue;
            }
            if want.mode == ApplyMode::IndexOnly {
                let want = self.remove(&path).map(|w| w.entry);
                if let Some(entry) = want {
                    steps.push(WantStep::Adopt(entry));
                }
                continue;
            }
            steps.push(WantStep::Commit(Box::new(want.clone())));
            self.set_state(
                &path,
                WantState::Committing {
                    deadline: now.plus_nanos(COMMIT_DEADLINE_NANOS),
                },
            );
        }
        steps
    }
}

/// The host's report on a fetch (§7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FetchReport {
    Ok,
    NotAvailable,
    HashMismatch,
}

enum Selection {
    /// Allowed sources, best first. Never empty.
    Sources(Vec<(Tier, NodeId)>),
    NoSource,
    Deferred(Tier),
}

/// A want ready to fetch: its best `(tier, node)`, its path, and every
/// allowed source in order. Sorted by the first three fields.
type Fetchable = (Tier, NodeId, RelPath, Vec<(Tier, NodeId)>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::ContentHash;
    use crate::id::HostName;

    fn node(i: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = i;
        NodeId::from_bytes(b)
    }

    fn bid(i: u8) -> BatchId {
        let mut b = [0u8; 16];
        b[15] = i;
        BatchId::from_bytes(b)
    }

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_unix_nanos(secs * 1_000_000_000)
    }

    fn entry(path: &str, kind: Kind, size: u64, by: u8, deleted: bool) -> Entry {
        let mut h = [0u8; 32];
        h[0] = 1;
        Entry {
            path: p(path),
            kind,
            size,
            mtime_ns: 0,
            exec: false,
            hash: if kind == Kind::Dir || deleted {
                ContentHash::EMPTY
            } else {
                ContentHash::from_bytes(h)
            },
            prev_hash: ContentHash::EMPTY,
            version: Version::empty().incremented(node(by)),
            deleted,
            modified_by: node(by),
            author_host: HostName::new("h").unwrap(),
        }
    }

    fn item(e: Entry, mode: ApplyMode) -> ApplyItem {
        ApplyItem::Apply {
            entry: e,
            mode,
            conflict: None,
        }
    }

    fn list_with(paths: &[(&str, Kind, u64, ApplyMode, bool)]) -> WantList {
        let mut l = WantList::default();
        for (path, kind, size, mode, deleted) in paths {
            assert!(
                l.insert(
                    item(entry(path, *kind, *size, 2, *deleted), *mode),
                    bid(1),
                    node(2),
                    1,
                )
                .is_none()
            );
        }
        l.drain_changes();
        l
    }

    fn peers(list: &[(u8, Tier)]) -> BTreeMap<NodeId, Tier> {
        list.iter().map(|(n, t)| (node(*n), *t)).collect()
    }

    #[test]
    fn tiers_order_best_first_and_apply_size_limits() {
        assert!(Tier::Lan < Tier::Direct && Tier::Direct < Tier::Relay);
        let r = Rules::default();
        assert!(Tier::Lan.allows(&r, u64::MAX));
        assert!(Tier::Direct.allows(&r, r.direct_limit));
        assert!(!Tier::Direct.allows(&r, r.direct_limit + 1));
        assert!(!Tier::Relay.allows(&r, r.relay_limit + 1));
    }

    #[test]
    fn a_source_that_was_not_available_is_a_source_again_when_it_announces() {
        // Only node 2 (the batch source) announced the version.
        let mut l = list_with(&[("f", Kind::File, 10, ApplyMode::Fetch, false)]);
        let version = entry("f", Kind::File, 10, 2, false).version;
        let peers = peers(&[(2, Tier::Direct), (3, Tier::Direct)]);
        let steps = l.dispatch(t(0), &Rules::default(), &peers);
        assert!(matches!(&steps[0], WantStep::Fetch { from, .. } if *from == node(2)));
        l.fetched(&p("f"), &version, FetchReport::NotAvailable);
        assert!(l.dispatch(t(1), &Rules::default(), &peers).is_empty());
        assert_eq!(l.get(&p("f")).unwrap().state, WantState::NoSource);
        assert!(
            l.get(&p("f")).unwrap().excluded.is_empty(),
            "not available is not a mismatch"
        );
        // Node 2 announces the version again (it has merged to it): asked again.
        l.note_announced(&p("f"), &version, node(2));
        let steps = l.dispatch(t(2), &Rules::default(), &peers);
        assert!(
            matches!(&steps[0], WantStep::Fetch { from, .. } if *from == node(2)),
            "{steps:?}"
        );
        // A mismatch, by contrast, sticks.
        l.fetched(&p("f"), &version, FetchReport::HashMismatch);
        l.note_announced(&p("f"), &version, node(2));
        assert!(l.dispatch(t(3), &Rules::default(), &peers).is_empty());
        assert_eq!(l.get(&p("f")).unwrap().state, WantState::NoSource);
    }

    #[test]
    fn selection_prefers_tier_then_node_and_skips_excluded() {
        let mut l = list_with(&[("f", Kind::File, 10, ApplyMode::Fetch, false)]);
        l.note_announced(
            &p("f"),
            &entry("f", Kind::File, 10, 2, false).version,
            node(3),
        );
        l.note_announced(
            &p("f"),
            &entry("f", Kind::File, 10, 2, false).version,
            node(4),
        );
        let steps = l.dispatch(
            t(0),
            &Rules::default(),
            &peers(&[(2, Tier::Relay), (3, Tier::Direct), (4, Tier::Direct)]),
        );
        assert_eq!(
            steps,
            [WantStep::Fetch {
                path: p("f"),
                version: entry("f", Kind::File, 10, 2, false).version,
                from: node(3)
            }],
            "direct beats relay, node 3 beats node 4"
        );
        assert!(
            matches!(l.get(&p("f")).unwrap().state, WantState::Fetching { from, .. } if from == node(3))
        );
        // NotAvailable from 3: 4 is next; a mismatch from 4 leaves 2; a second mismatch gives up.
        l.fetched(
            &p("f"),
            &entry("f", Kind::File, 10, 2, false).version,
            FetchReport::NotAvailable,
        );
        let steps = l.dispatch(
            t(1),
            &Rules::default(),
            &peers(&[(2, Tier::Relay), (3, Tier::Direct), (4, Tier::Direct)]),
        );
        assert!(matches!(&steps[0], WantStep::Fetch { from, .. } if *from == node(4)));
        l.fetched(
            &p("f"),
            &entry("f", Kind::File, 10, 2, false).version,
            FetchReport::HashMismatch,
        );
        let steps = l.dispatch(
            t(2),
            &Rules::default(),
            &peers(&[(2, Tier::Relay), (3, Tier::Direct), (4, Tier::Direct)]),
        );
        assert!(matches!(&steps[0], WantStep::Fetch { from, .. } if *from == node(2)));
        l.fetched(
            &p("f"),
            &entry("f", Kind::File, 10, 2, false).version,
            FetchReport::HashMismatch,
        );
        assert_eq!(l.get(&p("f")).unwrap().state, WantState::GaveUp);
        assert!(!l.in_flight(&p("f")), "given up is observable");
        assert!(
            l.dispatch(t(3), &Rules::default(), &peers(&[(2, Tier::Lan)]))
                .is_empty()
        );
    }

    #[test]
    fn deferral_names_the_tier_that_would_do_and_lifts_on_upgrade() {
        let r = Rules::default();
        let big = r.relay_limit + 1;
        let mut l = list_with(&[("big", Kind::File, big, ApplyMode::Fetch, false)]);
        assert!(l.dispatch(t(0), &r, &peers(&[(2, Tier::Relay)])).is_empty());
        assert_eq!(
            l.get(&p("big")).unwrap().state,
            WantState::Deferred { need: Tier::Direct }
        );
        assert!(!l.in_flight(&p("big")));
        assert!(l.dispatch(t(0), &r, &peers(&[])).is_empty());
        assert_eq!(l.get(&p("big")).unwrap().state, WantState::NoSource);
        let steps = l.dispatch(t(1), &r, &peers(&[(2, Tier::Direct)]));
        assert_eq!(steps.len(), 1);
        assert!(l.in_flight(&p("big")));
        let huge = r.direct_limit + 1;
        let mut l = list_with(&[("huge", Kind::File, huge, ApplyMode::Fetch, false)]);
        l.dispatch(t(0), &r, &peers(&[(2, Tier::Direct)]));
        assert_eq!(
            l.get(&p("huge")).unwrap().state,
            WantState::Deferred { need: Tier::Lan }
        );
    }

    #[test]
    fn fetch_slots_are_limited_per_peer_and_per_folder() {
        let r = Rules {
            max_fetches_per_peer: 2,
            max_fetches_per_folder: 3,
            ..Rules::default()
        };
        let mut l = list_with(&[
            ("a", Kind::File, 1, ApplyMode::Fetch, false),
            ("b", Kind::File, 1, ApplyMode::Fetch, false),
            ("c", Kind::File, 1, ApplyMode::Fetch, false),
            ("d", Kind::File, 1, ApplyMode::Fetch, false),
            ("e", Kind::File, 1, ApplyMode::Fetch, false),
        ]);
        for path in ["a", "b", "c", "d", "e"] {
            l.note_announced(
                &p(path),
                &entry(path, Kind::File, 1, 2, false).version,
                node(3),
            );
        }
        let steps = l.dispatch(t(0), &r, &peers(&[(2, Tier::Lan), (3, Tier::Lan)]));
        assert_eq!(steps.len(), 3, "folder limit");
        let from: Vec<NodeId> = steps
            .iter()
            .map(|s| match s {
                WantStep::Fetch { from, .. } => *from,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            from,
            [node(2), node(2), node(3)],
            "two on node 2 (the smaller id), then node 3"
        );
        assert_eq!(l.fetching(), 3);
        assert_eq!(
            l.get(&p("d")).unwrap().state,
            WantState::Wanted,
            "no slot: stays wanted, no deadline"
        );
        // A fetch finishing frees a slot.
        l.fetched(
            &p("a"),
            &entry("a", Kind::File, 1, 2, false).version,
            FetchReport::Ok,
        );
        let steps = l.dispatch(t(1), &r, &peers(&[(2, Tier::Lan), (3, Tier::Lan)]));
        assert_eq!(steps.len(), 2, "a commits and one more fetch starts");
        assert!(matches!(steps[0], WantStep::Fetch { .. }));
        assert!(matches!(&steps[1], WantStep::Commit(w) if w.path() == &p("a")));
        assert_eq!(l.fetching(), 3);
    }

    #[test]
    fn ordering_gate_holds_children_and_directory_deletes() {
        let mut l = list_with(&[
            ("d/inner", Kind::File, 1, ApplyMode::Fetch, false),
            ("d", Kind::Dir, 0, ApplyMode::Direct, false),
            ("old/x", Kind::File, 0, ApplyMode::Direct, true),
            ("old", Kind::Dir, 0, ApplyMode::Direct, true),
        ]);
        l.fetched(
            &p("d/inner"),
            &entry("d/inner", Kind::File, 1, 2, false).version,
            FetchReport::Ok,
        );
        let steps = l.dispatch(t(0), &Rules::default(), &peers(&[(2, Tier::Lan)]));
        let committed: Vec<&str> = steps
            .iter()
            .filter_map(|s| match s {
                WantStep::Commit(w) => Some(w.path().as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            committed,
            ["d", "old/x"],
            "the directory first, the child delete first"
        );
        assert_eq!(l.get(&p("d/inner")).unwrap().state, WantState::Blocked);
        assert_eq!(l.get(&p("old")).unwrap().state, WantState::Blocked);
        assert!(l.in_flight(&p("d/inner")));
        // Once the parent and the child delete commit, the rest follows.
        l.remove(&p("d"));
        l.remove(&p("old/x"));
        let steps = l.dispatch(t(1), &Rules::default(), &peers(&[(2, Tier::Lan)]));
        let committed: Vec<&str> = steps
            .iter()
            .filter_map(|s| match s {
                WantStep::Commit(w) => Some(w.path().as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(committed, ["d/inner", "old"]);
    }

    #[test]
    fn deadlines_expire_and_progress_extends_them() {
        let mut l = list_with(&[("f", Kind::File, 1, ApplyMode::Fetch, false)]);
        let v = entry("f", Kind::File, 1, 2, false).version;
        l.dispatch(t(0), &Rules::default(), &peers(&[(2, Tier::Lan)]));
        assert_eq!(l.next_deadline(), Some(t(60)));
        l.progress(t(50), &p("f"), &v);
        assert_eq!(l.next_deadline(), Some(t(110)));
        assert!(l.expire(t(109)).is_empty());
        assert_eq!(l.expire(t(110)), vec![p("f")]);
        assert_eq!(l.get(&p("f")).unwrap().state, WantState::Wanted);
        assert!(
            l.get(&p("f")).unwrap().excluded.is_empty(),
            "a stall does not exclude the source"
        );
        // A late Ok is still taken.
        l.fetched(&p("f"), &v, FetchReport::Ok);
        assert!(l.get(&p("f")).unwrap().fetched);
        let steps = l.dispatch(t(111), &Rules::default(), &peers(&[(2, Tier::Lan)]));
        assert!(matches!(&steps[0], WantStep::Commit(_)));
        assert_eq!(l.next_deadline(), Some(t(141)));
    }

    #[test]
    fn insert_merges_sources_replaces_on_dominate_and_refuses_otherwise() {
        let mut l = WantList::default();
        let e = entry("f", Kind::File, 1, 2, false);
        assert!(
            l.insert(item(e.clone(), ApplyMode::Fetch), bid(1), node(2), 1)
                .is_none()
        );
        assert_eq!(
            l.get(&p("f")).unwrap().sources,
            [node(2)].into_iter().collect()
        );
        assert!(
            l.insert(item(e.clone(), ApplyMode::Fetch), bid(2), node(3), 1)
                .is_none()
        );
        assert_eq!(
            l.get(&p("f")).unwrap().sources.len(),
            2,
            "same version: another source"
        );
        let mut newer = e.clone();
        newer.version = e.version.incremented(node(2));
        assert!(
            l.insert(item(newer.clone(), ApplyMode::Fetch), bid(3), node(2), 2)
                .is_none()
        );
        assert_eq!(
            l.get(&p("f")).unwrap().version(),
            &newer.version,
            "dominating: replaced"
        );
        assert_eq!(l.get(&p("f")).unwrap().sources.len(), 1);
        let mut other = e.clone();
        other.version = Version::empty().incremented(node(5));
        assert!(
            l.insert(item(other, ApplyMode::Fetch), bid(4), node(5), 1)
                .is_some(),
            "concurrent: refused"
        );
        let changes = l.drain_changes();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].1.is_some());
        l.remove(&p("f"));
        assert_eq!(l.drain_changes(), vec![(p("f"), None)]);
    }

    #[test]
    fn restore_resets_transient_states_and_serde_skips_changes() {
        let mut l = list_with(&[("f", Kind::File, 1, ApplyMode::Fetch, false)]);
        l.dispatch(t(0), &Rules::default(), &peers(&[(2, Tier::Lan)]));
        let w = l.get(&p("f")).unwrap().clone();
        assert!(matches!(w.state, WantState::Fetching { .. }));
        let mut fresh = WantList::default();
        fresh.restore(w);
        assert_eq!(fresh.get(&p("f")).unwrap().state, WantState::Wanted);
        let bytes = postcard::to_stdvec(&l).unwrap();
        let back: WantList = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.get(&p("f")), l.get(&p("f")));
    }
}
