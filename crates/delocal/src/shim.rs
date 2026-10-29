//! The mtime precision shim (DESIGN.md §7.3): how a filesystem with coarse
//! timestamps holds a received file's mtime as far as the engine can tell.
//!
//! FAT keeps an mtime to 2 s, ext4 with 128-byte inodes and some network
//! mounts to the second. A file received with a finer mtime is stored with a
//! coarser one, so the next scan would see it touched, give it a new
//! version and announce it, and every peer would apply the touch, forever.
//! So after every `Write` and `SetMeta` the host reads back the mtime the
//! filesystem kept, and if it is not the one asked for, keeps the pair
//! (`mtime_shim`, §11). From then on, wherever the host turns a file's stat
//! into what the engine sees (the scan, a single observation, the commit
//! guard), a file at that path whose mtime on disk is still the stored one
//! is reported with the requested one ([`Shims::reported`]). The engine
//! never sees the difference. Any other mtime there, a user's edit for
//! one, is reported as it is.

use std::collections::BTreeMap;

use delocal_engine::{Kind, Observed, RelPath};

use crate::store::Shim;

/// One folder's pairs, as the host keeps them (`mtime_shim`, §11).
pub trait Shims: Send + Sync {
    /// The pair kept for `path`, if there is one.
    fn pair(&self, path: &RelPath) -> Option<&Shim>;

    /// The mtime the engine is told for the file at `path` whose mtime on
    /// disk is `stored_ns`: the one the host asked for while the file holds
    /// what the filesystem kept of it, and otherwise what is on disk.
    fn reported(&self, path: &RelPath, stored_ns: i64) -> i64 {
        match self.pair(path) {
            Some(shim) if shim.stored_ns == stored_ns => shim.requested_ns,
            _ => stored_ns,
        }
    }

    /// `seen` at `path` as the engine is told it: a file's mtime through
    /// [`reported`](Shims::reported). Directories and symlinks carry no
    /// mtime (§7.1).
    fn observed(&self, path: &RelPath, seen: Observed) -> Observed {
        if seen.kind == Kind::File {
            Observed {
                mtime_ns: self.reported(path, seen.mtime_ns),
                ..seen
            }
        } else {
            seen
        }
    }
}

impl Shims for BTreeMap<RelPath, Shim> {
    fn pair(&self, path: &RelPath) -> Option<&Shim> {
        self.get(path)
    }
}

#[cfg(test)]
mod tests {
    use delocal_engine::ContentHash;

    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn pairs(list: &[(&str, i64, i64)]) -> BTreeMap<RelPath, Shim> {
        list.iter()
            .map(|(path, requested_ns, stored_ns)| {
                let shim = Shim {
                    path: p(path),
                    requested_ns: *requested_ns,
                    stored_ns: *stored_ns,
                };
                (p(path), shim)
            })
            .collect()
    }

    fn seen(kind: Kind, mtime_ns: i64) -> Observed {
        Observed {
            kind,
            size: 1,
            mtime_ns,
            exec: false,
            hash: ContentHash::EMPTY,
        }
    }

    /// The requested mtime only for the exact stored one at that path;
    /// anything else as it is.
    #[test]
    fn a_pair_reports_the_requested_mtime_only_for_the_stored_one() {
        let shims = pairs(&[("f", 1_234_567_891, 0)]);
        assert_eq!(shims.reported(&p("f"), 0), 1_234_567_891);
        assert_eq!(shims.reported(&p("f"), 2_000_000_000), 2_000_000_000);
        assert_eq!(shims.reported(&p("f"), 1_234_567_891), 1_234_567_891);
        assert_eq!(shims.reported(&p("g"), 0), 0);
        assert_eq!(
            shims.observed(&p("f"), seen(Kind::File, 0)).mtime_ns,
            1_234_567_891
        );
        assert_eq!(
            shims.observed(&p("f"), seen(Kind::Dir, 0)),
            seen(Kind::Dir, 0)
        );
        assert_eq!(
            shims.observed(&p("f"), seen(Kind::Symlink, 0)),
            seen(Kind::Symlink, 0)
        );
    }
}
