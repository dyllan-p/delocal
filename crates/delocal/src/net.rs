//! The transport (DESIGN.md §6.3, §12, §5): TCP connections between this
//! user's machines, one per pair, and the messages they carry.
//!
//! So far:
//!
//! - [`conn`]: one connection: `Hello` and `Goodbye`, pings every 15 s,
//!   and the drop after 45 s of silence (§6.3)
//! - [`frame`]: each message as a length and its postcard bytes, capped at
//!   16 MiB (§6.3)
//! - [`wire`]: the protocol's messages, and the envelope the engine's
//!   travel in (§12)
//!
//! Not here yet: listening, dialing and the rules for both (§6.3, §5),
//! the daemon's use of them, discovery (§6.2), the binding check (§5), and
//! file transfer (§7.5).

pub mod conn;
pub mod frame;
pub mod wire;
