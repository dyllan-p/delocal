//! The scanner (DESIGN.md §7.3): a full scan of a folder, reported to the
//! engine as a bracket of `Scanned` events.
//!
//! So far:
//!
//! - [`ignore_rules`]: the defaults and `.delocalignore`, and which paths
//!   they ignore

pub mod ignore_rules;
