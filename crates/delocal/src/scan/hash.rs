//! Hashing a file for a scan (DESIGN.md §7.1, §7.3): BLAKE3 of its content,
//! streamed, and the stability check around it.
//!
//! **Settled first.** A file is not hashed until its inode change time is
//! at least 2 s old ([`settled`]), so a file still being written is not
//! announced half-written. The change time, not the mtime, because only the
//! kernel sets it: a file copied with its mtime preserved, or carrying an
//! mtime from the future off a device with a wrong clock, is judged by when
//! it last changed here. `now` is the caller's, the monotonic clock of §7.8,
//! never read here, so tests decide what has settled.
//!
//! **Unchanged while hashed.** After the last byte the file is stated again
//! by name, through the directory the walk held, and the hash counts only if
//! its size, mtime and change time are what they were, and exactly as many
//! bytes were read as the size said (§7.3). A write during hashing moves the
//! change time, whatever the writer does to the mtime afterwards, and so do
//! a chmod and a rename; a file renamed over this one during hashing was
//! written or moved within the last 2 s, so its change time is not this
//! file's settled one. A file that changed is [`Hashed::Unstable`], and its
//! hash is thrown away: a torn hash would announce content that never
//! existed. So is a file that was something else by the time it was opened
//! (a symlink, a FIFO, a directory), which the open refuses without
//! following or waiting.
//!
//! Symlinks are hashed from their target in the walk (§7.1); they have no
//! content to stream, and are replaced whole.

use std::ffi::OsStr;
use std::io::{self, Read};

use delocal_engine::{ContentHash, Timestamp};

use crate::fs::{Dir, NotAFile, Stat};

/// How long ago a file must last have changed before it is hashed, and how
/// long after an unstable one the host observes it again (§7.3).
pub const SETTLE_NANOS: i64 = 2_000_000_000;

/// The most read at once: the size of a transfer chunk (§7.5 step 3).
pub const CHUNK: usize = 1 << 20;

/// Whether a file whose inode change time is `ctime_ns` has been left
/// alone for 2 s at `now`. A change time ahead of `now` (this machine's
/// clock stepped back) has not.
pub fn settled(ctime_ns: i64, now: Timestamp) -> bool {
    now.since(Timestamp::from_unix_nanos(ctime_ns)) >= SETTLE_NANOS
}

/// Whether the re-stat after hashing shows the file as it was (§7.3): its
/// size, mtime and change time.
fn unchanged(before: &Stat, after: &Stat) -> bool {
    (before.size, before.mtime_ns, before.ctime_ns) == (after.size, after.mtime_ns, after.ctime_ns)
}

/// What hashing one file found.
#[derive(Debug)]
pub enum Hashed {
    /// The file's content, which did not change while it was read.
    Stable(ContentHash),
    /// The file changed while it was read, or was no longer a file when it
    /// was opened: the hash, if any, is thrown away.
    Unstable,
    /// The file is gone.
    Vanished,
    /// It could not be opened or read.
    Failed(io::Error),
}

/// Hash the file `name` in `dir`, which the walk stated as `before`, and
/// check it did not change meanwhile (see the module docs).
pub fn hash_file(dir: &dyn Dir, name: &OsStr, before: &Stat) -> Hashed {
    let mut file = match dir.open_read(name) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Hashed::Vanished,
        Err(e) if NotAFile::of(&e).is_some() => return Hashed::Unstable,
        Err(e) => return Hashed::Failed(e),
    };
    // A small file needs no more room than it has bytes, and an empty one
    // some room to see that it is still empty.
    let room = usize::try_from(before.size).map_or(CHUNK, |size| size.clamp(1, CHUNK));
    let mut buf = vec![0; room];
    let mut hasher = blake3::Hasher::new();
    let mut read: u64 = 0;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                read = read.saturating_add(n as u64);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Hashed::Failed(e),
        }
    }
    drop(file);
    match dir.lstat(name) {
        Ok(after) if unchanged(before, &after) && read == before.size => {
            Hashed::Stable(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
        }
        Ok(_) => Hashed::Unstable,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Hashed::Vanished,
        Err(e) => Hashed::Failed(e),
    }
}

/// The hash of a symlink's target (§7.1), from its raw bytes.
pub fn hash_target(target: &[u8]) -> ContentHash {
    ContentHash::from_bytes(*blake3::hash(target).as_bytes())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fs::{Folder, Fs, RealFolder};

    fn name(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    fn blake3_of(bytes: &[u8]) -> ContentHash {
        ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
    }

    /// The folder's root, held, with the stat of `f` in it.
    fn held(fs: &dyn Fs) -> (Box<dyn Dir>, Stat) {
        let root = fs.open_dir(Path::new("")).unwrap();
        let stat = root.lstat(name("f")).unwrap();
        (root, stat)
    }

    #[test]
    fn settled_means_changed_two_seconds_ago_and_not_in_the_future() {
        let now = Timestamp::from_unix_nanos(10 * SETTLE_NANOS);
        let at = |ago: i64| now.as_unix_nanos() - ago;
        assert!(settled(at(SETTLE_NANOS), now));
        assert!(settled(at(3600 * 1_000_000_000), now));
        assert!(!settled(at(SETTLE_NANOS - 1), now));
        assert!(!settled(at(0), now));
        assert!(
            !settled(at(-5 * 1_000_000_000), now),
            "a change time ahead of now"
        );
        assert!(settled(i64::MIN, Timestamp::from_unix_nanos(i64::MAX)));
    }

    #[test]
    fn a_file_hashes_to_the_blake3_of_its_bytes_whatever_its_size() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = RealFolder::new(tmp.path()).open().unwrap();
        for size in [0, 1, 4095, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 5] {
            let bytes: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
            std::fs::write(tmp.path().join("f"), &bytes).unwrap();
            let (root, before) = held(&*fs);
            match hash_file(&*root, name("f"), &before) {
                Hashed::Stable(hash) => assert_eq!(hash, blake3_of(&bytes), "{size} bytes"),
                other => panic!("{size} bytes: {other:?}"),
            }
        }
        assert_ne!(blake3_of(b""), ContentHash::EMPTY);
    }

    #[test]
    fn a_file_that_is_not_as_it_was_stated_is_unstable() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = RealFolder::new(tmp.path()).open().unwrap();
        std::fs::write(tmp.path().join("f"), "content").unwrap();
        let (root, before) = held(&*fs);
        let unstable =
            |before: &Stat| matches!(hash_file(&*root, name("f"), before), Hashed::Unstable);
        assert!(!unstable(&before));
        // What the re-stat compares (§7.3): the mtime, the change time, and
        // the size (with as many bytes read as the old one said).
        let changed = [
            Stat {
                mtime_ns: before.mtime_ns - 1,
                ..before
            },
            Stat {
                ctime_ns: before.ctime_ns - 1,
                ..before
            },
            Stat {
                size: before.size + 1,
                ..before
            },
        ];
        for stat in &changed {
            assert!(unstable(stat), "{stat:?}");
        }
        // And nothing else: a chmod or a rename moves the change time too,
        // so the mode or the inode alone is never the only difference.
        let same = [
            Stat {
                mode: before.mode ^ 0o100,
                ..before
            },
            Stat {
                ino: before.ino + 1,
                ..before
            },
        ];
        for stat in &same {
            assert!(!unstable(stat), "{stat:?}");
        }

        // A file swapped for a symlink before the open is not followed.
        std::fs::remove_file(tmp.path().join("f")).unwrap();
        std::os::unix::fs::symlink("elsewhere", tmp.path().join("f")).unwrap();
        assert!(unstable(&before));

        // And one that is gone has vanished.
        std::fs::remove_file(tmp.path().join("f")).unwrap();
        assert!(matches!(
            hash_file(&*root, name("f"), &before),
            Hashed::Vanished
        ));
    }

    #[cfg(feature = "faults")]
    #[test]
    fn a_file_that_cannot_be_read_fails() {
        use crate::fs::FaultyFolder;
        use crate::fs::faulty::{Fault, Op, Rule, Spec, Trigger};

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), vec![7; 3 * CHUNK]).unwrap();
        let cases = [
            (Op::OpenRead, Trigger::Call(1), Fault::Eacces),
            (Op::Read, Trigger::Offset(CHUNK as u64 + 3), Fault::Eio),
            (Op::Lstat, Trigger::Call(2), Fault::Eio),
        ];
        for (op, at, fail) in cases {
            let rule = Rule {
                op,
                path: "f".into(),
                at,
                fail,
            };
            let folder =
                FaultyFolder::new(RealFolder::new(tmp.path()), Spec { rules: vec![rule] }).unwrap();
            let fs = folder.open().unwrap();
            let (root, before) = held(&*fs);
            match hash_file(&*root, name("f"), &before) {
                Hashed::Failed(e) => assert_eq!(e.raw_os_error(), fail.errno(), "{op:?}"),
                other => panic!("{op:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_symlink_hashes_its_target_bytes() {
        assert_eq!(hash_target(b"../elsewhere"), blake3_of(b"../elsewhere"));
        assert_ne!(hash_target(b"a"), hash_target(b"b"));
    }
}
