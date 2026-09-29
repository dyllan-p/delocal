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
    /// Probability that a full scan cannot inspect a path it reaches, a
    /// file it cannot read or a directory it cannot list, and reports it
    /// `Skipped` (§7.3), the directory's contents unreported. With the same
    /// probability per scan, a tracked path becomes ignored for the next
    /// one to four of its node's scans, as if a rule had been added to
    /// `.delocalignore` and later taken out: while it is, scans and the
    /// watcher report it `Skipped`. The final phase's scans skip nothing.
    pub skip: f64,
    /// Probability that a fetch or a commit fails on its machine with an
    /// I/O error (§7.5 "local failures"). A fetch fails as its bytes are
    /// written. A `Write` fails at the rename that puts the new content in
    /// place, after the displacement, which the host undoes before it
    /// reports, and half such failures take the temp file with them; a
    /// `Remove` or `SetMeta` fails before it changes anything.
    pub io_failure: f64,
    /// Probability, per fetch or `Write` that needs space, that its node's
    /// disk fills up (§7.5). It stays full for 30 s to 10 min, and while it
    /// is, every fetch and every `Write` there fails with `DiskFull`, a
    /// `Write` at its rename as above. The host checks for space every 30 s
    /// while the folder's inbound is paused and reports `SpaceRecovered`
    /// once there is some. The final phase frees every disk.
    pub disk_fill: f64,
    /// Probability that a step spells the file name it touches in upper
    /// case (`F3` for `f3`), so that paths differing only by case exist.
    /// Above 0 the first node's filesystem ignores case (§7.6): its user's
    /// writes land on an existing file of either spelling, and its host
    /// reports `CaseCollision` for a write whose path differs only by case
    /// from a live record. In the final phase a user on a case-sensitive
    /// node resolves every such pair, as §7.6 says the user does.
    pub case_variants: f64,
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
            skip: 0.002,
            io_failure: 0.0,
            disk_fill: 0.0,
            case_variants: 0.0,
            nodes: None,
        }
    }
}

impl fmt::Display for Knobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "--corruption {} --drop-watcher {} --delay-ms {}..{} --crash-after-rename {} --crash-between-renames {} --group-commit-lag {} --displace-subtrees {} --skip {} --io-failure {} --disk-fill {} --case-variants {}",
            self.corruption,
            self.drop_watcher,
            self.delay_ms.0,
            self.delay_ms.1,
            self.crash_after_rename,
            self.crash_between_renames,
            self.group_commit_lag,
            self.displace_subtrees,
            self.skip,
            self.io_failure,
            self.disk_fill,
            self.case_variants
        )?;
        if let Some(n) = self.nodes {
            write!(f, " --nodes {n}")?;
        }
        Ok(())
    }
}
