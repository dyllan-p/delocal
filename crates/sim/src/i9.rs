//! I9 (DESIGN.md §14.1): what revert discards stays discarded. After a
//! revert, a machine announces a new local version at a path it reverted
//! only when its user has changed the path since; landing a received
//! version there is announced as usual.
//!
//! The check is off unless `--check-i9 true` turns it on, and the required
//! CI jobs leave it off: the engine still fails it, for reasons older than
//! the invariant, and those are being classified before any is fixed. A
//! failure is named by its [`Class`], so a sweep with `--keep-going` counts
//! the classes, `scripts/i9.sh` prints the counts per slice, and the
//! shrinker keeps a failing step list to the class it was found with.
//!
//! [`Watch`] is the bookkeeping. The host in `sim.rs` feeds it at the points
//! its methods name and asks it about every entry a node sends. It only
//! reads what the run does, so a seed makes the same history with the check
//! on or off.

use std::collections::{BTreeMap, BTreeSet};

use delocal_engine::folder::{Displace, FolderStatus};
use delocal_engine::{Action, ContentHash, Entry, Kind, NodeId, RelPath, Timestamp};

/// What is at a path, as I9 compares it: the kind, content and exec bit of
/// a live entry, `None` for nothing.
pub(crate) type Content = Option<(Kind, ContentHash, bool)>;

/// `entry` as a [`Content`].
fn content(entry: &Entry) -> Content {
    (!entry.deleted).then_some((entry.kind, entry.hash, entry.exec))
}

/// What made a new local version, as far as the classes need to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// A scan or the watcher reported a file at the path.
    Observed,
    /// A scan or the watcher reported the path `Absent`.
    Absent,
    /// The end of a full scan's bracket, whose deletion pass tombstones the
    /// paths the scan did not find (§7.3).
    ScanFinished,
    /// The report of a commit of `path`, which records the conflict copy it
    /// displaced a losing file to as this machine's change (§7.6) when the
    /// version is at another path.
    Applied { path: RelPath },
    /// Any other event, by name.
    Other(String),
}

/// Why a new local version at a reverted path fails I9, judged by the
/// first new local version the node made there since the revert: the rest
/// usually follow from it. The five named classes are the ones the first
/// sweeps were traced into; anything else is [`Class::Other`], named by the
/// event that made that first version, until it is traced into a class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// A scan reported the very file the revert moved to the trash. It read
    /// the file before the move, which waits for the revert's group of
    /// writes (§11), and reported it before or after the move ran.
    SawTrashedFile,
    /// A revert moved a directory above the path to the trash, and the
    /// path's file with it, while the path's record stayed live: restored
    /// by that revert, or by an earlier one. The next scan found the path
    /// empty.
    TrashedWithDirectory,
    /// A commit displaced a directory above the path to a conflict copy, and
    /// the path's file with it (§7.6, a non-empty loser), before the revert
    /// or after it. The next scan found the path empty.
    MovedWithDirectory,
    /// A scan reported the file that was at the path when the revert ran,
    /// which the revert left there because the engine did not know of it:
    /// the last observation of it was set aside while a commit at the path
    /// was in flight (§7.5), and the commit then failed on it.
    KeptUnknownFile,
    /// A commit at another path displaced a losing file to a conflict copy
    /// at the path, or beneath one, which a revert had removed. The commit
    /// records the copy as this machine's change, or the next scan finds a
    /// child that went along as an add (§7.6). A conflict copy's name
    /// repeats for the same loser, and a removed path's counters start
    /// again.
    ConflictCopy,
    /// Anything else, by the event that made the first new local version.
    Other(String),
}

impl Class {
    /// The name a failure of this class carries as its invariant, which is
    /// what a sweep groups failures by and what the shrinker keeps.
    pub(crate) fn invariant(&self) -> String {
        match self {
            Class::SawTrashedFile => "I9 saw trashed file".to_owned(),
            Class::TrashedWithDirectory => "I9 trashed with directory".to_owned(),
            Class::MovedWithDirectory => "I9 moved with directory".to_owned(),
            Class::KeptUnknownFile => "I9 kept unknown file".to_owned(),
            Class::ConflictCopy => "I9 conflict copy".to_owned(),
            Class::Other(what) => format!("I9 other: {what}"),
        }
    }
}

/// Whether the revert moved what was at the path to the trash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trashed {
    No,
    /// The path's own trash move.
    Itself,
    /// The trash move of a directory above the path.
    WithDirectory,
}

/// I9's mark on a path a node reverted.
#[derive(Clone, Debug)]
struct Mark {
    /// When the engine was last fed an observation of the path before the
    /// revert. What the user did after that, the revert could not know of,
    /// and did not discard.
    basis: Option<Timestamp>,
    /// The node's own counter in the record the revert left at the path, 0
    /// if it removed the record. A version of the node's with a larger one
    /// is a new local version.
    own: u64,
    /// What was at the path as the revert ran.
    found: Content,
    /// What the node's record at the path says, as last written.
    current: Content,
    trashed: Trashed,
    /// A commit displaced a directory above the path to a conflict copy,
    /// and the path's file with it, and the engine has not observed the
    /// path since: before the revert, or after it.
    moved_with_directory: bool,
    /// A commit since the revert displaced a file to the path, as a
    /// conflict copy or beneath one (§7.6).
    copied_here: bool,
    /// Each new local version since the revert, by the node's counter in
    /// it: what made it, and what it says is at the path.
    made: BTreeMap<u64, (Cause, Content)>,
    /// The node's own counters of new local versions I9 lets through though
    /// the user made none: the tombstone of content nobody can serve (§8.3
    /// step 4), the local change a landing whose report a crash took comes
    /// back as (§13), and a deny's bump (§8.2), which is the user's decision.
    /// Merges keep the counter, so they are let through with them.
    let_through: BTreeSet<u64>,
    /// What a commit left at the path, if a crash has since taken its
    /// report (§13): the restart's scan finds it as a local change.
    lost: Option<Content>,
}

/// The bookkeeping I9 needs, across every node of a run.
#[derive(Clone, Debug, Default)]
pub(crate) struct Watch {
    marks: BTreeMap<(NodeId, RelPath), Mark>,
    /// Marks made while their node's group of writes is open (§11), with
    /// the mark each replaced: a crash before the group is durable takes
    /// the revert with it, so its marks go too.
    undurable_marks: BTreeMap<NodeId, Vec<(RelPath, Option<Mark>)>>,
    /// When each node's engine was last fed an observation of each path.
    last_observed: BTreeMap<(NodeId, RelPath), Timestamp>,
    /// Observations fed while their node's group of writes is open (§11),
    /// with the time each replaced and whether the path was in `moved`: a
    /// crash before the group is durable takes what the engine learned from
    /// them, so the engine never saw them.
    undurable_observed: BTreeMap<NodeId, Vec<(RelPath, Option<Timestamp>, bool)>>,
    /// Paths whose file a displacement of a directory above them took away
    /// (§7.6), until the engine next observes them.
    moved: BTreeSet<(NodeId, RelPath)>,
    /// When the user last edited a file somewhere else that has since come
    /// back to a path: a conflict copy the journal's undo moved back at a
    /// restart (§7.5), edited while the node was down.
    edited_elsewhere: BTreeMap<(NodeId, RelPath), Timestamp>,
    /// Commits reported while their node's group of writes is open (§11),
    /// with the action each performed: a crash before the group is durable
    /// takes the record the report wrote, so the landing is lost as if the
    /// report never came.
    undurable_landings: BTreeMap<NodeId, Vec<(RelPath, Action)>>,
}

impl Watch {
    /// When `id`'s engine was last fed an observation of `path`.
    #[cfg(test)]
    pub(crate) fn last_observed(&self, id: NodeId, path: &RelPath) -> Option<Timestamp> {
        self.last_observed.get(&(id, path.clone())).copied()
    }

    /// `id`'s engine was fed an observation of `path` at `at`. `group_open`
    /// says whether the node's writes wait in an open group (§11).
    pub(crate) fn observed(&mut self, id: NodeId, path: &RelPath, at: Timestamp, group_open: bool) {
        let replaced = self.last_observed.insert((id, path.clone()), at);
        let moved = self.moved.remove(&(id, path.clone()));
        if group_open {
            self.undurable_observed
                .entry(id)
                .or_default()
                .push((path.clone(), replaced, moved));
        }
    }

    /// The actions of one event on `id`, which `cause` describes. A revert
    /// puts records back under their old `seq`, at or below `seq_before`,
    /// the index's `seq` as the event began, and removes those peers never
    /// saw: each such path is marked, with what `disk` says is there and
    /// whether the revert's trash moves take it. Every other index write at
    /// a marked path that raises the node's own counter is a new local
    /// version, and what made it is noted. One the engine makes for a reason
    /// §8.3 or §13 gives, or a deny's bump, is let through. `group_open` says
    /// whether the event's writes wait in an open group (§11).
    pub(crate) fn note(
        &mut self,
        id: NodeId,
        actions: &[Action],
        seq_before: u64,
        cause: &Cause,
        group_open: bool,
        disk: impl Fn(&RelPath) -> Content,
    ) {
        let status = |want: fn(&FolderStatus) -> bool| {
            actions
                .iter()
                .any(|a| matches!(a, Action::StatusChanged { status, .. } if want(status)))
        };
        let reverted = status(|s| matches!(s, FolderStatus::Reverted { .. }));
        let denied = status(|s| matches!(s, FolderStatus::Denied { .. }));
        // What the revert moves to the trash: each by a commit of its own
        // (§8.3 step 2, draft 57), a want the revert makes, whose move may
        // wait for those beneath it.
        let trash: Vec<&RelPath> = actions
            .iter()
            .filter_map(|a| match a {
                Action::MoveToTrash { path, .. } => Some(path),
                Action::WantChanged {
                    path,
                    want: Some(want),
                    ..
                } if want.is_trash() => Some(path),
                _ => None,
            })
            .collect();
        let mut marked = BTreeSet::new();
        let new_mark = |watch: &Self, path: &RelPath, own: u64, restored: Content| Mark {
            basis: watch.last_observed.get(&(id, path.clone())).copied(),
            own,
            found: disk(path),
            trashed: if trash.contains(&path) {
                Trashed::Itself
            } else if trash.iter().any(|t| t.is_ancestor_of(path)) {
                Trashed::WithDirectory
            } else {
                Trashed::No
            },
            current: restored,
            moved_with_directory: watch.moved.contains(&(id, path.clone())),
            copied_here: false,
            made: BTreeMap::new(),
            let_through: BTreeSet::new(),
            lost: None,
        };
        for action in actions {
            match action {
                Action::IndexChanged { record, .. } => {
                    let path = &record.entry.path;
                    let own = record.entry.version.counter(id);
                    if record.seq <= seq_before {
                        if reverted {
                            let m = new_mark(self, path, own, content(&record.entry));
                            self.mark(id, path, m, group_open);
                            marked.insert(path.clone());
                        }
                        continue;
                    }
                    let Some(m) = self.marks.get_mut(&(id, path.clone())) else {
                        continue;
                    };
                    let made = content(&record.entry);
                    let replaced = std::mem::replace(&mut m.current, made);
                    if own <= m.own {
                        continue;
                    }
                    let unrecoverable = actions.iter().any(|a| {
                        matches!(a, Action::StatusChanged { status: FolderStatus::Unrecoverable { path: p }, .. } if p == path)
                    });
                    // A deny's bump keeps the content of the record it
                    // replaces, a tombstone's included (§8.2).
                    let bumped = denied && record.entry.modified_by == id && made == replaced;
                    // Only a scan finds what a lost landing left (§13).
                    let scanned =
                        matches!(cause, Cause::Observed | Cause::Absent | Cause::ScanFinished);
                    let landed = scanned && m.lost == Some(made);
                    if landed {
                        m.lost = None;
                    }
                    // A revert writes a directory it would have trashed a
                    // live record of its own when a live record beneath it
                    // is restored or kept (§8.3 step 2): the live directory
                    // record of I9's row. Nothing else in a revert writes a
                    // live directory record of this machine's.
                    let kept_dir = reverted
                        && record.entry.kind == Kind::Dir
                        && !record.entry.deleted
                        && record.entry.modified_by == id;
                    if unrecoverable || bumped || landed || kept_dir {
                        m.let_through.insert(own);
                    }
                    m.made.entry(own).or_insert_with(|| (cause.clone(), made));
                }
                Action::IndexRemoved { path, .. } if reverted => {
                    let m = new_mark(self, path, 0, None);
                    self.mark(id, path, m, group_open);
                    marked.insert(path.clone());
                }
                _ => {}
            }
        }
        // A revert's trash move of a directory takes everything beneath it,
        // paths this revert leaves alone included, and a path an earlier
        // revert marked then loses its file the same way.
        if reverted {
            let taken: Vec<(RelPath, Mark)> = self
                .marks
                .iter()
                .filter(|((n, p), _)| {
                    *n == id && !marked.contains(p) && trash.iter().any(|t| t.is_ancestor_of(p))
                })
                .map(|((_, p), m)| {
                    let mut m = m.clone();
                    m.found = disk(p);
                    m.trashed = Trashed::WithDirectory;
                    (p.clone(), m)
                })
                .collect();
            for (p, m) in taken {
                self.mark(id, &p, m, group_open);
            }
        }
    }

    /// Mark `path` on `id`. A new mark starts with nothing let through: a
    /// revert that removes a record starts its counters again, and a landing
    /// lost before a revert is no longer to be found, since a revert waits
    /// for the restart's full scan (§8.3).
    fn mark(&mut self, id: NodeId, path: &RelPath, mark: Mark, group_open: bool) {
        let replaced = self.marks.insert((id, path.clone()), mark);
        if group_open {
            self.undurable_marks
                .entry(id)
                .or_default()
                .push((path.clone(), replaced));
        }
    }

    /// `id`'s open group of writes is durable (§11).
    pub(crate) fn durable(&mut self, id: NodeId) {
        self.undurable_landings.remove(&id);
        self.undurable_marks.remove(&id);
        self.undurable_observed.remove(&id);
    }

    /// `id` crashed with a group of writes open (§11): a revert the group
    /// held never happened, the landings it recorded are lost, and the
    /// engine never saw the observations it took in.
    pub(crate) fn crashed(&mut self, id: NodeId, disk: impl Fn(&RelPath) -> Content) {
        let observed = self.undurable_observed.remove(&id).unwrap_or_default();
        for (path, replaced, moved) in observed.into_iter().rev() {
            match replaced {
                Some(at) => self.last_observed.insert((id, path.clone()), at),
                None => self.last_observed.remove(&(id, path.clone())),
            };
            if moved {
                self.moved.insert((id, path));
            }
        }
        let marks = self.undurable_marks.remove(&id).unwrap_or_default();
        for (path, replaced) in marks.into_iter().rev() {
            match replaced {
                Some(mark) => self.marks.insert((id, path), mark),
                None => self.marks.remove(&(id, path)),
            };
        }
        for (path, action) in self.undurable_landings.remove(&id).unwrap_or_default() {
            self.lost(id, &path, &action, &disk);
        }
    }

    /// A commit on `id` of `path`, which performed `action`, was reported
    /// `Ok` into an open group of writes.
    pub(crate) fn landed_undurably(&mut self, id: NodeId, path: &RelPath, action: Action) {
        self.undurable_landings
            .entry(id)
            .or_default()
            .push((path.clone(), action));
    }

    /// A commit on `id` of `path` landed and a crash took its report (§13):
    /// what it left at the path, and at the conflict-copy path it displaced
    /// the losing file to, is found by the restart's scan as local changes.
    pub(crate) fn lost(
        &mut self,
        id: NodeId,
        path: &RelPath,
        action: &Action,
        disk: impl Fn(&RelPath) -> Content,
    ) {
        let copy = match action {
            Action::Write { displace, .. } | Action::Remove { displace, .. } => match displace {
                Displace::ConflictCopy(to) => Some(to),
                Displace::Trash => None,
            },
            _ => None,
        };
        for p in std::iter::once(path).chain(copy) {
            if let Some(m) = self.marks.get_mut(&(id, p.clone())) {
                m.lost = Some(disk(p));
            }
        }
    }

    /// The file now at `path` on `id` came back from somewhere its user
    /// edited it, last at `at`: for I9 that is an edit of `path`.
    pub(crate) fn edited_elsewhere(&mut self, id: NodeId, path: &RelPath, at: Timestamp) {
        let last = self
            .edited_elsewhere
            .entry((id, path.clone()))
            .or_insert(at);
        *last = (*last).max(at);
    }

    /// A commit on `id` displaced what was at `from` to `to`, as a conflict
    /// copy (§7.6); `beneath` if it went along with a directory above it.
    pub(crate) fn displaced(&mut self, id: NodeId, from: &RelPath, to: &RelPath, beneath: bool) {
        if beneath {
            self.moved.insert((id, from.clone()));
            if let Some(m) = self.marks.get_mut(&(id, from.clone())) {
                m.moved_with_directory = true;
            }
        }
        if let Some(m) = self.marks.get_mut(&(id, to.clone())) {
            m.copied_here = true;
        }
    }

    /// I9 for one entry `id` is sending: a new local version at a path it
    /// reverted fails unless its user has changed the path since the engine
    /// last saw it before the revert (`edited` is when the user last did),
    /// or the version is let through. The failure's class and what it says.
    pub(crate) fn revives(
        &self,
        id: NodeId,
        entry: &Entry,
        edited: Option<Timestamp>,
    ) -> Option<(Class, String)> {
        let mark = self.marks.get(&(id, entry.path.clone()))?;
        let counter = entry.version.counter(id);
        let elsewhere = self
            .edited_elsewhere
            .get(&(id, entry.path.clone()))
            .copied();
        let edited = edited.max(elsewhere);
        let exempt = |c: u64| mark.let_through.contains(&c);
        if counter <= mark.own
            || edited.is_some_and(|t| mark.basis.is_none_or(|b| t > b))
            || exempt(counter)
        {
            return None;
        }
        let first = mark
            .made
            .range(mark.own + 1..=counter)
            .find(|(c, _)| !exempt(**c));
        let class = match first {
            None => Class::Other("no new version made since the revert".to_owned()),
            Some((_, (cause, made))) => classify(mark, &entry.path, cause, made),
        };
        let how = first.map_or_else(
            || "nothing".to_owned(),
            |(c, (cause, made))| format!("counter {c}, from {cause:?}, saying {made:?}"),
        );
        let detail = format!(
            "{} announced its own version {:?} of {} after reverting the path, and its user has not changed the path since the engine last saw it at {:?}. The revert left counter {} and found {:?} there (trashed: {:?}); the first new version since was {how}",
            id.short(),
            entry.version,
            entry.path,
            mark.basis,
            mark.own,
            mark.found,
            mark.trashed,
        );
        Some((class, detail))
    }
}

/// The class of a failure at `path` whose first new local version `cause`
/// made, saying `made` is there.
fn classify(mark: &Mark, path: &RelPath, cause: &Cause, made: &Content) -> Class {
    let found = made.is_some() && *made == mark.found;
    match cause {
        Cause::Observed if found && mark.trashed != Trashed::No => Class::SawTrashedFile,
        Cause::Observed if found => Class::KeptUnknownFile,
        Cause::Absent | Cause::ScanFinished if mark.trashed == Trashed::WithDirectory => {
            Class::TrashedWithDirectory
        }
        Cause::Absent | Cause::ScanFinished if mark.moved_with_directory => {
            Class::MovedWithDirectory
        }
        Cause::Applied { path: at } if at != path && mark.copied_here => Class::ConflictCopy,
        Cause::Observed if mark.copied_here => Class::ConflictCopy,
        Cause::Applied { .. } => Class::Other("applied".to_owned()),
        Cause::Observed => Class::Other("observed".to_owned()),
        Cause::Absent | Cause::ScanFinished => Class::Other("found empty".to_owned()),
        Cause::Other(what) => Class::Other(what.clone()),
    }
}

#[cfg(test)]
mod tests {
    use delocal_engine::{BatchId, FolderId, HostName, IndexRecord, Version};

    use super::*;

    const FOLDER: FolderId = FolderId::from_bytes([9; 16]);
    const A: NodeId = NodeId::from_bytes([1; 16]);
    const B: NodeId = NodeId::from_bytes([2; 16]);
    const X: ContentHash = ContentHash::from_bytes([3; 32]);
    const Y: ContentHash = ContentHash::from_bytes([4; 32]);

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn at(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    /// `path` at `version` by A, holding `hash` over `prev`, or a tombstone
    /// if `hash` is empty.
    fn entry(path: &str, version: &[(NodeId, u64)], hash: ContentHash, prev: ContentHash) -> Entry {
        let deleted = hash == ContentHash::EMPTY;
        Entry {
            path: rel(path),
            kind: Kind::File,
            size: 1,
            mtime_ns: 1,
            stamp: 1,
            exec: false,
            hash,
            prev_hash: prev,
            version: version.iter().copied().collect::<Version>(),
            deleted,
            modified_by: A,
            author_host: HostName::empty(),
        }
    }

    fn changed(entry: &Entry, seq: u64) -> Action {
        Action::IndexChanged {
            folder: FOLDER,
            record: IndexRecord {
                entry: entry.clone(),
                seq,
            },
        }
    }

    fn reverted() -> Action {
        Action::StatusChanged {
            folder: FOLDER,
            status: FolderStatus::Reverted {
                batch: BatchId::from_bytes([5; 16]),
                trashed: 1,
                refetch: 1,
                kept: 0,
            },
        }
    }

    fn trash(path: &str) -> Action {
        Action::MoveToTrash {
            folder: FOLDER,
            path: rel(path),
            version: Version::empty(),
            expected: delocal_engine::Observed {
                kind: Kind::File,
                size: 0,
                mtime_ns: 0,
                exec: false,
                hash: ContentHash::EMPTY,
            },
        }
    }

    /// X at every path, as a disk that has not changed.
    fn x_on_disk(_: &RelPath) -> Content {
        Some((Kind::File, X, false))
    }

    /// A watch where A observed `path` at 10 s and then reverted it to
    /// {A: 1, B: 1} holding Y, with the index at `seq` 5 when the revert
    /// began, moving X to the trash with `trashed` (the path or a
    /// directory above it) unless that is empty.
    fn reverted_watch(path: &str, trashed: &str) -> Watch {
        let mut watch = Watch::default();
        watch.observed(A, &rel(path), at(10), false);
        revert(&mut watch, path, trashed);
        watch
    }

    /// A reverts `path` to {A: 1, B: 1} holding Y, as in [`reverted_watch`],
    /// on `watch` as it stands.
    fn revert(watch: &mut Watch, path: &str, trashed: &str) {
        let restored = entry(path, &[(A, 1), (B, 1)], Y, ContentHash::EMPTY);
        let mut actions = vec![changed(&restored, 3), reverted()];
        if !trashed.is_empty() {
            actions.push(trash(trashed));
        }
        let cause = Cause::Other("Revert".to_owned());
        watch.note(A, &actions, 5, &cause, false, x_on_disk);
    }

    /// A's new version {A: 2, B: 1} at `path`, holding `hash`, made by
    /// `cause` as the index's seventh write, and what I9 says of sending
    /// it if the user last edited the path at `edited`.
    fn made(
        watch: &mut Watch,
        path: &str,
        hash: ContentHash,
        cause: Cause,
        edited: Option<Timestamp>,
    ) -> Option<Class> {
        let new = entry(path, &[(A, 2), (B, 1)], hash, Y);
        watch.note(A, &[changed(&new, 7)], 6, &cause, false, x_on_disk);
        watch.revives(A, &new, edited).map(|(class, _)| class)
    }

    /// A conflict copy the user edited while the node was down comes back
    /// to its path when the journal is undone at the restart (§7.5): that
    /// edit is the user's change of the path, and what it makes there is
    /// let through.
    #[test]
    fn an_edit_of_a_copy_that_comes_back_is_an_edit_of_the_path() {
        let mut watch = reverted_watch("f", "f");
        watch.edited_elsewhere(A, &rel("f"), at(12));
        assert_eq!(made(&mut watch, "f", X, Cause::Observed, None), None);
    }

    /// I9's row lets through the live directory record a revert writes for
    /// a directory it keeps because a live record beneath it is restored
    /// (§8.3 step 2): here d's tombstone is restored and, in the same
    /// event, d gets a live directory record of A's own.
    #[test]
    fn a_reverts_own_live_directory_record_is_let_through() {
        let mut watch = Watch::default();
        watch.observed(A, &rel("d"), at(10), false);
        let restored = entry(
            "d",
            &[(A, 1), (B, 1)],
            ContentHash::EMPTY,
            ContentHash::EMPTY,
        );
        let dir = Entry {
            kind: Kind::Dir,
            deleted: false,
            ..entry(
                "d",
                &[(A, 2), (B, 1)],
                ContentHash::EMPTY,
                ContentHash::EMPTY,
            )
        };
        let actions = [changed(&restored, 3), changed(&dir, 6), reverted()];
        let cause = Cause::Other("Revert".to_owned());
        watch.note(A, &actions, 5, &cause, false, |_| {
            Some((Kind::Dir, ContentHash::EMPTY, false))
        });
        assert_eq!(watch.revives(A, &dir, None).map(|(class, _)| class), None);
    }

    /// Class 1: a scan reports the file the revert moved to the trash, as a
    /// new local version.
    #[test]
    fn a_scan_that_reports_the_file_a_revert_trashed_fails() {
        let mut watch = reverted_watch("f", "f");
        let class = made(&mut watch, "f", X, Cause::Observed, Some(at(5)));
        assert_eq!(class, Some(Class::SawTrashedFile));
    }

    /// What the user did after the engine last saw the path, the revert
    /// could not know of, and did not discard.
    #[test]
    fn a_user_change_since_the_last_observation_is_not_a_failure() {
        let mut watch = reverted_watch("f", "f");
        assert_eq!(
            made(&mut watch, "f", X, Cause::Observed, Some(at(11))),
            None
        );
    }

    /// §11: a crash before the revert's group of writes is durable takes the
    /// revert with it, so it marks nothing.
    #[test]
    fn a_revert_a_crash_took_marks_nothing() {
        let mut watch = Watch::default();
        let restored = entry("f", &[(A, 1), (B, 1)], Y, ContentHash::EMPTY);
        let cause = Cause::Other("Revert".to_owned());
        watch.note(
            A,
            &[changed(&restored, 3), reverted(), trash("f")],
            5,
            &cause,
            true,
            x_on_disk,
        );
        watch.crashed(A, x_on_disk);
        assert_eq!(made(&mut watch, "f", X, Cause::Observed, None), None);
    }

    /// §11: a crash before the group of writes an observation was fed into
    /// is durable takes what the engine learned from it, so the engine last
    /// saw the path at the observation before. The user's edit between the
    /// two is one the revert could not know of.
    #[test]
    fn an_observation_a_crash_took_is_not_the_last_sight() {
        let mut watch = Watch::default();
        watch.observed(A, &rel("f"), at(10), false);
        watch.observed(A, &rel("f"), at(20), true);
        watch.crashed(A, x_on_disk);
        revert(&mut watch, "f", "f");
        assert_eq!(
            made(&mut watch, "f", X, Cause::Observed, Some(at(15))),
            None
        );
        // Once its group is durable, the observation stays.
        let mut watch = Watch::default();
        watch.observed(A, &rel("f"), at(10), false);
        watch.observed(A, &rel("f"), at(20), true);
        watch.durable(A);
        watch.crashed(A, x_on_disk);
        revert(&mut watch, "f", "f");
        let class = made(&mut watch, "f", X, Cause::Observed, Some(at(15)));
        assert_eq!(class, Some(Class::SawTrashedFile));
    }

    /// §13: a landing whose report a crash took comes back as a local
    /// change, found by a scan, and is let through. Only a scan finds it: a
    /// version made otherwise with the same content, such as a deny's bump
    /// would be, does not use it up.
    #[test]
    fn a_lost_landing_is_let_through_only_when_a_scan_finds_it() {
        let lost = Action::Remove {
            folder: FOLDER,
            path: rel("f"),
            expected: None,
            displace: Displace::Trash,
        };
        let mut watch = reverted_watch("f", "");
        watch.lost(A, &rel("f"), &lost, x_on_disk);
        let tick = Cause::Other("Tick".to_owned());
        assert!(made(&mut watch.clone(), "f", X, tick, None).is_some());
        assert_eq!(made(&mut watch, "f", X, Cause::Observed, None), None);
    }

    /// Class 2: a later revert's trash move of a directory takes a file an
    /// earlier revert restored beneath it, and the next scan's deletion
    /// pass tombstones it.
    #[test]
    fn a_later_reverts_trash_move_of_a_directory_takes_a_marked_file() {
        let mut watch = reverted_watch("d/f", "");
        let cause = Cause::Other("Revert".to_owned());
        watch.note(A, &[reverted(), trash("d")], 6, &cause, false, x_on_disk);
        let class = made(
            &mut watch,
            "d/f",
            ContentHash::EMPTY,
            Cause::ScanFinished,
            None,
        );
        assert_eq!(class, Some(Class::TrashedWithDirectory));
    }

    /// Class 3: a directory displaced to a conflict copy before the revert
    /// took the file along, and the engine had not seen the path since.
    #[test]
    fn a_file_a_directory_took_to_a_conflict_copy_before_the_revert_is_class_3() {
        let mut watch = Watch::default();
        watch.displaced(A, &rel("d/f"), &rel("d.conflict/f"), true);
        let restored = entry("d/f", &[(A, 1), (B, 1)], Y, ContentHash::EMPTY);
        let cause = Cause::Other("Revert".to_owned());
        watch.note(
            A,
            &[changed(&restored, 3), reverted()],
            5,
            &cause,
            false,
            |_| None,
        );
        let class = made(
            &mut watch,
            "d/f",
            ContentHash::EMPTY,
            Cause::ScanFinished,
            None,
        );
        assert_eq!(class, Some(Class::MovedWithDirectory));
    }

    /// Class 5: a commit of another path displaces its losing file to a
    /// conflict copy at a path the revert removed, and records the copy.
    #[test]
    fn a_conflict_copy_recorded_at_a_removed_path_is_class_5() {
        let mut watch = Watch::default();
        let removed = Action::IndexRemoved {
            folder: FOLDER,
            path: rel("f.conflict"),
        };
        let cause = Cause::Other("Revert".to_owned());
        watch.note(
            A,
            &[removed, reverted(), trash("f.conflict")],
            5,
            &cause,
            false,
            x_on_disk,
        );
        watch.displaced(A, &rel("f"), &rel("f.conflict"), false);
        let applied = Cause::Applied { path: rel("f") };
        let class = made(&mut watch, "f.conflict", Y, applied, None);
        assert_eq!(class, Some(Class::ConflictCopy));
    }
}
