use super::*;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex};

#[path = "../../tests/support/mod.rs"]
mod support;

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{PermissionsExt, symlink};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        fs::create_dir_all(&base).unwrap();
        let path = base.join(format!("engine-fault-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&path).unwrap(); Self(path)
    }
    fn request(&self) -> Request {
        Request { operation: Operation::Move, operands: crate::Operands::Exact { source: "source".into(), destination: "destination".into() }, options: crate::Options::default(), context: crate::Context { cwd: self.0.clone(), home: None, scratch: self.0.clone() } }
    }
    fn run(&self, request: &Request, hook: impl Fn(&str) -> io::Result<()> + Send + Sync + 'static) -> (Outcome, Vec<Event>) {
        HOOK.with(|selected| *selected.borrow_mut() = Some(Arc::new(hook)));
        let events = Mutex::new(Vec::new());
        let outcome = execute(request, &Cancellation::default(), &|event| { events.lock().unwrap().push(event); Ok(()) });
        HOOK.with(|selected| *selected.borrow_mut() = None);
        let events = events.into_inner().unwrap();
        for event in &events { if let Event::Diagnostic(diagnostic) = event { eprintln!("{}", diagnostic.message); } }
        (outcome, events)
    }
}
impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }

fn force_exdev(boundary: &str) -> io::Result<()> { if boundary == "rename" { Err(io::Error::from_raw_os_error(libc::EXDEV)) } else { Ok(()) } }

#[cfg(target_os = "linux")]
#[test]
fn dense_destination_allocation_is_reported_without_claiming_sparse_support() {
    for dense_source in [false, true] {
        for force_dense_destination in [false, true] {
            let fixture = Fixture::new();
            let length = 256 * 1024;
            let source = fixture.0.join("source");
            if dense_source {
                fs::write(&source, vec![1; length]).unwrap();
            } else {
                fs::File::create(&source).unwrap().set_len(length as u64).unwrap();
            }
            if !dense_source && fs::metadata(&source).unwrap().blocks() * 512 >= length as u64 {
                eprintln!("SKIP: fixture filesystem does not expose sparse allocation");
                return;
            }
            let source_allocated = fs::metadata(&source).unwrap().blocks() * 512;
            let mut request = fixture.request();
            request.operation = Operation::Copy;
            let (outcome, events) = fixture.run(&request, move |point| {
                if force_dense_destination && point == "during_copy" {
                    crate::platform::linux::force_dense_destination_for_test();
                }
                Ok(())
            });
            assert_eq!(outcome.status(), 0);
            assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), fs::read(source).unwrap());
            let dense_destination = fs::metadata(fixture.0.join("destination")).unwrap().blocks() * 512 >= length as u64;
            let warning = events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("destination reports dense allocation")));
            assert_eq!(warning, source_allocated < length as u64 && dense_destination);
        }
    }
}

#[cfg(target_os = "linux")]
struct CopyStrategyHooks;

#[cfg(target_os = "linux")]
impl CopyStrategyHooks {
    fn select(skip_reflink: bool, fail_copy_range_after: Option<u64>) -> Self {
        crate::platform::linux::skip_reflink_for_test(skip_reflink);
        crate::platform::linux::fail_copy_range_after_for_test(fail_copy_range_after);
        Self
    }
}

#[cfg(target_os = "linux")]
impl Drop for CopyStrategyHooks {
    fn drop(&mut self) {
        crate::platform::linux::skip_reflink_for_test(false);
        crate::platform::linux::fail_copy_range_after_for_test(None);
    }
}

#[cfg(target_os = "linux")]
fn set_test_mtime(path: &std::path::Path, seconds: i64, nanoseconds: i64) {
    let file = File::open(path).unwrap();
    rustix::fs::futimens(
        file.as_fd(),
        &rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec { tv_sec: seconds, tv_nsec: nanoseconds },
            last_modification: rustix::fs::Timespec { tv_sec: seconds, tv_nsec: nanoseconds },
        },
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
fn set_test_xattr(fd: impl AsFd, name: &[u8], value: &[u8]) -> bool {
    match rustix::fs::fsetxattr(fd, OsStr::from_bytes(name), value, rustix::fs::XattrFlags::empty()) {
        Ok(()) => true,
        Err(error) if matches!(error, rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP | rustix::io::Errno::XDEV | rustix::io::Errno::INVAL | rustix::io::Errno::PERM) => {
            eprintln!("Linux metadata fixture unavailable for {}: {error}", String::from_utf8_lossy(name));
            false
        }
        Err(error) => panic!("set Linux metadata fixture {}: {error}", String::from_utf8_lossy(name)),
    }
}

#[cfg(target_os = "linux")]
fn get_test_xattr(path: &std::path::Path, name: &[u8]) -> Vec<u8> {
    let file = File::open(path).unwrap();
    let mut value = vec![0; 4096];
    let length = rustix::fs::fgetxattr(file.as_fd(), OsStr::from_bytes(name), &mut value).unwrap();
    value.truncate(length);
    value
}

#[cfg(target_os = "linux")]
fn test_access_acl() -> Vec<u8> {
    let entries = [
        (0x01_u16, 6_u16, u32::MAX),
        (0x02, 4, 1234),
        (0x04, 4, u32::MAX),
        (0x10, 4, u32::MAX),
        (0x20, 0, u32::MAX),
    ];
    let mut bytes = Vec::with_capacity(4 + entries.len() * 8);
    bytes.extend_from_slice(&2_u32.to_le_bytes());
    for (tag, permission, id) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&permission.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}

#[cfg(target_os = "linux")]
fn path_metadata(path: &std::path::Path) -> CopyMetadata {
    let metadata = fs::metadata(path).unwrap();
    CopyMetadata {
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode() & 0o7777,
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Eq, PartialEq)]
struct CopyMetadata {
    uid: u32,
    gid: u32,
    mode: u32,
    mtime: (i64, i64),
}

#[cfg(target_os = "linux")]
#[derive(Debug, Eq, PartialEq)]
struct CopyStrategySnapshot {
    linked_bytes: Vec<u8>,
    linked_metadata: CopyMetadata,
    readonly_bytes: Vec<u8>,
    readonly_metadata: CopyMetadata,
    linked_names_share_inode: bool,
    linked_nlink: u64,
    raw_symlink_target: Vec<u8>,
    user_xattr: Option<Vec<u8>>,
    acl: Option<Vec<u8>>,
}

#[cfg(target_os = "linux")]
fn strategy_snapshot(fixture: &Fixture, output: &str, preserve_xattr: bool, preserve_acl: bool) -> CopyStrategySnapshot {
    let output = fixture.0.join(output);
    let linked = fs::metadata(output.join("alpha")).unwrap();
    let alias = fs::metadata(output.join("beta")).unwrap();
    CopyStrategySnapshot {
        linked_bytes: fs::read(output.join("alpha")).unwrap(),
        linked_metadata: path_metadata(&output.join("alpha")),
        readonly_bytes: fs::read(output.join("readonly")).unwrap(),
        readonly_metadata: path_metadata(&output.join("readonly")),
        linked_names_share_inode: linked.ino() == alias.ino(),
        linked_nlink: linked.nlink(),
        raw_symlink_target: fs::read_link(output.join("raw-link")).unwrap().as_os_str().as_bytes().to_vec(),
        user_xattr: preserve_xattr.then(|| get_test_xattr(&output.join("alpha"), b"user.ish-fs-engine-parity")),
        acl: preserve_acl.then(|| get_test_xattr(&output.join("alpha"), b"system.posix_acl_access")),
    }
}

#[cfg(target_os = "linux")]
fn execute_copy_strategy(fixture: &Fixture, output: &str, skip_reflink: bool, fail_copy_range_after: Option<u64>) -> (Progress, bool) {
    fs::create_dir(fixture.0.join(output)).unwrap();
    let request = Request {
        operation: Operation::Copy,
        operands: crate::Operands::Into {
            directory: output.into(),
            first: "left/alpha".into(),
            rest: vec!["right/beta".into(), "readonly".into(), "raw-link".into()],
        },
        options: crate::Options { jobs: Some(crate::Jobs::new(1).unwrap()), ..crate::Options::default() },
        context: crate::Context { cwd: fixture.0.clone(), home: None, scratch: fixture.0.clone() },
    };
    let hooks = CopyStrategyHooks::select(skip_reflink, fail_copy_range_after);
    let outcome = execute(&request, &Cancellation::default(), &|_: Event| Ok(()));
    let copy_range_fault_triggered = crate::platform::linux::copy_range_fault_triggered_for_test();
    drop(hooks);
    assert_eq!(outcome.status(), 0);
    (outcome.progress, copy_range_fault_triggered)
}

#[cfg(target_os = "linux")]
fn clone_probe(fixture: &Fixture) -> Result<(), rustix::io::Errno> {
    let source = File::open(fixture.0.join("left/alpha")).unwrap();
    let destination_path = fixture.0.join("clone-probe");
    let destination = OpenOptions::new().read(true).write(true).create_new(true).open(&destination_path).unwrap();
    let result = rustix::fs::ioctl_ficlone(destination.as_fd(), source.as_fd());
    drop(destination);
    fs::remove_file(destination_path).unwrap();
    result
}

#[cfg(target_os = "linux")]
fn copy_range_probe(fixture: &Fixture) -> Result<usize, rustix::io::Errno> {
    let source = File::open(fixture.0.join("left/alpha")).unwrap();
    let destination_path = fixture.0.join("copy-range-probe");
    let destination = OpenOptions::new().read(true).write(true).create_new(true).open(&destination_path).unwrap();
    let mut source_offset = 0;
    let mut destination_offset = 0;
    let result = rustix::fs::copy_file_range(source.as_fd(), Some(&mut source_offset), destination.as_fd(), Some(&mut destination_offset), 1);
    drop(destination);
    fs::remove_file(destination_path).unwrap();
    result
}

#[test]
fn forced_exdev_tree_preserves_then_retires_recorded_objects() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap(); fs::create_dir(fixture.0.join("source/d")).unwrap();
    fs::write(fixture.0.join("source/d/a"), b"a").unwrap(); fs::write(fixture.0.join("source/d/b"), b"b").unwrap();
    let (outcome, _) = fixture.run(&fixture.request(), force_exdev);
    assert_eq!(outcome.status(), 0); assert!(!fixture.0.join("source").exists());
    assert_eq!(fs::read(fixture.0.join("destination/d/a")).unwrap(), b"a"); assert_eq!(fs::read(fixture.0.join("destination/d/b")).unwrap(), b"b");
}

#[test]
fn move_self_induced_hardlink_metadata_change_allowed() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap();
    fs::write(fixture.0.join("source/a"), b"linked").unwrap(); fs::hard_link(fixture.0.join("source/a"), fixture.0.join("source/b")).unwrap(); fs::hard_link(fixture.0.join("source/a"), fixture.0.join("outside")).unwrap();
    let (outcome, _) = fixture.run(&fixture.request(), force_exdev);
    assert_eq!(outcome.status(), 0); assert!(!fixture.0.join("source").exists());
    assert_eq!(fs::metadata(fixture.0.join("destination/a")).unwrap().ino(), fs::metadata(fixture.0.join("destination/b")).unwrap().ino());
    assert_eq!(fs::read(fixture.0.join("outside")).unwrap(), b"linked");
}

#[test]
fn each_transfer_failure_boundary_retains_source() {
    for boundary in ["before_copy", "during_copy", "after_copy", "after_metadata", "destination_file_sync", "during_sync", "before_publish", "after_publish", "after_durable", "before_retire", "before_capture", "after_capture", "before_remove"] {
        let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"complete bytes").unwrap();
        let (outcome, _) = fixture.run(&fixture.request(), move |point| {
            force_exdev(point)?;
            if point == boundary { Err(io::Error::other("boundary fault")) } else { Ok(()) }
        });
        assert_eq!(outcome.status(), 1, "{boundary}");
        assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"complete bytes", "{boundary}");
        let expected_publication = matches!(boundary, "after_publish" | "after_durable" | "before_retire" | "before_capture" | "after_capture" | "before_remove");
        assert_eq!(fixture.0.join("destination").exists(), expected_publication, "{boundary}");
        let entries: Vec<_> = fs::read_dir(&fixture.0).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert!(entries.iter().all(|name| name == "source" || name == "destination"), "{boundary}: {entries:?}");
    }
}

#[test]
fn destination_changed_after_durable_publication_retains_source() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"transferred").unwrap();
    let destination = fixture.0.join("destination");
    let (outcome, _) = fixture.run(&fixture.request(), move |point| {
        force_exdev(point)?;
        if point == "before_retire" { fs::write(&destination, b"replacement").unwrap(); }
        Ok(())
    });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"transferred");
}

#[test]
fn new_source_entry_after_durable_publication_is_retained() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap(); fs::write(fixture.0.join("source/a"), b"copied").unwrap();
    let source = fixture.0.join("source");
    let (outcome, _) = fixture.run(&fixture.request(), move |point| { force_exdev(point)?; if point == "after_durable" { fs::write(source.join("new"), b"retain").unwrap(); } Ok(()) });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("source/new")).unwrap(), b"retain"); assert_eq!(fs::read(fixture.0.join("source/a")).unwrap(), b"copied");
}

#[test]
fn late_target_no_replace_race() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"source").unwrap();
    let destination = fixture.0.join("destination"); let mut request = fixture.request(); request.operation = Operation::Copy;
    let (outcome, _) = fixture.run(&request, move |point| { if point == "before_publish" { fs::write(&destination, b"late target").unwrap(); } Ok(()) });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"late target");
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
}

#[test]
fn staging_name_substitution_retains_ambiguous_object() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"source").unwrap();
    let directory = fixture.0.clone(); let mut request = fixture.request(); request.operation = Operation::Copy;
    let substituted = Arc::new(Mutex::new(None::<PathBuf>)); let captured = substituted.clone();
    let (outcome, events) = fixture.run(&request, move |point| {
        if point == "before_publish" {
            let stage = fs::read_dir(&directory).unwrap().map(|entry| entry.unwrap().path()).find(|entry| entry.file_name().unwrap().as_encoded_bytes().starts_with(b".ish-fs-stage-")).unwrap();
            fs::rename(&stage, directory.join("owned-retained")).unwrap(); fs::write(&stage, b"unowned sentinel").unwrap(); *captured.lock().unwrap() = Some(stage);
        }
        Ok(())
    });
    assert_eq!(outcome.status(), 1); assert!(!fixture.0.join("destination").exists());
    assert_eq!(fs::read(substituted.lock().unwrap().as_ref().unwrap()).unwrap(), b"unowned sentinel");
    assert_eq!(fs::read(fixture.0.join("owned-retained")).unwrap(), b"source");
    assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("staging cleanup incomplete"))));
}

#[test]
fn forced_exdev_merge_subtrees_and_skips_retire_only_transferred_entries() {
    let fixture = Fixture::new(); fs::create_dir_all(fixture.0.join("source/tree/deep")).unwrap(); fs::create_dir(fixture.0.join("destination")).unwrap();
    fs::write(fixture.0.join("source/tree/deep/new"), b"new").unwrap(); fs::write(fixture.0.join("source/keep"), b"source keep").unwrap(); fs::write(fixture.0.join("destination/keep"), b"destination keep").unwrap();
    let mut request = fixture.request(); request.options.merge = true; request.options.collision = crate::Collision::Skip;
    let (outcome, _) = fixture.run(&request, force_exdev);
    assert_eq!(outcome.status(), 0); assert_eq!(fs::read(fixture.0.join("source/keep")).unwrap(), b"source keep"); assert!(!fixture.0.join("source/tree").exists());
    assert_eq!(fs::read(fixture.0.join("destination/tree/deep/new")).unwrap(), b"new"); assert_eq!(fs::read(fixture.0.join("destination/keep")).unwrap(), b"destination keep");
}

#[test]
fn forced_exdev_hardlinks_across_two_and_three_operands() {
    for names in [vec!["a", "b"], vec!["a", "b", "c"]] {
        let fixture = Fixture::new(); fs::write(fixture.0.join("a"), b"linked invocation").unwrap();
        for name in &names[1..] { fs::hard_link(fixture.0.join("a"), fixture.0.join(name)).unwrap(); }
        fs::create_dir(fixture.0.join("out")).unwrap();
        let mut request = fixture.request(); request.operands = crate::Operands::Into { directory: "out".into(), first: names[0].into(), rest: names[1..].iter().map(PathBuf::from).collect() };
        let (outcome, _) = fixture.run(&request, force_exdev); assert_eq!(outcome.status(), 0);
        let identity = fs::metadata(fixture.0.join("out/a")).unwrap().ino();
        for name in &names { assert!(!fixture.0.join(name).exists()); assert_eq!(fs::metadata(fixture.0.join("out").join(name)).unwrap().ino(), identity); }
    }
}

#[test]
fn changed_hardlink_representative_is_not_reused() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("a"), b"source bytes").unwrap(); fs::hard_link(fixture.0.join("a"), fixture.0.join("b")).unwrap(); fs::create_dir(fixture.0.join("out")).unwrap();
    let mut request = fixture.request(); request.operation = Operation::Copy; request.operands = crate::Operands::Into { directory: "out".into(), first: "a".into(), rest: vec!["b".into()] };
    let outcome = execute(&request, &Cancellation::default(), &|event| {
        if let Event::Operand(operand) = event && operand.source == std::path::Path::new("a") { fs::write(fixture.0.join("out/a"), b"changed data").unwrap(); }
        Ok(())
    });
    assert_eq!(outcome.status(), 1); assert!(!fixture.0.join("out/b").exists()); assert_eq!(fs::read(fixture.0.join("b")).unwrap(), b"source bytes");
}

#[test]
fn deep_hardlinks_across_different_branches_use_component_anchors() {
    use rustix::fd::AsFd;
    use rustix::fs::{self as unix, Mode, OFlags};
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap();
    let source = unix::open(fixture.0.join("source"), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    let long = "x".repeat(200);
    let mut deep = unix::openat(&source, ".", anchored::DIRECTORY, Mode::empty()).unwrap();
    for _ in 0..22 { unix::mkdirat(&deep, long.as_str(), Mode::RUSR | Mode::WUSR | Mode::XUSR).unwrap(); deep = unix::openat(&deep, long.as_str(), anchored::DIRECTORY, Mode::empty()).unwrap(); }
    unix::mkdirat(&deep, "left", Mode::RUSR | Mode::WUSR | Mode::XUSR).unwrap(); unix::mkdirat(&deep, "right", Mode::RUSR | Mode::WUSR | Mode::XUSR).unwrap();
    let left = unix::openat(&deep, "left", anchored::DIRECTORY, Mode::empty()).unwrap(); let right = unix::openat(&deep, "right", anchored::DIRECTORY, Mode::empty()).unwrap();
    let file = unix::openat(&left, "a", OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR).unwrap(); rustix::io::write(&file, b"deep link").unwrap();
    unix::linkat(&left, "a", &right, "b", unix::AtFlags::empty()).unwrap();
    let mut request = fixture.request(); request.operation = Operation::Copy;
    let (outcome, _) = fixture.run(&request, |_| Ok(())); assert_eq!(outcome.status(), 0);
    let mut copied = unix::open(fixture.0.join("destination"), anchored::DIRECTORY, Mode::empty()).unwrap();
    for _ in 0..22 { copied = unix::openat(&copied, long.as_str(), anchored::DIRECTORY, Mode::empty()).unwrap(); }
    let left = unix::openat(&copied, "left", anchored::DIRECTORY, Mode::empty()).unwrap(); let right = unix::openat(&copied, "right", anchored::DIRECTORY, Mode::empty()).unwrap();
    assert_eq!(roots::observe(left.as_fd(), std::ffi::OsStr::new("a")).unwrap().identity, roots::observe(right.as_fd(), std::ffi::OsStr::new("b")).unwrap().identity);
}

#[test]
fn rename_non_exdev_error_does_not_fallback() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"source").unwrap();
    let traced = Arc::new(Mutex::new(Vec::new())); let trace = traced.clone();
    let mut request = fixture.request(); request.context.scratch = fixture.0.join("absent");
    let (outcome, _) = fixture.run(&request, move |point| { trace.lock().unwrap().push(point.to_owned()); if point == "rename" { Err(io::Error::from_raw_os_error(libc::EACCES)) } else { Ok(()) } });
    assert_eq!(outcome.status(), 1); assert_eq!(*traced.lock().unwrap(), ["rename"]); assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"source"); assert!(!fixture.0.join("destination").exists());
}

#[test]
fn destination_sync_completes_before_publication_and_source_retirement() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"complete").unwrap();
    let traced = Arc::new(Mutex::new(Vec::new())); let trace = traced.clone();
    let (outcome, _) = fixture.run(&fixture.request(), move |point| { trace.lock().unwrap().push(point.to_owned()); force_exdev(point) });
    assert_eq!(outcome.status(), 0);
    let trace = traced.lock().unwrap();
    let position = |point: &str| trace.iter().position(|entry| entry == point).unwrap();
    assert!(position("after_copy") < position("after_metadata"));
    assert!(position("after_metadata") < position("destination_file_sync"));
    assert!(position("destination_file_synced") < position("before_publish"));
    assert!(position("after_publish") < position("destination_directory_sync"));
    assert!(position("destination_directory_synced") < position("after_durable"));
    assert!(position("after_durable") < position("before_capture"));
    assert!(position("after_capture") < position("before_remove"));
    assert!(position("after_remove") < position("source_parent_sync"));
    assert!(position("source_parent_sync") < position("source_parent_synced"));
}

#[test]
fn source_removed_sync_failure_is_explicit() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"complete").unwrap();
    let (outcome, events) = fixture.run(&fixture.request(), |point| { force_exdev(point)?; if point == "source_parent_sync" { Err(io::Error::other("source sync failed")) } else { Ok(()) } });
    assert_eq!(outcome.status(), 1); assert!(!fixture.0.join("source").exists()); assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"complete");
    assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::SourceRemovedSyncFailed)));
}

#[test]
fn capture_restoration_conflict_retains_inspectable_record_and_both_source_names() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap(); fs::write(fixture.0.join("source/file"), b"transferred").unwrap();
    let original = fixture.0.join("source/file");
    let (outcome, events) = fixture.run(&fixture.request(), move |point| { force_exdev(point)?; if point == "after_capture" { fs::write(&original, b"new untransferred").unwrap(); return Err(io::Error::other("capture interruption")); } Ok(()) });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("source/file")).unwrap(), b"new untransferred"); assert_eq!(fs::read(fixture.0.join("destination/file")).unwrap(), b"transferred");
    let captures: Vec<_> = fs::read_dir(fixture.0.join("source")).unwrap().map(|entry| entry.unwrap().path()).filter(|path| path.file_name().unwrap().as_encoded_bytes().starts_with(b".ish-fs-retired-")).collect();
    let intents: Vec<_> = fs::read_dir(fixture.0.join("source")).unwrap().map(|entry| entry.unwrap().path()).filter(|path| path.file_name().unwrap().as_encoded_bytes().starts_with(b".ish-fs-intent-")).collect();
    assert_eq!(captures.len(), 1); assert_eq!(intents.len(), 1); assert_eq!(fs::read(&captures[0]).unwrap(), b"transferred");
    let parent = rustix::fs::open(fixture.0.join("source"), anchored::DIRECTORY, rustix::fs::Mode::empty()).unwrap();
    use rustix::fd::AsFd;
    let name = intents[0].file_name().unwrap(); let identity = roots::observe(parent.as_fd(), name).unwrap().identity;
    let record = records::validate_intent(parent.as_fd(), name, identity).unwrap(); assert_eq!(record.source, std::path::Path::new("file"));
    assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("source/.ish-fs-retired-") && diagnostic.message.contains("source/.ish-fs-intent-"))));
}

#[test]
fn cancellation_at_transfer_and_retirement_boundaries_retains_source() {
    for boundary in ["before_copy", "after_copy", "after_publish", "before_retire", "after_capture"] {
        let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"retained").unwrap();
        let cancellation = Cancellation::default(); let controller = cancellation.clone();
        HOOK.with(|selected| *selected.borrow_mut() = Some(Arc::new(move |point| { force_exdev(point)?; if point == boundary { controller.cancel(); } Ok(()) })));
        let outcome = execute(&fixture.request(), &cancellation, &|_: Event| Ok(())); HOOK.with(|selected| *selected.borrow_mut() = None);
        assert_eq!(outcome.status(), 130, "{boundary}"); assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"retained", "{boundary}");
        assert!(fs::read_dir(&fixture.0).unwrap().all(|entry| matches!(entry.unwrap().file_name().to_str(), Some("source" | "destination"))), "{boundary}");
    }
}

#[test]
fn worker_panic_stops_discovery_joins_workers_and_cleans_owned_staging() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap();
    for index in 0..512 { fs::write(fixture.0.join(format!("source/{index}")), b"bytes").unwrap(); }
    let mut request = fixture.request(); request.operation = Operation::Copy;
    let (outcome, _) = fixture.run(&request, |point| { if point == "before_copy" { panic!("deterministic worker failure"); } Ok(()) });
    assert_eq!(outcome.status(), 1); assert!(!fixture.0.join("destination").exists()); assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
}

#[test]
fn record_version_length_identity_owner_validation() {
    use rustix::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"source").unwrap();
    let parent = rustix::fs::open(&fixture.0, anchored::DIRECTORY, rustix::fs::Mode::empty()).unwrap();
    let observed = roots::observe(parent.as_fd(), std::ffi::OsStr::new("source")).unwrap();
    let record = Record { source: "source".into(), destination: "captured".into(), observed, published: observed.identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 };
    let identity = records::write_intent(parent.as_fd(), std::ffi::OsStr::new("intent"), &record).unwrap();
    assert_eq!(records::validate_intent(parent.as_fd(), std::ffi::OsStr::new("intent"), identity).unwrap(), record);
    let valid = fs::read(fixture.0.join("intent")).unwrap();
    for offset in [7, 8, 12, 29] {
        let mut bytes = valid.clone(); bytes[offset] ^= 0xff; fs::write(fixture.0.join("intent"), bytes).unwrap();
        assert!(records::validate_intent(parent.as_fd(), std::ffi::OsStr::new("intent"), identity).is_err(), "offset {offset}");
    }
    fs::write(fixture.0.join("intent"), &valid).unwrap();
    let mut different = identity; different.inode += 1;
    assert!(records::validate_intent(parent.as_fd(), std::ffi::OsStr::new("intent"), different).is_err());
    fs::set_permissions(fixture.0.join("intent"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(records::validate_intent(parent.as_fd(), std::ffi::OsStr::new("intent"), identity).is_err());
    fs::set_permissions(fixture.0.join("intent"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(fixture.0.join("intent"), fixture.0.join("alias")).unwrap();
    assert!(records::validate_intent(parent.as_fd(), std::ffi::OsStr::new("intent"), identity).is_err());
}

#[test]
fn spool_streams_record_payload_larger_than_memory_budget() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"source").unwrap();
    use rustix::fd::AsFd;
    let parent = rustix::fs::open(&fixture.0, anchored::DIRECTORY, rustix::fs::Mode::empty()).unwrap();
    let observed = roots::observe(parent.as_fd(), std::ffi::OsStr::new("source")).unwrap();
    let long = "x".repeat(32 * 1024);
    let record = Record { source: PathBuf::from(&long), destination: PathBuf::from(&long), observed, published: observed.identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 };
    let mut spool = records::Records::new(&fixture.0).unwrap();
    for _ in 0..1024 { spool.push(&record).unwrap(); }
    assert!(spool.end() > crate::RECORD_MEMORY_BYTES as u64);
    let mut cursor = spool.end(); let mut count = 0;
    while let Some(found) = spool.previous(&mut cursor).unwrap() { assert_eq!(found, record); count += 1; }
    assert_eq!(count, 1024); assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
}

#[cfg(target_os = "linux")]
#[test]
fn constrained_descriptor_budget_rejects_transfer_before_staging() {
    if std::env::var_os("ISH_FS_FD_MOVE_CHILD").is_some() {
        let fixture = Fixture::new(); fs::create_dir_all(fixture.0.join("source/deep/tree")).unwrap(); fs::write(fixture.0.join("source/deep/tree/a"), b"linked").unwrap(); fs::hard_link(fixture.0.join("source/deep/tree/a"), fixture.0.join("source/deep/tree/b")).unwrap();
        let original_limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        rustix::process::setrlimit(rustix::process::Resource::Nofile, rustix::process::Rlimit { current: Some(48), maximum: original_limit.maximum }).unwrap();
        let mut request = fixture.request(); request.options.jobs = Some(crate::Jobs::new(1).unwrap());
        let (outcome, _) = fixture.run(&request, force_exdev);
        assert_eq!(outcome.status(), 1); assert!(!fixture.0.join("destination").exists()); assert_eq!(fs::read(fixture.0.join("source/deep/tree/a")).unwrap(), b"linked"); assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
        rustix::process::setrlimit(rustix::process::Resource::Nofile, rustix::process::Rlimit { current: Some(64), maximum: original_limit.maximum }).unwrap();
        let (outcome, _) = fixture.run(&request, force_exdev);
        assert_eq!(outcome.status(), 0); assert!(!fixture.0.join("source").exists());
        assert_eq!(fs::metadata(fixture.0.join("destination/deep/tree/a")).unwrap().ino(), fs::metadata(fixture.0.join("destination/deep/tree/b")).unwrap().ino());
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "engine::tests::constrained_descriptor_budget_rejects_transfer_before_staging", "--nocapture"]).env("ISH_FS_FD_MOVE_CHILD", "1").output().unwrap();
    assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
fn saturated_discovery_cancellation_and_broken_output_join_workers() {
    for broken_output in [false, true] {
        let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap();
        for index in 0..512 { fs::write(fixture.0.join(format!("source/{index}")), b"source").unwrap(); }
        let mut request = fixture.request(); request.operation = Operation::Copy; request.options.jobs = Some(crate::Jobs::new(4).unwrap());
        let cancellation = Cancellation::default(); let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new((Mutex::new(false), std::sync::Condvar::new())); let worker_barrier = barrier.clone(); let worker_active = active.clone();
        HOOK.with(|selected| *selected.borrow_mut() = Some(Arc::new(move |point| {
            if point == "before_copy" {
                worker_active.fetch_add(1, Ordering::AcqRel);
                let (lock, ready) = &*worker_barrier;
                let guard = lock.lock().unwrap(); drop(ready.wait_while(guard, |released| !*released).unwrap());
                worker_active.fetch_sub(1, Ordering::AcqRel);
            }
            Ok(())
        })));
        let outcome = execute(&request, &cancellation, &|event| {
            if matches!(event, Event::Progress(progress) if progress.discovered_files >= 262) {
                if broken_output { cancellation.cancel_for_output(); } else { cancellation.cancel(); }
                let (lock, ready) = &*barrier; *lock.lock().unwrap() = true; ready.notify_all();
                if broken_output { return Err(io::Error::from(io::ErrorKind::BrokenPipe)); }
            }
            Ok(())
        });
        HOOK.with(|selected| *selected.borrow_mut() = None);
        assert_eq!(outcome.status(), if broken_output { 1 } else { 130 }); assert_eq!(active.load(Ordering::Acquire), 0);
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1); assert_eq!(fs::read_dir(fixture.0.join("source")).unwrap().count(), 512);
    }
}

#[test]
fn cross_device_special_object_is_retained_without_opening() {
    let fixture = Fixture::new(); support::make_fifo(&fixture.0.join("source")).unwrap();
    let identity = fs::symlink_metadata(fixture.0.join("source")).unwrap().ino();
    let (outcome, _) = fixture.run(&fixture.request(), force_exdev);
    assert_eq!(outcome.status(), 1); assert_eq!(fs::symlink_metadata(fixture.0.join("source")).unwrap().ino(), identity); assert!(!fixture.0.join("destination").exists());
}

#[test]
fn same_size_same_mtime_source_change_after_transfer_is_retained() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"original").unwrap();
    let source = fixture.0.join("source"); let modified = fs::metadata(&source).unwrap().modified().unwrap();
    let (outcome, _) = fixture.run(&fixture.request(), move |point| {
        force_exdev(point)?;
        if point == "after_durable" {
            fs::write(&source, b"modified").unwrap(); fs::File::options().write(true).open(&source).unwrap().set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
        }
        Ok(())
    });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"modified"); assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"original");
}

#[test]
fn partial_retirement_reports_completed_destination_and_retained_source() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap();
    for name in ["a", "b", "c"] { fs::write(fixture.0.join("source").join(name), name.as_bytes()).unwrap(); }
    let removals = Arc::new(std::sync::atomic::AtomicUsize::new(0)); let count = removals.clone();
    let (outcome, events) = fixture.run(&fixture.request(), move |point| {
        force_exdev(point)?;
        if point == "before_remove" && count.fetch_add(1, Ordering::AcqRel) == 1 { return Err(io::Error::other("second retirement interrupted")); }
        Ok(())
    });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read_dir(fixture.0.join("source")).unwrap().count(), 2);
    for name in ["a", "b", "c"] { assert_eq!(fs::read(fixture.0.join("destination").join(name)).unwrap(), name.as_bytes()); }
    assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::SourcePartiallyRetired)));
}

#[test]
fn replacement_revalidation_retains_late_changed_destination() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"source").unwrap(); fs::write(fixture.0.join("destination"), b"old").unwrap();
    let destination = fixture.0.join("destination"); let retained = fixture.0.join("retained");
    let mut request = fixture.request(); request.operation = Operation::Copy; request.options.collision = crate::Collision::Replace;
    let (outcome, _) = fixture.run(&request, move |point| { if point == "before_publish" { fs::rename(&destination, &retained).unwrap(); fs::write(&destination, b"late existing").unwrap(); } Ok(()) });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"late existing"); assert_eq!(fs::read(fixture.0.join("retained")).unwrap(), b"old"); assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 3);
}

#[test]
fn rename_replacement_restores_late_swapped_destination() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"source").unwrap(); fs::write(fixture.0.join("destination"), b"old").unwrap();
    let destination = fixture.0.join("destination"); let retained = fixture.0.join("retained");
    let mut request = fixture.request(); request.options.collision = crate::Collision::Replace;
    let (outcome, _) = fixture.run(&request, move |point| { if point == "before_replace" { fs::rename(&destination, &retained).unwrap(); fs::write(&destination, b"late existing").unwrap(); } Ok(()) });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"late existing"); assert_eq!(fs::read(fixture.0.join("retained")).unwrap(), b"old"); assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"source"); assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 3);
}

#[test]
fn copy_replacement_restores_late_swapped_destination() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"source").unwrap(); fs::write(fixture.0.join("destination"), b"old").unwrap();
    let destination = fixture.0.join("destination"); let retained = fixture.0.join("retained");
    let mut request = fixture.request(); request.operation = Operation::Copy; request.options.collision = crate::Collision::Replace;
    let (outcome, _) = fixture.run(&request, move |point| { if point == "before_replace" { fs::rename(&destination, &retained).unwrap(); fs::write(&destination, b"late existing").unwrap(); } Ok(()) });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"late existing"); assert_eq!(fs::read(fixture.0.join("retained")).unwrap(), b"old"); assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 3);
}

#[test]
fn cancellation_emits_terminal_outcomes_for_all_unstarted_operands() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("out")).unwrap();
    for name in ["a", "b", "c"] { fs::write(fixture.0.join(name), name.as_bytes()).unwrap(); }
    let mut request = fixture.request(); request.operation = Operation::Copy; request.operands = crate::Operands::Into { directory: "out".into(), first: "a".into(), rest: vec!["b".into(), "c".into()] };
    let cancellation = Cancellation::default(); let outcomes = Mutex::new(Vec::new());
    let outcome = execute(&request, &cancellation, &|event| { if let Event::Operand(operand) = event { outcomes.lock().unwrap().push(operand); cancellation.cancel(); } Ok(()) });
    assert_eq!(outcome.status(), 130); assert_eq!(outcome.progress.failed_files, 0);
    let outcomes = outcomes.into_inner().unwrap(); assert_eq!(outcomes.len(), 3); assert_eq!(outcomes[0].state, OperandState::Copied); assert_eq!(outcomes[1].state, OperandState::Cancelled); assert_eq!(outcomes[2].state, OperandState::Cancelled);
    assert!(!fixture.0.join("out/b").exists()); assert!(!fixture.0.join("out/c").exists());
}

#[test]
fn failed_copy_merge_reports_partial_destination_publication() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap(); fs::create_dir(fixture.0.join("destination")).unwrap();
    fs::write(fixture.0.join("source/file"), b"published child").unwrap();
    let mut request = fixture.request(); request.operation = Operation::Copy; request.options.merge = true;
    let (outcome, events) = fixture.run(&request, |point| { if point == "after_publish" { Err(io::Error::other("first child published before error")) } else { Ok(()) } });
    assert_eq!(outcome.status(), 1); assert_eq!(fs::read(fixture.0.join("destination/file")).unwrap(), b"published child"); assert_eq!(fs::read(fixture.0.join("source/file")).unwrap(), b"published child");
    assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::DestinationPartiallyPublished)));
    assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("source/file") && diagnostic.message.contains("destination/file"))));
}

#[test]
fn child_source_sync_failure_reports_retained_root_as_partial_retirement() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap(); fs::write(fixture.0.join("source/a"), b"a").unwrap(); fs::write(fixture.0.join("source/b"), b"b").unwrap();
    let (outcome, events) = fixture.run(&fixture.request(), |point| { force_exdev(point)?; if point == "source_parent_sync" { Err(io::Error::other("child parent sync failed")) } else { Ok(()) } });
    assert_eq!(outcome.status(), 1); assert!(fixture.0.join("source").is_dir()); assert_eq!(fs::read_dir(fixture.0.join("source")).unwrap().count(), 1);
    assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::SourcePartiallyRetired)));
    assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("source entry \"source/") && diagnostic.message.contains("parent synchronization failed at \"source\""))));
}

#[test]
fn observed_mutation_immediately_before_removal_retains_source() {
    for mutate_capture in [false, true] {
        let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"copied").unwrap(); let directory = fixture.0.clone();
        let (outcome, _) = fixture.run(&fixture.request(), move |point| {
            force_exdev(point)?;
            if point == "before_remove" {
                let target = if mutate_capture { fs::read_dir(&directory).unwrap().map(|entry| entry.unwrap().path()).find(|path| path.file_name().unwrap().as_encoded_bytes().starts_with(b".ish-fs-retired-")).unwrap() } else { directory.join("destination") };
                fs::write(target, b"edited").unwrap();
            }
            Ok(())
        });
        assert_eq!(outcome.status(), 1);
        assert_eq!(fs::read(fixture.0.join("source")).unwrap(), if mutate_capture { b"edited" } else { b"copied" });
        assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), if mutate_capture { b"copied" } else { b"edited" });
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
    }
}

#[test]
fn cancellation_after_restrictive_directory_metadata_cleans_owned_stage() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(); fs::create_dir_all(fixture.0.join("source/d")).unwrap(); fs::write(fixture.0.join("source/d/file"), b"retained").unwrap(); fs::set_permissions(fixture.0.join("source/d"), fs::Permissions::from_mode(0o555)).unwrap();
    let mut request = fixture.request(); request.operation = Operation::Copy; let cancellation = Cancellation::default();
    let outcome = execute(&request, &cancellation, &|event| { if matches!(event, Event::Progress(progress) if progress.completed_files == 2) { cancellation.cancel(); } Ok(()) });
    let entries = fs::read_dir(&fixture.0).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>();
    for entry in &entries {
        if entry.as_encoded_bytes().starts_with(b".ish-fs-stage-") { fs::set_permissions(fixture.0.join(entry).join("d"), fs::Permissions::from_mode(0o755)).unwrap(); }
    }
    let source_mode = fs::metadata(fixture.0.join("source/d")).unwrap().mode() & 0o777;
    fs::set_permissions(fixture.0.join("source/d"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(outcome.status(), 130); assert_eq!(source_mode, 0o555); assert_eq!(entries, [std::ffi::OsString::from("source")]); assert_eq!(fs::read(fixture.0.join("source/d/file")).unwrap(), b"retained");
}

#[cfg(target_os = "linux")]
fn native_copy_capability(fixture: &Fixture) -> Result<(), String> {
    let source = File::open(fixture.0.join("left/alpha")).unwrap();
    rustix::fs::seek(source.as_fd(), rustix::fs::SeekFrom::Data(0)).map_err(|error| format!("SEEK_DATA returned {error}"))?;
    rustix::fs::seek(source.as_fd(), rustix::fs::SeekFrom::Hole(0)).map_err(|error| format!("SEEK_HOLE returned {error}"))?;
    match copy_range_probe(fixture) {
        Ok(1) => Ok(()),
        Ok(count) => Err(format!("one-byte copy_file_range probe returned {count}")),
        Err(error) if matches!(error, rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP | rustix::io::Errno::XDEV | rustix::io::Errno::INVAL) => Err(format!("copy_file_range returned {error}")),
        Err(error) => panic!("copy_file_range capability probe failed unexpectedly: {error}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn public_into_copy_methods_preserve_identical_bytes_metadata_and_hardlinks() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("left")).unwrap();
    fs::create_dir_all(fixture.0.join("right")).unwrap();
    let linked_bytes = (0..512 * 1024 + 37).map(|index| ((index * 31 + 7) % 251) as u8).collect::<Vec<_>>();
    let readonly_bytes = (0..768 * 1024 + 91).map(|index| ((index * 17 + 19) % 251) as u8).collect::<Vec<_>>();
    let linked_path = fixture.0.join("left/alpha");
    let readonly_path = fixture.0.join("readonly");
    fs::write(&linked_path, &linked_bytes).unwrap();
    fs::set_permissions(&linked_path, fs::Permissions::from_mode(0o640)).unwrap();
    let linked_file = OpenOptions::new().read(true).write(true).open(&linked_path).unwrap();
    let user_xattr = set_test_xattr(linked_file.as_fd(), b"user.ish-fs-engine-parity", b"parity xattr");
    let acl = test_access_acl();
    let acl_set = match rustix::fs::fsetxattr(linked_file.as_fd(), OsStr::from_bytes(b"system.posix_acl_access"), &acl, rustix::fs::XattrFlags::empty()) {
        Ok(()) => true,
        Err(error) if matches!(error, rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP | rustix::io::Errno::XDEV | rustix::io::Errno::INVAL | rustix::io::Errno::PERM) => {
            eprintln!("Linux ACL fixture unavailable: {error}");
            false
        }
        Err(error) => panic!("set Linux ACL fixture: {error}"),
    };
    set_test_mtime(&linked_path, 1_700_001_100, 123_000_000);
    drop(linked_file);
    fs::hard_link(&linked_path, fixture.0.join("right/beta")).unwrap();
    fs::write(&readonly_path, &readonly_bytes).unwrap();
    set_test_mtime(&readonly_path, 1_700_001_200, 456_000_000);
    fs::set_permissions(&readonly_path, fs::Permissions::from_mode(0o444)).unwrap();
    let raw_target = OsStr::from_bytes(b"target-\xff\n");
    symlink(raw_target, fixture.0.join("raw-link")).unwrap();
    let source_linked_metadata = path_metadata(&linked_path);
    let source_readonly_metadata = path_metadata(&readonly_path);
    assert_eq!(source_linked_metadata.mode, 0o640);
    assert_eq!(source_readonly_metadata.mode, 0o444);
    let source_xattr = user_xattr.then(|| get_test_xattr(&linked_path, b"user.ish-fs-engine-parity"));
    let source_acl = acl_set.then(|| get_test_xattr(&linked_path, b"system.posix_acl_access"));
    let expected_streamed_bytes = (linked_bytes.len() + readonly_bytes.len()) as u64;

    let (automatic_progress, _) = execute_copy_strategy(&fixture, "out-auto", false, None);
    assert_eq!(automatic_progress.completed_files, 4);
    assert_eq!(automatic_progress.cloned_files + automatic_progress.native_copied_files + automatic_progress.buffered_copied_files, 2);
    assert_eq!(automatic_progress.logical_bytes_cloned + automatic_progress.bytes_streamed, expected_streamed_bytes);
    if automatic_progress.cloned_files == 0 {
        match clone_probe(&fixture) {
            Err(error) if matches!(error, rustix::io::Errno::XDEV | rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP | rustix::io::Errno::NOTTY | rustix::io::Errno::INVAL) => {
                eprintln!("SKIP reflink method proof: FICLONE probe returned {error}");
            }
            Ok(()) => panic!("automatic public copy selected no clones although FICLONE succeeded on the fixture filesystem"),
            Err(error) => panic!("FICLONE capability probe failed unexpectedly: {error}"),
        }
    }

    let (native_progress, _) = execute_copy_strategy(&fixture, "out-native", true, None);
    assert_eq!(native_progress.completed_files, 4);
    assert_eq!(native_progress.cloned_files, 0);
    assert_eq!(native_progress.native_copied_files + native_progress.buffered_copied_files, 2);
    assert_eq!(native_progress.logical_bytes_cloned + native_progress.bytes_streamed, expected_streamed_bytes);
    if native_progress.native_copied_files == 0 {
        match native_copy_capability(&fixture) {
            Err(reason) => eprintln!("SKIP native copy_file_range method proof: {reason}"),
            Ok(()) => panic!("public copy selected no native files although sparse seeking and copy_file_range work on the fixture filesystem"),
        }
    }

    let (buffered_progress, copy_range_fault_triggered) = execute_copy_strategy(&fixture, "out-buffered", true, Some(0));
    assert_eq!(buffered_progress.completed_files, 4);
    assert_eq!(buffered_progress.cloned_files, 0);
    assert_eq!(buffered_progress.native_copied_files, 0);
    assert_eq!(buffered_progress.buffered_copied_files, 2);
    assert_eq!(buffered_progress.logical_bytes_cloned, 0);
    assert_eq!(buffered_progress.bytes_streamed, expected_streamed_bytes);
    if !copy_range_fault_triggered {
        match native_copy_capability(&fixture) {
            Err(reason) => eprintln!("SKIP forced copy_file_range fallback proof: {reason}; buffered exact-byte path was selected"),
            Ok(()) => panic!("copy_file_range test hook was not reached although native copy capabilities work on the fixture filesystem"),
        }
    }

    let snapshots = [
        strategy_snapshot(&fixture, "out-auto", user_xattr, acl_set),
        strategy_snapshot(&fixture, "out-native", user_xattr, acl_set),
        strategy_snapshot(&fixture, "out-buffered", user_xattr, acl_set),
    ];
    assert_eq!(snapshots[0], snapshots[1]);
    assert_eq!(snapshots[1], snapshots[2]);
    for snapshot in &snapshots {
        assert_eq!(snapshot.linked_bytes, linked_bytes);
        assert_eq!(snapshot.linked_metadata, source_linked_metadata);
        assert_eq!(snapshot.readonly_bytes, readonly_bytes);
        assert_eq!(snapshot.readonly_metadata, source_readonly_metadata);
        assert!(snapshot.linked_names_share_inode);
        assert_eq!(snapshot.linked_nlink, 2);
        assert_eq!(snapshot.raw_symlink_target, raw_target.as_bytes());
        assert_eq!(snapshot.user_xattr, source_xattr);
        assert_eq!(snapshot.acl, source_acl);
    }
}

#[path = "repair_tests.rs"]
mod repair_tests;
