//! The daemon (DESIGN.md §10 `delocal daemon`, §11, §7.8): the long-running
//! process the service runs.
//!
//! It is built up over the next commits. So far, the pieces it starts with:
//!
//! - [`clock`]: the `now` it hands the engine, the wall clock at start plus
//!   the monotonic time since (§7.8)
//! - [`node`]: `node.json`, this installation's node ID (§5)
//! - [`lock`]: one daemon per state directory (§11)
//! - [`limits`]: the open-file limit, raised at start
//! - [`logs`]: rotating log files in `logs/` (§11)

pub mod clock;
pub mod limits;
pub mod lock;
pub mod logs;
pub mod node;
