use super::*;

fn changed_published_version_retains_source(merge: bool, mode: bool) {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let mut request = fixture.request();
    if merge {
        fs::create_dir(fixture.0.join("source")).unwrap();
        fs::create_dir(fixture.0.join("destination")).unwrap();
        fs::write(fixture.0.join("source/first"), b"original").unwrap();
        fs::write(fixture.0.join("source/second"), b"original").unwrap();
        request.options.merge = true;
    } else { fs::write(fixture.0.join("source"), b"original").unwrap(); }
    if merge {
        for name in ["first", "second"] { fs::set_permissions(fixture.0.join("source").join(name), fs::Permissions::from_mode(0o644)).unwrap(); }
    } else { fs::set_permissions(fixture.0.join("source"), fs::Permissions::from_mode(0o644)).unwrap(); }
    let base = fixture.0.clone();
    let altered = Arc::new(Mutex::new(None));
    let changed = altered.clone();
    let (outcome, _) = fixture.run(&request, move |point| {
        force_exdev(point)?;
        let path = if !merge && point == "after_publish" { Some(base.join("destination")) }
            else if merge && point == "before_copy" { fs::read_dir(base.join("destination")).unwrap().next().map(|entry| entry.unwrap().path()) }
            else { None };
        if let Some(path) = path {
            if mode {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            } else { fs::write(&path, b"tampered-longer").unwrap(); }
            *changed.lock().unwrap() = Some(path);
        }
        Ok(())
    });
    assert!(altered.lock().unwrap().is_some());
    assert_eq!(outcome.status(), 1, "merge={merge}, mode={mode}");
    if merge {
        assert_eq!(fs::read_dir(fixture.0.join("source")).unwrap().count(), 2);
    } else { assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"original"); }
}

#[test]
fn repair_after_publish_content_mutation_retains_source() { changed_published_version_retains_source(false, false); }
#[test]
fn repair_after_publish_mode_mutation_retains_source() { changed_published_version_retains_source(false, true); }
#[test]
fn repair_earlier_merge_child_content_mutation_retains_source() { changed_published_version_retains_source(true, false); }
#[test]
fn repair_earlier_merge_child_mode_mutation_retains_source() { changed_published_version_retains_source(true, true); }

#[test]
fn repair_symlink_hardlinks_preserve_invocation_topology() {
    use std::os::unix::fs::symlink;
    for operation in [Operation::Copy, Operation::Move] {
        for into in [false, true] {
            let fixture = Fixture::new();
            fs::create_dir(fixture.0.join("source")).unwrap();
            symlink("missing-target", fixture.0.join("source/a")).unwrap();
            fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source/b")).unwrap();
            fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source/c")).unwrap();
            let original = fs::symlink_metadata(fixture.0.join("source/a")).unwrap();
            assert!(original.file_type().is_symlink());
            for name in ["b", "c"] { assert_eq!(fs::symlink_metadata(fixture.0.join("source").join(name)).unwrap().ino(), original.ino()); }
            let mut request = fixture.request();
            request.operation = operation;
            if into {
                fs::create_dir(fixture.0.join("destination")).unwrap();
                request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/a".into(), rest: vec!["source/b".into(), "source/c".into()] };
            }
            let (outcome, _) = fixture.run(&request, force_exdev);
            assert_eq!(outcome.status(), 0);
            let a = fs::symlink_metadata(fixture.0.join("destination/a")).unwrap();
            for name in ["b", "c"] {
                let path = fixture.0.join("destination").join(name);
                assert_eq!(fs::symlink_metadata(&path).unwrap().ino(), a.ino(), "operation={operation:?}, into={into}");
                assert_eq!(fs::read_link(path).unwrap(), PathBuf::from("missing-target"));
            }
        }
    }
}

#[test]
fn repair_unrelated_hardlink_groups_transfer_concurrently() {
    use std::sync::Condvar;
    use std::time::Duration;
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("source")).unwrap();
    for group in ["a", "b"] {
        fs::write(fixture.0.join("source").join(group), b"contents").unwrap();
        fs::hard_link(fixture.0.join("source").join(group), fixture.0.join("source").join(format!("{group}-alias"))).unwrap();
    }
    let mut request = fixture.request();
    request.operation = Operation::Copy;
    request.options.jobs = Some(crate::Jobs::new(4).unwrap());
    let gate = Arc::new((Mutex::new(0), Condvar::new()));
    let copies = gate.clone();
    let (outcome, _) = fixture.run(&request, move |point| {
        if point == "during_copy" {
            let (lock, ready) = &*copies;
            let mut entered = lock.lock().unwrap();
            *entered += 1;
            ready.notify_all();
            let (_entered, timed) = ready.wait_timeout_while(entered, Duration::from_secs(2), |count| *count < 2).unwrap();
            if timed.timed_out() { return Err(io::Error::other("unrelated inode transfer blocked behind global hardlink lock")); }
        }
        Ok(())
    });
    assert_eq!(outcome.status(), 0);
    assert_eq!(*gate.0.lock().unwrap(), 2);
    for group in ["a", "b"] {
        assert_eq!(fs::metadata(fixture.0.join("destination").join(group)).unwrap().ino(), fs::metadata(fixture.0.join("destination").join(format!("{group}-alias"))).unwrap().ino());
    }
}

#[test]
fn repair_symlink_merge_skips_and_rename_preserve_transferred_aliases() {
    use std::os::unix::fs::symlink;
    for merge in [false, true] {
        for rename in [false, true] {
            let fixture = Fixture::new();
            fs::create_dir(fixture.0.join("source")).unwrap();
            symlink("dangling", fixture.0.join("source/a")).unwrap();
            for name in ["b", "c"] { fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source").join(name)).unwrap(); }
            let mut request = fixture.request();
            if merge {
                fs::create_dir(fixture.0.join("destination")).unwrap();
                symlink("retained", fixture.0.join("destination/c")).unwrap();
                request.options.merge = true;
                request.options.collision = crate::Collision::Skip;
            } else {
                fs::create_dir(fixture.0.join("destination")).unwrap();
                request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/a".into(), rest: vec!["source/b".into(), "source/c".into()] };
            }
            let (outcome, _) = fixture.run(&request, move |point| { if rename { Ok(()) } else { force_exdev(point) } });
            assert_eq!(outcome.status(), 0, "merge={merge}, rename={rename}");
            assert_eq!(fs::symlink_metadata(fixture.0.join("destination/a")).unwrap().ino(), fs::symlink_metadata(fixture.0.join("destination/b")).unwrap().ino());
            if merge {
                assert_eq!(fs::read_link(fixture.0.join("source/c")).unwrap(), PathBuf::from("dangling"));
                assert_eq!(fs::read_link(fixture.0.join("destination/c")).unwrap(), PathBuf::from("retained"));
                assert!(fixture.0.join("source/a").symlink_metadata().is_err());
                assert!(fixture.0.join("source/b").symlink_metadata().is_err());
            }
        }
    }
}

#[test]
fn repair_mixed_rename_and_transfer_preserve_symlink_group() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("source")).unwrap();
    fs::create_dir(fixture.0.join("destination")).unwrap();
    symlink("dangling", fixture.0.join("source/a")).unwrap();
    for name in ["b", "c"] { fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source").join(name)).unwrap(); }
    let mut request = fixture.request();
    request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/a".into(), rest: vec!["source/b".into(), "source/c".into()] };
    let renames = std::sync::atomic::AtomicUsize::new(0);
    let (outcome, _) = fixture.run(&request, move |point| {
        if point == "rename" && renames.fetch_add(1, Ordering::Relaxed) > 0 { return force_exdev(point); }
        Ok(())
    });
    assert_eq!(outcome.status(), 0);
    let inode = fs::symlink_metadata(fixture.0.join("destination/a")).unwrap().ino();
    for name in ["a", "b", "c"] {
        assert_eq!(fs::symlink_metadata(fixture.0.join("destination").join(name)).unwrap().ino(), inode);
        assert!(fs::symlink_metadata(fixture.0.join("source").join(name)).is_err());
    }
}

#[test]
fn repair_failed_or_cancelled_representative_releases_alias_workers() {
    for cancel in [false, true] {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("source")).unwrap();
        fs::write(fixture.0.join("source/a"), b"contents").unwrap();
        for name in ["b", "c"] { fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source").join(name)).unwrap(); }
        let mut request = fixture.request();
        request.operation = Operation::Copy;
        request.options.jobs = Some(crate::Jobs::new(3).unwrap());
        let cancellation = Arc::new(Cancellation::default());
        let selected = cancellation.clone();
        HOOK.with(|hook| *hook.borrow_mut() = Some(Arc::new(move |point| {
            if point == "during_copy" {
                if cancel { selected.cancel(); } else { return Err(io::Error::other("representative copy failure")); }
            }
            Ok(())
        })));
        let outcome = execute(&request, &cancellation, &|_| Ok(()));
        HOOK.with(|hook| *hook.borrow_mut() = None);
        assert_eq!(outcome.status(), if cancel { 130 } else { 1 });
        for name in ["a", "b", "c"] { assert_eq!(fs::read(fixture.0.join("source").join(name)).unwrap(), b"contents"); }
        assert!(!fixture.0.join("destination").exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }
}

#[test]
fn repair_published_hardlink_alias_mutation_retains_source() {
    use std::os::unix::fs::PermissionsExt;
    for mode in [false, true] {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("source")).unwrap();
        fs::write(fixture.0.join("source/a"), b"original").unwrap();
        fs::set_permissions(fixture.0.join("source/a"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source/b")).unwrap();
        let destination = fixture.0.join("destination/b");
        let (outcome, _) = fixture.run(&fixture.request(), move |point| {
            force_exdev(point)?;
            if point == "after_publish" {
                if mode { fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap(); }
                else { fs::write(&destination, b"tampered").unwrap(); }
            }
            Ok(())
        });
        assert_eq!(outcome.status(), 1);
        for name in ["a", "b"] { assert_eq!(fs::read(fixture.0.join("source").join(name)).unwrap(), b"original"); }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn repair_published_access_metadata_mutation_retains_source() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("source"), b"original").unwrap();
    let source = File::open(fixture.0.join("source")).unwrap();
    if !set_test_xattr(&source, b"user.ish-fs-test", b"original") { return; }
    let destination = fixture.0.join("destination");
    let (outcome, _) = fixture.run(&fixture.request(), move |point| {
        force_exdev(point)?;
        if point == "after_publish" {
            let destination = File::open(&destination).unwrap();
            rustix::fs::fsetxattr(&destination, "user.ish-fs-test", b"changed", rustix::fs::XattrFlags::empty()).unwrap();
        }
        Ok(())
    });
    assert_eq!(outcome.status(), 1);
    assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"original");
}

#[cfg(target_os = "linux")]
#[test]
fn repair_deferred_metadata_validates_successful_output() {
    use rustix::fs::{IFlags, ioctl_getflags, ioctl_setflags};
    struct ResetFlags(Vec<PathBuf>);
    impl Drop for ResetFlags {
        fn drop(&mut self) {
            for path in &self.0 {
                if let Ok(file) = File::open(path) { ioctl_setflags(&file, IFlags::empty()).unwrap(); }
            }
        }
    }
    for mutate in [false, true] {
        let fixture = Fixture::new();
        let source = fixture.0.join("source");
        let destination = fixture.0.join("destination");
        fs::write(&source, b"original").unwrap();
        let file = File::open(&source).unwrap();
        let flags = match ioctl_getflags(&file) {
            Ok(flags) => flags,
            Err(rustix::io::Errno::NOTSUP | rustix::io::Errno::NOTTY) => { eprintln!("SKIP deferred metadata: inode flags unsupported"); return; },
            Err(error) => panic!("capture flags: {error}"),
        };
        match ioctl_setflags(&file, flags | IFlags::IMMUTABLE) {
            Ok(()) => {},
            Err(error) if matches!(error, rustix::io::Errno::PERM | rustix::io::Errno::NOTSUP | rustix::io::Errno::NOTTY) => { eprintln!("SKIP deferred metadata: cannot set immutable flag: {error}"); return; },
            Err(error) => panic!("set immutable flag: {error}"),
        }
        let _reset = ResetFlags(vec![source.clone(), destination.clone()]);
        let target = destination.clone();
        let mut request = fixture.request();
        request.operation = Operation::Copy;
        let (outcome, _) = fixture.run(&request, move |point| {
            if mutate && point == "after_publish" { fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap(); }
            Ok(())
        });
        assert_eq!(outcome.status(), i32::from(mutate));
        if !mutate { assert!(ioctl_getflags(File::open(destination).unwrap()).unwrap().contains(IFlags::IMMUTABLE)); }
        assert_eq!(fs::read(source).unwrap(), b"original");
    }
}

#[test]
fn repair_completed_primitive_mutations_are_not_accepted_as_output() {
    for boundary in ["after_copy", "after_metadata", "during_sync", "destination_file_synced"] {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("source"), b"original").unwrap();
        let base = fixture.0.clone();
        let (outcome, _) = fixture.run(&fixture.request(), move |point| {
            force_exdev(point)?;
            if point == boundary {
                let stage = fs::read_dir(&base).unwrap().map(|entry| entry.unwrap().path())
                    .find(|path| path.file_name().unwrap().as_encoded_bytes().starts_with(b".ish-fs-stage-")).unwrap();
                fs::write(stage, b"tampered").unwrap();
            }
            Ok(())
        });
        assert_eq!(outcome.status(), 1, "{boundary}");
        assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"original");
        assert!(!fixture.0.join("destination").exists());
    }
}

#[test]
fn repair_transferred_group_prevents_later_alias_rename_split() {
    use std::os::unix::fs::symlink;
    for merge in [false, true] {
        for links in [2, 3] {
            for symbolic in [false, true] {
                let fixture = Fixture::new();
                fs::create_dir(fixture.0.join("source")).unwrap();
                fs::create_dir(fixture.0.join("destination")).unwrap();
                if symbolic { symlink("dangling", fixture.0.join("source/a")).unwrap(); }
                else { fs::write(fixture.0.join("source/a"), b"payload").unwrap(); }
                for name in ["b", "c"].into_iter().take(links - 1) {
                    fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source").join(name)).unwrap();
                }
                let mut request = fixture.request();
                if merge { request.options.merge = true; }
                else {
                    request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/a".into(), rest: ["source/b", "source/c"].into_iter().take(links - 1).map(PathBuf::from).collect() };
                }
                let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let renames = attempts.clone();
                let (outcome, _) = fixture.run(&request, move |point| {
                    if point == "rename" && renames.fetch_add(1, Ordering::Relaxed) == 0 { return force_exdev(point); }
                    Ok(())
                });
                assert_eq!(outcome.status(), 0, "merge={merge}, links={links}, symbolic={symbolic}");
                let inode = fs::symlink_metadata(fixture.0.join("destination/a")).unwrap().ino();
                for name in ["a", "b", "c"].into_iter().take(links) {
                    assert_eq!(fs::symlink_metadata(fixture.0.join("destination").join(name)).unwrap().ino(), inode, "merge={merge}, links={links}, symbolic={symbolic}");
                    assert!(fs::symlink_metadata(fixture.0.join("source").join(name)).is_err());
                }
                assert_eq!(attempts.load(Ordering::Relaxed), 1);
                if !symbolic { assert_eq!(outcome.progress.cloned_files + outcome.progress.native_copied_files + outcome.progress.buffered_copied_files, 1); }
            }
        }
    }
}

fn mixed_tree_aliases_keep_one_inode(tree_first: bool) {
    use std::os::unix::fs::symlink;
    for symbolic in [false, true] {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("source/tree")).unwrap();
        fs::create_dir(fixture.0.join("destination")).unwrap();
        if symbolic { symlink("dangling", fixture.0.join("source/tree/member")).unwrap(); }
        else { fs::write(fixture.0.join("source/tree/member"), b"payload").unwrap(); }
        fs::hard_link(fixture.0.join("source/tree/member"), fixture.0.join("source/alias")).unwrap();
        let mut request = fixture.request();
        let (first, second) = if tree_first { ("source/tree", "source/alias") } else { ("source/alias", "source/tree") };
        request.operands = crate::Operands::Into { directory: "destination".into(), first: first.into(), rest: vec![second.into()] };
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let (outcome, _) = fixture.run(&request, move |point| {
            if point == "rename" {
                let attempt = attempts.fetch_add(1, Ordering::Relaxed);
                if (tree_first && attempt > 0) || (!tree_first && attempt == 0) { return force_exdev(point); }
            }
            Ok(())
        });
        assert_eq!(outcome.status(), 0, "tree_first={tree_first}, symbolic={symbolic}");
        let member = fs::symlink_metadata(fixture.0.join("destination/tree/member")).unwrap();
        let alias = fs::symlink_metadata(fixture.0.join("destination/alias")).unwrap();
        assert_eq!(member.ino(), alias.ino(), "tree_first={tree_first}, symbolic={symbolic}");
        assert!(fs::symlink_metadata(fixture.0.join("source/alias")).is_err());
        assert!(fs::symlink_metadata(fixture.0.join("source/tree")).is_err());
    }
}

#[test]
fn repair_renamed_tree_alias_is_reused_for_later_transfer() { mixed_tree_aliases_keep_one_inode(true); }

#[test]
fn repair_copied_alias_is_reused_for_later_directory() { mixed_tree_aliases_keep_one_inode(false); }

#[test]
fn repair_isolated_directory_rename_needs_no_scratch_or_traversal() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("source/nested")).unwrap();
    fs::write(fixture.0.join("source/nested/file"), b"payload").unwrap();
    let mut request = fixture.request();
    request.context.scratch = fixture.0.join("missing-scratch");
    let (outcome, _) = fixture.run(&request, |point| {
        if matches!(point, "before_copy" | "directory_group_scan" | "renamed_tree_index") { return Err(io::Error::other("isolated rename unexpectedly traversed")); }
        Ok(())
    });
    assert_eq!(outcome.status(), 0);
    assert_eq!(fs::read(fixture.0.join("destination/nested/file")).unwrap(), b"payload");
    assert!(!request.context.scratch.exists());
}

#[test]
fn repair_renamed_tree_groups_are_indexed_in_one_pass() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("source/tree")).unwrap();
    fs::create_dir(fixture.0.join("destination")).unwrap();
    let mut rest = Vec::new();
    for index in 0..32 {
        let member = fixture.0.join(format!("source/tree/member-{index}"));
        let alias = PathBuf::from(format!("source/alias-{index}"));
        fs::write(&member, b"payload").unwrap();
        fs::hard_link(&member, fixture.0.join(&alias)).unwrap();
        rest.push(alias);
    }
    for index in 0..32 {
        let unique = PathBuf::from(format!("source/unique-{index}"));
        fs::write(fixture.0.join(&unique), b"unrelated").unwrap();
        rest.push(unique);
    }
    let mut request = fixture.request();
    request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/tree".into(), rest };
    let counts = Arc::new(Mutex::new((0, 0, 0)));
    let visited = counts.clone();
    let (outcome, _) = fixture.run(&request, move |point| {
        let mut counts = visited.lock().unwrap();
        match point {
            "rename" => { counts.0 += 1; if counts.0 > 1 { return force_exdev(point); } },
            "renamed_tree_index" => counts.1 += 1,
            "hardlink_discovery_leaf" => counts.2 += 1,
            _ => {},
        }
        Ok(())
    });
    assert_eq!(outcome.status(), 0);
    let counts = counts.lock().unwrap();
    assert_eq!(counts.1, 1);
    assert_eq!(counts.2, 32);
    for index in 0..32 {
        assert_eq!(fs::metadata(fixture.0.join(format!("destination/tree/member-{index}"))).unwrap().ino(), fs::metadata(fixture.0.join(format!("destination/alias-{index}"))).unwrap().ino());
    }
}

#[test]
fn repair_substituted_renamed_tree_retains_later_source() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("source/tree")).unwrap();
    fs::create_dir(fixture.0.join("destination")).unwrap();
    fs::write(fixture.0.join("source/tree/member"), b"payload").unwrap();
    fs::hard_link(fixture.0.join("source/tree/member"), fixture.0.join("source/alias")).unwrap();
    let mut request = fixture.request();
    request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/tree".into(), rest: vec!["source/alias".into()] };
    let base = fixture.0.clone();
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let (outcome, _) = fixture.run(&request, move |point| {
        if point == "rename" && attempts.fetch_add(1, Ordering::Relaxed) > 0 {
            fs::rename(base.join("destination/tree"), base.join("retained-tree")).unwrap();
            fs::create_dir(base.join("destination/tree")).unwrap();
            return force_exdev(point);
        }
        Ok(())
    });
    assert_eq!(outcome.status(), 1);
    assert_eq!(fs::read(fixture.0.join("source/alias")).unwrap(), b"payload");
    assert_eq!(fs::read(fixture.0.join("retained-tree/member")).unwrap(), b"payload");
    assert!(fs::symlink_metadata(fixture.0.join("destination/alias")).is_err());
}

#[test]
fn repair_failed_renamed_tree_index_does_not_mark_root_complete() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("source/tree")).unwrap();
    fs::create_dir(fixture.0.join("destination")).unwrap();
    for name in ["a", "b"] {
        fs::write(fixture.0.join("source/tree").join(name), b"payload").unwrap();
        fs::hard_link(fixture.0.join("source/tree").join(name), fixture.0.join("source").join(name)).unwrap();
    }
    let mut request = fixture.request();
    request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/tree".into(), rest: vec!["source/a".into(), "source/b".into()] };
    let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = scans.clone();
    let renames = std::sync::atomic::AtomicUsize::new(0);
    let (outcome, _) = fixture.run(&request, move |point| {
        if point == "rename" && renames.fetch_add(1, Ordering::Relaxed) > 0 { return force_exdev(point); }
        if point == "renamed_tree_index" && counted.fetch_add(1, Ordering::Relaxed) == 0 { return Err(io::Error::other("index interrupted")); }
        Ok(())
    });
    assert_eq!(outcome.status(), 1);
    assert_eq!(scans.load(Ordering::Relaxed), 2);
    assert_eq!(fs::read(fixture.0.join("source/a")).unwrap(), b"payload");
    assert_eq!(fs::metadata(fixture.0.join("destination/b")).unwrap().ino(), fs::metadata(fixture.0.join("destination/tree/b")).unwrap().ino());
    assert!(fs::symlink_metadata(fixture.0.join("source/b")).is_err());
}

#[test]
fn repair_reused_renamed_inode_sync_failure_retains_current_source() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("source")).unwrap();
    fs::create_dir(fixture.0.join("destination")).unwrap();
    fs::write(fixture.0.join("source/a"), b"payload").unwrap();
    fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source/b")).unwrap();
    let mut request = fixture.request();
    request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/a".into(), rest: vec!["source/b".into()] };
    let renames = std::sync::atomic::AtomicUsize::new(0);
    let (outcome, _) = fixture.run(&request, move |point| {
        if point == "rename" && renames.fetch_add(1, Ordering::Relaxed) > 0 { return force_exdev(point); }
        if point == "destination_file_sync" { return Err(io::Error::other("reused inode sync failed")); }
        Ok(())
    });
    assert_eq!(outcome.status(), 1);
    assert_eq!(fs::read(fixture.0.join("source/b")).unwrap(), b"payload");
    assert_eq!(fs::read(fixture.0.join("destination/a")).unwrap(), b"payload");
    assert_eq!(fs::metadata(fixture.0.join("source/b")).unwrap().ino(), fs::metadata(fixture.0.join("destination/a")).unwrap().ino());
    assert!(fs::symlink_metadata(fixture.0.join("destination/b")).is_err());
}

#[test]
fn repair_unrecorded_directory_rename_retains_remaining_sources() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("source/tree")).unwrap();
    fs::create_dir(fixture.0.join("destination")).unwrap();
    fs::write(fixture.0.join("source/tree/member"), b"payload").unwrap();
    fs::hard_link(fixture.0.join("source/tree/member"), fixture.0.join("source/alias")).unwrap();
    let mut request = fixture.request();
    request.operands = crate::Operands::Into { directory: "destination".into(), first: "source/tree".into(), rest: vec!["source/alias".into()] };
    let (outcome, events) = fixture.run(&request, |point| {
        if point == "renamed_directory_record" { return Err(io::Error::other("directory rename receipt unavailable")); }
        Ok(())
    });
    assert_eq!(outcome.status(), 1);
    assert_eq!(fs::read(fixture.0.join("destination/tree/member")).unwrap(), b"payload");
    assert_eq!(fs::read(fixture.0.join("source/alias")).unwrap(), b"payload");
    assert!(fs::symlink_metadata(fixture.0.join("destination/alias")).is_err());
    assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::SourcePartiallyRetired)));
}
