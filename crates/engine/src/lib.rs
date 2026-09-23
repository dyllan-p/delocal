//! `delocal-engine`: the pure sync engine (DESIGN.md §7, §8 and Appendix B).
//!
//! The engine takes events (scan results, incoming messages, timer ticks) and
//! returns actions (write file, send message, record history). It does no I/O
//! and depends on nothing that does. Phase 0 contains only the version
//! constant; Phase 1 adds the index, versions, batches, conflicts, the brake
//! and the simulator (§15).

// Tests may unwrap and expect (CLAUDE.md conventions); production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// The version of delocal, taken from the workspace at compile time.
///
/// Both crates inherit `workspace.package.version`, so this is also the
/// version the `delocal` binary reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_is_semver_shaped() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "expected major.minor.patch, got {VERSION}");
        for part in parts {
            part.parse::<u64>()
                .expect("each version component is a number");
        }
    }
}
