//! The `delocal` binary (DESIGN.md §10 and Appendix B).
//!
//! Phase 0: prints the version and exits. The daemon, CLI, filesystem, SQLite,
//! networking, Tailscale and service-install modules arrive in Phases 2 to 4.

// Tests may unwrap and expect (CLAUDE.md conventions); production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

fn main() {
    println!("delocal {}", delocal_engine::VERSION);
}
