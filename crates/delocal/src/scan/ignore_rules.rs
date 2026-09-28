//! Ignore rules (DESIGN.md §7.3): which paths a scan leaves alone.
//!
//! Three sources. `.delocal/` at the folder root is never an entry at all
//! ([`crate::names::Name::Reserved`]), so it never reaches these rules. Then
//! the defaults every folder ships with ([`DEFAULTS`]), and then the user's
//! `.delocalignore` at the folder root, in gitignore syntax. Later rules win,
//! as in git, so a user's `!.DS_Store` brings back what a default hides.
//!
//! **Matching** is `ignore`'s gitignore matcher (Appendix A), used for its
//! matcher alone: the scan walks through the filesystem layer itself, so
//! injected faults reach every call (§14.2). Rules match index paths, never
//! names on disk. Index paths are NFC and the same on every member, so every
//! member ignores the same paths, and each rule is put in NFC too before it
//! is compiled, since an editor may save the file in either form (a rule
//! for `café` typed as `cafe` and a combining accent would otherwise match
//! nothing). Matching is case-sensitive on every platform, for the same
//! reason: one folder, one set of ignored paths.
//!
//! **Beneath an ignored directory** everything is ignored. Git cannot
//! re-include a file whose directory is excluded, and a walk does not
//! descend into one, so `logs/` with `!logs/keep` still ignores
//! `logs/keep`. [`IgnoreRules::ignores_entry`] asks about one entry a walk
//! has reached, whose directories were not ignored; [`IgnoreRules::ignores`]
//! asks about any path, a record's for instance, and checks every directory
//! above it too, so the two always agree about a path on disk.
//!
//! A line that is not a valid pattern, or not UTF-8, is left out and listed
//! in [`IgnoreRules::invalid`] for `status`; the rest still apply.

use std::io::{self, Read};

use delocal_engine::RelPath;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use unicode_normalization::UnicodeNormalization;

use crate::fs::Dir;

/// The rules every folder has (§7.3), before the user's.
pub const DEFAULTS: [&str; 6] = [".DS_Store", "._*", "*.swp", "*~", ".#*", ".Trash*"];

/// The user's rules, at the folder root (§7.3, §11).
pub const IGNORE_FILE: &str = ".delocalignore";

/// A folder's ignore rules, compiled. See the module docs.
#[derive(Clone, Debug)]
pub struct IgnoreRules {
    matcher: Gitignore,
    invalid: Vec<InvalidRule>,
}

/// A line of `.delocalignore` that was left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidRule {
    /// Counting from 1.
    pub line: usize,
    /// The line as it is in the file, or as much of it as is UTF-8.
    pub text: String,
    /// Why it was left out.
    pub reason: String,
}

impl IgnoreRules {
    /// The defaults, then the rules in `user`, the bytes of
    /// `.delocalignore`, if there is one.
    pub fn new(user: Option<&[u8]>) -> io::Result<Self> {
        // Matched paths are relative, so the root is only what the matcher
        // would strip from an absolute one: "." strips nothing.
        let mut builder = GitignoreBuilder::new(".");
        for rule in DEFAULTS {
            builder.add_line(None, rule).map_err(invalid_data)?;
        }
        let mut invalid = Vec::new();
        for (index, bytes) in user.map(lines).unwrap_or_default().into_iter().enumerate() {
            let line = index + 1;
            let text = match std::str::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    invalid.push(InvalidRule {
                        line,
                        text: String::from_utf8_lossy(bytes).into_owned(),
                        reason: "not UTF-8".into(),
                    });
                    continue;
                }
            };
            // A byte-order mark on the first line is not part of the rule,
            // as git reads it.
            let text = if line == 1 {
                text.trim_start_matches('\u{feff}')
            } else {
                text
            };
            let nfc: String = text.nfc().collect();
            let from = Some(IGNORE_FILE.into());
            if let Err(e) = builder.add_line(from, &nfc) {
                invalid.push(InvalidRule {
                    line,
                    text: text.into(),
                    reason: e.to_string(),
                });
            }
        }
        let matcher = builder.build().map_err(invalid_data)?;
        Ok(Self { matcher, invalid })
    }

    /// Read `.delocalignore` from the folder root, held as `root`, and
    /// compile it after the defaults. No file there means the defaults
    /// alone. Anything else there (a directory, a symlink, which is not
    /// followed) or a file that cannot be read is an error: the scan cannot
    /// tell which paths the user meant to leave alone.
    pub fn read(root: &dyn Dir) -> io::Result<Self> {
        let mut file = match root.open_read(IGNORE_FILE.as_ref()) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Self::new(None),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Self::new(Some(&bytes))
    }

    /// Whether the entry at `path`, a directory or not, is ignored, given
    /// that no directory above it is: what a walk asks of each entry it has
    /// reached.
    pub fn ignores_entry(&self, path: &RelPath, is_dir: bool) -> bool {
        self.matcher.matched(path.as_str(), is_dir).is_ignore()
    }

    /// Whether `path`, a directory or not, is ignored, or any directory
    /// above it: what a scan asks of a path it may not have reached.
    pub fn ignores(&self, path: &RelPath, is_dir: bool) -> bool {
        let mut above = path.parent();
        while let Some(dir) = above {
            if self.ignores_entry(&dir, true) {
                return true;
            }
            above = dir.parent();
        }
        self.ignores_entry(path, is_dir)
    }

    /// The lines of `.delocalignore` that were left out, in order.
    pub fn invalid(&self) -> &[InvalidRule] {
        &self.invalid
    }
}

/// The lines of a file, without their `\n` or `\r\n`.
fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = bytes
        .split(|b| *b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();
    // A final newline ends the last line; it does not start another.
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn invalid_data(e: ignore::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fs::{FileKind, Folder, NotAFile, RealFolder};

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    /// One case: a path, whether it is a directory, and what `ignores`
    /// says. `ignores_entry` must agree unless a directory above the path is
    /// ignored, which the last column says.
    type Case = (&'static str, bool, bool, bool);

    fn check(rules: Option<&str>, cases: &[Case]) {
        let ignore = IgnoreRules::new(rules.map(str::as_bytes)).unwrap();
        assert_eq!(ignore.invalid(), [], "{rules:?}");
        for &(path, is_dir, ignored, above) in cases {
            assert_eq!(
                ignore.ignores(&p(path), is_dir),
                ignored,
                "{path} (dir: {is_dir}) under {rules:?}"
            );
            let entry = ignore.ignores_entry(&p(path), is_dir);
            if above {
                assert!(ignored, "{path}: ignored for what is above it");
            } else {
                assert_eq!(entry, ignored, "{path}: the entry alone");
            }
        }
    }

    #[test]
    fn the_defaults() {
        check(
            None,
            &[
                (".DS_Store", false, true, false),
                ("photos/2026/.DS_Store", false, true, false),
                ("._report.docx", false, true, false),
                ("notes.txt.swp", false, true, false),
                ("notes.txt~", false, true, false),
                (".#notes.txt", false, true, false),
                (".Trash-1000", true, true, false),
                (".Trashes", true, true, false),
                ("docs/.Trashes/x", false, true, true),
                // Near misses.
                ("DS_Store", false, false, false),
                ("notes.swp.txt", false, false, false),
                ("a~b", false, false, false),
                ("x._y", false, false, false),
                ("Trash", true, false, false),
                ("report.docx", false, false, false),
            ],
        );
    }

    #[test]
    fn user_rules_in_gitignore_syntax() {
        let rules = "\
# build output
*.log
!keep.log
build/
/top
docs/*.tmp
**/cache
a/**
\\#hash
trailing.txt
*.LOWER
";
        check(
            Some(rules),
            &[
                // Unanchored rules match at any depth; a negation wins after.
                ("a.log", false, true, false),
                ("x/y/b.log", false, true, false),
                ("keep.log", false, false, false),
                ("x/keep.log", false, false, false),
                // A trailing slash is for directories only, and everything
                // beneath one is ignored.
                ("build", true, true, false),
                ("build", false, false, false),
                ("x/build", true, true, false),
                ("build/out.o", false, true, true),
                ("build/deep/er", true, true, true),
                // A leading slash anchors to the root.
                ("top", false, true, false),
                ("x/top", false, false, false),
                // A slash inside anchors the whole pattern.
                ("docs/a.tmp", false, true, false),
                ("x/docs/a.tmp", false, false, false),
                ("docs/sub/a.tmp", false, false, false),
                ("q/r/cache", true, true, false),
                ("cache", false, true, false),
                // `a/**` is everything inside `a`, not `a` itself.
                ("a", true, false, false),
                ("a/b", false, true, false),
                ("a/b/c", false, true, true),
                // Escapes, comments and trailing spaces.
                ("#hash", false, true, false),
                ("# build output", false, false, false),
                ("trailing.txt", false, true, false),
                // Case-sensitive everywhere.
                ("x.lower", false, false, false),
                ("x.LOWER", false, true, false),
            ],
        );
    }

    #[test]
    fn nothing_beneath_an_ignored_directory_comes_back() {
        check(
            Some("logs/\n!logs/keep.txt\n!logs/\n"),
            // `!logs/` re-includes the directory itself, the last rule
            // for it winning, and then what is beneath it is not ignored.
            &[
                ("logs", true, false, false),
                ("logs/keep.txt", false, false, false),
            ],
        );
        check(
            Some("logs/\n!logs/keep.txt\n"),
            &[
                ("logs", true, true, false),
                // The matcher alone would re-include it; the directory above
                // it is ignored, so it stays ignored, as in git.
                ("logs/keep.txt", false, true, true),
            ],
        );
        let ignore = IgnoreRules::new(Some(b"logs/\n!logs/keep.txt\n")).unwrap();
        assert!(!ignore.ignores_entry(&p("logs/keep.txt"), false));
    }

    #[test]
    fn a_user_rule_can_bring_back_a_default() {
        check(
            Some("!.DS_Store\n"),
            &[
                (".DS_Store", false, false, false),
                ("x.swp", false, true, false),
            ],
        );
    }

    #[test]
    fn rules_match_index_paths_in_nfc_whatever_form_the_file_is_in() {
        // "café" with a combining accent, as some editors save it; index
        // paths are NFC, so the rule is too.
        check(
            Some("cafe\u{301}/\n"),
            &[
                ("café", true, true, false),
                ("café/menu", false, true, true),
            ],
        );
        check(Some("café\n"), &[("café", false, true, false)]);
    }

    #[test]
    fn line_endings_a_byte_order_mark_and_bad_lines() {
        let ignore =
            IgnoreRules::new(Some(b"\xef\xbb\xbf*.log\r\nbad\xff\r\n{a,b\n*.tmp")).unwrap();
        assert!(ignore.ignores(&p("x.log"), false), "BOM and CRLF");
        assert!(ignore.ignores(&p("x.tmp"), false), "no final newline");
        let invalid: Vec<(usize, &str)> = ignore
            .invalid()
            .iter()
            .map(|rule| (rule.line, rule.text.as_str()))
            .collect();
        assert_eq!(invalid, [(2, "bad\u{fffd}"), (3, "{a,b")]);
        assert_eq!(ignore.invalid()[0].reason, "not UTF-8");
        assert!(
            !ignore.ignores(&p("a"), false),
            "a bad rule matches nothing"
        );
        assert!(IgnoreRules::new(Some(b"")).unwrap().invalid().is_empty());
    }

    #[test]
    fn the_file_is_read_through_the_held_root() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = RealFolder::new(tmp.path());
        let read = || {
            let fs = folder.open().unwrap();
            IgnoreRules::read(&*fs.open_dir(Path::new("")).unwrap())
        };

        // No file: the defaults.
        let rules = read().unwrap();
        assert!(rules.ignores(&p(".DS_Store"), false));
        assert!(!rules.ignores(&p("x.log"), false));

        std::fs::write(tmp.path().join(IGNORE_FILE), "*.log\n").unwrap();
        assert!(read().unwrap().ignores(&p("x.log"), false));

        // Anything but a file is an error, a symlink not followed.
        std::fs::remove_file(tmp.path().join(IGNORE_FILE)).unwrap();
        std::fs::write(tmp.path().join("elsewhere"), "*.log\n").unwrap();
        std::os::unix::fs::symlink("elsewhere", tmp.path().join(IGNORE_FILE)).unwrap();
        let err = read().unwrap_err();
        assert_eq!(
            NotAFile::of(&err),
            Some(NotAFile {
                kind: FileKind::Symlink
            })
        );
        std::fs::remove_file(tmp.path().join(IGNORE_FILE)).unwrap();
        std::fs::create_dir(tmp.path().join(IGNORE_FILE)).unwrap();
        let err = read().unwrap_err();
        assert_eq!(
            NotAFile::of(&err),
            Some(NotAFile {
                kind: FileKind::Dir
            })
        );
    }

    #[cfg(feature = "faults")]
    #[test]
    fn a_file_that_cannot_be_read_is_an_error() {
        use crate::fs::{
            FaultyFolder, faulty::Fault, faulty::Op, faulty::Rule, faulty::Spec, faulty::Trigger,
        };

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(IGNORE_FILE), "*.log\n").unwrap();
        for (op, fail) in [(Op::OpenRead, Fault::Eacces), (Op::Read, Fault::Eio)] {
            let at = if op == Op::Read {
                Trigger::Offset(0)
            } else {
                Trigger::Call(1)
            };
            let spec = Spec {
                rules: vec![Rule {
                    op,
                    path: IGNORE_FILE.into(),
                    at,
                    fail,
                }],
            };
            let folder = FaultyFolder::new(RealFolder::new(tmp.path()), spec).unwrap();
            let fs = folder.open().unwrap();
            let err = IgnoreRules::read(&*fs.open_dir(Path::new("")).unwrap()).unwrap_err();
            assert_eq!(err.raw_os_error(), fail.errno(), "{op:?}");
        }
    }
}
