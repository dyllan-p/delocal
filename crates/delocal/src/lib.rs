//! `delocal`: the daemon's machinery, as a library the binary uses
//! (DESIGN.md Appendix B).
//!
//! Phase 2 builds this crate up one pull request at a time (§15). The engine
//! (`delocal-engine`) decides; this crate does the I/O it asks for. So far
//! there is nothing here yet.

// Tests may unwrap and expect (CLAUDE.md conventions); production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
