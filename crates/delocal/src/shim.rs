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
//!
//! **Which writes keep the pairs.** A commit that set an mtime puts the
//! pair if the filesystem kept another, and removes the path's old pair if
//! it kept the one asked for ([`kept`]). A commit that leaves the path
//! empty removes its pair ([`gone`]). A file displaced to a conflict copy
//! takes its pair with it, since the copy's record carries the mtime the
//! pair stands for (§7.6). A pair is otherwise left as it is: one whose
//! stored mtime is no longer on disk translates nothing.
//!
//! **Durability.** A `Write`'s pair goes with its report, in its group
//! (§11), like every write a commit leaves. A `SetMeta`'s is made durable
//! before the commit reports. A crash that lost it would leave the landed
//! `SetMeta` (a revert's reset among them) looking like a local touch at
//! the restart: the retry's guard would find the stored mtime with nothing
//! to translate it, so its observation would not be what the commit leaves,
//! which is what lets a failure count as the landing (§7.5).

use std::collections::BTreeMap;

use delocal_engine::{FolderId, Kind, Observed, RelPath};

use crate::store::{HostWrite, Shim};

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

/// The write that keeps `path`'s pair true once the host has asked for
/// `requested_ns` there and read back `stored_ns`: the pair if the two
/// differ and it is not already kept, the removal of an old pair if they
/// agree, and otherwise nothing.
pub fn kept(
    folder: FolderId,
    path: &RelPath,
    requested_ns: i64,
    stored_ns: i64,
    shims: &dyn Shims,
) -> Option<HostWrite> {
    if requested_ns == stored_ns {
        return gone(folder, path, shims);
    }
    let shim = Shim {
        path: path.clone(),
        requested_ns,
        stored_ns,
    };
    (shims.pair(path) != Some(&shim)).then_some(HostWrite::PutShim { folder, shim })
}

/// The removal of `path`'s pair, if it has one.
pub fn gone(folder: FolderId, path: &RelPath, shims: &dyn Shims) -> Option<HostWrite> {
    shims.pair(path).map(|_| HostWrite::RemoveShim {
        folder,
        path: path.clone(),
    })
}

/// The pair of a file displaced from `from` to `to`, its conflict copy,
/// if it had one (§7.6).
pub fn moved(
    folder: FolderId,
    from: &RelPath,
    to: &RelPath,
    shims: &dyn Shims,
) -> Option<HostWrite> {
    shims.pair(from).map(|shim| HostWrite::PutShim {
        folder,
        shim: Shim {
            path: to.clone(),
            ..shim.clone()
        },
    })
}

#[cfg(test)]
mod tests {
    use delocal_engine::ContentHash;

    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    fn folder() -> FolderId {
        FolderId::from_bytes([7; 16])
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

    #[test]
    fn kept_puts_a_pair_that_differs_and_removes_one_that_no_longer_does() {
        let none = pairs(&[]);
        let some = pairs(&[("f", 5, 4)]);
        let put = |requested_ns, stored_ns| {
            Some(HostWrite::PutShim {
                folder: folder(),
                shim: Shim {
                    path: p("f"),
                    requested_ns,
                    stored_ns,
                },
            })
        };
        let remove = Some(HostWrite::RemoveShim {
            folder: folder(),
            path: p("f"),
        });
        // Kept to the nanosecond: no pair, and an old one goes.
        assert_eq!(kept(folder(), &p("f"), 7, 7, &none), None);
        assert_eq!(kept(folder(), &p("f"), 7, 7, &some), remove);
        // Kept coarser: a pair, unless that very pair is kept already.
        assert_eq!(kept(folder(), &p("f"), 7, 6, &none), put(7, 6));
        assert_eq!(kept(folder(), &p("f"), 7, 6, &some), put(7, 6));
        assert_eq!(kept(folder(), &p("f"), 5, 4, &some), None);
        assert_eq!(gone(folder(), &p("f"), &some), remove);
        assert_eq!(gone(folder(), &p("f"), &none), None);
    }

    #[test]
    fn a_displaced_file_takes_its_pair_to_its_copy() {
        let some = pairs(&[("f", 5, 4)]);
        assert_eq!(
            moved(folder(), &p("f"), &p("f.conflict"), &some),
            Some(HostWrite::PutShim {
                folder: folder(),
                shim: Shim {
                    path: p("f.conflict"),
                    requested_ns: 5,
                    stored_ns: 4,
                },
            })
        );
        assert_eq!(moved(folder(), &p("g"), &p("g.conflict"), &some), None);
    }
}
