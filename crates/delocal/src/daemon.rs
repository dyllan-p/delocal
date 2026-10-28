//! The daemon (DESIGN.md §10 `delocal daemon`, §11, §7.8): the long-running
//! process the service runs.
//!
//! It is built up over the next commits. So far, the pieces it starts with:
//!
//! - [`clock`]: the `now` it hands the engine, the wall clock at start plus
//!   the monotonic time since (§7.8)

pub mod clock;
