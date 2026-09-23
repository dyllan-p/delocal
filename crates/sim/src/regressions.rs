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

/// A conflict's merged version `M` is announced first by the winner's
/// holder, which does not have `M` until it has seen the loser too; it
/// answered `NotAvailable`, was excluded for good, and its later
/// announcement of `M` did not make it a source again, so the want had no
/// source forever.
#[test]
fn a_source_that_announces_the_wanted_version_again_is_asked_again() {
    passes(
        0,
        &[
            Step::Tier {
                a: 2,
                b: 1,
                tier: 2,
            },
            Step::Chmod { node: 7, path: 5 },
            Step::Settle { secs: 12 },
            Step::Touch { node: 6, path: 7 },
            Step::Tier {
                a: 1,
                b: 4,
                tier: 2,
            },
            Step::Modify {
                node: 7,
                path: 1,
                content: 5,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![None, None],
            },
            Step::Tier {
                a: 4,
                b: 5,
                tier: 0,
            },
            Step::Create {
                node: 5,
                path: 2,
                content: 2,
            },
            Step::MassDelete {
                node: 2,
                fraction: 95,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(2), Some(2)],
            },
            Step::Modify {
                node: 2,
                path: 4,
                content: 3,
            },
            Step::Create {
                node: 6,
                path: 1,
                content: 2,
            },
            Step::Settle { secs: 27 },
            Step::Offline { node: 3 },
            Step::Create {
                node: 0,
                path: 6,
                content: 4,
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
            Step::Online { node: 0 },
            Step::Create {
                node: 6,
                path: 5,
                content: 1,
            },
            Step::Create {
                node: 1,
                path: 1,
                content: 4,
            },
            Step::Modify {
                node: 5,
                path: 11,
                content: 2,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 24,
            },
            Step::Touch { node: 2, path: 8 },
            Step::Tier {
                a: 4,
                b: 1,
                tier: 2,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 6,
                },
                delay_secs: 10,
            },
            Step::Delete { node: 0, path: 5 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 45,
            },
            Step::Delete { node: 7, path: 3 },
            Step::Online { node: 1 },
            Step::Rmdir { node: 0, dir: 2 },
            Step::Chmod { node: 7, path: 1 },
            Step::Crash {
                node: 4,
                gap_secs: 35,
            },
            Step::Create {
                node: 5,
                path: 7,
                content: 1,
            },
            Step::Heal { a: 0, b: 2 },
            Step::Settle { secs: 9 },
            Step::Modify {
                node: 2,
                path: 6,
                content: 4,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 41,
            },
            Step::Create {
                node: 2,
                path: 2,
                content: 3,
            },
            Step::Delete { node: 4, path: 2 },
            Step::Settle { secs: 22 },
            Step::Touch { node: 5, path: 10 },
            Step::Modify {
                node: 0,
                path: 10,
                content: 2,
            },
            Step::Online { node: 2 },
            Step::Rename {
                node: 1,
                from: 2,
                to: 9,
            },
            Step::Create {
                node: 5,
                path: 7,
                content: 1,
            },
            Step::Delete { node: 2, path: 1 },
            Step::Offline { node: 3 },
            Step::Modify {
                node: 2,
                path: 6,
                content: 5,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(4), Some(5), Some(1), Some(5), Some(2), None, Some(1)],
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(2), Some(3), Some(3), None, None],
            },
            Step::Create {
                node: 0,
                path: 7,
                content: 3,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 49,
            },
            Step::Delete { node: 6, path: 1 },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 39,
            },
            Step::Rename {
                node: 3,
                from: 4,
                to: 9,
            },
            Step::Offline { node: 0 },
            Step::Create {
                node: 1,
                path: 7,
                content: 3,
            },
            Step::Symlink {
                node: 4,
                path: 10,
                target: 1,
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(5), Some(2), Some(1), Some(5), Some(3)],
            },
            Step::Crash {
                node: 3,
                gap_secs: 40,
            },
            Step::Symlink {
                node: 6,
                path: 11,
                target: 7,
            },
            Step::Partition { a: 6, b: 0 },
            Step::MassModify {
                node: 7,
                fraction: 82,
                content: 5,
            },
            Step::Create {
                node: 4,
                path: 6,
                content: 3,
            },
            Step::Online { node: 4 },
            Step::Delete { node: 7, path: 1 },
            Step::Online { node: 3 },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(5), None, None],
            },
            Step::User {
                node: 4,
                action: crate::UserAction::DenyAll,
                delay_secs: 43,
            },
            Step::Chmod { node: 1, path: 11 },
            Step::Online { node: 4 },
            Step::Symlink {
                node: 7,
                path: 9,
                target: 3,
            },
            Step::Delete { node: 4, path: 8 },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Crash {
                node: 0,
                gap_secs: 22,
            },
            Step::Rename {
                node: 5,
                from: 10,
                to: 5,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 39,
            },
            Step::MassDelete {
                node: 6,
                fraction: 91,
            },
            Step::Delete { node: 5, path: 3 },
            Step::Delete { node: 1, path: 7 },
            Step::Tier {
                a: 6,
                b: 6,
                tier: 1,
            },
            Step::Modify {
                node: 3,
                path: 4,
                content: 4,
            },
            Step::Modify {
                node: 2,
                path: 5,
                content: 1,
            },
        ],
    );
}

/// A scan of a frozen path thawed its entry while the folder was still
/// paused, and the entry, from a batch the brake had held for its other
/// paths, went through the brake alone, passed, and was committed while
/// its batch was still under review.
#[test]
fn an_entry_of_a_held_batch_is_not_committed_before_approve() {
    passes(
        5,
        &[
            Step::MassDelete {
                node: 2,
                fraction: 65,
            },
            Step::Modify {
                node: 0,
                path: 2,
                content: 4,
            },
            Step::Settle { secs: 20 },
            Step::Create {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::Symlink {
                node: 6,
                path: 6,
                target: 11,
            },
            Step::MassDelete {
                node: 1,
                fraction: 62,
            },
            Step::Modify {
                node: 5,
                path: 10,
                content: 2,
            },
            Step::Online { node: 5 },
            Step::Crash {
                node: 7,
                gap_secs: 96,
            },
            Step::Rename {
                node: 4,
                from: 6,
                to: 7,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(3), None, Some(5), Some(4), Some(3)],
            },
            Step::Create {
                node: 3,
                path: 0,
                content: 1,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(4), Some(3), None, None, Some(2), None],
            },
            Step::Symlink {
                node: 7,
                path: 1,
                target: 4,
            },
            Step::Partition { a: 6, b: 4 },
            Step::Delete { node: 7, path: 10 },
            Step::Offline { node: 0 },
            Step::MassModify {
                node: 4,
                fraction: 56,
                content: 3,
            },
            Step::Delete { node: 1, path: 5 },
            Step::Symlink {
                node: 5,
                path: 6,
                target: 9,
            },
            Step::Crash {
                node: 5,
                gap_secs: 56,
            },
            Step::Online { node: 4 },
            Step::Modify {
                node: 3,
                path: 8,
                content: 3,
            },
            Step::Touch { node: 6, path: 9 },
            Step::Delete { node: 6, path: 7 },
            Step::Modify {
                node: 6,
                path: 9,
                content: 3,
            },
            Step::Chmod { node: 2, path: 4 },
            Step::Crash {
                node: 1,
                gap_secs: 66,
            },
            Step::Delete { node: 4, path: 5 },
            Step::Crash {
                node: 0,
                gap_secs: 72,
            },
            Step::Chmod { node: 6, path: 7 },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 4,
            },
            Step::Partition { a: 2, b: 6 },
            Step::User {
                node: 0,
                action: crate::UserAction::Rules {
                    hold_count: 1,
                    hold_pct: 1,
                },
                delay_secs: 35,
            },
            Step::Partition { a: 1, b: 3 },
            Step::Chmod { node: 5, path: 8 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(4), Some(4), Some(1), None, Some(2)],
            },
            Step::Create {
                node: 1,
                path: 7,
                content: 3,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 4,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 2,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Rules {
                    hold_count: 4,
                    hold_pct: 18,
                },
                delay_secs: 35,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::Revert,
                delay_secs: 14,
            },
            Step::Rename {
                node: 0,
                from: 11,
                to: 10,
            },
            Step::Delete { node: 0, path: 7 },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 0,
            },
            Step::Create {
                node: 0,
                path: 2,
                content: 3,
            },
            Step::Heal { a: 4, b: 0 },
            Step::MassModify {
                node: 1,
                fraction: 82,
                content: 1,
            },
            Step::Delete { node: 4, path: 11 },
            Step::Partition { a: 1, b: 1 },
            Step::Create {
                node: 4,
                path: 10,
                content: 4,
            },
            Step::Modify {
                node: 0,
                path: 8,
                content: 1,
            },
        ],
    );
}

/// A crash between a fetch and its commit kept the want's fetched flag
/// while the temp file died with the process; the restarted node asked the
/// host to commit content it did not have, the commit failed, and the entry
/// was deferred at a path with no record and no file, which no scan ever
/// reports.
#[test]
fn a_want_fetched_before_a_crash_is_fetched_again() {
    passes(
        5,
        &[
            Step::Chmod { node: 6, path: 7 },
            Step::Offline { node: 0 },
            Step::MassDelete {
                node: 2,
                fraction: 65,
            },
            Step::Modify {
                node: 0,
                path: 2,
                content: 4,
            },
            Step::Settle { secs: 20 },
            Step::Delete { node: 6, path: 3 },
            Step::Tier {
                a: 4,
                b: 6,
                tier: 2,
            },
            Step::Modify {
                node: 7,
                path: 1,
                content: 5,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 58,
            },
            Step::Rmdir { node: 1, dir: 0 },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(4), Some(4), Some(2), None, Some(4), None],
            },
            Step::Delete { node: 3, path: 7 },
            Step::Settle { secs: 36 },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 56,
            },
            Step::Modify {
                node: 0,
                path: 6,
                content: 2,
            },
            Step::Create {
                node: 1,
                path: 11,
                content: 5,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![None, Some(4), None, None, Some(5), Some(1), None],
            },
            Step::Tier {
                a: 4,
                b: 7,
                tier: 2,
            },
            Step::Chmod { node: 1, path: 7 },
            Step::Modify {
                node: 5,
                path: 1,
                content: 3,
            },
            Step::Crash {
                node: 3,
                gap_secs: 7,
            },
            Step::Partition { a: 0, b: 0 },
            Step::Settle { secs: 28 },
            Step::Online { node: 5 },
            Step::Symlink {
                node: 2,
                path: 6,
                target: 1,
            },
            Step::Tier {
                a: 6,
                b: 4,
                tier: 2,
            },
            Step::Heal { a: 5, b: 1 },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Create {
                node: 2,
                path: 1,
                content: 1,
            },
        ],
    );
}
