//! The contract of [`Fs`] as tests (DESIGN.md §14.2). Each check takes an
//! implementation, makes its own temporary directory and asserts what the
//! scanner and the commit path will rely on. [`conformance_tests!`] turns
//! the whole list into one `#[test]` per check for an implementation.
//!
//! `RealFs` runs them on the machine's real filesystem, which CI does on
//! Linux and macOS. The set-mtime check also asserts that nanoseconds
//! survive a round trip, which answers §7.3's **[verify]** for the
//! filesystems the temporary directories live on (tmpfs or ext4 on Linux,
//! APFS on macOS). The last checks are §7.3's reaching a path: a commit at a
//! path longer than either platform's `PATH_MAX`, and parents that are not
//! directories, one of them a symlink to a directory outside the folder
//! that must come through untouched.

use std::ffi::OsString;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{FileKind, Fs, ParentNotADirectory};

/// One `#[test]` per check. Each makes a temporary directory holding
/// `folder`, the root it passes to `$make` (a closure from the root to an
/// implementation), and room beside it for checks that need an outside.
macro_rules! conformance_tests {
    ($make:expr) => {
        $crate::fs::conformance::conformance_tests!(
            $make;
            read_dir_gives_raw_names_sorted_by_bytes,
            lstat_reports_kind_size_and_mode_without_following,
            read_link_gives_the_raw_target,
            open_read_reads_from_the_start_and_seeks,
            create_new_refuses_anything_already_there,
            open_append_writes_at_the_end_and_needs_a_file,
            sync_dir_accepts_a_directory,
            rename_moves_and_replaces_a_file,
            remove_file_removes_files_and_symlinks_not_targets,
            remove_dir_removes_only_empty_directories,
            create_dir_needs_a_parent_and_an_empty_path,
            symlink_makes_a_link_that_is_never_followed,
            set_mtime_round_trips_nanoseconds_without_following,
            set_mode_sets_bits_and_refuses_a_symlink,
            available_space_is_reported_for_a_directory,
            paths_stay_inside_the_folder,
            a_path_beyond_path_max_is_committed_one_component_at_a_time,
            a_parent_that_is_a_file_stops_the_operation,
            a_parent_swapped_for_a_symlink_keeps_everything_inside_the_folder,
        );
    };
    ($make:expr; $($check:ident),* $(,)?) => {
        $(
            #[test]
            fn $check() {
                let tmp = tempfile::tempdir().unwrap();
                let root = tmp.path().join("folder");
                std::fs::create_dir(&root).unwrap();
                let fs = ($make)(root.as_path());
                $crate::fs::conformance::$check(&fs, &root);
            }
        )*
    };
}
pub(crate) use conformance_tests;

fn p(path: &str) -> &Path {
    Path::new(path)
}

fn write(fs: &dyn Fs, path: &Path, bytes: &[u8]) {
    let mut file = fs.create_new(path).unwrap();
    file.write_all(bytes).unwrap();
    file.sync().unwrap();
}

fn read(fs: &dyn Fs, path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    fs.open_read(path).unwrap().read_to_end(&mut bytes).unwrap();
    bytes
}

/// The kind of the error `result` must be. Not `unwrap_err`, which needs
/// `T: Debug`, and the file handles are not.
fn kind_of<T>(result: std::io::Result<T>) -> ErrorKind {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.kind(),
    }
}

pub fn read_dir_gives_raw_names_sorted_by_bytes(fs: &dyn Fs, _root: &Path) {
    // No two names differ only by case: the macOS runner's APFS is
    // case-insensitive and would refuse the second.
    for name in ["b", "a", "Z", "é"] {
        write(fs, p(name), b"x");
    }
    fs.create_dir(p("d")).unwrap();
    fs.symlink(Path::new("nowhere"), p("s")).unwrap();
    let names = fs.read_dir(p("")).unwrap();
    // "Z" (0x5A) sorts before "a" (0x61), and "é" (0xC3 0xA9) after ASCII.
    assert_eq!(names, ["Z", "a", "b", "d", "s", "é"]);

    assert!(fs.read_dir(p("d")).unwrap().is_empty());
    assert_eq!(kind_of(fs.read_dir(p("missing"))), ErrorKind::NotFound);
    assert_eq!(kind_of(fs.read_dir(p("a"))), ErrorKind::NotADirectory);
}

pub fn lstat_reports_kind_size_and_mode_without_following(fs: &dyn Fs, root: &Path) {
    write(fs, p("f"), b"hello");
    fs.set_mode(p("f"), 0o640).unwrap();
    write(fs, p("g"), b"");
    fs.create_dir(p("d")).unwrap();
    fs.symlink(Path::new("f"), p("to-f")).unwrap();
    fs.symlink(Path::new("missing-target"), p("dangling"))
        .unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(root.join("sock")).unwrap();

    let f = fs.lstat(p("f")).unwrap();
    assert_eq!((f.kind, f.size, f.mode), (FileKind::File, 5, 0o640));
    assert_eq!(fs.lstat(p("d")).unwrap().kind, FileKind::Dir);
    let to_f = fs.lstat(p("to-f")).unwrap();
    assert_eq!((to_f.kind, to_f.size), (FileKind::Symlink, 1));
    let dangling = fs.lstat(p("dangling")).unwrap();
    assert_eq!(
        (dangling.kind, dangling.size),
        (FileKind::Symlink, "missing-target".len() as u64)
    );
    assert_eq!(fs.lstat(p("sock")).unwrap().kind, FileKind::Other);

    let g = fs.lstat(p("g")).unwrap();
    assert_eq!(f.dev, g.dev);
    assert_ne!(f.ino, g.ino);
    assert_eq!(kind_of(fs.lstat(p("missing"))), ErrorKind::NotFound);
}

pub fn read_link_gives_the_raw_target(fs: &dyn Fs, _root: &Path) {
    // Not normalised, not resolved, and it need not exist.
    let target = Path::new("a/./b/../c");
    fs.symlink(target, p("s")).unwrap();
    assert_eq!(fs.read_link(p("s")).unwrap(), target);

    write(fs, p("f"), b"x");
    assert_eq!(kind_of(fs.read_link(p("f"))), ErrorKind::InvalidInput);
    assert_eq!(kind_of(fs.read_link(p("missing"))), ErrorKind::NotFound);
}

pub fn open_read_reads_from_the_start_and_seeks(fs: &dyn Fs, _root: &Path) {
    write(fs, p("f"), b"hello world");
    assert_eq!(read(fs, p("f")), b"hello world");

    let mut file = fs.open_read(p("f")).unwrap();
    assert_eq!(file.seek(SeekFrom::Start(6)).unwrap(), 6);
    let mut rest = String::new();
    file.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "world");

    // Opening follows a symlink, as the module docs say.
    fs.symlink(Path::new("f"), p("s")).unwrap();
    assert_eq!(read(fs, p("s")), b"hello world");
    assert_eq!(kind_of(fs.open_read(p("missing"))), ErrorKind::NotFound);
}

pub fn create_new_refuses_anything_already_there(fs: &dyn Fs, _root: &Path) {
    write(fs, p("f"), b"abc");
    assert_eq!(read(fs, p("f")), b"abc");
    assert_eq!(kind_of(fs.create_new(p("f"))), ErrorKind::AlreadyExists);
    assert_eq!(read(fs, p("f")), b"abc", "not truncated");

    // A symlink is refused too, dangling or not, so a temp file is never
    // written through one.
    fs.symlink(Path::new("elsewhere"), p("s")).unwrap();
    assert_eq!(kind_of(fs.create_new(p("s"))), ErrorKind::AlreadyExists);
    assert_eq!(kind_of(fs.lstat(p("elsewhere"))), ErrorKind::NotFound);
    fs.create_dir(p("d")).unwrap();
    assert_eq!(kind_of(fs.create_new(p("d"))), ErrorKind::AlreadyExists);
    assert_eq!(
        kind_of(fs.create_new(p("no-parent/f"))),
        ErrorKind::NotFound
    );
}

pub fn open_append_writes_at_the_end_and_needs_a_file(fs: &dyn Fs, _root: &Path) {
    write(fs, p("f"), b"abc");
    let mut file = fs.open_append(p("f")).unwrap();
    file.write_all(b"def").unwrap();
    file.sync().unwrap();
    drop(file);
    assert_eq!(read(fs, p("f")), b"abcdef");

    // Opening follows a symlink, as the module docs say.
    fs.symlink(Path::new("f"), p("s")).unwrap();
    let mut file = fs.open_append(p("s")).unwrap();
    file.write_all(b"g").unwrap();
    drop(file);
    assert_eq!(read(fs, p("f")), b"abcdefg");

    assert_eq!(kind_of(fs.open_append(p("missing"))), ErrorKind::NotFound);
    assert_eq!(kind_of(fs.lstat(p("missing"))), ErrorKind::NotFound);
}

pub fn sync_dir_accepts_a_directory(fs: &dyn Fs, _root: &Path) {
    fs.create_dir(p("d")).unwrap();
    write(fs, p("d/f"), b"x");
    fs.sync_dir(p("d")).unwrap();
    fs.sync_dir(p("")).unwrap();
    assert_eq!(kind_of(fs.sync_dir(p("missing"))), ErrorKind::NotFound);
}

pub fn rename_moves_and_replaces_a_file(fs: &dyn Fs, _root: &Path) {
    write(fs, p("a"), b"A");
    fs.rename(p("a"), p("b")).unwrap();
    assert_eq!(kind_of(fs.lstat(p("a"))), ErrorKind::NotFound);
    assert_eq!(read(fs, p("b")), b"A");

    write(fs, p("c"), b"C");
    fs.rename(p("c"), p("b")).unwrap();
    assert_eq!(read(fs, p("b")), b"C", "replaced");
    assert_eq!(fs.read_dir(p("")).unwrap(), ["b"]);

    // A directory moves with its children, as §7.6's displacement expects.
    fs.create_dir(p("d")).unwrap();
    write(fs, p("d/child"), b"x");
    fs.rename(p("d"), p("e")).unwrap();
    assert_eq!(read(fs, p("e/child")), b"x");

    // A file does not replace a directory: both stay as they were, so a
    // commit that meets a directory at its path displaces it first (§7.5
    // step 7).
    assert!(fs.rename(p("b"), p("e")).is_err());
    assert_eq!(read(fs, p("b")), b"C");
    assert_eq!(read(fs, p("e/child")), b"x");

    assert_eq!(
        kind_of(fs.rename(p("missing"), p("x"))),
        ErrorKind::NotFound
    );
}

pub fn remove_file_removes_files_and_symlinks_not_targets(fs: &dyn Fs, _root: &Path) {
    write(fs, p("f"), b"x");
    fs.symlink(Path::new("f"), p("s")).unwrap();
    fs.remove_file(p("s")).unwrap();
    assert_eq!(fs.read_dir(p("")).unwrap(), ["f"]);
    fs.remove_file(p("f")).unwrap();
    assert!(fs.read_dir(p("")).unwrap().is_empty());

    fs.create_dir(p("d")).unwrap();
    assert!(fs.remove_file(p("d")).is_err());
    assert_eq!(fs.lstat(p("d")).unwrap().kind, FileKind::Dir);
    assert_eq!(kind_of(fs.remove_file(p("missing"))), ErrorKind::NotFound);
}

pub fn remove_dir_removes_only_empty_directories(fs: &dyn Fs, _root: &Path) {
    fs.create_dir(p("empty")).unwrap();
    fs.remove_dir(p("empty")).unwrap();
    assert_eq!(kind_of(fs.lstat(p("empty"))), ErrorKind::NotFound);

    fs.create_dir(p("full")).unwrap();
    write(fs, p("full/f"), b"x");
    assert_eq!(
        kind_of(fs.remove_dir(p("full"))),
        ErrorKind::DirectoryNotEmpty
    );
    assert_eq!(read(fs, p("full/f")), b"x");
    assert_eq!(
        kind_of(fs.remove_dir(p("full/f"))),
        ErrorKind::NotADirectory
    );
    assert_eq!(kind_of(fs.remove_dir(p("missing"))), ErrorKind::NotFound);
}

pub fn create_dir_needs_a_parent_and_an_empty_path(fs: &dyn Fs, _root: &Path) {
    fs.create_dir(p("d")).unwrap();
    assert_eq!(fs.lstat(p("d")).unwrap().kind, FileKind::Dir);
    assert_eq!(kind_of(fs.create_dir(p("d"))), ErrorKind::AlreadyExists);
    assert_eq!(kind_of(fs.create_dir(p("x/y"))), ErrorKind::NotFound);
    fs.symlink(Path::new("elsewhere"), p("s")).unwrap();
    assert_eq!(kind_of(fs.create_dir(p("s"))), ErrorKind::AlreadyExists);
    assert_eq!(kind_of(fs.lstat(p("elsewhere"))), ErrorKind::NotFound);
}

pub fn symlink_makes_a_link_that_is_never_followed(fs: &dyn Fs, _root: &Path) {
    fs.symlink(Path::new("does/not/exist"), p("s")).unwrap();
    assert_eq!(fs.lstat(p("s")).unwrap().kind, FileKind::Symlink);
    assert_eq!(fs.read_link(p("s")).unwrap(), Path::new("does/not/exist"));
    assert_eq!(
        kind_of(fs.symlink(Path::new("other"), p("s"))),
        ErrorKind::AlreadyExists
    );
}

pub fn set_mtime_round_trips_nanoseconds_without_following(fs: &dyn Fs, root: &Path) {
    let f = p("f");
    write(fs, f, b"x");
    let accessed = std::fs::symlink_metadata(root.join(f))
        .unwrap()
        .accessed()
        .unwrap();
    for mtime_ns in [1_700_000_000_123_456_789, 1, 0, -1_500_000_000] {
        fs.set_mtime(f, mtime_ns).unwrap();
        assert_eq!(fs.lstat(f).unwrap().mtime_ns, mtime_ns);
    }
    assert_eq!(
        std::fs::symlink_metadata(root.join(f))
            .unwrap()
            .accessed()
            .unwrap(),
        accessed,
        "the access time is left alone"
    );

    // On a symlink the link's own time changes and the target's does not.
    fs.symlink(Path::new("f"), p("s")).unwrap();
    fs.set_mtime(p("s"), 42_000_000_007).unwrap();
    assert_eq!(fs.lstat(p("s")).unwrap().mtime_ns, 42_000_000_007);
    assert_eq!(fs.lstat(f).unwrap().mtime_ns, -1_500_000_000);
    assert_eq!(kind_of(fs.set_mtime(p("missing"), 1)), ErrorKind::NotFound);
}

pub fn set_mode_sets_bits_and_refuses_a_symlink(fs: &dyn Fs, _root: &Path) {
    let f = p("f");
    write(fs, f, b"x");
    fs.set_mode(f, 0o755).unwrap();
    assert_eq!(fs.lstat(f).unwrap().mode, 0o755);
    fs.set_mode(f, 0o600).unwrap();
    assert_eq!(fs.lstat(f).unwrap().mode, 0o600);
    fs.create_dir(p("d")).unwrap();
    fs.set_mode(p("d"), 0o700).unwrap();
    assert_eq!(fs.lstat(p("d")).unwrap().mode, 0o700);

    fs.symlink(Path::new("f"), p("s")).unwrap();
    assert_eq!(kind_of(fs.set_mode(p("s"), 0o777)), ErrorKind::InvalidInput);
    assert_eq!(fs.lstat(f).unwrap().mode, 0o600, "the target is untouched");
    assert_eq!(
        kind_of(fs.set_mode(p("missing"), 0o644)),
        ErrorKind::NotFound
    );
}

pub fn available_space_is_reported_for_a_directory(fs: &dyn Fs, _root: &Path) {
    assert!(fs.available_space(p("")).unwrap() > 0);
    assert_eq!(
        kind_of(fs.available_space(p("missing"))),
        ErrorKind::NotFound
    );
}

pub fn paths_stay_inside_the_folder(fs: &dyn Fs, root: &Path) {
    write(fs, p("a"), b"x");
    assert_eq!(fs.lstat(p("./a")).unwrap().size, 1);
    assert_eq!(fs.lstat(p("")).unwrap().kind, FileKind::Dir, "the root");
    // Absolute paths, even to inside the folder, and anything with `..`.
    let absolute = root.join("a");
    for outside in [
        p("/etc"),
        absolute.as_path(),
        p(".."),
        p("../x"),
        p("a/../a"),
    ] {
        assert_eq!(
            kind_of(fs.lstat(outside)),
            ErrorKind::InvalidInput,
            "{outside:?}"
        );
    }
    // The root has no name to remove, create or rename.
    assert_eq!(kind_of(fs.remove_dir(p(""))), ErrorKind::InvalidInput);
    assert_eq!(kind_of(fs.create_new(p(""))), ErrorKind::InvalidInput);
    assert_eq!(kind_of(fs.rename(p("a"), p(""))), ErrorKind::InvalidInput);
}

pub fn a_path_beyond_path_max_is_committed_one_component_at_a_time(fs: &dyn Fs, root: &Path) {
    // Fifteen 255-byte directories, 256 bytes each with its slash, and a
    // 250-byte name: a 4,090-byte index path, within §7.1's limits.
    let mut dir = PathBuf::new();
    for level in 0..15 {
        dir.push(format!("{}{level:x}", "d".repeat(254)));
        fs.create_dir(&dir).unwrap();
    }
    let name = "f".repeat(250);
    let path = dir.join(&name);
    assert_eq!(path.as_os_str().len(), 4090);

    // The whole host path is past PATH_MAX on both platforms (4,096 bytes
    // on Linux, 1,024 on macOS), so the kernel refuses it given whole.
    let whole = root.join(&path);
    assert!(whole.as_os_str().len() > 4096);
    assert_eq!(
        std::fs::symlink_metadata(&whole)
            .unwrap_err()
            .raw_os_error(),
        Some(rustix::io::Errno::NAMETOOLONG.raw_os_error())
    );

    // A commit as §7.5 makes one: a temp file written and synced, given
    // its mtime and mode, renamed into place, and its parent synced.
    fs.create_dir(p(".delocal")).unwrap();
    fs.create_dir(p(".delocal/tmp")).unwrap();
    let temp = p(".delocal/tmp/t");
    write(fs, temp, b"deep");
    fs.set_mtime(temp, 1_700_000_000_000_000_001).unwrap();
    fs.set_mode(temp, 0o755).unwrap();
    fs.rename(temp, &path).unwrap();
    fs.sync_dir(&dir).unwrap();

    let st = fs.lstat(&path).unwrap();
    assert_eq!(
        (st.kind, st.size, st.mtime_ns, st.mode),
        (FileKind::File, 4, 1_700_000_000_000_000_001, 0o755)
    );
    assert_eq!(read(fs, &path), b"deep");
    assert_eq!(fs.read_dir(&dir).unwrap(), [OsString::from(name)]);
    assert!(fs.available_space(&dir).unwrap() > 0);

    // And taken apart again the same way.
    fs.remove_file(&path).unwrap();
    while !dir.as_os_str().is_empty() {
        fs.remove_dir(&dir).unwrap();
        dir.pop();
    }
    assert_eq!(fs.read_dir(p("")).unwrap(), [".delocal"]);
}

/// The error of a `result` that stopped at the parent `parent`.
fn assert_stopped_at<T>(result: io::Result<T>, parent: &str) {
    match result {
        Ok(_) => panic!("expected to stop at {parent}"),
        Err(e) => {
            assert_eq!(e.kind(), ErrorKind::NotADirectory);
            assert_eq!(
                ParentNotADirectory::of(&e),
                Some(&ParentNotADirectory {
                    parent: parent.into()
                })
            );
        }
    }
}

pub fn a_parent_that_is_a_file_stops_the_operation(fs: &dyn Fs, _root: &Path) {
    write(fs, p("file"), b"x");
    assert_stopped_at(fs.lstat(p("file/x")), "file");
    assert_stopped_at(fs.create_new(p("file/x/y")), "file");
    fs.create_dir(p("d")).unwrap();
    write(fs, p("d/file"), b"x");
    assert_stopped_at(fs.create_dir(p("d/file/x")), "d/file");

    // A missing parent is not this error but NotFound, which a commit
    // answers by creating the parent (§7.5 step 8).
    let missing = fs.lstat(p("missing/x")).unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::NotFound);
    assert_eq!(ParentNotADirectory::of(&missing), None);
}

/// Everything about a directory tree that an operation could change, read
/// with `std` from outside the layer under test.
fn snapshot(dir: &Path) -> Vec<(OsString, u32, i64, i64, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let contents = if meta.is_file() {
            std::fs::read(&path).unwrap()
        } else {
            Vec::new()
        };
        entries.push((name, meta.mode(), meta.mtime(), meta.mtime_nsec(), contents));
        if meta.is_dir() {
            entries.extend(snapshot(&path));
        }
    }
    entries
}

pub fn a_parent_swapped_for_a_symlink_keeps_everything_inside_the_folder(fs: &dyn Fs, root: &Path) {
    // Beside the folder, a directory for the symlink to point at.
    let outside = root.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("f"), "outside").unwrap();
    std::fs::create_dir(outside.join("sub")).unwrap();
    let before = snapshot(&outside);

    // Inside, `d` is a directory when first seen, then swapped for a
    // symlink to `outside`.
    fs.create_dir(p("d")).unwrap();
    write(fs, p("d/f"), b"inside");
    write(fs, p("x"), b"x");
    fs.lstat(p("d/f")).unwrap();
    std::fs::rename(root.join("d"), root.join("d.old")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("d")).unwrap();

    assert_stopped_at(fs.lstat(p("d/f")), "d");
    assert_stopped_at(fs.lstat(p("d/sub/deeper")), "d");
    assert_stopped_at(fs.read_link(p("d/f")), "d");
    assert_stopped_at(fs.open_read(p("d/f")), "d");
    assert_stopped_at(fs.open_append(p("d/f")), "d");
    assert_stopped_at(fs.create_new(p("d/new")), "d");
    assert_stopped_at(fs.read_dir(p("d/sub")), "d");
    assert_stopped_at(fs.sync_dir(p("d/sub")), "d");
    assert_stopped_at(fs.available_space(p("d/sub")), "d");
    assert_stopped_at(fs.rename(p("x"), p("d/x")), "d");
    assert_stopped_at(fs.rename(p("d/f"), p("y")), "d");
    assert_stopped_at(fs.remove_file(p("d/f")), "d");
    assert_stopped_at(fs.remove_dir(p("d/sub")), "d");
    assert_stopped_at(fs.create_dir(p("d/new")), "d");
    assert_stopped_at(fs.symlink(p("t"), p("d/l")), "d");
    assert_stopped_at(fs.set_mtime(p("d/f"), 1), "d");
    assert_stopped_at(fs.set_mode(p("d/f"), 0o777), "d");

    // At `d` itself is the symlink, and it is not a directory to list.
    assert_eq!(fs.lstat(p("d")).unwrap().kind, FileKind::Symlink);
    assert_eq!(kind_of(fs.read_dir(p("d"))), ErrorKind::NotADirectory);
    assert_eq!(kind_of(fs.sync_dir(p("d"))), ErrorKind::NotADirectory);

    assert_eq!(
        snapshot(&outside),
        before,
        "nothing outside the folder changed"
    );
    assert_eq!(read(fs, p("x")), b"x");
    assert_eq!(read(fs, p("d.old/f")), b"inside");
}
