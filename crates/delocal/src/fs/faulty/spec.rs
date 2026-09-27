//! Fault specs (DESIGN.md §14.2): the scripted rules a
//! [`FaultyFs`](super::FaultyFs) follows, read from JSON when the daemon
//! spawns.
//!
//! A rule is an operation, a path pattern, a trigger and a fault:
//!
//! ```json
//! { "rules": [
//!     { "op": "write",  "path": "**/.delocal/tmp/*", "at": { "offset": 1048576 }, "fail": "ENOSPC" },
//!     { "op": "rename", "path": "**/.delocal/trash/**", "at": { "call": 2 },    "fail": "EXDEV" },
//!     { "op": "lstat",  "path": "**/private/*",       "at": { "from_call": 1 }, "fail": "EACCES" }
//! ] }
//! ```
//!
//! - `op` is one [`Op`]: an [`Fs`](crate::fs::Fs) method, or `read`,
//!   `write` or `sync` on an open file.
//! - `path` is a [`Pattern`](super::pattern::Pattern) over the host path. A
//!   rename matches if either of its paths does; a symlink matches on the
//!   link, not the target; a read, write or sync on the path the file was
//!   opened with.
//! - `at` is `{"call": n}`, the rule's nth matching call only, counting from
//!   1; `{"from_call": n}`, its nth matching call and every one after; or
//!   `{"offset": n}`, for `read` and `write` only: a call that would cross
//!   byte `n` of the file moves the bytes before it and returns short, and
//!   any call at or past `n` fails.
//! - `fail` is `ENOSPC`, `EIO`, `EACCES`, or `EXDEV`, the failing rename:
//!   the error only a rename gives, when the filesystem will not move the
//!   entry (a mount point inside the folder, §8.4). `EXDEV` is accepted on
//!   `rename` only; the other three fail a rename too. A failed call does
//!   nothing, except that `ENOSPC` on `available_space` reports 0 bytes
//!   free instead of failing, which is what a full disk looks like there.
//!
//! Unknown fields are refused, so a misspelt rule is an error, not a rule
//! that silently never fires.

use std::fmt;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A whole fault spec: the rules, in order. See the module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub rules: Vec<Rule>,
}

/// One rule. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub op: Op,
    pub path: String,
    pub at: Trigger,
    pub fail: Fault,
}

/// An operation a rule applies to: every [`Fs`](crate::fs::Fs) method, and
/// the three calls on an open file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    ReadDir,
    Lstat,
    ReadLink,
    OpenRead,
    CreateNew,
    OpenAppend,
    SyncDir,
    Rename,
    RemoveFile,
    RemoveDir,
    CreateDir,
    Symlink,
    SetMtime,
    SetMode,
    AvailableSpace,
    /// `Read::read` on a file from `open_read`.
    Read,
    /// `Write::write` on a file from `create_new` or `open_append`.
    Write,
    /// `WriteFile::sync`.
    Sync,
}

/// When a rule fires. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Call(u64),
    FromCall(u64),
    Offset(u64),
}

/// The error a rule injects. Spelt as the errno in a spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Fault {
    #[serde(rename = "ENOSPC")]
    Enospc,
    #[serde(rename = "EIO")]
    Eio,
    #[serde(rename = "EACCES")]
    Eacces,
    #[serde(rename = "EXDEV")]
    Exdev,
}

impl Fault {
    /// The errno. These four have the same numbers on Linux and macOS,
    /// which both inherit them from Version 7 Unix, so no `libc` is needed.
    pub fn errno(self) -> i32 {
        match self {
            Self::Eio => 5,
            Self::Eacces => 13,
            Self::Exdev => 18,
            Self::Enospc => 28,
        }
    }

    /// The error the operating system itself would return, so a caller
    /// cannot tell an injected fault from a real one.
    pub fn error(self) -> io::Error {
        io::Error::from_raw_os_error(self.errno())
    }
}

/// Why a spec was refused.
#[derive(Debug)]
pub enum SpecError {
    /// The spec file could not be read.
    Read(io::Error),
    /// Not JSON, or not the shape of a spec.
    Json(serde_json::Error),
    /// Rule `rule` (counting from 0) is well-formed but means nothing.
    Invalid { rule: usize, reason: &'static str },
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "cannot read the fault spec: {e}"),
            Self::Json(e) => write!(f, "fault spec is not valid: {e}"),
            Self::Invalid { rule, reason } => write!(f, "fault rule {rule}: {reason}"),
        }
    }
}

impl std::error::Error for SpecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::Invalid { .. } => None,
        }
    }
}

impl Spec {
    /// Parse and validate a spec from JSON.
    pub fn parse(json: &str) -> Result<Self, SpecError> {
        let spec: Self = serde_json::from_str(json).map_err(SpecError::Json)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Read, parse and validate the spec file at `path`. The spec is the
    /// test's, not the folder's, so it is read with `std` and never through
    /// an [`Fs`](crate::fs::Fs).
    pub fn load(path: &Path) -> Result<Self, SpecError> {
        Self::parse(&std::fs::read_to_string(path).map_err(SpecError::Read)?)
    }

    /// Refuse rules that parse but could never mean anything.
    pub fn validate(&self) -> Result<(), SpecError> {
        for (index, rule) in self.rules.iter().enumerate() {
            let invalid = |reason| {
                Err(SpecError::Invalid {
                    rule: index,
                    reason,
                })
            };
            if rule.path.is_empty() {
                return invalid("the path pattern is empty and would match nothing");
            }
            match rule.at {
                Trigger::Call(0) | Trigger::FromCall(0) => {
                    return invalid("calls count from 1");
                }
                Trigger::Offset(_) if !matches!(rule.op, Op::Read | Op::Write) => {
                    return invalid("an offset applies to read and write only");
                }
                _ => {}
            }
            if rule.fail == Fault::Exdev && rule.op != Op::Rename {
                return invalid("EXDEV applies to rename only");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_example_parses() {
        let spec = Spec::parse(
            r#"{ "rules": [
                { "op": "write",  "path": "**/.delocal/tmp/*", "at": { "offset": 1048576 }, "fail": "ENOSPC" },
                { "op": "rename", "path": "**/.delocal/trash/**", "at": { "call": 2 },    "fail": "EXDEV" },
                { "op": "lstat",  "path": "**/private/*",       "at": { "from_call": 1 }, "fail": "EACCES" }
            ] }"#,
        )
        .unwrap();
        assert_eq!(
            spec.rules,
            [
                Rule {
                    op: Op::Write,
                    path: "**/.delocal/tmp/*".into(),
                    at: Trigger::Offset(1_048_576),
                    fail: Fault::Enospc,
                },
                Rule {
                    op: Op::Rename,
                    path: "**/.delocal/trash/**".into(),
                    at: Trigger::Call(2),
                    fail: Fault::Exdev,
                },
                Rule {
                    op: Op::Lstat,
                    path: "**/private/*".into(),
                    at: Trigger::FromCall(1),
                    fail: Fault::Eacces,
                },
            ]
        );
        assert_eq!(Spec::parse(r#"{"rules": []}"#).unwrap(), Spec::default());
    }

    #[test]
    fn every_op_has_its_snake_case_name() {
        let names = [
            (Op::ReadDir, "read_dir"),
            (Op::Lstat, "lstat"),
            (Op::ReadLink, "read_link"),
            (Op::OpenRead, "open_read"),
            (Op::CreateNew, "create_new"),
            (Op::OpenAppend, "open_append"),
            (Op::SyncDir, "sync_dir"),
            (Op::Rename, "rename"),
            (Op::RemoveFile, "remove_file"),
            (Op::RemoveDir, "remove_dir"),
            (Op::CreateDir, "create_dir"),
            (Op::Symlink, "symlink"),
            (Op::SetMtime, "set_mtime"),
            (Op::SetMode, "set_mode"),
            (Op::AvailableSpace, "available_space"),
            (Op::Read, "read"),
            (Op::Write, "write"),
            (Op::Sync, "sync"),
        ];
        for (op, name) in names {
            assert_eq!(serde_json::to_string(&op).unwrap(), format!("\"{name}\""));
        }
    }

    #[test]
    fn nonsense_is_refused() {
        let rule = |op: &str, at: &str, fail: &str| {
            format!(
                r#"{{"rules": [{{"op": "{op}", "path": "**", "at": {at}, "fail": "{fail}"}}]}}"#
            )
        };
        let invalid = |json: String| match Spec::parse(&json) {
            Err(SpecError::Invalid { rule: 0, reason }) => reason,
            other => panic!("{json}: expected Invalid, got {other:?}"),
        };
        assert_eq!(
            invalid(rule("lstat", r#"{"call": 0}"#, "EIO")),
            "calls count from 1"
        );
        assert_eq!(
            invalid(rule("lstat", r#"{"from_call": 0}"#, "EIO")),
            "calls count from 1"
        );
        assert_eq!(
            invalid(rule("lstat", r#"{"offset": 4}"#, "EIO")),
            "an offset applies to read and write only"
        );
        assert_eq!(
            invalid(rule("write", r#"{"call": 1}"#, "EXDEV")),
            "EXDEV applies to rename only"
        );
        assert_eq!(
            invalid(
                r#"{"rules": [{"op": "lstat", "path": "", "at": {"call": 1}, "fail": "EIO"}]}"#
                    .into()
            ),
            "the path pattern is empty and would match nothing"
        );

        for json in [
            rule("stat", r#"{"call": 1}"#, "EIO"),
            rule("lstat", r#"{"call": 1}"#, "ENOENT"),
            rule("lstat", r#"{"nth": 1}"#, "EIO"),
            rule("lstat", r#"{"call": -1}"#, "EIO"),
            r#"{"rules": [{"op": "lstat", "path": "**", "at": {"call": 1}, "fail": "EIO", "when": 1}]}"#.into(),
            r#"{"rule": []}"#.into(),
            "not json".into(),
        ] {
            assert!(
                matches!(Spec::parse(&json), Err(SpecError::Json(_))),
                "{json} should not parse"
            );
        }
    }

    #[test]
    fn faults_are_the_operating_systems_errors() {
        use std::io::ErrorKind;
        assert_eq!(Fault::Enospc.error().kind(), ErrorKind::StorageFull);
        assert_eq!(Fault::Eacces.error().kind(), ErrorKind::PermissionDenied);
        assert_eq!(Fault::Exdev.error().kind(), ErrorKind::CrossesDevices);
        // EIO has no stable ErrorKind; its message is the same on both.
        assert!(
            Fault::Eio
                .error()
                .to_string()
                .starts_with("Input/output error")
        );
        for fault in [Fault::Enospc, Fault::Eio, Fault::Eacces, Fault::Exdev] {
            assert_eq!(fault.error().raw_os_error(), Some(fault.errno()));
        }
    }

    #[test]
    fn load_reads_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spec.json");
        std::fs::write(&path, r#"{"rules": []}"#).unwrap();
        assert_eq!(Spec::load(&path).unwrap(), Spec::default());
        assert!(matches!(
            Spec::load(&dir.path().join("missing.json")),
            Err(SpecError::Read(_))
        ));
    }
}
