//! The trash (DESIGN.md §8.4): `<folder>/.delocal/trash/YYYY-MM-DD/<path>`,
//! where a commit moves what a remote change overwrites or deletes, and a
//! revert what it moves aside.
//!
//! **Where things go.** The day is the UTC day of the move (draft 63): the
//! name only has to be stable and sortable, since pruning goes by the
//! recorded time. Beneath it the entry keeps its index path. A name already
//! taken that day gets `~1`, `~2`, … before its extension, split as §7.6
//! splits conflict names, and a name that would then exceed 255 bytes is
//! cut as §7.6 cuts them, stem first, keeping the `~N` ([`numbered`]). A
//! parent taken by something that is not a directory gets a number the same
//! way. Every move is a rename within the folder's filesystem, and like
//! every rename of a commit it never replaces anything (§7.5).
//!
//! **Rows.** Each entry moved in has a row in the `trash` table (§11): where
//! it is, the path it came from, its hash, when it was moved, and its size,
//! for pruning, `delocal trash` and filling a fetch from the trash (§7.5
//! step 2). A directory moved in whole (a write displacing one) is one row,
//! with the empty hash a directory has (§7.1) and the size of everything in
//! it. A commit whose displacement is journalled makes its row durable with
//! the journal's (§7.5); every other move's row is written with the commit's
//! report. A crash between a move and its row leaves a file with no row,
//! which [`reconcile`] adopts at the next start, and a row whose file is
//! gone is dropped there too.
//!
//! **Pruning** ([`prune`]): every entry moved in more than 30 days ago, and
//! then the oldest first while the trash holds more than 10 GiB (§8.4,
//! §17 question 8), with the directories left empty. Nothing here enters a
//! directory on another device than the folder's: the commit path never
//! moves a mounted disk into the trash, and if one is mounted there anyway,
//! pruning, sizing and reconciling all stop at it.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use delocal_engine::path::MAX_COMPONENT_LEN;
use delocal_engine::time::format_utc_compact;
use delocal_engine::{ContentHash, FolderId, Kind, RelPath, Timestamp};

use crate::fs::{FileKind, Fs, NotAFile, ParentNotADirectory, Refusing, rename_refusing};
use crate::scan::hash::{CHUNK, hash_target};
use crate::store::{HostWrite, TrashRow};

/// The trash, relative to the folder root.
pub const DIR: &str = ".delocal/trash";

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// How long and how much the trash keeps (§8.4). Per folder via `rules`
/// in Phase 4; these are the defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Entries moved in longer ago than this go.
    pub max_age_nanos: i64,
    /// Past this many bytes, the oldest entries go first.
    pub max_bytes: u64,
}

impl Default for Policy {
    /// 30 days and 10 GiB (§8.4, §17 question 8).
    fn default() -> Self {
        Self {
            max_age_nanos: 30 * NANOS_PER_DAY,
            max_bytes: 10 * 1024 * 1024 * 1024,
        }
    }
}

/// The day directory for an entry moved in at `at`: `YYYY-MM-DD`, in UTC.
pub fn day(at: Timestamp) -> String {
    let compact = format_utc_compact(at.as_unix_nanos());
    // `YYYYMMDD-HHMMSS`: the year is everything before the last four
    // digits of the date.
    let date = compact.split('-').next().unwrap_or_default();
    let (year, month_day) = date.split_at(date.len().saturating_sub(4));
    let (month, day) = month_day.split_at(month_day.len().min(2));
    format!("{year}-{month}-{day}")
}

/// `name` the `n`th time it is taken (§8.4): itself for 0, otherwise with
/// `~n` before its extension, split as §7.6 splits conflict names (the
/// last `.` not at the start; a directory on the whole name), and cut to
/// 255 bytes as §7.6 cuts them, the stem first, then the extension from
/// its end, always keeping `~n`.
pub fn numbered(name: &str, kind: Kind, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    let (stem, ext) = split_name(name, kind);
    fit(stem, &format!("~{n}"), ext, MAX_COMPONENT_LEN)
}

/// §7.6's split, which the engine keeps to itself: stem and extension at
/// the last `.` that is not the first character; a directory keeps its
/// whole name.
fn split_name(name: &str, kind: Kind) -> (&str, &str) {
    if kind == Kind::Dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// The longest prefix of `s` within `max` bytes that ends on a character
/// boundary.
fn cut(s: &str, max: usize) -> &str {
    let mut n = max.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// `<stem><suffix><ext>` within `max` bytes, as §7.6 fits a conflict name:
/// the stem gives way first, then the extension from its end.
fn fit(stem: &str, suffix: &str, ext: &str, max: usize) -> String {
    let budget = max.saturating_sub(suffix.len());
    if stem.len() + ext.len() <= budget {
        return format!("{stem}{suffix}{ext}");
    }
    let stem = cut(stem, budget.saturating_sub(ext.len()));
    let ext = cut(ext, budget.saturating_sub(stem.len()));
    format!("{stem}{suffix}{ext}")
}

/// A place in the trash: where an entry goes, and its row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// Relative to the folder root.
    pub disk: PathBuf,
    /// Relative to `.delocal/trash/`, as the row records it.
    pub row: PathBuf,
}

/// What a move into the trash is about to move: what the row records.
#[derive(Clone, Debug)]
pub struct Moving<'a> {
    /// The index path it is moved from.
    pub original: &'a RelPath,
    pub kind: Kind,
    /// Its content hash, as the guard found it matching (a directory's is
    /// [`ContentHash::EMPTY`]).
    pub hash: ContentHash,
    /// Its size; for a directory, of everything in it.
    pub size: u64,
    /// When the move runs.
    pub at: Timestamp,
}

impl Moving<'_> {
    /// The row for this entry at `place`.
    pub fn row(&self, place: &Place) -> TrashRow {
        TrashRow {
            trashed_path: place.row.clone(),
            original_path: self.original.clone(),
            hash: self.hash,
            trashed_at: self.at,
            size: self.size,
        }
    }
}

/// What [`move_in`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    pub place: Place,
    /// How its rename was made.
    pub refusing: Refusing,
}

/// Why [`move_in`] moved nothing.
#[derive(Debug)]
pub enum NotMoved {
    /// Nothing was at `from` any more.
    Gone,
    /// The trash could not be made ready, or the rename failed.
    Failed(io::Error),
}

/// Move what is at `from` (relative to the folder root) into the trash, to
/// the first free place for `what` (§8.4). `before` runs with each place
/// just before its rename, and its error stops the move: a journalled
/// commit makes its row durable there (§7.5). A place taken between the
/// check and the rename is skipped for the next one; nothing in the trash
/// is ever replaced.
pub fn move_in(
    fs: &dyn Fs,
    from: &Path,
    what: &Moving<'_>,
    before: &mut dyn FnMut(&Place) -> io::Result<()>,
) -> Result<Moved, NotMoved> {
    let day = day(what.at);
    let parent = parents(fs, &day, what.original).map_err(|e| NotMoved::Failed(own(e)))?;
    let name = what.original.file_name();
    let place = |n: u32| {
        let row = parent.join(numbered(name, what.kind, n));
        Place {
            disk: Path::new(DIR).join(&row),
            row,
        }
    };
    let taken = |n: u32| match fs.lstat(&place(n).disk) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    };
    let mut from_n = 0;
    loop {
        let n = free_number(&taken, from_n).map_err(|e| NotMoved::Failed(own(e)))?;
        let place = place(n);
        before(&place).map_err(NotMoved::Failed)?;
        match rename_refusing(fs, from, &place.disk) {
            Ok(refusing) => return Ok(Moved { place, refusing }),
            // Taken in the window: look on from the next number.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                from_n = n.checked_add(1).ok_or_else(|| {
                    NotMoved::Failed(io::Error::other("no free name in the trash"))
                })?;
            }
            // Anything else: if the entry went, or what holds it stopped
            // being a directory, the path changed; otherwise the trouble
            // is the trash's own, a local failure.
            Err(e) => {
                return match fs.lstat(from) {
                    Err(gone)
                        if gone.kind() == io::ErrorKind::NotFound
                            || ParentNotADirectory::of(&gone).is_some() =>
                    {
                        Err(NotMoved::Gone)
                    }
                    _ => Err(NotMoved::Failed(own(e))),
                };
            }
        }
    }
}

/// An error in the trash's own directories, as a commit must read it: as a
/// local failure, never as its path changing underneath, which a missing
/// place or one that is not a directory would otherwise look like. An
/// error that is a local failure already (`ENOSPC`, `EIO`) keeps its
/// number.
fn own(e: io::Error) -> io::Error {
    let looks_changed = matches!(
        e.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::AlreadyExists
            | io::ErrorKind::DirectoryNotEmpty
    ) || ParentNotADirectory::of(&e).is_some()
        || NotAFile::of(&e).is_some();
    if looks_changed {
        io::Error::other(format!("in the trash: {e}"))
    } else {
        e
    }
}

/// A number at or after `from` whose name is not `taken`: the lowest, if
/// the taken numbers from `from` on have no gaps, which they do not unless
/// something was removed from a day's trash by hand; any free number makes
/// a valid name. It gallops (1, 2, 4, …) to a free number and then halves
/// back to the first, so a name trashed thousands of times in a day costs
/// a few dozen `lstat`s, not thousands.
fn free_number(taken: &dyn Fn(u32) -> io::Result<bool>, from: u32) -> io::Result<u32> {
    if !taken(from)? {
        return Ok(from);
    }
    // `low` is taken; find a free `high` above it.
    let (mut low, mut step) = (from, 1u32);
    let mut high = loop {
        let candidate = low
            .checked_add(step)
            .ok_or_else(|| io::Error::other("no free name in the trash"))?;
        if !taken(candidate)? {
            break candidate;
        }
        low = candidate;
        step = step.saturating_mul(2);
    };
    // The first free number in (low, high], as far as halving can tell.
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        if taken(mid)? {
            low = mid;
        } else {
            high = mid;
        }
    }
    Ok(high)
}

/// Make the directories an entry from `original` goes into, under the
/// day's, and return where they are relative to `.delocal/trash/`. A
/// directory that is missing is made; a name taken by something else is
/// numbered, as a file's would be. `.delocal/trash/` is made if missing, but
/// never `.delocal/` (see [`crate::tmp::ensure`]).
fn parents(fs: &dyn Fs, day: &str, original: &RelPath) -> io::Result<PathBuf> {
    let trash = Path::new(DIR);
    let mut at = PathBuf::from(day);
    for fixed in [Path::new(""), at.as_path()] {
        if directory(fs, trash, fixed)? == Directory::Taken {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                trash.join(fixed).display()
            )));
        }
    }
    let names: Vec<&str> = original.components().collect();
    for name in &names[..names.len().saturating_sub(1)] {
        let mut n = 0;
        loop {
            let next = at.join(numbered(name, Kind::Dir, n));
            let usable = match directory(fs, trash, &next)? {
                Directory::Made => true,
                Directory::Found => exact(fs, trash, &next)?,
                Directory::Taken => false,
            };
            if usable {
                at = next;
                break;
            }
            n += 1;
        }
    }
    Ok(at)
}

/// What [`directory`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Directory {
    /// Nothing was there, and it made one.
    Made,
    /// A directory was there.
    Found,
    /// Something else was there.
    Taken,
}

/// Make sure `trash/at` is a directory, making it if nothing is there.
fn directory(fs: &dyn Fs, trash: &Path, at: &Path) -> io::Result<Directory> {
    let path = trash.join(at);
    loop {
        match fs.lstat(&path) {
            Ok(stat) if stat.kind == FileKind::Dir => return Ok(Directory::Found),
            Ok(_) => return Ok(Directory::Taken),
            Err(e) if e.kind() == io::ErrorKind::NotFound => match fs.create_dir(&path) {
                Ok(()) => return Ok(Directory::Made),
                // Made by someone else a moment ago: look again.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            },
            Err(e) => return Err(e),
        }
    }
}

/// Whether the directory found at `trash/at` is listed by its parent
/// under exactly that name. On a filesystem that folds names (§7.3) a
/// `stat` finds a differently spelt directory too, and an entry put there
/// would get a row naming a path the disk does not hold; such a name is
/// numbered instead.
fn exact(fs: &dyn Fs, trash: &Path, at: &Path) -> io::Result<bool> {
    let parent = trash.join(at.parent().unwrap_or(Path::new("")));
    let Some(name) = at.file_name() else {
        return Ok(true);
    };
    Ok(fs.read_dir(&parent)?.iter().any(|listed| listed == name))
}

/// The bytes in what is at `path`: a file's size, a symlink's target
/// length, and for a directory everything in it on `device`, the folder
/// root's. What cannot be read counts for nothing, and neither does a disk
/// mounted inside.
pub fn size_of(fs: &dyn Fs, path: &Path, device: u64) -> u64 {
    let Ok(stat) = fs.lstat(path) else {
        return 0;
    };
    match stat.kind {
        FileKind::Dir if stat.dev != device => 0,
        FileKind::Dir => fs.read_dir(path).map_or(0, |names| {
            names
                .iter()
                .map(|name| size_of(fs, &path.join(name), device))
                .fold(0, u64::saturating_add)
        }),
        FileKind::File | FileKind::Symlink => stat.size,
        FileKind::Other => 0,
    }
}

/// Remove what is at `path` in the trash, a directory with everything in
/// it. Nothing there is not an error. A directory on another device than
/// `device` (a disk mounted in the trash) is never entered or removed:
/// that is an error, and what is beneath it stays.
fn remove_all(fs: &dyn Fs, path: &Path, device: u64) -> io::Result<()> {
    let stat = match fs.lstat(path) {
        Ok(stat) => stat,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let gone = if stat.kind == FileKind::Dir {
        if stat.dev != device {
            return Err(io::Error::other(format!(
                "{} is on another device: a disk is mounted there",
                path.display()
            )));
        }
        for name in fs.read_dir(path)? {
            remove_all(fs, &path.join(name), device)?;
        }
        fs.remove_dir(path)
    } else {
        fs.remove_file(path)
    };
    match gone {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Remove the directories above `row` that are now empty, up to the trash
/// itself, which stays.
fn remove_empty_parents(fs: &dyn Fs, row: &Path) {
    let mut above = row.parent();
    while let Some(dir) = above.filter(|dir| !dir.as_os_str().is_empty()) {
        if fs.remove_dir(&Path::new(DIR).join(dir)).is_err() {
            return;
        }
        above = dir.parent();
    }
}

/// What [`prune`] did.
#[derive(Debug, Default)]
pub struct Pruned {
    /// The rows of the entries removed, to delete in the store.
    pub writes: Vec<HostWrite>,
    /// Entries that could not be removed, which keep their rows and are
    /// tried again at the next pruning.
    pub failed: Vec<(PathBuf, io::Error)>,
}

/// Prune the trash of `folder`, whose rows are `rows`, at `now` (§8.4):
/// every entry older than the policy's age, then the oldest first while
/// the rest exceed its size, with the directories they leave empty.
pub fn prune(
    fs: &dyn Fs,
    folder: FolderId,
    rows: &[TrashRow],
    now: Timestamp,
    policy: Policy,
) -> Pruned {
    let mut rows: Vec<&TrashRow> = rows.iter().collect();
    rows.sort_by(|a, b| (a.trashed_at, &a.trashed_path).cmp(&(b.trashed_at, &b.trashed_path)));
    let mut total = rows.iter().map(|row| row.size).fold(0, u64::saturating_add);
    let mut pruned = Pruned::default();
    let device = match fs.lstat(Path::new("")) {
        Ok(stat) => stat.dev,
        Err(e) => {
            pruned.failed.push((PathBuf::new(), e));
            return pruned;
        }
    };
    for row in rows {
        let old = now.since(row.trashed_at) > policy.max_age_nanos;
        if !old && total <= policy.max_bytes {
            break;
        }
        match remove_all(fs, &Path::new(DIR).join(&row.trashed_path), device) {
            Ok(()) => {
                total = total.saturating_sub(row.size);
                remove_empty_parents(fs, &row.trashed_path);
                pruned.writes.push(HostWrite::RemoveTrash {
                    folder,
                    trashed_path: row.trashed_path.clone(),
                });
            }
            Err(e) => pruned.failed.push((row.trashed_path.clone(), e)),
        }
    }
    pruned
}

/// Bring the trash's rows into line with what is in it, at daemon start:
/// a row whose entry is gone is dropped, and an entry with no row (a crash
/// between a move and the report that carried its row) is given one, dated
/// `now` and hashed from the disk. Its original path is its place under the
/// day, which keeps any `~N` it was given. What cannot be read keeps no
/// row.
pub fn reconcile(
    fs: &dyn Fs,
    folder: FolderId,
    rows: &[TrashRow],
    now: Timestamp,
) -> Vec<HostWrite> {
    let mut writes = Vec::new();
    let Ok(device) = fs.lstat(Path::new("")).map(|stat| stat.dev) else {
        return writes;
    };
    let known: BTreeSet<&Path> = rows.iter().map(|row| row.trashed_path.as_path()).collect();
    for row in rows {
        let at = Path::new(DIR).join(&row.trashed_path);
        if matches!(fs.lstat(&at), Err(e) if e.kind() == io::ErrorKind::NotFound) {
            writes.push(HostWrite::RemoveTrash {
                folder,
                trashed_path: row.trashed_path.clone(),
            });
        }
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(dir) = stack.pop() {
        let Ok(names) = fs.read_dir(&Path::new(DIR).join(&dir)) else {
            continue;
        };
        for name in names {
            let row = dir.join(&name);
            if known.contains(row.as_path()) {
                continue;
            }
            let at = Path::new(DIR).join(&row);
            let Ok(stat) = fs.lstat(&at) else {
                continue;
            };
            let hash = match stat.kind {
                // A disk mounted in the trash is not the trash's.
                FileKind::Dir if stat.dev != device => continue,
                FileKind::Dir => {
                    stack.push(row);
                    continue;
                }
                FileKind::File => hash_file(fs, &at),
                FileKind::Symlink => fs
                    .read_link(&at)
                    .map(|target| hash_target(target.as_os_str().as_bytes())),
                FileKind::Other => continue,
            };
            let (Ok(hash), Some(original)) = (hash, original(&row)) else {
                continue;
            };
            writes.push(HostWrite::PutTrash {
                folder,
                row: TrashRow {
                    trashed_path: row,
                    original_path: original,
                    hash,
                    trashed_at: now,
                    size: stat.size,
                },
            });
        }
    }
    writes
}

/// The path an entry at `row` under its day came from, as far as the trash
/// can tell: the rest of the path after the day.
fn original(row: &Path) -> Option<RelPath> {
    let mut parts = row.components();
    parts.next()?;
    let rest: Vec<&str> = parts
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    RelPath::new(rest.join("/")).ok()
}

/// BLAKE3 of the file at `path`, streamed.
pub(crate) fn hash_file(fs: &dyn Fs, path: &Path) -> io::Result<ContentHash> {
    let mut file = fs.open_read(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0; CHUNK];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(test)]
mod tests;
