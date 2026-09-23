//! Conflicts (DESIGN.md §7.6): the winner rule, the merged version `M`, and
//! the conflict-copy name.
//!
//! Two versions of one path conflict when they are concurrent (§7.2) and
//! their content differs (§7.6). Every machine resolves the same pair to
//! the same `M = merge(W, L)`: the winner's content fields under the
//! component-wise maximum of the two vectors, no increment. `M` dominates
//! both inputs, so no machine has to be told what the others decided.
//!
//! Everything here is a pure function of the two entries. Nothing is looked
//! up locally, which is what makes the conflict-copy name identical on
//! every machine that computes it.

use serde::{Deserialize, Serialize};

use crate::entry::{Entry, Kind};
use crate::path::{MAX_COMPONENT_LEN, RelPath};
use crate::time::format_utc_compact;

/// Which of two entries a rule picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    /// The first argument (`a`, or `incoming` in [`resolve`]).
    First,
    /// The second argument (`b`, or `local` in [`resolve`]).
    Second,
}

impl Side {
    /// The other side.
    pub const fn other(self) -> Self {
        match self {
            Self::First => Self::Second,
            Self::Second => Self::First,
        }
    }

    /// The side seen from swapped arguments.
    pub const fn flip(self) -> Self {
        self.other()
    }
}

/// The outcome of one comparison under the winner rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Winner {
    pub side: Side,
    /// True if only rule 5 separated the two. Should never happen (§7.6);
    /// the simulator counts it.
    pub fallback: bool,
}

/// The §7.6 winner rule, total and symmetric: `winner(a, b)` and
/// `winner(b, a)` name the same entry.
///
/// 1. Exactly one side is a tombstone: the live side wins.
/// 2. Exactly one side is metadata-only (`hash == prev_hash`): the other wins.
/// 3. Larger `mtime_ns`.
/// 4. Larger `modified_by`.
/// 5. Larger `hash`, then `kind` (file < dir < symlink), then `exec` set,
///    then larger `size`, `prev_hash` and `author_host`. Only to make the
///    rule total; two concurrent versions from one author should be
///    impossible. `kind` is in the chain because a symlink and a file can
///    share a hash (the target string and the content) with everything
///    else equal, and the two sides must still agree.
///
/// Two entries equal in every field the rule looks at differ only in
/// their vectors, so `M` is the same whichever is picked; they give
/// `First` with `fallback: true`.
pub fn winner(a: &Entry, b: &Entry) -> Winner {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let pick = |ord: std::cmp::Ordering, fallback: bool| match ord {
        Greater => Some(Winner {
            side: Side::First,
            fallback,
        }),
        Less => Some(Winner {
            side: Side::Second,
            fallback,
        }),
        Equal => None,
    };
    // 1. live beats tombstone
    if let Some(w) = pick(b.deleted.cmp(&a.deleted), false) {
        return w;
    }
    // 2. content change beats touch
    if let Some(w) = pick(b.is_metadata_only().cmp(&a.is_metadata_only()), false) {
        return w;
    }
    // 3, 4.
    if let Some(w) = pick(a.mtime_ns.cmp(&b.mtime_ns), false) {
        return w;
    }
    if let Some(w) = pick(a.modified_by.cmp(&b.modified_by), false) {
        return w;
    }
    // 5. the total fallback
    total_fallback(a, b)
}

/// Rule 5: every remaining content field, in a fixed order, so two entries
/// that differ at all are ordered the same way from either side.
fn total_fallback(a: &Entry, b: &Entry) -> Winner {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let ord = a
        .hash
        .cmp(&b.hash)
        .then(a.kind.cmp(&b.kind))
        .then(a.exec.cmp(&b.exec))
        .then(a.size.cmp(&b.size))
        .then(a.prev_hash.cmp(&b.prev_hash))
        .then(a.author_host.cmp(&b.author_host));
    Winner {
        side: match ord {
            Greater | Equal => Side::First,
            Less => Side::Second,
        },
        fallback: true,
    }
}

/// Which side's content fields the §7.2 identical-content merge keeps:
/// larger `mtime_ns`, then larger `modified_by`, then the total fallback.
/// Rules 1 and 2 do not apply, because identical content means both are
/// tombstones or neither is, and a touch is not demoted by anything when
/// the content is the same anyway.
pub fn prefer_fields(a: &Entry, b: &Entry) -> Winner {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let pick = |ord: std::cmp::Ordering, fallback: bool| match ord {
        Greater => Some(Winner {
            side: Side::First,
            fallback,
        }),
        Less => Some(Winner {
            side: Side::Second,
            fallback,
        }),
        Equal => None,
    };
    pick(a.mtime_ns.cmp(&b.mtime_ns), false)
        .or_else(|| pick(a.modified_by.cmp(&b.modified_by), false))
        .unwrap_or_else(|| total_fallback(a, b))
}

/// `M = merge(W, L)` (§7.6): `w`'s content fields under the component-wise
/// maximum of both vectors, no increment. Also the §7.2 identical-content
/// merge when `w` is the side [`prefer_fields`] picked.
pub fn merged(w: &Entry, l: &Entry) -> Entry {
    Entry {
        version: w.version.merge(&l.version),
        ..w.clone()
    }
}

/// What the losing local file becomes when the winner's content is
/// committed over it (§7.6, §7.5 step 7).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConflictCopy {
    /// Where the host moves the existing file instead of the trash.
    pub path: RelPath,
    /// The record the file had, `L`: its content fields become the copy's.
    pub loser: Entry,
}

/// A resolved conflict between an incoming entry and the local record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    /// Which input won.
    pub winner: Side,
    /// `M`, identical on every machine given the same two inputs.
    pub merged: Entry,
    /// Rule 5 decided it.
    pub fallback: bool,
}

/// Resolve a concurrent, different-content pair. `resolve(a, b)` and
/// `resolve(b, a)` produce the same `merged` entry.
pub fn resolve(incoming: &Entry, local: &Entry) -> Resolution {
    let w = winner(incoming, local);
    let (win, lose) = match w.side {
        Side::First => (incoming, local),
        Side::Second => (local, incoming),
    };
    Resolution {
        winner: w.side,
        merged: merged(win, lose),
        fallback: w.fallback,
    }
}

/// Split a file name into stem and extension on the last `.` that is not
/// at position 0. Directories keep the whole name (§7.6).
fn split_name(name: &str, kind: Kind) -> (&str, &str) {
    if kind == Kind::Dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// The longest prefix of `s` within `max` bytes that ends on a char boundary.
fn cut(s: &str, max: usize) -> &str {
    let mut n = max.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// `<stem><suffix><ext>` within `max` bytes: the stem gives way first, then
/// the extension from its end. The suffix is always kept whole.
fn fit(stem: &str, suffix: &str, ext: &str, max: usize) -> String {
    let budget = max.saturating_sub(suffix.len());
    if stem.len() + ext.len() <= budget {
        return format!("{stem}{suffix}{ext}");
    }
    let stem = cut(stem, budget.saturating_sub(ext.len()));
    let ext = cut(ext, budget.saturating_sub(stem.len()));
    format!("{stem}{suffix}{ext}")
}

/// The conflict-copy path for a losing entry `l` (§7.6):
///
/// ```text
/// <stem>.conflict-<L mtime as UTC YYYYMMDD-HHMMSS>-<L.author_host><ext>
/// ```
///
/// Split on the last extension; dotfiles and directories take the suffix on
/// the whole name; an empty `author_host` gives the short form of
/// `modified_by`. Deterministically truncated so the component fits
/// [`MAX_COMPONENT_LEN`] bytes. Built only from `l`, so every machine gets
/// the same path. Returns `None` only if the result is somehow not a valid
/// path, which the construction rules out; callers then displace to trash.
pub fn conflict_copy_name(l: &Entry) -> Option<RelPath> {
    let name = l.path.file_name();
    let (stem, ext) = split_name(name, l.kind);
    let who = if l.author_host.is_empty() {
        l.modified_by.short().to_string()
    } else {
        l.author_host.as_str().to_owned()
    };
    let suffix = format!(".conflict-{}-{}", format_utc_compact(l.mtime_ns), who);
    let file_name = fit(stem, &suffix, ext, MAX_COMPONENT_LEN);
    match l.path.parent() {
        Some(parent) => parent.join(&file_name).ok(),
        None => RelPath::new(file_name).ok(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::entry::ContentHash;
    use crate::id::{HostName, NodeId};
    use crate::version::Version;

    fn node(i: u8) -> NodeId {
        let mut b = [0u8; 16];
        b[0] = i;
        NodeId::from_bytes(b)
    }

    fn hash(i: u8) -> ContentHash {
        let mut b = [0u8; 32];
        b[0] = i;
        b[31] = 1;
        ContentHash::from_bytes(b)
    }

    fn entry(path: &str, by: u8, mtime_ns: i64, h: u8) -> Entry {
        Entry {
            path: RelPath::new(path).unwrap(),
            kind: Kind::File,
            size: 10,
            mtime_ns,
            exec: false,
            hash: hash(h),
            prev_hash: hash(100),
            version: Version::empty().incremented(node(by)),
            deleted: false,
            modified_by: node(by),
            author_host: HostName::new(format!("host{by}")).unwrap(),
        }
    }

    fn ts(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
        // days from civil (Hinnant), for test inputs only
        let (y, mo) = if mo <= 2 { (y - 1, mo + 12) } else { (y, mo) };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (mo - 3) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        ((days * 86_400) + h * 3600 + mi * 60 + s) * 1_000_000_000
    }

    #[test]
    fn example_name_from_the_design() {
        let mut l = entry("report.xlsx", 1, ts(2026, 9, 22, 14, 30, 5), 1);
        l.author_host = HostName::new("laptop").unwrap();
        assert_eq!(
            conflict_copy_name(&l).unwrap().as_str(),
            "report.conflict-20260922-143005-laptop.xlsx"
        );
    }

    #[test]
    fn name_rules_for_extensions_dotfiles_dirs_and_empty_host() {
        let t = ts(2026, 9, 22, 14, 30, 5);
        let mut l = entry("a/b/archive.tar.gz", 1, t, 1);
        l.author_host = HostName::new("pi").unwrap();
        assert_eq!(
            conflict_copy_name(&l).unwrap().as_str(),
            "a/b/archive.tar.conflict-20260922-143005-pi.gz",
            "split on the last extension, parent kept"
        );
        let mut dot = entry(".bashrc", 1, t, 1);
        dot.author_host = HostName::new("pi").unwrap();
        assert_eq!(
            conflict_copy_name(&dot).unwrap().as_str(),
            ".bashrc.conflict-20260922-143005-pi"
        );
        let mut dir = entry("photos.old", 1, 0, 1);
        dir.kind = Kind::Dir;
        dir.author_host = HostName::new("pi").unwrap();
        assert_eq!(
            conflict_copy_name(&dir).unwrap().as_str(),
            "photos.old.conflict-19700101-000000-pi",
            "directories take the whole name and have no mtime"
        );
        let mut anon = entry("x.txt", 7, t, 1);
        anon.author_host = HostName::empty();
        assert_eq!(
            conflict_copy_name(&anon).unwrap().as_str(),
            "x.conflict-20260922-143005-07000000.txt",
            "short node id when the host is empty"
        );
        let mut noext = entry("Makefile", 1, t, 1);
        noext.author_host = HostName::new("pi").unwrap();
        assert_eq!(
            conflict_copy_name(&noext).unwrap().as_str(),
            "Makefile.conflict-20260922-143005-pi"
        );
    }

    #[test]
    fn long_names_are_cut_to_fit() {
        let t = ts(2026, 9, 22, 14, 30, 5);
        let host = HostName::new("h".repeat(63)).unwrap();
        // A 250-byte stem plus a 4-byte extension: the stem gives way.
        let mut l = entry(&format!("{}.txt", "s".repeat(251)), 1, t, 1);
        l.author_host = host.clone();
        let name = conflict_copy_name(&l).unwrap();
        assert_eq!(name.file_name().len(), 255);
        assert!(name.file_name().ends_with(&format!("-{host}.txt")));
        assert!(name.file_name().starts_with("sss"));
        // Multi-byte stem: the cut lands on a char boundary.
        let mut u = entry(&format!("{}.md", "ü".repeat(125)), 1, t, 1);
        u.author_host = host.clone();
        let name = conflict_copy_name(&u).unwrap();
        assert!(name.file_name().len() <= 255);
        assert!(name.file_name().ends_with(".md"));
        // An extension too long on its own: it is cut from the end.
        let mut e = entry(&format!("a.{}", "e".repeat(253)), 1, t, 1);
        e.author_host = host;
        let name = conflict_copy_name(&e).unwrap();
        assert_eq!(name.file_name().len(), 255);
        assert!(name.file_name().contains(".conflict-"));
    }

    #[test]
    fn winner_rule_in_order() {
        let a = entry("f", 1, 100, 1);
        let b = entry("f", 2, 200, 2);
        // 3: larger mtime
        assert_eq!(winner(&a, &b).side, Side::Second);
        assert_eq!(winner(&b, &a).side, Side::First);
        // 1: live beats tombstone regardless of mtime
        let mut dead = b.clone();
        dead.deleted = true;
        dead.hash = ContentHash::EMPTY;
        assert_eq!(winner(&a, &dead).side, Side::First);
        assert_eq!(winner(&dead, &a).side, Side::Second);
        // 2: a real edit beats a touch regardless of mtime
        let mut touch = b.clone();
        touch.prev_hash = touch.hash;
        assert!(touch.is_metadata_only());
        assert_eq!(winner(&a, &touch).side, Side::First);
        assert_eq!(winner(&touch, &a).side, Side::Second);
        // 4: equal mtime, larger node id
        let c = entry("f", 3, 200, 3);
        let w = winner(&b, &c);
        assert_eq!((w.side, w.fallback), (Side::Second, false));
        // 5: same author and mtime, fallback on hash
        let d = entry("f", 2, 200, 9);
        let w = winner(&b, &d);
        assert_eq!((w.side, w.fallback), (Side::Second, true));
        assert_eq!(winner(&d, &b).side, Side::First);
        // 5: a symlink and a file with the same bytes, author and time
        let mut link = b.clone();
        link.kind = Kind::Symlink;
        let w = winner(&b, &link);
        assert_eq!(
            (w.side, w.fallback),
            (Side::Second, true),
            "symlink orders above file"
        );
        assert_eq!(winner(&link, &b).side, Side::First);
        // full tie
        let w = winner(&b, &b);
        assert_eq!((w.side, w.fallback), (Side::First, true));
    }

    #[test]
    fn merged_takes_winner_fields_under_the_merged_vector() {
        let mut a = entry("f", 1, 100, 1);
        a.version = Version::empty().incremented(node(1)).incremented(node(1));
        let b = entry("f", 2, 200, 2);
        let r = resolve(&a, &b);
        assert_eq!(r.winner, Side::Second);
        assert!(!r.fallback);
        let m = &r.merged;
        assert_eq!((m.hash, m.mtime_ns, m.modified_by), (hash(2), 200, node(2)));
        assert_eq!(m.author_host.as_str(), "host2");
        assert_eq!(m.version.counter(node(1)), 2);
        assert_eq!(m.version.counter(node(2)), 1);
        assert!(m.version.dominates(&a.version));
        assert!(m.version.dominates(&b.version));
        assert_eq!(resolve(&b, &a).merged, r.merged, "symmetric");
    }

    // ---- properties -------------------------------------------------------------

    fn any_entry() -> impl Strategy<Value = Entry> {
        (
            0u8..3,        // path
            0u8..4,        // author
            0i64..4,       // mtime
            0u8..3,        // hash
            0u8..3,        // prev_hash
            any::<bool>(), // exec
            any::<bool>(), // deleted
            0u8..3,        // kind
            0u8..3,        // extra version key
        )
            .prop_map(|(path, by, mtime, h, ph, exec, deleted, kind, other)| {
                let kind = match kind {
                    0 => Kind::File,
                    1 => Kind::Dir,
                    _ => Kind::Symlink,
                };
                let mut e = entry(
                    ["d/a.txt", "b", ".rc"][path as usize],
                    by,
                    if kind == Kind::File { mtime } else { 0 },
                    h,
                );
                e.kind = kind;
                e.exec = exec && kind == Kind::File;
                e.deleted = deleted;
                e.prev_hash = hash(ph);
                if deleted || kind == Kind::Dir {
                    e.hash = ContentHash::EMPTY;
                }
                e.version = e.version.incremented(node(other));
                e
            })
    }

    /// Two entries at one path with different content: a conflict.
    fn conflicting_pair() -> impl Strategy<Value = (Entry, Entry)> {
        (any_entry(), any_entry())
            .prop_map(|(a, mut b)| {
                b.path = a.path.clone();
                (a, b)
            })
            .prop_filter("identical content is a merge, not a conflict", |(a, b)| {
                !a.same_content(b)
            })
    }

    proptest! {
        /// Every machine given the same pair, in either order, computes the
        /// same M; M dominates both inputs and has the winner's content.
        #[test]
        fn resolution_is_symmetric_and_dominating((a, b) in conflicting_pair()) {
            let ab = resolve(&a, &b);
            let ba = resolve(&b, &a);
            prop_assert_eq!(postcard::to_stdvec(&ab.merged).unwrap(), postcard::to_stdvec(&ba.merged).unwrap());
            prop_assert_eq!(ab.winner, ba.winner.flip());
            prop_assert_eq!(ab.fallback, ba.fallback);
            let m = &ab.merged;
            prop_assert!(m.version.dominates_or_equals(&a.version));
            prop_assert!(m.version.dominates_or_equals(&b.version));
            prop_assert_eq!(&m.version, &a.version.merge(&b.version));
            let w = if ab.winner == Side::First { &a } else { &b };
            prop_assert!(m.same_content(w));
            prop_assert_eq!((m.mtime_ns, m.modified_by, &m.author_host, m.prev_hash, m.size),
                            (w.mtime_ns, w.modified_by, &w.author_host, w.prev_hash, w.size));
            // Rule 1 and 2, when they apply.
            if a.deleted != b.deleted {
                prop_assert_eq!(w.deleted, false);
            } else if a.is_metadata_only() != b.is_metadata_only() {
                prop_assert!(!w.is_metadata_only());
            }
            // Rule 5 only fires when 3 and 4 tie.
            if ab.fallback {
                prop_assert_eq!(a.mtime_ns, b.mtime_ns);
                prop_assert_eq!(a.modified_by, b.modified_by);
            }
        }

        /// The conflict-copy name is a valid path with a legal component
        /// length, keeps the parent, and is the same on every machine (here:
        /// after a serde round trip of L).
        #[test]
        fn conflict_copy_name_is_valid_and_deterministic(l in any_entry(), long in 0usize..300, host_len in 0usize..64) {
            let mut l = l;
            let stem: String = "é".repeat(long / 2) + &"x".repeat(long % 2);
            l.path = RelPath::new(format!("dir/{stem}.dat")).or_else(|_| RelPath::new("dir/x.dat")).unwrap();
            l.author_host = HostName::new("h".repeat(host_len)).unwrap();
            let name = conflict_copy_name(&l).unwrap();
            prop_assert!(name.file_name().len() <= MAX_COMPONENT_LEN);
            prop_assert_eq!(name.parent(), l.path.parent());
            prop_assert!(name.file_name().contains(".conflict-"));
            prop_assert!(RelPath::new(name.as_str()).is_ok());
            let copy: Entry = postcard::from_bytes(&postcard::to_stdvec(&l).unwrap()).unwrap();
            prop_assert_eq!(conflict_copy_name(&copy), Some(name));
        }
    }
}
