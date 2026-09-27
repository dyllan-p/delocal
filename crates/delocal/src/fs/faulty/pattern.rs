//! Path patterns for fault rules (DESIGN.md §14.2): which paths a rule
//! applies to.
//!
//! A pattern matches a whole path, relative to the folder root as the
//! [`Fs`](crate::fs::Fs) takes it, byte by byte, so it works on names that
//! are not UTF-8. `*` matches any run of bytes without a `/`; `**/`
//! matches zero or more whole directories, so `/r/**/x` matches `/r/x` and
//! `/r/a/b/x`; `**` anywhere else matches any run of bytes, `/` included;
//! and `?` matches one byte that is not `/`. Every other byte matches
//! itself. There are no escapes and no classes. So `.delocal/tmp/*` matches
//! every temp file, `**/report.txt` a `report.txt` at any depth, the root
//! included, and `**` everything.
//!
//! Hand-rolled rather than taken from a crate: the matcher is one short
//! function, and the gitignore matcher in `ignore` (Appendix A) has
//! semantics of its own (anchoring, negation, directory-only rules) that a
//! fault rule does not want.

/// A compiled pattern. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    tokens: Vec<Token>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Byte(u8),
    /// `?`
    One,
    /// `*`
    Star,
    /// `**/`: nothing, or any run of bytes ending in `/`.
    AnyDirs,
    /// `**` not followed by `/`.
    AnyDepth,
}

impl Pattern {
    /// Compile `pattern`. Every string is a valid pattern.
    pub fn new(pattern: &str) -> Self {
        let mut tokens = Vec::new();
        let mut bytes = pattern.bytes().peekable();
        while let Some(b) = bytes.next() {
            tokens.push(match b {
                b'*' if bytes.peek() == Some(&b'*') => {
                    bytes.next();
                    if bytes.peek() == Some(&b'/') {
                        bytes.next();
                        Token::AnyDirs
                    } else {
                        Token::AnyDepth
                    }
                }
                b'*' => Token::Star,
                b'?' => Token::One,
                b => Token::Byte(b),
            });
        }
        Self { tokens }
    }

    /// Whether the pattern matches all of `path`.
    pub fn matches(&self, path: &[u8]) -> bool {
        // Dynamic programming from the end: `next[j]` says whether the
        // tokens after the current one match `path[j..]`. Time is tokens ×
        // path length, with no backtracking blow-up however many stars.
        let len = path.len();
        let mut next = vec![false; len + 1];
        next[len] = true;
        for token in self.tokens.iter().rev() {
            let mut here = vec![false; len + 1];
            match *token {
                Token::Byte(b) => {
                    for j in 0..len {
                        here[j] = path[j] == b && next[j + 1];
                    }
                }
                Token::One => {
                    for j in 0..len {
                        here[j] = path[j] != b'/' && next[j + 1];
                    }
                }
                Token::AnyDirs => {
                    // `ends_in_slash` says whether some run of bytes from
                    // `j` ending in a `/` leaves the rest matching `next`.
                    let mut ends_in_slash = false;
                    here[len] = next[len];
                    for j in (0..len).rev() {
                        ends_in_slash = (path[j] == b'/' && next[j + 1]) || ends_in_slash;
                        here[j] = next[j] || ends_in_slash;
                    }
                }
                Token::Star | Token::AnyDepth => {
                    let crosses = *token == Token::AnyDepth;
                    here[len] = next[len];
                    for j in (0..len).rev() {
                        here[j] = next[j] || ((crosses || path[j] != b'/') && here[j + 1]);
                    }
                }
            }
            next = here;
        }
        next[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table() {
        let cases: &[(&str, &[u8], bool)] = &[
            ("**", b"/r/a/b", true),
            ("**", b"", true),
            ("**/a", b"/r/a", true),
            ("**/a", b"/a", true),
            ("**/a", b"a", true),
            ("**/a", b"/r/ba", false),
            ("**/a", b"/r/a/b", false),
            ("/r/*", b"/r/a", true),
            ("/r/*", b"/r/", true),
            ("/r/*", b"/r/a/b", false),
            ("/r/**", b"/r/a/b", true),
            ("/r/?", b"/r/a", true),
            ("/r/?", b"/r/ab", false),
            ("/r/?", b"/r//", false),
            ("*/a", b"/r/a", false),
            (
                "**/.delocal/tmp/*",
                b"/home/u/Sync/.delocal/tmp/ab-12",
                true,
            ),
            ("**/.delocal/tmp/*", b"/home/u/Sync/.delocal/trash/x", false),
            ("**/d/**", b"/r/d/x/y", true),
            ("**/d/**", b"/r/dd/x", false),
            ("**/a*", b"/r/a\xff\xfe", true),
            ("**/?", b"/r/\xff", true),
            ("/exact", b"/exact", true),
            ("/exact", b"/exactly", false),
            ("", b"", true),
            ("", b"/", false),
            ("***", b"/a/b", true),
            // `**/` is zero or more whole directories.
            ("/r/**/x", b"/r/x", true),
            ("/r/**/x", b"/r/a/b/x", true),
            ("/r/**/x", b"/r/ax", false),
            ("/r/**/x", b"/rx", false),
            ("**/trash/**/*", b"/f/trash/x", true),
            ("**/trash/**/*", b"/f/trash/2026-09-27/a/b", true),
            ("**/trash/**/*", b"/f/trashy/x", false),
            ("/r/**", b"/r/", true),
            ("/r/**", b"/r", false),
        ];
        for &(pattern, path, expected) in cases {
            assert_eq!(
                Pattern::new(pattern).matches(path),
                expected,
                "{pattern:?} against {:?}",
                String::from_utf8_lossy(path)
            );
        }
    }
}
