//! The scanner (DESIGN.md §7.3): a full scan of a folder, reported to the
//! engine as a bracket of `Scanned` events.
//!
//! So far:
//!
//! - [`hash`]: BLAKE3 of a file, streamed, and the stability check
//! - [`ignore_rules`]: the defaults and `.delocalignore`, and which paths
//!   they ignore
//! - [`ordered`]: work on several threads, results in the order it was
//!   handed out

pub mod hash;
pub mod ignore_rules;
pub mod ordered;
