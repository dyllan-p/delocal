//! The random steps a run applies (DESIGN.md §14.1), both as a proptest
//! strategy (for shrinking) and as a PRNG-driven generator (for the
//! binary's seed sweeps). Nodes are indices modulo the node count; paths
//! come from a small vocabulary so the same path is edited on several nodes
//! often; content is a byte seed the host expands to real bytes.

use proptest::prelude::*;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::knobs::Knobs;

/// What the simulated user does to held or paused work (§8.2, §8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserAction {
    ApproveAll,
    DenyAll,
    Revert,
    /// Tighten or loosen the brake and fetch limits.
    Rules {
        hold_count: u64,
        hold_pct: u8,
    },
}

/// One step of a run. Node numbers are taken modulo the node count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Create {
        node: u8,
        path: u8,
        content: u8,
    },
    Modify {
        node: u8,
        path: u8,
        content: u8,
    },
    Delete {
        node: u8,
        path: u8,
    },
    Rename {
        node: u8,
        from: u8,
        to: u8,
    },
    Touch {
        node: u8,
        path: u8,
    },
    Chmod {
        node: u8,
        path: u8,
    },
    Mkdir {
        node: u8,
        dir: u8,
    },
    Rmdir {
        node: u8,
        dir: u8,
    },
    Symlink {
        node: u8,
        path: u8,
        target: u8,
    },
    /// The same path edited on every online node in one step, each with its
    /// own content (`None` deletes).
    Everywhere {
        path: u8,
        contents: Vec<Option<u8>>,
    },
    Partition {
        a: u8,
        b: u8,
    },
    Heal {
        a: u8,
        b: u8,
    },
    Tier {
        a: u8,
        b: u8,
        tier: u8,
    },
    Offline {
        node: u8,
    },
    Online {
        node: u8,
    },
    Crash {
        node: u8,
        gap_secs: u32,
    },
    MassDelete {
        node: u8,
        fraction: u8,
    },
    MassModify {
        node: u8,
        fraction: u8,
        content: u8,
    },
    User {
        node: u8,
        action: UserAction,
        delay_secs: u32,
    },
    /// Let the world run for this long with nothing new.
    Settle {
        secs: u32,
    },
}

/// Number of distinct file paths and directories in the vocabulary.
pub const PATHS: u8 = 12;
pub const DIRS: u8 = 3;

/// A file path from the vocabulary: `dN/fM` or top-level `fM`.
pub fn path_name(i: u8) -> String {
    let i = i % PATHS;
    if i < 4 {
        format!("f{i}")
    } else {
        format!("d{}/f{i}", i % DIRS)
    }
}

/// A directory path from the vocabulary.
pub fn dir_name(i: u8) -> String {
    format!("d{}", i % DIRS)
}

/// The proptest strategy: what `cargo test` shrinks over.
pub fn step() -> impl Strategy<Value = Step> {
    let node = 0u8..8;
    let path = 0u8..PATHS;
    prop_oneof![
        8 => (node.clone(), path.clone(), 1u8..6).prop_map(|(node, path, content)| Step::Create { node, path, content }),
        8 => (node.clone(), path.clone(), 1u8..6).prop_map(|(node, path, content)| Step::Modify { node, path, content }),
        5 => (node.clone(), path.clone()).prop_map(|(node, path)| Step::Delete { node, path }),
        2 => (node.clone(), path.clone(), path.clone()).prop_map(|(node, from, to)| Step::Rename { node, from, to }),
        2 => (node.clone(), path.clone()).prop_map(|(node, path)| Step::Touch { node, path }),
        2 => (node.clone(), path.clone()).prop_map(|(node, path)| Step::Chmod { node, path }),
        2 => (node.clone(), 0u8..DIRS).prop_map(|(node, dir)| Step::Mkdir { node, dir }),
        1 => (node.clone(), 0u8..DIRS).prop_map(|(node, dir)| Step::Rmdir { node, dir }),
        1 => (node.clone(), path.clone(), path.clone()).prop_map(|(node, path, target)| Step::Symlink { node, path, target }),
        3 => (path.clone(), prop::collection::vec(prop::option::of(1u8..6), 2..8)).prop_map(|(path, contents)| Step::Everywhere { path, contents }),
        3 => (node.clone(), node.clone()).prop_map(|(a, b)| Step::Partition { a, b }),
        3 => (node.clone(), node.clone()).prop_map(|(a, b)| Step::Heal { a, b }),
        2 => (node.clone(), node.clone(), 0u8..3).prop_map(|(a, b, tier)| Step::Tier { a, b, tier }),
        2 => node.clone().prop_map(|node| Step::Offline { node }),
        2 => node.clone().prop_map(|node| Step::Online { node }),
        2 => (node.clone(), 1u32..120).prop_map(|(node, gap_secs)| Step::Crash { node, gap_secs }),
        1 => (node.clone(), 50u8..100).prop_map(|(node, fraction)| Step::MassDelete { node, fraction }),
        1 => (node.clone(), 50u8..100, 1u8..6).prop_map(|(node, fraction, content)| Step::MassModify { node, fraction, content }),
        3 => (node.clone(), user_action(), 0u32..60).prop_map(|(node, action, delay_secs)| Step::User { node, action, delay_secs }),
        4 => (1u32..40).prop_map(|secs| Step::Settle { secs }),
    ]
}

fn user_action() -> impl Strategy<Value = UserAction> {
    prop_oneof![
        4 => Just(UserAction::ApproveAll),
        2 => Just(UserAction::DenyAll),
        2 => Just(UserAction::Revert),
        1 => (0u64..6, 0u8..60).prop_map(|(hold_count, hold_pct)| UserAction::Rules { hold_count, hold_pct }),
    ]
}

/// The PRNG-driven generator for the binary: `count` steps from `seed`,
/// with the same weights as [`step`].
pub fn generate(seed: u64, _knobs: &Knobs, count: usize) -> Vec<Step> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x5eed_5eed_5eed_5eed);
    (0..count).map(|_| one(&mut rng)).collect()
}

fn one(rng: &mut ChaCha8Rng) -> Step {
    let node = rng.random_range(0u8..8);
    let path = rng.random_range(0u8..PATHS);
    match rng.random_range(0u32..57) {
        0..8 => Step::Create {
            node,
            path,
            content: rng.random_range(1u8..6),
        },
        8..16 => Step::Modify {
            node,
            path,
            content: rng.random_range(1u8..6),
        },
        16..21 => Step::Delete { node, path },
        21..23 => Step::Rename {
            node,
            from: path,
            to: rng.random_range(0u8..PATHS),
        },
        23..25 => Step::Touch { node, path },
        25..27 => Step::Chmod { node, path },
        27..29 => Step::Mkdir {
            node,
            dir: rng.random_range(0u8..DIRS),
        },
        29 => Step::Rmdir {
            node,
            dir: rng.random_range(0u8..DIRS),
        },
        30 => Step::Symlink {
            node,
            path,
            target: rng.random_range(0u8..PATHS),
        },
        31..34 => {
            let n = rng.random_range(2usize..8);
            let contents = (0..n)
                .map(|_| {
                    if rng.random_range(0u8..4) == 0 {
                        None
                    } else {
                        Some(rng.random_range(1u8..6))
                    }
                })
                .collect();
            Step::Everywhere { path, contents }
        }
        34..37 => Step::Partition {
            a: node,
            b: rng.random_range(0u8..8),
        },
        37..40 => Step::Heal {
            a: node,
            b: rng.random_range(0u8..8),
        },
        40..42 => Step::Tier {
            a: node,
            b: rng.random_range(0u8..8),
            tier: rng.random_range(0u8..3),
        },
        42..44 => Step::Offline { node },
        44..46 => Step::Online { node },
        46..48 => Step::Crash {
            node,
            gap_secs: rng.random_range(1u32..120),
        },
        48 => Step::MassDelete {
            node,
            fraction: rng.random_range(50u8..100),
        },
        49 => Step::MassModify {
            node,
            fraction: rng.random_range(50u8..100),
            content: rng.random_range(1u8..6),
        },
        50..53 => Step::User {
            node,
            action: match rng.random_range(0u8..9) {
                0..4 => UserAction::ApproveAll,
                4..6 => UserAction::DenyAll,
                6..8 => UserAction::Revert,
                _ => UserAction::Rules {
                    hold_count: rng.random_range(0u64..6),
                    hold_pct: rng.random_range(0u8..60),
                },
            },
            delay_secs: rng.random_range(0u32..60),
        },
        _ => Step::Settle {
            secs: rng.random_range(1u32..40),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic_and_paths_are_valid() {
        let a = generate(42, &Knobs::default(), 100);
        let b = generate(42, &Knobs::default(), 100);
        assert_eq!(a, b);
        assert_ne!(a, generate(43, &Knobs::default(), 100));
        for i in 0..PATHS {
            delocal_engine::RelPath::new(path_name(i)).unwrap();
        }
        for i in 0..DIRS {
            delocal_engine::RelPath::new(dir_name(i)).unwrap();
        }
    }
}
