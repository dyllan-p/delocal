//! Entry paths (DESIGN.md §7.1): relative, forward slashes, no leading `./`.
//!
//! [`RelPath`] is the key of the index and the `path` of every entry and
//! batch entry. Construction validates shape: non-empty, no empty component
//! (so no leading, trailing or doubled slash), no `.` or `..` component, no
//! NUL, no component over 255 bytes (`NAME_MAX` on ext4 and APFS, so such a
//! file cannot exist on a supported filesystem), and not under `.delocal/`
//! at the folder root, which §7.3 reserves and always ignores. Rejecting
//! reserved paths here means a batch from a misbehaving peer cannot write
//! into `.delocal/`.
//!
//! NFC normalisation is the host's job before a path reaches the engine
//! (§7.1): the engine treats paths as opaque UTF-8.
//!
//! `Ord` is the byte order of the string. Because a parent is a strict
//! prefix of its children, sorting puts every directory before its contents,
//! which is the order §7.5 wants for creates; reverse it for deletes.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The directory at every folder root that delocal owns (§7.3, §11).
pub const RESERVED_DIR: &str = ".delocal";

/// Longest path component, in bytes: `NAME_MAX` on ext4 and APFS.
pub const MAX_COMPONENT_LEN: usize = 255;

/// Why a string is not a valid [`RelPath`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelPathError {
    /// The empty string. The folder root itself is not an entry.
    Empty,
    /// Starts with `/`.
    Absolute,
    /// Ends with `/`.
    TrailingSlash,
    /// Contains `//`.
    EmptyComponent,
    /// A `.` component, including a leading `./`.
    DotComponent,
    /// A `..` component.
    DotDotComponent,
    /// A NUL byte, which no filesystem accepts.
    Nul,
    /// A component longer than [`MAX_COMPONENT_LEN`] bytes.
    ComponentTooLong(usize),
    /// `.delocal` or something under it at the folder root.
    Reserved,
}

impl fmt::Display for RelPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "path is empty",
            Self::Absolute => "path starts with /",
            Self::TrailingSlash => "path ends with /",
            Self::EmptyComponent => "path contains //",
            Self::DotComponent => "path contains a . component",
            Self::DotDotComponent => "path contains a .. component",
            Self::Nul => "path contains a NUL byte",
            Self::ComponentTooLong(_) => "path has a component longer than 255 bytes",
            Self::Reserved => "path is under the reserved .delocal directory",
        })
    }
}

impl std::error::Error for RelPathError {}

/// A validated relative path inside a folder. See the module docs.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    /// Validate and wrap.
    pub fn new(s: impl Into<String>) -> Result<Self, RelPathError> {
        let s = s.into();
        if s.is_empty() {
            return Err(RelPathError::Empty);
        }
        if s.contains('\0') {
            return Err(RelPathError::Nul);
        }
        if s.starts_with('/') {
            return Err(RelPathError::Absolute);
        }
        if s.ends_with('/') {
            return Err(RelPathError::TrailingSlash);
        }
        for component in s.split('/') {
            match component {
                "" => return Err(RelPathError::EmptyComponent),
                "." => return Err(RelPathError::DotComponent),
                ".." => return Err(RelPathError::DotDotComponent),
                c if c.len() > MAX_COMPONENT_LEN => {
                    return Err(RelPathError::ComponentTooLong(c.len()));
                }
                _ => {}
            }
        }
        if s.split('/').next() == Some(RESERVED_DIR) {
            return Err(RelPathError::Reserved);
        }
        Ok(Self(s))
    }

    /// The path as text, forward slashes, no leading `./`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path's components, root first. Never empty.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Number of components. A top-level entry has depth 1.
    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// The last component.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The containing directory, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<Self> {
        self.0.rfind('/').map(|i| Self(self.0[..i].to_owned()))
    }

    /// Append one component. Fails if `name` is not a valid single component.
    pub fn join(&self, name: &str) -> Result<Self, RelPathError> {
        if name.contains('/') {
            return Err(RelPathError::EmptyComponent);
        }
        Self::new(format!("{}/{}", self.0, name))
    }

    /// True if `self` is a proper ancestor directory of `other`.
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        other.0.len() > self.0.len()
            && other.0.starts_with(&self.0)
            && other.0.as_bytes()[self.0.len()] == b'/'
    }
}

impl TryFrom<String> for RelPath {
    type Error = RelPathError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<RelPath> for String {
    fn from(p: RelPath) -> Self {
        p.0
    }
}

impl FromStr for RelPath {
    type Err = RelPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for RelPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn accepts_ordinary_paths() {
        let long = "x".repeat(255);
        let two_long = format!("{}/{}", "y".repeat(255), "z".repeat(255));
        for ok in [
            "a",
            "a/b",
            "docs/report.xlsx",
            ".bashrc",
            "with space/and-dash_underscore",
            "unicode/naïve/日本語.txt",
            "trailing.dot.",
            "back\\slash",
            "...",
            "delocal",
            "x/.delocal/inner",
            long.as_str(),
            two_long.as_str(),
        ] {
            assert!(RelPath::new(ok).is_ok(), "{ok:?} should be valid");
        }
    }

    #[test]
    fn rejects_by_rule() {
        let too_long = "x".repeat(256);
        // Bytes, not chars: 128 two-byte characters are 256 bytes.
        let too_long_utf8 = "ü".repeat(128);
        let cases = [
            ("", RelPathError::Empty),
            ("/abs", RelPathError::Absolute),
            ("dir/", RelPathError::TrailingSlash),
            ("a//b", RelPathError::EmptyComponent),
            ("./a", RelPathError::DotComponent),
            ("a/./b", RelPathError::DotComponent),
            (".", RelPathError::DotComponent),
            ("../a", RelPathError::DotDotComponent),
            ("a/..", RelPathError::DotDotComponent),
            ("a\0b", RelPathError::Nul),
            (too_long.as_str(), RelPathError::ComponentTooLong(256)),
            (too_long_utf8.as_str(), RelPathError::ComponentTooLong(256)),
            (".delocal", RelPathError::Reserved),
            (".delocal/trash/x", RelPathError::Reserved),
        ];
        for (input, err) in cases {
            assert_eq!(RelPath::new(input), Err(err), "{input:?}");
        }
    }

    #[test]
    fn structure_helpers() {
        let path = p("docs/2026/report.xlsx");
        assert_eq!(path.depth(), 3);
        assert_eq!(path.file_name(), "report.xlsx");
        assert_eq!(path.parent(), Some(p("docs/2026")));
        assert_eq!(p("docs").parent(), None);
        assert_eq!(p("docs").file_name(), "docs");
        assert_eq!(
            path.components().collect::<Vec<_>>(),
            ["docs", "2026", "report.xlsx"]
        );
        assert_eq!(p("docs").join("a.txt"), Ok(p("docs/a.txt")));
        assert_eq!(p("docs").join("a/b"), Err(RelPathError::EmptyComponent));
        assert_eq!(p("docs").join(".."), Err(RelPathError::DotDotComponent));
        assert_eq!(
            p("docs").join(&"n".repeat(256)),
            Err(RelPathError::ComponentTooLong(256))
        );
    }

    #[test]
    fn ancestry_needs_a_slash_boundary() {
        assert!(p("a").is_ancestor_of(&p("a/b")));
        assert!(p("a").is_ancestor_of(&p("a/b/c")));
        assert!(!p("a").is_ancestor_of(&p("a")));
        assert!(!p("a").is_ancestor_of(&p("ab")));
        assert!(!p("a/b").is_ancestor_of(&p("a")));
    }

    #[test]
    fn parents_sort_before_children() {
        let mut v = vec![p("a/b/c"), p("a-b"), p("a"), p("a/b"), p("a/c")];
        v.sort();
        assert_eq!(v, [p("a"), p("a-b"), p("a/b"), p("a/b/c"), p("a/c")]);
    }

    #[test]
    fn serde_validates_and_round_trips() {
        let path: RelPath = serde_json::from_str("\"a/b.txt\"").unwrap();
        assert_eq!(path, p("a/b.txt"));
        assert_eq!(serde_json::to_string(&path).unwrap(), "\"a/b.txt\"");
        assert!(serde_json::from_str::<RelPath>("\"../x\"").is_err());
        assert!(serde_json::from_str::<RelPath>("\".delocal/x\"").is_err());
        let bytes = postcard::to_stdvec(&path).unwrap();
        assert_eq!(postcard::from_bytes::<RelPath>(&bytes).unwrap(), path);
    }

    fn component() -> impl Strategy<Value = String> {
        "[a-z.]{1,4}".prop_filter("no dot components", |s| s != "." && s != "..")
    }

    proptest! {
        #[test]
        fn parent_is_ancestor_and_join_inverts_it(
            parts in prop::collection::vec(component(), 1..5)
        ) {
            let path = RelPath::new(parts.join("/")).unwrap();
            prop_assert_eq!(path.depth(), parts.len());
            prop_assert_eq!(path.file_name(), parts.last().unwrap().as_str());
            match path.parent() {
                None => prop_assert_eq!(parts.len(), 1),
                Some(parent) => {
                    prop_assert!(parent.is_ancestor_of(&path));
                    prop_assert!(parent < path);
                    prop_assert_eq!(parent.join(path.file_name()), Ok(path.clone()));
                }
            }
        }

        #[test]
        fn string_round_trip(parts in prop::collection::vec(component(), 1..5)) {
            let s = parts.join("/");
            let path = RelPath::new(s.clone()).unwrap();
            prop_assert_eq!(path.as_str(), s.as_str());
            prop_assert_eq!(String::from(path.clone()), s);
        }
    }
}
