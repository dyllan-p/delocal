//! `delocal`: the daemon's machinery, as a library the binary uses
//! (DESIGN.md Appendix B).
//!
//! Phase 2 builds this crate up one pull request at a time (§15). The engine
//! (`delocal-engine`) decides; this crate does the I/O it asks for. So far:
//!
//! - [`commit`]: the commit path, the host side of `Write`, `Remove`,
//!   `SetMeta` and `MoveToTrash`, and its journal (§7.5, §8.3)
//! - [`daemon`]: `delocal daemon`, the engine driven by real events: its
//!   start, the engine task, group commit with effects after durability,
//!   the executor, the timers and the watcher (§10, §11, §7.8, §7.3)
//! - [`fs`]: the filesystem layer, every operation on a folder (§14.2)
//! - [`names`]: names on disk and the index paths they map to (§7.1, §7.3)
//! - [`reach`]: an index path on disk: its names, the walk to it that never
//!   crosses a mount point, and what a filesystem that folds names answers
//!   (§7.3, §7.5)
//! - [`scan`]: the full scan, its fast path, hashing, ignore rules and root
//!   guard (§7.3)
//! - [`store`]: the SQLite database, the persisted parts and the host's
//!   tables (§11, §8.5)
//! - [`tailscale`]: everything delocal asks of Tailscale, behind one trait,
//!   its fake, and connection tiers (§6.1, §6.4)
//! - [`tmp`]: `.delocal/tmp/`, its names and what goes at start (§7.5)
//! - [`trash`]: `.delocal/trash/`, its names, rows and pruning (§8.4)

// Tests may unwrap and expect (CLAUDE.md conventions); production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod commit;
pub mod daemon;
pub mod fs;
pub mod names;
pub mod reach;
pub mod scan;
pub mod store;
pub mod tailscale;
pub mod tmp;
pub mod trash;
