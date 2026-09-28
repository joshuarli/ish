//! Filesystem round trips: recursive removal, moves with overwrite,
//! directory metadata, symlinks, raw-byte names, hard-link groups, and large
//! and sparse files, through undo, redo, and undo again. Every undo and redo
//! runs in a fresh process, so each step also exercises restart.

mod common;

#[cfg(target_vendor = "apple")]
use std::os::darwin::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::time::{Duration, SystemTime};

use common::*;
use ish_undo::journal::{Content, Strength};

fn set_mtime(path: &std::path::Path, secs: u64) {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    let f = std::fs::File::options().read(true).open(path).unwrap();
    f.set_modified(t).unwrap();
}

fn dir_xattr_name() -> &'static std::ffi::CStr {
    if cfg!(target_os = "linux") {
        c"user.ish-test"
    } else {
        c"com.ish.test"
    }
}

fn set_dir_xattr(path: &std::path::Path, value: &[u8]) -> bool {
    let fd = ish_undo::sys::open_dir(path).unwrap();
    rustix::fs::fsetxattr(
        &fd,
        dir_xattr_name(),
        value,
        rustix::fs::XattrFlags::empty(),
    )
    .is_ok()
}

fn get_dir_xattr(path: &std::path::Path) -> Option<Vec<u8>> {
    let fd = ish_undo::sys::open_dir(path).unwrap();
    let mut buf = vec![0u8; 256];
    let n = rustix::fs::fgetxattr(&fd, dir_xattr_name(), &mut buf[..]).ok()?;
    buf.truncate(n);
    Some(buf)
}

#[test]
fn recursive_removal_restores_contents_and_directory_metadata() {
    let fx = Fixture::new("rm-tree");
    fx.write("d/a.txt", b"alpha\n");
    let bin: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 251) as u8).collect();
    fx.write("d/sub/b.bin", &bin);
    fx.write("d/sub/deeper/c", b"");
    std::fs::create_dir(fx.path("d/empty")).unwrap();
    std::fs::set_permissions(fx.path("d/a.txt"), std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::set_permissions(fx.path("d/sub"), std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::set_permissions(fx.path("d/empty"), std::fs::Permissions::from_mode(0o711)).unwrap();
    let xattr = set_dir_xattr(&fx.path("d/sub"), b"meta");
    set_mtime(&fx.path("d/a.txt"), 1_000_000_000);
    set_mtime(&fx.path("d/sub/deeper"), 1_100_000_000);
    set_mtime(&fx.path("d/sub"), 1_200_000_000);
    set_mtime(&fx.path("d"), 1_300_000_000);

    let (status, id, err) = fx.shell(&[&format!("rm:{}", argv(&["-r", "d"]))]);
    assert_eq!(status, 0, "{err}");
    assert!(!fx.exists("d"));
    let expect = unlink_strength(&fx.work);
    assert!(
        fx.strengths(id).iter().all(|s| *s == expect),
        "{:?}",
        fx.strengths(id)
    );
    // Clone and link capture never copy payload bytes.
    assert!(fx.model(id).saved_versions().all(|s| s.copied == 0));

    for round in 0..2 {
        let (status, _, err) = fx.undo(&[&id.to_string()]);
        assert_eq!(status, 0, "round {round}: {err}");
        assert_eq!(fx.read("d/a.txt"), b"alpha\n");
        assert_eq!(fx.read("d/sub/b.bin"), bin);
        assert_eq!(fx.read("d/sub/deeper/c"), b"");
        assert!(fx.path("d/empty").is_dir());
        assert_eq!(mode(&fx.path("d/a.txt")), 0o640);
        assert_eq!(mode(&fx.path("d/sub")), 0o750);
        assert_eq!(mode(&fx.path("d/empty")), 0o711);
        let mtime = |p: &str| std::fs::metadata(fx.path(p)).unwrap().mtime();
        assert_eq!(mtime("d/a.txt"), 1_000_000_000);
        // Directory times are applied after their children are restored.
        assert_eq!(mtime("d/sub/deeper"), 1_100_000_000);
        assert_eq!(mtime("d/sub"), 1_200_000_000);
        assert_eq!(mtime("d"), 1_300_000_000);
        if xattr {
            assert_eq!(
                get_dir_xattr(&fx.path("d/sub")).as_deref(),
                Some(&b"meta"[..])
            );
        }

        let (status, _, err) = fx.undo(&["redo", &id.to_string()]);
        assert_eq!(status, 0, "round {round}: {err}");
        assert!(!fx.exists("d"), "redo removes the tree again");
    }
}

#[test]
fn move_over_existing_file_round_trips() {
    let fx = Fixture::new("mv-over");
    fx.write("a", b"new contents");
    fx.write("b", b"old b");
    let a_ino = ino(&fx.path("a"));
    let (status, id, err) = fx.shell(&[&format!("mv:{}", argv(&["a", "b"]))]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("b"), b"new contents");
    assert_eq!(
        ino(&fx.path("b")),
        a_ino,
        "same-filesystem move is a rename"
    );
    assert!(!fx.exists("a"));

    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("a"), b"new contents");
    assert_eq!(ino(&fx.path("a")), a_ino, "undo moves the same object back");
    assert_eq!(fx.read("b"), b"old b");

    let (status, _, err) = fx.undo(&["redo"]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("b"), b"new contents");
    assert!(!fx.exists("a"));
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("b"), b"old b");
}

#[test]
fn moving_a_directory_does_not_copy_it() {
    let fx = Fixture::new("mv-dir");
    for i in 0..50 {
        fx.write(&format!("tree/f{i}"), &vec![b'x'; 10_000]);
    }
    std::fs::create_dir(fx.path("dest")).unwrap();
    let (status, id, err) = fx.shell(&[&format!("mv:{}", argv(&["tree", "dest"]))]);
    assert_eq!(status, 0, "{err}");
    assert!(fx.path("dest/tree/f49").exists());
    let mut stores = ish_undo::store::Stores::new(fx.home_store(), id).unwrap();
    let usage = ish_undo::retention::usage(&mut stores, id);
    assert_eq!(usage.objects, 0, "a rename preserves nothing");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert!(fx.path("tree/f49").exists());
    assert!(!fx.exists("dest/tree"));
}

#[test]
fn symlinks_are_preserved_and_never_followed() {
    let fx = Fixture::new("symlinks");
    fx.write("outside/keep", b"keep me");
    fx.write("target", b"target data");
    symlink("target", fx.path("link")).unwrap();
    symlink("does/not/exist", fx.path("dangling")).unwrap();
    std::fs::create_dir(fx.path("d")).unwrap();
    symlink(fx.path("outside"), fx.path("d/escape")).unwrap();
    let (status, _, err) = fx.shell(&[
        &format!("rm:{}", argv(&["link", "dangling"])),
        &format!("rm:{}", argv(&["-r", "d"])),
    ]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        fx.read("outside/keep"),
        b"keep me",
        "removal did not follow the symlink"
    );
    assert_eq!(fx.read("target"), b"target data");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        std::fs::read_link(fx.path("link")).unwrap().as_os_str(),
        "target"
    );
    assert_eq!(
        std::fs::read_link(fx.path("dangling")).unwrap().as_os_str(),
        "does/not/exist"
    );
    assert_eq!(
        std::fs::read_link(fx.path("d/escape")).unwrap(),
        fx.path("outside")
    );
}

/// Whether the filesystem accepts names that are not valid UTF-8 (APFS
/// rejects them with EILSEQ; Linux filesystems store any bytes).
fn accepts_raw_names(dir: &std::path::Path) -> bool {
    let probe = dir.join(raw_name(b".probe\xff"));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[test]
fn raw_byte_names_round_trip() {
    let fx = Fixture::new("raw-names");
    let raw_ok = accepts_raw_names(&fx.work);
    let mut names: Vec<&[u8]> = vec![b"line\nbreak", "caf\u{e9}".as_bytes(), b"tab\tand space"];
    let (top, moved): (&[u8], &[u8]) = if raw_ok {
        names.extend([&b"caf\xe9"[..], b"\xff\xfe"]);
        (b"top\x80", b"moved\x81")
    } else {
        eprintln!("skipping non-UTF-8 names: the filesystem rejects them");
        ("top\u{3b1}".as_bytes(), "moved\u{3b2}".as_bytes())
    };
    std::fs::create_dir(fx.path("d")).unwrap();
    for name in &names {
        std::fs::write(fx.path("d").join(raw_name(name)), name).unwrap();
    }
    std::fs::write(fx.work.join(raw_name(top)), b"top").unwrap();
    let (status, _, err) = fx.shell(&[
        &format!("rm:{}", argv(&["-r", "d"])),
        &format!("mv:{}", argv(&[&escape(top), &escape(moved)])),
    ]);
    assert_eq!(status, 0, "{err}");
    assert!(fx.work.join(raw_name(moved)).exists());
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    for name in &names {
        assert_eq!(
            std::fs::read(fx.path("d").join(raw_name(name))).unwrap(),
            *name
        );
    }
    assert_eq!(std::fs::read(fx.work.join(raw_name(top))).unwrap(), b"top");
    let mut listed: Vec<Vec<u8>> = std::fs::read_dir(fx.path("d"))
        .unwrap()
        .map(|e| e.unwrap().file_name().as_bytes().to_vec())
        .collect();
    listed.sort();
    let mut expected: Vec<Vec<u8>> = names.iter().map(|n| n.to_vec()).collect();
    expected.sort();
    assert_eq!(listed, expected);
}

#[test]
fn hard_link_groups_are_rejoined_without_touching_outside_aliases() {
    let fx = Fixture::new("hardlinks");
    fx.write("d/x", b"shared");
    std::fs::hard_link(fx.path("d/x"), fx.path("d/y")).unwrap();
    fx.write("d/z", b"with outside alias");
    std::fs::hard_link(fx.path("d/z"), fx.path("alias")).unwrap();
    let (status, _, err) = fx.shell(&[&format!("rm:{}", argv(&["-r", "d"]))]);
    assert_eq!(status, 0, "{err}");
    std::fs::write(fx.path("alias"), b"alias changed later").unwrap();

    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(ino(&fx.path("d/x")), ino(&fx.path("d/y")), "group rejoined");
    assert_eq!(fx.read("d/x"), b"shared");
    // The outside alias keeps its newer contents and is not modified.
    assert_eq!(fx.read("alias"), b"alias changed later");
    let strength = unlink_strength(&fx.work);
    if strength == Strength::Clone {
        assert_eq!(
            fx.read("d/z"),
            b"with outside alias",
            "frozen version restored"
        );
        assert_ne!(ino(&fx.path("d/z")), ino(&fx.path("alias")));
        assert!(err.contains("hard links") || fx.undo(&["show"]).1.contains("hard links"));
    } else {
        // A linked version is the retained inode: the alias's write shows.
        assert_eq!(ino(&fx.path("d/z")), ino(&fx.path("alias")));
    }
}

#[test]
fn large_and_sparse_files_round_trip_without_payload_copies() {
    let fx = Fixture::new("large");
    let large = 24 * 1024 * 1024;
    let pattern = |i: usize| (i.wrapping_mul(2_654_435_761) >> 13) as u8;
    let data: Vec<u8> = (0..large).map(pattern).collect();
    fx.write("large.bin", &data);
    let sparse = fx.path("sparse.img");
    {
        let f = std::fs::File::create(&sparse).unwrap();
        f.set_len(512 * 1024 * 1024).unwrap();
        rustix::io::pwrite(&f, b"head", 0).unwrap();
        rustix::io::pwrite(&f, b"tail", 512 * 1024 * 1024 - 4).unwrap();
    }
    let sparse_blocks = std::fs::metadata(&sparse).unwrap().blocks();
    let (status, id, err) = fx.shell(&[&format!("rm:{}", argv(&["large.bin", "sparse.img"]))]);
    assert_eq!(status, 0, "{err}");
    let model = fx.model(id);
    for saved in model.saved_versions() {
        assert!(matches!(saved.content, Content::File { .. }));
        assert_eq!(
            saved.copied, 0,
            "deletion capture did not copy payload bytes"
        );
    }
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert!(fx.read("large.bin") == data, "large file contents restored");
    let meta = std::fs::metadata(&sparse).unwrap();
    assert_eq!(meta.len(), 512 * 1024 * 1024);
    // Restoring through a clone or link keeps the file sparse.
    assert!(
        meta.blocks() <= sparse_blocks.max(64) * 2,
        "blocks {} vs {}",
        meta.blocks(),
        sparse_blocks
    );
    let f = std::fs::File::open(&sparse).unwrap();
    let mut buf = [0u8; 4];
    rustix::io::pread(&f, &mut buf, 0).unwrap();
    assert_eq!(&buf, b"head");
    rustix::io::pread(&f, &mut buf, 512 * 1024 * 1024 - 4).unwrap();
    assert_eq!(&buf, b"tail");
}

#[test]
fn byte_copy_fallback_keeps_sparse_files_sparse() {
    let fx = Fixture::new("sparse-copy");
    let sparse = fx.path("sparse.img");
    {
        let f = std::fs::File::create(&sparse).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
        rustix::io::pwrite(&f, b"data", 32 * 1024 * 1024).unwrap();
    }
    // Force the frozen-copy path: a redirection needs a frozen pre-image and
    // cloning is made unavailable.
    let (status, id, err) = fx.shell_with(
        SESSION,
        &["--fault", "clone:0=ENOTSUP", "--fault", "clone:1=ENOTSUP"],
        &["append:sparse.img=!"],
    );
    assert_eq!(status, 0, "{err}");
    let saved: Vec<_> = fx.model(id).saved_versions().cloned().collect();
    assert_eq!(saved[0].strength(), Some(Strength::Copy));
    assert!(
        saved[0].copied < 1024 * 1024,
        "holes were skipped: copied {}",
        saved[0].copied
    );
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    let meta = std::fs::metadata(&sparse).unwrap();
    assert_eq!(meta.len(), 64 * 1024 * 1024);
}

#[test]
fn dangerous_targets_are_refused_without_executing() {
    use ish_undo::ops::{Guard, Refusal};
    let fx = Fixture::new("guards");
    fx.write("f", b"x");
    // Create the store so its ancestors are protected.
    fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    let guard = Guard::new(std::slice::from_ref(&fx.store));
    let stat = |p: &std::path::Path| ish_undo::sys::lstat(p).unwrap();
    let name = |p: &std::path::Path| {
        p.file_name()
            .unwrap_or(std::ffi::OsStr::new("/"))
            .to_owned()
    };

    // Nonexecuting planner decisions: nothing below is removed.
    let root = std::path::Path::new("/");
    assert_eq!(
        guard.check(&name(root), &stat(root), None),
        Err(Refusal::Root)
    );
    for protected in [&fx.home, &fx.store, &fx.root] {
        assert!(
            matches!(
                guard.check(&name(protected), &stat(protected), None),
                Err(Refusal::Store(_))
            ),
            "{}",
            protected.display()
        );
    }
    let inside = fx.store.join("txn");
    let parent = ish_undo::sys::open_dir(&fx.store).unwrap();
    use std::os::fd::AsFd;
    assert!(matches!(
        guard.check(&name(&inside), &stat(&inside), Some(parent.as_fd())),
        Err(Refusal::Store(_))
    ));
    assert_eq!(
        guard.check(std::ffi::OsStr::new(".."), &stat(&fx.work), None),
        Err(Refusal::DotOrDotDot)
    );
    let work_parent = ish_undo::sys::open_dir(&fx.root).unwrap();
    assert_eq!(
        guard.check(&name(&fx.work), &stat(&fx.work), Some(work_parent.as_fd())),
        Ok(())
    );

    // Through the commands, against the fixture-local home only.
    let home = fx.home.to_str().unwrap();
    let (status, _, err) = fx.shell(&[&format!("rm:{}", argv(&["-rf", home]))]);
    assert_eq!(status, 1);
    assert!(err.contains("undo store"), "{err}");
    assert!(fx.store.exists());
    let (status, _, err) = fx.shell(&[&format!("mv:{}", argv(&[home, "moved-home"]))]);
    assert_eq!(status, 1);
    assert!(err.contains("undo store"), "{err}");
    let (status, _, err) = fx.shell(&[&format!("rm:{}", argv(&["-r", "."]))]);
    assert_eq!(status, 1);
    assert!(err.contains("'.' or '..'"), "{err}");

    fx.write("d/inner/x", b"x");
    let (status, _, err) = fx.shell(&[&format!("mv:{}", argv(&["d", "d/inner/deeper"]))]);
    assert_eq!(status, 1);
    assert!(err.contains("subdirectory of itself"), "{err}");
    assert!(fx.exists("d/inner/x"));
}

#[test]
fn file_metadata_survives_capture_and_restore() {
    let fx = Fixture::new("file-meta");
    fx.write("f", b"meta");
    let f = fx.path("f");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o4751)).unwrap();
    set_mtime(&f, 1_234_567_890);
    let xname = if cfg!(target_os = "linux") {
        c"user.ish-file"
    } else {
        c"com.ish.file"
    };
    let file = std::fs::File::open(&f).unwrap();
    let xattr = rustix::fs::fsetxattr(&file, xname, b"xv", rustix::fs::XattrFlags::empty()).is_ok();
    let acl = cfg!(target_vendor = "apple")
        && std::process::Command::new("/bin/chmod")
            .args(["+a", "user:nobody allow read"])
            .arg(&f)
            .status()
            .unwrap()
            .success();
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsFd;
        ish_undo::sys::set_flags(file.as_fd(), libc::UF_HIDDEN).unwrap();
    }
    drop(file);

    let (status, _, err) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    assert_eq!(status, 0, "{err}");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");

    assert_eq!(fx.read("f"), b"meta");
    assert_eq!(mode(&f), 0o4751);
    assert_eq!(std::fs::metadata(&f).unwrap().mtime(), 1_234_567_890);
    if xattr {
        let file = std::fs::File::open(&f).unwrap();
        let mut buf = [0u8; 8];
        let n = rustix::fs::fgetxattr(&file, xname, &mut buf[..]).unwrap();
        assert_eq!(&buf[..n], b"xv");
    }
    if acl {
        let ls = std::process::Command::new("/bin/ls")
            .arg("-le")
            .arg(&f)
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&ls.stdout);
        assert!(listing.contains("user:nobody allow read"), "{listing}");
    }
    #[cfg(target_vendor = "apple")]
    assert_ne!(
        std::fs::metadata(&f).unwrap().st_flags() & libc::UF_HIDDEN,
        0
    );
}

#[test]
fn dot_operands_are_refused_before_resolution() {
    let fx = Fixture::new("dots");
    fx.write("d/keep", b"k");
    for operand in ["d/.", "d/./", "d/..", "."] {
        let (status, _, err) = fx.shell(&[&format!("rm:{}", argv(&["-r", operand]))]);
        assert_eq!(status, 1, "{operand}: {err}");
        assert!(err.contains("'.' or '..'"), "{operand}: {err}");
    }
    let (status, _, err) = fx.shell(&[&format!("mv:{}", argv(&["d/.", "e"]))]);
    assert_eq!(status, 1, "{err}");
    assert_eq!(fx.read("d/keep"), b"k");
    // A trailing slash on a real name is not a dot operand.
    let (status, _, err) = fx.shell(&[&format!("rm:{}", argv(&["-r", "d/"]))]);
    assert_eq!(status, 0, "{err}");
    assert!(!fx.exists("d"));
}

#[test]
fn read_only_files_keep_extended_attributes() {
    let fx = Fixture::new("ro-xattr");
    fx.write("ro", b"read only");
    let name = if cfg!(target_os = "linux") {
        c"user.ish-ro"
    } else {
        c"com.ish.ro"
    };
    let file = std::fs::File::open(fx.path("ro")).unwrap();
    if rustix::fs::fsetxattr(&file, name, b"v", rustix::fs::XattrFlags::empty()).is_err() {
        eprintln!("skipping: extended attributes unsupported here");
        return;
    }
    std::fs::set_permissions(fx.path("ro"), std::fs::Permissions::from_mode(0o444)).unwrap();
    let (status, id, err) = fx.shell(&[&format!("rm:{}", argv(&["ro"]))]);
    assert_eq!(status, 0, "{err}");
    let (_, show, _) = fx.undo(&["show", &id.to_string()]);
    assert!(!show.contains("not restored"), "{show}");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(mode(&fx.path("ro")), 0o444);
    let file = std::fs::File::open(fx.path("ro")).unwrap();
    let mut buf = [0u8; 4];
    let n = rustix::fs::fgetxattr(&file, name, &mut buf[..]).unwrap();
    assert_eq!(&buf[..n], b"v");
}
