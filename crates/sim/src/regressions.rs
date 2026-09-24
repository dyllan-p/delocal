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

/// Requests named a version, and a conflict's merged version `M` exists
/// nowhere until someone merges: the winner's holders answered
/// `NotAvailable` for `M` although they held its content, and a path whose
/// concurrent holders never all met stayed different across the mesh.
/// Fetches are by hash now (§7.5 steps 2 and 3).
#[test]
fn content_is_fetched_by_hash_from_whoever_holds_it() {
    passes(
        108,
        &[
            Step::Mkdir { node: 6, dir: 1 },
            Step::Crash {
                node: 7,
                gap_secs: 33,
            },
            Step::Offline { node: 3 },
            Step::Create {
                node: 6,
                path: 5,
                content: 3,
            },
            Step::Create {
                node: 5,
                path: 6,
                content: 2,
            },
            Step::Create {
                node: 3,
                path: 2,
                content: 5,
            },
            Step::Create {
                node: 6,
                path: 6,
                content: 1,
            },
            Step::Offline { node: 5 },
            Step::Symlink {
                node: 4,
                path: 7,
                target: 11,
            },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(5), None],
            },
            Step::Rename {
                node: 3,
                from: 5,
                to: 7,
            },
            Step::Tier {
                a: 6,
                b: 2,
                tier: 1,
            },
            Step::Delete { node: 4, path: 2 },
            Step::Heal { a: 3, b: 0 },
            Step::Delete { node: 7, path: 4 },
            Step::Tier {
                a: 6,
                b: 3,
                tier: 0,
            },
            Step::Everywhere {
                path: 11,
                contents: vec![None, Some(3)],
            },
            Step::Modify {
                node: 0,
                path: 11,
                content: 4,
            },
            Step::Offline { node: 3 },
            Step::Modify {
                node: 7,
                path: 1,
                content: 1,
            },
            Step::Rename {
                node: 3,
                from: 1,
                to: 5,
            },
            Step::Symlink {
                node: 4,
                path: 6,
                target: 1,
            },
            Step::Modify {
                node: 5,
                path: 4,
                content: 5,
            },
            Step::Delete { node: 6, path: 1 },
            Step::Create {
                node: 2,
                path: 0,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 9,
                content: 2,
            },
            Step::Modify {
                node: 5,
                path: 2,
                content: 3,
            },
            Step::Create {
                node: 7,
                path: 10,
                content: 2,
            },
        ],
    );
}

/// A batch lost to a crash or a dropped link was never caught up: the
/// receiver took the next batch's `seq_high` as its watermark although
/// it had never seen the sequence numbers below it, so `have_up_to` asked
/// for nothing and a directory's tombstone never reached one node, which
/// kept the directory alive (I3). Batches chain through `seq_low` now and
/// acknowledgements are the contiguous watermark (§7.4).
#[test]
fn a_batch_lost_in_flight_is_caught_up() {
    passes(
        29,
        &[
            Step::Partition { a: 4, b: 6 },
            Step::Create {
                node: 2,
                path: 1,
                content: 4,
            },
            Step::Symlink {
                node: 7,
                path: 8,
                target: 8,
            },
            Step::Touch { node: 0, path: 9 },
            Step::Heal { a: 0, b: 1 },
            Step::Create {
                node: 5,
                path: 2,
                content: 3,
            },
            Step::Mkdir { node: 0, dir: 1 },
            Step::MassModify {
                node: 4,
                fraction: 91,
                content: 2,
            },
            Step::Delete { node: 7, path: 6 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(5), Some(1), Some(2), None, None, Some(2)],
            },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(2), Some(5), None],
            },
            Step::Tier {
                a: 0,
                b: 6,
                tier: 0,
            },
            Step::Settle { secs: 35 },
            Step::Tier {
                a: 5,
                b: 1,
                tier: 1,
            },
            Step::Crash {
                node: 5,
                gap_secs: 27,
            },
            Step::MassModify {
                node: 3,
                fraction: 86,
                content: 1,
            },
            Step::Modify {
                node: 6,
                path: 8,
                content: 4,
            },
            Step::Symlink {
                node: 7,
                path: 10,
                target: 8,
            },
            Step::Touch { node: 4, path: 11 },
            Step::Modify {
                node: 5,
                path: 7,
                content: 2,
            },
            Step::Modify {
                node: 7,
                path: 2,
                content: 1,
            },
            Step::Chmod { node: 5, path: 2 },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 57,
            },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Heal { a: 6, b: 7 },
            Step::Symlink {
                node: 2,
                path: 6,
                target: 3,
            },
            Step::Settle { secs: 13 },
            Step::Tier {
                a: 0,
                b: 6,
                tier: 0,
            },
            Step::Create {
                node: 1,
                path: 8,
                content: 1,
            },
            Step::Crash {
                node: 6,
                gap_secs: 77,
            },
            Step::Partition { a: 4, b: 7 },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 2,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![
                    Some(4),
                    Some(4),
                    Some(4),
                    Some(4),
                    Some(5),
                    Some(2),
                    Some(5),
                ],
            },
            Step::Create {
                node: 1,
                path: 4,
                content: 1,
            },
            Step::Heal { a: 4, b: 0 },
            Step::Delete { node: 5, path: 3 },
            Step::Heal { a: 5, b: 2 },
            Step::MassDelete {
                node: 1,
                fraction: 75,
            },
            Step::Delete { node: 1, path: 7 },
            Step::Create {
                node: 0,
                path: 6,
                content: 1,
            },
            Step::Modify {
                node: 0,
                path: 2,
                content: 2,
            },
            Step::Rmdir { node: 3, dir: 1 },
            Step::Create {
                node: 0,
                path: 0,
                content: 3,
            },
            Step::Everywhere {
                path: 3,
                contents: vec![None, Some(1), Some(5)],
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(3), None],
            },
            Step::MassModify {
                node: 5,
                fraction: 59,
                content: 1,
            },
            Step::Symlink {
                node: 4,
                path: 2,
                target: 4,
            },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Everywhere {
                path: 6,
                contents: vec![None, Some(3)],
            },
            Step::Heal { a: 5, b: 4 },
            Step::Settle { secs: 30 },
            Step::Create {
                node: 1,
                path: 6,
                content: 3,
            },
            Step::Settle { secs: 26 },
            Step::Delete { node: 7, path: 2 },
            Step::Create {
                node: 3,
                path: 2,
                content: 1,
            },
            Step::Rmdir { node: 1, dir: 0 },
            Step::Online { node: 6 },
            Step::Offline { node: 1 },
            Step::Delete { node: 3, path: 2 },
            Step::Modify {
                node: 6,
                path: 0,
                content: 5,
            },
            Step::Offline { node: 7 },
            Step::Modify {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(3), Some(1), Some(2), Some(2), Some(1)],
            },
            Step::Modify {
                node: 4,
                path: 11,
                content: 2,
            },
            Step::Touch { node: 7, path: 6 },
            Step::Modify {
                node: 7,
                path: 9,
                content: 5,
            },
            Step::Rename {
                node: 3,
                from: 1,
                to: 10,
            },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Crash {
                node: 0,
                gap_secs: 38,
            },
            Step::Online { node: 0 },
            Step::Heal { a: 0, b: 2 },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 59,
            },
            Step::Create {
                node: 3,
                path: 5,
                content: 3,
            },
            Step::Modify {
                node: 4,
                path: 3,
                content: 4,
            },
        ],
    );
}

/// A chmod whose watcher event was dropped was invisible to every later
/// scan: the fast path compared size and mtime, and chmod changes neither,
/// so the index disagreed with the disk about the exec bit forever. The
/// fast path compares the exec bit now (§7.3).
#[test]
fn a_chmod_the_watcher_missed_is_found_by_the_next_scan() {
    passes(
        83,
        &[
            Step::Modify {
                node: 0,
                path: 3,
                content: 2,
            },
            Step::Crash {
                node: 2,
                gap_secs: 50,
            },
            Step::Chmod { node: 2, path: 3 },
        ],
    );
}

/// Same class as above.
#[test]
fn a_missed_chmod_of_a_conflict_copy_is_found_by_the_next_scan() {
    passes(
        37,
        &[
            Step::Partition { a: 1, b: 0 },
            Step::Heal { a: 2, b: 6 },
            Step::Rename {
                node: 6,
                from: 0,
                to: 2,
            },
            Step::Modify {
                node: 2,
                path: 9,
                content: 5,
            },
            Step::Modify {
                node: 2,
                path: 2,
                content: 5,
            },
            Step::Settle { secs: 10 },
            Step::Crash {
                node: 2,
                gap_secs: 112,
            },
            Step::Heal { a: 3, b: 4 },
            Step::Settle { secs: 37 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(3), Some(3), Some(3)],
            },
            Step::Online { node: 2 },
            Step::Rename {
                node: 3,
                from: 2,
                to: 4,
            },
            Step::Delete { node: 3, path: 4 },
            Step::Create {
                node: 3,
                path: 4,
                content: 2,
            },
            Step::Partition { a: 4, b: 6 },
            Step::Chmod { node: 0, path: 8 },
            Step::Delete { node: 3, path: 3 },
            Step::Modify {
                node: 6,
                path: 9,
                content: 5,
            },
            Step::Create {
                node: 3,
                path: 3,
                content: 2,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 14,
                },
                delay_secs: 29,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(4), Some(4), Some(3), Some(1)],
            },
            Step::Create {
                node: 0,
                path: 8,
                content: 3,
            },
            Step::Partition { a: 6, b: 7 },
            Step::Heal { a: 1, b: 1 },
            Step::Modify {
                node: 0,
                path: 7,
                content: 5,
            },
            Step::Create {
                node: 2,
                path: 7,
                content: 3,
            },
            Step::Modify {
                node: 2,
                path: 6,
                content: 3,
            },
            Step::Settle { secs: 12 },
            Step::Everywhere {
                path: 1,
                contents: vec![None, Some(1), Some(2), Some(2), Some(3), Some(1)],
            },
            Step::Chmod { node: 6, path: 8 },
            Step::Settle { secs: 2 },
            Step::Tier {
                a: 5,
                b: 6,
                tier: 2,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(2), Some(2), None, None, Some(5), Some(4)],
            },
            Step::Create {
                node: 4,
                path: 9,
                content: 1,
            },
            Step::Create {
                node: 5,
                path: 1,
                content: 2,
            },
            Step::Chmod { node: 7, path: 4 },
            Step::Offline { node: 3 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(1), Some(3)],
            },
            Step::Offline { node: 5 },
            Step::Touch { node: 0, path: 8 },
            Step::Create {
                node: 7,
                path: 10,
                content: 5,
            },
            Step::Heal { a: 6, b: 1 },
            Step::Delete { node: 4, path: 4 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(5), Some(1), Some(4), Some(2)],
            },
            Step::Heal { a: 1, b: 4 },
            Step::Create {
                node: 5,
                path: 11,
                content: 4,
            },
            Step::Chmod { node: 3, path: 0 },
            Step::Create {
                node: 7,
                path: 11,
                content: 5,
            },
            Step::MassModify {
                node: 2,
                fraction: 60,
                content: 1,
            },
            Step::Mkdir { node: 3, dir: 0 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 58,
            },
            Step::Settle { secs: 1 },
            Step::Delete { node: 4, path: 8 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(1), Some(1), None],
            },
            Step::Offline { node: 3 },
            Step::Heal { a: 0, b: 7 },
            Step::Settle { secs: 8 },
            Step::Modify {
                node: 2,
                path: 0,
                content: 1,
            },
            Step::Partition { a: 5, b: 1 },
            Step::Chmod { node: 7, path: 11 },
            Step::Create {
                node: 2,
                path: 8,
                content: 3,
            },
            Step::Create {
                node: 6,
                path: 9,
                content: 4,
            },
            Step::Chmod { node: 6, path: 1 },
            Step::Modify {
                node: 6,
                path: 3,
                content: 3,
            },
            Step::Rename {
                node: 2,
                from: 11,
                to: 0,
            },
            Step::Create {
                node: 0,
                path: 6,
                content: 4,
            },
            Step::Rmdir { node: 2, dir: 1 },
            Step::Partition { a: 7, b: 5 },
            Step::Offline { node: 0 },
            Step::Create {
                node: 7,
                path: 3,
                content: 5,
            },
            Step::MassModify {
                node: 0,
                fraction: 55,
                content: 1,
            },
        ],
    );
}

/// Same class as above.
#[test]
fn a_missed_chmod_is_found_after_a_long_run() {
    passes(
        76,
        &[
            Step::Settle { secs: 32 },
            Step::Rename {
                node: 5,
                from: 11,
                to: 9,
            },
            Step::Rename {
                node: 2,
                from: 9,
                to: 4,
            },
            Step::Tier {
                a: 0,
                b: 1,
                tier: 2,
            },
            Step::Create {
                node: 6,
                path: 4,
                content: 2,
            },
            Step::MassModify {
                node: 4,
                fraction: 94,
                content: 4,
            },
            Step::Delete { node: 7, path: 8 },
            Step::Offline { node: 0 },
            Step::Settle { secs: 33 },
            Step::Offline { node: 2 },
            Step::Create {
                node: 0,
                path: 7,
                content: 2,
            },
            Step::Symlink {
                node: 3,
                path: 5,
                target: 8,
            },
            Step::Touch { node: 7, path: 4 },
            Step::Chmod { node: 2, path: 11 },
            Step::Rename {
                node: 3,
                from: 1,
                to: 4,
            },
            Step::Mkdir { node: 6, dir: 2 },
            Step::Online { node: 4 },
            Step::Tier {
                a: 3,
                b: 0,
                tier: 1,
            },
            Step::Touch { node: 6, path: 1 },
            Step::MassModify {
                node: 5,
                fraction: 95,
                content: 3,
            },
            Step::Crash {
                node: 4,
                gap_secs: 94,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![None, Some(2), None, None, Some(2), Some(5), Some(3)],
            },
            Step::Heal { a: 1, b: 5 },
            Step::Create {
                node: 3,
                path: 5,
                content: 1,
            },
            Step::Create {
                node: 6,
                path: 11,
                content: 2,
            },
            Step::Modify {
                node: 5,
                path: 0,
                content: 2,
            },
            Step::Rename {
                node: 1,
                from: 0,
                to: 1,
            },
            Step::Touch { node: 3, path: 4 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 2,
            },
            Step::Create {
                node: 5,
                path: 5,
                content: 5,
            },
            Step::Modify {
                node: 2,
                path: 5,
                content: 5,
            },
            Step::Heal { a: 6, b: 1 },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Delete { node: 4, path: 1 },
            Step::Crash {
                node: 2,
                gap_secs: 21,
            },
            Step::Modify {
                node: 3,
                path: 7,
                content: 3,
            },
            Step::MassDelete {
                node: 5,
                fraction: 93,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(3), Some(3), Some(4), Some(4)],
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Offline { node: 1 },
            Step::Modify {
                node: 4,
                path: 3,
                content: 2,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 25,
            },
            Step::Modify {
                node: 3,
                path: 1,
                content: 2,
            },
            Step::Symlink {
                node: 1,
                path: 5,
                target: 8,
            },
            Step::Modify {
                node: 4,
                path: 8,
                content: 4,
            },
            Step::Online { node: 2 },
            Step::Create {
                node: 2,
                path: 11,
                content: 5,
            },
            Step::Partition { a: 4, b: 7 },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 55,
            },
            Step::Create {
                node: 2,
                path: 10,
                content: 2,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(4), None, Some(5), Some(3), Some(2)],
            },
            Step::Chmod { node: 1, path: 10 },
            Step::Offline { node: 5 },
            Step::Create {
                node: 1,
                path: 1,
                content: 4,
            },
            Step::Delete { node: 2, path: 2 },
            Step::Chmod { node: 2, path: 4 },
            Step::MassModify {
                node: 1,
                fraction: 93,
                content: 3,
            },
            Step::Delete { node: 5, path: 6 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(3), None, Some(4), Some(3), Some(3), Some(4)],
            },
            Step::Create {
                node: 7,
                path: 4,
                content: 4,
            },
            Step::Crash {
                node: 7,
                gap_secs: 91,
            },
            Step::Partition { a: 1, b: 1 },
            Step::Rename {
                node: 2,
                from: 6,
                to: 10,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Rules {
                    hold_count: 5,
                    hold_pct: 14,
                },
                delay_secs: 36,
            },
            Step::Modify {
                node: 1,
                path: 9,
                content: 5,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(4), None, Some(2), Some(1)],
            },
            Step::Chmod { node: 0, path: 7 },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(2), Some(1), Some(4), Some(5), None],
            },
        ],
    );
}

/// Three concurrent versions of one path merged pairwise in different
/// orders gave two nodes the same vector with different content (a file on
/// one, a symlink on the other): a symlink that replaced a file ranked
/// below the file it replaced, and one node met the newer increment alone.
/// The stamp is strictly increasing along every chain and is the first key
/// of the winner rule (§7.1, §7.6), so equal vectors mean equal content (I7).
#[test]
fn equal_vectors_mean_equal_content_when_a_symlink_replaces_a_file() {
    passes(
        2,
        &[
            Step::Delete { node: 4, path: 11 },
            Step::Tier {
                a: 4,
                b: 5,
                tier: 2,
            },
            Step::Create {
                node: 6,
                path: 11,
                content: 5,
            },
            Step::Create {
                node: 5,
                path: 5,
                content: 3,
            },
            Step::Heal { a: 4, b: 4 },
            Step::Partition { a: 0, b: 7 },
            Step::Touch { node: 5, path: 1 },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Delete { node: 4, path: 2 },
            Step::Partition { a: 5, b: 3 },
            Step::Modify {
                node: 3,
                path: 7,
                content: 5,
            },
            Step::Online { node: 4 },
            Step::Offline { node: 2 },
            Step::Create {
                node: 6,
                path: 5,
                content: 1,
            },
            Step::Symlink {
                node: 1,
                path: 11,
                target: 3,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 5,
            },
            Step::Modify {
                node: 4,
                path: 11,
                content: 1,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 38,
            },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Settle { secs: 11 },
            Step::Delete { node: 0, path: 4 },
            Step::Tier {
                a: 0,
                b: 5,
                tier: 1,
            },
            Step::Tier {
                a: 5,
                b: 7,
                tier: 1,
            },
            Step::Delete { node: 4, path: 1 },
            Step::Modify {
                node: 7,
                path: 2,
                content: 3,
            },
            Step::Rmdir { node: 7, dir: 2 },
            Step::Delete { node: 1, path: 11 },
            Step::Modify {
                node: 1,
                path: 0,
                content: 4,
            },
            Step::Modify {
                node: 0,
                path: 7,
                content: 4,
            },
            Step::Partition { a: 5, b: 6 },
            Step::Modify {
                node: 6,
                path: 0,
                content: 4,
            },
            Step::Rename {
                node: 5,
                from: 3,
                to: 4,
            },
            Step::Create {
                node: 6,
                path: 3,
                content: 3,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::Revert,
                delay_secs: 4,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 3,
            },
            Step::Settle { secs: 18 },
            Step::Tier {
                a: 4,
                b: 0,
                tier: 2,
            },
            Step::Delete { node: 7, path: 6 },
            Step::Create {
                node: 4,
                path: 10,
                content: 2,
            },
            Step::Settle { secs: 3 },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Delete { node: 7, path: 8 },
            Step::Partition { a: 6, b: 2 },
            Step::Modify {
                node: 0,
                path: 11,
                content: 2,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 19,
            },
            Step::Touch { node: 0, path: 5 },
            Step::Offline { node: 1 },
            Step::Mkdir { node: 3, dir: 0 },
            Step::Create {
                node: 4,
                path: 11,
                content: 5,
            },
            Step::Modify {
                node: 2,
                path: 5,
                content: 5,
            },
            Step::Rename {
                node: 4,
                from: 7,
                to: 5,
            },
            Step::Offline { node: 1 },
            Step::Delete { node: 1, path: 7 },
            Step::Settle { secs: 22 },
            Step::Mkdir { node: 3, dir: 0 },
            Step::Partition { a: 1, b: 1 },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 14,
            },
            Step::Create {
                node: 7,
                path: 8,
                content: 1,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 35,
                },
                delay_secs: 23,
            },
            Step::Mkdir { node: 7, dir: 0 },
            Step::Chmod { node: 1, path: 1 },
            Step::Chmod { node: 7, path: 11 },
            Step::Heal { a: 0, b: 3 },
            Step::Delete { node: 5, path: 7 },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 26,
            },
            Step::Delete { node: 2, path: 1 },
            Step::Modify {
                node: 6,
                path: 0,
                content: 1,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(1), Some(2), Some(3)],
            },
            Step::Modify {
                node: 4,
                path: 2,
                content: 2,
            },
            Step::Chmod { node: 2, path: 4 },
            Step::Offline { node: 0 },
            Step::Create {
                node: 4,
                path: 6,
                content: 3,
            },
            Step::MassModify {
                node: 0,
                fraction: 90,
                content: 3,
            },
            Step::Modify {
                node: 3,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 5,
                path: 6,
                content: 4,
            },
            Step::Heal { a: 6, b: 2 },
            Step::Symlink {
                node: 3,
                path: 9,
                target: 11,
            },
            Step::Create {
                node: 4,
                path: 0,
                content: 5,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(5), None, Some(3), None, None],
            },
            Step::MassModify {
                node: 1,
                fraction: 57,
                content: 2,
            },
            Step::Create {
                node: 0,
                path: 11,
                content: 4,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(2), Some(1), Some(1), Some(5)],
            },
            Step::Modify {
                node: 1,
                path: 6,
                content: 2,
            },
            Step::Heal { a: 2, b: 4 },
            Step::Everywhere {
                path: 4,
                contents: vec![None, Some(1), None],
            },
            Step::Partition { a: 4, b: 6 },
            Step::Settle { secs: 12 },
            Step::Partition { a: 6, b: 7 },
            Step::Partition { a: 1, b: 6 },
            Step::Heal { a: 2, b: 2 },
            Step::Online { node: 0 },
            Step::Offline { node: 7 },
            Step::Online { node: 7 },
            Step::Create {
                node: 6,
                path: 1,
                content: 5,
            },
            Step::Create {
                node: 1,
                path: 10,
                content: 1,
            },
            Step::Settle { secs: 39 },
            Step::Delete { node: 6, path: 3 },
            Step::Create {
                node: 5,
                path: 11,
                content: 4,
            },
            Step::Heal { a: 4, b: 2 },
            Step::Modify {
                node: 4,
                path: 8,
                content: 1,
            },
            Step::Touch { node: 0, path: 2 },
            Step::Modify {
                node: 7,
                path: 9,
                content: 3,
            },
            Step::Offline { node: 0 },
            Step::Create {
                node: 3,
                path: 6,
                content: 5,
            },
            Step::Create {
                node: 1,
                path: 7,
                content: 4,
            },
            Step::Partition { a: 1, b: 7 },
            Step::Create {
                node: 2,
                path: 7,
                content: 2,
            },
            Step::MassDelete {
                node: 5,
                fraction: 89,
            },
            Step::Settle { secs: 26 },
            Step::Delete { node: 3, path: 4 },
            Step::Touch { node: 1, path: 0 },
            Step::Delete { node: 3, path: 10 },
            Step::MassDelete {
                node: 7,
                fraction: 84,
            },
            Step::Online { node: 4 },
            Step::Partition { a: 3, b: 7 },
            Step::Partition { a: 6, b: 7 },
            Step::Create {
                node: 1,
                path: 9,
                content: 3,
            },
            Step::Mkdir { node: 3, dir: 0 },
            Step::Symlink {
                node: 0,
                path: 11,
                target: 4,
            },
            Step::Settle { secs: 24 },
            Step::Tier {
                a: 6,
                b: 0,
                tier: 0,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 39,
            },
            Step::Delete { node: 0, path: 2 },
            Step::Create {
                node: 0,
                path: 9,
                content: 5,
            },
            Step::MassModify {
                node: 6,
                fraction: 69,
                content: 2,
            },
            Step::Modify {
                node: 1,
                path: 7,
                content: 4,
            },
            Step::Crash {
                node: 7,
                gap_secs: 117,
            },
            Step::Heal { a: 3, b: 6 },
            Step::Create {
                node: 5,
                path: 2,
                content: 2,
            },
            Step::Create {
                node: 5,
                path: 9,
                content: 3,
            },
            Step::Offline { node: 4 },
            Step::Symlink {
                node: 4,
                path: 11,
                target: 3,
            },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Partition { a: 4, b: 6 },
        ],
    );
}

/// Same class: two tombstones merged on one node, a tombstone and a live
/// edit then the other tombstone on another; one vector, one deleted and one
/// live, each dropping the other as Equal forever.
#[test]
fn equal_vectors_mean_equal_content_when_deletes_race_an_edit() {
    passes(
        111,
        &[
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 19,
            },
            Step::Symlink {
                node: 2,
                path: 9,
                target: 7,
            },
            Step::Create {
                node: 2,
                path: 9,
                content: 4,
            },
            Step::MassModify {
                node: 5,
                fraction: 51,
                content: 5,
            },
            Step::Delete { node: 5, path: 9 },
            Step::MassDelete {
                node: 2,
                fraction: 66,
            },
            Step::Create {
                node: 3,
                path: 9,
                content: 4,
            },
            Step::Online { node: 5 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(3), Some(2), Some(2), Some(5)],
            },
            Step::Chmod { node: 4, path: 9 },
            Step::Modify {
                node: 4,
                path: 4,
                content: 1,
            },
            Step::Delete { node: 2, path: 0 },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(1), Some(3), Some(1), Some(1), Some(3), Some(1)],
            },
            Step::Modify {
                node: 5,
                path: 11,
                content: 5,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 12,
            },
            Step::Heal { a: 4, b: 5 },
            Step::Modify {
                node: 3,
                path: 7,
                content: 1,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(4), None, Some(4)],
            },
            Step::Create {
                node: 0,
                path: 8,
                content: 4,
            },
            Step::Heal { a: 6, b: 6 },
            Step::Heal { a: 0, b: 4 },
            Step::Crash {
                node: 4,
                gap_secs: 117,
            },
            Step::Crash {
                node: 2,
                gap_secs: 118,
            },
            Step::Create {
                node: 0,
                path: 0,
                content: 5,
            },
            Step::Crash {
                node: 5,
                gap_secs: 4,
            },
            Step::Tier {
                a: 7,
                b: 2,
                tier: 2,
            },
            Step::Modify {
                node: 7,
                path: 11,
                content: 1,
            },
            Step::Tier {
                a: 2,
                b: 5,
                tier: 0,
            },
            Step::Partition { a: 4, b: 2 },
            Step::Delete { node: 4, path: 9 },
            Step::Online { node: 0 },
            Step::Modify {
                node: 4,
                path: 11,
                content: 5,
            },
            Step::Tier {
                a: 0,
                b: 4,
                tier: 0,
            },
            Step::Partition { a: 3, b: 3 },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 12,
            },
            Step::Delete { node: 0, path: 10 },
            Step::Touch { node: 1, path: 3 },
            Step::Create {
                node: 6,
                path: 4,
                content: 2,
            },
            Step::Heal { a: 0, b: 1 },
            Step::Partition { a: 4, b: 1 },
            Step::Rename {
                node: 3,
                from: 6,
                to: 8,
            },
            Step::Modify {
                node: 3,
                path: 11,
                content: 4,
            },
            Step::Delete { node: 1, path: 8 },
            Step::Rename {
                node: 7,
                from: 8,
                to: 2,
            },
            Step::Create {
                node: 2,
                path: 0,
                content: 4,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 13,
                },
                delay_secs: 4,
            },
            Step::Create {
                node: 5,
                path: 7,
                content: 1,
            },
            Step::Modify {
                node: 4,
                path: 6,
                content: 2,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(3), Some(2), None, Some(1), Some(2), Some(4)],
            },
            Step::Touch { node: 7, path: 0 },
            Step::Settle { secs: 29 },
            Step::Create {
                node: 6,
                path: 9,
                content: 4,
            },
            Step::Create {
                node: 7,
                path: 6,
                content: 2,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 4,
            },
            Step::Settle { secs: 38 },
            Step::Partition { a: 4, b: 0 },
            Step::Online { node: 4 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 2,
            },
            Step::Online { node: 1 },
            Step::Settle { secs: 34 },
            Step::Symlink {
                node: 4,
                path: 0,
                target: 8,
            },
            Step::Tier {
                a: 6,
                b: 7,
                tier: 1,
            },
            Step::Create {
                node: 1,
                path: 6,
                content: 5,
            },
            Step::Settle { secs: 38 },
            Step::Delete { node: 3, path: 2 },
            Step::Rename {
                node: 6,
                from: 3,
                to: 4,
            },
            Step::Modify {
                node: 6,
                path: 6,
                content: 4,
            },
            Step::Delete { node: 1, path: 8 },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Tier {
                a: 6,
                b: 7,
                tier: 1,
            },
            Step::Create {
                node: 7,
                path: 0,
                content: 5,
            },
            Step::Touch { node: 3, path: 8 },
            Step::Create {
                node: 7,
                path: 10,
                content: 4,
            },
            Step::Create {
                node: 0,
                path: 10,
                content: 4,
            },
            Step::Settle { secs: 14 },
            Step::Modify {
                node: 0,
                path: 4,
                content: 2,
            },
            Step::Offline { node: 0 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(2), Some(5), None, None],
            },
            Step::Modify {
                node: 4,
                path: 4,
                content: 5,
            },
            Step::Partition { a: 4, b: 2 },
            Step::Create {
                node: 7,
                path: 6,
                content: 1,
            },
            Step::Online { node: 6 },
            Step::Heal { a: 6, b: 6 },
            Step::Delete { node: 6, path: 9 },
            Step::Modify {
                node: 7,
                path: 5,
                content: 3,
            },
            Step::Settle { secs: 12 },
            Step::Online { node: 4 },
            Step::Modify {
                node: 0,
                path: 3,
                content: 4,
            },
            Step::Crash {
                node: 4,
                gap_secs: 1,
            },
            Step::Tier {
                a: 4,
                b: 1,
                tier: 0,
            },
            Step::Symlink {
                node: 5,
                path: 9,
                target: 1,
            },
            Step::Partition { a: 3, b: 5 },
            Step::Delete { node: 3, path: 4 },
            Step::Settle { secs: 9 },
            Step::MassDelete {
                node: 2,
                fraction: 77,
            },
            Step::MassDelete {
                node: 3,
                fraction: 59,
            },
            Step::Modify {
                node: 3,
                path: 4,
                content: 2,
            },
        ],
    );
}

/// A `deny` with no local record made a tombstone stamped 1 that dominated
/// a version stamped by its mtime; a node that met the tombstone alone
/// ranked its own edit above it while a node holding the dominated version
/// ranked that above everything, and the same vector carried two contents
/// (I7). A deny's bump now stamps one past every version it dominates.
#[test]
fn a_deny_ranks_above_every_version_it_dominates() {
    passes(
        0,
        &[
            Step::Tier {
                a: 7,
                b: 7,
                tier: 0,
            },
            Step::Create {
                node: 7,
                path: 10,
                content: 5,
            },
            Step::Modify {
                node: 0,
                path: 7,
                content: 4,
            },
            Step::Modify {
                node: 1,
                path: 8,
                content: 1,
            },
            Step::Create {
                node: 1,
                path: 4,
                content: 3,
            },
            Step::Heal { a: 4, b: 0 },
            Step::Create {
                node: 6,
                path: 5,
                content: 2,
            },
            Step::Offline { node: 4 },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Create {
                node: 7,
                path: 6,
                content: 2,
            },
            Step::Create {
                node: 6,
                path: 7,
                content: 4,
            },
            Step::Create {
                node: 3,
                path: 8,
                content: 3,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 8,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(3), Some(5), Some(1)],
            },
            Step::Modify {
                node: 0,
                path: 1,
                content: 4,
            },
            Step::Partition { a: 3, b: 3 },
            Step::Settle { secs: 30 },
            Step::Create {
                node: 6,
                path: 8,
                content: 5,
            },
            Step::Crash {
                node: 1,
                gap_secs: 16,
            },
            Step::MassModify {
                node: 7,
                fraction: 98,
                content: 1,
            },
            Step::Partition { a: 7, b: 4 },
            Step::Heal { a: 3, b: 5 },
            Step::Delete { node: 0, path: 11 },
            Step::Online { node: 0 },
            Step::Create {
                node: 7,
                path: 6,
                content: 1,
            },
            Step::Delete { node: 2, path: 6 },
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
            Step::Offline { node: 2 },
            Step::Settle { secs: 20 },
            Step::Mkdir { node: 2, dir: 2 },
            Step::Create {
                node: 6,
                path: 10,
                content: 3,
            },
            Step::Create {
                node: 1,
                path: 7,
                content: 3,
            },
            Step::Settle { secs: 22 },
            Step::Online { node: 6 },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(5), Some(4), Some(3)],
            },
            Step::Crash {
                node: 7,
                gap_secs: 19,
            },
            Step::Create {
                node: 1,
                path: 9,
                content: 3,
            },
            Step::Tier {
                a: 6,
                b: 7,
                tier: 1,
            },
            Step::Modify {
                node: 3,
                path: 9,
                content: 1,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(2), Some(5)],
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(4), None, Some(4)],
            },
            Step::Delete { node: 7, path: 1 },
            Step::Modify {
                node: 3,
                path: 9,
                content: 3,
            },
            Step::Modify {
                node: 1,
                path: 3,
                content: 5,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::DenyAll,
                delay_secs: 43,
            },
            Step::Chmod { node: 1, path: 11 },
            Step::Online { node: 4 },
            Step::Rmdir { node: 0, dir: 0 },
            Step::Modify {
                node: 5,
                path: 2,
                content: 1,
            },
            Step::Rename {
                node: 1,
                from: 7,
                to: 7,
            },
            Step::Settle { secs: 15 },
            Step::User {
                node: 7,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 29,
                },
                delay_secs: 9,
            },
        ],
    );
}

/// Two holders of the same losing version each displaced it to the same
/// conflict-copy path. One wanted the other's copy and was fetching it when
/// its own commit wrote its copy record at that path; the fetch then landed
/// over that record with a version that did not dominate it (I8). Every
/// index write now re-classifies the wants at its path, so the want meets
/// the node's own copy as an identical-content merge.
#[test]
fn a_want_for_a_peers_conflict_copy_meets_our_own_copy_as_a_merge() {
    passes(
        17,
        &[
            Step::Rename {
                node: 5,
                from: 3,
                to: 6,
            },
            Step::Settle { secs: 13 },
            Step::Create {
                node: 4,
                path: 0,
                content: 5,
            },
            Step::MassDelete {
                node: 1,
                fraction: 52,
            },
            Step::Settle { secs: 18 },
            Step::Touch { node: 7, path: 1 },
            Step::Crash {
                node: 3,
                gap_secs: 13,
            },
            Step::Touch { node: 1, path: 9 },
            Step::Heal { a: 4, b: 1 },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(5), Some(4), Some(2), Some(2), Some(2)],
            },
        ],
    );
}

/// A node denied a batch, then reverted: its deny bump was discarded before
/// anyone saw it. Another node later reached the same vector by a merge
/// with the node's re-issued counter, and I7 compared it with the discarded
/// record. Records a revert discarded are no evidence of anything.
#[test]
fn a_vector_a_revert_discarded_may_be_reached_again_by_a_merge() {
    passes(
        164,
        &[
            Step::MassDelete {
                node: 3,
                fraction: 68,
            },
            Step::Create {
                node: 7,
                path: 1,
                content: 4,
            },
            Step::Crash {
                node: 6,
                gap_secs: 26,
            },
            Step::Modify {
                node: 4,
                path: 11,
                content: 1,
            },
            Step::Touch { node: 4, path: 10 },
            Step::Settle { secs: 33 },
            Step::Settle { secs: 12 },
            Step::Offline { node: 4 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 40,
            },
            Step::Online { node: 5 },
            Step::Rmdir { node: 5, dir: 0 },
            Step::Delete { node: 2, path: 4 },
            Step::Heal { a: 0, b: 6 },
            Step::Rename {
                node: 0,
                from: 6,
                to: 5,
            },
            Step::Partition { a: 5, b: 0 },
            Step::Modify {
                node: 1,
                path: 4,
                content: 3,
            },
            Step::Online { node: 4 },
            Step::Chmod { node: 6, path: 5 },
            Step::Online { node: 2 },
            Step::Delete { node: 1, path: 11 },
            Step::Touch { node: 3, path: 9 },
            Step::Tier {
                a: 7,
                b: 0,
                tier: 2,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![
                    Some(3),
                    Some(3),
                    Some(1),
                    Some(5),
                    Some(2),
                    Some(4),
                    Some(1),
                ],
            },
            Step::Create {
                node: 0,
                path: 7,
                content: 5,
            },
            Step::Chmod { node: 7, path: 2 },
            Step::Modify {
                node: 0,
                path: 2,
                content: 2,
            },
            Step::Modify {
                node: 1,
                path: 9,
                content: 1,
            },
            Step::Chmod { node: 2, path: 6 },
            Step::MassDelete {
                node: 2,
                fraction: 54,
            },
            Step::Modify {
                node: 2,
                path: 1,
                content: 2,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 14,
            },
            Step::Online { node: 4 },
            Step::Offline { node: 3 },
            Step::Modify {
                node: 3,
                path: 3,
                content: 2,
            },
            Step::MassDelete {
                node: 7,
                fraction: 59,
            },
            Step::Partition { a: 5, b: 3 },
            Step::Online { node: 2 },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 29,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 34,
            },
            Step::Crash {
                node: 1,
                gap_secs: 78,
            },
            Step::Rename {
                node: 3,
                from: 9,
                to: 7,
            },
        ],
    );
}

/// A node removed a directory, paused on the deletes, and reverted: the
/// tombstones were discarded unannounced, and I3 took one for a deletion the
/// mesh had agreed on.
#[test]
fn a_tombstone_a_revert_discarded_is_not_a_deletion() {
    passes(
        176,
        &[
            Step::Modify {
                node: 1,
                path: 11,
                content: 3,
            },
            Step::Rename {
                node: 1,
                from: 10,
                to: 9,
            },
            Step::Chmod { node: 4, path: 3 },
            Step::Tier {
                a: 7,
                b: 5,
                tier: 0,
            },
            Step::Modify {
                node: 6,
                path: 4,
                content: 3,
            },
            Step::Delete { node: 3, path: 9 },
            Step::Heal { a: 2, b: 2 },
            Step::Modify {
                node: 5,
                path: 0,
                content: 3,
            },
            Step::MassDelete {
                node: 4,
                fraction: 75,
            },
            Step::Modify {
                node: 3,
                path: 3,
                content: 4,
            },
            Step::Chmod { node: 7, path: 6 },
            Step::Modify {
                node: 6,
                path: 9,
                content: 5,
            },
            Step::Tier {
                a: 5,
                b: 0,
                tier: 0,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(3), None, Some(5), Some(3), Some(3), None, None],
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 50,
            },
            Step::Create {
                node: 1,
                path: 6,
                content: 2,
            },
            Step::Create {
                node: 4,
                path: 5,
                content: 1,
            },
            Step::Heal { a: 2, b: 1 },
            Step::Heal { a: 0, b: 6 },
            Step::Settle { secs: 17 },
            Step::User {
                node: 3,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 30,
                },
                delay_secs: 49,
            },
            Step::Chmod { node: 2, path: 5 },
            Step::Settle { secs: 31 },
            Step::Offline { node: 4 },
            Step::Crash {
                node: 3,
                gap_secs: 93,
            },
            Step::Settle { secs: 29 },
            Step::Create {
                node: 7,
                path: 8,
                content: 1,
            },
            Step::Crash {
                node: 1,
                gap_secs: 97,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::DenyAll,
                delay_secs: 50,
            },
            Step::Create {
                node: 2,
                path: 11,
                content: 2,
            },
            Step::Create {
                node: 4,
                path: 4,
                content: 5,
            },
            Step::Rename {
                node: 1,
                from: 11,
                to: 10,
            },
            Step::Delete { node: 1, path: 1 },
            Step::Rmdir { node: 5, dir: 2 },
            Step::Mkdir { node: 4, dir: 1 },
            Step::Modify {
                node: 4,
                path: 1,
                content: 2,
            },
            Step::Settle { secs: 5 },
            Step::Touch { node: 3, path: 5 },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 4,
            },
            Step::Create {
                node: 3,
                path: 11,
                content: 4,
            },
        ],
    );
}

/// A node's first version of a path was discarded by its revert; its next
/// change re-issued the same vector with the same content but a later
/// mtime. The version table kept the discarded record (same content, no
/// I7 clash), so the conflict copy the node later made of the newer record
/// was named after an mtime the table did not know.
#[test]
fn a_reissued_vector_replaces_the_discarded_record_whatever_its_content() {
    passes(
        26,
        &[
            Step::Touch { node: 6, path: 3 },
            Step::Tier {
                a: 7,
                b: 6,
                tier: 0,
            },
            Step::Create {
                node: 5,
                path: 6,
                content: 5,
            },
            Step::Online { node: 3 },
            Step::Chmod { node: 3, path: 10 },
            Step::Create {
                node: 6,
                path: 5,
                content: 2,
            },
            Step::Create {
                node: 1,
                path: 6,
                content: 1,
            },
            Step::Tier {
                a: 6,
                b: 5,
                tier: 0,
            },
            Step::Online { node: 1 },
            Step::Create {
                node: 6,
                path: 1,
                content: 4,
            },
            Step::MassModify {
                node: 3,
                fraction: 88,
                content: 3,
            },
            Step::Delete { node: 0, path: 10 },
            Step::Chmod { node: 1, path: 11 },
            Step::Create {
                node: 6,
                path: 1,
                content: 3,
            },
            Step::Online { node: 5 },
            Step::Rename {
                node: 0,
                from: 0,
                to: 8,
            },
            Step::Tier {
                a: 5,
                b: 4,
                tier: 2,
            },
            Step::Touch { node: 4, path: 11 },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 26,
            },
            Step::Create {
                node: 5,
                path: 4,
                content: 2,
            },
            Step::Delete { node: 1, path: 3 },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(4), None, Some(5), None],
            },
            Step::Create {
                node: 4,
                path: 11,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 1,
                content: 1,
            },
            Step::Crash {
                node: 3,
                gap_secs: 41,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(4), Some(1), Some(3), None, Some(3)],
            },
            Step::Create {
                node: 5,
                path: 5,
                content: 5,
            },
            Step::MassDelete {
                node: 7,
                fraction: 83,
            },
            Step::Modify {
                node: 7,
                path: 2,
                content: 1,
            },
            Step::MassModify {
                node: 4,
                fraction: 80,
                content: 3,
            },
            Step::Create {
                node: 4,
                path: 7,
                content: 5,
            },
            Step::Rename {
                node: 1,
                from: 4,
                to: 3,
            },
            Step::Modify {
                node: 2,
                path: 3,
                content: 2,
            },
            Step::Touch { node: 3, path: 9 },
            Step::Rmdir { node: 6, dir: 1 },
            Step::Touch { node: 7, path: 9 },
            Step::User {
                node: 0,
                action: crate::UserAction::Revert,
                delay_secs: 38,
            },
            Step::Delete { node: 6, path: 11 },
            Step::Partition { a: 4, b: 3 },
            Step::Tier {
                a: 1,
                b: 4,
                tier: 1,
            },
            Step::Offline { node: 2 },
            Step::Create {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Settle { secs: 21 },
            Step::Create {
                node: 6,
                path: 11,
                content: 4,
            },
            Step::Modify {
                node: 7,
                path: 3,
                content: 1,
            },
            Step::Offline { node: 3 },
            Step::MassModify {
                node: 1,
                fraction: 87,
                content: 4,
            },
            Step::Modify {
                node: 5,
                path: 3,
                content: 2,
            },
            Step::Modify {
                node: 2,
                path: 5,
                content: 1,
            },
            Step::Create {
                node: 2,
                path: 0,
                content: 1,
            },
            Step::Partition { a: 5, b: 7 },
            Step::Modify {
                node: 7,
                path: 0,
                content: 3,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 14,
            },
            Step::Modify {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Online { node: 2 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(5), Some(1)],
            },
        ],
    );
}

/// A symlink retargeted under a dropped watcher event was invisible to every
/// later scan: the fast path matched symlinks by kind alone. The source kept
/// announcing the old target's hash and served the new target's bytes, so
/// receivers mismatched twice and gave up. The fast path compares the
/// target now (§7.3).
#[test]
fn a_retargeted_symlink_the_watcher_missed_is_found_by_the_next_scan() {
    passes(
        90,
        &[
            Step::Create {
                node: 3,
                path: 9,
                content: 2,
            },
            Step::MassModify {
                node: 5,
                fraction: 73,
                content: 5,
            },
            Step::Delete { node: 3, path: 9 },
            Step::Partition { a: 1, b: 6 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(4), Some(5), None],
            },
            Step::Crash {
                node: 3,
                gap_secs: 46,
            },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Modify {
                node: 2,
                path: 7,
                content: 3,
            },
            Step::Crash {
                node: 1,
                gap_secs: 81,
            },
            Step::Crash {
                node: 6,
                gap_secs: 82,
            },
            Step::Offline { node: 2 },
            Step::Crash {
                node: 1,
                gap_secs: 29,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 4,
            },
            Step::Delete { node: 1, path: 5 },
            Step::Delete { node: 0, path: 6 },
            Step::Delete { node: 1, path: 7 },
            Step::Settle { secs: 17 },
            Step::Create {
                node: 7,
                path: 4,
                content: 2,
            },
            Step::Touch { node: 5, path: 3 },
            Step::Settle { secs: 14 },
            Step::Tier {
                a: 7,
                b: 1,
                tier: 2,
            },
            Step::Mkdir { node: 6, dir: 2 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 10,
            },
            Step::Symlink {
                node: 2,
                path: 1,
                target: 2,
            },
            Step::Create {
                node: 4,
                path: 2,
                content: 3,
            },
            Step::Create {
                node: 1,
                path: 6,
                content: 3,
            },
            Step::Symlink {
                node: 2,
                path: 1,
                target: 11,
            },
        ],
    );
}

/// A file announced, fetched by nobody, deleted by its user and then
/// reverted: the restored record described content that existed nowhere,
/// the restoring want never found a source, and the index said live while
/// the disk said absent forever. Once every member has answered
/// NotAvailable the deletion stands (§8.3 step 4).
#[test]
fn a_reverted_deletion_nobody_can_serve_stands() {
    passes(
        106,
        &[
            Step::Delete { node: 1, path: 5 },
            Step::Modify {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Mkdir { node: 5, dir: 1 },
            Step::Crash {
                node: 2,
                gap_secs: 61,
            },
            Step::Delete { node: 0, path: 8 },
            Step::Partition { a: 0, b: 1 },
            Step::Modify {
                node: 5,
                path: 10,
                content: 3,
            },
            Step::MassDelete {
                node: 2,
                fraction: 57,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Delete { node: 0, path: 8 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(5), Some(1), Some(2), Some(5), Some(5), None, Some(5)],
            },
            Step::Create {
                node: 0,
                path: 8,
                content: 3,
            },
            Step::Modify {
                node: 0,
                path: 5,
                content: 4,
            },
            Step::Partition { a: 4, b: 0 },
            Step::Symlink {
                node: 3,
                path: 3,
                target: 5,
            },
            Step::Offline { node: 5 },
            Step::Modify {
                node: 4,
                path: 4,
                content: 1,
            },
            Step::Modify {
                node: 4,
                path: 1,
                content: 3,
            },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Heal { a: 5, b: 0 },
            Step::Settle { secs: 30 },
            Step::Partition { a: 0, b: 0 },
            Step::Delete { node: 7, path: 11 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 40,
            },
            Step::Crash {
                node: 4,
                gap_secs: 99,
            },
            Step::Modify {
                node: 1,
                path: 1,
                content: 3,
            },
            Step::Create {
                node: 2,
                path: 2,
                content: 3,
            },
            Step::Chmod { node: 0, path: 6 },
            Step::Modify {
                node: 5,
                path: 4,
                content: 3,
            },
            Step::MassDelete {
                node: 4,
                fraction: 85,
            },
            Step::Delete { node: 5, path: 2 },
            Step::Delete { node: 6, path: 3 },
            Step::Create {
                node: 1,
                path: 7,
                content: 1,
            },
            Step::Offline { node: 0 },
            Step::Offline { node: 2 },
            Step::Partition { a: 5, b: 2 },
            Step::Heal { a: 2, b: 0 },
            Step::Everywhere {
                path: 6,
                contents: vec![None, Some(1), None],
            },
            Step::Online { node: 7 },
            Step::Touch { node: 6, path: 8 },
            Step::Partition { a: 1, b: 5 },
            Step::Everywhere {
                path: 3,
                contents: vec![None, None, Some(5)],
            },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(2), None, Some(3), Some(5), Some(2), Some(2)],
            },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Settle { secs: 28 },
            Step::Rename {
                node: 0,
                from: 0,
                to: 11,
            },
            Step::Delete { node: 0, path: 4 },
            Step::Delete { node: 0, path: 1 },
            Step::Modify {
                node: 5,
                path: 2,
                content: 2,
            },
            Step::Rename {
                node: 7,
                from: 1,
                to: 4,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 15,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 42,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![None, None, Some(1), None, Some(4), Some(2), None],
            },
        ],
    );
}

/// A paused folder answered a peer's catch-up with a record it had adopted
/// but never announced (the pending set kept it as what a later local
/// change replaced). The batch's seq_high moved the peer's watermark past
/// the withheld pending records, and when the pause was approved and they
/// went out, that peer never asked for them: a conflict copy live on one
/// node and absent on another.
#[test]
fn catch_up_never_sends_what_this_machine_has_not_announced() {
    passes(
        33,
        &[
            Step::Delete { node: 7, path: 10 },
            Step::Settle { secs: 2 },
            Step::Delete { node: 5, path: 11 },
            Step::Delete { node: 7, path: 3 },
            Step::Create {
                node: 5,
                path: 11,
                content: 4,
            },
            Step::Heal { a: 1, b: 1 },
            Step::Settle { secs: 12 },
            Step::Touch { node: 6, path: 4 },
            Step::Settle { secs: 16 },
            Step::Everywhere {
                path: 3,
                contents: vec![None, None, Some(4), Some(2), Some(1), Some(5), Some(3)],
            },
            Step::Heal { a: 1, b: 0 },
            Step::Heal { a: 7, b: 4 },
            Step::Touch { node: 4, path: 11 },
            Step::Heal { a: 4, b: 7 },
            Step::Modify {
                node: 1,
                path: 5,
                content: 3,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::DenyAll,
                delay_secs: 39,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 1,
            },
            Step::Online { node: 5 },
            Step::MassModify {
                node: 3,
                fraction: 97,
                content: 5,
            },
            Step::Create {
                node: 2,
                path: 11,
                content: 1,
            },
            Step::Chmod { node: 4, path: 2 },
            Step::Partition { a: 4, b: 1 },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 54,
            },
            Step::Delete { node: 7, path: 6 },
            Step::Create {
                node: 4,
                path: 5,
                content: 4,
            },
            Step::Tier {
                a: 4,
                b: 3,
                tier: 1,
            },
            Step::Chmod { node: 4, path: 7 },
            Step::Modify {
                node: 7,
                path: 10,
                content: 5,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Modify {
                node: 0,
                path: 4,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 1,
            },
            Step::Partition { a: 1, b: 6 },
            Step::Symlink {
                node: 3,
                path: 10,
                target: 4,
            },
            Step::Delete { node: 6, path: 9 },
            Step::Settle { secs: 6 },
            Step::Online { node: 5 },
            Step::Rmdir { node: 3, dir: 1 },
            Step::Create {
                node: 0,
                path: 7,
                content: 2,
            },
            Step::Settle { secs: 31 },
            Step::Partition { a: 7, b: 3 },
            Step::Delete { node: 1, path: 2 },
            Step::Modify {
                node: 5,
                path: 2,
                content: 4,
            },
            Step::Online { node: 0 },
            Step::MassDelete {
                node: 7,
                fraction: 85,
            },
            Step::Rename {
                node: 3,
                from: 0,
                to: 2,
            },
            Step::Offline { node: 3 },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 10,
            },
        ],
    );
}

/// A revert restored a node's announced record and the checker took that
/// record, too, for one the revert had discarded, so a deletion it
/// dominated looked uncontradicted. Only the node's versions above the
/// restored one were pending and discarded.
#[test]
fn a_revert_discards_only_the_versions_above_the_restored_record() {
    passes(
        203,
        &[
            Step::Touch { node: 4, path: 4 },
            Step::Partition { a: 1, b: 5 },
            Step::Delete { node: 4, path: 6 },
            Step::Settle { secs: 29 },
            Step::Mkdir { node: 4, dir: 2 },
            Step::Rmdir { node: 3, dir: 0 },
            Step::Create {
                node: 7,
                path: 8,
                content: 1,
            },
            Step::Modify {
                node: 4,
                path: 3,
                content: 1,
            },
            Step::Touch { node: 4, path: 10 },
            Step::Create {
                node: 6,
                path: 1,
                content: 4,
            },
            Step::Delete { node: 0, path: 6 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(1), Some(2), None, Some(1), Some(4)],
            },
            Step::Chmod { node: 6, path: 6 },
            Step::Delete { node: 4, path: 8 },
            Step::Settle { secs: 23 },
            Step::Chmod { node: 4, path: 8 },
            Step::Settle { secs: 14 },
            Step::Tier {
                a: 2,
                b: 6,
                tier: 0,
            },
            Step::Modify {
                node: 6,
                path: 9,
                content: 3,
            },
            Step::Settle { secs: 9 },
            Step::Offline { node: 6 },
            Step::MassDelete {
                node: 1,
                fraction: 93,
            },
            Step::Delete { node: 0, path: 5 },
            Step::Heal { a: 7, b: 0 },
            Step::Crash {
                node: 0,
                gap_secs: 35,
            },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Offline { node: 4 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(4), Some(2), Some(5), Some(2)],
            },
            Step::Symlink {
                node: 5,
                path: 8,
                target: 0,
            },
            Step::Create {
                node: 6,
                path: 5,
                content: 1,
            },
            Step::Heal { a: 5, b: 6 },
            Step::Create {
                node: 0,
                path: 6,
                content: 3,
            },
            Step::Create {
                node: 3,
                path: 1,
                content: 3,
            },
            Step::Online { node: 2 },
            Step::Delete { node: 5, path: 3 },
            Step::Delete { node: 3, path: 8 },
            Step::Symlink {
                node: 1,
                path: 0,
                target: 11,
            },
            Step::Offline { node: 5 },
            Step::Online { node: 6 },
            Step::Online { node: 5 },
            Step::Heal { a: 6, b: 4 },
            Step::Delete { node: 2, path: 3 },
            Step::Modify {
                node: 6,
                path: 0,
                content: 3,
            },
            Step::Delete { node: 2, path: 6 },
            Step::Rename {
                node: 3,
                from: 0,
                to: 3,
            },
            Step::Partition { a: 2, b: 6 },
            Step::Tier {
                a: 7,
                b: 2,
                tier: 1,
            },
            Step::Mkdir { node: 5, dir: 1 },
            Step::Heal { a: 6, b: 2 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 18,
            },
            Step::Touch { node: 1, path: 6 },
            Step::Partition { a: 3, b: 5 },
            Step::Modify {
                node: 3,
                path: 7,
                content: 4,
            },
            Step::Crash {
                node: 7,
                gap_secs: 105,
            },
            Step::Heal { a: 4, b: 1 },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 36,
            },
            Step::Touch { node: 6, path: 8 },
            Step::Everywhere {
                path: 1,
                contents: vec![None, Some(5), Some(2)],
            },
            Step::Symlink {
                node: 0,
                path: 8,
                target: 0,
            },
            Step::Create {
                node: 6,
                path: 5,
                content: 4,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 4,
            },
            Step::Touch { node: 3, path: 4 },
            Step::Settle { secs: 7 },
            Step::Delete { node: 0, path: 4 },
            Step::Partition { a: 2, b: 7 },
            Step::Create {
                node: 4,
                path: 2,
                content: 4,
            },
            Step::Modify {
                node: 5,
                path: 10,
                content: 2,
            },
            Step::Heal { a: 1, b: 5 },
            Step::Settle { secs: 14 },
            Step::Delete { node: 3, path: 9 },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(4), Some(1)],
            },
            Step::Online { node: 1 },
            Step::Create {
                node: 2,
                path: 8,
                content: 3,
            },
            Step::Delete { node: 3, path: 1 },
            Step::Offline { node: 5 },
            Step::MassDelete {
                node: 1,
                fraction: 95,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 45,
                },
                delay_secs: 3,
            },
            Step::Offline { node: 5 },
            Step::Online { node: 5 },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 2,
            },
            Step::Crash {
                node: 3,
                gap_secs: 44,
            },
            Step::Tier {
                a: 5,
                b: 4,
                tier: 0,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::DenyAll,
                delay_secs: 57,
            },
            Step::Create {
                node: 7,
                path: 11,
                content: 2,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 59,
            },
            Step::Create {
                node: 0,
                path: 0,
                content: 3,
            },
            Step::Settle { secs: 3 },
            Step::Modify {
                node: 0,
                path: 5,
                content: 5,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![None, Some(5), Some(4), None, Some(4), Some(3), Some(4)],
            },
        ],
    );
}

/// The commit guard compared size and mtime for files and kind alone for
/// symlinks, so a chmod or a retarget between a losing version's record
/// and the commit that displaced its file went through, and the conflict
/// copy carried the user's later state under the loser's name (I4). The
/// guard is the scan fast path's predicate now (§7.5 step 6).
#[test]
fn a_chmod_under_a_pending_commit_is_changed_underneath() {
    passes(
        83,
        &[
            Step::Touch { node: 0, path: 2 },
            Step::Modify {
                node: 6,
                path: 1,
                content: 1,
            },
            Step::Modify {
                node: 4,
                path: 7,
                content: 1,
            },
            Step::Heal { a: 3, b: 1 },
            Step::Offline { node: 1 },
            Step::Create {
                node: 1,
                path: 1,
                content: 2,
            },
            Step::Heal { a: 2, b: 7 },
            Step::MassModify {
                node: 6,
                fraction: 72,
                content: 2,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(5), Some(1)],
            },
            Step::Chmod { node: 4, path: 4 },
            Step::Mkdir { node: 5, dir: 1 },
            Step::Settle { secs: 24 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(3), Some(5), None, Some(5), Some(1), Some(5)],
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(5), None, None, Some(3), Some(1), Some(4), Some(3)],
            },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(2), None, Some(5), Some(1)],
            },
            Step::Mkdir { node: 4, dir: 2 },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Chmod { node: 4, path: 3 },
            Step::Create {
                node: 2,
                path: 2,
                content: 5,
            },
            Step::MassModify {
                node: 2,
                fraction: 52,
                content: 2,
            },
            Step::Online { node: 6 },
            Step::Create {
                node: 7,
                path: 1,
                content: 4,
            },
            Step::Create {
                node: 2,
                path: 11,
                content: 2,
            },
            Step::Create {
                node: 0,
                path: 3,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 11,
                content: 5,
            },
            Step::Chmod { node: 5, path: 5 },
            Step::Partition { a: 0, b: 6 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(5), Some(1), None, Some(4), Some(3), Some(1), Some(2)],
            },
            Step::Heal { a: 6, b: 5 },
            Step::Modify {
                node: 2,
                path: 10,
                content: 3,
            },
            Step::Modify {
                node: 3,
                path: 4,
                content: 4,
            },
            Step::Partition { a: 1, b: 4 },
            Step::Mkdir { node: 4, dir: 2 },
            Step::Symlink {
                node: 4,
                path: 7,
                target: 0,
            },
            Step::Delete { node: 1, path: 5 },
            Step::Touch { node: 5, path: 6 },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(1), Some(2), Some(2), Some(2), Some(5), Some(5)],
            },
            Step::Partition { a: 2, b: 2 },
            Step::Chmod { node: 2, path: 1 },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(4), Some(3), Some(5), None],
            },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 32,
            },
            Step::Partition { a: 2, b: 5 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 59,
            },
            Step::Settle { secs: 34 },
            Step::Crash {
                node: 5,
                gap_secs: 28,
            },
            Step::Crash {
                node: 6,
                gap_secs: 75,
            },
            Step::Tier {
                a: 6,
                b: 1,
                tier: 0,
            },
            Step::Modify {
                node: 6,
                path: 0,
                content: 2,
            },
            Step::Heal { a: 0, b: 7 },
            Step::MassModify {
                node: 2,
                fraction: 99,
                content: 1,
            },
            Step::Rmdir { node: 6, dir: 2 },
            Step::Modify {
                node: 1,
                path: 3,
                content: 4,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 3,
            },
            Step::Partition { a: 0, b: 6 },
            Step::Tier {
                a: 5,
                b: 6,
                tier: 1,
            },
            Step::Create {
                node: 7,
                path: 10,
                content: 2,
            },
            Step::Modify {
                node: 1,
                path: 6,
                content: 1,
            },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(3), Some(5), None, Some(5), Some(4)],
            },
            Step::Tier {
                a: 4,
                b: 1,
                tier: 0,
            },
            Step::Modify {
                node: 0,
                path: 7,
                content: 3,
            },
            Step::Rmdir { node: 3, dir: 0 },
            Step::Online { node: 5 },
            Step::Rename {
                node: 3,
                from: 7,
                to: 0,
            },
            Step::Delete { node: 7, path: 7 },
            Step::Create {
                node: 6,
                path: 2,
                content: 1,
            },
            Step::Settle { secs: 8 },
            Step::Crash {
                node: 0,
                gap_secs: 7,
            },
            Step::Create {
                node: 0,
                path: 0,
                content: 4,
            },
            Step::Heal { a: 2, b: 7 },
            Step::Create {
                node: 6,
                path: 10,
                content: 2,
            },
            Step::Heal { a: 1, b: 5 },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 6,
            },
            Step::Settle { secs: 30 },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Crash {
                node: 1,
                gap_secs: 88,
            },
            Step::Crash {
                node: 0,
                gap_secs: 111,
            },
            Step::Touch { node: 0, path: 0 },
            Step::Delete { node: 5, path: 6 },
            Step::Partition { a: 7, b: 3 },
            Step::Modify {
                node: 2,
                path: 2,
                content: 1,
            },
            Step::Rename {
                node: 1,
                from: 8,
                to: 11,
            },
            Step::Crash {
                node: 2,
                gap_secs: 50,
            },
            Step::Create {
                node: 4,
                path: 8,
                content: 3,
            },
            Step::Chmod { node: 2, path: 3 },
            Step::Modify {
                node: 3,
                path: 0,
                content: 5,
            },
            Step::Delete { node: 4, path: 4 },
            Step::Rmdir { node: 0, dir: 1 },
            Step::Offline { node: 4 },
            Step::Online { node: 4 },
            Step::Modify {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![None, Some(5), Some(5), Some(4)],
            },
            Step::Touch { node: 1, path: 3 },
            Step::Touch { node: 0, path: 10 },
            Step::Create {
                node: 6,
                path: 2,
                content: 1,
            },
            Step::Partition { a: 6, b: 6 },
            Step::Create {
                node: 1,
                path: 6,
                content: 4,
            },
            Step::Offline { node: 5 },
            Step::Modify {
                node: 6,
                path: 10,
                content: 2,
            },
            Step::Tier {
                a: 2,
                b: 5,
                tier: 1,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(2), Some(4), Some(4), None, Some(2), None],
            },
            Step::Chmod { node: 6, path: 2 },
            Step::Partition { a: 3, b: 4 },
            Step::Rename {
                node: 3,
                from: 4,
                to: 10,
            },
            Step::Partition { a: 3, b: 2 },
        ],
    );
}

/// Same class.
#[test]
fn a_chmod_under_a_pending_commit_is_changed_underneath_in_a_shorter_run() {
    passes(
        126,
        &[
            Step::Delete { node: 1, path: 5 },
            Step::Create {
                node: 0,
                path: 6,
                content: 1,
            },
            Step::Online { node: 4 },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(1), Some(1)],
            },
            Step::Crash {
                node: 0,
                gap_secs: 72,
            },
            Step::Modify {
                node: 0,
                path: 10,
                content: 3,
            },
            Step::Heal { a: 0, b: 7 },
            Step::Create {
                node: 6,
                path: 1,
                content: 4,
            },
            Step::Rename {
                node: 5,
                from: 4,
                to: 1,
            },
            Step::Rmdir { node: 2, dir: 2 },
            Step::Heal { a: 1, b: 4 },
            Step::Create {
                node: 0,
                path: 4,
                content: 1,
            },
            Step::Chmod { node: 4, path: 2 },
            Step::Create {
                node: 5,
                path: 6,
                content: 4,
            },
            Step::Modify {
                node: 4,
                path: 2,
                content: 3,
            },
            Step::Settle { secs: 13 },
            Step::Modify {
                node: 4,
                path: 6,
                content: 1,
            },
            Step::Rename {
                node: 1,
                from: 11,
                to: 11,
            },
            Step::Rename {
                node: 1,
                from: 6,
                to: 0,
            },
            Step::Offline { node: 0 },
            Step::Modify {
                node: 2,
                path: 4,
                content: 5,
            },
            Step::Modify {
                node: 5,
                path: 8,
                content: 2,
            },
            Step::Partition { a: 5, b: 6 },
            Step::MassModify {
                node: 1,
                fraction: 98,
                content: 1,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 42,
            },
            Step::Modify {
                node: 3,
                path: 2,
                content: 2,
            },
            Step::Partition { a: 1, b: 4 },
            Step::Delete { node: 3, path: 8 },
            Step::Chmod { node: 0, path: 7 },
            Step::Touch { node: 2, path: 5 },
            Step::Modify {
                node: 7,
                path: 8,
                content: 4,
            },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(5), Some(4), None],
            },
            Step::Chmod { node: 5, path: 7 },
            Step::Touch { node: 4, path: 2 },
            Step::Chmod { node: 7, path: 0 },
            Step::Rename {
                node: 1,
                from: 10,
                to: 4,
            },
            Step::Online { node: 4 },
            Step::Offline { node: 2 },
            Step::Settle { secs: 32 },
            Step::Crash {
                node: 7,
                gap_secs: 106,
            },
            Step::Delete { node: 3, path: 4 },
            Step::Heal { a: 6, b: 3 },
            Step::Chmod { node: 7, path: 0 },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(1), Some(3), Some(1)],
            },
            Step::Create {
                node: 1,
                path: 0,
                content: 2,
            },
        ],
    );
}

/// Same class.
#[test]
fn a_retarget_under_a_pending_commit_is_changed_underneath() {
    passes(
        314,
        &[
            Step::Modify {
                node: 4,
                path: 9,
                content: 1,
            },
            Step::Symlink {
                node: 3,
                path: 8,
                target: 9,
            },
            Step::Chmod { node: 2, path: 11 },
            Step::Crash {
                node: 0,
                gap_secs: 87,
            },
            Step::Symlink {
                node: 1,
                path: 11,
                target: 9,
            },
            Step::Rename {
                node: 1,
                from: 0,
                to: 8,
            },
            Step::Heal { a: 4, b: 1 },
            Step::Partition { a: 2, b: 1 },
            Step::Modify {
                node: 6,
                path: 11,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 4,
            },
            Step::Partition { a: 2, b: 1 },
            Step::Symlink {
                node: 4,
                path: 2,
                target: 1,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 54,
            },
            Step::Chmod { node: 6, path: 10 },
            Step::Online { node: 4 },
            Step::Delete { node: 5, path: 6 },
            Step::MassDelete {
                node: 6,
                fraction: 95,
            },
            Step::Crash {
                node: 0,
                gap_secs: 29,
            },
            Step::Rmdir { node: 0, dir: 0 },
            Step::Modify {
                node: 4,
                path: 9,
                content: 3,
            },
            Step::Touch { node: 2, path: 9 },
            Step::Heal { a: 0, b: 5 },
            Step::Touch { node: 5, path: 0 },
            Step::Delete { node: 6, path: 6 },
            Step::Settle { secs: 37 },
            Step::Modify {
                node: 2,
                path: 4,
                content: 5,
            },
            Step::Rename {
                node: 5,
                from: 5,
                to: 0,
            },
            Step::Partition { a: 6, b: 5 },
            Step::Everywhere {
                path: 10,
                contents: vec![None, Some(1)],
            },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(1), None, Some(1), Some(1), None],
            },
            Step::Create {
                node: 3,
                path: 10,
                content: 2,
            },
            Step::Settle { secs: 27 },
            Step::Create {
                node: 5,
                path: 10,
                content: 4,
            },
            Step::Heal { a: 5, b: 6 },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Delete { node: 4, path: 10 },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Create {
                node: 7,
                path: 9,
                content: 3,
            },
            Step::Modify {
                node: 1,
                path: 4,
                content: 4,
            },
            Step::Modify {
                node: 5,
                path: 4,
                content: 2,
            },
            Step::Modify {
                node: 0,
                path: 7,
                content: 3,
            },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(2), Some(2), Some(1)],
            },
            Step::Heal { a: 1, b: 5 },
            Step::Create {
                node: 4,
                path: 5,
                content: 5,
            },
            Step::Rename {
                node: 4,
                from: 5,
                to: 10,
            },
            Step::Create {
                node: 1,
                path: 3,
                content: 5,
            },
            Step::Settle { secs: 29 },
            Step::Modify {
                node: 3,
                path: 8,
                content: 3,
            },
            Step::MassModify {
                node: 4,
                fraction: 60,
                content: 3,
            },
            Step::Settle { secs: 16 },
            Step::Tier {
                a: 3,
                b: 3,
                tier: 0,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(2), None, Some(3), Some(3), Some(1), Some(1)],
            },
            Step::Tier {
                a: 3,
                b: 0,
                tier: 2,
            },
            Step::Modify {
                node: 5,
                path: 3,
                content: 1,
            },
            Step::Mkdir { node: 7, dir: 0 },
            Step::Everywhere {
                path: 4,
                contents: vec![None, Some(4), None, Some(1)],
            },
            Step::Offline { node: 5 },
            Step::Delete { node: 3, path: 5 },
            Step::Create {
                node: 1,
                path: 10,
                content: 2,
            },
            Step::Modify {
                node: 0,
                path: 4,
                content: 1,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::Revert,
                delay_secs: 24,
            },
            Step::Chmod { node: 4, path: 2 },
            Step::Create {
                node: 1,
                path: 1,
                content: 5,
            },
            Step::Chmod { node: 1, path: 5 },
            Step::MassModify {
                node: 4,
                fraction: 68,
                content: 5,
            },
            Step::Symlink {
                node: 5,
                path: 5,
                target: 1,
            },
            Step::Create {
                node: 5,
                path: 4,
                content: 5,
            },
            Step::Heal { a: 4, b: 1 },
            Step::Partition { a: 2, b: 4 },
            Step::Modify {
                node: 4,
                path: 4,
                content: 4,
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 3,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 44,
            },
            Step::Create {
                node: 6,
                path: 3,
                content: 1,
            },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Symlink {
                node: 4,
                path: 7,
                target: 5,
            },
            Step::Modify {
                node: 3,
                path: 1,
                content: 1,
            },
            Step::Mkdir { node: 3, dir: 0 },
            Step::Partition { a: 5, b: 3 },
            Step::Modify {
                node: 3,
                path: 8,
                content: 4,
            },
            Step::Modify {
                node: 6,
                path: 2,
                content: 4,
            },
            Step::Delete { node: 1, path: 7 },
            Step::Heal { a: 5, b: 2 },
            Step::Tier {
                a: 0,
                b: 3,
                tier: 1,
            },
            Step::Touch { node: 7, path: 8 },
            Step::Rmdir { node: 6, dir: 0 },
            Step::Heal { a: 4, b: 5 },
            Step::Online { node: 0 },
            Step::Modify {
                node: 7,
                path: 7,
                content: 3,
            },
            Step::Tier {
                a: 4,
                b: 6,
                tier: 0,
            },
            Step::Offline { node: 0 },
            Step::Rmdir { node: 7, dir: 2 },
            Step::Crash {
                node: 5,
                gap_secs: 81,
            },
            Step::Modify {
                node: 3,
                path: 2,
                content: 2,
            },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Rename {
                node: 1,
                from: 1,
                to: 8,
            },
            Step::Offline { node: 7 },
            Step::Partition { a: 1, b: 0 },
            Step::Mkdir { node: 5, dir: 0 },
            Step::Modify {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::Heal { a: 7, b: 3 },
            Step::MassDelete {
                node: 6,
                fraction: 58,
            },
            Step::Crash {
                node: 5,
                gap_secs: 64,
            },
            Step::Rename {
                node: 6,
                from: 2,
                to: 8,
            },
            Step::Symlink {
                node: 0,
                path: 7,
                target: 0,
            },
        ],
    );
}

/// Same class.
#[test]
fn a_retarget_under_a_pending_commit_is_changed_underneath_in_another_run() {
    passes(
        491,
        &[
            Step::Touch { node: 2, path: 11 },
            Step::Modify {
                node: 3,
                path: 9,
                content: 3,
            },
            Step::Delete { node: 7, path: 3 },
            Step::Create {
                node: 0,
                path: 7,
                content: 4,
            },
            Step::Rmdir { node: 3, dir: 1 },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(5), None, Some(5), Some(5)],
            },
            Step::Tier {
                a: 6,
                b: 6,
                tier: 1,
            },
            Step::Partition { a: 0, b: 0 },
            Step::Settle { secs: 9 },
            Step::MassDelete {
                node: 2,
                fraction: 73,
            },
            Step::Rename {
                node: 2,
                from: 10,
                to: 11,
            },
            Step::Delete { node: 1, path: 2 },
            Step::Partition { a: 7, b: 6 },
            Step::Touch { node: 7, path: 11 },
            Step::Modify {
                node: 0,
                path: 4,
                content: 4,
            },
            Step::Create {
                node: 7,
                path: 5,
                content: 4,
            },
            Step::Offline { node: 6 },
            Step::Online { node: 3 },
            Step::Touch { node: 5, path: 5 },
            Step::User {
                node: 0,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 8,
                },
                delay_secs: 12,
            },
            Step::Delete { node: 7, path: 8 },
            Step::Delete { node: 6, path: 1 },
            Step::Crash {
                node: 3,
                gap_secs: 7,
            },
            Step::Modify {
                node: 4,
                path: 1,
                content: 1,
            },
            Step::Offline { node: 4 },
            Step::Rmdir { node: 7, dir: 2 },
            Step::Heal { a: 3, b: 5 },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 52,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![None, Some(1), None, Some(5)],
            },
            Step::Delete { node: 5, path: 11 },
            Step::Rename {
                node: 6,
                from: 1,
                to: 3,
            },
            Step::Offline { node: 6 },
            Step::MassModify {
                node: 0,
                fraction: 75,
                content: 2,
            },
            Step::Online { node: 0 },
            Step::Create {
                node: 3,
                path: 5,
                content: 5,
            },
            Step::Offline { node: 6 },
            Step::Delete { node: 0, path: 11 },
            Step::Offline { node: 7 },
            Step::Partition { a: 0, b: 0 },
            Step::Settle { secs: 22 },
            Step::Crash {
                node: 4,
                gap_secs: 40,
            },
            Step::Create {
                node: 0,
                path: 3,
                content: 2,
            },
            Step::Partition { a: 0, b: 4 },
            Step::Mkdir { node: 3, dir: 0 },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 42,
            },
            Step::Online { node: 4 },
            Step::Create {
                node: 3,
                path: 7,
                content: 1,
            },
            Step::Touch { node: 3, path: 8 },
            Step::Modify {
                node: 3,
                path: 3,
                content: 3,
            },
            Step::Offline { node: 6 },
            Step::Delete { node: 6, path: 10 },
            Step::Crash {
                node: 2,
                gap_secs: 104,
            },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Create {
                node: 7,
                path: 6,
                content: 3,
            },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Rename {
                node: 5,
                from: 6,
                to: 11,
            },
            Step::Online { node: 5 },
            Step::Online { node: 5 },
            Step::Heal { a: 5, b: 6 },
            Step::Rename {
                node: 4,
                from: 1,
                to: 2,
            },
            Step::Settle { secs: 24 },
            Step::Rename {
                node: 5,
                from: 10,
                to: 0,
            },
            Step::Crash {
                node: 1,
                gap_secs: 93,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 21,
            },
            Step::Heal { a: 2, b: 1 },
            Step::Modify {
                node: 2,
                path: 3,
                content: 1,
            },
            Step::Create {
                node: 1,
                path: 11,
                content: 2,
            },
            Step::Heal { a: 7, b: 4 },
            Step::Create {
                node: 6,
                path: 11,
                content: 2,
            },
            Step::Online { node: 2 },
            Step::Create {
                node: 2,
                path: 8,
                content: 1,
            },
            Step::Create {
                node: 3,
                path: 10,
                content: 4,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 2,
            },
            Step::Settle { secs: 18 },
            Step::Modify {
                node: 7,
                path: 4,
                content: 1,
            },
            Step::Touch { node: 1, path: 11 },
            Step::Delete { node: 7, path: 8 },
            Step::Modify {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::Crash {
                node: 6,
                gap_secs: 69,
            },
            Step::Modify {
                node: 3,
                path: 10,
                content: 2,
            },
            Step::Chmod { node: 2, path: 2 },
            Step::Partition { a: 2, b: 2 },
            Step::Tier {
                a: 2,
                b: 0,
                tier: 1,
            },
            Step::Rename {
                node: 6,
                from: 4,
                to: 10,
            },
            Step::Modify {
                node: 0,
                path: 5,
                content: 5,
            },
            Step::Modify {
                node: 4,
                path: 5,
                content: 2,
            },
            Step::Chmod { node: 3, path: 1 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(5), Some(4), Some(3), Some(5), Some(2), None, None],
            },
            Step::Delete { node: 0, path: 0 },
            Step::Crash {
                node: 4,
                gap_secs: 1,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![None, Some(4), Some(3), Some(1), Some(1)],
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 11,
            },
            Step::Modify {
                node: 5,
                path: 4,
                content: 4,
            },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(1), Some(1), Some(2), Some(3), Some(4), Some(1)],
            },
            Step::Chmod { node: 1, path: 6 },
            Step::Modify {
                node: 7,
                path: 8,
                content: 4,
            },
            Step::Modify {
                node: 7,
                path: 8,
                content: 4,
            },
            Step::MassDelete {
                node: 6,
                fraction: 77,
            },
            Step::Partition { a: 1, b: 7 },
            Step::Offline { node: 3 },
            Step::Create {
                node: 7,
                path: 0,
                content: 3,
            },
            Step::Chmod { node: 6, path: 2 },
            Step::Delete { node: 5, path: 8 },
            Step::Heal { a: 6, b: 2 },
            Step::Create {
                node: 0,
                path: 0,
                content: 2,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Rules {
                    hold_count: 4,
                    hold_pct: 0,
                },
                delay_secs: 0,
            },
            Step::Create {
                node: 4,
                path: 2,
                content: 4,
            },
            Step::Rmdir { node: 6, dir: 1 },
            Step::Symlink {
                node: 3,
                path: 8,
                target: 7,
            },
            Step::Tier {
                a: 0,
                b: 5,
                tier: 1,
            },
            Step::Settle { secs: 7 },
            Step::Tier {
                a: 5,
                b: 1,
                tier: 1,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 26,
                },
                delay_secs: 11,
            },
            Step::Partition { a: 5, b: 4 },
            Step::Symlink {
                node: 3,
                path: 8,
                target: 10,
            },
            Step::Modify {
                node: 5,
                path: 8,
                content: 5,
            },
        ],
    );
}

/// A concurrent entry joining a held item had its version quarantined but
/// not its entry, so `deny`'s bump folded the version into its vector while
/// the stamp it started from ignored it: the bump dominated a metadata-only
/// version and ranked below it. Its tombstone then lost to that content on
/// one node and won on another, and one vector carried both (I7). Every
/// quarantined version now carries its stamp, and the bump starts above
/// the largest of them.
#[test]
fn a_joined_versions_stamp_counts_for_the_deny_bump() {
    passes(
        938,
        &[
            Step::Partition { a: 5, b: 1 },
            Step::Modify {
                node: 6,
                path: 7,
                content: 4,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![None, None, Some(3), None],
            },
            Step::Create {
                node: 4,
                path: 9,
                content: 4,
            },
            Step::Modify {
                node: 1,
                path: 9,
                content: 5,
            },
            Step::Touch { node: 2, path: 5 },
            Step::Create {
                node: 6,
                path: 2,
                content: 5,
            },
            Step::Symlink {
                node: 1,
                path: 4,
                target: 6,
            },
            Step::Touch { node: 4, path: 1 },
            Step::Modify {
                node: 6,
                path: 4,
                content: 2,
            },
            Step::Tier {
                a: 7,
                b: 4,
                tier: 0,
            },
            Step::Create {
                node: 4,
                path: 0,
                content: 3,
            },
            Step::Modify {
                node: 4,
                path: 0,
                content: 5,
            },
            Step::Create {
                node: 5,
                path: 0,
                content: 5,
            },
            Step::Delete { node: 4, path: 4 },
            Step::Tier {
                a: 0,
                b: 7,
                tier: 0,
            },
            Step::Settle { secs: 9 },
            Step::Modify {
                node: 1,
                path: 3,
                content: 2,
            },
            Step::Heal { a: 1, b: 4 },
            Step::Heal { a: 2, b: 6 },
            Step::Create {
                node: 7,
                path: 9,
                content: 3,
            },
            Step::Rmdir { node: 6, dir: 0 },
            Step::Create {
                node: 0,
                path: 1,
                content: 1,
            },
            Step::Delete { node: 0, path: 8 },
            Step::Online { node: 4 },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 36,
            },
            Step::Modify {
                node: 7,
                path: 5,
                content: 4,
            },
            Step::Rmdir { node: 4, dir: 2 },
            Step::Delete { node: 3, path: 3 },
            Step::Offline { node: 5 },
            Step::Modify {
                node: 4,
                path: 8,
                content: 2,
            },
            Step::Create {
                node: 2,
                path: 10,
                content: 2,
            },
            Step::Settle { secs: 17 },
            Step::Heal { a: 7, b: 6 },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 40,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 5,
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(2), Some(5), Some(4), None, Some(3), None],
            },
            Step::Settle { secs: 12 },
            Step::MassDelete {
                node: 2,
                fraction: 59,
            },
            Step::Delete { node: 5, path: 0 },
            Step::Heal { a: 1, b: 1 },
            Step::Modify {
                node: 1,
                path: 6,
                content: 5,
            },
            Step::Heal { a: 4, b: 0 },
            Step::Create {
                node: 6,
                path: 4,
                content: 4,
            },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Create {
                node: 6,
                path: 7,
                content: 2,
            },
            Step::Offline { node: 4 },
            Step::Create {
                node: 5,
                path: 5,
                content: 5,
            },
            Step::Modify {
                node: 0,
                path: 10,
                content: 2,
            },
            Step::Delete { node: 4, path: 8 },
            Step::Delete { node: 3, path: 0 },
            Step::Modify {
                node: 0,
                path: 5,
                content: 2,
            },
            Step::Modify {
                node: 7,
                path: 4,
                content: 4,
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 3,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 27,
            },
            Step::Modify {
                node: 4,
                path: 10,
                content: 1,
            },
            Step::Create {
                node: 6,
                path: 3,
                content: 2,
            },
            Step::Partition { a: 5, b: 5 },
            Step::Touch { node: 5, path: 8 },
            Step::MassDelete {
                node: 7,
                fraction: 98,
            },
            Step::Delete { node: 6, path: 0 },
            Step::MassModify {
                node: 2,
                fraction: 61,
                content: 2,
            },
            Step::Modify {
                node: 4,
                path: 9,
                content: 4,
            },
            Step::Delete { node: 3, path: 1 },
            Step::Partition { a: 3, b: 4 },
            Step::MassModify {
                node: 2,
                fraction: 99,
                content: 4,
            },
            Step::Settle { secs: 18 },
            Step::Create {
                node: 4,
                path: 1,
                content: 5,
            },
            Step::Rmdir { node: 0, dir: 1 },
            Step::Mkdir { node: 5, dir: 0 },
            Step::Heal { a: 6, b: 6 },
            Step::Create {
                node: 7,
                path: 7,
                content: 2,
            },
            Step::Modify {
                node: 5,
                path: 1,
                content: 2,
            },
            Step::Delete { node: 2, path: 1 },
            Step::Heal { a: 3, b: 4 },
            Step::Modify {
                node: 7,
                path: 8,
                content: 3,
            },
            Step::Offline { node: 5 },
            Step::Settle { secs: 39 },
            Step::Crash {
                node: 2,
                gap_secs: 113,
            },
            Step::Modify {
                node: 7,
                path: 0,
                content: 2,
            },
            Step::Create {
                node: 2,
                path: 6,
                content: 1,
            },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Modify {
                node: 6,
                path: 3,
                content: 2,
            },
            Step::Partition { a: 0, b: 1 },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Modify {
                node: 2,
                path: 5,
                content: 3,
            },
            Step::Delete { node: 3, path: 7 },
            Step::Create {
                node: 6,
                path: 6,
                content: 1,
            },
            Step::Create {
                node: 4,
                path: 9,
                content: 5,
            },
            Step::Delete { node: 4, path: 7 },
            Step::MassDelete {
                node: 5,
                fraction: 58,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 0,
            },
            Step::Rename {
                node: 1,
                from: 10,
                to: 9,
            },
            Step::Rename {
                node: 1,
                from: 11,
                to: 5,
            },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Partition { a: 4, b: 3 },
            Step::Create {
                node: 7,
                path: 0,
                content: 3,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(1), Some(3)],
            },
            Step::Settle { secs: 31 },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 23,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(2), Some(3), Some(4), Some(1)],
            },
            Step::Chmod { node: 0, path: 5 },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 11,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![None, Some(1), None, Some(3), Some(4)],
            },
            Step::Delete { node: 5, path: 1 },
            Step::Modify {
                node: 4,
                path: 6,
                content: 4,
            },
            Step::Modify {
                node: 4,
                path: 8,
                content: 4,
            },
            Step::Mkdir { node: 4, dir: 0 },
            Step::Create {
                node: 0,
                path: 6,
                content: 3,
            },
            Step::Delete { node: 0, path: 8 },
            Step::Modify {
                node: 6,
                path: 8,
                content: 5,
            },
            Step::MassDelete {
                node: 3,
                fraction: 72,
            },
            Step::Symlink {
                node: 2,
                path: 8,
                target: 6,
            },
            Step::Heal { a: 3, b: 5 },
            Step::Heal { a: 0, b: 2 },
            Step::Tier {
                a: 3,
                b: 7,
                tier: 2,
            },
            Step::Delete { node: 0, path: 5 },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(5), Some(1), None],
            },
            Step::Modify {
                node: 0,
                path: 1,
                content: 5,
            },
            Step::Create {
                node: 4,
                path: 11,
                content: 4,
            },
            Step::Create {
                node: 0,
                path: 9,
                content: 1,
            },
            Step::Create {
                node: 2,
                path: 7,
                content: 3,
            },
            Step::Offline { node: 0 },
            Step::Mkdir { node: 3, dir: 2 },
            Step::Delete { node: 6, path: 10 },
            Step::Create {
                node: 5,
                path: 1,
                content: 5,
            },
            Step::Modify {
                node: 2,
                path: 10,
                content: 2,
            },
            Step::Modify {
                node: 7,
                path: 2,
                content: 5,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(3), Some(3), Some(2), None, Some(5)],
            },
            Step::Modify {
                node: 1,
                path: 10,
                content: 4,
            },
            Step::Partition { a: 2, b: 5 },
            Step::Modify {
                node: 1,
                path: 10,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 8,
                content: 5,
            },
            Step::Touch { node: 4, path: 1 },
            Step::Modify {
                node: 6,
                path: 5,
                content: 5,
            },
            Step::Chmod { node: 4, path: 4 },
            Step::Delete { node: 0, path: 8 },
            Step::Settle { secs: 38 },
            Step::User {
                node: 6,
                action: crate::UserAction::DenyAll,
                delay_secs: 9,
            },
            Step::MassDelete {
                node: 2,
                fraction: 58,
            },
            Step::Chmod { node: 1, path: 2 },
            Step::Partition { a: 0, b: 0 },
        ],
    );
}
