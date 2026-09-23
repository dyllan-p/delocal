//! Seeds the simulator found and the engine was fixed for (DESIGN.md
//! §14.4: every bug gets a simulator step that reproduces it before it is
//! fixed). Each test pins the seed, the knobs and the minimal step list the
//! shrinker produced, so the fix is guarded by exactly what found the bug.

use crate::steps::Step;
use crate::{Knobs, run_twice};

fn passes(seed: u64, steps: &[Step]) {
    if let Err(f) = run_twice(seed, &Knobs::default(), steps) {
        panic!("{f}");
    }
}

/// The same content edited on every node at once: the identical-content
/// merges tie on every field and were counted as rule-5 conflict decisions.
#[test]
fn identical_content_everywhere_is_not_a_winner_fallback() {
    passes(
        0,
        &[Step::Everywhere {
            path: 6,
            contents: vec![Some(2), Some(2)],
        }],
    );
}

/// The same directory created on two nodes: the same tie, on directories.
#[test]
fn the_same_directory_created_twice_is_not_a_winner_fallback() {
    passes(
        1000,
        &[
            Step::Mkdir { node: 5, dir: 2 },
            Step::Tier {
                a: 0,
                b: 2,
                tier: 2,
            },
            Step::Create {
                node: 1,
                path: 11,
                content: 4,
            },
            Step::Mkdir { node: 3, dir: 2 },
        ],
    );
}

/// A version already accepted and being fetched arrived again relayed in a
/// batch that tripped the brake, was quarantined, and its own fetch then
/// adopted it from quarantine.
#[test]
fn a_relayed_copy_of_a_wanted_version_is_not_quarantined() {
    passes(
        5000,
        &[
            Step::Modify {
                node: 0,
                path: 8,
                content: 4,
            },
            Step::Create {
                node: 7,
                path: 11,
                content: 4,
            },
            Step::Settle { secs: 36 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(2), None, Some(3)],
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(5), Some(3), None],
            },
            Step::Offline { node: 5 },
            Step::MassModify {
                node: 3,
                fraction: 59,
                content: 1,
            },
            Step::Tier {
                a: 2,
                b: 4,
                tier: 0,
            },
            Step::Chmod { node: 6, path: 4 },
        ],
    );
}
