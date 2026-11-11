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

#[test]
fn destination_symlink_is_not_traversed() {
    let root = temp_fixture_root("dest_symlink");
    let src = root.join("src");
    let dst = root.join("dest");
    let outside = root.join("outside");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::create_dir_all(&dst).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(src.join("sub/copied"), b"inside-copy").unwrap();
    fs::write(outside.join("sentinel"), b"outside-sentinel").unwrap();
    std::os::unix::fs::symlink("../outside", dst.join("sub")).unwrap();

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());

    let req = SyncRequest {
        operands: SyncOperands::Contents {
            source: src,
            directory: dst.clone(),
        },
        options: SyncOptions {
            delete: true,
            dry_run: false,
            jobs: None,
        },
        context,
    };
    let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
    assert_eq!(outcome.status(), 0);

    assert_eq!(
        fs::read(outside.join("sentinel")).unwrap(),
        b"outside-sentinel",
        "deletion must not follow the destination symlink"
    );
    assert!(
        !outside.join("copied").exists(),
        "transfers must not follow the destination symlink"
    );
    let mut outside_names: Vec<_> = fs::read_dir(&outside)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    outside_names.sort();
    assert_eq!(
        outside_names,
        vec![std::ffi::OsString::from("sentinel")],
        "nothing may be written outside the destination"
    );

    let dst_sub = fs::symlink_metadata(dst.join("sub")).unwrap();
    assert!(dst_sub.is_dir() && !dst_sub.is_symlink());
    assert_eq!(fs::read(dst.join("sub/copied")).unwrap(), b"inside-copy");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn contents_source_inside_destination_is_rejected_before_mutation() {
    let root = temp_fixture_root("contents_overlap");
    let dst = root.join("dest");
    let src = dst.join("src");
    fs::create_dir_all(&src).unwrap();
    let only_copy = src.join("only-copy");
    fs::write(&only_copy, b"source-must-survive").unwrap();

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let messages = std::sync::Mutex::new(Vec::new());
    let sink = |event: Event| {
        if let Event::Diagnostic(diagnostic) = event {
            messages.lock().unwrap().push(diagnostic.message);
        }
        Ok::<(), io::Error>(())
    };

    let req = SyncRequest {
        operands: SyncOperands::Contents {
            source: src.clone(),
            directory: dst.clone(),
        },
        options: SyncOptions {
            delete: true,
            dry_run: false,
            jobs: None,
        },
        context,
    };
    let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
    assert_eq!(outcome.status(), 2);
    assert!(
        messages
            .lock()
            .unwrap()
            .iter()
            .any(|message| message.contains("overlap")),
        "expected an overlap diagnostic"
    );

    assert_eq!(fs::read(&only_copy).unwrap(), b"source-must-survive");
    assert!(dst.is_dir());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn overlap_rejected_for_operand_aliases() {
    let root = temp_fixture_root("operand_alias");
    let dst = root.join("dest");
    let src = dst.join("src");
    fs::create_dir_all(&src).unwrap();
    let only_copy = src.join("only-copy");
    fs::write(&only_copy, b"alias-source").unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&dst, &alias).unwrap();

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());

    let symlinked_source = alias.join("src");
    let req_alias = SyncRequest {
        operands: SyncOperands::Contents {
            source: symlinked_source,
            directory: dst.clone(),
        },
        options: SyncOptions {
            delete: true,
            dry_run: false,
            jobs: None,
        },
        context: context.clone(),
    };
    let outcome_alias = ish_fs::sync::execute(&req_alias, &cancel, &sink);
    assert_eq!(outcome_alias.status(), 2);
    assert_eq!(fs::read(&only_copy).unwrap(), b"alias-source");

    let dotted_source = root.join("dest/../dest/src");
    let req_dotted = SyncRequest {
        operands: SyncOperands::Contents {
            source: dotted_source,
            directory: dst.clone(),
        },
        options: SyncOptions {
            delete: true,
            dry_run: false,
            jobs: None,
        },
        context,
    };
    let outcome_dotted = ish_fs::sync::execute(&req_dotted, &cancel, &sink);
    assert_eq!(outcome_dotted.status(), 2);
    assert_eq!(fs::read(&only_copy).unwrap(), b"alias-source");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn overlap_rejected_for_multiple_source_into() {
    let root = temp_fixture_root("multi_source_overlap");
    let src = root.join("src");
    let dst = root.join("dest");
    let inside = dst.join("inside");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&inside).unwrap();
    fs::write(src.join("file"), b"source-file").unwrap();
    let inside_file = inside.join("only-copy");
    fs::write(&inside_file, b"inside-source").unwrap();

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());

    let req = SyncRequest {
        operands: SyncOperands::Into {
            directory: dst.clone(),
            sources: vec![src.clone(), inside.clone()],
        },
        options: SyncOptions {
            delete: true,
            dry_run: false,
            jobs: None,
        },
        context,
    };
    let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
    assert_eq!(outcome.status(), 2);

    assert_eq!(fs::read(src.join("file")).unwrap(), b"source-file");
    assert_eq!(fs::read(&inside_file).unwrap(), b"inside-source");
    assert!(
        !dst.join("src").exists(),
        "no mapping may run after overlap rejection"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn delete_permission_failure_reports_failed_status() {
    let root = temp_fixture_root("delete_permission");
    let src = root.join("src");
    let dst = root.join("dest");
    let locked = dst.join("locked");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&locked).unwrap();
    let kept = locked.join("kept");
    fs::write(&kept, b"must-be-kept").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let context = Context {
        cwd: root.clone(),
        home: None,
        scratch: root.join("scratch"),
    };
    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());

    let req = SyncRequest {
        operands: SyncOperands::Contents {
            source: src,
            directory: dst.clone(),
        },
        options: SyncOptions {
            delete: true,
            dry_run: false,
            jobs: None,
        },
        context,
    };
    let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    if rustix::process::geteuid().as_raw() != 0 {
        assert_eq!(outcome.status(), 1);
        assert_eq!(outcome.progress.deleted_files, 0);
        assert_eq!(fs::read(&kept).unwrap(), b"must-be-kept");
        assert!(kept.exists(), "failed deletion must retain the entry");
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn exact_destination_through_a_symlinked_parent_operand_is_followed() {
    let root = temp_fixture_root("exact_link_parent");
    fs::write(root.join("file"), b"payload").unwrap();
    fs::create_dir_all(root.join("real")).unwrap();
    std::os::unix::fs::symlink("real", root.join("link")).unwrap();
    let context = Context { cwd: root.clone(), home: None, scratch: root.join("scratch") };
    let request = SyncRequest {
        operands: SyncOperands::Exact { source: "file".into(), destination: "link/file".into() },
        options: SyncOptions::default(),
        context,
    };
    let outcome = ish_fs::sync::execute(&request, &Cancellation::default(), &|_event: Event| Ok::<(), io::Error>(()));
    assert_eq!(outcome.status(), 0);
    assert!(fs::symlink_metadata(root.join("link")).unwrap().is_symlink());
    assert_eq!(fs::read(root.join("real/file")).unwrap(), b"payload");
    let _ = fs::remove_dir_all(&root);
}
