//! Tunables the binary exposes as flags, so a failing seed can be replayed
//! with the same world and a class of failure isolated.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Everything about the world that is not the seed or the steps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Knobs {
    /// Probability that a fetch's bytes arrive corrupted (`HashMismatch`).
    pub corruption: f64,
    /// Probability that a filesystem change produces no watcher event and
    /// is left for the next full scan.
    pub drop_watcher: f64,
    /// Message delay range in milliseconds, inclusive.
    pub delay_ms: (u64, u64),
    /// Probability that a completed commit's report is lost to a crash
    /// landing between the rename and `Applied` (§13).
    pub crash_after_rename: f64,
    /// Probability that a commit which displaces a file crashes between the
    /// displacement and the rename that puts the new content in place
    /// (§7.5, "Two renames, one commit"). The host's commit journal undoes
    /// the displacement at the restart, before the first scan.
    pub crash_between_renames: f64,
    /// The most events a group of writes waits for, after the event that
    /// opened it, before it becomes durable (§11 group commit). The effects
    /// of the group's events wait with it, and a crash loses both. Each
    /// group draws its lag from 0 to this; 0 makes every event's writes
    /// durable when the event ends.
    pub group_commit_lag: u32,
    /// A displaced directory takes everything under it, as a real rename
    /// does (§7.6, §14.1): the children's old records are tombstoned by the
    /// next scan and the moved children appear as adds. Off, only the
    /// directory's own entry moves, and a non-empty directory cannot be
    /// displaced to a conflict copy by a delete.
    pub displace_subtrees: bool,
    /// Number of nodes, or `None` to draw 2 to 8 from the seed.
    pub nodes: Option<u8>,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            corruption: 0.0,
            drop_watcher: 0.3,
            delay_ms: (5, 800),
            crash_after_rename: 0.1,
            crash_between_renames: 0.05,
            group_commit_lag: 4,
            displace_subtrees: true,
            nodes: None,
        }
    }
}

impl fmt::Display for Knobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "--corruption {} --drop-watcher {} --delay-ms {}..{} --crash-after-rename {} --crash-between-renames {} --group-commit-lag {} --displace-subtrees {}",
            self.corruption,
            self.drop_watcher,
            self.delay_ms.0,
            self.delay_ms.1,
            self.crash_after_rename,
            self.crash_between_renames,
            self.group_commit_lag,
            self.displace_subtrees
        )?;
        if let Some(n) = self.nodes {
            write!(f, " --nodes {n}")?;
        }
        Ok(())
    }
}
