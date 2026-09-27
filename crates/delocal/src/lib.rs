//! `delocal`: the daemon's machinery, as a library the binary uses
//! (DESIGN.md Appendix B).
//!
//! Phase 2 builds this crate up one pull request at a time (§15). The engine
//! (`delocal-engine`) decides; this crate does the I/O it asks for. So far:
//!
//! - [`fs`]: the filesystem layer, every operation on a folder (§14.2)
//! - [`names`]: names on disk and the index paths they map to (§7.1, §7.3)
//! - [`store`]: the SQLite database, the persisted parts and the host's
//!   tables (§11, §8.5)

// Tests may unwrap and expect (CLAUDE.md conventions); production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod fs;
pub mod names;
pub mod store;
