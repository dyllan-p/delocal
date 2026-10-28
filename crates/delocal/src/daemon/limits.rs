//! The open-file limit (DESIGN.md §7.3, §7.5): raised at start.
//!
//! The daemon holds a descriptor for every directory a scan is in, one
//! for each file a hashing thread reads, the store's, and a folder root per
//! operation. macOS starts processes with a soft limit of 256, which a deep
//! tree scanned with eight hashers beside a commit can run into, and a scan
//! that ran out would skip what it could not open. The soft limit is raised
//! to the hard one, which needs no privilege. macOS refuses a soft limit
//! above `OPEN_MAX` (10,240) for most hard limits, unlimited included, so
//! that is tried next.

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// macOS's `OPEN_MAX`, from `<sys/syslimits.h>`.
const OPEN_MAX: u64 = 10_240;

/// What [`raise_open_files`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raised {
    /// The soft limit went from `from` to `to`.
    To { from: u64, to: u64 },
    /// It was already as high as it can go.
    Already { at: Option<u64> },
    /// Every higher value was refused; it stays at `at`.
    Refused { at: u64 },
}

/// Raise the soft limit on open files as far as the hard limit allows.
pub fn raise_open_files() -> Raised {
    let limit = getrlimit(Resource::Nofile);
    // `None` is unlimited.
    let Some(current) = limit.current else {
        return Raised::Already { at: None };
    };
    let hard = limit.maximum.unwrap_or(u64::MAX);
    // Highest first: once a target is no higher than the limit already is,
    // the limit is as high as it goes.
    for target in [hard, hard.min(OPEN_MAX)] {
        if target <= current {
            return Raised::Already { at: Some(current) };
        }
        let new = Rlimit {
            current: (target != u64::MAX).then_some(target),
            maximum: limit.maximum,
        };
        if setrlimit(Resource::Nofile, new).is_ok() {
            return Raised::To {
                from: current,
                to: target,
            };
        }
    }
    Raised::Refused { at: current }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raising the test process's own soft limit is harmless, and
    /// raising it again finds it raised.
    #[test]
    fn the_soft_limit_ends_at_the_hard_one_or_at_open_max() {
        let raised = raise_open_files();
        let limit = getrlimit(Resource::Nofile);
        let hard = limit.maximum.unwrap_or(u64::MAX);
        match raised {
            Raised::To { from, to } => {
                assert!(to > from);
                assert_eq!(limit.current, Some(to));
            }
            Raised::Already { .. } => {}
            Raised::Refused { at } => panic!("refused at {at}, hard {hard}"),
        }
        let current = limit.current.unwrap_or(u64::MAX);
        assert!(
            current == hard || current == OPEN_MAX.min(hard),
            "{limit:?}"
        );
        assert!(matches!(raise_open_files(), Raised::Already { .. }));
    }
}
