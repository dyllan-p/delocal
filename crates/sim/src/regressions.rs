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

/// A want created from the deferred set (an unfreeze, a superseded entry, a
/// re-classification under an observable want) skipped quarantine matching
/// and the brake, and could adopt a quarantined version.
#[test]
fn entries_readmitted_from_the_deferred_set_respect_the_quarantine() {
    passes(
        9000,
        &[
            Step::Online { node: 2 },
            Step::Delete { node: 5, path: 11 },
            Step::Everywhere {
                path: 5,
                contents: vec![None, None, None, Some(5), Some(2)],
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 57,
            },
            Step::Partition { a: 6, b: 0 },
            Step::Heal { a: 0, b: 6 },
            Step::Modify {
                node: 3,
                path: 1,
                content: 3,
            },
            Step::Create {
                node: 2,
                path: 6,
                content: 4,
            },
            Step::Create {
                node: 1,
                path: 3,
                content: 3,
            },
            Step::Modify {
                node: 0,
                path: 6,
                content: 2,
            },
            Step::Create {
                node: 3,
                path: 10,
                content: 3,
            },
            Step::Partition { a: 5, b: 1 },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 11,
            },
            Step::Partition { a: 3, b: 5 },
            Step::MassModify {
                node: 7,
                fraction: 68,
                content: 3,
            },
        ],
    );
}

/// `revert` put the announced records back and removed never-announced adds
/// without reporting either, so the persisted index kept the pending
/// records and a restart would have re-announced the reverted changes.
#[test]
fn a_revert_reports_every_index_write() {
    passes(
        0,
        &[
            Step::Settle { secs: 34 },
            Step::Modify {
                node: 1,
                path: 10,
                content: 5,
            },
            Step::Chmod { node: 4, path: 5 },
            Step::Offline { node: 7 },
            Step::Touch { node: 2, path: 2 },
            Step::Settle { secs: 3 },
            Step::Everywhere {
                path: 5,
                contents: vec![None, None, Some(3), Some(4), Some(5), Some(3)],
            },
            Step::User {
                node: 5,
                action: crate::UserAction::DenyAll,
                delay_secs: 2,
            },
            Step::Partition { a: 7, b: 4 },
            Step::Modify {
                node: 3,
                path: 5,
                content: 5,
            },
            Step::Create {
                node: 4,
                path: 5,
                content: 4,
            },
            Step::Offline { node: 1 },
            Step::Delete { node: 7, path: 11 },
            Step::Create {
                node: 5,
                path: 2,
                content: 2,
            },
            Step::MassDelete {
                node: 2,
                fraction: 95,
            },
            Step::Modify {
                node: 1,
                path: 2,
                content: 5,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 5,
            },
        ],
    );
}
