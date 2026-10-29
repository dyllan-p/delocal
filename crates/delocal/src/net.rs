//! The transport (DESIGN.md §6.3, §12, §5): TCP connections between this
//! user's machines, one per pair, and the messages they carry.
//!
//! So far:
//!
//! - [`frame`]: each message as a length and its postcard bytes, capped at
//!   16 MiB (§6.3)
//! - [`wire`]: the protocol's messages, and the envelope the engine's
//!   travel in (§12)
//!
//! Not here yet: connections (§6.3), the daemon's use of them, discovery
//! (§6.2), the binding check (§5), and file transfer (§7.5).

pub mod frame;
pub mod wire;
