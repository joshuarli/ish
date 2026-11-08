use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use ish_fs::sync::{SyncOperands, SyncOptions, SyncRequest};
use ish_fs::{Cancellation, Context, Event};

fn temp_fixture_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ish_fs_sync_adv_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn readonly_and_permission_boundaries() {
    let root = temp_fixture_root("readonly");
    let src = root.join("src");
    let dst = root.join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();

    let src_file = src.join("readonly.txt");
    fs::write(&src_file, b"initial readonly content").unwrap();
    fs::set_permissions(&src_file, fs::Permissions::from_mode(0o444)).unwrap();

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());

    let req1 = SyncRequest {
        operands: SyncOperands::Contents {
            source: src.clone(),
            directory: dst.clone(),
        },
        options: SyncOptions::default(),
        context: context.clone(),
    };
    let outcome1 = ish_fs::sync::execute(&req1, &cancel, &sink);
    assert_eq!(outcome1.status(), 0);
    assert_eq!(outcome1.progress.transferred_files, 1);
    let dst_file = dst.join("readonly.txt");
    assert_eq!(fs::symlink_metadata(&dst_file).unwrap().mode() & 0o777, 0o444);

    fs::set_permissions(&src_file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&src_file, b"updated readonly content").unwrap();
    fs::set_permissions(&src_file, fs::Permissions::from_mode(0o444)).unwrap();

    let req2 = SyncRequest {
        operands: SyncOperands::Contents {
            source: src,
            directory: dst.clone(),
        },
        options: SyncOptions::default(),
        context: context.clone(),
    };
    let outcome2 = ish_fs::sync::execute(&req2, &cancel, &sink);
    assert_eq!(outcome2.status(), 0);
    assert_eq!(outcome2.progress.transferred_files, 1);
    assert_eq!(fs::read(&dst_file).unwrap(), b"updated readonly content");
    assert_eq!(fs::symlink_metadata(&dst_file).unwrap().mode() & 0o777, 0o444);

    let ro_dst = root.join("ro_dst");
    fs::create_dir_all(&ro_dst).unwrap();
    fs::set_permissions(&ro_dst, fs::Permissions::from_mode(0o555)).unwrap();

    let req_fail = SyncRequest {
        operands: SyncOperands::Exact {
            source: src_file.clone(),
            destination: ro_dst.join("readonly.txt"),
        },
        options: SyncOptions::default(),
        context,
    };
    let outcome_fail = ish_fs::sync::execute(&req_fail, &cancel, &sink);
    assert_eq!(outcome_fail.status(), 1);

    fs::set_permissions(&ro_dst, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&src_file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&dst_file, fs::Permissions::from_mode(0o644)).unwrap();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn broken_symlink_preservation_and_skip() {
    let root = temp_fixture_root("broken_symlink");
    let src = root.join("src");
    let dst = root.join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();

    let broken_target = PathBuf::from("nonexistent_relative_target_12345");
    std::os::unix::fs::symlink(&broken_target, src.join("link")).unwrap();

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());

    let req1 = SyncRequest {
        operands: SyncOperands::Contents {
            source: src.clone(),
            directory: dst.clone(),
        },
        options: SyncOptions::default(),
        context: context.clone(),
    };
    let outcome1 = ish_fs::sync::execute(&req1, &cancel, &sink);
    assert_eq!(outcome1.status(), 0);
    assert_eq!(outcome1.progress.transferred_files, 1);
    assert_eq!(fs::read_link(dst.join("link")).unwrap(), broken_target);

    let outcome2 = ish_fs::sync::execute(&req1, &cancel, &sink);
    assert_eq!(outcome2.status(), 0);
    assert_eq!(outcome2.progress.transferred_files, 0);
    assert_eq!(outcome2.progress.skipped_files, 1);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn non_utf8_raw_byte_filenames() {
    let root = temp_fixture_root("non_utf8");
    let src = root.join("src");
    let dst = root.join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();

    let raw_name = std::ffi::OsStr::from_bytes(b"raw-\xff-\xfe-sample.bin");
    let src_file = src.join(raw_name);
    match fs::write(&src_file, b"raw byte payload") {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        Err(error) => panic!("create raw-name source file: {error}"),
    }

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());

    let req1 = SyncRequest {
        operands: SyncOperands::Contents {
            source: src.clone(),
            directory: dst.clone(),
        },
        options: SyncOptions::default(),
        context: context.clone(),
    };
    let outcome1 = ish_fs::sync::execute(&req1, &cancel, &sink);
    assert_eq!(outcome1.status(), 0);
    assert_eq!(outcome1.progress.transferred_files, 1);

    let dst_file = dst.join(raw_name);
    assert!(dst_file.exists());
    assert_eq!(fs::read(&dst_file).unwrap(), b"raw byte payload");

    let outcome2 = ish_fs::sync::execute(&req1, &cancel, &sink);
    assert_eq!(outcome2.status(), 0);
    assert_eq!(outcome2.progress.transferred_files, 0);
    assert_eq!(outcome2.progress.skipped_files, 1);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn cancellation_cleans_temporary_artifacts() {
    let root = temp_fixture_root("cancellation");
    let src = root.join("src");
    let dst = root.join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();

    for i in 1..=10 {
        fs::write(src.join(format!("file-{i}.bin")), vec![b'X'; 64 * 1024]).unwrap();
        fs::write(dst.join(format!("file-{i}.bin")), vec![b'Y'; 64 * 1024]).unwrap();
    }

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    cancel.cancel();

    let sink = |_event: Event| Ok::<(), io::Error>(());
    let req = SyncRequest {
        operands: SyncOperands::Contents {
            source: src,
            directory: dst.clone(),
        },
        options: SyncOptions::default(),
        context,
    };
    let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
    assert_eq!(outcome.status(), 130);

    for entry in fs::read_dir(&dst).unwrap().flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        assert!(
            !name_str.starts_with(".ish-sync-") || !name_str.ends_with(".tmp"),
            "found orphaned staging file: {name_str}"
        );
    }

    for i in 1..=10 {
        assert_eq!(
            fs::read(dst.join(format!("file-{i}.bin"))).unwrap(),
            vec![b'Y'; 64 * 1024],
            "pre-existing file {i} must be preserved on cancellation"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_atomic_replacement_cleans_temp_on_source_error() {
    let root = temp_fixture_root("atomic_cleanup");
    let src = root.join("src");
    let dst = root.join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();

    let target_file = dst.join("target.bin");
    fs::write(&target_file, b"original-destination-content").unwrap();

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());

    let req = SyncRequest {
        operands: SyncOperands::Exact {
            source: src.join("nonexistent_source.bin"),
            destination: target_file.clone(),
        },
        options: SyncOptions::default(),
        context,
    };
    let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
    assert_eq!(outcome.status(), 1);

    assert_eq!(fs::read(&target_file).unwrap(), b"original-destination-content");

    for entry in fs::read_dir(&dst).unwrap().flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        assert!(
            !name_str.starts_with(".ish-sync-"),
            "found orphaned staging file: {name_str}"
        );
    }

    let _ = fs::remove_dir_all(&root);
}
