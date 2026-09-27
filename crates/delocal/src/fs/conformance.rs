//! The contract of [`Fs`] as tests (DESIGN.md §14.2). Each check takes an
//! implementation, makes its own temporary directory and asserts what the
//! scanner and the commit path will rely on. [`conformance_tests!`] turns
//! the whole list into one `#[test]` per check for an implementation.
//!
//! `RealFs` runs them on the machine's real filesystem, which CI does on
//! Linux and macOS. The set-mtime check also asserts that nanoseconds
//! survive a round trip, which answers §7.3's **[verify]** for the
//! filesystems the temporary directories live on (tmpfs or ext4 on Linux,
//! APFS on macOS).

use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

use tempfile::TempDir;

use super::{FileKind, Fs};

/// One `#[test]` per check, each on a fresh value of `$make`.
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
        );
    };
    ($make:expr; $($check:ident),* $(,)?) => {
        $(
            #[test]
            fn $check() {
                let fs = $make;
                $crate::fs::conformance::$check(&fs);
            }
        )*
    };
}
pub(crate) use conformance_tests;

fn tempdir() -> TempDir {
    tempfile::tempdir().unwrap()
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

pub fn read_dir_gives_raw_names_sorted_by_bytes(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    // No two names differ only by case: the macOS runner's APFS is
    // case-insensitive and would refuse the second.
    for name in ["b", "a", "Z", "é"] {
        write(fs, &root.join(name), b"x");
    }
    fs.create_dir(&root.join("d")).unwrap();
    fs.symlink(Path::new("nowhere"), &root.join("s")).unwrap();
    let names = fs.read_dir(root).unwrap();
    // "Z" (0x5A) sorts before "a" (0x61), and "é" (0xC3 0xA9) after ASCII.
    assert_eq!(names, ["Z", "a", "b", "d", "s", "é"]);

    assert!(fs.read_dir(&root.join("d")).unwrap().is_empty());
    assert_eq!(
        kind_of(fs.read_dir(&root.join("missing"))),
        ErrorKind::NotFound
    );
    assert_eq!(
        kind_of(fs.read_dir(&root.join("a"))),
        ErrorKind::NotADirectory
    );
}

pub fn lstat_reports_kind_size_and_mode_without_following(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    write(fs, &root.join("f"), b"hello");
    fs.set_mode(&root.join("f"), 0o640).unwrap();
    write(fs, &root.join("g"), b"");
    fs.create_dir(&root.join("d")).unwrap();
    fs.symlink(Path::new("f"), &root.join("to-f")).unwrap();
    fs.symlink(Path::new("missing-target"), &root.join("dangling"))
        .unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(root.join("sock")).unwrap();

    let f = fs.lstat(&root.join("f")).unwrap();
    assert_eq!((f.kind, f.size, f.mode), (FileKind::File, 5, 0o640));
    assert_eq!(fs.lstat(&root.join("d")).unwrap().kind, FileKind::Dir);
    let to_f = fs.lstat(&root.join("to-f")).unwrap();
    assert_eq!((to_f.kind, to_f.size), (FileKind::Symlink, 1));
    let dangling = fs.lstat(&root.join("dangling")).unwrap();
    assert_eq!(
        (dangling.kind, dangling.size),
        (FileKind::Symlink, "missing-target".len() as u64)
    );
    assert_eq!(fs.lstat(&root.join("sock")).unwrap().kind, FileKind::Other);

    let g = fs.lstat(&root.join("g")).unwrap();
    assert_eq!(f.dev, g.dev);
    assert_ne!(f.ino, g.ino);
    assert_eq!(
        kind_of(fs.lstat(&root.join("missing"))),
        ErrorKind::NotFound
    );
}

pub fn read_link_gives_the_raw_target(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    // Not normalised, not resolved, and it need not exist.
    let target = Path::new("a/./b/../c");
    fs.symlink(target, &root.join("s")).unwrap();
    assert_eq!(fs.read_link(&root.join("s")).unwrap(), target);

    write(fs, &root.join("f"), b"x");
    assert_eq!(
        kind_of(fs.read_link(&root.join("f"))),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        kind_of(fs.read_link(&root.join("missing"))),
        ErrorKind::NotFound
    );
}

pub fn open_read_reads_from_the_start_and_seeks(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    write(fs, &root.join("f"), b"hello world");
    assert_eq!(read(fs, &root.join("f")), b"hello world");

    let mut file = fs.open_read(&root.join("f")).unwrap();
    assert_eq!(file.seek(SeekFrom::Start(6)).unwrap(), 6);
    let mut rest = String::new();
    file.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "world");

    // Opening follows a symlink, as the module docs say.
    fs.symlink(Path::new("f"), &root.join("s")).unwrap();
    assert_eq!(read(fs, &root.join("s")), b"hello world");
    assert_eq!(
        kind_of(fs.open_read(&root.join("missing"))),
        ErrorKind::NotFound
    );
}

pub fn create_new_refuses_anything_already_there(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    write(fs, &root.join("f"), b"abc");
    assert_eq!(read(fs, &root.join("f")), b"abc");
    assert_eq!(
        kind_of(fs.create_new(&root.join("f"))),
        ErrorKind::AlreadyExists
    );
    assert_eq!(read(fs, &root.join("f")), b"abc", "not truncated");

    // A symlink is refused too, dangling or not, so a temp file is never
    // written through one.
    fs.symlink(Path::new("elsewhere"), &root.join("s")).unwrap();
    assert_eq!(
        kind_of(fs.create_new(&root.join("s"))),
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        kind_of(fs.lstat(&root.join("elsewhere"))),
        ErrorKind::NotFound
    );
    fs.create_dir(&root.join("d")).unwrap();
    assert_eq!(
        kind_of(fs.create_new(&root.join("d"))),
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        kind_of(fs.create_new(&root.join("no-parent/f"))),
        ErrorKind::NotFound
    );
}

pub fn open_append_writes_at_the_end_and_needs_a_file(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    write(fs, &root.join("f"), b"abc");
    let mut file = fs.open_append(&root.join("f")).unwrap();
    file.write_all(b"def").unwrap();
    file.sync().unwrap();
    drop(file);
    assert_eq!(read(fs, &root.join("f")), b"abcdef");

    // Opening follows a symlink, as the module docs say.
    fs.symlink(Path::new("f"), &root.join("s")).unwrap();
    let mut file = fs.open_append(&root.join("s")).unwrap();
    file.write_all(b"g").unwrap();
    drop(file);
    assert_eq!(read(fs, &root.join("f")), b"abcdefg");

    assert_eq!(
        kind_of(fs.open_append(&root.join("missing"))),
        ErrorKind::NotFound
    );
    assert_eq!(
        kind_of(fs.lstat(&root.join("missing"))),
        ErrorKind::NotFound
    );
}

pub fn sync_dir_accepts_a_directory(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    fs.create_dir(&root.join("d")).unwrap();
    write(fs, &root.join("d/f"), b"x");
    fs.sync_dir(&root.join("d")).unwrap();
    fs.sync_dir(root).unwrap();
    assert_eq!(
        kind_of(fs.sync_dir(&root.join("missing"))),
        ErrorKind::NotFound
    );
}

pub fn rename_moves_and_replaces_a_file(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    write(fs, &root.join("a"), b"A");
    fs.rename(&root.join("a"), &root.join("b")).unwrap();
    assert_eq!(kind_of(fs.lstat(&root.join("a"))), ErrorKind::NotFound);
    assert_eq!(read(fs, &root.join("b")), b"A");

    write(fs, &root.join("c"), b"C");
    fs.rename(&root.join("c"), &root.join("b")).unwrap();
    assert_eq!(read(fs, &root.join("b")), b"C", "replaced");
    assert_eq!(fs.read_dir(root).unwrap(), ["b"]);

    // A directory moves with its children, as §7.6's displacement expects.
    fs.create_dir(&root.join("d")).unwrap();
    write(fs, &root.join("d/child"), b"x");
    fs.rename(&root.join("d"), &root.join("e")).unwrap();
    assert_eq!(read(fs, &root.join("e/child")), b"x");

    // A file does not replace a directory: both stay as they were, so a
    // commit that meets a directory at its path displaces it first (§7.5
    // step 7).
    assert!(fs.rename(&root.join("b"), &root.join("e")).is_err());
    assert_eq!(read(fs, &root.join("b")), b"C");
    assert_eq!(read(fs, &root.join("e/child")), b"x");

    assert_eq!(
        kind_of(fs.rename(&root.join("missing"), &root.join("x"))),
        ErrorKind::NotFound
    );
}

pub fn remove_file_removes_files_and_symlinks_not_targets(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    write(fs, &root.join("f"), b"x");
    fs.symlink(Path::new("f"), &root.join("s")).unwrap();
    fs.remove_file(&root.join("s")).unwrap();
    assert_eq!(fs.read_dir(root).unwrap(), ["f"]);
    fs.remove_file(&root.join("f")).unwrap();
    assert!(fs.read_dir(root).unwrap().is_empty());

    fs.create_dir(&root.join("d")).unwrap();
    assert!(fs.remove_file(&root.join("d")).is_err());
    assert_eq!(fs.lstat(&root.join("d")).unwrap().kind, FileKind::Dir);
    assert_eq!(
        kind_of(fs.remove_file(&root.join("missing"))),
        ErrorKind::NotFound
    );
}

pub fn remove_dir_removes_only_empty_directories(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    fs.create_dir(&root.join("empty")).unwrap();
    fs.remove_dir(&root.join("empty")).unwrap();
    assert_eq!(kind_of(fs.lstat(&root.join("empty"))), ErrorKind::NotFound);

    fs.create_dir(&root.join("full")).unwrap();
    write(fs, &root.join("full/f"), b"x");
    assert_eq!(
        kind_of(fs.remove_dir(&root.join("full"))),
        ErrorKind::DirectoryNotEmpty
    );
    assert_eq!(read(fs, &root.join("full/f")), b"x");
    assert_eq!(
        kind_of(fs.remove_dir(&root.join("full/f"))),
        ErrorKind::NotADirectory
    );
    assert_eq!(
        kind_of(fs.remove_dir(&root.join("missing"))),
        ErrorKind::NotFound
    );
}

pub fn create_dir_needs_a_parent_and_an_empty_path(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    fs.create_dir(&root.join("d")).unwrap();
    assert_eq!(fs.lstat(&root.join("d")).unwrap().kind, FileKind::Dir);
    assert_eq!(
        kind_of(fs.create_dir(&root.join("d"))),
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        kind_of(fs.create_dir(&root.join("x/y"))),
        ErrorKind::NotFound
    );
    fs.symlink(Path::new("elsewhere"), &root.join("s")).unwrap();
    assert_eq!(
        kind_of(fs.create_dir(&root.join("s"))),
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        kind_of(fs.lstat(&root.join("elsewhere"))),
        ErrorKind::NotFound
    );
}

pub fn symlink_makes_a_link_that_is_never_followed(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    fs.symlink(Path::new("does/not/exist"), &root.join("s"))
        .unwrap();
    assert_eq!(fs.lstat(&root.join("s")).unwrap().kind, FileKind::Symlink);
    assert_eq!(
        fs.read_link(&root.join("s")).unwrap(),
        Path::new("does/not/exist")
    );
    assert_eq!(
        kind_of(fs.symlink(Path::new("other"), &root.join("s"))),
        ErrorKind::AlreadyExists
    );
}

pub fn set_mtime_round_trips_nanoseconds_without_following(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    let f = root.join("f");
    write(fs, &f, b"x");
    let accessed = std::fs::symlink_metadata(&f).unwrap().accessed().unwrap();
    for mtime_ns in [1_700_000_000_123_456_789, 1, 0, -1_500_000_000] {
        fs.set_mtime(&f, mtime_ns).unwrap();
        assert_eq!(fs.lstat(&f).unwrap().mtime_ns, mtime_ns);
    }
    assert_eq!(
        std::fs::symlink_metadata(&f).unwrap().accessed().unwrap(),
        accessed,
        "the access time is left alone"
    );

    // On a symlink the link's own time changes and the target's does not.
    fs.symlink(Path::new("f"), &root.join("s")).unwrap();
    fs.set_mtime(&root.join("s"), 42_000_000_007).unwrap();
    assert_eq!(fs.lstat(&root.join("s")).unwrap().mtime_ns, 42_000_000_007);
    assert_eq!(fs.lstat(&f).unwrap().mtime_ns, -1_500_000_000);
    assert_eq!(
        kind_of(fs.set_mtime(&root.join("missing"), 1)),
        ErrorKind::NotFound
    );
}

pub fn set_mode_sets_bits_and_refuses_a_symlink(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    let f = root.join("f");
    write(fs, &f, b"x");
    fs.set_mode(&f, 0o755).unwrap();
    assert_eq!(fs.lstat(&f).unwrap().mode, 0o755);
    fs.set_mode(&f, 0o600).unwrap();
    assert_eq!(fs.lstat(&f).unwrap().mode, 0o600);
    fs.create_dir(&root.join("d")).unwrap();
    fs.set_mode(&root.join("d"), 0o700).unwrap();
    assert_eq!(fs.lstat(&root.join("d")).unwrap().mode, 0o700);

    fs.symlink(Path::new("f"), &root.join("s")).unwrap();
    assert_eq!(
        kind_of(fs.set_mode(&root.join("s"), 0o777)),
        ErrorKind::InvalidInput
    );
    assert_eq!(fs.lstat(&f).unwrap().mode, 0o600, "the target is untouched");
    assert_eq!(
        kind_of(fs.set_mode(&root.join("missing"), 0o644)),
        ErrorKind::NotFound
    );
}

pub fn available_space_is_reported_for_a_directory(fs: &dyn Fs) {
    let dir = tempdir();
    let root = dir.path();
    assert!(fs.available_space(root).unwrap() > 0);
    assert_eq!(
        kind_of(fs.available_space(&root.join("missing"))),
        ErrorKind::NotFound
    );
}
