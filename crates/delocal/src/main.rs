//! The `delocal` binary (DESIGN.md §10 and Appendix B).
//!
//! Phase 0: prints the version and exits. The daemon and the CLI arrive in
//! Phases 2 to 4, built on the `delocal` library (`src/lib.rs`), which holds
//! the filesystem, SQLite, networking and Tailscale machinery.

// Tests may unwrap and expect (CLAUDE.md conventions); production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

fn main() {
    println!("delocal {}", delocal_engine::VERSION);
}
