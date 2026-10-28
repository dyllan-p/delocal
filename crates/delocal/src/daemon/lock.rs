//! One daemon per state directory (DESIGN.md §11).
//!
//! The state directory holds the store and this machine's identity, and two
//! daemons on one would each act on the folders as if the other did not
//! exist: two engines committing into one folder, two writers interleaving
//! their groups. So the daemon takes an exclusive lock on the directory
//! before it reads or writes anything in it, and a second daemon refuses to
//! start. The lock is `flock` on the directory itself, so the layout gains
//! no file, and the kernel releases it when the process ends however it
//! ends, a `kill -9` included: a lock that could outlive its daemon would
//! keep the next one from starting.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

/// The held lock; dropping it releases it.
#[derive(Debug)]
pub struct StateLock {
    _dir: File,
}

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another daemon holds it.
    Held { dir: PathBuf },
    /// The directory could not be opened or locked.
    Io { dir: PathBuf, error: io::Error },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Held { dir } => write!(
                f,
                "another delocal daemon is running with the state directory {}",
                dir.display()
            ),
            Self::Io { dir, error } => write!(f, "could not lock {}: {error}", dir.display()),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Held { .. } => None,
            Self::Io { error, .. } => Some(error),
        }
    }
}

/// Take the lock on `dir`, without waiting: if another process holds it,
/// [`LockError::Held`].
pub fn lock(dir: &Path) -> Result<StateLock, LockError> {
    let io = |error: io::Error| LockError::Io {
        dir: dir.to_path_buf(),
        error,
    };
    let file = File::open(dir).map_err(io)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(StateLock { _dir: file }),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(LockError::Held {
            dir: dir.to_path_buf(),
        }),
        Err(e) => Err(io(e.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_is_refused_until_the_first_goes() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock(dir.path()).unwrap();
        assert!(matches!(lock(dir.path()), Err(LockError::Held { .. })));
        drop(first);
        let again = lock(dir.path()).unwrap();
        drop(again);
    }

    #[test]
    fn a_missing_directory_is_an_error_not_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            lock(&dir.path().join("missing")),
            Err(LockError::Io { .. })
        ));
    }
}
