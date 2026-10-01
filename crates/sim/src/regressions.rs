//! Seeds the simulator found and the engine was fixed for (DESIGN.md
//! §14.4: every bug gets a simulator step that reproduces it before it is
//! fixed). Each test pins a seed, the knobs and the minimal step list the
//! shrinker produced.
//!
//! A pin guards its fix only while its history still reaches the bug, and a
//! change to the simulator's draws or defaults can quietly move it
//! elsewhere. So every pin NAME has `scripts/pins/NAME.patch`, which
//! disables its fix, and is valid only if it fails with that patch applied
//! and passes without it; `scripts/pins.sh` checks both halves, and the
//! nightly runs it over every pin (§14.4). A pin that stops failing with its
//! patch is pinned again at a seed that does, found with the patch applied
//! at the current knobs and shrunk the same way, or, if no seed reaches the
//! fix, replaced by the fix's unit test, which its patch then names.

use crate::steps::Step;
use crate::{Knobs, run_twice};

fn passes(seed: u64, steps: &[Step]) {
    passes_with(seed, &Knobs::default(), steps);
}

/// For seeds found with other knobs than the defaults.
fn passes_with(seed: u64, knobs: &Knobs, steps: &[Step]) {
    if let Err(f) = run_twice(seed, knobs, steps) {
        panic!("{f}");
    }
}

/// The knobs of the nightly's corruption slice (§14.1): one fetch in twenty
/// arrives with the wrong hash.
fn corrupting() -> Knobs {
    Knobs {
        corruption: 0.05,
        ..Knobs::default()
    }
}

/// `knobs` without draft 34's simulator models (§14.1): no crash between a
/// commit's two renames, no group-commit lag, a displaced directory leaves
/// its children, and no scan skips a path (§7.3). Nor the later ones: no
/// fetch or commit fails on its machine (§7.5), and no node ignores case
/// (§7.6). Every pin before them was found this way.
fn before_draft_34(knobs: &Knobs) -> Knobs {
    Knobs {
        crash_between_renames: 0.0,
        group_commit_lag: 0,
        displace_subtrees: false,
        skip: 0.0,
        io_failure: 0.0,
        disk_fill: 0.0,
        case_variants: 0.0,
        ..knobs.clone()
    }
}

/// For a pin whose history draft 34's models change so that it no longer
/// reaches its scenario: replay the history it was found in, which fails
/// with its fix disabled, and keep the list passing under `knobs` as well.
fn passes_as_found(seed: u64, knobs: &Knobs, steps: &[Step]) {
    passes_with(seed, &before_draft_34(knobs), steps);
    passes_with(seed, knobs, steps);
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
/// Re-pinned at seed 0 with the default knobs, shrunk to 9 steps, after
/// drafts 52 and 53's failed-report rules moved seed 1000's history.
#[test]
fn the_same_directory_created_twice_is_not_a_winner_fallback() {
    passes(
        0,
        &[
            Step::Modify {
                node: 4,
                path: 4,
                content: 4,
            },
            Step::Modify {
                node: 4,
                path: 4,
                content: 4,
            },
            Step::Modify {
                node: 7,
                path: 11,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::Offline { node: 7 },
            Step::Chmod { node: 3, path: 1 },
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
                node: 7,
                path: 9,
                content: 1,
            },
        ],
    );
}

/// `revert` put the announced records back and removed never-announced adds
/// without reporting either, so the persisted index kept the pending
/// records and a restart would have re-announced the reverted changes.
/// Replayed as found: group commit alone loses its scenario (PR 1b).
#[test]
fn a_revert_reports_every_index_write() {
    passes_as_found(
        0,
        &Knobs::default(),
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

/// A source that answered `NotAvailable` was excluded for good, and its
/// later announcement of the wanted content did not make it a source again,
/// so the want had no source forever. It was found with a conflict's merged
/// version `M`, whose first announcer, the winner's holder, could not serve
/// it until it had seen the loser too. Pinned at seed 929 with the default
/// knobs, shrunk to 26 steps (found at 100 steps), after the late-fetch fix
/// moved seed 768's history; with the fix disabled the run fails I1, one
/// node never getting `d2/f8`. Seven of seeds 0-999 fail that way at 400
/// steps.
#[test]
fn a_source_that_announces_the_wanted_version_again_is_asked_again() {
    passes(
        929,
        &[
            Step::Modify {
                node: 2,
                path: 9,
                content: 2,
            },
            Step::Create {
                node: 3,
                path: 1,
                content: 4,
            },
            Step::Create {
                node: 1,
                path: 2,
                content: 3,
            },
            Step::Settle { secs: 15 },
            Step::Offline { node: 3 },
            Step::Delete { node: 1, path: 3 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(2), Some(3), Some(4), Some(1), Some(3)],
            },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(4), None],
            },
            Step::Heal { a: 1, b: 4 },
            Step::Settle { secs: 7 },
            Step::Create {
                node: 6,
                path: 8,
                content: 5,
            },
            Step::Heal { a: 5, b: 2 },
            Step::Rmdir { node: 2, dir: 0 },
            Step::Crash {
                node: 0,
                gap_secs: 105,
            },
            Step::Online { node: 7 },
            Step::Offline { node: 4 },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 1,
                    hold_pct: 50,
                },
                delay_secs: 58,
            },
            Step::Modify {
                node: 4,
                path: 6,
                content: 4,
            },
            Step::Create {
                node: 7,
                path: 10,
                content: 4,
            },
            Step::Online { node: 6 },
            Step::Crash {
                node: 2,
                gap_secs: 7,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 47,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 4,
            },
            Step::Online { node: 4 },
            Step::Delete { node: 0, path: 3 },
            Step::Create {
                node: 0,
                path: 0,
                content: 2,
            },
        ],
    );
}

/// A crash between a fetch and its commit kept the want's fetched flag
/// while the temp file died with the process; the restarted node asked the
/// host to commit content it did not have, the commit failed, and the entry
/// was deferred at a path with no record and no file, which no scan ever
/// reports. Two fixes guard it, and with either alone the node recovers: a
/// restored want fetches again (ccf9d16), and the end of a full scan
/// reconsiders deferred entries at paths nothing records (ebae00b). Pinned
/// at seed 0 with the default knobs, shrunk to 41 steps; with both fixes
/// disabled it fails quiescence, an entry deferred ChangedUnderneath at
/// d0/f6 with no record there.
#[test]
fn a_want_fetched_before_a_crash_is_fetched_again() {
    passes(
        0,
        &[
            Step::Online { node: 1 },
            Step::Symlink {
                node: 4,
                path: 10,
                target: 1,
            },
            Step::Partition { a: 7, b: 4 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(5), Some(2), Some(1), Some(5), Some(3)],
            },
            Step::Create {
                node: 1,
                path: 5,
                content: 2,
            },
            Step::Crash {
                node: 3,
                gap_secs: 40,
            },
            Step::Create {
                node: 4,
                path: 3,
                content: 5,
            },
            Step::Touch { node: 4, path: 3 },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(3), Some(1), Some(3)],
            },
            Step::Offline { node: 7 },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Delete { node: 7, path: 4 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(1), None, Some(3), Some(3)],
            },
            Step::Modify {
                node: 7,
                path: 7,
                content: 1,
            },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Partition { a: 6, b: 2 },
            Step::Rename {
                node: 2,
                from: 6,
                to: 8,
            },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Create {
                node: 2,
                path: 3,
                content: 3,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 41,
            },
            Step::Settle { secs: 19 },
            Step::Modify {
                node: 3,
                path: 3,
                content: 4,
            },
            Step::Settle { secs: 29 },
            Step::Modify {
                node: 4,
                path: 3,
                content: 5,
            },
            Step::Heal { a: 3, b: 3 },
            Step::Create {
                node: 4,
                path: 6,
                content: 5,
            },
            Step::Settle { secs: 33 },
            Step::Crash {
                node: 2,
                gap_secs: 86,
            },
            Step::Heal { a: 0, b: 0 },
            Step::Delete { node: 2, path: 10 },
            Step::Touch { node: 4, path: 11 },
            Step::MassModify {
                node: 7,
                fraction: 82,
                content: 5,
            },
            Step::Modify {
                node: 5,
                path: 11,
                content: 3,
            },
            Step::Chmod { node: 2, path: 7 },
            Step::Delete { node: 1, path: 2 },
            Step::Heal { a: 3, b: 1 },
            Step::Online { node: 2 },
            Step::Touch { node: 6, path: 3 },
            Step::Crash {
                node: 3,
                gap_secs: 99,
            },
            Step::Touch { node: 6, path: 1 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(1), Some(4)],
            },
        ],
    );
}

/// Requests named a version, and a conflict's merged version `M` exists
/// nowhere until someone merges: the winner's holders answered
/// `NotAvailable` for `M` although they held its content. Fetches are by
/// hash now (§7.5 steps 2 and 3). The pin fails quiescence: re-pinned at
/// seed 560 with the default knobs, shrunk to 143 steps, after
/// draft 54's refusal rules moved seed 22's history; with the host serving by
/// version again the run never settles, an entry at f0 staying deferred
/// for good. It was found failing I1, a path whose concurrent holders never
/// all met staying different across the mesh, at seed 30, until the
/// late-fetch fix moved that history.
#[test]
fn content_is_fetched_by_hash_from_whoever_holds_it() {
    passes(
        560,
        &[
            Step::Delete { node: 4, path: 10 },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(2), None, Some(4)],
            },
            Step::Heal { a: 6, b: 1 },
            Step::Touch { node: 4, path: 10 },
            Step::Touch { node: 0, path: 0 },
            Step::Modify {
                node: 0,
                path: 6,
                content: 3,
            },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Online { node: 7 },
            Step::Rename {
                node: 7,
                from: 9,
                to: 11,
            },
            Step::Crash {
                node: 7,
                gap_secs: 83,
            },
            Step::Modify {
                node: 4,
                path: 8,
                content: 1,
            },
            Step::Heal { a: 0, b: 4 },
            Step::Create {
                node: 0,
                path: 5,
                content: 3,
            },
            Step::Heal { a: 5, b: 0 },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 26,
            },
            Step::Delete { node: 5, path: 2 },
            Step::Rename {
                node: 5,
                from: 1,
                to: 2,
            },
            Step::Modify {
                node: 3,
                path: 6,
                content: 2,
            },
            Step::Partition { a: 4, b: 2 },
            Step::Partition { a: 4, b: 3 },
            Step::Create {
                node: 3,
                path: 5,
                content: 5,
            },
            Step::Heal { a: 1, b: 7 },
            Step::Create {
                node: 6,
                path: 10,
                content: 3,
            },
            Step::Settle { secs: 17 },
            Step::Settle { secs: 34 },
            Step::Chmod { node: 6, path: 9 },
            Step::Mkdir { node: 5, dir: 0 },
            Step::Partition { a: 7, b: 3 },
            Step::Tier {
                a: 7,
                b: 5,
                tier: 0,
            },
            Step::Settle { secs: 17 },
            Step::Partition { a: 0, b: 0 },
            Step::Offline { node: 0 },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 51,
            },
            Step::Partition { a: 7, b: 0 },
            Step::Online { node: 7 },
            Step::Modify {
                node: 7,
                path: 7,
                content: 2,
            },
            Step::Delete { node: 2, path: 11 },
            Step::Crash {
                node: 1,
                gap_secs: 29,
            },
            Step::Modify {
                node: 7,
                path: 1,
                content: 2,
            },
            Step::Delete { node: 2, path: 2 },
            Step::Everywhere {
                path: 10,
                contents: vec![None, None],
            },
            Step::Create {
                node: 4,
                path: 5,
                content: 4,
            },
            Step::Heal { a: 4, b: 0 },
            Step::Delete { node: 5, path: 9 },
            Step::Delete { node: 6, path: 5 },
            Step::Modify {
                node: 7,
                path: 2,
                content: 3,
            },
            Step::Create {
                node: 0,
                path: 0,
                content: 5,
            },
            Step::Create {
                node: 0,
                path: 6,
                content: 1,
            },
            Step::Create {
                node: 5,
                path: 9,
                content: 2,
            },
            Step::Settle { secs: 36 },
            Step::MassDelete {
                node: 0,
                fraction: 89,
            },
            Step::Symlink {
                node: 2,
                path: 4,
                target: 9,
            },
            Step::Offline { node: 2 },
            Step::Touch { node: 7, path: 4 },
            Step::Rename {
                node: 4,
                from: 0,
                to: 9,
            },
            Step::Offline { node: 6 },
            Step::Online { node: 1 },
            Step::User {
                node: 6,
                action: crate::UserAction::DenyAll,
                delay_secs: 5,
            },
            Step::Tier {
                a: 7,
                b: 2,
                tier: 0,
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(2), Some(4), Some(4)],
            },
            Step::Settle { secs: 10 },
            Step::Everywhere {
                path: 8,
                contents: vec![None, Some(2), Some(5)],
            },
            Step::Modify {
                node: 0,
                path: 4,
                content: 1,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(3), Some(4), Some(5), Some(3)],
            },
            Step::Settle { secs: 37 },
            Step::Heal { a: 2, b: 1 },
            Step::Modify {
                node: 3,
                path: 6,
                content: 2,
            },
            Step::Chmod { node: 2, path: 11 },
            Step::MassModify {
                node: 7,
                fraction: 63,
                content: 3,
            },
            Step::Symlink {
                node: 2,
                path: 8,
                target: 1,
            },
            Step::Delete { node: 3, path: 11 },
            Step::Offline { node: 2 },
            Step::Modify {
                node: 2,
                path: 9,
                content: 4,
            },
            Step::Create {
                node: 5,
                path: 10,
                content: 5,
            },
            Step::Rmdir { node: 3, dir: 0 },
            Step::Settle { secs: 29 },
            Step::Delete { node: 5, path: 7 },
            Step::Offline { node: 2 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 49,
            },
            Step::Delete { node: 3, path: 0 },
            Step::Settle { secs: 18 },
            Step::Modify {
                node: 0,
                path: 6,
                content: 3,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 32,
            },
            Step::Tier {
                a: 1,
                b: 2,
                tier: 2,
            },
            Step::Offline { node: 3 },
            Step::Crash {
                node: 5,
                gap_secs: 11,
            },
            Step::Delete { node: 3, path: 3 },
            Step::Delete { node: 5, path: 3 },
            Step::Heal { a: 7, b: 3 },
            Step::Create {
                node: 3,
                path: 11,
                content: 4,
            },
            Step::Touch { node: 1, path: 9 },
            Step::Touch { node: 2, path: 9 },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 14,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(4), Some(4)],
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 52,
                },
                delay_secs: 38,
            },
            Step::Modify {
                node: 0,
                path: 0,
                content: 4,
            },
            Step::Modify {
                node: 1,
                path: 7,
                content: 2,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 52,
            },
            Step::Create {
                node: 6,
                path: 8,
                content: 5,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 28,
            },
            Step::Online { node: 0 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(1), None],
            },
            Step::Chmod { node: 5, path: 4 },
            Step::Settle { secs: 3 },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Create {
                node: 2,
                path: 3,
                content: 1,
            },
            Step::Tier {
                a: 2,
                b: 6,
                tier: 1,
            },
            Step::Tier {
                a: 0,
                b: 5,
                tier: 0,
            },
            Step::Modify {
                node: 6,
                path: 2,
                content: 2,
            },
            Step::Create {
                node: 3,
                path: 0,
                content: 1,
            },
            Step::Heal { a: 4, b: 1 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 4,
            },
            Step::Tier {
                a: 1,
                b: 7,
                tier: 0,
            },
            Step::Tier {
                a: 2,
                b: 5,
                tier: 0,
            },
            Step::Delete { node: 0, path: 10 },
            Step::Heal { a: 7, b: 0 },
            Step::Online { node: 5 },
            Step::Modify {
                node: 0,
                path: 6,
                content: 4,
            },
            Step::Offline { node: 6 },
            Step::Create {
                node: 5,
                path: 11,
                content: 2,
            },
            Step::Online { node: 6 },
            Step::Rename {
                node: 1,
                from: 4,
                to: 11,
            },
            Step::Settle { secs: 20 },
            Step::Modify {
                node: 1,
                path: 0,
                content: 3,
            },
            Step::Modify {
                node: 0,
                path: 5,
                content: 3,
            },
            Step::Partition { a: 1, b: 5 },
            Step::Create {
                node: 2,
                path: 10,
                content: 5,
            },
            Step::Tier {
                a: 5,
                b: 1,
                tier: 0,
            },
            Step::Partition { a: 3, b: 1 },
            Step::Create {
                node: 7,
                path: 11,
                content: 1,
            },
            Step::Symlink {
                node: 7,
                path: 6,
                target: 7,
            },
            Step::Rename {
                node: 3,
                from: 0,
                to: 5,
            },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(1), None, Some(2), None],
            },
            Step::Crash {
                node: 5,
                gap_secs: 113,
            },
            Step::Offline { node: 6 },
            Step::Heal { a: 1, b: 2 },
            Step::Symlink {
                node: 0,
                path: 1,
                target: 4,
            },
            Step::Create {
                node: 3,
                path: 3,
                content: 1,
            },
            Step::Settle { secs: 29 },
            Step::Everywhere {
                path: 8,
                contents: vec![None, Some(4), Some(2), Some(1), Some(5), Some(1), Some(4)],
            },
            Step::Settle { secs: 15 },
            Step::Create {
                node: 2,
                path: 0,
                content: 5,
            },
            Step::Chmod { node: 1, path: 10 },
        ],
    );
}

/// A batch lost to a crash or a dropped link was never caught up: the
/// receiver took the next batch's `seq_high` as its watermark although
/// it had never seen the sequence numbers below it, so `have_up_to` asked
/// for nothing and the lost records never reached it. Batches chain
/// through `seq_low` now and acknowledgements are the contiguous watermark
/// (§7.4). Re-pinned at seed 88 with the default knobs, shrunk to 254
/// steps, after draft 54's refusal rules moved seed 229's history; with the fix
/// disabled it fails I1: two nodes' indexes differ at a conflict copy of
/// `f3`, live on one and absent on the other.
#[test]
fn a_batch_lost_in_flight_is_caught_up() {
    passes(
        88,
        &[
            Step::Offline { node: 6 },
            Step::Rename {
                node: 1,
                from: 2,
                to: 7,
            },
            Step::Offline { node: 7 },
            Step::Symlink {
                node: 5,
                path: 4,
                target: 11,
            },
            Step::Offline { node: 5 },
            Step::Settle { secs: 21 },
            Step::Modify {
                node: 5,
                path: 6,
                content: 1,
            },
            Step::Chmod { node: 5, path: 6 },
            Step::Delete { node: 5, path: 0 },
            Step::MassDelete {
                node: 1,
                fraction: 84,
            },
            Step::Create {
                node: 1,
                path: 2,
                content: 4,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 11,
            },
            Step::Tier {
                a: 3,
                b: 0,
                tier: 2,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 56,
            },
            Step::Delete { node: 6, path: 7 },
            Step::Touch { node: 1, path: 5 },
            Step::Delete { node: 7, path: 5 },
            Step::Partition { a: 2, b: 0 },
            Step::Modify {
                node: 4,
                path: 10,
                content: 2,
            },
            Step::Rename {
                node: 5,
                from: 5,
                to: 1,
            },
            Step::Partition { a: 7, b: 1 },
            Step::Create {
                node: 2,
                path: 11,
                content: 5,
            },
            Step::Partition { a: 4, b: 0 },
            Step::Create {
                node: 2,
                path: 9,
                content: 4,
            },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Create {
                node: 3,
                path: 0,
                content: 2,
            },
            Step::Create {
                node: 3,
                path: 10,
                content: 5,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 45,
            },
            Step::Tier {
                a: 4,
                b: 4,
                tier: 0,
            },
            Step::Heal { a: 0, b: 3 },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Touch { node: 4, path: 8 },
            Step::Create {
                node: 2,
                path: 9,
                content: 4,
            },
            Step::Online { node: 4 },
            Step::Modify {
                node: 2,
                path: 11,
                content: 2,
            },
            Step::Symlink {
                node: 1,
                path: 9,
                target: 11,
            },
            Step::Online { node: 5 },
            Step::Crash {
                node: 6,
                gap_secs: 3,
            },
            Step::MassDelete {
                node: 6,
                fraction: 61,
            },
            Step::Chmod { node: 4, path: 10 },
            Step::Create {
                node: 0,
                path: 3,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 2,
                content: 1,
            },
            Step::Partition { a: 4, b: 3 },
            Step::Modify {
                node: 6,
                path: 0,
                content: 4,
            },
            Step::Chmod { node: 2, path: 11 },
            Step::Create {
                node: 6,
                path: 6,
                content: 5,
            },
            Step::Offline { node: 0 },
            Step::MassModify {
                node: 3,
                fraction: 81,
                content: 3,
            },
            Step::Crash {
                node: 6,
                gap_secs: 18,
            },
            Step::Settle { secs: 9 },
            Step::Partition { a: 5, b: 6 },
            Step::Delete { node: 6, path: 1 },
            Step::MassDelete {
                node: 6,
                fraction: 62,
            },
            Step::Settle { secs: 39 },
            Step::Create {
                node: 6,
                path: 0,
                content: 1,
            },
            Step::Create {
                node: 0,
                path: 4,
                content: 4,
            },
            Step::Create {
                node: 2,
                path: 7,
                content: 5,
            },
            Step::Modify {
                node: 4,
                path: 11,
                content: 1,
            },
            Step::Create {
                node: 0,
                path: 3,
                content: 3,
            },
            Step::Modify {
                node: 4,
                path: 4,
                content: 4,
            },
            Step::Rename {
                node: 7,
                from: 2,
                to: 0,
            },
            Step::Create {
                node: 3,
                path: 10,
                content: 4,
            },
            Step::Settle { secs: 15 },
            Step::Delete { node: 0, path: 5 },
            Step::Create {
                node: 3,
                path: 0,
                content: 4,
            },
            Step::Tier {
                a: 7,
                b: 2,
                tier: 1,
            },
            Step::Create {
                node: 4,
                path: 2,
                content: 4,
            },
            Step::Offline { node: 7 },
            Step::Rmdir { node: 5, dir: 1 },
            Step::Offline { node: 0 },
            Step::MassModify {
                node: 7,
                fraction: 80,
                content: 3,
            },
            Step::Modify {
                node: 7,
                path: 7,
                content: 3,
            },
            Step::Mkdir { node: 1, dir: 2 },
            Step::MassDelete {
                node: 1,
                fraction: 67,
            },
            Step::Tier {
                a: 7,
                b: 3,
                tier: 0,
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 2,
            },
            Step::Settle { secs: 23 },
            Step::Settle { secs: 24 },
            Step::Heal { a: 6, b: 2 },
            Step::Modify {
                node: 3,
                path: 8,
                content: 2,
            },
            Step::Heal { a: 7, b: 6 },
            Step::Modify {
                node: 6,
                path: 2,
                content: 5,
            },
            Step::Mkdir { node: 5, dir: 0 },
            Step::Symlink {
                node: 6,
                path: 3,
                target: 1,
            },
            Step::Rename {
                node: 6,
                from: 8,
                to: 7,
            },
            Step::Heal { a: 6, b: 3 },
            Step::Modify {
                node: 3,
                path: 5,
                content: 4,
            },
            Step::Heal { a: 4, b: 6 },
            Step::Modify {
                node: 2,
                path: 5,
                content: 3,
            },
            Step::Partition { a: 1, b: 3 },
            Step::Settle { secs: 24 },
            Step::Partition { a: 4, b: 3 },
            Step::Crash {
                node: 4,
                gap_secs: 99,
            },
            Step::Touch { node: 5, path: 11 },
            Step::Touch { node: 4, path: 3 },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Delete { node: 2, path: 6 },
            Step::Modify {
                node: 3,
                path: 9,
                content: 5,
            },
            Step::Create {
                node: 5,
                path: 4,
                content: 4,
            },
            Step::Tier {
                a: 3,
                b: 0,
                tier: 2,
            },
            Step::Offline { node: 4 },
            Step::Create {
                node: 6,
                path: 11,
                content: 3,
            },
            Step::Modify {
                node: 2,
                path: 8,
                content: 3,
            },
            Step::Create {
                node: 3,
                path: 4,
                content: 5,
            },
            Step::Chmod { node: 6, path: 5 },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 22,
            },
            Step::Rename {
                node: 5,
                from: 2,
                to: 5,
            },
            Step::Modify {
                node: 7,
                path: 2,
                content: 3,
            },
            Step::Tier {
                a: 6,
                b: 3,
                tier: 1,
            },
            Step::Create {
                node: 2,
                path: 3,
                content: 2,
            },
            Step::Heal { a: 3, b: 7 },
            Step::Tier {
                a: 7,
                b: 6,
                tier: 2,
            },
            Step::Delete { node: 1, path: 9 },
            Step::Rename {
                node: 2,
                from: 8,
                to: 11,
            },
            Step::Offline { node: 2 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(1), Some(2), Some(3), Some(5), Some(1), Some(4), None],
            },
            Step::Offline { node: 3 },
            Step::Delete { node: 5, path: 3 },
            Step::Online { node: 2 },
            Step::Crash {
                node: 5,
                gap_secs: 46,
            },
            Step::Create {
                node: 5,
                path: 5,
                content: 3,
            },
            Step::Delete { node: 5, path: 11 },
            Step::Touch { node: 0, path: 0 },
            Step::Partition { a: 0, b: 6 },
            Step::Modify {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Online { node: 7 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(4), Some(3), Some(5), None, Some(3), Some(1)],
            },
            Step::Online { node: 3 },
            Step::MassDelete {
                node: 3,
                fraction: 92,
            },
            Step::Chmod { node: 3, path: 0 },
            Step::Online { node: 1 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(3), None, None, Some(2), Some(4), None, None],
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 32,
            },
            Step::Delete { node: 2, path: 1 },
            Step::Heal { a: 7, b: 1 },
            Step::Delete { node: 2, path: 7 },
            Step::Settle { secs: 16 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(1), Some(1), Some(2)],
            },
            Step::Heal { a: 1, b: 7 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 56,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Rules {
                    hold_count: 5,
                    hold_pct: 32,
                },
                delay_secs: 48,
            },
            Step::Crash {
                node: 1,
                gap_secs: 61,
            },
            Step::Settle { secs: 35 },
            Step::Heal { a: 2, b: 7 },
            Step::Symlink {
                node: 4,
                path: 9,
                target: 10,
            },
            Step::Partition { a: 5, b: 7 },
            Step::Heal { a: 0, b: 5 },
            Step::Delete { node: 6, path: 2 },
            Step::Partition { a: 5, b: 5 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(4), Some(5)],
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 29,
            },
            Step::Create {
                node: 3,
                path: 9,
                content: 5,
            },
            Step::Modify {
                node: 0,
                path: 8,
                content: 2,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(5), Some(3), Some(4), None, Some(4)],
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 2,
            },
            Step::Partition { a: 4, b: 2 },
            Step::Everywhere {
                path: 7,
                contents: vec![None, Some(5), None],
            },
            Step::Modify {
                node: 0,
                path: 2,
                content: 5,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 16,
            },
            Step::Touch { node: 1, path: 4 },
            Step::Modify {
                node: 7,
                path: 9,
                content: 1,
            },
            Step::Partition { a: 4, b: 2 },
            Step::Delete { node: 2, path: 3 },
            Step::Heal { a: 0, b: 5 },
            Step::Create {
                node: 7,
                path: 6,
                content: 4,
            },
            Step::Modify {
                node: 3,
                path: 3,
                content: 2,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![None, Some(2), Some(2), Some(1), Some(5)],
            },
            Step::Heal { a: 5, b: 0 },
            Step::Symlink {
                node: 4,
                path: 3,
                target: 3,
            },
            Step::Create {
                node: 6,
                path: 2,
                content: 4,
            },
            Step::MassModify {
                node: 5,
                fraction: 91,
                content: 3,
            },
            Step::Modify {
                node: 3,
                path: 2,
                content: 2,
            },
            Step::Create {
                node: 0,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 3,
                path: 3,
                content: 2,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(2), Some(3), None],
            },
            Step::Partition { a: 2, b: 0 },
            Step::Mkdir { node: 2, dir: 2 },
            Step::Modify {
                node: 6,
                path: 3,
                content: 1,
            },
            Step::Symlink {
                node: 4,
                path: 2,
                target: 10,
            },
            Step::Delete { node: 0, path: 5 },
            Step::Delete { node: 3, path: 3 },
            Step::Settle { secs: 33 },
            Step::Mkdir { node: 5, dir: 0 },
            Step::Heal { a: 6, b: 4 },
            Step::Delete { node: 1, path: 2 },
            Step::Delete { node: 2, path: 10 },
            Step::Modify {
                node: 4,
                path: 5,
                content: 1,
            },
            Step::Modify {
                node: 0,
                path: 0,
                content: 3,
            },
            Step::Create {
                node: 1,
                path: 5,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 5,
                content: 4,
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Settle { secs: 31 },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(1), Some(1), Some(3), Some(5)],
            },
            Step::Create {
                node: 4,
                path: 1,
                content: 5,
            },
            Step::Delete { node: 2, path: 9 },
            Step::Delete { node: 7, path: 2 },
            Step::Rename {
                node: 6,
                from: 9,
                to: 8,
            },
            Step::Rmdir { node: 6, dir: 2 },
            Step::Modify {
                node: 6,
                path: 8,
                content: 1,
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Crash {
                node: 2,
                gap_secs: 42,
            },
            Step::Modify {
                node: 2,
                path: 5,
                content: 4,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::Rules {
                    hold_count: 5,
                    hold_pct: 14,
                },
                delay_secs: 36,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 25,
            },
            Step::Create {
                node: 0,
                path: 6,
                content: 5,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(2), None, None, None, Some(3), Some(3)],
            },
            Step::Modify {
                node: 4,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 5,
                path: 9,
                content: 3,
            },
            Step::Create {
                node: 7,
                path: 3,
                content: 3,
            },
            Step::Delete { node: 7, path: 4 },
            Step::Online { node: 1 },
            Step::Tier {
                a: 0,
                b: 4,
                tier: 0,
            },
            Step::Offline { node: 6 },
            Step::Mkdir { node: 1, dir: 0 },
            Step::Modify {
                node: 7,
                path: 0,
                content: 3,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 3,
            },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Create {
                node: 5,
                path: 0,
                content: 3,
            },
            Step::Create {
                node: 7,
                path: 1,
                content: 1,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 29,
                },
                delay_secs: 7,
            },
            Step::Settle { secs: 38 },
            Step::Settle { secs: 38 },
            Step::Modify {
                node: 2,
                path: 10,
                content: 4,
            },
            Step::Symlink {
                node: 5,
                path: 5,
                target: 0,
            },
            Step::Partition { a: 7, b: 2 },
            Step::Partition { a: 5, b: 2 },
            Step::Create {
                node: 7,
                path: 6,
                content: 3,
            },
            Step::Heal { a: 0, b: 1 },
            Step::Settle { secs: 6 },
            Step::Create {
                node: 7,
                path: 0,
                content: 2,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![None, Some(2), Some(5), Some(2)],
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(4), Some(4), Some(3), Some(3), None, None],
            },
            Step::Modify {
                node: 1,
                path: 7,
                content: 4,
            },
            Step::Create {
                node: 0,
                path: 3,
                content: 5,
            },
            Step::Heal { a: 7, b: 0 },
            Step::Partition { a: 5, b: 5 },
            Step::Partition { a: 2, b: 6 },
            Step::Online { node: 2 },
            Step::Touch { node: 6, path: 8 },
            Step::Rename {
                node: 4,
                from: 7,
                to: 0,
            },
            Step::Settle { secs: 30 },
            Step::Create {
                node: 0,
                path: 4,
                content: 5,
            },
            Step::Settle { secs: 16 },
            Step::Offline { node: 4 },
            Step::Heal { a: 3, b: 2 },
            Step::Online { node: 2 },
            Step::Settle { secs: 4 },
            Step::Chmod { node: 3, path: 10 },
            Step::Tier {
                a: 1,
                b: 1,
                tier: 2,
            },
            Step::Tier {
                a: 7,
                b: 6,
                tier: 0,
            },
            Step::Modify {
                node: 1,
                path: 2,
                content: 4,
            },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Settle { secs: 11 },
        ],
    );
}

/// A chmod whose watcher event was dropped was invisible to every later
/// scan: the fast path compared size and mtime, and chmod changes neither,
/// so the index disagreed with the disk about the exec bit forever. The
/// fast path compares the exec bit now (§7.3).
/// Replayed as found: group commit alone loses its scenario (PR 1b).
#[test]
fn a_chmod_the_watcher_missed_is_found_by_the_next_scan() {
    passes_as_found(
        83,
        &Knobs::default(),
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

/// Same class as above, found on a conflict copy and pinned as
/// `a_missed_chmod_of_a_conflict_copy_is_found_by_the_next_scan`. At the
/// current defaults its history reaches a missed chmod of an ordinary file
/// under an incoming deletion: with the fix disabled the scan never sees
/// the chmod, the commit guard, which does compare the exec bit, reports
/// the deletion's commit changed underneath, the tombstone waits for an
/// observation of the path that never comes (§7.5 step 6), and the run
/// fails quiescence. Re-pinned at seed 0 with the default knobs, shrunk to
/// 61 steps, after drafts 52 and 53's failed-report rules moved its history; the
/// tombstone waits at f1.
#[test]
fn a_missed_chmod_under_an_incoming_deletion_is_found_by_the_next_scan() {
    passes(
        0,
        &[
            Step::Create {
                node: 6,
                path: 0,
                content: 3,
            },
            Step::Tier {
                a: 2,
                b: 1,
                tier: 2,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![None, Some(5), Some(4), None, None],
            },
            Step::Chmod { node: 7, path: 5 },
            Step::Settle { secs: 12 },
            Step::Tier {
                a: 0,
                b: 0,
                tier: 2,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 24,
            },
            Step::Touch { node: 2, path: 8 },
            Step::Create {
                node: 1,
                path: 3,
                content: 3,
            },
            Step::Tier {
                a: 2,
                b: 6,
                tier: 1,
            },
            Step::Heal { a: 0, b: 5 },
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
            Step::Rename {
                node: 5,
                from: 6,
                to: 1,
            },
            Step::Delete { node: 0, path: 5 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 45,
            },
            Step::Delete { node: 7, path: 3 },
            Step::Crash {
                node: 4,
                gap_secs: 35,
            },
            Step::Heal { a: 0, b: 0 },
            Step::Online { node: 0 },
            Step::Partition { a: 4, b: 7 },
            Step::Rename {
                node: 5,
                from: 5,
                to: 5,
            },
            Step::Offline { node: 2 },
            Step::Rename {
                node: 2,
                from: 6,
                to: 8,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 41,
            },
            Step::Settle { secs: 19 },
            Step::Modify {
                node: 3,
                path: 3,
                content: 4,
            },
            Step::Settle { secs: 29 },
            Step::Modify {
                node: 4,
                path: 3,
                content: 5,
            },
            Step::Heal { a: 3, b: 3 },
            Step::Settle { secs: 11 },
            Step::Modify {
                node: 1,
                path: 3,
                content: 5,
            },
            Step::Chmod { node: 1, path: 11 },
            Step::Online { node: 4 },
            Step::Rmdir { node: 0, dir: 0 },
            Step::Touch { node: 6, path: 1 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(1), Some(4)],
            },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Everywhere {
                path: 6,
                contents: vec![None, Some(4)],
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 9,
                content: 1,
            },
            Step::Touch { node: 0, path: 5 },
            Step::Symlink {
                node: 4,
                path: 7,
                target: 0,
            },
            Step::Create {
                node: 6,
                path: 1,
                content: 2,
            },
            Step::Heal { a: 5, b: 2 },
            Step::Rename {
                node: 1,
                from: 7,
                to: 7,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 54,
            },
            Step::Delete { node: 7, path: 7 },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 39,
            },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Crash {
                node: 0,
                gap_secs: 63,
            },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Create {
                node: 4,
                path: 4,
                content: 5,
            },
            Step::MassDelete {
                node: 6,
                fraction: 91,
            },
            Step::Delete { node: 5, path: 3 },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Delete { node: 1, path: 7 },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 50,
            },
            Step::Create {
                node: 1,
                path: 10,
                content: 3,
            },
            Step::Offline { node: 7 },
            Step::Chmod { node: 3, path: 1 },
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

/// Concurrent versions of one path merged pairwise in different orders
/// gave two nodes the same vector with different content (a file on one, a
/// symlink on the other): a symlink that replaced a file ranked below the
/// file it replaced, and one node met the newer increment alone. The stamp
/// is strictly increasing along every chain and is the first key of the
/// winner rule (§7.1, §7.6), so equal vectors mean equal content (I7).
/// Re-pinned at seed 90 with the default knobs, shrunk to 354 steps
/// keeping a symlink and a file, after drafts 52 and 53's failed-report rules moved seed
/// 81's history; with the stamp taken out of the winner rule it fails I7 (a
/// symlink and a file under one vector at f1).
#[test]
fn equal_vectors_mean_equal_content_when_a_symlink_replaces_a_file() {
    passes(
        90,
        &[
            Step::Chmod { node: 7, path: 8 },
            Step::Crash {
                node: 6,
                gap_secs: 112,
            },
            Step::Create {
                node: 4,
                path: 6,
                content: 2,
            },
            Step::Create {
                node: 7,
                path: 7,
                content: 5,
            },
            Step::Modify {
                node: 1,
                path: 4,
                content: 2,
            },
            Step::Settle { secs: 28 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(5), Some(2), Some(2), Some(2), Some(4)],
            },
            Step::Create {
                node: 5,
                path: 4,
                content: 1,
            },
            Step::Heal { a: 7, b: 0 },
            Step::Online { node: 5 },
            Step::Heal { a: 7, b: 6 },
            Step::Delete { node: 1, path: 4 },
            Step::Crash {
                node: 3,
                gap_secs: 62,
            },
            Step::Partition { a: 2, b: 6 },
            Step::Settle { secs: 8 },
            Step::Rename {
                node: 4,
                from: 6,
                to: 11,
            },
            Step::Symlink {
                node: 3,
                path: 6,
                target: 0,
            },
            Step::Rename {
                node: 2,
                from: 4,
                to: 8,
            },
            Step::Modify {
                node: 4,
                path: 1,
                content: 4,
            },
            Step::Touch { node: 1, path: 3 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(4), Some(2), Some(5)],
            },
            Step::Settle { secs: 16 },
            Step::Heal { a: 6, b: 7 },
            Step::Rmdir { node: 3, dir: 2 },
            Step::Rename {
                node: 0,
                from: 8,
                to: 5,
            },
            Step::Partition { a: 6, b: 2 },
            Step::Modify {
                node: 2,
                path: 3,
                content: 4,
            },
            Step::Offline { node: 4 },
            Step::Offline { node: 2 },
            Step::Create {
                node: 3,
                path: 11,
                content: 4,
            },
            Step::Rmdir { node: 4, dir: 0 },
            Step::Offline { node: 6 },
            Step::MassDelete {
                node: 5,
                fraction: 69,
            },
            Step::Delete { node: 3, path: 8 },
            Step::MassModify {
                node: 7,
                fraction: 55,
                content: 5,
            },
            Step::Rename {
                node: 4,
                from: 6,
                to: 11,
            },
            Step::Chmod { node: 6, path: 8 },
            Step::Rename {
                node: 3,
                from: 10,
                to: 4,
            },
            Step::Delete { node: 0, path: 5 },
            Step::Crash {
                node: 6,
                gap_secs: 51,
            },
            Step::Settle { secs: 29 },
            Step::Symlink {
                node: 4,
                path: 5,
                target: 5,
            },
            Step::Delete { node: 1, path: 1 },
            Step::Settle { secs: 4 },
            Step::Modify {
                node: 5,
                path: 2,
                content: 1,
            },
            Step::Delete { node: 7, path: 7 },
            Step::Modify {
                node: 0,
                path: 1,
                content: 5,
            },
            Step::Settle { secs: 27 },
            Step::Online { node: 5 },
            Step::Settle { secs: 26 },
            Step::Create {
                node: 0,
                path: 9,
                content: 1,
            },
            Step::Offline { node: 2 },
            Step::Settle { secs: 17 },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Create {
                node: 2,
                path: 6,
                content: 3,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 4,
            },
            Step::Modify {
                node: 1,
                path: 5,
                content: 5,
            },
            Step::Delete { node: 6, path: 7 },
            Step::Touch { node: 0, path: 0 },
            Step::Tier {
                a: 5,
                b: 1,
                tier: 1,
            },
            Step::Symlink {
                node: 2,
                path: 7,
                target: 0,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(5), None, Some(4), Some(5), Some(5), Some(4), Some(4)],
            },
            Step::Partition { a: 0, b: 0 },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Delete { node: 6, path: 4 },
            Step::Delete { node: 4, path: 5 },
            Step::Mkdir { node: 4, dir: 1 },
            Step::Modify {
                node: 7,
                path: 9,
                content: 4,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(3), Some(4), Some(5), Some(5), None],
            },
            Step::Modify {
                node: 1,
                path: 5,
                content: 5,
            },
            Step::Modify {
                node: 4,
                path: 6,
                content: 4,
            },
            Step::Online { node: 5 },
            Step::Delete { node: 7, path: 3 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 2,
            },
            Step::Modify {
                node: 6,
                path: 9,
                content: 4,
            },
            Step::Modify {
                node: 4,
                path: 0,
                content: 3,
            },
            Step::Mkdir { node: 1, dir: 0 },
            Step::Modify {
                node: 1,
                path: 0,
                content: 3,
            },
            Step::Settle { secs: 33 },
            Step::Modify {
                node: 2,
                path: 10,
                content: 1,
            },
            Step::Rename {
                node: 0,
                from: 0,
                to: 0,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(5), None],
            },
            Step::Create {
                node: 3,
                path: 4,
                content: 5,
            },
            Step::Create {
                node: 3,
                path: 6,
                content: 4,
            },
            Step::Rename {
                node: 2,
                from: 8,
                to: 6,
            },
            Step::Create {
                node: 5,
                path: 3,
                content: 3,
            },
            Step::Delete { node: 7, path: 10 },
            Step::Settle { secs: 25 },
            Step::Create {
                node: 7,
                path: 8,
                content: 3,
            },
            Step::Offline { node: 5 },
            Step::Rmdir { node: 0, dir: 1 },
            Step::Create {
                node: 2,
                path: 9,
                content: 4,
            },
            Step::MassDelete {
                node: 3,
                fraction: 71,
            },
            Step::Modify {
                node: 6,
                path: 6,
                content: 1,
            },
            Step::Heal { a: 2, b: 2 },
            Step::Online { node: 5 },
            Step::Offline { node: 7 },
            Step::Modify {
                node: 3,
                path: 1,
                content: 2,
            },
            Step::Create {
                node: 6,
                path: 9,
                content: 1,
            },
            Step::Rmdir { node: 7, dir: 2 },
            Step::Create {
                node: 7,
                path: 10,
                content: 5,
            },
            Step::Rename {
                node: 7,
                from: 0,
                to: 6,
            },
            Step::Modify {
                node: 7,
                path: 8,
                content: 3,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 11,
                },
                delay_secs: 2,
            },
            Step::Partition { a: 1, b: 5 },
            Step::MassDelete {
                node: 2,
                fraction: 90,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 13,
            },
            Step::Modify {
                node: 0,
                path: 2,
                content: 4,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 42,
            },
            Step::Create {
                node: 1,
                path: 10,
                content: 5,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 36,
            },
            Step::Online { node: 3 },
            Step::Settle { secs: 3 },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 49,
            },
            Step::Create {
                node: 2,
                path: 3,
                content: 1,
            },
            Step::Create {
                node: 2,
                path: 6,
                content: 3,
            },
            Step::Rmdir { node: 0, dir: 1 },
            Step::Everywhere {
                path: 7,
                contents: vec![
                    Some(4),
                    Some(1),
                    Some(1),
                    Some(5),
                    Some(3),
                    Some(1),
                    Some(2),
                ],
            },
            Step::Crash {
                node: 4,
                gap_secs: 23,
            },
            Step::Settle { secs: 36 },
            Step::Chmod { node: 5, path: 6 },
            Step::Create {
                node: 6,
                path: 2,
                content: 5,
            },
            Step::Create {
                node: 6,
                path: 3,
                content: 4,
            },
            Step::Heal { a: 3, b: 2 },
            Step::Everywhere {
                path: 11,
                contents: vec![None, None, Some(3), Some(3), Some(5), Some(4)],
            },
            Step::Settle { secs: 30 },
            Step::Rename {
                node: 3,
                from: 9,
                to: 1,
            },
            Step::Delete { node: 1, path: 7 },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(1), None, Some(2)],
            },
            Step::Modify {
                node: 7,
                path: 6,
                content: 1,
            },
            Step::Heal { a: 3, b: 1 },
            Step::Create {
                node: 7,
                path: 8,
                content: 1,
            },
            Step::Heal { a: 2, b: 4 },
            Step::Settle { secs: 25 },
            Step::Delete { node: 6, path: 11 },
            Step::Rename {
                node: 5,
                from: 11,
                to: 0,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 5,
            },
            Step::Settle { secs: 6 },
            Step::Chmod { node: 5, path: 7 },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 47,
            },
            Step::Create {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Offline { node: 6 },
            Step::Settle { secs: 29 },
            Step::Create {
                node: 5,
                path: 6,
                content: 4,
            },
            Step::Online { node: 7 },
            Step::Rmdir { node: 7, dir: 1 },
            Step::Online { node: 0 },
            Step::User {
                node: 6,
                action: crate::UserAction::DenyAll,
                delay_secs: 19,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 48,
            },
            Step::Create {
                node: 5,
                path: 0,
                content: 4,
            },
            Step::Delete { node: 6, path: 4 },
            Step::Settle { secs: 21 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 22,
            },
            Step::Crash {
                node: 2,
                gap_secs: 103,
            },
            Step::MassModify {
                node: 0,
                fraction: 86,
                content: 2,
            },
            Step::Delete { node: 4, path: 11 },
            Step::Chmod { node: 5, path: 8 },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(4), Some(4), Some(1), Some(1), Some(5), None, Some(2)],
            },
            Step::Heal { a: 3, b: 2 },
            Step::Create {
                node: 0,
                path: 7,
                content: 3,
            },
            Step::Modify {
                node: 6,
                path: 1,
                content: 3,
            },
            Step::Settle { secs: 10 },
            Step::Modify {
                node: 7,
                path: 10,
                content: 3,
            },
            Step::Delete { node: 7, path: 9 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(3), Some(5), Some(2)],
            },
            Step::Heal { a: 2, b: 1 },
            Step::Crash {
                node: 3,
                gap_secs: 26,
            },
            Step::Settle { secs: 7 },
            Step::Rename {
                node: 1,
                from: 6,
                to: 1,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(2), Some(2), Some(4), Some(3), Some(2), Some(3)],
            },
            Step::Delete { node: 3, path: 7 },
            Step::Online { node: 7 },
            Step::Modify {
                node: 5,
                path: 7,
                content: 5,
            },
            Step::Create {
                node: 4,
                path: 10,
                content: 3,
            },
            Step::Delete { node: 4, path: 8 },
            Step::Crash {
                node: 5,
                gap_secs: 3,
            },
            Step::Settle { secs: 2 },
            Step::Delete { node: 1, path: 11 },
            Step::Modify {
                node: 4,
                path: 7,
                content: 1,
            },
            Step::Create {
                node: 5,
                path: 4,
                content: 5,
            },
            Step::Rename {
                node: 6,
                from: 10,
                to: 6,
            },
            Step::Heal { a: 6, b: 7 },
            Step::Online { node: 6 },
            Step::Online { node: 3 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 57,
            },
            Step::Settle { secs: 9 },
            Step::Create {
                node: 5,
                path: 2,
                content: 4,
            },
            Step::Create {
                node: 3,
                path: 9,
                content: 2,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(5), None, Some(2)],
            },
            Step::Heal { a: 2, b: 5 },
            Step::Rename {
                node: 4,
                from: 1,
                to: 7,
            },
            Step::Create {
                node: 0,
                path: 9,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 2,
                content: 5,
            },
            Step::Create {
                node: 3,
                path: 0,
                content: 4,
            },
            Step::Create {
                node: 7,
                path: 9,
                content: 4,
            },
            Step::Delete { node: 0, path: 5 },
            Step::Rmdir { node: 2, dir: 2 },
            Step::Crash {
                node: 6,
                gap_secs: 117,
            },
            Step::Create {
                node: 5,
                path: 1,
                content: 4,
            },
            Step::Online { node: 5 },
            Step::Mkdir { node: 6, dir: 1 },
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
            Step::Create {
                node: 4,
                path: 6,
                content: 4,
            },
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
            Step::Create {
                node: 2,
                path: 10,
                content: 4,
            },
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
            Step::Create {
                node: 4,
                path: 11,
                content: 2,
            },
            Step::Chmod { node: 6, path: 7 },
            Step::Crash {
                node: 7,
                gap_secs: 75,
            },
            Step::Heal { a: 2, b: 6 },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Modify {
                node: 0,
                path: 9,
                content: 1,
            },
            Step::Modify {
                node: 3,
                path: 0,
                content: 4,
            },
            Step::Modify {
                node: 6,
                path: 3,
                content: 5,
            },
            Step::Chmod { node: 6, path: 7 },
            Step::Mkdir { node: 1, dir: 0 },
            Step::Create {
                node: 4,
                path: 6,
                content: 5,
            },
            Step::Settle { secs: 31 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 32,
            },
            Step::Create {
                node: 5,
                path: 5,
                content: 3,
            },
            Step::Touch { node: 2, path: 7 },
            Step::Delete { node: 6, path: 6 },
            Step::Settle { secs: 17 },
            Step::Chmod { node: 1, path: 0 },
            Step::Create {
                node: 5,
                path: 10,
                content: 2,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(3), None, None],
            },
            Step::Create {
                node: 2,
                path: 8,
                content: 2,
            },
            Step::Settle { secs: 6 },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Create {
                node: 1,
                path: 1,
                content: 2,
            },
            Step::Modify {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Settle { secs: 20 },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 9,
            },
            Step::Tier {
                a: 2,
                b: 3,
                tier: 2,
            },
            Step::Chmod { node: 4, path: 1 },
            Step::Modify {
                node: 2,
                path: 8,
                content: 4,
            },
            Step::Delete { node: 3, path: 0 },
            Step::Offline { node: 3 },
            Step::Create {
                node: 7,
                path: 1,
                content: 4,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Rules {
                    hold_count: 1,
                    hold_pct: 15,
                },
                delay_secs: 27,
            },
            Step::Create {
                node: 5,
                path: 6,
                content: 3,
            },
            Step::Touch { node: 2, path: 0 },
            Step::Partition { a: 6, b: 2 },
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
            Step::Offline { node: 6 },
            Step::Online { node: 2 },
            Step::Delete { node: 1, path: 7 },
            Step::Settle { secs: 17 },
            Step::Heal { a: 5, b: 0 },
            Step::Create {
                node: 7,
                path: 4,
                content: 2,
            },
            Step::Touch { node: 5, path: 3 },
            Step::Touch { node: 5, path: 9 },
            Step::Heal { a: 2, b: 3 },
            Step::MassModify {
                node: 7,
                fraction: 58,
                content: 3,
            },
            Step::Modify {
                node: 3,
                path: 9,
                content: 4,
            },
            Step::Delete { node: 1, path: 11 },
            Step::MassDelete {
                node: 1,
                fraction: 71,
            },
            Step::Settle { secs: 14 },
            Step::Tier {
                a: 7,
                b: 1,
                tier: 2,
            },
            Step::Mkdir { node: 6, dir: 2 },
            Step::Create {
                node: 0,
                path: 0,
                content: 2,
            },
            Step::Partition { a: 7, b: 4 },
            Step::Modify {
                node: 1,
                path: 3,
                content: 3,
            },
            Step::Heal { a: 0, b: 2 },
            Step::Partition { a: 7, b: 6 },
            Step::Chmod { node: 2, path: 7 },
            Step::Create {
                node: 6,
                path: 11,
                content: 2,
            },
            Step::Crash {
                node: 2,
                gap_secs: 21,
            },
            Step::MassModify {
                node: 4,
                fraction: 85,
                content: 2,
            },
            Step::Partition { a: 2, b: 0 },
            Step::Heal { a: 6, b: 7 },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 36,
            },
            Step::Settle { secs: 23 },
            Step::Chmod { node: 0, path: 10 },
            Step::Modify {
                node: 3,
                path: 6,
                content: 2,
            },
            Step::Settle { secs: 28 },
            Step::MassDelete {
                node: 1,
                fraction: 92,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(5), Some(5), Some(3)],
            },
            Step::Create {
                node: 2,
                path: 6,
                content: 3,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 4,
            },
            Step::Tier {
                a: 6,
                b: 5,
                tier: 2,
            },
            Step::Create {
                node: 1,
                path: 9,
                content: 4,
            },
            Step::Settle { secs: 3 },
            Step::Chmod { node: 6, path: 10 },
            Step::Settle { secs: 3 },
            Step::Create {
                node: 7,
                path: 11,
                content: 1,
            },
            Step::Modify {
                node: 5,
                path: 3,
                content: 4,
            },
            Step::Rename {
                node: 5,
                from: 8,
                to: 0,
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
            Step::Partition { a: 4, b: 0 },
            Step::Offline { node: 5 },
            Step::Touch { node: 0, path: 8 },
            Step::Everywhere {
                path: 7,
                contents: vec![None, Some(1), Some(4), Some(1), Some(4), Some(3)],
            },
            Step::Create {
                node: 1,
                path: 10,
                content: 4,
            },
            Step::Modify {
                node: 2,
                path: 5,
                content: 2,
            },
            Step::Heal { a: 1, b: 1 },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Create {
                node: 0,
                path: 7,
                content: 5,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 52,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::DenyAll,
                delay_secs: 9,
            },
            Step::Tier {
                a: 4,
                b: 3,
                tier: 1,
            },
            Step::Chmod { node: 6, path: 9 },
            Step::Crash {
                node: 4,
                gap_secs: 38,
            },
            Step::MassDelete {
                node: 7,
                fraction: 71,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 24,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 23,
            },
            Step::Crash {
                node: 7,
                gap_secs: 99,
            },
            Step::Offline { node: 2 },
            Step::Create {
                node: 4,
                path: 5,
                content: 1,
            },
            Step::Online { node: 7 },
            Step::Rename {
                node: 6,
                from: 3,
                to: 3,
            },
            Step::Partition { a: 3, b: 4 },
            Step::Delete { node: 0, path: 2 },
            Step::Heal { a: 0, b: 7 },
            Step::Online { node: 2 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 4,
            },
            Step::Create {
                node: 4,
                path: 10,
                content: 2,
            },
            Step::Offline { node: 6 },
            Step::Modify {
                node: 1,
                path: 7,
                content: 2,
            },
            Step::Partition { a: 3, b: 6 },
            Step::Rmdir { node: 5, dir: 2 },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Modify {
                node: 1,
                path: 7,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 2,
                content: 1,
            },
            Step::Partition { a: 3, b: 7 },
            Step::Crash {
                node: 7,
                gap_secs: 55,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 32,
            },
            Step::Modify {
                node: 4,
                path: 6,
                content: 5,
            },
            Step::Delete { node: 2, path: 6 },
            Step::Modify {
                node: 4,
                path: 4,
                content: 1,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::DenyAll,
                delay_secs: 11,
            },
            Step::Modify {
                node: 6,
                path: 8,
                content: 4,
            },
            Step::Create {
                node: 5,
                path: 6,
                content: 3,
            },
            Step::Online { node: 5 },
            Step::MassModify {
                node: 2,
                fraction: 72,
                content: 2,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 16,
            },
            Step::Settle { secs: 8 },
            Step::MassModify {
                node: 4,
                fraction: 71,
                content: 2,
            },
            Step::Create {
                node: 0,
                path: 1,
                content: 1,
            },
            Step::Rename {
                node: 6,
                from: 0,
                to: 4,
            },
            Step::Heal { a: 3, b: 5 },
        ],
    );
}

/// Same class, with deletes: tombstones and a live edit of one path merged
/// in different orders on different nodes gave one vector, one deleted and
/// one live, each dropping the other as Equal forever. Re-pinned at seed 6
/// with the default knobs, shrunk to 67 steps keeping the deletion, after
/// draft 54's refusal rules moved seed 4's history; with the stamp taken out of
/// the winner rule it fails I7 (a live file and a tombstone under one
/// vector at d1/f4).
#[test]
fn equal_vectors_mean_equal_content_when_deletes_race_an_edit() {
    passes(
        6,
        &[
            Step::User {
                node: 2,
                action: crate::UserAction::DenyAll,
                delay_secs: 24,
            },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(5), Some(3), Some(4), Some(1)],
            },
            Step::Delete { node: 4, path: 7 },
            Step::Delete { node: 6, path: 2 },
            Step::Settle { secs: 22 },
            Step::Offline { node: 3 },
            Step::Chmod { node: 0, path: 11 },
            Step::Modify {
                node: 2,
                path: 9,
                content: 5,
            },
            Step::Delete { node: 2, path: 5 },
            Step::Create {
                node: 3,
                path: 9,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 11,
                content: 3,
            },
            Step::Rename {
                node: 3,
                from: 0,
                to: 0,
            },
            Step::Modify {
                node: 5,
                path: 0,
                content: 2,
            },
            Step::Modify {
                node: 1,
                path: 9,
                content: 3,
            },
            Step::Heal { a: 4, b: 3 },
            Step::Rename {
                node: 4,
                from: 4,
                to: 0,
            },
            Step::Partition { a: 6, b: 3 },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(3), None, Some(1), Some(2), None, Some(5), Some(1)],
            },
            Step::Create {
                node: 7,
                path: 4,
                content: 4,
            },
            Step::Settle { secs: 34 },
            Step::Online { node: 6 },
            Step::Rename {
                node: 7,
                from: 6,
                to: 4,
            },
            Step::Create {
                node: 5,
                path: 10,
                content: 3,
            },
            Step::Offline { node: 3 },
            Step::User {
                node: 0,
                action: crate::UserAction::Revert,
                delay_secs: 32,
            },
            Step::Crash {
                node: 3,
                gap_secs: 98,
            },
            Step::Offline { node: 4 },
            Step::Online { node: 4 },
            Step::Crash {
                node: 4,
                gap_secs: 18,
            },
            Step::Delete { node: 1, path: 6 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(1), Some(1), Some(1), Some(5), Some(1)],
            },
            Step::Heal { a: 3, b: 6 },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 43,
            },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Rmdir { node: 3, dir: 2 },
            Step::Online { node: 5 },
            Step::Partition { a: 4, b: 2 },
            Step::Modify {
                node: 6,
                path: 4,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 5,
                content: 3,
            },
            Step::Delete { node: 7, path: 5 },
            Step::Touch { node: 0, path: 5 },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(1), Some(1), None],
            },
            Step::Create {
                node: 1,
                path: 2,
                content: 2,
            },
            Step::MassModify {
                node: 6,
                fraction: 89,
                content: 2,
            },
            Step::Online { node: 3 },
            Step::Delete { node: 4, path: 8 },
            Step::Crash {
                node: 4,
                gap_secs: 4,
            },
            Step::Rename {
                node: 3,
                from: 3,
                to: 3,
            },
            Step::Create {
                node: 1,
                path: 4,
                content: 3,
            },
            Step::Rmdir { node: 4, dir: 1 },
            Step::Modify {
                node: 2,
                path: 1,
                content: 5,
            },
            Step::Heal { a: 2, b: 4 },
            Step::Modify {
                node: 0,
                path: 3,
                content: 3,
            },
            Step::MassModify {
                node: 1,
                fraction: 65,
                content: 1,
            },
            Step::Delete { node: 0, path: 8 },
            Step::Tier {
                a: 3,
                b: 3,
                tier: 0,
            },
            Step::Tier {
                a: 1,
                b: 4,
                tier: 2,
            },
            Step::Modify {
                node: 0,
                path: 8,
                content: 3,
            },
            Step::Delete { node: 5, path: 9 },
            Step::Crash {
                node: 1,
                gap_secs: 9,
            },
            Step::Delete { node: 3, path: 3 },
            Step::Rmdir { node: 0, dir: 0 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(5), Some(1)],
            },
            Step::MassDelete {
                node: 3,
                fraction: 94,
            },
            Step::Partition { a: 4, b: 5 },
            Step::Delete { node: 1, path: 1 },
            Step::Modify {
                node: 3,
                path: 8,
                content: 1,
            },
        ],
    );
}

/// A `deny`'s bump stamped one past the local record, or 1 with no local
/// record, and so could rank below a quarantined version it dominated; a
/// node that met the bump alone ranked its own edit above it while a node
/// holding the dominated version ranked that above everything, and the same
/// vector carried two contents (I7). A deny's bump now stamps one past
/// every version it dominates. Pinned at seed 164 with the default knobs,
/// shrunk to 28 steps, after the hold on adoptions at a released commit's
/// path moved seed 521's history (22 of seeds 0-999 fail with the fix
/// disabled); it fails I7, two contents under one vector at f1.
#[test]
fn a_deny_ranks_above_every_version_it_dominates() {
    passes(
        164,
        &[
            Step::Online { node: 5 },
            Step::Rmdir { node: 4, dir: 2 },
            Step::Settle { secs: 32 },
            Step::Modify {
                node: 0,
                path: 2,
                content: 3,
            },
            Step::Create {
                node: 7,
                path: 11,
                content: 5,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![None, Some(4), Some(3), Some(5)],
            },
            Step::Touch { node: 7, path: 2 },
            Step::Partition { a: 6, b: 4 },
            Step::Settle { secs: 14 },
            Step::Crash {
                node: 5,
                gap_secs: 75,
            },
            Step::Modify {
                node: 2,
                path: 0,
                content: 4,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(1), Some(3), Some(3)],
            },
            Step::Mkdir { node: 0, dir: 0 },
            Step::MassModify {
                node: 1,
                fraction: 66,
                content: 1,
            },
            Step::MassDelete {
                node: 6,
                fraction: 74,
            },
            Step::Symlink {
                node: 0,
                path: 6,
                target: 5,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 53,
            },
            Step::MassModify {
                node: 2,
                fraction: 85,
                content: 3,
            },
            Step::Create {
                node: 3,
                path: 10,
                content: 3,
            },
            Step::Mkdir { node: 4, dir: 1 },
            Step::Partition { a: 5, b: 3 },
            Step::Online { node: 2 },
            Step::Offline { node: 4 },
            Step::Create {
                node: 4,
                path: 0,
                content: 4,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 29,
            },
            Step::Partition { a: 7, b: 7 },
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

/// Two holders of the same losing version each displaced it to the same
/// conflict-copy path. One wanted the other's copy and was fetching it when
/// its own commit wrote its copy record at that path; the fetch then landed
/// over that record with a version that did not dominate it (I8). Every
/// index write now re-classifies the wants at its path, so the want meets
/// the node's own copy as an identical-content merge. Re-pinned at seed 0
/// with the default knobs, shrunk to 57 steps, after drafts 52 and 53's failed-report rules
/// moved seed 4's history; without the re-classification after the copy
/// record it fails I8 (a commit at a conflict copy of d1/f4 reported over
/// the node's own record).
#[test]
fn a_want_for_a_peers_conflict_copy_meets_our_own_copy_as_a_merge() {
    passes(
        0,
        &[
            Step::Offline { node: 2 },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(3), None],
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 2,
            },
            Step::Chmod { node: 5, path: 9 },
            Step::Modify {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Modify {
                node: 0,
                path: 2,
                content: 2,
            },
            Step::Tier {
                a: 6,
                b: 5,
                tier: 1,
            },
            Step::Create {
                node: 0,
                path: 10,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 5,
                content: 5,
            },
            Step::Chmod { node: 0, path: 0 },
            Step::Create {
                node: 5,
                path: 9,
                content: 4,
            },
            Step::Crash {
                node: 3,
                gap_secs: 61,
            },
            Step::Mkdir { node: 3, dir: 0 },
            Step::Create {
                node: 5,
                path: 0,
                content: 2,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Rules {
                    hold_count: 4,
                    hold_pct: 53,
                },
                delay_secs: 12,
            },
            Step::Delete { node: 6, path: 5 },
            Step::Offline { node: 3 },
            Step::Crash {
                node: 0,
                gap_secs: 2,
            },
            Step::Partition { a: 2, b: 5 },
            Step::Online { node: 2 },
            Step::Touch { node: 6, path: 3 },
            Step::Crash {
                node: 3,
                gap_secs: 99,
            },
            Step::Touch { node: 6, path: 1 },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Everywhere {
                path: 6,
                contents: vec![None, Some(4)],
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 9,
                content: 1,
            },
            Step::Touch { node: 0, path: 5 },
            Step::Symlink {
                node: 4,
                path: 7,
                target: 0,
            },
            Step::Partition { a: 2, b: 4 },
            Step::Delete { node: 6, path: 2 },
            Step::Create {
                node: 6,
                path: 1,
                content: 2,
            },
            Step::Heal { a: 5, b: 2 },
            Step::Heal { a: 4, b: 4 },
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
            Step::Delete { node: 7, path: 7 },
            Step::Create {
                node: 0,
                path: 7,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 4,
                content: 4,
            },
            Step::Rmdir { node: 1, dir: 2 },
            Step::Modify {
                node: 5,
                path: 4,
                content: 5,
            },
            Step::Create {
                node: 2,
                path: 7,
                content: 5,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![None, Some(2), Some(2)],
            },
            Step::Chmod { node: 3, path: 1 },
            Step::Rmdir { node: 7, dir: 2 },
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
            Step::Settle { secs: 38 },
            Step::Modify {
                node: 3,
                path: 3,
                content: 3,
            },
            Step::Symlink {
                node: 5,
                path: 10,
                target: 5,
            },
            Step::Delete { node: 2, path: 3 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 58,
            },
            Step::Modify {
                node: 5,
                path: 0,
                content: 2,
            },
            Step::Partition { a: 5, b: 1 },
            Step::Modify {
                node: 2,
                path: 5,
                content: 1,
            },
            Step::Delete { node: 5, path: 7 },
        ],
    );
}

/// A node denied a batch, then reverted: its deny bump was discarded before
/// anyone saw it. Another node later reached the same vector by a merge
/// with the node's re-issued counter, and I7 compared it with the discarded
/// record. Records a revert discarded are no evidence of anything.
/// Re-pinned at seed 231 again, shrunk to 118 steps, after draft 54's refusal rules
/// moved seed 1018's history. With the fix disabled it fails I7 (version
/// identity) at a conflict copy of d2/f8: the same vector seen with two
/// contents.
#[test]
fn a_vector_a_revert_discarded_may_be_reached_again_by_a_merge() {
    passes(
        231,
        &[
            Step::Touch { node: 6, path: 1 },
            Step::Partition { a: 7, b: 0 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(3), Some(2), None, None, None, Some(5), Some(4)],
            },
            Step::MassModify {
                node: 6,
                fraction: 55,
                content: 5,
            },
            Step::Offline { node: 3 },
            Step::Crash {
                node: 0,
                gap_secs: 76,
            },
            Step::Delete { node: 7, path: 2 },
            Step::Partition { a: 7, b: 0 },
            Step::Create {
                node: 5,
                path: 7,
                content: 2,
            },
            Step::Modify {
                node: 4,
                path: 7,
                content: 2,
            },
            Step::Touch { node: 0, path: 11 },
            Step::Delete { node: 6, path: 3 },
            Step::Rename {
                node: 0,
                from: 3,
                to: 2,
            },
            Step::MassDelete {
                node: 1,
                fraction: 87,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(1), Some(3), Some(2), Some(1), None],
            },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Partition { a: 2, b: 7 },
            Step::Create {
                node: 0,
                path: 3,
                content: 4,
            },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Offline { node: 4 },
            Step::Modify {
                node: 0,
                path: 8,
                content: 1,
            },
            Step::Modify {
                node: 1,
                path: 7,
                content: 5,
            },
            Step::Partition { a: 1, b: 5 },
            Step::MassDelete {
                node: 7,
                fraction: 66,
            },
            Step::Partition { a: 4, b: 6 },
            Step::Modify {
                node: 4,
                path: 1,
                content: 4,
            },
            Step::Offline { node: 5 },
            Step::Mkdir { node: 2, dir: 2 },
            Step::Heal { a: 6, b: 5 },
            Step::Rmdir { node: 0, dir: 1 },
            Step::Mkdir { node: 2, dir: 2 },
            Step::Create {
                node: 6,
                path: 4,
                content: 2,
            },
            Step::Modify {
                node: 7,
                path: 8,
                content: 3,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 52,
            },
            Step::Modify {
                node: 7,
                path: 2,
                content: 3,
            },
            Step::Offline { node: 1 },
            Step::Crash {
                node: 2,
                gap_secs: 108,
            },
            Step::Modify {
                node: 4,
                path: 9,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Partition { a: 3, b: 0 },
            Step::Heal { a: 0, b: 2 },
            Step::Crash {
                node: 2,
                gap_secs: 110,
            },
            Step::Partition { a: 5, b: 0 },
            Step::Online { node: 3 },
            Step::Chmod { node: 3, path: 7 },
            Step::Create {
                node: 4,
                path: 1,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 4,
            },
            Step::Touch { node: 7, path: 8 },
            Step::Delete { node: 3, path: 8 },
            Step::Online { node: 6 },
            Step::Crash {
                node: 4,
                gap_secs: 40,
            },
            Step::Tier {
                a: 2,
                b: 4,
                tier: 0,
            },
            Step::Create {
                node: 6,
                path: 0,
                content: 1,
            },
            Step::Rename {
                node: 1,
                from: 6,
                to: 8,
            },
            Step::Heal { a: 7, b: 6 },
            Step::Online { node: 6 },
            Step::Delete { node: 3, path: 10 },
            Step::Touch { node: 5, path: 7 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 55,
            },
            Step::Delete { node: 4, path: 3 },
            Step::Delete { node: 6, path: 7 },
            Step::Modify {
                node: 3,
                path: 10,
                content: 1,
            },
            Step::Tier {
                a: 4,
                b: 1,
                tier: 0,
            },
            Step::Symlink {
                node: 4,
                path: 7,
                target: 2,
            },
            Step::Crash {
                node: 0,
                gap_secs: 15,
            },
            Step::Symlink {
                node: 3,
                path: 8,
                target: 1,
            },
            Step::Create {
                node: 2,
                path: 1,
                content: 5,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 2,
            },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Tier {
                a: 2,
                b: 3,
                tier: 1,
            },
            Step::Settle { secs: 38 },
            Step::Heal { a: 0, b: 0 },
            Step::MassModify {
                node: 4,
                fraction: 84,
                content: 2,
            },
            Step::Online { node: 0 },
            Step::Modify {
                node: 7,
                path: 1,
                content: 2,
            },
            Step::Online { node: 0 },
            Step::Partition { a: 6, b: 1 },
            Step::Chmod { node: 6, path: 2 },
            Step::Heal { a: 5, b: 5 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 1,
            },
            Step::Rmdir { node: 2, dir: 0 },
            Step::Mkdir { node: 4, dir: 2 },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 34,
            },
            Step::Online { node: 5 },
            Step::Settle { secs: 30 },
            Step::Create {
                node: 2,
                path: 3,
                content: 4,
            },
            Step::Heal { a: 6, b: 5 },
            Step::Modify {
                node: 5,
                path: 4,
                content: 5,
            },
            Step::Settle { secs: 30 },
            Step::Rename {
                node: 7,
                from: 1,
                to: 8,
            },
            Step::Offline { node: 5 },
            Step::Create {
                node: 3,
                path: 0,
                content: 1,
            },
            Step::Modify {
                node: 1,
                path: 11,
                content: 5,
            },
            Step::Delete { node: 0, path: 0 },
            Step::Touch { node: 1, path: 9 },
            Step::Delete { node: 0, path: 2 },
            Step::Heal { a: 0, b: 3 },
            Step::Heal { a: 4, b: 2 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 27,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::DenyAll,
                delay_secs: 37,
            },
            Step::Settle { secs: 7 },
            Step::Symlink {
                node: 3,
                path: 8,
                target: 7,
            },
            Step::Heal { a: 2, b: 4 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(1), Some(4), Some(2), Some(5)],
            },
            Step::Delete { node: 6, path: 8 },
            Step::Modify {
                node: 1,
                path: 3,
                content: 1,
            },
            Step::Modify {
                node: 3,
                path: 10,
                content: 2,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 28,
            },
            Step::Delete { node: 0, path: 11 },
            Step::Create {
                node: 4,
                path: 4,
                content: 1,
            },
            Step::Create {
                node: 5,
                path: 2,
                content: 1,
            },
            Step::Delete { node: 0, path: 5 },
            Step::Modify {
                node: 5,
                path: 11,
                content: 3,
            },
            Step::Online { node: 5 },
            Step::Create {
                node: 1,
                path: 3,
                content: 1,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 46,
            },
            Step::Create {
                node: 3,
                path: 11,
                content: 2,
            },
            Step::Crash {
                node: 0,
                gap_secs: 96,
            },
        ],
    );
}

/// A node removed a directory, paused on the deletes, and reverted: the
/// tombstones were discarded unannounced, and I3 took one for a deletion the
/// mesh had agreed on. Re-pinned at seed 23 with the default knobs, shrunk
/// to 63 steps, after drafts 52 and 53's failed-report rules moved seed 176's history;
/// with the fix disabled it fails I3 at d0/f9.
#[test]
fn a_tombstone_a_revert_discarded_is_not_a_deletion() {
    passes(
        23,
        &[
            Step::Delete { node: 1, path: 1 },
            Step::Partition { a: 0, b: 0 },
            Step::Rename {
                node: 4,
                from: 7,
                to: 10,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 59,
            },
            Step::Heal { a: 6, b: 6 },
            Step::Crash {
                node: 2,
                gap_secs: 61,
            },
            Step::Heal { a: 3, b: 0 },
            Step::Modify {
                node: 7,
                path: 2,
                content: 4,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 22,
            },
            Step::Rename {
                node: 0,
                from: 3,
                to: 1,
            },
            Step::Rename {
                node: 1,
                from: 5,
                to: 0,
            },
            Step::Modify {
                node: 1,
                path: 11,
                content: 5,
            },
            Step::Create {
                node: 6,
                path: 6,
                content: 2,
            },
            Step::Heal { a: 4, b: 7 },
            Step::Heal { a: 3, b: 3 },
            Step::Modify {
                node: 3,
                path: 11,
                content: 1,
            },
            Step::Create {
                node: 7,
                path: 7,
                content: 3,
            },
            Step::Delete { node: 3, path: 3 },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(4), Some(4)],
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 27,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::Revert,
                delay_secs: 53,
            },
            Step::Create {
                node: 1,
                path: 1,
                content: 1,
            },
            Step::Modify {
                node: 0,
                path: 0,
                content: 4,
            },
            Step::Delete { node: 0, path: 11 },
            Step::Touch { node: 0, path: 10 },
            Step::Heal { a: 0, b: 1 },
            Step::Partition { a: 2, b: 1 },
            Step::Delete { node: 3, path: 3 },
            Step::Crash {
                node: 3,
                gap_secs: 79,
            },
            Step::Modify {
                node: 5,
                path: 6,
                content: 3,
            },
            Step::Touch { node: 7, path: 7 },
            Step::Partition { a: 7, b: 1 },
            Step::Settle { secs: 31 },
            Step::Heal { a: 0, b: 7 },
            Step::Rename {
                node: 6,
                from: 9,
                to: 7,
            },
            Step::Tier {
                a: 6,
                b: 6,
                tier: 2,
            },
            Step::Crash {
                node: 7,
                gap_secs: 91,
            },
            Step::Heal { a: 6, b: 6 },
            Step::Symlink {
                node: 2,
                path: 7,
                target: 4,
            },
            Step::Partition { a: 6, b: 7 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(5), Some(2), Some(5)],
            },
            Step::Create {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Modify {
                node: 6,
                path: 1,
                content: 2,
            },
            Step::MassModify {
                node: 5,
                fraction: 98,
                content: 4,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 36,
            },
            Step::Touch { node: 1, path: 6 },
            Step::User {
                node: 1,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 54,
                },
                delay_secs: 58,
            },
            Step::MassDelete {
                node: 6,
                fraction: 68,
            },
            Step::Mkdir { node: 5, dir: 0 },
            Step::Settle { secs: 27 },
            Step::Tier {
                a: 4,
                b: 3,
                tier: 2,
            },
            Step::Modify {
                node: 0,
                path: 3,
                content: 5,
            },
            Step::Online { node: 6 },
            Step::Partition { a: 2, b: 7 },
            Step::Touch { node: 2, path: 11 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 35,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 1,
            },
            Step::Modify {
                node: 7,
                path: 6,
                content: 3,
            },
            Step::Rmdir { node: 1, dir: 2 },
            Step::User {
                node: 1,
                action: crate::UserAction::Revert,
                delay_secs: 52,
            },
            Step::Create {
                node: 4,
                path: 9,
                content: 1,
            },
            Step::MassDelete {
                node: 7,
                fraction: 73,
            },
            Step::Settle { secs: 32 },
        ],
    );
}

/// A node's version of a path was discarded by its revert; its next change
/// re-issued the same vector with the same content but a later mtime. The
/// version table kept the discarded record (same content, no I7 clash), so
/// the conflict copy the node later made of the newer record was named
/// after an mtime the table did not know.
/// Re-pinned at seed 120 with the default knobs, shrunk to 27 steps, after
/// drafts 52 and 53's failed-report rules moved seed 202's history. With the fix disabled
/// it fails I4 (bounded conflicts): a conflict copy of d0/f6 matches no
/// losing version at its original path.
#[test]
fn a_reissued_vector_replaces_the_discarded_record_whatever_its_content() {
    passes(
        120,
        &[
            Step::Delete { node: 6, path: 0 },
            Step::Partition { a: 6, b: 6 },
            Step::Create {
                node: 7,
                path: 7,
                content: 3,
            },
            Step::Delete { node: 6, path: 2 },
            Step::Modify {
                node: 1,
                path: 9,
                content: 4,
            },
            Step::Modify {
                node: 3,
                path: 11,
                content: 3,
            },
            Step::Create {
                node: 6,
                path: 5,
                content: 4,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![None, Some(3), Some(5), Some(5), Some(2), None],
            },
            Step::Modify {
                node: 6,
                path: 5,
                content: 1,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 7,
            },
            Step::Partition { a: 7, b: 5 },
            Step::Rmdir { node: 6, dir: 1 },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 15,
            },
            Step::Settle { secs: 28 },
            Step::Chmod { node: 7, path: 9 },
            Step::Create {
                node: 3,
                path: 11,
                content: 1,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(1), Some(2), Some(2), Some(3)],
            },
            Step::Rmdir { node: 4, dir: 1 },
            Step::Modify {
                node: 1,
                path: 2,
                content: 3,
            },
            Step::Create {
                node: 6,
                path: 0,
                content: 2,
            },
            Step::MassModify {
                node: 0,
                fraction: 97,
                content: 2,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 40,
            },
            Step::Touch { node: 6, path: 3 },
            Step::MassModify {
                node: 1,
                fraction: 87,
                content: 3,
            },
            Step::Crash {
                node: 4,
                gap_secs: 16,
            },
            Step::Crash {
                node: 7,
                gap_secs: 60,
            },
            Step::MassModify {
                node: 0,
                fraction: 96,
                content: 2,
            },
        ],
    );
}

/// A symlink retargeted under a dropped watcher event was invisible to every
/// later scan: the fast path matched symlinks by kind alone. The source kept
/// announcing the old target's hash and served the new target's bytes, so
/// receivers mismatched twice and gave up. The fast path compares the
/// target now (§7.3). Pinned at seed 176 with the default knobs, shrunk to
/// 10 steps; with the fix disabled a peer's want for the retargeted link
/// ends as `NoSource` with its deadline still set, and the run fails
/// quiescence.
#[test]
fn a_retargeted_symlink_the_watcher_missed_is_found_by_the_next_scan() {
    passes(
        176,
        &[
            Step::Create {
                node: 2,
                path: 5,
                content: 1,
            },
            Step::Rmdir { node: 5, dir: 0 },
            Step::Create {
                node: 6,
                path: 1,
                content: 5,
            },
            Step::Settle { secs: 33 },
            Step::Chmod { node: 7, path: 9 },
            Step::Settle { secs: 4 },
            Step::Delete { node: 5, path: 6 },
            Step::Symlink {
                node: 3,
                path: 4,
                target: 5,
            },
            Step::Modify {
                node: 2,
                path: 9,
                content: 4,
            },
            Step::Symlink {
                node: 3,
                path: 4,
                target: 4,
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
/// went out, that peer never asked for them: a conflict copy with one
/// content on one node and another on the other.
/// Re-pinned at seed 33 again with the default knobs, shrunk to 33 steps,
/// after draft 54's refusal rules moved seed 20's history. With the fix disabled
/// it fails I1 (convergence): a conflict copy of d2/f11 is live on
/// 91ed4416 and absent on 6a47a2b9.
#[test]
fn catch_up_never_sends_what_this_machine_has_not_announced() {
    passes(
        33,
        &[
            Step::Create {
                node: 1,
                path: 2,
                content: 5,
            },
            Step::Chmod { node: 6, path: 7 },
            Step::Modify {
                node: 2,
                path: 5,
                content: 1,
            },
            Step::Heal { a: 7, b: 7 },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Modify {
                node: 4,
                path: 11,
                content: 5,
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
            Step::Tier {
                a: 4,
                b: 3,
                tier: 1,
            },
            Step::Offline { node: 4 },
            Step::Symlink {
                node: 2,
                path: 11,
                target: 2,
            },
            Step::Delete { node: 6, path: 10 },
            Step::Online { node: 4 },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Chmod { node: 5, path: 10 },
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
            Step::Create {
                node: 2,
                path: 0,
                content: 5,
            },
            Step::Create {
                node: 4,
                path: 1,
                content: 2,
            },
            Step::Rmdir { node: 3, dir: 1 },
            Step::Chmod { node: 1, path: 3 },
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
/// Re-pinned at seed 4664 with the default knobs, shrunk to 233 steps,
/// after draft 54's refusal rules moved seed 3744's history. With the fix disabled
/// it fails I3 (no resurrection): d2, deleted by f15aebc2's tombstone with
/// nothing concurrent or newer, is still live on 9382b34a.
/// The fix's unit test guards it whether or not a seed reaches it:
/// `sim::tests::a_revert_sets_aside_only_the_versions_above_the_record_it_restored`.
#[test]
fn a_revert_discards_only_the_versions_above_the_restored_record() {
    passes(
        4664,
        &[
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 45,
            },
            Step::Modify {
                node: 1,
                path: 7,
                content: 3,
            },
            Step::Online { node: 6 },
            Step::Heal { a: 0, b: 3 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(3), Some(5), Some(3), Some(5)],
            },
            Step::Heal { a: 6, b: 5 },
            Step::Rename {
                node: 4,
                from: 9,
                to: 4,
            },
            Step::Partition { a: 4, b: 7 },
            Step::Heal { a: 4, b: 1 },
            Step::Tier {
                a: 5,
                b: 5,
                tier: 0,
            },
            Step::Create {
                node: 7,
                path: 4,
                content: 2,
            },
            Step::Partition { a: 7, b: 6 },
            Step::MassModify {
                node: 3,
                fraction: 92,
                content: 2,
            },
            Step::Settle { secs: 21 },
            Step::Tier {
                a: 0,
                b: 3,
                tier: 1,
            },
            Step::Rename {
                node: 7,
                from: 4,
                to: 2,
            },
            Step::Settle { secs: 23 },
            Step::Touch { node: 1, path: 0 },
            Step::Delete { node: 2, path: 9 },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Everywhere {
                path: 11,
                contents: vec![Some(5), Some(1)],
            },
            Step::Tier {
                a: 6,
                b: 1,
                tier: 0,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(2), Some(1), Some(5), Some(3), None, None, Some(2)],
            },
            Step::Create {
                node: 0,
                path: 0,
                content: 3,
            },
            Step::Offline { node: 7 },
            Step::Touch { node: 5, path: 0 },
            Step::Tier {
                a: 2,
                b: 3,
                tier: 2,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 17,
            },
            Step::MassDelete {
                node: 0,
                fraction: 58,
            },
            Step::Symlink {
                node: 5,
                path: 6,
                target: 11,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 31,
            },
            Step::Partition { a: 1, b: 5 },
            Step::Create {
                node: 5,
                path: 9,
                content: 2,
            },
            Step::Delete { node: 2, path: 9 },
            Step::Rename {
                node: 1,
                from: 8,
                to: 5,
            },
            Step::Online { node: 6 },
            Step::Create {
                node: 7,
                path: 2,
                content: 3,
            },
            Step::Modify {
                node: 4,
                path: 4,
                content: 3,
            },
            Step::Settle { secs: 9 },
            Step::Modify {
                node: 0,
                path: 6,
                content: 2,
            },
            Step::Mkdir { node: 4, dir: 0 },
            Step::Settle { secs: 2 },
            Step::Online { node: 5 },
            Step::Offline { node: 2 },
            Step::Heal { a: 1, b: 3 },
            Step::Tier {
                a: 5,
                b: 3,
                tier: 2,
            },
            Step::Delete { node: 4, path: 4 },
            Step::MassDelete {
                node: 6,
                fraction: 56,
            },
            Step::Modify {
                node: 6,
                path: 3,
                content: 1,
            },
            Step::Symlink {
                node: 5,
                path: 11,
                target: 4,
            },
            Step::Crash {
                node: 6,
                gap_secs: 7,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 5,
                    hold_pct: 41,
                },
                delay_secs: 56,
            },
            Step::Mkdir { node: 4, dir: 0 },
            Step::Create {
                node: 2,
                path: 4,
                content: 1,
            },
            Step::Delete { node: 1, path: 3 },
            Step::Settle { secs: 38 },
            Step::Create {
                node: 7,
                path: 6,
                content: 5,
            },
            Step::MassDelete {
                node: 1,
                fraction: 51,
            },
            Step::Chmod { node: 1, path: 5 },
            Step::Tier {
                a: 2,
                b: 1,
                tier: 2,
            },
            Step::Create {
                node: 2,
                path: 9,
                content: 4,
            },
            Step::Heal { a: 6, b: 7 },
            Step::Heal { a: 7, b: 4 },
            Step::Modify {
                node: 7,
                path: 4,
                content: 3,
            },
            Step::Modify {
                node: 3,
                path: 0,
                content: 3,
            },
            Step::Touch { node: 6, path: 10 },
            Step::Partition { a: 4, b: 3 },
            Step::Partition { a: 4, b: 1 },
            Step::Online { node: 7 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 55,
            },
            Step::Delete { node: 1, path: 0 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(5), None, None],
            },
            Step::Tier {
                a: 1,
                b: 7,
                tier: 1,
            },
            Step::Delete { node: 1, path: 5 },
            Step::Modify {
                node: 5,
                path: 5,
                content: 1,
            },
            Step::Heal { a: 6, b: 2 },
            Step::Delete { node: 1, path: 3 },
            Step::Chmod { node: 1, path: 5 },
            Step::Rename {
                node: 3,
                from: 6,
                to: 10,
            },
            Step::Symlink {
                node: 2,
                path: 6,
                target: 6,
            },
            Step::Create {
                node: 5,
                path: 9,
                content: 2,
            },
            Step::Delete { node: 1, path: 4 },
            Step::Delete { node: 2, path: 1 },
            Step::Delete { node: 7, path: 10 },
            Step::Delete { node: 6, path: 2 },
            Step::Crash {
                node: 1,
                gap_secs: 41,
            },
            Step::MassDelete {
                node: 4,
                fraction: 51,
            },
            Step::Heal { a: 0, b: 1 },
            Step::Touch { node: 5, path: 4 },
            Step::Delete { node: 2, path: 3 },
            Step::Crash {
                node: 4,
                gap_secs: 24,
            },
            Step::Create {
                node: 2,
                path: 7,
                content: 4,
            },
            Step::Create {
                node: 3,
                path: 8,
                content: 2,
            },
            Step::Modify {
                node: 4,
                path: 4,
                content: 3,
            },
            Step::Crash {
                node: 5,
                gap_secs: 49,
            },
            Step::Heal { a: 2, b: 0 },
            Step::Modify {
                node: 4,
                path: 5,
                content: 4,
            },
            Step::Create {
                node: 1,
                path: 4,
                content: 4,
            },
            Step::Touch { node: 1, path: 9 },
            Step::MassDelete {
                node: 6,
                fraction: 90,
            },
            Step::Mkdir { node: 1, dir: 0 },
            Step::User {
                node: 4,
                action: crate::UserAction::Rules {
                    hold_count: 1,
                    hold_pct: 52,
                },
                delay_secs: 51,
            },
            Step::Modify {
                node: 5,
                path: 0,
                content: 1,
            },
            Step::Chmod { node: 3, path: 9 },
            Step::Offline { node: 0 },
            Step::Delete { node: 1, path: 1 },
            Step::Create {
                node: 1,
                path: 6,
                content: 3,
            },
            Step::Touch { node: 4, path: 4 },
            Step::Rename {
                node: 5,
                from: 6,
                to: 8,
            },
            Step::Delete { node: 4, path: 9 },
            Step::Delete { node: 3, path: 5 },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Offline { node: 7 },
            Step::Create {
                node: 1,
                path: 4,
                content: 2,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(5), None, Some(1), Some(4), None],
            },
            Step::Heal { a: 4, b: 1 },
            Step::Partition { a: 5, b: 5 },
            Step::Rename {
                node: 3,
                from: 11,
                to: 3,
            },
            Step::Settle { secs: 30 },
            Step::Modify {
                node: 5,
                path: 6,
                content: 3,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 49,
            },
            Step::Tier {
                a: 5,
                b: 2,
                tier: 1,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::Revert,
                delay_secs: 56,
            },
            Step::Symlink {
                node: 7,
                path: 4,
                target: 11,
            },
            Step::Modify {
                node: 4,
                path: 5,
                content: 5,
            },
            Step::Create {
                node: 4,
                path: 7,
                content: 5,
            },
            Step::Touch { node: 4, path: 3 },
            Step::Online { node: 3 },
            Step::Create {
                node: 3,
                path: 4,
                content: 4,
            },
            Step::Offline { node: 3 },
            Step::Partition { a: 2, b: 6 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(1), None],
            },
            Step::MassDelete {
                node: 0,
                fraction: 85,
            },
            Step::Create {
                node: 2,
                path: 8,
                content: 2,
            },
            Step::Crash {
                node: 4,
                gap_secs: 32,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![None, Some(4), Some(1), Some(5), Some(2), None],
            },
            Step::Modify {
                node: 1,
                path: 8,
                content: 1,
            },
            Step::Offline { node: 2 },
            Step::Partition { a: 4, b: 0 },
            Step::Create {
                node: 0,
                path: 7,
                content: 4,
            },
            Step::Delete { node: 6, path: 6 },
            Step::Crash {
                node: 0,
                gap_secs: 106,
            },
            Step::Mkdir { node: 0, dir: 1 },
            Step::User {
                node: 4,
                action: crate::UserAction::DenyAll,
                delay_secs: 46,
            },
            Step::Offline { node: 5 },
            Step::Online { node: 4 },
            Step::Heal { a: 5, b: 0 },
            Step::Create {
                node: 5,
                path: 3,
                content: 2,
            },
            Step::Modify {
                node: 4,
                path: 9,
                content: 5,
            },
            Step::Create {
                node: 1,
                path: 11,
                content: 4,
            },
            Step::Create {
                node: 6,
                path: 2,
                content: 5,
            },
            Step::Online { node: 2 },
            Step::Modify {
                node: 7,
                path: 0,
                content: 4,
            },
            Step::Create {
                node: 5,
                path: 1,
                content: 3,
            },
            Step::Create {
                node: 6,
                path: 7,
                content: 5,
            },
            Step::Settle { secs: 9 },
            Step::Create {
                node: 2,
                path: 3,
                content: 5,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(3), Some(5), Some(3), Some(1), None, None],
            },
            Step::Create {
                node: 2,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 1,
                path: 7,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 4,
            },
            Step::Create {
                node: 5,
                path: 3,
                content: 3,
            },
            Step::Offline { node: 1 },
            Step::Modify {
                node: 6,
                path: 9,
                content: 5,
            },
            Step::Heal { a: 3, b: 0 },
            Step::Rmdir { node: 2, dir: 2 },
            Step::Create {
                node: 5,
                path: 1,
                content: 1,
            },
            Step::Touch { node: 0, path: 3 },
            Step::Crash {
                node: 5,
                gap_secs: 23,
            },
            Step::Offline { node: 0 },
            Step::Heal { a: 1, b: 5 },
            Step::Settle { secs: 4 },
            Step::Create {
                node: 7,
                path: 10,
                content: 3,
            },
            Step::MassModify {
                node: 3,
                fraction: 64,
                content: 5,
            },
            Step::Symlink {
                node: 4,
                path: 1,
                target: 6,
            },
            Step::Chmod { node: 1, path: 11 },
            Step::Delete { node: 4, path: 2 },
            Step::Settle { secs: 25 },
            Step::Online { node: 6 },
            Step::Create {
                node: 0,
                path: 11,
                content: 2,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 43,
            },
            Step::Partition { a: 0, b: 2 },
            Step::Crash {
                node: 0,
                gap_secs: 78,
            },
            Step::Online { node: 1 },
            Step::Heal { a: 3, b: 4 },
            Step::Rename {
                node: 4,
                from: 3,
                to: 11,
            },
            Step::Delete { node: 3, path: 2 },
            Step::Everywhere {
                path: 7,
                contents: vec![None, Some(4)],
            },
            Step::Tier {
                a: 1,
                b: 7,
                tier: 2,
            },
            Step::Partition { a: 6, b: 5 },
            Step::Modify {
                node: 7,
                path: 1,
                content: 1,
            },
            Step::Rename {
                node: 7,
                from: 11,
                to: 5,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(4), None, Some(3), Some(3)],
            },
            Step::Offline { node: 6 },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(3), Some(1)],
            },
            Step::Delete { node: 3, path: 5 },
            Step::Chmod { node: 0, path: 8 },
            Step::Create {
                node: 1,
                path: 9,
                content: 1,
            },
            Step::Modify {
                node: 1,
                path: 6,
                content: 5,
            },
            Step::Modify {
                node: 1,
                path: 6,
                content: 1,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 6,
            },
            Step::Settle { secs: 3 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 29,
            },
            Step::Create {
                node: 2,
                path: 5,
                content: 5,
            },
            Step::Delete { node: 3, path: 7 },
            Step::Tier {
                a: 1,
                b: 3,
                tier: 0,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![None, Some(1), Some(1), Some(5), Some(1), Some(2), Some(4)],
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(5), Some(3), Some(5), Some(4), Some(4)],
            },
            Step::Touch { node: 5, path: 5 },
            Step::Heal { a: 4, b: 7 },
            Step::Rename {
                node: 3,
                from: 3,
                to: 8,
            },
            Step::Tier {
                a: 4,
                b: 5,
                tier: 2,
            },
            Step::Create {
                node: 7,
                path: 5,
                content: 3,
            },
            Step::Delete { node: 1, path: 1 },
            Step::Offline { node: 4 },
            Step::Delete { node: 6, path: 6 },
            Step::Online { node: 0 },
            Step::Rename {
                node: 1,
                from: 5,
                to: 9,
            },
            Step::Modify {
                node: 7,
                path: 8,
                content: 4,
            },
            Step::Delete { node: 7, path: 1 },
            Step::Crash {
                node: 6,
                gap_secs: 61,
            },
            Step::Rmdir { node: 0, dir: 2 },
            Step::Modify {
                node: 1,
                path: 1,
                content: 1,
            },
            Step::Settle { secs: 4 },
            Step::Chmod { node: 1, path: 11 },
            Step::Touch { node: 6, path: 0 },
            Step::Delete { node: 6, path: 3 },
            Step::Create {
                node: 5,
                path: 8,
                content: 1,
            },
            Step::Delete { node: 4, path: 11 },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 42,
            },
            Step::Create {
                node: 2,
                path: 11,
                content: 3,
            },
            Step::Modify {
                node: 1,
                path: 5,
                content: 3,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 4,
            },
        ],
    );
}

/// The commit guard compared size and mtime for files and kind alone for
/// symlinks, so a chmod or a retarget between a losing version's record
/// and the commit that displaced its file went through, and the conflict
/// copy carried the user's later state under the loser's name (I4). The
/// guard is the scan fast path's predicate now (§7.5 step 6). Re-pinned at
/// seed 97 with the default knobs, shrunk to 254 steps, after
/// drafts 52 and 53's failed-report rules moved seed 39's history; with the guard's exec
/// comparison disabled it fails I4: a conflict copy of f0 has the losing
/// version's hash but not its exec bit.
/// The fix's unit test guards it whether or not a seed reaches it:
/// `sim::tests::the_commit_guard_refuses_a_file_whose_exec_bit_changed`.
#[test]
fn a_chmod_under_a_pending_commit_is_changed_underneath() {
    passes(
        97,
        &[
            Step::Heal { a: 1, b: 4 },
            Step::Symlink {
                node: 1,
                path: 5,
                target: 2,
            },
            Step::Partition { a: 0, b: 1 },
            Step::Rmdir { node: 1, dir: 2 },
            Step::Delete { node: 3, path: 4 },
            Step::Delete { node: 2, path: 6 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 2,
            },
            Step::Mkdir { node: 2, dir: 2 },
            Step::Symlink {
                node: 1,
                path: 5,
                target: 3,
            },
            Step::Delete { node: 3, path: 4 },
            Step::Tier {
                a: 3,
                b: 4,
                tier: 2,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![None, Some(5), Some(4), Some(2), Some(5)],
            },
            Step::MassModify {
                node: 6,
                fraction: 66,
                content: 4,
            },
            Step::Create {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Settle { secs: 12 },
            Step::Heal { a: 2, b: 6 },
            Step::Tier {
                a: 1,
                b: 0,
                tier: 1,
            },
            Step::Modify {
                node: 3,
                path: 10,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 4,
                content: 3,
            },
            Step::Create {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Delete { node: 0, path: 0 },
            Step::Online { node: 1 },
            Step::Rmdir { node: 3, dir: 0 },
            Step::Settle { secs: 31 },
            Step::MassDelete {
                node: 6,
                fraction: 72,
            },
            Step::Heal { a: 1, b: 1 },
            Step::Settle { secs: 8 },
            Step::Heal { a: 1, b: 0 },
            Step::Online { node: 0 },
            Step::Rename {
                node: 4,
                from: 5,
                to: 0,
            },
            Step::Partition { a: 1, b: 7 },
            Step::Modify {
                node: 3,
                path: 11,
                content: 5,
            },
            Step::Crash {
                node: 7,
                gap_secs: 12,
            },
            Step::Partition { a: 3, b: 2 },
            Step::Modify {
                node: 0,
                path: 2,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 8,
                content: 2,
            },
            Step::Partition { a: 0, b: 4 },
            Step::Online { node: 4 },
            Step::Create {
                node: 7,
                path: 8,
                content: 5,
            },
            Step::Chmod { node: 7, path: 1 },
            Step::Rename {
                node: 7,
                from: 3,
                to: 0,
            },
            Step::Partition { a: 2, b: 7 },
            Step::Delete { node: 3, path: 7 },
            Step::Modify {
                node: 3,
                path: 3,
                content: 5,
            },
            Step::Delete { node: 6, path: 8 },
            Step::Offline { node: 6 },
            Step::Modify {
                node: 0,
                path: 8,
                content: 1,
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 5,
            },
            Step::Crash {
                node: 7,
                gap_secs: 33,
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(4), Some(1)],
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Touch { node: 1, path: 11 },
            Step::Symlink {
                node: 0,
                path: 8,
                target: 11,
            },
            Step::Create {
                node: 1,
                path: 6,
                content: 2,
            },
            Step::Rename {
                node: 7,
                from: 5,
                to: 1,
            },
            Step::Rmdir { node: 1, dir: 0 },
            Step::Offline { node: 7 },
            Step::Partition { a: 1, b: 5 },
            Step::User {
                node: 2,
                action: crate::UserAction::DenyAll,
                delay_secs: 4,
            },
            Step::Offline { node: 3 },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 5,
                    hold_pct: 33,
                },
                delay_secs: 1,
            },
            Step::Settle { secs: 3 },
            Step::Rename {
                node: 5,
                from: 7,
                to: 2,
            },
            Step::Delete { node: 5, path: 10 },
            Step::Delete { node: 2, path: 5 },
            Step::Rename {
                node: 7,
                from: 1,
                to: 6,
            },
            Step::Delete { node: 1, path: 11 },
            Step::Crash {
                node: 2,
                gap_secs: 54,
            },
            Step::MassModify {
                node: 6,
                fraction: 74,
                content: 4,
            },
            Step::Modify {
                node: 4,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 3,
                path: 9,
                content: 2,
            },
            Step::Chmod { node: 2, path: 10 },
            Step::Everywhere {
                path: 5,
                contents: vec![None, None],
            },
            Step::Modify {
                node: 6,
                path: 3,
                content: 4,
            },
            Step::Online { node: 2 },
            Step::Delete { node: 6, path: 5 },
            Step::Partition { a: 2, b: 2 },
            Step::Chmod { node: 4, path: 9 },
            Step::Heal { a: 6, b: 2 },
            Step::Modify {
                node: 4,
                path: 11,
                content: 1,
            },
            Step::Partition { a: 1, b: 1 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(1), Some(2)],
            },
            Step::Delete { node: 7, path: 7 },
            Step::Touch { node: 5, path: 4 },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Modify {
                node: 6,
                path: 4,
                content: 3,
            },
            Step::Heal { a: 0, b: 4 },
            Step::Crash {
                node: 3,
                gap_secs: 46,
            },
            Step::Rename {
                node: 7,
                from: 3,
                to: 8,
            },
            Step::Create {
                node: 5,
                path: 11,
                content: 3,
            },
            Step::Offline { node: 7 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(1), Some(2)],
            },
            Step::Heal { a: 1, b: 0 },
            Step::Create {
                node: 4,
                path: 9,
                content: 2,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![None, None],
            },
            Step::Create {
                node: 3,
                path: 10,
                content: 4,
            },
            Step::Modify {
                node: 7,
                path: 1,
                content: 5,
            },
            Step::Settle { secs: 3 },
            Step::Offline { node: 6 },
            Step::Rename {
                node: 3,
                from: 2,
                to: 7,
            },
            Step::MassDelete {
                node: 3,
                fraction: 93,
            },
            Step::Settle { secs: 12 },
            Step::Touch { node: 2, path: 8 },
            Step::Modify {
                node: 7,
                path: 0,
                content: 1,
            },
            Step::Mkdir { node: 4, dir: 2 },
            Step::MassModify {
                node: 7,
                fraction: 79,
                content: 5,
            },
            Step::Settle { secs: 22 },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 27,
            },
            Step::Create {
                node: 7,
                path: 4,
                content: 2,
            },
            Step::MassDelete {
                node: 1,
                fraction: 81,
            },
            Step::Settle { secs: 37 },
            Step::Chmod { node: 0, path: 9 },
            Step::Modify {
                node: 5,
                path: 9,
                content: 2,
            },
            Step::Settle { secs: 2 },
            Step::Modify {
                node: 7,
                path: 2,
                content: 1,
            },
            Step::Tier {
                a: 2,
                b: 2,
                tier: 2,
            },
            Step::Delete { node: 6, path: 5 },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Delete { node: 6, path: 6 },
            Step::Chmod { node: 6, path: 11 },
            Step::Mkdir { node: 3, dir: 2 },
            Step::Modify {
                node: 3,
                path: 6,
                content: 5,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 36,
            },
            Step::Settle { secs: 16 },
            Step::Symlink {
                node: 6,
                path: 0,
                target: 10,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 32,
            },
            Step::Modify {
                node: 6,
                path: 8,
                content: 4,
            },
            Step::Symlink {
                node: 6,
                path: 7,
                target: 8,
            },
            Step::Crash {
                node: 0,
                gap_secs: 42,
            },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Create {
                node: 3,
                path: 0,
                content: 3,
            },
            Step::Modify {
                node: 1,
                path: 6,
                content: 1,
            },
            Step::Delete { node: 4, path: 9 },
            Step::Heal { a: 4, b: 6 },
            Step::Create {
                node: 3,
                path: 3,
                content: 3,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 5,
            },
            Step::Delete { node: 7, path: 2 },
            Step::Create {
                node: 1,
                path: 11,
                content: 4,
            },
            Step::Create {
                node: 1,
                path: 9,
                content: 1,
            },
            Step::Settle { secs: 27 },
            Step::Everywhere {
                path: 3,
                contents: vec![None, Some(1), None, Some(5)],
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 4,
            },
            Step::Delete { node: 5, path: 7 },
            Step::Heal { a: 7, b: 7 },
            Step::Create {
                node: 1,
                path: 8,
                content: 1,
            },
            Step::Heal { a: 0, b: 0 },
            Step::Offline { node: 7 },
            Step::Online { node: 0 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(1), Some(5)],
            },
            Step::Modify {
                node: 4,
                path: 8,
                content: 5,
            },
            Step::Delete { node: 0, path: 8 },
            Step::Delete { node: 2, path: 0 },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 25,
            },
            Step::Modify {
                node: 7,
                path: 4,
                content: 2,
            },
            Step::Crash {
                node: 4,
                gap_secs: 71,
            },
            Step::Settle { secs: 10 },
            Step::Rename {
                node: 2,
                from: 9,
                to: 0,
            },
            Step::Tier {
                a: 6,
                b: 2,
                tier: 0,
            },
            Step::Settle { secs: 18 },
            Step::Heal { a: 1, b: 4 },
            Step::Tier {
                a: 7,
                b: 5,
                tier: 1,
            },
            Step::Create {
                node: 6,
                path: 7,
                content: 4,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![None, Some(4)],
            },
            Step::Touch { node: 2, path: 11 },
            Step::Create {
                node: 0,
                path: 4,
                content: 4,
            },
            Step::Delete { node: 6, path: 11 },
            Step::Offline { node: 7 },
            Step::Offline { node: 2 },
            Step::Create {
                node: 5,
                path: 5,
                content: 1,
            },
            Step::Create {
                node: 0,
                path: 9,
                content: 1,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 1,
            },
            Step::Partition { a: 2, b: 3 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 20,
            },
            Step::Delete { node: 2, path: 8 },
            Step::Symlink {
                node: 7,
                path: 8,
                target: 1,
            },
            Step::Rmdir { node: 2, dir: 1 },
            Step::Settle { secs: 24 },
            Step::Settle { secs: 33 },
            Step::Delete { node: 0, path: 1 },
            Step::Create {
                node: 4,
                path: 1,
                content: 2,
            },
            Step::Create {
                node: 7,
                path: 8,
                content: 1,
            },
            Step::Create {
                node: 4,
                path: 11,
                content: 4,
            },
            Step::Delete { node: 7, path: 7 },
            Step::Create {
                node: 5,
                path: 5,
                content: 4,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![None, Some(2), Some(5)],
            },
            Step::Modify {
                node: 3,
                path: 9,
                content: 3,
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Create {
                node: 0,
                path: 9,
                content: 2,
            },
            Step::MassModify {
                node: 1,
                fraction: 99,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 2,
                content: 2,
            },
            Step::Chmod { node: 5, path: 10 },
            Step::Heal { a: 6, b: 1 },
            Step::Settle { secs: 4 },
            Step::Crash {
                node: 3,
                gap_secs: 100,
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 4,
            },
            Step::Touch { node: 3, path: 10 },
            Step::Settle { secs: 1 },
            Step::Heal { a: 7, b: 4 },
            Step::Chmod { node: 6, path: 10 },
            Step::Tier {
                a: 0,
                b: 1,
                tier: 0,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(4), Some(5)],
            },
            Step::Crash {
                node: 6,
                gap_secs: 93,
            },
            Step::Create {
                node: 3,
                path: 7,
                content: 4,
            },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Create {
                node: 0,
                path: 9,
                content: 2,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 46,
            },
            Step::Settle { secs: 25 },
            Step::Create {
                node: 3,
                path: 8,
                content: 5,
            },
            Step::Rename {
                node: 3,
                from: 6,
                to: 11,
            },
            Step::Create {
                node: 6,
                path: 3,
                content: 1,
            },
            Step::Symlink {
                node: 5,
                path: 1,
                target: 7,
            },
            Step::Create {
                node: 3,
                path: 4,
                content: 5,
            },
            Step::Create {
                node: 4,
                path: 3,
                content: 3,
            },
            Step::Settle { secs: 32 },
            Step::Settle { secs: 1 },
            Step::Partition { a: 6, b: 3 },
            Step::Create {
                node: 5,
                path: 10,
                content: 5,
            },
            Step::Rename {
                node: 4,
                from: 4,
                to: 5,
            },
            Step::Partition { a: 0, b: 7 },
            Step::Delete { node: 2, path: 8 },
            Step::Symlink {
                node: 7,
                path: 11,
                target: 5,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(2), Some(3), Some(5), Some(5), Some(4), Some(2), None],
            },
            Step::Create {
                node: 4,
                path: 1,
                content: 5,
            },
            Step::Settle { secs: 3 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 2,
            },
            Step::Modify {
                node: 4,
                path: 5,
                content: 5,
            },
            Step::Modify {
                node: 5,
                path: 2,
                content: 4,
            },
            Step::Settle { secs: 11 },
            Step::Touch { node: 1, path: 1 },
            Step::Chmod { node: 5, path: 4 },
            Step::Modify {
                node: 0,
                path: 6,
                content: 1,
            },
            Step::Create {
                node: 4,
                path: 9,
                content: 1,
            },
            Step::Tier {
                a: 6,
                b: 7,
                tier: 1,
            },
            Step::Offline { node: 3 },
            Step::Create {
                node: 4,
                path: 1,
                content: 5,
            },
            Step::Crash {
                node: 3,
                gap_secs: 73,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 3,
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Rename {
                node: 7,
                from: 2,
                to: 11,
            },
            Step::Create {
                node: 3,
                path: 11,
                content: 5,
            },
            Step::Rename {
                node: 6,
                from: 7,
                to: 1,
            },
            Step::MassModify {
                node: 2,
                fraction: 73,
                content: 2,
            },
            Step::Settle { secs: 26 },
            Step::Settle { secs: 36 },
            Step::Modify {
                node: 4,
                path: 3,
                content: 2,
            },
            Step::Rename {
                node: 5,
                from: 9,
                to: 9,
            },
            Step::Chmod { node: 4, path: 0 },
            Step::Modify {
                node: 2,
                path: 8,
                content: 4,
            },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Offline { node: 3 },
            Step::Rename {
                node: 7,
                from: 6,
                to: 5,
            },
            Step::Online { node: 6 },
            Step::Modify {
                node: 7,
                path: 2,
                content: 2,
            },
            Step::Modify {
                node: 6,
                path: 0,
                content: 1,
            },
        ],
    );
}

/// Same class, in a shorter run. Re-pinned at seed 134 with the default
/// knobs, shrunk to 43 steps, after drafts 52 and 53's failed-report rules moved seed
/// 125's history; with the guard's exec comparison disabled it fails I4
/// the same way, at a conflict copy of f2.
#[test]
fn a_chmod_under_a_pending_commit_is_changed_underneath_in_a_shorter_run() {
    passes(
        134,
        &[
            Step::Modify {
                node: 3,
                path: 7,
                content: 5,
            },
            Step::Modify {
                node: 1,
                path: 8,
                content: 4,
            },
            Step::Touch { node: 6, path: 6 },
            Step::Delete { node: 2, path: 0 },
            Step::Create {
                node: 6,
                path: 4,
                content: 2,
            },
            Step::MassModify {
                node: 3,
                fraction: 57,
                content: 5,
            },
            Step::Create {
                node: 1,
                path: 5,
                content: 5,
            },
            Step::Modify {
                node: 2,
                path: 8,
                content: 2,
            },
            Step::Online { node: 2 },
            Step::Modify {
                node: 2,
                path: 6,
                content: 1,
            },
            Step::Modify {
                node: 6,
                path: 7,
                content: 1,
            },
            Step::Settle { secs: 24 },
            Step::Settle { secs: 22 },
            Step::Modify {
                node: 0,
                path: 8,
                content: 5,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 1,
            },
            Step::Modify {
                node: 3,
                path: 8,
                content: 4,
            },
            Step::Mkdir { node: 6, dir: 2 },
            Step::Delete { node: 6, path: 10 },
            Step::Create {
                node: 3,
                path: 2,
                content: 1,
            },
            Step::Modify {
                node: 6,
                path: 6,
                content: 2,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 39,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 4,
            },
            Step::Create {
                node: 5,
                path: 10,
                content: 4,
            },
            Step::Modify {
                node: 0,
                path: 3,
                content: 5,
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 1,
            },
            Step::Create {
                node: 4,
                path: 0,
                content: 1,
            },
            Step::Modify {
                node: 1,
                path: 8,
                content: 5,
            },
            Step::MassDelete {
                node: 5,
                fraction: 72,
            },
            Step::MassDelete {
                node: 0,
                fraction: 71,
            },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(3), Some(3), Some(2), Some(4)],
            },
            Step::Heal { a: 7, b: 2 },
            Step::Touch { node: 7, path: 2 },
            Step::Crash {
                node: 7,
                gap_secs: 1,
            },
            Step::Settle { secs: 35 },
            Step::Modify {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Delete { node: 3, path: 2 },
            Step::Offline { node: 7 },
            Step::Partition { a: 7, b: 1 },
            Step::Delete { node: 4, path: 5 },
            Step::Chmod { node: 7, path: 2 },
            Step::Crash {
                node: 0,
                gap_secs: 36,
            },
        ],
    );
}

/// Same class, for a retarget. Re-pinned at seed 497 with the default
/// knobs, shrunk to 151 steps, after drafts 52 and 53's failed-report rules moved seed 1's
/// history; with the guard's target comparison disabled it fails I4: a
/// symlink's conflict copy at d0/f9 has the user's later target, not the
/// losing version's.
/// The fix's unit test guards it whether or not a seed reaches it:
/// `sim::tests::the_commit_guard_refuses_a_symlink_whose_target_changed`.
#[test]
fn a_retarget_under_a_pending_commit_is_changed_underneath() {
    passes(
        497,
        &[
            Step::Symlink {
                node: 6,
                path: 3,
                target: 6,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 11,
                },
                delay_secs: 1,
            },
            Step::Modify {
                node: 1,
                path: 11,
                content: 1,
            },
            Step::Rename {
                node: 4,
                from: 0,
                to: 10,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![None, Some(5), Some(3), Some(4), Some(3)],
            },
            Step::Chmod { node: 3, path: 2 },
            Step::Modify {
                node: 6,
                path: 6,
                content: 2,
            },
            Step::Partition { a: 7, b: 0 },
            Step::Delete { node: 7, path: 1 },
            Step::Online { node: 3 },
            Step::Create {
                node: 4,
                path: 8,
                content: 2,
            },
            Step::Offline { node: 4 },
            Step::Touch { node: 6, path: 5 },
            Step::Modify {
                node: 6,
                path: 6,
                content: 1,
            },
            Step::Create {
                node: 0,
                path: 1,
                content: 4,
            },
            Step::Create {
                node: 1,
                path: 3,
                content: 5,
            },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Touch { node: 3, path: 9 },
            Step::Create {
                node: 7,
                path: 3,
                content: 3,
            },
            Step::Create {
                node: 1,
                path: 11,
                content: 3,
            },
            Step::MassDelete {
                node: 3,
                fraction: 88,
            },
            Step::Delete { node: 0, path: 0 },
            Step::Tier {
                a: 3,
                b: 3,
                tier: 0,
            },
            Step::Create {
                node: 5,
                path: 8,
                content: 4,
            },
            Step::Modify {
                node: 1,
                path: 10,
                content: 4,
            },
            Step::Create {
                node: 3,
                path: 0,
                content: 3,
            },
            Step::Delete { node: 0, path: 0 },
            Step::Crash {
                node: 3,
                gap_secs: 80,
            },
            Step::Symlink {
                node: 0,
                path: 4,
                target: 11,
            },
            Step::Crash {
                node: 5,
                gap_secs: 48,
            },
            Step::Touch { node: 1, path: 2 },
            Step::Modify {
                node: 0,
                path: 0,
                content: 4,
            },
            Step::Modify {
                node: 6,
                path: 8,
                content: 1,
            },
            Step::Settle { secs: 4 },
            Step::Offline { node: 5 },
            Step::Modify {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Crash {
                node: 2,
                gap_secs: 118,
            },
            Step::Heal { a: 2, b: 0 },
            Step::Create {
                node: 0,
                path: 3,
                content: 1,
            },
            Step::Modify {
                node: 1,
                path: 10,
                content: 1,
            },
            Step::Create {
                node: 7,
                path: 2,
                content: 2,
            },
            Step::Modify {
                node: 3,
                path: 5,
                content: 3,
            },
            Step::Settle { secs: 31 },
            Step::Modify {
                node: 6,
                path: 6,
                content: 3,
            },
            Step::MassModify {
                node: 2,
                fraction: 83,
                content: 4,
            },
            Step::Offline { node: 4 },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Everywhere {
                path: 10,
                contents: vec![None, Some(5)],
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(3), None, Some(1), Some(5), Some(3), Some(5)],
            },
            Step::Create {
                node: 5,
                path: 9,
                content: 2,
            },
            Step::Settle { secs: 15 },
            Step::Modify {
                node: 4,
                path: 0,
                content: 3,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 25,
                },
                delay_secs: 26,
            },
            Step::Online { node: 6 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 32,
            },
            Step::Create {
                node: 6,
                path: 2,
                content: 3,
            },
            Step::Modify {
                node: 6,
                path: 6,
                content: 3,
            },
            Step::Delete { node: 1, path: 6 },
            Step::Tier {
                a: 3,
                b: 3,
                tier: 0,
            },
            Step::Modify {
                node: 6,
                path: 11,
                content: 2,
            },
            Step::Heal { a: 2, b: 7 },
            Step::Modify {
                node: 0,
                path: 4,
                content: 3,
            },
            Step::Mkdir { node: 6, dir: 2 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(2), Some(2), None, Some(5), Some(1), Some(4), Some(4)],
            },
            Step::Offline { node: 4 },
            Step::Delete { node: 0, path: 6 },
            Step::Create {
                node: 2,
                path: 1,
                content: 3,
            },
            Step::Settle { secs: 39 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(4), Some(5), None, Some(5)],
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 3,
            },
            Step::Chmod { node: 1, path: 2 },
            Step::Offline { node: 0 },
            Step::Modify {
                node: 6,
                path: 11,
                content: 3,
            },
            Step::Create {
                node: 0,
                path: 8,
                content: 2,
            },
            Step::Delete { node: 7, path: 6 },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(5), Some(4), Some(1), None],
            },
            Step::Heal { a: 7, b: 7 },
            Step::Touch { node: 7, path: 3 },
            Step::Delete { node: 5, path: 11 },
            Step::Chmod { node: 1, path: 10 },
            Step::Modify {
                node: 3,
                path: 8,
                content: 3,
            },
            Step::Create {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Online { node: 1 },
            Step::Create {
                node: 5,
                path: 7,
                content: 4,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 47,
            },
            Step::Modify {
                node: 6,
                path: 0,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 5,
                content: 4,
            },
            Step::Modify {
                node: 0,
                path: 1,
                content: 1,
            },
            Step::Settle { secs: 35 },
            Step::Heal { a: 2, b: 1 },
            Step::Modify {
                node: 1,
                path: 1,
                content: 2,
            },
            Step::Tier {
                a: 5,
                b: 6,
                tier: 0,
            },
            Step::Delete { node: 0, path: 1 },
            Step::Modify {
                node: 1,
                path: 6,
                content: 3,
            },
            Step::Modify {
                node: 0,
                path: 6,
                content: 3,
            },
            Step::Tier {
                a: 3,
                b: 3,
                tier: 1,
            },
            Step::Modify {
                node: 6,
                path: 4,
                content: 4,
            },
            Step::Online { node: 0 },
            Step::Touch { node: 5, path: 1 },
            Step::Modify {
                node: 6,
                path: 8,
                content: 3,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(3), Some(4), Some(5), Some(2), Some(4), Some(4)],
            },
            Step::Online { node: 6 },
            Step::Partition { a: 2, b: 1 },
            Step::Modify {
                node: 5,
                path: 11,
                content: 3,
            },
            Step::Modify {
                node: 1,
                path: 5,
                content: 4,
            },
            Step::Delete { node: 3, path: 5 },
            Step::Chmod { node: 0, path: 8 },
            Step::Settle { secs: 17 },
            Step::Create {
                node: 7,
                path: 7,
                content: 1,
            },
            Step::Touch { node: 7, path: 8 },
            Step::Modify {
                node: 2,
                path: 3,
                content: 4,
            },
            Step::Create {
                node: 4,
                path: 8,
                content: 2,
            },
            Step::Symlink {
                node: 1,
                path: 9,
                target: 7,
            },
            Step::Crash {
                node: 3,
                gap_secs: 57,
            },
            Step::Mkdir { node: 1, dir: 0 },
            Step::Create {
                node: 2,
                path: 4,
                content: 3,
            },
            Step::Delete { node: 6, path: 6 },
            Step::Create {
                node: 7,
                path: 5,
                content: 1,
            },
            Step::Partition { a: 4, b: 1 },
            Step::Rename {
                node: 5,
                from: 4,
                to: 7,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![None, Some(5), Some(3), Some(5)],
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 48,
            },
            Step::Partition { a: 5, b: 2 },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 49,
            },
            Step::Modify {
                node: 6,
                path: 6,
                content: 2,
            },
            Step::Create {
                node: 5,
                path: 3,
                content: 1,
            },
            Step::Everywhere {
                path: 11,
                contents: vec![None, Some(5), Some(1)],
            },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 19,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 5,
            },
            Step::Touch { node: 2, path: 6 },
            Step::Modify {
                node: 6,
                path: 2,
                content: 1,
            },
            Step::Touch { node: 2, path: 3 },
            Step::Delete { node: 7, path: 11 },
            Step::Chmod { node: 5, path: 5 },
            Step::Delete { node: 0, path: 5 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(5), Some(3), Some(5), None, Some(3)],
            },
            Step::Heal { a: 5, b: 3 },
            Step::Delete { node: 6, path: 4 },
            Step::Delete { node: 0, path: 0 },
            Step::Touch { node: 0, path: 0 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(2), Some(3), Some(1), None],
            },
            Step::Partition { a: 6, b: 0 },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 43,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(5), Some(2), Some(1), Some(4)],
            },
            Step::Heal { a: 0, b: 2 },
            Step::Heal { a: 2, b: 1 },
            Step::Heal { a: 0, b: 1 },
            Step::Rmdir { node: 3, dir: 1 },
            Step::Online { node: 4 },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 28,
            },
            Step::Symlink {
                node: 1,
                path: 9,
                target: 2,
            },
        ],
    );
}

/// Same class, for a retarget in another run. Re-pinned at seed 1310 with
/// the default knobs, shrunk to 275 steps, after drafts 52 and 53's failed-report rules
/// moved seed 1's history; with the guard's target comparison disabled it
/// fails I4 the same way, at a conflict copy of d1/f7.
#[test]
fn a_retarget_under_a_pending_commit_is_changed_underneath_in_another_run() {
    passes(
        1310,
        &[
            Step::Settle { secs: 11 },
            Step::Delete { node: 6, path: 7 },
            Step::Modify {
                node: 0,
                path: 1,
                content: 4,
            },
            Step::Tier {
                a: 4,
                b: 4,
                tier: 0,
            },
            Step::Partition { a: 3, b: 7 },
            Step::Tier {
                a: 6,
                b: 7,
                tier: 2,
            },
            Step::Tier {
                a: 7,
                b: 1,
                tier: 2,
            },
            Step::Rename {
                node: 0,
                from: 2,
                to: 1,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(3), Some(5)],
            },
            Step::Delete { node: 7, path: 10 },
            Step::Create {
                node: 7,
                path: 9,
                content: 4,
            },
            Step::Modify {
                node: 6,
                path: 9,
                content: 2,
            },
            Step::Create {
                node: 7,
                path: 3,
                content: 2,
            },
            Step::Crash {
                node: 2,
                gap_secs: 24,
            },
            Step::MassModify {
                node: 5,
                fraction: 99,
                content: 4,
            },
            Step::Modify {
                node: 4,
                path: 11,
                content: 2,
            },
            Step::Create {
                node: 0,
                path: 9,
                content: 4,
            },
            Step::Crash {
                node: 3,
                gap_secs: 68,
            },
            Step::Create {
                node: 5,
                path: 5,
                content: 1,
            },
            Step::Partition { a: 2, b: 2 },
            Step::Chmod { node: 0, path: 0 },
            Step::Rmdir { node: 5, dir: 2 },
            Step::Crash {
                node: 2,
                gap_secs: 41,
            },
            Step::Chmod { node: 4, path: 5 },
            Step::Create {
                node: 0,
                path: 8,
                content: 2,
            },
            Step::Tier {
                a: 5,
                b: 0,
                tier: 0,
            },
            Step::Mkdir { node: 3, dir: 1 },
            Step::Everywhere {
                path: 11,
                contents: vec![
                    Some(3),
                    Some(3),
                    Some(2),
                    Some(3),
                    Some(4),
                    Some(2),
                    Some(1),
                ],
            },
            Step::Partition { a: 1, b: 6 },
            Step::Online { node: 3 },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Modify {
                node: 6,
                path: 1,
                content: 2,
            },
            Step::Settle { secs: 26 },
            Step::Heal { a: 6, b: 0 },
            Step::Delete { node: 7, path: 1 },
            Step::Rename {
                node: 4,
                from: 8,
                to: 1,
            },
            Step::Settle { secs: 1 },
            Step::Heal { a: 7, b: 2 },
            Step::Modify {
                node: 1,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 7,
                path: 8,
                content: 4,
            },
            Step::Online { node: 6 },
            Step::Modify {
                node: 1,
                path: 0,
                content: 1,
            },
            Step::Rename {
                node: 0,
                from: 8,
                to: 10,
            },
            Step::Partition { a: 3, b: 4 },
            Step::Symlink {
                node: 5,
                path: 9,
                target: 6,
            },
            Step::MassDelete {
                node: 2,
                fraction: 57,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 1,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 13,
            },
            Step::Modify {
                node: 4,
                path: 11,
                content: 4,
            },
            Step::Offline { node: 3 },
            Step::Create {
                node: 0,
                path: 8,
                content: 3,
            },
            Step::Rename {
                node: 0,
                from: 5,
                to: 4,
            },
            Step::Heal { a: 3, b: 3 },
            Step::Mkdir { node: 3, dir: 0 },
            Step::Heal { a: 0, b: 7 },
            Step::Delete { node: 3, path: 8 },
            Step::Partition { a: 7, b: 7 },
            Step::Chmod { node: 7, path: 6 },
            Step::MassModify {
                node: 6,
                fraction: 81,
                content: 4,
            },
            Step::Partition { a: 1, b: 2 },
            Step::Partition { a: 2, b: 1 },
            Step::Create {
                node: 7,
                path: 8,
                content: 2,
            },
            Step::Heal { a: 0, b: 4 },
            Step::Settle { secs: 21 },
            Step::Modify {
                node: 3,
                path: 11,
                content: 1,
            },
            Step::Modify {
                node: 5,
                path: 9,
                content: 4,
            },
            Step::Heal { a: 1, b: 5 },
            Step::Modify {
                node: 5,
                path: 0,
                content: 1,
            },
            Step::Create {
                node: 3,
                path: 7,
                content: 4,
            },
            Step::Heal { a: 2, b: 6 },
            Step::Create {
                node: 7,
                path: 9,
                content: 3,
            },
            Step::Modify {
                node: 6,
                path: 3,
                content: 1,
            },
            Step::Create {
                node: 2,
                path: 2,
                content: 5,
            },
            Step::Online { node: 1 },
            Step::Modify {
                node: 4,
                path: 9,
                content: 2,
            },
            Step::Heal { a: 1, b: 4 },
            Step::Modify {
                node: 2,
                path: 7,
                content: 2,
            },
            Step::Modify {
                node: 7,
                path: 2,
                content: 1,
            },
            Step::Partition { a: 5, b: 0 },
            Step::Delete { node: 2, path: 10 },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Rename {
                node: 0,
                from: 9,
                to: 3,
            },
            Step::Partition { a: 2, b: 2 },
            Step::Touch { node: 4, path: 10 },
            Step::Modify {
                node: 5,
                path: 3,
                content: 4,
            },
            Step::Create {
                node: 5,
                path: 5,
                content: 2,
            },
            Step::Create {
                node: 7,
                path: 4,
                content: 3,
            },
            Step::Partition { a: 1, b: 3 },
            Step::Modify {
                node: 2,
                path: 8,
                content: 5,
            },
            Step::Rename {
                node: 7,
                from: 2,
                to: 5,
            },
            Step::Heal { a: 2, b: 4 },
            Step::Create {
                node: 2,
                path: 10,
                content: 1,
            },
            Step::Tier {
                a: 3,
                b: 7,
                tier: 2,
            },
            Step::Heal { a: 0, b: 0 },
            Step::Create {
                node: 6,
                path: 3,
                content: 2,
            },
            Step::Create {
                node: 5,
                path: 0,
                content: 2,
            },
            Step::Delete { node: 5, path: 11 },
            Step::Crash {
                node: 1,
                gap_secs: 82,
            },
            Step::Modify {
                node: 2,
                path: 9,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 4,
                content: 1,
            },
            Step::Modify {
                node: 4,
                path: 5,
                content: 4,
            },
            Step::Rename {
                node: 1,
                from: 6,
                to: 7,
            },
            Step::Create {
                node: 6,
                path: 11,
                content: 2,
            },
            Step::Delete { node: 3, path: 6 },
            Step::Create {
                node: 1,
                path: 7,
                content: 2,
            },
            Step::Partition { a: 1, b: 6 },
            Step::Crash {
                node: 5,
                gap_secs: 110,
            },
            Step::Partition { a: 5, b: 5 },
            Step::Create {
                node: 7,
                path: 5,
                content: 3,
            },
            Step::Rename {
                node: 1,
                from: 5,
                to: 11,
            },
            Step::Online { node: 5 },
            Step::Modify {
                node: 4,
                path: 5,
                content: 3,
            },
            Step::Partition { a: 2, b: 2 },
            Step::Chmod { node: 3, path: 8 },
            Step::Heal { a: 2, b: 0 },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(5), Some(4), Some(3), Some(4)],
            },
            Step::Chmod { node: 2, path: 7 },
            Step::Create {
                node: 6,
                path: 9,
                content: 2,
            },
            Step::Partition { a: 2, b: 3 },
            Step::Heal { a: 4, b: 3 },
            Step::Chmod { node: 4, path: 3 },
            Step::Offline { node: 7 },
            Step::Modify {
                node: 1,
                path: 3,
                content: 2,
            },
            Step::Heal { a: 2, b: 2 },
            Step::Online { node: 2 },
            Step::Online { node: 2 },
            Step::Modify {
                node: 3,
                path: 0,
                content: 4,
            },
            Step::Partition { a: 5, b: 1 },
            Step::Heal { a: 4, b: 7 },
            Step::Modify {
                node: 6,
                path: 7,
                content: 4,
            },
            Step::Crash {
                node: 7,
                gap_secs: 54,
            },
            Step::Touch { node: 5, path: 11 },
            Step::Create {
                node: 7,
                path: 2,
                content: 4,
            },
            Step::Rmdir { node: 0, dir: 2 },
            Step::Modify {
                node: 1,
                path: 8,
                content: 1,
            },
            Step::Rename {
                node: 0,
                from: 5,
                to: 5,
            },
            Step::Delete { node: 5, path: 1 },
            Step::Delete { node: 3, path: 9 },
            Step::Create {
                node: 5,
                path: 0,
                content: 5,
            },
            Step::Settle { secs: 14 },
            Step::Create {
                node: 0,
                path: 4,
                content: 2,
            },
            Step::Settle { secs: 11 },
            Step::Rmdir { node: 2, dir: 1 },
            Step::Partition { a: 7, b: 3 },
            Step::Rmdir { node: 7, dir: 0 },
            Step::Crash {
                node: 5,
                gap_secs: 54,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 55,
            },
            Step::Modify {
                node: 2,
                path: 4,
                content: 3,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 5,
            },
            Step::Offline { node: 1 },
            Step::Rmdir { node: 7, dir: 0 },
            Step::Touch { node: 7, path: 9 },
            Step::Partition { a: 5, b: 0 },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(2), Some(5)],
            },
            Step::Heal { a: 3, b: 4 },
            Step::Modify {
                node: 2,
                path: 0,
                content: 5,
            },
            Step::Partition { a: 4, b: 7 },
            Step::Touch { node: 1, path: 10 },
            Step::Modify {
                node: 3,
                path: 11,
                content: 3,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 5,
            },
            Step::Create {
                node: 6,
                path: 8,
                content: 2,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 23,
            },
            Step::Offline { node: 3 },
            Step::Heal { a: 5, b: 7 },
            Step::Chmod { node: 7, path: 2 },
            Step::Chmod { node: 4, path: 5 },
            Step::Heal { a: 6, b: 5 },
            Step::Settle { secs: 9 },
            Step::Partition { a: 2, b: 3 },
            Step::Touch { node: 5, path: 8 },
            Step::Heal { a: 1, b: 6 },
            Step::Rename {
                node: 3,
                from: 9,
                to: 5,
            },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Settle { secs: 7 },
            Step::Mkdir { node: 3, dir: 2 },
            Step::Partition { a: 4, b: 4 },
            Step::Touch { node: 2, path: 2 },
            Step::Offline { node: 0 },
            Step::Settle { secs: 21 },
            Step::Mkdir { node: 2, dir: 1 },
            Step::MassModify {
                node: 0,
                fraction: 79,
                content: 1,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::DenyAll,
                delay_secs: 6,
            },
            Step::Online { node: 3 },
            Step::Offline { node: 1 },
            Step::Settle { secs: 17 },
            Step::Modify {
                node: 1,
                path: 7,
                content: 1,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 16,
            },
            Step::Partition { a: 5, b: 2 },
            Step::Online { node: 4 },
            Step::Delete { node: 6, path: 1 },
            Step::Create {
                node: 4,
                path: 3,
                content: 4,
            },
            Step::Settle { secs: 5 },
            Step::Tier {
                a: 2,
                b: 7,
                tier: 1,
            },
            Step::Symlink {
                node: 0,
                path: 6,
                target: 9,
            },
            Step::Heal { a: 1, b: 3 },
            Step::Heal { a: 3, b: 2 },
            Step::Create {
                node: 7,
                path: 2,
                content: 4,
            },
            Step::Modify {
                node: 1,
                path: 1,
                content: 4,
            },
            Step::Heal { a: 2, b: 7 },
            Step::Crash {
                node: 2,
                gap_secs: 20,
            },
            Step::Rmdir { node: 2, dir: 1 },
            Step::Settle { secs: 29 },
            Step::Modify {
                node: 1,
                path: 6,
                content: 5,
            },
            Step::MassModify {
                node: 0,
                fraction: 76,
                content: 5,
            },
            Step::Delete { node: 1, path: 1 },
            Step::Create {
                node: 7,
                path: 0,
                content: 2,
            },
            Step::Online { node: 1 },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Modify {
                node: 5,
                path: 8,
                content: 5,
            },
            Step::Create {
                node: 0,
                path: 11,
                content: 4,
            },
            Step::Create {
                node: 2,
                path: 10,
                content: 4,
            },
            Step::Settle { secs: 39 },
            Step::Partition { a: 1, b: 7 },
            Step::Create {
                node: 2,
                path: 8,
                content: 2,
            },
            Step::Delete { node: 1, path: 5 },
            Step::Partition { a: 3, b: 3 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 0,
            },
            Step::Create {
                node: 3,
                path: 9,
                content: 3,
            },
            Step::Settle { secs: 6 },
            Step::MassModify {
                node: 5,
                fraction: 61,
                content: 2,
            },
            Step::Rename {
                node: 1,
                from: 9,
                to: 0,
            },
            Step::MassModify {
                node: 3,
                fraction: 68,
                content: 2,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 57,
            },
            Step::Heal { a: 2, b: 0 },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(3), Some(2), None],
            },
            Step::Settle { secs: 15 },
            Step::Rename {
                node: 6,
                from: 7,
                to: 7,
            },
            Step::Crash {
                node: 5,
                gap_secs: 37,
            },
            Step::Settle { secs: 18 },
            Step::Touch { node: 2, path: 8 },
            Step::Create {
                node: 6,
                path: 10,
                content: 5,
            },
            Step::Offline { node: 3 },
            Step::Mkdir { node: 3, dir: 1 },
            Step::MassDelete {
                node: 4,
                fraction: 79,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 51,
            },
            Step::Heal { a: 7, b: 6 },
            Step::Rmdir { node: 2, dir: 0 },
            Step::Rmdir { node: 5, dir: 0 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(4), Some(5), None, Some(4), None, Some(3)],
            },
            Step::Mkdir { node: 3, dir: 2 },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Delete { node: 1, path: 6 },
            Step::Create {
                node: 2,
                path: 2,
                content: 2,
            },
            Step::Settle { secs: 28 },
            Step::Symlink {
                node: 2,
                path: 7,
                target: 4,
            },
            Step::Settle { secs: 9 },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(5), Some(5)],
            },
            Step::Touch { node: 1, path: 2 },
            Step::Create {
                node: 0,
                path: 2,
                content: 3,
            },
            Step::Tier {
                a: 7,
                b: 7,
                tier: 1,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(5), Some(1), Some(5)],
            },
            Step::Everywhere {
                path: 8,
                contents: vec![None, None, Some(2), Some(1)],
            },
            Step::Partition { a: 7, b: 6 },
            Step::Chmod { node: 0, path: 5 },
            Step::Rename {
                node: 0,
                from: 0,
                to: 10,
            },
            Step::MassDelete {
                node: 1,
                fraction: 91,
            },
            Step::Tier {
                a: 5,
                b: 1,
                tier: 2,
            },
            Step::Settle { secs: 18 },
            Step::Chmod { node: 3, path: 3 },
            Step::Heal { a: 2, b: 3 },
            Step::Tier {
                a: 4,
                b: 1,
                tier: 2,
            },
            Step::Chmod { node: 7, path: 6 },
            Step::Modify {
                node: 5,
                path: 7,
                content: 1,
            },
            Step::Delete { node: 5, path: 5 },
            Step::Modify {
                node: 5,
                path: 8,
                content: 1,
            },
            Step::Crash {
                node: 1,
                gap_secs: 104,
            },
            Step::Create {
                node: 6,
                path: 1,
                content: 4,
            },
            Step::Create {
                node: 2,
                path: 10,
                content: 4,
            },
            Step::Touch { node: 0, path: 5 },
            Step::Symlink {
                node: 2,
                path: 7,
                target: 2,
            },
            Step::Settle { secs: 9 },
            Step::Delete { node: 3, path: 9 },
            Step::Modify {
                node: 7,
                path: 7,
                content: 2,
            },
            Step::Delete { node: 5, path: 3 },
        ],
    );
}

/// A concurrent entry joining a held item had its version quarantined but
/// not its entry, so `deny`'s bump folded the version into its vector while
/// the stamp it started from ignored it: the bump dominated the joined
/// version and ranked below it, two nodes ranked them differently, and one
/// vector carried two contents (I7). Every quarantined version now carries
/// its stamp, and the bump starts above the largest of them. Re-pinned at
/// seed 1488 with the default knobs, shrunk to 348 steps keeping the
/// tombstone, after draft 54's refusal rules moved seed 183's history; with the
/// fix disabled it fails I7 (a live file and a tombstone under one vector
/// at d2/f11).
#[test]
fn a_joined_versions_stamp_counts_for_the_deny_bump() {
    passes(
        1488,
        &[
            Step::Create {
                node: 5,
                path: 0,
                content: 4,
            },
            Step::Settle { secs: 21 },
            Step::Settle { secs: 23 },
            Step::Modify {
                node: 6,
                path: 9,
                content: 1,
            },
            Step::Rename {
                node: 7,
                from: 7,
                to: 1,
            },
            Step::Create {
                node: 2,
                path: 9,
                content: 3,
            },
            Step::Modify {
                node: 1,
                path: 7,
                content: 1,
            },
            Step::Chmod { node: 0, path: 4 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(4), Some(2), Some(2)],
            },
            Step::Create {
                node: 3,
                path: 4,
                content: 1,
            },
            Step::Settle { secs: 25 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(1), Some(3), Some(3), Some(5), None, Some(2)],
            },
            Step::Rmdir { node: 6, dir: 0 },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 0,
            },
            Step::Heal { a: 1, b: 2 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(4), Some(4), Some(2)],
            },
            Step::Create {
                node: 5,
                path: 5,
                content: 1,
            },
            Step::Delete { node: 3, path: 8 },
            Step::Create {
                node: 3,
                path: 4,
                content: 5,
            },
            Step::Delete { node: 5, path: 3 },
            Step::Delete { node: 2, path: 6 },
            Step::Modify {
                node: 2,
                path: 6,
                content: 5,
            },
            Step::Create {
                node: 2,
                path: 1,
                content: 1,
            },
            Step::MassModify {
                node: 7,
                fraction: 86,
                content: 2,
            },
            Step::Touch { node: 6, path: 1 },
            Step::Symlink {
                node: 1,
                path: 1,
                target: 7,
            },
            Step::Create {
                node: 6,
                path: 8,
                content: 5,
            },
            Step::Settle { secs: 4 },
            Step::Create {
                node: 7,
                path: 0,
                content: 2,
            },
            Step::Delete { node: 2, path: 11 },
            Step::Touch { node: 0, path: 6 },
            Step::Partition { a: 7, b: 0 },
            Step::Crash {
                node: 4,
                gap_secs: 91,
            },
            Step::Delete { node: 2, path: 2 },
            Step::Modify {
                node: 6,
                path: 4,
                content: 4,
            },
            Step::Delete { node: 6, path: 1 },
            Step::Crash {
                node: 5,
                gap_secs: 69,
            },
            Step::MassDelete {
                node: 2,
                fraction: 59,
            },
            Step::Online { node: 1 },
            Step::Crash {
                node: 2,
                gap_secs: 103,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![None, Some(5)],
            },
            Step::Online { node: 6 },
            Step::Chmod { node: 4, path: 8 },
            Step::Create {
                node: 3,
                path: 6,
                content: 3,
            },
            Step::MassDelete {
                node: 2,
                fraction: 75,
            },
            Step::Create {
                node: 0,
                path: 0,
                content: 2,
            },
            Step::Symlink {
                node: 2,
                path: 7,
                target: 1,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![None, None, None],
            },
            Step::Create {
                node: 0,
                path: 1,
                content: 5,
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Settle { secs: 34 },
            Step::Rename {
                node: 6,
                from: 4,
                to: 0,
            },
            Step::Heal { a: 1, b: 5 },
            Step::Delete { node: 6, path: 4 },
            Step::Crash {
                node: 2,
                gap_secs: 86,
            },
            Step::Create {
                node: 5,
                path: 0,
                content: 2,
            },
            Step::Delete { node: 4, path: 3 },
            Step::Heal { a: 1, b: 6 },
            Step::Create {
                node: 2,
                path: 5,
                content: 5,
            },
            Step::Online { node: 3 },
            Step::Delete { node: 3, path: 8 },
            Step::Touch { node: 3, path: 11 },
            Step::Create {
                node: 4,
                path: 9,
                content: 1,
            },
            Step::Modify {
                node: 4,
                path: 9,
                content: 4,
            },
            Step::Partition { a: 4, b: 5 },
            Step::Touch { node: 2, path: 4 },
            Step::Create {
                node: 1,
                path: 3,
                content: 2,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::Rules {
                    hold_count: 5,
                    hold_pct: 33,
                },
                delay_secs: 35,
            },
            Step::Partition { a: 5, b: 4 },
            Step::Crash {
                node: 2,
                gap_secs: 104,
            },
            Step::Offline { node: 6 },
            Step::Create {
                node: 7,
                path: 1,
                content: 1,
            },
            Step::Modify {
                node: 6,
                path: 8,
                content: 3,
            },
            Step::Rmdir { node: 5, dir: 2 },
            Step::User {
                node: 5,
                action: crate::UserAction::DenyAll,
                delay_secs: 19,
            },
            Step::Touch { node: 7, path: 6 },
            Step::Delete { node: 1, path: 4 },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 4,
            },
            Step::Partition { a: 2, b: 2 },
            Step::Modify {
                node: 1,
                path: 2,
                content: 5,
            },
            Step::Online { node: 1 },
            Step::Heal { a: 5, b: 5 },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 37,
            },
            Step::Create {
                node: 4,
                path: 0,
                content: 3,
            },
            Step::Mkdir { node: 4, dir: 2 },
            Step::Create {
                node: 0,
                path: 7,
                content: 5,
            },
            Step::MassDelete {
                node: 7,
                fraction: 75,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(4), Some(5), Some(4), Some(3)],
            },
            Step::Crash {
                node: 1,
                gap_secs: 89,
            },
            Step::Settle { secs: 20 },
            Step::Touch { node: 3, path: 5 },
            Step::Create {
                node: 7,
                path: 4,
                content: 2,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 22,
            },
            Step::Settle { secs: 3 },
            Step::Rename {
                node: 5,
                from: 8,
                to: 4,
            },
            Step::Online { node: 4 },
            Step::Create {
                node: 7,
                path: 8,
                content: 2,
            },
            Step::Touch { node: 7, path: 9 },
            Step::Tier {
                a: 2,
                b: 0,
                tier: 1,
            },
            Step::Heal { a: 0, b: 4 },
            Step::Everywhere {
                path: 10,
                contents: vec![None, Some(1), None, Some(3), None],
            },
            Step::MassDelete {
                node: 3,
                fraction: 73,
            },
            Step::Mkdir { node: 4, dir: 0 },
            Step::Offline { node: 4 },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(2), Some(5), Some(5), None],
            },
            Step::Modify {
                node: 6,
                path: 1,
                content: 5,
            },
            Step::Modify {
                node: 7,
                path: 5,
                content: 5,
            },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Everywhere {
                path: 3,
                contents: vec![None, Some(3), Some(4), Some(1), Some(1), Some(1)],
            },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(5), None, Some(2), Some(4), Some(1)],
            },
            Step::Modify {
                node: 3,
                path: 10,
                content: 3,
            },
            Step::Crash {
                node: 7,
                gap_secs: 108,
            },
            Step::Modify {
                node: 1,
                path: 3,
                content: 1,
            },
            Step::Tier {
                a: 4,
                b: 0,
                tier: 2,
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 2,
            },
            Step::Create {
                node: 1,
                path: 11,
                content: 2,
            },
            Step::Partition { a: 7, b: 4 },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 38,
                },
                delay_secs: 5,
            },
            Step::Settle { secs: 11 },
            Step::Modify {
                node: 7,
                path: 5,
                content: 3,
            },
            Step::Create {
                node: 5,
                path: 6,
                content: 5,
            },
            Step::Modify {
                node: 4,
                path: 8,
                content: 2,
            },
            Step::Modify {
                node: 6,
                path: 11,
                content: 1,
            },
            Step::Delete { node: 6, path: 10 },
            Step::Heal { a: 1, b: 5 },
            Step::Partition { a: 2, b: 5 },
            Step::Symlink {
                node: 7,
                path: 10,
                target: 0,
            },
            Step::Partition { a: 1, b: 4 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(1), Some(5)],
            },
            Step::Tier {
                a: 1,
                b: 0,
                tier: 0,
            },
            Step::MassDelete {
                node: 3,
                fraction: 77,
            },
            Step::Partition { a: 7, b: 5 },
            Step::Create {
                node: 5,
                path: 0,
                content: 2,
            },
            Step::Rename {
                node: 3,
                from: 3,
                to: 1,
            },
            Step::Create {
                node: 6,
                path: 8,
                content: 1,
            },
            Step::Delete { node: 3, path: 4 },
            Step::Settle { secs: 4 },
            Step::Crash {
                node: 0,
                gap_secs: 49,
            },
            Step::Settle { secs: 34 },
            Step::Modify {
                node: 1,
                path: 5,
                content: 1,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Rules {
                    hold_count: 5,
                    hold_pct: 18,
                },
                delay_secs: 26,
            },
            Step::Rmdir { node: 7, dir: 0 },
            Step::Delete { node: 5, path: 0 },
            Step::Mkdir { node: 3, dir: 1 },
            Step::Settle { secs: 19 },
            Step::Settle { secs: 24 },
            Step::Tier {
                a: 3,
                b: 6,
                tier: 1,
            },
            Step::Modify {
                node: 6,
                path: 7,
                content: 4,
            },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Partition { a: 4, b: 1 },
            Step::Chmod { node: 0, path: 6 },
            Step::Partition { a: 3, b: 1 },
            Step::Tier {
                a: 0,
                b: 4,
                tier: 2,
            },
            Step::Modify {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 55,
            },
            Step::Delete { node: 1, path: 4 },
            Step::Modify {
                node: 4,
                path: 0,
                content: 2,
            },
            Step::MassDelete {
                node: 0,
                fraction: 87,
            },
            Step::Symlink {
                node: 7,
                path: 1,
                target: 11,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 46,
            },
            Step::Touch { node: 0, path: 4 },
            Step::Delete { node: 5, path: 0 },
            Step::Settle { secs: 8 },
            Step::Partition { a: 2, b: 5 },
            Step::Offline { node: 7 },
            Step::Rmdir { node: 4, dir: 0 },
            Step::Delete { node: 2, path: 7 },
            Step::Everywhere {
                path: 3,
                contents: vec![None, None, None, Some(1), Some(3), Some(4), Some(3)],
            },
            Step::Create {
                node: 1,
                path: 9,
                content: 2,
            },
            Step::Modify {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Crash {
                node: 0,
                gap_secs: 20,
            },
            Step::Chmod { node: 6, path: 11 },
            Step::Modify {
                node: 3,
                path: 10,
                content: 2,
            },
            Step::Mkdir { node: 6, dir: 2 },
            Step::Create {
                node: 4,
                path: 10,
                content: 3,
            },
            Step::Touch { node: 6, path: 7 },
            Step::Modify {
                node: 5,
                path: 8,
                content: 3,
            },
            Step::Offline { node: 1 },
            Step::Modify {
                node: 4,
                path: 4,
                content: 2,
            },
            Step::Delete { node: 2, path: 6 },
            Step::Create {
                node: 2,
                path: 6,
                content: 5,
            },
            Step::Modify {
                node: 5,
                path: 11,
                content: 4,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 59,
            },
            Step::Tier {
                a: 2,
                b: 1,
                tier: 0,
            },
            Step::Delete { node: 7, path: 5 },
            Step::Rename {
                node: 3,
                from: 6,
                to: 5,
            },
            Step::Modify {
                node: 1,
                path: 0,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 1,
                content: 5,
            },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(2), Some(1), None, None, Some(4), Some(5), None],
            },
            Step::Create {
                node: 5,
                path: 11,
                content: 2,
            },
            Step::Partition { a: 4, b: 2 },
            Step::Crash {
                node: 7,
                gap_secs: 50,
            },
            Step::Heal { a: 2, b: 5 },
            Step::Chmod { node: 4, path: 11 },
            Step::Delete { node: 0, path: 4 },
            Step::Create {
                node: 7,
                path: 9,
                content: 4,
            },
            Step::Settle { secs: 8 },
            Step::Tier {
                a: 4,
                b: 6,
                tier: 1,
            },
            Step::Delete { node: 3, path: 3 },
            Step::Create {
                node: 6,
                path: 10,
                content: 5,
            },
            Step::Partition { a: 5, b: 5 },
            Step::Create {
                node: 4,
                path: 6,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 2,
            },
            Step::Create {
                node: 1,
                path: 5,
                content: 5,
            },
            Step::Modify {
                node: 2,
                path: 8,
                content: 1,
            },
            Step::Delete { node: 4, path: 10 },
            Step::Create {
                node: 5,
                path: 2,
                content: 2,
            },
            Step::Settle { secs: 22 },
            Step::Modify {
                node: 1,
                path: 1,
                content: 2,
            },
            Step::Heal { a: 2, b: 4 },
            Step::Crash {
                node: 6,
                gap_secs: 49,
            },
            Step::Heal { a: 7, b: 1 },
            Step::Create {
                node: 7,
                path: 2,
                content: 2,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![None, None, Some(2)],
            },
            Step::Touch { node: 0, path: 8 },
            Step::Settle { secs: 14 },
            Step::Settle { secs: 3 },
            Step::Everywhere {
                path: 11,
                contents: vec![None, Some(4), None],
            },
            Step::Delete { node: 7, path: 10 },
            Step::Create {
                node: 2,
                path: 5,
                content: 1,
            },
            Step::Rename {
                node: 5,
                from: 11,
                to: 4,
            },
            Step::Modify {
                node: 3,
                path: 1,
                content: 1,
            },
            Step::Modify {
                node: 1,
                path: 9,
                content: 1,
            },
            Step::Settle { secs: 11 },
            Step::Offline { node: 7 },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(3), None, Some(1), Some(2)],
            },
            Step::Delete { node: 2, path: 2 },
            Step::Create {
                node: 2,
                path: 10,
                content: 3,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 32,
                },
                delay_secs: 59,
            },
            Step::Delete { node: 7, path: 9 },
            Step::Create {
                node: 6,
                path: 10,
                content: 3,
            },
            Step::Create {
                node: 0,
                path: 10,
                content: 2,
            },
            Step::Create {
                node: 5,
                path: 2,
                content: 5,
            },
            Step::Settle { secs: 37 },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 1,
                    hold_pct: 30,
                },
                delay_secs: 47,
            },
            Step::Modify {
                node: 3,
                path: 3,
                content: 2,
            },
            Step::Modify {
                node: 6,
                path: 7,
                content: 3,
            },
            Step::Delete { node: 3, path: 10 },
            Step::Settle { secs: 33 },
            Step::Offline { node: 0 },
            Step::Modify {
                node: 6,
                path: 6,
                content: 5,
            },
            Step::Rmdir { node: 0, dir: 1 },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Partition { a: 4, b: 2 },
            Step::Chmod { node: 3, path: 1 },
            Step::MassModify {
                node: 0,
                fraction: 71,
                content: 1,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 45,
            },
            Step::Heal { a: 2, b: 1 },
            Step::Create {
                node: 6,
                path: 10,
                content: 1,
            },
            Step::Online { node: 0 },
            Step::MassModify {
                node: 7,
                fraction: 88,
                content: 4,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::DenyAll,
                delay_secs: 30,
            },
            Step::Create {
                node: 7,
                path: 11,
                content: 3,
            },
            Step::Create {
                node: 6,
                path: 4,
                content: 4,
            },
            Step::Create {
                node: 0,
                path: 8,
                content: 3,
            },
            Step::Rename {
                node: 0,
                from: 0,
                to: 4,
            },
            Step::Heal { a: 0, b: 7 },
            Step::Symlink {
                node: 4,
                path: 0,
                target: 5,
            },
            Step::MassModify {
                node: 3,
                fraction: 66,
                content: 3,
            },
            Step::Symlink {
                node: 4,
                path: 5,
                target: 9,
            },
            Step::Create {
                node: 7,
                path: 2,
                content: 1,
            },
            Step::Create {
                node: 2,
                path: 3,
                content: 3,
            },
            Step::Modify {
                node: 0,
                path: 1,
                content: 1,
            },
            Step::MassDelete {
                node: 2,
                fraction: 61,
            },
            Step::Touch { node: 3, path: 0 },
            Step::Delete { node: 0, path: 1 },
            Step::Partition { a: 0, b: 5 },
            Step::Rename {
                node: 1,
                from: 11,
                to: 2,
            },
            Step::Mkdir { node: 7, dir: 0 },
            Step::Create {
                node: 5,
                path: 1,
                content: 4,
            },
            Step::Modify {
                node: 0,
                path: 0,
                content: 2,
            },
            Step::Delete { node: 6, path: 6 },
            Step::User {
                node: 2,
                action: crate::UserAction::DenyAll,
                delay_secs: 37,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 6,
            },
            Step::Create {
                node: 3,
                path: 0,
                content: 1,
            },
            Step::MassDelete {
                node: 0,
                fraction: 88,
            },
            Step::Modify {
                node: 6,
                path: 6,
                content: 3,
            },
            Step::Modify {
                node: 4,
                path: 9,
                content: 5,
            },
            Step::MassModify {
                node: 3,
                fraction: 88,
                content: 2,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![None, Some(3), Some(5), Some(1), Some(5), Some(3)],
            },
            Step::Modify {
                node: 7,
                path: 5,
                content: 4,
            },
            Step::Delete { node: 3, path: 9 },
            Step::Create {
                node: 3,
                path: 6,
                content: 1,
            },
            Step::Online { node: 7 },
            Step::Heal { a: 0, b: 3 },
            Step::Rmdir { node: 0, dir: 0 },
            Step::MassDelete {
                node: 6,
                fraction: 91,
            },
            Step::Heal { a: 0, b: 0 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 43,
            },
            Step::Create {
                node: 6,
                path: 0,
                content: 5,
            },
            Step::Settle { secs: 13 },
            Step::Offline { node: 7 },
            Step::Settle { secs: 34 },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 1,
            },
            Step::Modify {
                node: 0,
                path: 6,
                content: 4,
            },
            Step::Offline { node: 7 },
            Step::Modify {
                node: 3,
                path: 2,
                content: 2,
            },
            Step::Crash {
                node: 2,
                gap_secs: 102,
            },
            Step::Modify {
                node: 5,
                path: 2,
                content: 4,
            },
            Step::Create {
                node: 1,
                path: 7,
                content: 1,
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 1,
            },
            Step::Tier {
                a: 7,
                b: 1,
                tier: 2,
            },
            Step::Rmdir { node: 7, dir: 0 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 20,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(2), Some(2), Some(4), Some(5), Some(1)],
            },
            Step::Crash {
                node: 3,
                gap_secs: 4,
            },
            Step::Create {
                node: 5,
                path: 2,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 1,
                content: 3,
            },
            Step::MassDelete {
                node: 7,
                fraction: 82,
            },
            Step::Modify {
                node: 5,
                path: 4,
                content: 4,
            },
            Step::Delete { node: 5, path: 5 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(1), None, Some(4), Some(4)],
            },
            Step::Modify {
                node: 5,
                path: 1,
                content: 4,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 4,
            },
            Step::Settle { secs: 35 },
            Step::Heal { a: 4, b: 7 },
            Step::User {
                node: 5,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 33,
                },
                delay_secs: 12,
            },
            Step::MassDelete {
                node: 7,
                fraction: 51,
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 4,
            },
            Step::Settle { secs: 35 },
            Step::Crash {
                node: 3,
                gap_secs: 49,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(3), Some(5), None, Some(1), Some(1), Some(3)],
            },
            Step::Heal { a: 2, b: 1 },
            Step::Chmod { node: 0, path: 1 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 23,
            },
            Step::Modify {
                node: 5,
                path: 6,
                content: 2,
            },
            Step::Offline { node: 5 },
            Step::Partition { a: 1, b: 3 },
            Step::Settle { secs: 12 },
            Step::Create {
                node: 7,
                path: 8,
                content: 1,
            },
            Step::Heal { a: 1, b: 3 },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 46,
            },
            Step::Settle { secs: 4 },
            Step::Delete { node: 4, path: 10 },
            Step::Modify {
                node: 4,
                path: 11,
                content: 1,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 43,
            },
            Step::Delete { node: 6, path: 3 },
            Step::Partition { a: 7, b: 0 },
            Step::Modify {
                node: 0,
                path: 5,
                content: 3,
            },
            Step::Tier {
                a: 5,
                b: 3,
                tier: 0,
            },
            Step::Chmod { node: 5, path: 10 },
            Step::MassDelete {
                node: 2,
                fraction: 92,
            },
            Step::Settle { secs: 12 },
            Step::Heal { a: 4, b: 0 },
            Step::Create {
                node: 6,
                path: 1,
                content: 4,
            },
            Step::Online { node: 3 },
        ],
    );
}

/// `revert` moved the file to trash and restored the announced record, and
/// the refetch's `Write` then expected that record's shape on disk; the
/// path was absent, so the guard failed, the want was deferred, the next
/// bracket end tombstoned the path, and everything that later arrived
/// there stayed deferred (quiescence). A restoring want's commit now
/// expects the path to be absent (§8.3 step 2).
/// Replayed as found: group commit alone loses its scenario (PR 1b).
#[test]
fn a_reverts_refetch_expects_the_path_it_trashed_to_be_absent() {
    passes_as_found(
        650,
        &Knobs::default(),
        &[
            Step::Heal { a: 2, b: 5 },
            Step::Settle { secs: 19 },
            Step::Heal { a: 2, b: 4 },
            Step::MassModify {
                node: 6,
                fraction: 74,
                content: 2,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 5,
            },
            Step::Crash {
                node: 0,
                gap_secs: 101,
            },
            Step::Create {
                node: 2,
                path: 11,
                content: 3,
            },
            Step::Rename {
                node: 1,
                from: 8,
                to: 9,
            },
            Step::Delete { node: 7, path: 4 },
            Step::Create {
                node: 0,
                path: 4,
                content: 3,
            },
            Step::Delete { node: 1, path: 8 },
            Step::Symlink {
                node: 7,
                path: 2,
                target: 6,
            },
            Step::Online { node: 3 },
            Step::Create {
                node: 4,
                path: 6,
                content: 1,
            },
            Step::Offline { node: 0 },
            Step::Settle { secs: 2 },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Chmod { node: 5, path: 9 },
            Step::Delete { node: 0, path: 4 },
            Step::Create {
                node: 7,
                path: 2,
                content: 1,
            },
            Step::Online { node: 4 },
            Step::Settle { secs: 21 },
            Step::Rename {
                node: 1,
                from: 0,
                to: 4,
            },
            Step::Partition { a: 7, b: 5 },
            Step::Rename {
                node: 5,
                from: 8,
                to: 1,
            },
            Step::Delete { node: 4, path: 4 },
            Step::Delete { node: 0, path: 6 },
            Step::Offline { node: 2 },
            Step::Offline { node: 7 },
            Step::Modify {
                node: 3,
                path: 9,
                content: 1,
            },
            Step::Create {
                node: 1,
                path: 8,
                content: 1,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(4), Some(5), Some(2), Some(1), Some(5)],
            },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 55,
            },
            Step::Crash {
                node: 2,
                gap_secs: 69,
            },
            Step::MassDelete {
                node: 1,
                fraction: 85,
            },
            Step::Modify {
                node: 0,
                path: 8,
                content: 4,
            },
            Step::Heal { a: 0, b: 3 },
            Step::Delete { node: 2, path: 11 },
            Step::Partition { a: 4, b: 2 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(3), Some(4)],
            },
            Step::Settle { secs: 15 },
            Step::Create {
                node: 6,
                path: 6,
                content: 1,
            },
            Step::Create {
                node: 2,
                path: 7,
                content: 5,
            },
            Step::Heal { a: 5, b: 2 },
            Step::Partition { a: 5, b: 7 },
            Step::Delete { node: 0, path: 3 },
            Step::Offline { node: 4 },
            Step::Create {
                node: 2,
                path: 8,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 0,
                content: 2,
            },
            Step::Heal { a: 1, b: 3 },
            Step::Create {
                node: 7,
                path: 9,
                content: 5,
            },
            Step::Symlink {
                node: 3,
                path: 4,
                target: 9,
            },
            Step::Modify {
                node: 1,
                path: 5,
                content: 3,
            },
            Step::Create {
                node: 2,
                path: 1,
                content: 3,
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Settle { secs: 32 },
            Step::Modify {
                node: 5,
                path: 0,
                content: 4,
            },
            Step::Modify {
                node: 3,
                path: 8,
                content: 2,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 32,
            },
            Step::Chmod { node: 6, path: 1 },
            Step::Modify {
                node: 0,
                path: 7,
                content: 5,
            },
            Step::Partition { a: 4, b: 4 },
            Step::Modify {
                node: 2,
                path: 8,
                content: 1,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 17,
            },
            Step::Settle { secs: 6 },
            Step::Offline { node: 5 },
            Step::Partition { a: 7, b: 5 },
            Step::Modify {
                node: 5,
                path: 9,
                content: 3,
            },
            Step::Create {
                node: 0,
                path: 6,
                content: 4,
            },
            Step::Modify {
                node: 1,
                path: 1,
                content: 2,
            },
            Step::Symlink {
                node: 7,
                path: 1,
                target: 9,
            },
            Step::Create {
                node: 4,
                path: 6,
                content: 2,
            },
            Step::Create {
                node: 4,
                path: 8,
                content: 4,
            },
            Step::Delete { node: 6, path: 6 },
            Step::Create {
                node: 6,
                path: 9,
                content: 3,
            },
            Step::Heal { a: 3, b: 0 },
            Step::Symlink {
                node: 6,
                path: 9,
                target: 0,
            },
            Step::Settle { secs: 1 },
            Step::Modify {
                node: 5,
                path: 0,
                content: 3,
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Modify {
                node: 5,
                path: 0,
                content: 5,
            },
            Step::Settle { secs: 33 },
            Step::Delete { node: 7, path: 6 },
            Step::Modify {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Crash {
                node: 5,
                gap_secs: 119,
            },
            Step::Touch { node: 6, path: 1 },
            Step::Delete { node: 6, path: 9 },
            Step::Heal { a: 4, b: 1 },
            Step::Modify {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Partition { a: 2, b: 6 },
            Step::Partition { a: 1, b: 4 },
            Step::Modify {
                node: 7,
                path: 7,
                content: 3,
            },
            Step::Delete { node: 6, path: 1 },
            Step::Online { node: 1 },
            Step::Rmdir { node: 3, dir: 0 },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 24,
            },
            Step::Modify {
                node: 2,
                path: 7,
                content: 5,
            },
            Step::Touch { node: 6, path: 0 },
            Step::Delete { node: 1, path: 9 },
            Step::Rmdir { node: 3, dir: 1 },
            Step::Online { node: 0 },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 35,
            },
            Step::Create {
                node: 1,
                path: 2,
                content: 4,
            },
            Step::Partition { a: 3, b: 5 },
            Step::Create {
                node: 6,
                path: 11,
                content: 4,
            },
            Step::Delete { node: 1, path: 1 },
            Step::Create {
                node: 5,
                path: 10,
                content: 2,
            },
            Step::Rmdir { node: 1, dir: 0 },
            Step::Partition { a: 2, b: 7 },
            Step::Offline { node: 3 },
            Step::Modify {
                node: 1,
                path: 6,
                content: 3,
            },
            Step::Create {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Create {
                node: 5,
                path: 0,
                content: 1,
            },
            Step::Settle { secs: 21 },
            Step::Create {
                node: 5,
                path: 1,
                content: 5,
            },
            Step::Settle { secs: 9 },
            Step::Modify {
                node: 6,
                path: 1,
                content: 5,
            },
            Step::Offline { node: 1 },
            Step::Mkdir { node: 2, dir: 2 },
            Step::Delete { node: 0, path: 3 },
            Step::Modify {
                node: 5,
                path: 6,
                content: 4,
            },
            Step::Offline { node: 2 },
            Step::Tier {
                a: 4,
                b: 5,
                tier: 0,
            },
            Step::Modify {
                node: 5,
                path: 2,
                content: 2,
            },
            Step::Everywhere {
                path: 7,
                contents: vec![None, None, Some(3), None, Some(3), Some(3)],
            },
            Step::Delete { node: 6, path: 2 },
            Step::Create {
                node: 6,
                path: 6,
                content: 4,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 49,
            },
            Step::Create {
                node: 3,
                path: 7,
                content: 2,
            },
            Step::Create {
                node: 6,
                path: 0,
                content: 5,
            },
            Step::Rmdir { node: 3, dir: 2 },
            Step::Crash {
                node: 6,
                gap_secs: 43,
            },
            Step::Modify {
                node: 7,
                path: 2,
                content: 2,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 24,
            },
            Step::Modify {
                node: 3,
                path: 6,
                content: 3,
            },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Heal { a: 2, b: 3 },
            Step::Rename {
                node: 5,
                from: 0,
                to: 1,
            },
            Step::Modify {
                node: 5,
                path: 4,
                content: 5,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 3,
            },
            Step::Delete { node: 2, path: 2 },
            Step::Delete { node: 7, path: 9 },
            Step::Heal { a: 4, b: 0 },
            Step::Rename {
                node: 6,
                from: 7,
                to: 3,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(1), Some(3), Some(1), Some(2), None, Some(3)],
            },
            Step::Heal { a: 1, b: 1 },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(4), Some(3), Some(5), Some(4), Some(5)],
            },
            Step::MassModify {
                node: 4,
                fraction: 87,
                content: 1,
            },
            Step::Delete { node: 5, path: 0 },
            Step::Chmod { node: 0, path: 7 },
            Step::Delete { node: 0, path: 1 },
            Step::Partition { a: 3, b: 4 },
            Step::Heal { a: 6, b: 6 },
            Step::Create {
                node: 6,
                path: 6,
                content: 3,
            },
            Step::Modify {
                node: 5,
                path: 10,
                content: 4,
            },
            Step::Touch { node: 5, path: 5 },
            Step::Modify {
                node: 5,
                path: 2,
                content: 4,
            },
            Step::Settle { secs: 5 },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Delete { node: 6, path: 3 },
            Step::Online { node: 5 },
            Step::Crash {
                node: 3,
                gap_secs: 109,
            },
            Step::Settle { secs: 32 },
            Step::Settle { secs: 16 },
            Step::Heal { a: 4, b: 0 },
            Step::Rmdir { node: 5, dir: 0 },
            Step::Chmod { node: 7, path: 10 },
            Step::Tier {
                a: 3,
                b: 0,
                tier: 2,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 44,
            },
            Step::Heal { a: 3, b: 6 },
            Step::Modify {
                node: 6,
                path: 6,
                content: 1,
            },
            Step::Mkdir { node: 3, dir: 1 },
            Step::Create {
                node: 6,
                path: 10,
                content: 4,
            },
            Step::Tier {
                a: 7,
                b: 2,
                tier: 1,
            },
            Step::Chmod { node: 4, path: 2 },
            Step::MassModify {
                node: 3,
                fraction: 62,
                content: 1,
            },
            Step::Modify {
                node: 6,
                path: 11,
                content: 4,
            },
            Step::Symlink {
                node: 5,
                path: 2,
                target: 2,
            },
            Step::Modify {
                node: 3,
                path: 3,
                content: 1,
            },
            Step::Tier {
                a: 2,
                b: 7,
                tier: 0,
            },
            Step::Modify {
                node: 5,
                path: 6,
                content: 2,
            },
            Step::Settle { secs: 4 },
            Step::Delete { node: 7, path: 3 },
            Step::User {
                node: 5,
                action: crate::UserAction::DenyAll,
                delay_secs: 38,
            },
            Step::Create {
                node: 6,
                path: 8,
                content: 3,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 1,
            },
            Step::Create {
                node: 4,
                path: 0,
                content: 3,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![None, Some(1), None, Some(5), Some(3)],
            },
            Step::Partition { a: 1, b: 4 },
            Step::Online { node: 4 },
            Step::Delete { node: 7, path: 9 },
            Step::Modify {
                node: 1,
                path: 4,
                content: 4,
            },
            Step::Modify {
                node: 6,
                path: 8,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 10,
                content: 3,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 41,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(4), Some(5), Some(2), Some(4)],
            },
            Step::User {
                node: 1,
                action: crate::UserAction::Revert,
                delay_secs: 51,
            },
            Step::Heal { a: 1, b: 6 },
            Step::Create {
                node: 5,
                path: 5,
                content: 2,
            },
            Step::Create {
                node: 7,
                path: 3,
                content: 3,
            },
            Step::MassModify {
                node: 7,
                fraction: 74,
                content: 4,
            },
            Step::Rename {
                node: 5,
                from: 11,
                to: 3,
            },
            Step::Partition { a: 6, b: 1 },
            Step::Tier {
                a: 2,
                b: 0,
                tier: 0,
            },
            Step::Delete { node: 2, path: 0 },
            Step::Symlink {
                node: 7,
                path: 1,
                target: 9,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 5,
            },
            Step::Rename {
                node: 0,
                from: 0,
                to: 1,
            },
            Step::Partition { a: 1, b: 1 },
            Step::Create {
                node: 1,
                path: 1,
                content: 3,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(3), None],
            },
            Step::Heal { a: 6, b: 7 },
            Step::Settle { secs: 30 },
            Step::Heal { a: 0, b: 4 },
            Step::Settle { secs: 38 },
            Step::Online { node: 3 },
            Step::Partition { a: 4, b: 1 },
            Step::Settle { secs: 6 },
            Step::Delete { node: 7, path: 6 },
            Step::Create {
                node: 0,
                path: 7,
                content: 2,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 57,
            },
            Step::Settle { secs: 30 },
            Step::MassDelete {
                node: 2,
                fraction: 87,
            },
        ],
    );
}

/// A remote version arrived at a path where this node had a local version
/// it had not yet announced, and its want became a conflict's `M` folding
/// that version in; `revert` then discarded it, and with it the path's
/// record. `M` stayed, describing a merge that never happened, and the
/// versions arriving behind it were deferred as concurrent with it and
/// never left (quiescence). Revert now re-derives the want from the entry
/// as received, and a deferred version that dominates it replaces it.
/// Re-pinned at seed 107, found in a 2,000-step run with the default knobs
/// and shrunk to 30 steps, after draft 54's refusal rules moved seed 1056's
/// history, and no seed of 0-9,999 at 400 steps reached it; with the fix
/// disabled it fails I1, two nodes holding different content at d2/f5
/// for good, where it used to fail quiescence.
#[test]
fn a_want_at_a_reverted_path_is_rederived_from_the_received_entry() {
    passes(
        107,
        &[
            Step::Rename {
                node: 1,
                from: 8,
                to: 8,
            },
            Step::MassDelete {
                node: 4,
                fraction: 85,
            },
            Step::Settle { secs: 4 },
            Step::Partition { a: 6, b: 6 },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(4), None, Some(2), None],
            },
            Step::Heal { a: 3, b: 0 },
            Step::Create {
                node: 0,
                path: 11,
                content: 4,
            },
            Step::Create {
                node: 6,
                path: 6,
                content: 5,
            },
            Step::Heal { a: 4, b: 7 },
            Step::Modify {
                node: 0,
                path: 3,
                content: 4,
            },
            Step::Offline { node: 6 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(3), Some(5)],
            },
            Step::Delete { node: 5, path: 3 },
            Step::Online { node: 0 },
            Step::Create {
                node: 1,
                path: 2,
                content: 4,
            },
            Step::Tier {
                a: 4,
                b: 5,
                tier: 1,
            },
            Step::Rename {
                node: 7,
                from: 0,
                to: 9,
            },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Create {
                node: 3,
                path: 4,
                content: 3,
            },
            Step::Create {
                node: 1,
                path: 5,
                content: 3,
            },
            Step::Tier {
                a: 4,
                b: 2,
                tier: 2,
            },
            Step::Settle { secs: 8 },
            Step::Online { node: 5 },
            Step::Heal { a: 2, b: 4 },
            Step::Create {
                node: 4,
                path: 4,
                content: 1,
            },
            Step::MassDelete {
                node: 0,
                fraction: 95,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 45,
            },
            Step::Create {
                node: 0,
                path: 6,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 3,
                content: 2,
            },
            Step::Partition { a: 6, b: 5 },
        ],
    );
}

/// A concurrent version arriving at a restored path became a conflict
/// merge that carried the mark, and its winning content was the restored
/// record's own, which the user had deleted: a local delete skips the
/// trash, so the content existed nowhere. The want did not ask every
/// member, so not all of them could refuse it; it went without source and
/// never settled, and the machine that reverted kept a record no peer
/// matched (I1). Every want at a marked path now asks every member, and
/// step 4 settles it once all refuse (§8.3). Pinned at seed 414 with the
/// default knobs, shrunk to 72 steps; with the fix disabled it fails I1 at
/// d0/f6.
#[test]
fn a_marked_want_whose_content_exists_nowhere_settles_unrecoverable() {
    passes(
        414,
        &[
            Step::Modify {
                node: 7,
                path: 5,
                content: 2,
            },
            Step::MassDelete {
                node: 4,
                fraction: 85,
            },
            Step::Heal { a: 7, b: 2 },
            Step::Delete { node: 1, path: 6 },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Chmod { node: 0, path: 7 },
            Step::Online { node: 1 },
            Step::Delete { node: 3, path: 7 },
            Step::Partition { a: 4, b: 5 },
            Step::Modify {
                node: 1,
                path: 5,
                content: 2,
            },
            Step::Modify {
                node: 5,
                path: 9,
                content: 5,
            },
            Step::Crash {
                node: 6,
                gap_secs: 56,
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(5), None, None, None, None],
            },
            Step::Delete { node: 3, path: 3 },
            Step::Mkdir { node: 0, dir: 1 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 30,
            },
            Step::Tier {
                a: 3,
                b: 3,
                tier: 0,
            },
            Step::Settle { secs: 10 },
            Step::Delete { node: 6, path: 4 },
            Step::Create {
                node: 3,
                path: 7,
                content: 1,
            },
            Step::Offline { node: 6 },
            Step::Delete { node: 7, path: 9 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 18,
            },
            Step::Mkdir { node: 4, dir: 1 },
            Step::Crash {
                node: 7,
                gap_secs: 25,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 21,
            },
            Step::Modify {
                node: 4,
                path: 6,
                content: 3,
            },
            Step::Crash {
                node: 1,
                gap_secs: 89,
            },
            Step::Rename {
                node: 5,
                from: 3,
                to: 5,
            },
            Step::Delete { node: 6, path: 9 },
            Step::Modify {
                node: 7,
                path: 11,
                content: 3,
            },
            Step::Modify {
                node: 1,
                path: 2,
                content: 3,
            },
            Step::Rename {
                node: 0,
                from: 10,
                to: 7,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 26,
            },
            Step::Create {
                node: 4,
                path: 6,
                content: 1,
            },
            Step::Settle { secs: 33 },
            Step::Touch { node: 0, path: 1 },
            Step::Offline { node: 7 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 23,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 28,
            },
            Step::Settle { secs: 6 },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(4), Some(1), Some(3), Some(3), None, Some(1)],
            },
            Step::Create {
                node: 1,
                path: 9,
                content: 2,
            },
            Step::MassModify {
                node: 4,
                fraction: 50,
                content: 4,
            },
            Step::Modify {
                node: 2,
                path: 5,
                content: 1,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::DenyAll,
                delay_secs: 30,
            },
            Step::Settle { secs: 1 },
            Step::Settle { secs: 35 },
            Step::Heal { a: 6, b: 7 },
            Step::Delete { node: 0, path: 8 },
            Step::User {
                node: 1,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 18,
                },
                delay_secs: 41,
            },
            Step::Rmdir { node: 2, dir: 2 },
            Step::User {
                node: 1,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 35,
                },
                delay_secs: 49,
            },
            Step::Chmod { node: 0, path: 8 },
            Step::User {
                node: 0,
                action: crate::UserAction::Revert,
                delay_secs: 48,
            },
            Step::Create {
                node: 7,
                path: 8,
                content: 5,
            },
            Step::Online { node: 1 },
            Step::MassDelete {
                node: 7,
                fraction: 86,
            },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Chmod { node: 2, path: 7 },
            Step::Settle { secs: 22 },
            Step::Settle { secs: 3 },
            Step::User {
                node: 1,
                action: crate::UserAction::Revert,
                delay_secs: 0,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 10,
            },
            Step::MassModify {
                node: 7,
                fraction: 67,
                content: 2,
            },
            Step::Partition { a: 0, b: 4 },
            Step::Create {
                node: 3,
                path: 1,
                content: 5,
            },
            Step::Everywhere {
                path: 7,
                contents: vec![Some(4), Some(3), None, None, Some(4), Some(4)],
            },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Touch { node: 5, path: 5 },
            Step::Touch { node: 4, path: 5 },
            Step::Delete { node: 2, path: 0 },
        ],
    );
}

/// A winning tombstone moved a node's adopted file to a conflict copy
/// (§7.6), and the node's user then removed the copy's directory. I2 looked
/// for a user edit at the path the content was adopted at, not where sync
/// had put it, and called the user's own deletion a loss. The adoption now
/// follows the content to the copy (§14.1 I2). Re-pinned at seed 3532 with
/// the default knobs, shrunk to 58 steps, after drafts 52 and 53's failed-report rules
/// moved seed 3591's history: a seed that fails I2 with the patch because
/// an `Rmdir` removes the copy, and keeps doing so shrunk, the copy a
/// file's. With `follow` disabled it fails I2 (content 96e1768f, adopted at
/// d1/f10, in neither folder nor trash).
/// The fix's unit test guards it whether or not a seed reaches it:
/// `sim::tests::an_adoption_follows_its_content_to_the_conflict_copy`.
#[test]
fn a_conflict_copy_its_user_removed_with_its_directory_is_not_lost() {
    passes(
        3532,
        &[
            Step::Heal { a: 5, b: 2 },
            Step::Partition { a: 3, b: 3 },
            Step::Delete { node: 3, path: 1 },
            Step::Offline { node: 2 },
            Step::Touch { node: 1, path: 9 },
            Step::Chmod { node: 7, path: 0 },
            Step::Settle { secs: 37 },
            Step::Tier {
                a: 2,
                b: 6,
                tier: 1,
            },
            Step::Create {
                node: 1,
                path: 5,
                content: 2,
            },
            Step::Crash {
                node: 2,
                gap_secs: 22,
            },
            Step::Create {
                node: 3,
                path: 9,
                content: 2,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(4), Some(1)],
            },
            Step::Create {
                node: 3,
                path: 9,
                content: 1,
            },
            Step::Settle { secs: 24 },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 26,
            },
            Step::Settle { secs: 11 },
            Step::Delete { node: 1, path: 0 },
            Step::Heal { a: 3, b: 0 },
            Step::Create {
                node: 1,
                path: 9,
                content: 1,
            },
            Step::Rename {
                node: 7,
                from: 11,
                to: 11,
            },
            Step::Rename {
                node: 3,
                from: 3,
                to: 9,
            },
            Step::MassDelete {
                node: 7,
                fraction: 89,
            },
            Step::Create {
                node: 6,
                path: 11,
                content: 2,
            },
            Step::Settle { secs: 19 },
            Step::MassDelete {
                node: 7,
                fraction: 75,
            },
            Step::Create {
                node: 3,
                path: 10,
                content: 3,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(4), Some(5), Some(2), Some(1)],
            },
            Step::Touch { node: 4, path: 0 },
            Step::Partition { a: 7, b: 4 },
            Step::Modify {
                node: 0,
                path: 9,
                content: 5,
            },
            Step::Delete { node: 0, path: 3 },
            Step::Create {
                node: 7,
                path: 10,
                content: 2,
            },
            Step::Settle { secs: 24 },
            Step::Tier {
                a: 2,
                b: 7,
                tier: 0,
            },
            Step::Partition { a: 7, b: 6 },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Tier {
                a: 7,
                b: 4,
                tier: 1,
            },
            Step::Rmdir { node: 3, dir: 2 },
            Step::Online { node: 1 },
            Step::Delete { node: 7, path: 10 },
            Step::Chmod { node: 4, path: 2 },
            Step::Settle { secs: 32 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 44,
            },
            Step::Online { node: 2 },
            Step::Heal { a: 2, b: 7 },
            Step::Rename {
                node: 3,
                from: 11,
                to: 8,
            },
            Step::Rename {
                node: 0,
                from: 6,
                to: 1,
            },
            Step::Settle { secs: 8 },
            Step::Create {
                node: 5,
                path: 0,
                content: 1,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 29,
                },
                delay_secs: 3,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 24,
            },
            Step::Online { node: 3 },
            Step::Symlink {
                node: 7,
                path: 7,
                target: 1,
            },
            Step::Rmdir { node: 3, dir: 1 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 32,
            },
            Step::Chmod { node: 2, path: 2 },
            Step::Modify {
                node: 7,
                path: 11,
                content: 4,
            },
            Step::MassDelete {
                node: 7,
                fraction: 69,
            },
        ],
    );
}

/// The same, with an adopted file that sync moved to a conflict copy, and
/// the user's mass delete then removed. Re-pinned at seed 204 with the
/// default knobs, shrunk to 106 steps, after drafts 52 and 53's failed-report rules
/// moved seed 1284's history: a seed whose copy a mass delete removes,
/// the file itself rather than one moved with its directory, and does so
/// shrunk. With `follow` disabled it fails I2 (content 96e1768f, adopted at
/// d2/f5, in neither folder nor trash).
#[test]
fn a_conflict_copy_its_user_mass_deleted_is_not_lost() {
    passes(
        204,
        &[
            Step::Modify {
                node: 0,
                path: 5,
                content: 4,
            },
            Step::Create {
                node: 1,
                path: 5,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 5,
                content: 4,
            },
            Step::Rename {
                node: 3,
                from: 9,
                to: 4,
            },
            Step::Partition { a: 1, b: 1 },
            Step::Modify {
                node: 3,
                path: 7,
                content: 2,
            },
            Step::Crash {
                node: 4,
                gap_secs: 63,
            },
            Step::Crash {
                node: 6,
                gap_secs: 3,
            },
            Step::Partition { a: 3, b: 5 },
            Step::Chmod { node: 0, path: 4 },
            Step::Partition { a: 0, b: 0 },
            Step::Settle { secs: 22 },
            Step::Offline { node: 7 },
            Step::Chmod { node: 2, path: 11 },
            Step::Chmod { node: 2, path: 1 },
            Step::Create {
                node: 7,
                path: 10,
                content: 1,
            },
            Step::Heal { a: 2, b: 5 },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(5), Some(4), None, Some(2), Some(4), Some(3), Some(2)],
            },
            Step::Modify {
                node: 2,
                path: 4,
                content: 1,
            },
            Step::Offline { node: 5 },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 41,
            },
            Step::Partition { a: 2, b: 6 },
            Step::Modify {
                node: 0,
                path: 4,
                content: 2,
            },
            Step::Chmod { node: 2, path: 6 },
            Step::Delete { node: 1, path: 8 },
            Step::Create {
                node: 7,
                path: 10,
                content: 5,
            },
            Step::Delete { node: 4, path: 6 },
            Step::Create {
                node: 4,
                path: 0,
                content: 3,
            },
            Step::Crash {
                node: 7,
                gap_secs: 12,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 1,
            },
            Step::Rmdir { node: 5, dir: 2 },
            Step::Heal { a: 5, b: 1 },
            Step::Settle { secs: 20 },
            Step::Rename {
                node: 3,
                from: 9,
                to: 3,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(2), Some(3), Some(2), None, Some(4), Some(3)],
            },
            Step::Symlink {
                node: 6,
                path: 3,
                target: 3,
            },
            Step::Create {
                node: 0,
                path: 10,
                content: 1,
            },
            Step::Modify {
                node: 3,
                path: 3,
                content: 1,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(4), None],
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(1), Some(2), Some(1), None, Some(3), Some(1)],
            },
            Step::Delete { node: 4, path: 11 },
            Step::Create {
                node: 2,
                path: 0,
                content: 1,
            },
            Step::MassDelete {
                node: 6,
                fraction: 59,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 10,
            },
            Step::Settle { secs: 7 },
            Step::MassModify {
                node: 1,
                fraction: 98,
                content: 3,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(5), Some(5), Some(4), None],
            },
            Step::Everywhere {
                path: 10,
                contents: vec![None, Some(1), None],
            },
            Step::Modify {
                node: 2,
                path: 2,
                content: 2,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::Revert,
                delay_secs: 44,
            },
            Step::Tier {
                a: 6,
                b: 6,
                tier: 2,
            },
            Step::Settle { secs: 30 },
            Step::Touch { node: 5, path: 6 },
            Step::Crash {
                node: 7,
                gap_secs: 40,
            },
            Step::Delete { node: 7, path: 10 },
            Step::Chmod { node: 0, path: 4 },
            Step::Create {
                node: 4,
                path: 8,
                content: 2,
            },
            Step::Delete { node: 4, path: 5 },
            Step::Touch { node: 0, path: 6 },
            Step::Heal { a: 1, b: 2 },
            Step::Rename {
                node: 1,
                from: 0,
                to: 5,
            },
            Step::Modify {
                node: 3,
                path: 6,
                content: 2,
            },
            Step::Modify {
                node: 3,
                path: 2,
                content: 3,
            },
            Step::Modify {
                node: 0,
                path: 0,
                content: 2,
            },
            Step::Online { node: 0 },
            Step::Create {
                node: 2,
                path: 4,
                content: 5,
            },
            Step::Rmdir { node: 4, dir: 1 },
            Step::Touch { node: 3, path: 9 },
            Step::Partition { a: 0, b: 3 },
            Step::Symlink {
                node: 1,
                path: 0,
                target: 7,
            },
            Step::Settle { secs: 21 },
            Step::MassModify {
                node: 5,
                fraction: 79,
                content: 5,
            },
            Step::Online { node: 2 },
            Step::Modify {
                node: 0,
                path: 4,
                content: 1,
            },
            Step::Rename {
                node: 6,
                from: 7,
                to: 8,
            },
            Step::Online { node: 7 },
            Step::Heal { a: 5, b: 0 },
            Step::Online { node: 3 },
            Step::User {
                node: 2,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 33,
                },
                delay_secs: 8,
            },
            Step::Tier {
                a: 4,
                b: 1,
                tier: 1,
            },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Symlink {
                node: 4,
                path: 6,
                target: 11,
            },
            Step::MassDelete {
                node: 6,
                fraction: 62,
            },
            Step::Modify {
                node: 5,
                path: 5,
                content: 3,
            },
            Step::Mkdir { node: 6, dir: 0 },
            Step::Modify {
                node: 0,
                path: 11,
                content: 3,
            },
            Step::Create {
                node: 6,
                path: 6,
                content: 1,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::Revert,
                delay_secs: 42,
            },
            Step::Online { node: 2 },
            Step::Heal { a: 3, b: 3 },
            Step::Symlink {
                node: 7,
                path: 9,
                target: 9,
            },
            Step::Heal { a: 3, b: 7 },
            Step::Delete { node: 0, path: 3 },
            Step::Settle { secs: 32 },
            Step::MassModify {
                node: 1,
                fraction: 62,
                content: 3,
            },
            Step::Online { node: 5 },
            Step::Touch { node: 0, path: 3 },
            Step::Rmdir { node: 3, dir: 0 },
            Step::Mkdir { node: 4, dir: 1 },
            Step::Settle { secs: 15 },
            Step::Crash {
                node: 4,
                gap_secs: 102,
            },
            Step::Delete { node: 0, path: 0 },
            Step::Modify {
                node: 6,
                path: 11,
                content: 5,
            },
            Step::Partition { a: 1, b: 1 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 49,
            },
            Step::Heal { a: 5, b: 3 },
        ],
    );
}

/// The same, with the copy rewritten by the user's mass modify. Re-pinned
/// at seed 2456 with the default knobs, shrunk to 79 steps, after
/// draft 54's refusal rules moved seed 4531's history: of the seeds of 0-4999
/// that fail I2 with the patch, the one whose copy a mass modify
/// rewrites, and does so shrunk. With `follow` disabled it fails I2
/// (content e39ce9da, adopted at d0/f6 and moved to its conflict copy, in
/// neither folder nor trash).
#[test]
fn a_conflict_copy_its_user_rewrote_is_not_lost() {
    passes(
        2456,
        &[
            Step::Create {
                node: 7,
                path: 11,
                content: 1,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 22,
            },
            Step::Crash {
                node: 4,
                gap_secs: 35,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(2), Some(1), Some(5), Some(1)],
            },
            Step::Mkdir { node: 4, dir: 2 },
            Step::Symlink {
                node: 1,
                path: 10,
                target: 5,
            },
            Step::Delete { node: 1, path: 7 },
            Step::Modify {
                node: 4,
                path: 3,
                content: 5,
            },
            Step::Online { node: 7 },
            Step::MassModify {
                node: 7,
                fraction: 82,
                content: 1,
            },
            Step::Chmod { node: 6, path: 11 },
            Step::Create {
                node: 2,
                path: 11,
                content: 1,
            },
            Step::Mkdir { node: 5, dir: 0 },
            Step::MassModify {
                node: 0,
                fraction: 89,
                content: 3,
            },
            Step::Create {
                node: 6,
                path: 7,
                content: 2,
            },
            Step::Heal { a: 4, b: 2 },
            Step::Chmod { node: 2, path: 11 },
            Step::Delete { node: 5, path: 11 },
            Step::Modify {
                node: 2,
                path: 2,
                content: 4,
            },
            Step::Create {
                node: 7,
                path: 4,
                content: 1,
            },
            Step::Settle { secs: 36 },
            Step::Delete { node: 3, path: 6 },
            Step::Tier {
                a: 5,
                b: 2,
                tier: 2,
            },
            Step::Symlink {
                node: 6,
                path: 1,
                target: 11,
            },
            Step::Mkdir { node: 3, dir: 2 },
            Step::Delete { node: 0, path: 8 },
            Step::Chmod { node: 7, path: 6 },
            Step::Mkdir { node: 4, dir: 2 },
            Step::MassModify {
                node: 7,
                fraction: 80,
                content: 2,
            },
            Step::Touch { node: 0, path: 8 },
            Step::User {
                node: 7,
                action: crate::UserAction::ApproveAll,
                delay_secs: 20,
            },
            Step::Chmod { node: 5, path: 11 },
            Step::Create {
                node: 6,
                path: 3,
                content: 5,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![None, None, Some(2), Some(3)],
            },
            Step::Rename {
                node: 0,
                from: 7,
                to: 1,
            },
            Step::Rmdir { node: 4, dir: 1 },
            Step::Create {
                node: 3,
                path: 5,
                content: 1,
            },
            Step::Modify {
                node: 4,
                path: 5,
                content: 3,
            },
            Step::Delete { node: 0, path: 0 },
            Step::Heal { a: 2, b: 7 },
            Step::Modify {
                node: 1,
                path: 2,
                content: 4,
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 43,
            },
            Step::Create {
                node: 0,
                path: 4,
                content: 2,
            },
            Step::Crash {
                node: 7,
                gap_secs: 112,
            },
            Step::Create {
                node: 2,
                path: 6,
                content: 4,
            },
            Step::Mkdir { node: 5, dir: 1 },
            Step::Settle { secs: 39 },
            Step::User {
                node: 5,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 29,
                },
                delay_secs: 6,
            },
            Step::Chmod { node: 7, path: 4 },
            Step::Delete { node: 5, path: 10 },
            Step::Symlink {
                node: 1,
                path: 5,
                target: 7,
            },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Partition { a: 1, b: 5 },
            Step::Create {
                node: 4,
                path: 8,
                content: 4,
            },
            Step::Delete { node: 5, path: 1 },
            Step::MassDelete {
                node: 0,
                fraction: 54,
            },
            Step::Heal { a: 2, b: 5 },
            Step::Touch { node: 4, path: 6 },
            Step::Rmdir { node: 6, dir: 0 },
            Step::Create {
                node: 3,
                path: 6,
                content: 4,
            },
            Step::Online { node: 2 },
            Step::Tier {
                a: 2,
                b: 2,
                tier: 1,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(4), Some(2)],
            },
            Step::Touch { node: 1, path: 9 },
            Step::Modify {
                node: 4,
                path: 1,
                content: 3,
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 4,
            },
            Step::MassDelete {
                node: 2,
                fraction: 68,
            },
            Step::Create {
                node: 5,
                path: 3,
                content: 4,
            },
            Step::Modify {
                node: 5,
                path: 10,
                content: 5,
            },
            Step::Heal { a: 5, b: 6 },
            Step::Online { node: 4 },
            Step::Everywhere {
                path: 1,
                contents: vec![None, Some(5), Some(4), None, Some(4), None, None],
            },
            Step::Crash {
                node: 1,
                gap_secs: 6,
            },
            Step::Settle { secs: 36 },
            Step::MassDelete {
                node: 5,
                fraction: 58,
            },
            Step::Mkdir { node: 4, dir: 1 },
            Step::Heal { a: 1, b: 2 },
            Step::Create {
                node: 7,
                path: 5,
                content: 5,
            },
            Step::MassModify {
                node: 0,
                fraction: 94,
                content: 5,
            },
        ],
    );
}

/// Class A. A paused node held a peer's tombstone; its user denied, then
/// reverted. The deny's bump joined the pending batch, the revert
/// discarded it with the quarantine it had consumed, and the tombstone
/// was lost (I1). A deny now waits while the folder is paused, and a
/// revert that discards a deny returns its held item (§8.3): the pin fails
/// with both off, and either one alone keeps it.
/// Replayed as found: group commit alone loses its scenario (PR 1b).
#[test]
fn a_deny_on_a_paused_folder_waits_instead_of_joining_the_pending_batch() {
    passes_as_found(
        12922,
        &Knobs::default(),
        &[
            Step::Chmod { node: 1, path: 6 },
            Step::Create {
                node: 4,
                path: 8,
                content: 5,
            },
            Step::Delete { node: 7, path: 7 },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 14,
                },
                delay_secs: 17,
            },
            Step::Tier {
                a: 1,
                b: 2,
                tier: 0,
            },
            Step::Modify {
                node: 1,
                path: 5,
                content: 2,
            },
            Step::Partition { a: 2, b: 7 },
            Step::Partition { a: 2, b: 1 },
            Step::Offline { node: 1 },
            Step::Delete { node: 0, path: 8 },
            Step::Delete { node: 0, path: 7 },
            Step::Heal { a: 5, b: 1 },
            Step::Offline { node: 7 },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 33,
            },
            Step::Create {
                node: 1,
                path: 8,
                content: 1,
            },
            Step::Delete { node: 5, path: 2 },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 46,
            },
            Step::Modify {
                node: 6,
                path: 8,
                content: 5,
            },
            Step::Online { node: 4 },
            Step::Heal { a: 3, b: 2 },
            Step::Heal { a: 3, b: 2 },
            Step::Rename {
                node: 3,
                from: 4,
                to: 8,
            },
            Step::Online { node: 1 },
            Step::Settle { secs: 8 },
            Step::Delete { node: 3, path: 9 },
            Step::Heal { a: 5, b: 0 },
            Step::User {
                node: 6,
                action: crate::UserAction::Rules {
                    hold_count: 3,
                    hold_pct: 49,
                },
                delay_secs: 16,
            },
            Step::Create {
                node: 1,
                path: 7,
                content: 2,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Rules {
                    hold_count: 1,
                    hold_pct: 16,
                },
                delay_secs: 8,
            },
            Step::Create {
                node: 5,
                path: 10,
                content: 5,
            },
            Step::Settle { secs: 39 },
            Step::MassDelete {
                node: 3,
                fraction: 85,
            },
            Step::Partition { a: 7, b: 3 },
            Step::Rename {
                node: 2,
                from: 6,
                to: 0,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 59,
            },
            Step::Create {
                node: 6,
                path: 2,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 11,
                content: 4,
            },
            Step::Settle { secs: 2 },
            Step::Delete { node: 2, path: 8 },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 49,
            },
            Step::Modify {
                node: 2,
                path: 1,
                content: 4,
            },
            Step::Heal { a: 2, b: 2 },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 23,
            },
            Step::Heal { a: 4, b: 7 },
        ],
    );
}

/// Class C. A revert queued before a crash ran at the `ScanStarted` of the
/// restart's scan and removed records the host had already listed for the
/// bracket, which then reported `Unchanged` for a path with no record. A
/// revert now waits for the startup scan to finish, and no decision runs
/// inside an open bracket (§8.3): the pin fails with both off. Re-pinned
/// at seed 1031 with the default knobs, shrunk to 196 steps, after
/// drafts 52 and 53's failed-report rules moved seed 26's history; with both waits
/// disabled it fails as a host bug, `Unchanged` reported for a conflict
/// copy of d1/f10 with no live record.
#[test]
fn a_queued_revert_waits_for_the_startup_scan_to_finish() {
    passes(
        1031,
        &[
            Step::Touch { node: 5, path: 3 },
            Step::Touch { node: 7, path: 3 },
            Step::Rename {
                node: 1,
                from: 9,
                to: 1,
            },
            Step::Rename {
                node: 2,
                from: 1,
                to: 0,
            },
            Step::Modify {
                node: 6,
                path: 2,
                content: 4,
            },
            Step::Rename {
                node: 6,
                from: 2,
                to: 8,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(1), Some(3)],
            },
            Step::MassModify {
                node: 7,
                fraction: 80,
                content: 3,
            },
            Step::Partition { a: 1, b: 4 },
            Step::Heal { a: 6, b: 5 },
            Step::Rename {
                node: 4,
                from: 0,
                to: 3,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(3), Some(4), Some(2), Some(4), Some(5)],
            },
            Step::Rename {
                node: 3,
                from: 7,
                to: 7,
            },
            Step::Modify {
                node: 3,
                path: 5,
                content: 3,
            },
            Step::Rename {
                node: 3,
                from: 0,
                to: 4,
            },
            Step::Crash {
                node: 4,
                gap_secs: 36,
            },
            Step::Offline { node: 2 },
            Step::Touch { node: 6, path: 7 },
            Step::Delete { node: 5, path: 1 },
            Step::Heal { a: 3, b: 4 },
            Step::Tier {
                a: 1,
                b: 2,
                tier: 2,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(5), Some(3), Some(3), Some(3)],
            },
            Step::Partition { a: 4, b: 5 },
            Step::Heal { a: 2, b: 2 },
            Step::Offline { node: 4 },
            Step::Modify {
                node: 6,
                path: 4,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 11,
                content: 3,
            },
            Step::Offline { node: 5 },
            Step::Create {
                node: 5,
                path: 9,
                content: 2,
            },
            Step::Online { node: 4 },
            Step::Create {
                node: 1,
                path: 7,
                content: 1,
            },
            Step::Online { node: 6 },
            Step::Create {
                node: 4,
                path: 11,
                content: 4,
            },
            Step::Tier {
                a: 2,
                b: 1,
                tier: 1,
            },
            Step::Online { node: 0 },
            Step::Modify {
                node: 4,
                path: 8,
                content: 2,
            },
            Step::Mkdir { node: 4, dir: 0 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(2), Some(1), Some(2), None, None, Some(5), None],
            },
            Step::User {
                node: 2,
                action: crate::UserAction::Revert,
                delay_secs: 25,
            },
            Step::Offline { node: 0 },
            Step::Online { node: 3 },
            Step::Modify {
                node: 6,
                path: 8,
                content: 1,
            },
            Step::Settle { secs: 34 },
            Step::Delete { node: 5, path: 9 },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 56,
            },
            Step::Partition { a: 3, b: 3 },
            Step::Offline { node: 0 },
            Step::Offline { node: 5 },
            Step::MassDelete {
                node: 7,
                fraction: 58,
            },
            Step::Crash {
                node: 7,
                gap_secs: 56,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 9,
            },
            Step::Create {
                node: 5,
                path: 9,
                content: 3,
            },
            Step::Settle { secs: 26 },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 1,
            },
            Step::Heal { a: 5, b: 1 },
            Step::Touch { node: 6, path: 7 },
            Step::Rename {
                node: 0,
                from: 5,
                to: 0,
            },
            Step::Offline { node: 7 },
            Step::Delete { node: 6, path: 1 },
            Step::Create {
                node: 6,
                path: 3,
                content: 5,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(5), Some(5), Some(1), Some(4), None],
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 9,
            },
            Step::Delete { node: 2, path: 2 },
            Step::Modify {
                node: 0,
                path: 1,
                content: 4,
            },
            Step::Partition { a: 1, b: 4 },
            Step::Delete { node: 1, path: 8 },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Touch { node: 1, path: 8 },
            Step::Create {
                node: 4,
                path: 6,
                content: 1,
            },
            Step::Create {
                node: 1,
                path: 9,
                content: 3,
            },
            Step::Create {
                node: 4,
                path: 2,
                content: 3,
            },
            Step::Heal { a: 3, b: 5 },
            Step::Create {
                node: 1,
                path: 4,
                content: 5,
            },
            Step::Create {
                node: 2,
                path: 10,
                content: 3,
            },
            Step::MassDelete {
                node: 0,
                fraction: 58,
            },
            Step::Heal { a: 1, b: 0 },
            Step::Settle { secs: 12 },
            Step::Delete { node: 6, path: 11 },
            Step::Offline { node: 2 },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 3,
            },
            Step::Create {
                node: 7,
                path: 2,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 11,
                content: 4,
            },
            Step::Settle { secs: 21 },
            Step::Modify {
                node: 0,
                path: 4,
                content: 5,
            },
            Step::Heal { a: 3, b: 5 },
            Step::Modify {
                node: 0,
                path: 6,
                content: 2,
            },
            Step::Modify {
                node: 4,
                path: 0,
                content: 5,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::DenyAll,
                delay_secs: 13,
            },
            Step::Heal { a: 1, b: 2 },
            Step::Rename {
                node: 2,
                from: 9,
                to: 5,
            },
            Step::Symlink {
                node: 0,
                path: 6,
                target: 11,
            },
            Step::Online { node: 3 },
            Step::Mkdir { node: 3, dir: 2 },
            Step::Create {
                node: 3,
                path: 4,
                content: 3,
            },
            Step::MassModify {
                node: 5,
                fraction: 85,
                content: 2,
            },
            Step::Modify {
                node: 7,
                path: 11,
                content: 4,
            },
            Step::Create {
                node: 2,
                path: 10,
                content: 2,
            },
            Step::Partition { a: 5, b: 3 },
            Step::Touch { node: 5, path: 8 },
            Step::Partition { a: 7, b: 6 },
            Step::User {
                node: 2,
                action: crate::UserAction::DenyAll,
                delay_secs: 43,
            },
            Step::Modify {
                node: 7,
                path: 0,
                content: 1,
            },
            Step::Create {
                node: 3,
                path: 4,
                content: 3,
            },
            Step::Rmdir { node: 4, dir: 2 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 3,
            },
            Step::Touch { node: 7, path: 8 },
            Step::Modify {
                node: 3,
                path: 7,
                content: 5,
            },
            Step::Settle { secs: 23 },
            Step::Rmdir { node: 1, dir: 2 },
            Step::MassModify {
                node: 1,
                fraction: 68,
                content: 5,
            },
            Step::Partition { a: 3, b: 2 },
            Step::Chmod { node: 3, path: 7 },
            Step::Partition { a: 6, b: 2 },
            Step::Delete { node: 0, path: 7 },
            Step::Everywhere {
                path: 1,
                contents: vec![None, Some(3), Some(4), Some(3), Some(2)],
            },
            Step::Modify {
                node: 1,
                path: 2,
                content: 1,
            },
            Step::Modify {
                node: 0,
                path: 3,
                content: 5,
            },
            Step::Create {
                node: 3,
                path: 3,
                content: 3,
            },
            Step::Settle { secs: 9 },
            Step::Modify {
                node: 5,
                path: 9,
                content: 4,
            },
            Step::Create {
                node: 7,
                path: 0,
                content: 2,
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 2,
            },
            Step::Crash {
                node: 1,
                gap_secs: 104,
            },
            Step::Settle { secs: 34 },
            Step::Modify {
                node: 0,
                path: 6,
                content: 1,
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 1,
            },
            Step::Create {
                node: 2,
                path: 2,
                content: 3,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![None, Some(5), Some(1)],
            },
            Step::Delete { node: 7, path: 1 },
            Step::Rename {
                node: 5,
                from: 0,
                to: 1,
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 2,
            },
            Step::Delete { node: 3, path: 2 },
            Step::User {
                node: 4,
                action: crate::UserAction::DenyAll,
                delay_secs: 44,
            },
            Step::Heal { a: 7, b: 7 },
            Step::Modify {
                node: 4,
                path: 8,
                content: 3,
            },
            Step::Modify {
                node: 6,
                path: 2,
                content: 1,
            },
            Step::Online { node: 2 },
            Step::Create {
                node: 3,
                path: 5,
                content: 3,
            },
            Step::Symlink {
                node: 4,
                path: 0,
                target: 11,
            },
            Step::Modify {
                node: 4,
                path: 9,
                content: 4,
            },
            Step::Rename {
                node: 7,
                from: 5,
                to: 9,
            },
            Step::Delete { node: 3, path: 11 },
            Step::Online { node: 1 },
            Step::Heal { a: 0, b: 3 },
            Step::Create {
                node: 6,
                path: 1,
                content: 1,
            },
            Step::Rename {
                node: 7,
                from: 10,
                to: 8,
            },
            Step::Heal { a: 2, b: 0 },
            Step::Online { node: 4 },
            Step::Online { node: 0 },
            Step::Heal { a: 2, b: 7 },
            Step::Crash {
                node: 6,
                gap_secs: 74,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 31,
            },
            Step::Create {
                node: 6,
                path: 1,
                content: 1,
            },
            Step::Touch { node: 2, path: 7 },
            Step::Online { node: 0 },
            Step::MassModify {
                node: 4,
                fraction: 68,
                content: 1,
            },
            Step::Settle { secs: 35 },
            Step::Create {
                node: 1,
                path: 8,
                content: 3,
            },
            Step::Delete { node: 6, path: 7 },
            Step::Online { node: 5 },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 3,
            },
            Step::Online { node: 3 },
            Step::Delete { node: 5, path: 11 },
            Step::Rename {
                node: 4,
                from: 1,
                to: 4,
            },
            Step::Symlink {
                node: 2,
                path: 3,
                target: 8,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::Revert,
                delay_secs: 10,
            },
            Step::Rename {
                node: 2,
                from: 3,
                to: 2,
            },
            Step::Create {
                node: 6,
                path: 0,
                content: 4,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Heal { a: 6, b: 5 },
            Step::Create {
                node: 2,
                path: 11,
                content: 1,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(2), Some(3), Some(4), Some(2), Some(1), Some(3)],
            },
            Step::Partition { a: 0, b: 0 },
            Step::Online { node: 4 },
            Step::Modify {
                node: 5,
                path: 11,
                content: 1,
            },
            Step::Touch { node: 4, path: 9 },
            Step::Chmod { node: 6, path: 10 },
            Step::Settle { secs: 15 },
            Step::Create {
                node: 6,
                path: 8,
                content: 4,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 47,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 2,
            },
            Step::Heal { a: 2, b: 2 },
            Step::User {
                node: 4,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 3,
                },
                delay_secs: 58,
            },
            Step::Rename {
                node: 3,
                from: 7,
                to: 5,
            },
            Step::Partition { a: 5, b: 1 },
            Step::Crash {
                node: 6,
                gap_secs: 51,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(4), Some(2)],
            },
            Step::Touch { node: 7, path: 9 },
            Step::Crash {
                node: 4,
                gap_secs: 95,
            },
            Step::Create {
                node: 0,
                path: 4,
                content: 3,
            },
            Step::Heal { a: 6, b: 0 },
            Step::MassModify {
                node: 4,
                fraction: 88,
                content: 1,
            },
            Step::Settle { secs: 36 },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(5), None, Some(1), Some(2), None, Some(3)],
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 17,
            },
        ],
    );
}

/// Class A, the case a paused-only wait misses. The node was offline, so
/// its window did not tick; its user's deny ran on the unpaused folder,
/// and when the node came back the first tick paused on three local
/// changes with the deny's bumps in the pending batch. The user's revert
/// discarded them and the quarantine they had consumed (I1). revert now
/// returns the held item (§8.3). Shrunk before that; fails without it.
/// Replayed as found: group commit alone loses its scenario (PR 1b).
#[test]
fn a_revert_that_discards_a_denys_bumps_returns_its_held_item() {
    passes_as_found(
        17187,
        &Knobs::default(),
        &[
            Step::Partition { a: 4, b: 3 },
            Step::Offline { node: 3 },
            Step::Modify {
                node: 5,
                path: 5,
                content: 1,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 35,
            },
            Step::Delete { node: 0, path: 3 },
            Step::Mkdir { node: 5, dir: 1 },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Delete { node: 4, path: 9 },
            Step::Modify {
                node: 5,
                path: 2,
                content: 3,
            },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Modify {
                node: 2,
                path: 6,
                content: 3,
            },
            Step::Modify {
                node: 6,
                path: 0,
                content: 4,
            },
            Step::Create {
                node: 4,
                path: 1,
                content: 4,
            },
            Step::Delete { node: 5, path: 10 },
            Step::Online { node: 4 },
            Step::Partition { a: 1, b: 1 },
            Step::Delete { node: 7, path: 0 },
            Step::Modify {
                node: 4,
                path: 8,
                content: 5,
            },
            Step::Delete { node: 7, path: 11 },
            Step::Online { node: 4 },
            Step::Tier {
                a: 5,
                b: 7,
                tier: 0,
            },
            Step::Tier {
                a: 6,
                b: 4,
                tier: 0,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Revert,
                delay_secs: 4,
            },
            Step::Partition { a: 1, b: 3 },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(2), Some(4)],
            },
            Step::Settle { secs: 9 },
            Step::Touch { node: 5, path: 9 },
            Step::Settle { secs: 14 },
            Step::Create {
                node: 4,
                path: 10,
                content: 5,
            },
            Step::Partition { a: 6, b: 2 },
            Step::Heal { a: 4, b: 1 },
            Step::Settle { secs: 14 },
            Step::Modify {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::Rename {
                node: 3,
                from: 0,
                to: 6,
            },
            Step::Create {
                node: 7,
                path: 3,
                content: 4,
            },
            Step::Touch { node: 4, path: 6 },
            Step::Chmod { node: 0, path: 3 },
            Step::Touch { node: 1, path: 0 },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Delete { node: 0, path: 3 },
            Step::Partition { a: 2, b: 4 },
            Step::Create {
                node: 3,
                path: 3,
                content: 2,
            },
            Step::Rename {
                node: 1,
                from: 8,
                to: 1,
            },
            Step::Delete { node: 6, path: 4 },
            Step::Delete { node: 2, path: 5 },
            Step::Modify {
                node: 7,
                path: 1,
                content: 4,
            },
            Step::Crash {
                node: 4,
                gap_secs: 75,
            },
            Step::Touch { node: 6, path: 4 },
            Step::Touch { node: 4, path: 2 },
            Step::Offline { node: 1 },
            Step::Create {
                node: 6,
                path: 2,
                content: 1,
            },
            Step::Partition { a: 2, b: 3 },
            Step::Offline { node: 1 },
            Step::Rmdir { node: 3, dir: 0 },
            Step::Create {
                node: 6,
                path: 6,
                content: 3,
            },
            Step::Modify {
                node: 2,
                path: 10,
                content: 5,
            },
            Step::Settle { secs: 14 },
            Step::Everywhere {
                path: 5,
                contents: vec![None, Some(3), Some(2), Some(1), Some(4)],
            },
            Step::Modify {
                node: 7,
                path: 2,
                content: 3,
            },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Modify {
                node: 6,
                path: 4,
                content: 3,
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(5), None, Some(2), Some(3), None, None, Some(1)],
            },
            Step::Modify {
                node: 0,
                path: 8,
                content: 4,
            },
            Step::Chmod { node: 0, path: 4 },
            Step::Create {
                node: 6,
                path: 5,
                content: 3,
            },
            Step::Modify {
                node: 6,
                path: 2,
                content: 4,
            },
            Step::Crash {
                node: 3,
                gap_secs: 75,
            },
            Step::Delete { node: 7, path: 10 },
            Step::User {
                node: 0,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 38,
                },
                delay_secs: 16,
            },
            Step::Symlink {
                node: 6,
                path: 5,
                target: 6,
            },
            Step::Touch { node: 2, path: 3 },
            Step::Offline { node: 7 },
            Step::Offline { node: 7 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 25,
            },
            Step::Partition { a: 5, b: 0 },
            Step::Delete { node: 7, path: 8 },
            Step::Modify {
                node: 3,
                path: 7,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 2,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 10,
                content: 2,
            },
            Step::Online { node: 5 },
            Step::Partition { a: 1, b: 7 },
            Step::Heal { a: 0, b: 0 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(1), Some(2), Some(1)],
            },
            Step::Delete { node: 4, path: 1 },
            Step::Rmdir { node: 2, dir: 1 },
            Step::Settle { secs: 1 },
            Step::Heal { a: 5, b: 0 },
            Step::Offline { node: 1 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 49,
            },
            Step::Create {
                node: 7,
                path: 7,
                content: 2,
            },
            Step::MassModify {
                node: 3,
                fraction: 73,
                content: 2,
            },
            Step::Rename {
                node: 0,
                from: 1,
                to: 7,
            },
            Step::Rmdir { node: 7, dir: 2 },
            Step::Online { node: 1 },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 9,
            },
        ],
    );
}

/// Found at seed 90057 of the first 100,000-seed run, with fetch corruption
/// on: a want gave up after hash mismatches from two sources and stayed
/// given up while those sources reconnected, so the file never arrived
/// (§7.5). Re-pinned at seed 36 with fetch corruption on, shrunk to 248
/// steps, after draft 54's refusal rules moved seed 28's history; with the fix
/// disabled it fails I1 at a conflict copy of d1/f10.
#[test]
fn a_want_that_gave_up_is_wanted_again() {
    passes_with(
        36,
        &corrupting(),
        &[
            Step::Symlink {
                node: 5,
                path: 8,
                target: 7,
            },
            Step::Create {
                node: 4,
                path: 10,
                content: 1,
            },
            Step::Heal { a: 5, b: 7 },
            Step::Create {
                node: 6,
                path: 1,
                content: 2,
            },
            Step::Partition { a: 2, b: 3 },
            Step::Touch { node: 3, path: 7 },
            Step::Modify {
                node: 0,
                path: 3,
                content: 1,
            },
            Step::Partition { a: 0, b: 1 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(3), Some(4)],
            },
            Step::Partition { a: 7, b: 6 },
            Step::Online { node: 7 },
            Step::Delete { node: 0, path: 0 },
            Step::Delete { node: 4, path: 8 },
            Step::Modify {
                node: 3,
                path: 2,
                content: 3,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![None, None, Some(3), Some(4), Some(5), Some(5), None],
            },
            Step::Tier {
                a: 7,
                b: 3,
                tier: 2,
            },
            Step::Create {
                node: 2,
                path: 6,
                content: 5,
            },
            Step::Settle { secs: 3 },
            Step::MassModify {
                node: 0,
                fraction: 66,
                content: 3,
            },
            Step::Offline { node: 0 },
            Step::Mkdir { node: 4, dir: 1 },
            Step::Settle { secs: 27 },
            Step::Rename {
                node: 2,
                from: 6,
                to: 10,
            },
            Step::Modify {
                node: 5,
                path: 7,
                content: 5,
            },
            Step::Rmdir { node: 6, dir: 2 },
            Step::Partition { a: 5, b: 3 },
            Step::Create {
                node: 1,
                path: 3,
                content: 5,
            },
            Step::Modify {
                node: 1,
                path: 2,
                content: 5,
            },
            Step::Create {
                node: 2,
                path: 2,
                content: 3,
            },
            Step::Rmdir { node: 1, dir: 0 },
            Step::Create {
                node: 7,
                path: 4,
                content: 1,
            },
            Step::Create {
                node: 5,
                path: 11,
                content: 2,
            },
            Step::Online { node: 1 },
            Step::Settle { secs: 28 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(5), Some(1), Some(1), None],
            },
            Step::Crash {
                node: 1,
                gap_secs: 34,
            },
            Step::Create {
                node: 2,
                path: 3,
                content: 1,
            },
            Step::Modify {
                node: 3,
                path: 1,
                content: 3,
            },
            Step::Partition { a: 4, b: 7 },
            Step::Settle { secs: 36 },
            Step::Create {
                node: 1,
                path: 1,
                content: 4,
            },
            Step::Rename {
                node: 0,
                from: 4,
                to: 3,
            },
            Step::Delete { node: 1, path: 7 },
            Step::Delete { node: 7, path: 5 },
            Step::Rename {
                node: 5,
                from: 7,
                to: 5,
            },
            Step::Symlink {
                node: 2,
                path: 3,
                target: 9,
            },
            Step::Rename {
                node: 7,
                from: 1,
                to: 3,
            },
            Step::Create {
                node: 0,
                path: 2,
                content: 3,
            },
            Step::Partition { a: 7, b: 5 },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(4), None, Some(2), Some(5), Some(2), Some(1), Some(1)],
            },
            Step::Create {
                node: 6,
                path: 3,
                content: 4,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![None, Some(1), Some(2), Some(3)],
            },
            Step::Partition { a: 0, b: 7 },
            Step::Rename {
                node: 5,
                from: 9,
                to: 7,
            },
            Step::MassModify {
                node: 7,
                fraction: 77,
                content: 5,
            },
            Step::Tier {
                a: 6,
                b: 3,
                tier: 2,
            },
            Step::Heal { a: 3, b: 6 },
            Step::MassDelete {
                node: 3,
                fraction: 90,
            },
            Step::Modify {
                node: 0,
                path: 4,
                content: 2,
            },
            Step::MassDelete {
                node: 0,
                fraction: 56,
            },
            Step::Delete { node: 1, path: 11 },
            Step::Heal { a: 4, b: 4 },
            Step::Settle { secs: 29 },
            Step::Delete { node: 0, path: 6 },
            Step::MassModify {
                node: 4,
                fraction: 93,
                content: 5,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 26,
            },
            Step::MassModify {
                node: 0,
                fraction: 93,
                content: 1,
            },
            Step::Create {
                node: 6,
                path: 5,
                content: 1,
            },
            Step::Tier {
                a: 6,
                b: 6,
                tier: 2,
            },
            Step::Touch { node: 1, path: 0 },
            Step::MassModify {
                node: 5,
                fraction: 51,
                content: 3,
            },
            Step::Delete { node: 5, path: 3 },
            Step::Chmod { node: 4, path: 10 },
            Step::Modify {
                node: 6,
                path: 7,
                content: 4,
            },
            Step::Crash {
                node: 4,
                gap_secs: 18,
            },
            Step::Partition { a: 6, b: 6 },
            Step::Heal { a: 6, b: 0 },
            Step::Settle { secs: 35 },
            Step::Modify {
                node: 2,
                path: 7,
                content: 3,
            },
            Step::Heal { a: 3, b: 3 },
            Step::Offline { node: 4 },
            Step::User {
                node: 5,
                action: crate::UserAction::DenyAll,
                delay_secs: 56,
            },
            Step::Delete { node: 1, path: 10 },
            Step::Create {
                node: 5,
                path: 0,
                content: 4,
            },
            Step::Symlink {
                node: 3,
                path: 5,
                target: 6,
            },
            Step::Delete { node: 2, path: 0 },
            Step::Delete { node: 3, path: 1 },
            Step::Crash {
                node: 4,
                gap_secs: 117,
            },
            Step::MassDelete {
                node: 2,
                fraction: 50,
            },
            Step::Delete { node: 0, path: 6 },
            Step::Mkdir { node: 5, dir: 0 },
            Step::Mkdir { node: 6, dir: 2 },
            Step::Heal { a: 0, b: 4 },
            Step::Settle { secs: 23 },
            Step::Heal { a: 0, b: 5 },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(3), None],
            },
            Step::Heal { a: 1, b: 7 },
            Step::Offline { node: 0 },
            Step::Online { node: 4 },
            Step::Partition { a: 5, b: 0 },
            Step::Delete { node: 0, path: 0 },
            Step::Partition { a: 6, b: 7 },
            Step::Heal { a: 3, b: 7 },
            Step::Settle { secs: 20 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 51,
            },
            Step::Modify {
                node: 5,
                path: 10,
                content: 1,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 1,
            },
            Step::Symlink {
                node: 5,
                path: 10,
                target: 6,
            },
            Step::Rename {
                node: 4,
                from: 7,
                to: 4,
            },
            Step::Delete { node: 3, path: 4 },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(1), Some(5), Some(2), None, None],
            },
            Step::Crash {
                node: 3,
                gap_secs: 70,
            },
            Step::Heal { a: 7, b: 5 },
            Step::Tier {
                a: 0,
                b: 0,
                tier: 2,
            },
            Step::Heal { a: 1, b: 7 },
            Step::Delete { node: 4, path: 4 },
            Step::Modify {
                node: 1,
                path: 4,
                content: 3,
            },
            Step::MassModify {
                node: 2,
                fraction: 61,
                content: 1,
            },
            Step::Heal { a: 5, b: 2 },
            Step::User {
                node: 3,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 45,
                },
                delay_secs: 37,
            },
            Step::Rename {
                node: 0,
                from: 9,
                to: 4,
            },
            Step::Delete { node: 3, path: 8 },
            Step::Create {
                node: 3,
                path: 11,
                content: 3,
            },
            Step::Modify {
                node: 4,
                path: 6,
                content: 4,
            },
            Step::Create {
                node: 3,
                path: 2,
                content: 3,
            },
            Step::Modify {
                node: 7,
                path: 9,
                content: 2,
            },
            Step::Modify {
                node: 3,
                path: 0,
                content: 4,
            },
            Step::Heal { a: 7, b: 1 },
            Step::Rename {
                node: 5,
                from: 3,
                to: 10,
            },
            Step::Create {
                node: 3,
                path: 2,
                content: 4,
            },
            Step::Settle { secs: 34 },
            Step::Rmdir { node: 2, dir: 0 },
            Step::Modify {
                node: 7,
                path: 2,
                content: 5,
            },
            Step::Settle { secs: 5 },
            Step::Settle { secs: 38 },
            Step::Settle { secs: 34 },
            Step::Modify {
                node: 5,
                path: 10,
                content: 4,
            },
            Step::Settle { secs: 19 },
            Step::Rename {
                node: 4,
                from: 1,
                to: 6,
            },
            Step::Online { node: 0 },
            Step::Chmod { node: 2, path: 10 },
            Step::Touch { node: 3, path: 11 },
            Step::Rmdir { node: 1, dir: 0 },
            Step::Chmod { node: 2, path: 9 },
            Step::Settle { secs: 5 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(2), Some(1), Some(5)],
            },
            Step::Modify {
                node: 6,
                path: 10,
                content: 1,
            },
            Step::Tier {
                a: 0,
                b: 6,
                tier: 1,
            },
            Step::Create {
                node: 2,
                path: 8,
                content: 4,
            },
            Step::Delete { node: 3, path: 7 },
            Step::Delete { node: 2, path: 0 },
            Step::Modify {
                node: 7,
                path: 1,
                content: 3,
            },
            Step::Partition { a: 7, b: 6 },
            Step::Heal { a: 3, b: 4 },
            Step::Modify {
                node: 7,
                path: 8,
                content: 4,
            },
            Step::Delete { node: 2, path: 10 },
            Step::Online { node: 6 },
            Step::Offline { node: 6 },
            Step::Delete { node: 4, path: 6 },
            Step::Partition { a: 5, b: 1 },
            Step::Delete { node: 0, path: 6 },
            Step::Tier {
                a: 6,
                b: 3,
                tier: 0,
            },
            Step::Tier {
                a: 2,
                b: 7,
                tier: 0,
            },
            Step::Create {
                node: 4,
                path: 11,
                content: 5,
            },
            Step::Settle { secs: 12 },
            Step::Settle { secs: 11 },
            Step::Partition { a: 0, b: 0 },
            Step::Create {
                node: 3,
                path: 2,
                content: 2,
            },
            Step::Settle { secs: 39 },
            Step::Crash {
                node: 6,
                gap_secs: 26,
            },
            Step::Delete { node: 7, path: 10 },
            Step::Modify {
                node: 1,
                path: 10,
                content: 3,
            },
            Step::Tier {
                a: 6,
                b: 0,
                tier: 0,
            },
            Step::MassDelete {
                node: 5,
                fraction: 57,
            },
            Step::Offline { node: 3 },
            Step::Modify {
                node: 7,
                path: 2,
                content: 4,
            },
            Step::Delete { node: 4, path: 2 },
            Step::Heal { a: 1, b: 7 },
            Step::Settle { secs: 26 },
            Step::Create {
                node: 2,
                path: 8,
                content: 4,
            },
            Step::Crash {
                node: 1,
                gap_secs: 78,
            },
            Step::Settle { secs: 7 },
            Step::Modify {
                node: 4,
                path: 1,
                content: 4,
            },
            Step::Everywhere {
                path: 10,
                contents: vec![Some(5), None, Some(1), Some(1), Some(5), Some(3), Some(4)],
            },
            Step::Delete { node: 7, path: 11 },
            Step::MassModify {
                node: 0,
                fraction: 94,
                content: 3,
            },
            Step::Delete { node: 4, path: 4 },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(4), Some(4)],
            },
            Step::Delete { node: 0, path: 3 },
            Step::Delete { node: 7, path: 11 },
            Step::Create {
                node: 4,
                path: 10,
                content: 2,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(5), None, Some(5)],
            },
            Step::Settle { secs: 20 },
            Step::User {
                node: 5,
                action: crate::UserAction::Rules {
                    hold_count: 5,
                    hold_pct: 46,
                },
                delay_secs: 4,
            },
            Step::Delete { node: 7, path: 3 },
            Step::Modify {
                node: 0,
                path: 0,
                content: 5,
            },
            Step::Crash {
                node: 7,
                gap_secs: 114,
            },
            Step::Tier {
                a: 3,
                b: 0,
                tier: 2,
            },
            Step::Partition { a: 5, b: 7 },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Partition { a: 0, b: 6 },
            Step::Everywhere {
                path: 3,
                contents: vec![None, Some(5), Some(5)],
            },
            Step::Modify {
                node: 5,
                path: 0,
                content: 2,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![None, Some(5)],
            },
            Step::Rename {
                node: 3,
                from: 9,
                to: 1,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(3), None, Some(2), Some(4), Some(5), Some(4), None],
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Revert,
                delay_secs: 20,
            },
            Step::Chmod { node: 6, path: 7 },
            Step::Mkdir { node: 6, dir: 0 },
            Step::MassModify {
                node: 1,
                fraction: 93,
                content: 2,
            },
            Step::Modify {
                node: 3,
                path: 7,
                content: 5,
            },
            Step::Chmod { node: 7, path: 2 },
            Step::MassDelete {
                node: 1,
                fraction: 72,
            },
            Step::Chmod { node: 1, path: 4 },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Delete { node: 7, path: 5 },
            Step::Delete { node: 3, path: 4 },
            Step::Everywhere {
                path: 5,
                contents: vec![None, Some(3), None, Some(2)],
            },
            Step::Symlink {
                node: 0,
                path: 9,
                target: 10,
            },
            Step::Online { node: 4 },
            Step::MassModify {
                node: 0,
                fraction: 71,
                content: 4,
            },
            Step::Modify {
                node: 1,
                path: 0,
                content: 2,
            },
            Step::Partition { a: 1, b: 3 },
            Step::Create {
                node: 3,
                path: 0,
                content: 2,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(4), None, Some(1)],
            },
            Step::Heal { a: 4, b: 7 },
            Step::Modify {
                node: 3,
                path: 0,
                content: 5,
            },
            Step::Heal { a: 0, b: 7 },
            Step::Touch { node: 1, path: 4 },
            Step::Modify {
                node: 0,
                path: 2,
                content: 2,
            },
            Step::Modify {
                node: 1,
                path: 2,
                content: 5,
            },
            Step::Create {
                node: 1,
                path: 8,
                content: 2,
            },
            Step::Modify {
                node: 4,
                path: 8,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 3,
                content: 1,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 22,
            },
            Step::Modify {
                node: 3,
                path: 0,
                content: 1,
            },
            Step::Settle { secs: 34 },
            Step::Create {
                node: 1,
                path: 0,
                content: 3,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 3,
            },
            Step::Rename {
                node: 2,
                from: 3,
                to: 4,
            },
            Step::Delete { node: 4, path: 3 },
            Step::Tier {
                a: 7,
                b: 1,
                tier: 2,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 1,
            },
            Step::Create {
                node: 6,
                path: 5,
                content: 5,
            },
            Step::Online { node: 6 },
            Step::Mkdir { node: 4, dir: 1 },
            Step::Delete { node: 5, path: 9 },
            Step::Chmod { node: 1, path: 4 },
        ],
    );
}

/// One corrupted transfer excluded the only source of a want for good, and
/// the want waited without a source to the end of the run (§7.5). Draft
/// 31's events release an exclusion or a give-up, and so does draft 32's
/// expiry, so this fails only with both disabled. Pinned at seed 256 with
/// fetch corruption on, shrunk to 16 steps; with both disabled it fails I1
/// (f1 live on one node and absent on another), and with either alone it
/// passes. Seed 473 guarded this until the skip model (§7.3) moved its
/// history, and seed 231's shrunk list until the late-fetch fix did.
#[test]
fn a_source_excluded_after_one_mismatch_is_asked_again() {
    passes_with(
        256,
        &corrupting(),
        &[
            Step::Partition { a: 1, b: 7 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(3), Some(2), Some(2)],
            },
            Step::Rename {
                node: 7,
                from: 4,
                to: 9,
            },
            Step::Modify {
                node: 2,
                path: 5,
                content: 3,
            },
            Step::Partition { a: 7, b: 0 },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(1), Some(4), Some(3)],
            },
            Step::Rename {
                node: 7,
                from: 2,
                to: 1,
            },
            Step::Crash {
                node: 7,
                gap_secs: 113,
            },
            Step::Create {
                node: 2,
                path: 3,
                content: 2,
            },
            Step::Partition { a: 6, b: 2 },
            Step::Heal { a: 0, b: 2 },
            Step::Partition { a: 6, b: 6 },
            Step::Create {
                node: 3,
                path: 4,
                content: 5,
            },
            Step::Partition { a: 6, b: 3 },
            Step::Heal { a: 7, b: 2 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 3,
            },
        ],
    );
}

/// Seed 95175, fetch corruption on, 16 steps: after a revert, the only
/// source of a want served one corrupted transfer and was never asked again
/// (§7.5). Shrunk, it needs the expiry as well as draft 31's events.
/// Replayed as found: group commit alone loses its scenario (PR 1b).
#[test]
fn a_mismatch_after_a_revert_does_not_strand_the_file() {
    passes_as_found(
        95175,
        &corrupting(),
        &[
            Step::Everywhere {
                path: 3,
                contents: vec![Some(3), None, None, None, Some(4), Some(5), Some(3)],
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 2,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 10,
                },
                delay_secs: 38,
            },
            Step::Online { node: 1 },
            Step::Settle { secs: 3 },
            Step::Rename {
                node: 5,
                from: 8,
                to: 4,
            },
            Step::Settle { secs: 29 },
            Step::User {
                node: 1,
                action: crate::UserAction::Revert,
                delay_secs: 14,
            },
            Step::Touch { node: 2, path: 9 },
            Step::Touch { node: 1, path: 4 },
            Step::Heal { a: 0, b: 2 },
            Step::Rename {
                node: 0,
                from: 0,
                to: 4,
            },
            Step::Heal { a: 6, b: 2 },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 47,
            },
            Step::Touch { node: 6, path: 2 },
            Step::Tier {
                a: 2,
                b: 3,
                tier: 0,
            },
        ],
    );
}

/// Seed 99246, fetch corruption on, 37 steps: the only source of a want
/// served one corrupted transfer, then stayed connected and never announced
/// the path again, and nothing changed at the path, so none of draft 31's
/// events released it. Only the expiry does (§7.5, draft 32); this fails
/// with the expiry disabled.
/// Replayed as found: group commit alone loses its scenario (PR 1b).
#[test]
fn an_exclusion_expires_for_another_file_in_a_directory() {
    passes_as_found(
        99246,
        &corrupting(),
        &[
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 12,
            },
            Step::Chmod { node: 3, path: 9 },
            Step::Touch { node: 5, path: 1 },
            Step::Heal { a: 0, b: 0 },
            Step::User {
                node: 7,
                action: crate::UserAction::Rules {
                    hold_count: 0,
                    hold_pct: 56,
                },
                delay_secs: 9,
            },
            Step::Delete { node: 3, path: 9 },
            Step::Mkdir { node: 3, dir: 2 },
            Step::Delete { node: 7, path: 5 },
            Step::Delete { node: 1, path: 10 },
            Step::Online { node: 1 },
            Step::Heal { a: 1, b: 5 },
            Step::Rename {
                node: 4,
                from: 8,
                to: 2,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(5), Some(4), None, Some(5), Some(1), Some(4), None],
            },
            Step::User {
                node: 2,
                action: crate::UserAction::ApproveAll,
                delay_secs: 40,
            },
            Step::Mkdir { node: 7, dir: 0 },
            Step::Delete { node: 0, path: 4 },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Heal { a: 4, b: 0 },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 23,
            },
            Step::Online { node: 2 },
            Step::Create {
                node: 3,
                path: 11,
                content: 1,
            },
            Step::Delete { node: 1, path: 8 },
            Step::Settle { secs: 7 },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Modify {
                node: 0,
                path: 3,
                content: 1,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::ApproveAll,
                delay_secs: 11,
            },
            Step::Delete { node: 2, path: 2 },
            Step::Partition { a: 4, b: 6 },
            Step::Online { node: 1 },
            Step::Partition { a: 7, b: 7 },
            Step::Create {
                node: 3,
                path: 8,
                content: 5,
            },
            Step::Touch { node: 5, path: 2 },
            Step::Heal { a: 5, b: 7 },
            Step::Modify {
                node: 4,
                path: 5,
                content: 2,
            },
            Step::Partition { a: 3, b: 5 },
            Step::Heal { a: 4, b: 5 },
            Step::Create {
                node: 1,
                path: 6,
                content: 1,
            },
        ],
    );
}

/// Found at seed 10187: across two reverts one node issued the same vector
/// at a path twice, for symlinks with different targets. Symlinks carry no
/// mtime, so both losing versions have the same copy name, and I4 kept
/// only the first loser under the shared vector (§14.1). Re-pinned at seed
/// 400 with the default knobs, shrunk to 254 steps, after drafts 52 and 53's failed-report rules
/// moved the history it was replayed in; it fails I4 at f1.
#[test]
fn two_losers_under_one_reissued_vector_both_count() {
    passes(
        400,
        &[
            Step::Create {
                node: 3,
                path: 0,
                content: 1,
            },
            Step::Heal { a: 6, b: 5 },
            Step::Mkdir { node: 1, dir: 1 },
            Step::Modify {
                node: 0,
                path: 9,
                content: 1,
            },
            Step::Heal { a: 4, b: 2 },
            Step::Settle { secs: 34 },
            Step::Create {
                node: 5,
                path: 10,
                content: 5,
            },
            Step::Heal { a: 3, b: 1 },
            Step::Settle { secs: 26 },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(2), Some(4), Some(2), Some(5)],
            },
            Step::Create {
                node: 7,
                path: 1,
                content: 2,
            },
            Step::Crash {
                node: 1,
                gap_secs: 63,
            },
            Step::Modify {
                node: 5,
                path: 11,
                content: 1,
            },
            Step::Touch { node: 0, path: 1 },
            Step::Create {
                node: 0,
                path: 9,
                content: 3,
            },
            Step::Modify {
                node: 5,
                path: 10,
                content: 1,
            },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Create {
                node: 6,
                path: 3,
                content: 5,
            },
            Step::Touch { node: 3, path: 2 },
            Step::Create {
                node: 1,
                path: 5,
                content: 3,
            },
            Step::MassModify {
                node: 7,
                fraction: 93,
                content: 3,
            },
            Step::Online { node: 5 },
            Step::Create {
                node: 4,
                path: 7,
                content: 5,
            },
            Step::Create {
                node: 0,
                path: 8,
                content: 4,
            },
            Step::Partition { a: 1, b: 3 },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 14,
            },
            Step::Chmod { node: 2, path: 11 },
            Step::Modify {
                node: 5,
                path: 10,
                content: 1,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::DenyAll,
                delay_secs: 37,
            },
            Step::Online { node: 5 },
            Step::Partition { a: 4, b: 1 },
            Step::Settle { secs: 32 },
            Step::Crash {
                node: 4,
                gap_secs: 100,
            },
            Step::Rename {
                node: 5,
                from: 8,
                to: 0,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::Rules {
                    hold_count: 2,
                    hold_pct: 59,
                },
                delay_secs: 50,
            },
            Step::MassDelete {
                node: 3,
                fraction: 60,
            },
            Step::Modify {
                node: 1,
                path: 10,
                content: 2,
            },
            Step::Heal { a: 3, b: 2 },
            Step::Offline { node: 3 },
            Step::Create {
                node: 0,
                path: 6,
                content: 3,
            },
            Step::MassDelete {
                node: 5,
                fraction: 61,
            },
            Step::Mkdir { node: 4, dir: 0 },
            Step::Heal { a: 1, b: 2 },
            Step::Modify {
                node: 6,
                path: 3,
                content: 3,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![None, None, Some(2)],
            },
            Step::Mkdir { node: 1, dir: 0 },
            Step::MassDelete {
                node: 1,
                fraction: 75,
            },
            Step::Heal { a: 3, b: 3 },
            Step::Settle { secs: 18 },
            Step::Crash {
                node: 2,
                gap_secs: 49,
            },
            Step::Modify {
                node: 0,
                path: 1,
                content: 2,
            },
            Step::Create {
                node: 2,
                path: 8,
                content: 4,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 43,
            },
            Step::Modify {
                node: 6,
                path: 11,
                content: 4,
            },
            Step::Create {
                node: 2,
                path: 11,
                content: 1,
            },
            Step::Delete { node: 3, path: 2 },
            Step::Heal { a: 0, b: 4 },
            Step::Symlink {
                node: 0,
                path: 7,
                target: 1,
            },
            Step::Touch { node: 7, path: 2 },
            Step::Delete { node: 0, path: 11 },
            Step::Heal { a: 6, b: 2 },
            Step::Chmod { node: 3, path: 4 },
            Step::Create {
                node: 4,
                path: 7,
                content: 1,
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(4), None, Some(2), Some(5), None],
            },
            Step::Mkdir { node: 1, dir: 2 },
            Step::Create {
                node: 1,
                path: 9,
                content: 3,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 41,
            },
            Step::Offline { node: 6 },
            Step::Modify {
                node: 5,
                path: 1,
                content: 5,
            },
            Step::Rename {
                node: 2,
                from: 10,
                to: 1,
            },
            Step::Create {
                node: 6,
                path: 11,
                content: 5,
            },
            Step::Create {
                node: 0,
                path: 10,
                content: 4,
            },
            Step::Settle { secs: 15 },
            Step::Symlink {
                node: 1,
                path: 11,
                target: 6,
            },
            Step::Crash {
                node: 1,
                gap_secs: 50,
            },
            Step::Delete { node: 2, path: 2 },
            Step::Create {
                node: 6,
                path: 9,
                content: 1,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(3), None, Some(5)],
            },
            Step::Create {
                node: 6,
                path: 11,
                content: 5,
            },
            Step::Delete { node: 0, path: 8 },
            Step::Create {
                node: 1,
                path: 9,
                content: 3,
            },
            Step::Chmod { node: 3, path: 10 },
            Step::Delete { node: 2, path: 5 },
            Step::Settle { secs: 21 },
            Step::Crash {
                node: 0,
                gap_secs: 112,
            },
            Step::Tier {
                a: 1,
                b: 6,
                tier: 2,
            },
            Step::Crash {
                node: 0,
                gap_secs: 97,
            },
            Step::Rename {
                node: 5,
                from: 3,
                to: 7,
            },
            Step::Heal { a: 2, b: 6 },
            Step::Settle { secs: 31 },
            Step::MassModify {
                node: 1,
                fraction: 96,
                content: 1,
            },
            Step::Modify {
                node: 0,
                path: 3,
                content: 3,
            },
            Step::Delete { node: 3, path: 10 },
            Step::Create {
                node: 6,
                path: 2,
                content: 2,
            },
            Step::Tier {
                a: 6,
                b: 3,
                tier: 2,
            },
            Step::Create {
                node: 6,
                path: 9,
                content: 2,
            },
            Step::Mkdir { node: 2, dir: 1 },
            Step::Create {
                node: 6,
                path: 2,
                content: 5,
            },
            Step::Settle { secs: 15 },
            Step::MassModify {
                node: 2,
                fraction: 75,
                content: 3,
            },
            Step::Chmod { node: 3, path: 10 },
            Step::Create {
                node: 0,
                path: 8,
                content: 1,
            },
            Step::Mkdir { node: 5, dir: 0 },
            Step::Modify {
                node: 5,
                path: 11,
                content: 2,
            },
            Step::Create {
                node: 7,
                path: 10,
                content: 4,
            },
            Step::Modify {
                node: 0,
                path: 5,
                content: 5,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 5,
            },
            Step::Online { node: 4 },
            Step::Crash {
                node: 0,
                gap_secs: 61,
            },
            Step::Modify {
                node: 4,
                path: 7,
                content: 1,
            },
            Step::Partition { a: 7, b: 1 },
            Step::Offline { node: 5 },
            Step::Partition { a: 0, b: 2 },
            Step::Create {
                node: 4,
                path: 5,
                content: 4,
            },
            Step::Modify {
                node: 2,
                path: 9,
                content: 2,
            },
            Step::Modify {
                node: 3,
                path: 5,
                content: 1,
            },
            Step::Crash {
                node: 2,
                gap_secs: 48,
            },
            Step::Heal { a: 2, b: 3 },
            Step::Create {
                node: 1,
                path: 9,
                content: 4,
            },
            Step::Tier {
                a: 1,
                b: 1,
                tier: 1,
            },
            Step::Offline { node: 3 },
            Step::Delete { node: 5, path: 7 },
            Step::Chmod { node: 1, path: 8 },
            Step::Heal { a: 2, b: 6 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 9,
            },
            Step::Create {
                node: 0,
                path: 6,
                content: 3,
            },
            Step::Settle { secs: 33 },
            Step::Settle { secs: 19 },
            Step::Create {
                node: 1,
                path: 6,
                content: 1,
            },
            Step::Modify {
                node: 3,
                path: 0,
                content: 3,
            },
            Step::Crash {
                node: 6,
                gap_secs: 83,
            },
            Step::Offline { node: 6 },
            Step::Rename {
                node: 1,
                from: 3,
                to: 9,
            },
            Step::Modify {
                node: 0,
                path: 7,
                content: 1,
            },
            Step::Crash {
                node: 4,
                gap_secs: 56,
            },
            Step::Heal { a: 0, b: 6 },
            Step::Heal { a: 7, b: 2 },
            Step::User {
                node: 4,
                action: crate::UserAction::DenyAll,
                delay_secs: 32,
            },
            Step::Heal { a: 0, b: 4 },
            Step::Rename {
                node: 4,
                from: 11,
                to: 9,
            },
            Step::Symlink {
                node: 1,
                path: 1,
                target: 0,
            },
            Step::Delete { node: 0, path: 2 },
            Step::Heal { a: 6, b: 3 },
            Step::Create {
                node: 5,
                path: 7,
                content: 2,
            },
            Step::Create {
                node: 4,
                path: 11,
                content: 3,
            },
            Step::Delete { node: 3, path: 8 },
            Step::Rmdir { node: 0, dir: 2 },
            Step::Delete { node: 7, path: 9 },
            Step::Delete { node: 3, path: 3 },
            Step::Crash {
                node: 1,
                gap_secs: 105,
            },
            Step::Create {
                node: 1,
                path: 1,
                content: 5,
            },
            Step::Create {
                node: 6,
                path: 7,
                content: 4,
            },
            Step::Symlink {
                node: 4,
                path: 6,
                target: 5,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![None, Some(4), Some(3), None, Some(5), Some(1), Some(5)],
            },
            Step::Delete { node: 1, path: 10 },
            Step::Modify {
                node: 6,
                path: 0,
                content: 2,
            },
            Step::Partition { a: 5, b: 2 },
            Step::Modify {
                node: 6,
                path: 1,
                content: 2,
            },
            Step::Tier {
                a: 1,
                b: 0,
                tier: 2,
            },
            Step::Online { node: 2 },
            Step::Create {
                node: 4,
                path: 0,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 6,
                content: 5,
            },
            Step::Everywhere {
                path: 4,
                contents: vec![None, Some(3), Some(5), Some(2), Some(2)],
            },
            Step::Delete { node: 7, path: 7 },
            Step::Heal { a: 3, b: 1 },
            Step::Rename {
                node: 0,
                from: 9,
                to: 3,
            },
            Step::Rename {
                node: 1,
                from: 0,
                to: 4,
            },
            Step::Partition { a: 5, b: 4 },
            Step::Create {
                node: 0,
                path: 4,
                content: 2,
            },
            Step::Mkdir { node: 3, dir: 0 },
            Step::Everywhere {
                path: 5,
                contents: vec![None, Some(1), Some(3)],
            },
            Step::Create {
                node: 1,
                path: 11,
                content: 4,
            },
            Step::Partition { a: 0, b: 4 },
            Step::Delete { node: 1, path: 3 },
            Step::Symlink {
                node: 5,
                path: 5,
                target: 8,
            },
            Step::Symlink {
                node: 2,
                path: 4,
                target: 8,
            },
            Step::Chmod { node: 0, path: 4 },
            Step::Heal { a: 6, b: 1 },
            Step::Settle { secs: 36 },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(1), Some(3), Some(5), None, Some(3)],
            },
            Step::Crash {
                node: 6,
                gap_secs: 51,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::DenyAll,
                delay_secs: 38,
            },
            Step::Tier {
                a: 4,
                b: 4,
                tier: 2,
            },
            Step::Modify {
                node: 5,
                path: 9,
                content: 5,
            },
            Step::User {
                node: 4,
                action: crate::UserAction::DenyAll,
                delay_secs: 28,
            },
            Step::Offline { node: 2 },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(1), None, None],
            },
            Step::Delete { node: 0, path: 4 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(4), Some(2), Some(2), None, Some(1)],
            },
            Step::Delete { node: 6, path: 5 },
            Step::Rename {
                node: 2,
                from: 3,
                to: 10,
            },
            Step::Online { node: 1 },
            Step::Partition { a: 5, b: 1 },
            Step::Chmod { node: 2, path: 8 },
            Step::Modify {
                node: 3,
                path: 6,
                content: 3,
            },
            Step::Crash {
                node: 4,
                gap_secs: 7,
            },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(2), None, Some(1), Some(5)],
            },
            Step::Partition { a: 1, b: 4 },
            Step::Settle { secs: 32 },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 44,
            },
            Step::Touch { node: 1, path: 9 },
            Step::Partition { a: 1, b: 1 },
            Step::Modify {
                node: 6,
                path: 1,
                content: 1,
            },
            Step::Modify {
                node: 6,
                path: 2,
                content: 3,
            },
            Step::Chmod { node: 6, path: 8 },
            Step::Create {
                node: 4,
                path: 8,
                content: 2,
            },
            Step::Modify {
                node: 5,
                path: 4,
                content: 4,
            },
            Step::Create {
                node: 5,
                path: 6,
                content: 1,
            },
            Step::Chmod { node: 4, path: 2 },
            Step::Settle { secs: 36 },
            Step::Online { node: 4 },
            Step::Modify {
                node: 7,
                path: 6,
                content: 1,
            },
            Step::Partition { a: 5, b: 7 },
            Step::Tier {
                a: 4,
                b: 7,
                tier: 0,
            },
            Step::Heal { a: 4, b: 6 },
            Step::Heal { a: 4, b: 0 },
            Step::Online { node: 5 },
            Step::Mkdir { node: 1, dir: 2 },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 41,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::Revert,
                delay_secs: 37,
            },
            Step::Tier {
                a: 5,
                b: 2,
                tier: 1,
            },
            Step::Rename {
                node: 3,
                from: 11,
                to: 10,
            },
            Step::Delete { node: 2, path: 2 },
            Step::Modify {
                node: 6,
                path: 8,
                content: 2,
            },
            Step::Heal { a: 2, b: 4 },
            Step::Rename {
                node: 0,
                from: 0,
                to: 5,
            },
            Step::Chmod { node: 4, path: 10 },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Modify {
                node: 0,
                path: 6,
                content: 2,
            },
            Step::Settle { secs: 23 },
            Step::Delete { node: 0, path: 9 },
            Step::Online { node: 0 },
            Step::Tier {
                a: 7,
                b: 2,
                tier: 2,
            },
            Step::Create {
                node: 7,
                path: 0,
                content: 4,
            },
            Step::MassDelete {
                node: 4,
                fraction: 95,
            },
            Step::Modify {
                node: 4,
                path: 9,
                content: 1,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(1), Some(3), Some(5)],
            },
            Step::Modify {
                node: 3,
                path: 3,
                content: 1,
            },
            Step::Online { node: 0 },
            Step::Online { node: 4 },
            Step::Partition { a: 0, b: 0 },
            Step::Delete { node: 4, path: 6 },
            Step::Partition { a: 0, b: 2 },
            Step::Settle { secs: 16 },
            Step::Heal { a: 7, b: 4 },
            Step::User {
                node: 6,
                action: crate::UserAction::ApproveAll,
                delay_secs: 59,
            },
            Step::Delete { node: 6, path: 11 },
            Step::Touch { node: 7, path: 7 },
            Step::MassModify {
                node: 1,
                fraction: 87,
                content: 5,
            },
            Step::MassModify {
                node: 3,
                fraction: 86,
                content: 4,
            },
            Step::Settle { secs: 10 },
            Step::Chmod { node: 4, path: 3 },
            Step::Settle { secs: 33 },
            Step::Symlink {
                node: 1,
                path: 1,
                target: 3,
            },
        ],
    );
}

/// The same shape in a directory, found at seed 15791 at d2/f5: a vector
/// issued twice across a revert for two symlink targets under one copy name
/// (§14.1). Re-pinned at seed 12426 with the default knobs, shrunk to 207
/// steps, after drafts 52 and 53's failed-report rules moved the history it was replayed
/// in; it fails I4 at d1/f10.
#[test]
fn two_symlink_losers_under_one_reissued_vector_both_count() {
    passes(
        12426,
        &[
            Step::Rename {
                node: 0,
                from: 6,
                to: 9,
            },
            Step::User {
                node: 1,
                action: crate::UserAction::DenyAll,
                delay_secs: 59,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 53,
            },
            Step::Partition { a: 2, b: 7 },
            Step::Offline { node: 1 },
            Step::Online { node: 1 },
            Step::MassModify {
                node: 6,
                fraction: 80,
                content: 2,
            },
            Step::Rename {
                node: 3,
                from: 0,
                to: 9,
            },
            Step::Create {
                node: 6,
                path: 1,
                content: 5,
            },
            Step::Settle { secs: 21 },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 47,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::Rules {
                    hold_count: 4,
                    hold_pct: 48,
                },
                delay_secs: 59,
            },
            Step::Delete { node: 6, path: 8 },
            Step::Delete { node: 5, path: 10 },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(5), Some(3)],
            },
            Step::Tier {
                a: 3,
                b: 4,
                tier: 0,
            },
            Step::Modify {
                node: 1,
                path: 3,
                content: 1,
            },
            Step::Online { node: 4 },
            Step::Settle { secs: 26 },
            Step::Modify {
                node: 6,
                path: 10,
                content: 5,
            },
            Step::Settle { secs: 12 },
            Step::Everywhere {
                path: 0,
                contents: vec![None, Some(2)],
            },
            Step::Tier {
                a: 7,
                b: 4,
                tier: 0,
            },
            Step::Tier {
                a: 1,
                b: 3,
                tier: 2,
            },
            Step::Heal { a: 4, b: 5 },
            Step::Create {
                node: 5,
                path: 10,
                content: 3,
            },
            Step::Modify {
                node: 4,
                path: 6,
                content: 3,
            },
            Step::User {
                node: 3,
                action: crate::UserAction::DenyAll,
                delay_secs: 31,
            },
            Step::Heal { a: 2, b: 3 },
            Step::Create {
                node: 4,
                path: 9,
                content: 5,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 35,
            },
            Step::Heal { a: 5, b: 4 },
            Step::Heal { a: 0, b: 6 },
            Step::Rename {
                node: 5,
                from: 1,
                to: 0,
            },
            Step::Modify {
                node: 1,
                path: 9,
                content: 4,
            },
            Step::Heal { a: 7, b: 3 },
            Step::Create {
                node: 7,
                path: 8,
                content: 4,
            },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Modify {
                node: 1,
                path: 11,
                content: 1,
            },
            Step::Offline { node: 5 },
            Step::Modify {
                node: 6,
                path: 3,
                content: 3,
            },
            Step::Partition { a: 1, b: 7 },
            Step::Crash {
                node: 1,
                gap_secs: 105,
            },
            Step::Partition { a: 5, b: 5 },
            Step::Heal { a: 3, b: 0 },
            Step::Settle { secs: 38 },
            Step::Modify {
                node: 3,
                path: 0,
                content: 5,
            },
            Step::Delete { node: 4, path: 5 },
            Step::Symlink {
                node: 7,
                path: 10,
                target: 6,
            },
            Step::Offline { node: 0 },
            Step::Mkdir { node: 5, dir: 2 },
            Step::Partition { a: 3, b: 1 },
            Step::Chmod { node: 1, path: 8 },
            Step::Touch { node: 7, path: 5 },
            Step::Rmdir { node: 7, dir: 2 },
            Step::Heal { a: 4, b: 4 },
            Step::Partition { a: 7, b: 5 },
            Step::Tier {
                a: 0,
                b: 6,
                tier: 0,
            },
            Step::Rename {
                node: 2,
                from: 0,
                to: 1,
            },
            Step::Modify {
                node: 4,
                path: 3,
                content: 3,
            },
            Step::Partition { a: 4, b: 6 },
            Step::Modify {
                node: 7,
                path: 10,
                content: 5,
            },
            Step::Rmdir { node: 2, dir: 0 },
            Step::Rename {
                node: 6,
                from: 8,
                to: 2,
            },
            Step::User {
                node: 7,
                action: crate::UserAction::DenyAll,
                delay_secs: 43,
            },
            Step::MassDelete {
                node: 2,
                fraction: 86,
            },
            Step::Delete { node: 4, path: 4 },
            Step::MassModify {
                node: 0,
                fraction: 90,
                content: 2,
            },
            Step::Modify {
                node: 3,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 1,
                path: 11,
                content: 3,
            },
            Step::Rmdir { node: 1, dir: 1 },
            Step::Partition { a: 2, b: 1 },
            Step::User {
                node: 4,
                action: crate::UserAction::DenyAll,
                delay_secs: 18,
            },
            Step::Delete { node: 1, path: 6 },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Offline { node: 6 },
            Step::Create {
                node: 4,
                path: 6,
                content: 2,
            },
            Step::Delete { node: 0, path: 11 },
            Step::Modify {
                node: 1,
                path: 11,
                content: 1,
            },
            Step::Heal { a: 3, b: 5 },
            Step::Partition { a: 6, b: 4 },
            Step::Heal { a: 2, b: 6 },
            Step::Chmod { node: 5, path: 4 },
            Step::Tier {
                a: 5,
                b: 2,
                tier: 1,
            },
            Step::Symlink {
                node: 5,
                path: 10,
                target: 8,
            },
            Step::Delete { node: 4, path: 8 },
            Step::Settle { secs: 9 },
            Step::Touch { node: 6, path: 2 },
            Step::Crash {
                node: 0,
                gap_secs: 43,
            },
            Step::Create {
                node: 3,
                path: 7,
                content: 2,
            },
            Step::Heal { a: 2, b: 4 },
            Step::Modify {
                node: 0,
                path: 9,
                content: 5,
            },
            Step::Modify {
                node: 4,
                path: 6,
                content: 2,
            },
            Step::Rmdir { node: 5, dir: 0 },
            Step::Heal { a: 6, b: 1 },
            Step::Crash {
                node: 7,
                gap_secs: 73,
            },
            Step::Mkdir { node: 2, dir: 2 },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Delete { node: 4, path: 3 },
            Step::Delete { node: 1, path: 3 },
            Step::MassDelete {
                node: 2,
                fraction: 76,
            },
            Step::Partition { a: 6, b: 1 },
            Step::Create {
                node: 2,
                path: 9,
                content: 1,
            },
            Step::Create {
                node: 7,
                path: 1,
                content: 4,
            },
            Step::Tier {
                a: 6,
                b: 5,
                tier: 1,
            },
            Step::Create {
                node: 5,
                path: 6,
                content: 3,
            },
            Step::Crash {
                node: 6,
                gap_secs: 63,
            },
            Step::Create {
                node: 2,
                path: 1,
                content: 5,
            },
            Step::Delete { node: 7, path: 3 },
            Step::Create {
                node: 6,
                path: 3,
                content: 2,
            },
            Step::Heal { a: 7, b: 7 },
            Step::Modify {
                node: 0,
                path: 4,
                content: 5,
            },
            Step::Crash {
                node: 1,
                gap_secs: 111,
            },
            Step::Rename {
                node: 3,
                from: 1,
                to: 5,
            },
            Step::Modify {
                node: 2,
                path: 6,
                content: 3,
            },
            Step::Settle { secs: 30 },
            Step::Create {
                node: 4,
                path: 10,
                content: 5,
            },
            Step::Modify {
                node: 3,
                path: 7,
                content: 3,
            },
            Step::Rmdir { node: 2, dir: 1 },
            Step::Touch { node: 7, path: 10 },
            Step::Offline { node: 7 },
            Step::Partition { a: 1, b: 1 },
            Step::Create {
                node: 7,
                path: 7,
                content: 4,
            },
            Step::Chmod { node: 6, path: 9 },
            Step::Modify {
                node: 2,
                path: 1,
                content: 2,
            },
            Step::Create {
                node: 1,
                path: 4,
                content: 4,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![Some(3), None, Some(5), Some(2), Some(5), Some(4)],
            },
            Step::Create {
                node: 4,
                path: 2,
                content: 5,
            },
            Step::Partition { a: 6, b: 6 },
            Step::Mkdir { node: 2, dir: 0 },
            Step::Partition { a: 4, b: 6 },
            Step::Crash {
                node: 3,
                gap_secs: 34,
            },
            Step::Modify {
                node: 4,
                path: 10,
                content: 5,
            },
            Step::Online { node: 1 },
            Step::MassDelete {
                node: 4,
                fraction: 73,
            },
            Step::Settle { secs: 2 },
            Step::Modify {
                node: 2,
                path: 4,
                content: 4,
            },
            Step::Modify {
                node: 0,
                path: 2,
                content: 2,
            },
            Step::Online { node: 6 },
            Step::Chmod { node: 2, path: 8 },
            Step::Delete { node: 6, path: 3 },
            Step::Tier {
                a: 4,
                b: 6,
                tier: 1,
            },
            Step::Delete { node: 4, path: 8 },
            Step::Delete { node: 5, path: 4 },
            Step::Modify {
                node: 5,
                path: 1,
                content: 4,
            },
            Step::Create {
                node: 2,
                path: 4,
                content: 1,
            },
            Step::Offline { node: 3 },
            Step::Offline { node: 0 },
            Step::Symlink {
                node: 5,
                path: 6,
                target: 3,
            },
            Step::Settle { secs: 22 },
            Step::User {
                node: 3,
                action: crate::UserAction::ApproveAll,
                delay_secs: 32,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![Some(2), Some(2)],
            },
            Step::Settle { secs: 20 },
            Step::Rename {
                node: 6,
                from: 4,
                to: 6,
            },
            Step::Modify {
                node: 3,
                path: 4,
                content: 4,
            },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(3), Some(5), Some(5)],
            },
            Step::Modify {
                node: 7,
                path: 7,
                content: 1,
            },
            Step::Symlink {
                node: 6,
                path: 8,
                target: 1,
            },
            Step::Modify {
                node: 2,
                path: 11,
                content: 3,
            },
            Step::Rename {
                node: 1,
                from: 2,
                to: 8,
            },
            Step::Create {
                node: 6,
                path: 1,
                content: 5,
            },
            Step::Settle { secs: 16 },
            Step::Modify {
                node: 6,
                path: 0,
                content: 2,
            },
            Step::Heal { a: 4, b: 5 },
            Step::Delete { node: 7, path: 2 },
            Step::Partition { a: 5, b: 7 },
            Step::Settle { secs: 22 },
            Step::Delete { node: 3, path: 4 },
            Step::Rename {
                node: 1,
                from: 11,
                to: 2,
            },
            Step::Mkdir { node: 5, dir: 0 },
            Step::MassModify {
                node: 0,
                fraction: 71,
                content: 2,
            },
            Step::MassModify {
                node: 5,
                fraction: 88,
                content: 4,
            },
            Step::Create {
                node: 4,
                path: 8,
                content: 3,
            },
            Step::Create {
                node: 2,
                path: 11,
                content: 1,
            },
            Step::Create {
                node: 0,
                path: 5,
                content: 3,
            },
            Step::Delete { node: 3, path: 7 },
            Step::Heal { a: 6, b: 2 },
            Step::Create {
                node: 1,
                path: 10,
                content: 5,
            },
            Step::Rename {
                node: 1,
                from: 10,
                to: 3,
            },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(3), None, Some(5), Some(1)],
            },
            Step::Modify {
                node: 4,
                path: 3,
                content: 1,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::Revert,
                delay_secs: 6,
            },
            Step::Create {
                node: 5,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 7,
                path: 3,
                content: 4,
            },
            Step::Chmod { node: 4, path: 4 },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Modify {
                node: 0,
                path: 9,
                content: 5,
            },
            Step::Partition { a: 0, b: 4 },
            Step::Settle { secs: 15 },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Settle { secs: 8 },
            Step::MassModify {
                node: 5,
                fraction: 80,
                content: 1,
            },
            Step::Settle { secs: 37 },
            Step::Create {
                node: 7,
                path: 4,
                content: 3,
            },
            Step::Modify {
                node: 3,
                path: 4,
                content: 1,
            },
            Step::Tier {
                a: 3,
                b: 2,
                tier: 0,
            },
            Step::Create {
                node: 0,
                path: 2,
                content: 2,
            },
            Step::Touch { node: 7, path: 2 },
            Step::Offline { node: 0 },
            Step::Online { node: 0 },
            Step::Rmdir { node: 5, dir: 2 },
            Step::Tier {
                a: 3,
                b: 5,
                tier: 1,
            },
            Step::Offline { node: 5 },
            Step::Everywhere {
                path: 10,
                contents: vec![None, None, Some(5)],
            },
            Step::Modify {
                node: 5,
                path: 10,
                content: 5,
            },
            Step::Modify {
                node: 2,
                path: 10,
                content: 3,
            },
            Step::Symlink {
                node: 5,
                path: 10,
                target: 11,
            },
        ],
    );
}

/// One file edited on one node. On a peer, the fetch waited in an open
/// group of writes (§11) long enough to stall and be asked for again, so
/// two fetches of one version were in flight. The first report handed over
/// the content and the commit started; the second arrived while that commit
/// was in flight, set the want wanted again, and the next dispatch started
/// a second commit of the path (§7.5: at most one commit is in flight per
/// path, and it holds its path until the host reports it). The guard made
/// the second commit write nothing, so no invariant saw it; the checker
/// for commits in flight does.
#[test]
fn a_late_fetch_report_starts_no_second_commit() {
    passes(
        13,
        &[Step::Modify {
            node: 0,
            path: 0,
            content: 5,
        }],
    );
}

/// Found by the check for commits in flight with "Keep a released commit's
/// path until its own report" reverted, in the default sweep at 400 steps.
/// Re-pinned at seed 140, shrunk to 51 steps keeping the second commit,
/// after draft 54's refusal rules moved seed 15's history; the failure is a
/// second commit at d1/f4. On one node a restoring commit (§8.3) was in flight when
/// an observation of an occupant at its marked path cleared the mark and
/// released the want, as the revert exception says. In the same event the
/// engine re-derived the want from the observed change and started a new
/// commit of the path while the released one was still in flight. A rule
/// that releases a commit early releases the want, not the path (§7.5):
/// the new commit waits for the released one's report.
#[test]
fn a_commit_released_at_a_marked_path_bars_a_second_one() {
    passes(
        140,
        &[
            Step::Delete { node: 7, path: 11 },
            Step::Touch { node: 7, path: 0 },
            Step::Online { node: 2 },
            Step::Delete { node: 0, path: 5 },
            Step::Settle { secs: 24 },
            Step::Modify {
                node: 2,
                path: 1,
                content: 2,
            },
            Step::Settle { secs: 19 },
            Step::Modify {
                node: 7,
                path: 3,
                content: 1,
            },
            Step::Mkdir { node: 7, dir: 1 },
            Step::Heal { a: 3, b: 3 },
            Step::Heal { a: 6, b: 4 },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 4,
            },
            Step::Settle { secs: 6 },
            Step::Modify {
                node: 3,
                path: 9,
                content: 5,
            },
            Step::Chmod { node: 1, path: 7 },
            Step::Chmod { node: 7, path: 0 },
            Step::Delete { node: 1, path: 8 },
            Step::Chmod { node: 6, path: 8 },
            Step::Delete { node: 7, path: 5 },
            Step::Online { node: 4 },
            Step::Heal { a: 0, b: 6 },
            Step::Create {
                node: 4,
                path: 2,
                content: 4,
            },
            Step::Chmod { node: 6, path: 8 },
            Step::Partition { a: 0, b: 3 },
            Step::Create {
                node: 1,
                path: 4,
                content: 3,
            },
            Step::Create {
                node: 3,
                path: 0,
                content: 4,
            },
            Step::Settle { secs: 36 },
            Step::Create {
                node: 3,
                path: 6,
                content: 4,
            },
            Step::Heal { a: 4, b: 1 },
            Step::Delete { node: 1, path: 7 },
            Step::Create {
                node: 5,
                path: 11,
                content: 4,
            },
            Step::Everywhere {
                path: 2,
                contents: vec![None, Some(3)],
            },
            Step::Create {
                node: 2,
                path: 5,
                content: 4,
            },
            Step::Tier {
                a: 7,
                b: 3,
                tier: 2,
            },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(2), Some(3), Some(1)],
            },
            Step::Create {
                node: 3,
                path: 9,
                content: 1,
            },
            Step::MassModify {
                node: 6,
                fraction: 56,
                content: 5,
            },
            Step::Modify {
                node: 6,
                path: 0,
                content: 4,
            },
            Step::Heal { a: 5, b: 3 },
            Step::Delete { node: 1, path: 10 },
            Step::Delete { node: 4, path: 11 },
            Step::Settle { secs: 22 },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(2), Some(2), Some(5), Some(1), Some(4), None],
            },
            Step::Everywhere {
                path: 5,
                contents: vec![Some(2), None, None, Some(3), Some(1)],
            },
            Step::Rmdir { node: 5, dir: 1 },
            Step::Mkdir { node: 7, dir: 2 },
            Step::Modify {
                node: 6,
                path: 5,
                content: 5,
            },
            Step::Create {
                node: 7,
                path: 10,
                content: 2,
            },
            Step::User {
                node: 6,
                action: crate::UserAction::Revert,
                delay_secs: 35,
            },
            Step::Offline { node: 6 },
            Step::Modify {
                node: 1,
                path: 4,
                content: 3,
            },
        ],
    );
}

/// Found by the check for commits in flight with an index-only adoption
/// let through at a path a released commit holds, in the default sweep at
/// 400 steps; re-pinned at seed 28 with the default knobs, shrunk to 111
/// steps, after draft 54's refusal rules moved seed 100's history, at a conflict
/// copy of f0: a released commit held the path when the engine
/// changed its record, though the commit could still land its own version
/// there (§7.5).
#[test]
fn a_released_commit_bars_an_adoption_at_its_path() {
    passes(
        28,
        &[
            Step::Offline { node: 4 },
            Step::User {
                node: 6,
                action: crate::UserAction::DenyAll,
                delay_secs: 20,
            },
            Step::Tier {
                a: 7,
                b: 3,
                tier: 0,
            },
            Step::MassDelete {
                node: 3,
                fraction: 62,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(1), Some(4), Some(1), Some(1), Some(2)],
            },
            Step::Delete { node: 2, path: 8 },
            Step::Modify {
                node: 1,
                path: 9,
                content: 2,
            },
            Step::Create {
                node: 6,
                path: 5,
                content: 2,
            },
            Step::Modify {
                node: 1,
                path: 0,
                content: 2,
            },
            Step::Delete { node: 1, path: 9 },
            Step::Create {
                node: 1,
                path: 5,
                content: 2,
            },
            Step::Create {
                node: 1,
                path: 0,
                content: 1,
            },
            Step::MassModify {
                node: 1,
                fraction: 70,
                content: 2,
            },
            Step::Settle { secs: 11 },
            Step::Tier {
                a: 5,
                b: 1,
                tier: 2,
            },
            Step::Create {
                node: 0,
                path: 6,
                content: 5,
            },
            Step::MassModify {
                node: 0,
                fraction: 55,
                content: 3,
            },
            Step::Delete { node: 1, path: 6 },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(2), Some(3), None, Some(3), Some(4), Some(4)],
            },
            Step::Mkdir { node: 6, dir: 1 },
            Step::Rename {
                node: 4,
                from: 6,
                to: 5,
            },
            Step::Heal { a: 5, b: 2 },
            Step::User {
                node: 4,
                action: crate::UserAction::ApproveAll,
                delay_secs: 3,
            },
            Step::Touch { node: 7, path: 7 },
            Step::MassDelete {
                node: 6,
                fraction: 80,
            },
            Step::Modify {
                node: 3,
                path: 8,
                content: 2,
            },
            Step::User {
                node: 0,
                action: crate::UserAction::ApproveAll,
                delay_secs: 3,
            },
            Step::Rename {
                node: 0,
                from: 4,
                to: 8,
            },
            Step::Everywhere {
                path: 6,
                contents: vec![Some(1), Some(5)],
            },
            Step::Create {
                node: 7,
                path: 1,
                content: 3,
            },
            Step::Partition { a: 7, b: 1 },
            Step::Everywhere {
                path: 8,
                contents: vec![Some(5), Some(3), Some(1), Some(2), None],
            },
            Step::Online { node: 1 },
            Step::Heal { a: 2, b: 3 },
            Step::Online { node: 6 },
            Step::Delete { node: 6, path: 11 },
            Step::Online { node: 2 },
            Step::Online { node: 3 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 3,
            },
            Step::Create {
                node: 3,
                path: 4,
                content: 1,
            },
            Step::Heal { a: 1, b: 1 },
            Step::Modify {
                node: 2,
                path: 1,
                content: 2,
            },
            Step::Modify {
                node: 6,
                path: 1,
                content: 5,
            },
            Step::Mkdir { node: 3, dir: 2 },
            Step::Settle { secs: 3 },
            Step::Delete { node: 1, path: 11 },
            Step::Tier {
                a: 4,
                b: 0,
                tier: 0,
            },
            Step::Rename {
                node: 5,
                from: 4,
                to: 10,
            },
            Step::Modify {
                node: 6,
                path: 3,
                content: 4,
            },
            Step::Modify {
                node: 0,
                path: 4,
                content: 5,
            },
            Step::Create {
                node: 4,
                path: 2,
                content: 4,
            },
            Step::Partition { a: 4, b: 2 },
            Step::Crash {
                node: 2,
                gap_secs: 51,
            },
            Step::Mkdir { node: 0, dir: 2 },
            Step::Create {
                node: 0,
                path: 6,
                content: 4,
            },
            Step::Heal { a: 5, b: 5 },
            Step::Everywhere {
                path: 3,
                contents: vec![Some(4), None, Some(3), None, Some(1)],
            },
            Step::Touch { node: 4, path: 6 },
            Step::Create {
                node: 3,
                path: 7,
                content: 4,
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(3), Some(4), None, None, None],
            },
            Step::Everywhere {
                path: 4,
                contents: vec![Some(3), Some(4), None, None],
            },
            Step::Modify {
                node: 5,
                path: 6,
                content: 1,
            },
            Step::Touch { node: 7, path: 8 },
            Step::Rmdir { node: 5, dir: 0 },
            Step::Chmod { node: 4, path: 6 },
            Step::Delete { node: 3, path: 1 },
            Step::Modify {
                node: 5,
                path: 4,
                content: 1,
            },
            Step::User {
                node: 5,
                action: crate::UserAction::ApproveAll,
                delay_secs: 22,
            },
            Step::Partition { a: 2, b: 4 },
            Step::Everywhere {
                path: 9,
                contents: vec![None, Some(3)],
            },
            Step::Delete { node: 5, path: 2 },
            Step::Crash {
                node: 4,
                gap_secs: 55,
            },
            Step::Symlink {
                node: 0,
                path: 2,
                target: 3,
            },
            Step::Rmdir { node: 4, dir: 0 },
            Step::Tier {
                a: 7,
                b: 2,
                tier: 1,
            },
            Step::Settle { secs: 29 },
            Step::Offline { node: 6 },
            Step::Partition { a: 6, b: 1 },
            Step::Create {
                node: 7,
                path: 1,
                content: 3,
            },
            Step::Delete { node: 3, path: 0 },
            Step::Settle { secs: 29 },
            Step::MassModify {
                node: 7,
                fraction: 79,
                content: 3,
            },
            Step::Delete { node: 4, path: 3 },
            Step::Offline { node: 2 },
            Step::Everywhere {
                path: 1,
                contents: vec![Some(3), None, Some(1), Some(4)],
            },
            Step::Partition { a: 2, b: 6 },
            Step::Create {
                node: 5,
                path: 0,
                content: 5,
            },
            Step::Crash {
                node: 1,
                gap_secs: 4,
            },
            Step::Modify {
                node: 0,
                path: 3,
                content: 5,
            },
            Step::Delete { node: 0, path: 5 },
            Step::Crash {
                node: 0,
                gap_secs: 42,
            },
            Step::Heal { a: 7, b: 3 },
            Step::Crash {
                node: 7,
                gap_secs: 85,
            },
            Step::Delete { node: 6, path: 2 },
            Step::Modify {
                node: 3,
                path: 5,
                content: 5,
            },
            Step::Tier {
                a: 4,
                b: 3,
                tier: 2,
            },
            Step::Mkdir { node: 0, dir: 1 },
            Step::Partition { a: 2, b: 5 },
            Step::Rmdir { node: 5, dir: 1 },
            Step::Modify {
                node: 4,
                path: 2,
                content: 2,
            },
            Step::Settle { secs: 11 },
            Step::Tier {
                a: 6,
                b: 2,
                tier: 1,
            },
            Step::Modify {
                node: 4,
                path: 1,
                content: 1,
            },
            Step::Create {
                node: 5,
                path: 1,
                content: 5,
            },
            Step::Delete { node: 6, path: 0 },
            Step::Everywhere {
                path: 4,
                contents: vec![None, Some(5), Some(5)],
            },
            Step::Everywhere {
                path: 0,
                contents: vec![Some(2), Some(5), Some(2)],
            },
            Step::Create {
                node: 2,
                path: 8,
                content: 5,
            },
            Step::Offline { node: 6 },
            Step::Mkdir { node: 0, dir: 0 },
            Step::Modify {
                node: 7,
                path: 1,
                content: 2,
            },
        ],
    );
}
