#![cfg(target_os = "linux")]

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ish_fs::{Cancellation, Completion, Context, Event, OperandState, Operands, Operation, Options, Request, execute};
use rustix::fs::{self as rfs, AtFlags, Timespec, Timestamps};

#[allow(dead_code, unused_imports)]
#[path = "../src/platform/mod.rs"]
mod platform;
#[allow(dead_code)]
#[path = "../src/roots.rs"]
mod roots;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct OwnedDirectory(PathBuf);

impl OwnedDirectory {
    fn create(parent: &Path, purpose: &str) -> Self {
        loop {
            let path = parent.join(format!("native-boundaries-{purpose}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create owned fixture directory: {error}"),
            }
        }
    }
    fn path(&self) -> &Path { &self.0 }
}
impl Drop for OwnedDirectory {
    fn drop(&mut self) { fs::remove_dir_all(&self.0).expect("remove only the owned fixture directory"); }
}

fn workspace() -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("workspace root").to_path_buf() }
fn local(purpose: &str) -> OwnedDirectory {
    let scratch = workspace().join(".work/tmp");
    fs::create_dir_all(&scratch).expect("create workspace scratch");
    OwnedDirectory::create(&scratch, purpose)
}
fn cross_root(source_device: u64) -> Option<PathBuf> {
    let Some(value) = env::var_os("ISH_FS_TEST_CROSS_ROOT") else {
        eprintln!("SKIP: set ISH_FS_TEST_CROSS_ROOT to an existing distinct-device directory under this repo's .work");
        return None;
    };
    let work = workspace().join(".work");
    fs::create_dir_all(work.join("tmp")).expect("create workspace scratch");
    let work = fs::canonicalize(work).expect("canonicalize workspace .work");
    let root = fs::canonicalize(value).expect("canonicalize ISH_FS_TEST_CROSS_ROOT");
    assert!(root.starts_with(work), "ISH_FS_TEST_CROSS_ROOT must resolve under this repo's .work");
    assert!(fs::metadata(&root).expect("stat cross-device root").is_dir());
    let device = fs::metadata(&root).expect("stat cross-device root").dev();
    if device == source_device {
        eprintln!("SKIP: ISH_FS_TEST_CROSS_ROOT is on device {device}, same as workspace .work/tmp");
        return None;
    }
    Some(root)
}
fn run(request: &Request) -> (ish_fs::Outcome, Vec<Event>) {
    let events = Mutex::new(Vec::new());
    let outcome = execute(request, &Cancellation::default(), &|event| { events.lock().unwrap().push(event); Ok(()) });
    (outcome, events.into_inner().unwrap())
}
fn moved(events: &[Event]) -> bool {
    events.iter().any(|event| matches!(event, Event::Operand(outcome) if outcome.state == OperandState::Moved))
}
fn request(operation: Operation, cwd: &Path, scratch: &Path, source: &Path, destination: &Path) -> Request {
    Request { operation, operands: Operands::Exact { source: source.to_path_buf(), destination: destination.to_path_buf() }, options: Options::default(), context: Context { cwd: cwd.to_path_buf(), home: None, scratch: scratch.to_path_buf() } }
}
fn set_modified_time(path: &Path) {
    let file = OpenOptions::new().read(true).write(true).open(path).expect("open fixture file for timestamps");
    let time = Timespec { tv_sec: 1_700_000_123, tv_nsec: 0 };
    rfs::futimens(&file, &Timestamps { last_access: time, last_modification: time }).expect("set fixture timestamp");
}

#[test]
fn native_boundary_real_exdev_move_preserves_and_syncs() {
    let fixture = local("exdev-source");
    let source_device = fs::metadata(fixture.path()).expect("stat source fixture").dev();
    let Some(cross) = cross_root(source_device) else { return };
    let destination_parent = OwnedDirectory::create(&cross, "exdev-destination");
    let source = fixture.path().join("source");
    let source_file = source.join("payload");
    fs::create_dir(&source).expect("create source tree");
    fs::write(&source_file, b"real cross-device move payload").expect("write source payload");
    fs::set_permissions(&source_file, fs::Permissions::from_mode(0o641)).expect("set source file mode");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).expect("set source directory mode");
    set_modified_time(&source_file);
    let source_metadata = fs::metadata(&source_file).expect("stat source payload");

    let probe = fixture.path().join("rename-probe");
    fs::write(&probe, b"probe").expect("write rename probe");
    let error = fs::rename(&probe, destination_parent.path().join("rename-probe")).expect_err("cross-device rename must fail");
    assert_eq!(error.raw_os_error(), Some(libc::EXDEV));
    assert!(probe.exists());

    let destination = destination_parent.path().join("moved");
    let (outcome, events) = run(&request(Operation::Move, fixture.path(), fixture.path(), Path::new("source"), &destination));
    assert_eq!(outcome.completion, Completion::Complete);
    assert!(moved(&events));
    assert!(!source.exists());
    let destination_file = destination.join("payload");
    assert_eq!(fs::read(&destination_file).expect("read moved payload"), b"real cross-device move payload");
    let copied = fs::metadata(&destination_file).expect("stat moved payload");
    assert_eq!(copied.mode() & 0o777, source_metadata.mode() & 0o777);
    assert_eq!((copied.mtime(), copied.mtime_nsec()), (source_metadata.mtime(), source_metadata.mtime_nsec()));
    assert_eq!(fs::metadata(&destination).expect("stat moved directory").mode() & 0o777, 0o751);
    assert!(outcome.metrics.destination_sync_work > std::time::Duration::ZERO);
    assert!(outcome.metrics.source_retirement_elapsed > std::time::Duration::ZERO);
}

#[test]
fn native_boundary_foreign_readable_file_survives_real_exdev_move() {
    if rustix::process::geteuid() != rustix::process::Uid::ROOT {
        eprintln!("SKIP: foreign-owner preservation requires effective uid 0 to create the disposable source fixture");
        return;
    }
    let fixture = local("foreign-source");
    let source_device = fs::metadata(fixture.path()).expect("stat source fixture").dev();
    let Some(cross) = cross_root(source_device) else { return };
    let destination_parent = OwnedDirectory::create(&cross, "foreign-destination");
    let source = fixture.path().join("source");
    fs::write(&source, b"foreign readable payload").expect("write source payload");
    rfs::chownat(rfs::CWD, &source, Some(rustix::process::Uid::from_raw(65_534)), None, AtFlags::empty()).expect("assign foreign source owner");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o044)).expect("make foreign source readable");
    let original = fs::metadata(&source).expect("stat foreign source");
    assert_eq!(original.uid(), 65_534);
    assert_eq!(original.mode() & 0o777, 0o044);

    let destination = destination_parent.path().join("moved");
    let (outcome, events) = run(&request(Operation::Move, fixture.path(), fixture.path(), Path::new("source"), &destination));
    assert_eq!(outcome.completion, Completion::Complete);
    assert!(moved(&events));
    assert!(!source.exists());
    assert_eq!(fs::read(&destination).expect("read moved foreign payload"), b"foreign readable payload");
    let copied = fs::metadata(&destination).expect("stat moved foreign payload");
    assert_eq!((copied.uid(), copied.gid(), copied.mode() & 0o777), (original.uid(), original.gid(), 0o044));
    assert!(outcome.metrics.destination_sync_work > std::time::Duration::ZERO);
    assert!(outcome.metrics.source_retirement_elapsed > std::time::Duration::ZERO);
}

#[test]
fn native_boundary_nested_mount_requires_opt_in() {
    let (Some(source), Some(mount)) = (env::var_os("ISH_FS_TEST_NESTED_SOURCE"), env::var_os("ISH_FS_TEST_NESTED_MOUNT")) else {
        eprintln!("SKIP: set ISH_FS_TEST_NESTED_SOURCE and ISH_FS_TEST_NESTED_MOUNT to a disposable tree and nested mount under this repo's .work");
        return;
    };
    let work = workspace().join(".work");
    fs::create_dir_all(work.join("tmp")).expect("create workspace scratch");
    let work = fs::canonicalize(work).expect("canonicalize workspace .work");
    let source = fs::canonicalize(source).expect("canonicalize nested-mount source fixture");
    let mount = fs::canonicalize(mount).expect("canonicalize nested-mount child fixture");
    assert!(source.starts_with(&work) && mount.starts_with(&source) && source != mount);
    let source_meta = fs::metadata(&source).expect("stat source fixture");
    let mount_meta = fs::metadata(&mount).expect("stat nested mount fixture");
    assert!(fs::symlink_metadata(&source).expect("lstat source fixture").is_dir());
    assert!(fs::symlink_metadata(&mount).expect("lstat nested mount fixture").is_dir());
    assert_eq!(source_meta.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(mount_meta.uid(), rustix::process::geteuid().as_raw());
    let source_fd = File::open(&source).expect("open source fixture");
    let mount_fd = File::open(&mount).expect("open nested mount fixture");
    let source_mount = match platform::linux::mount_id(source_fd.as_fd()) {
        Ok(id) => id,
        Err(error) => { eprintln!("SKIP: Linux statx mount IDs are unavailable: {error}"); return; }
    };
    let mount_id = platform::linux::mount_id(mount_fd.as_fd()).expect("read nested mount ID");
    assert_ne!(source_mount, mount_id, "fixture paths must have different mount IDs");
    let source_id = (source_meta.dev(), source_meta.ino());
    let mount_id_before = (mount_meta.dev(), mount_meta.ino(), mount_id);
    let payload = OwnedDirectory::create(&mount, "nested-payload");
    fs::write(payload.path().join("proof"), b"nested mounted filesystem payload").expect("write owned mounted fixture payload");

    let destination_parent = local("nested-destination");
    assert!(!destination_parent.path().starts_with(&source), "source fixture must not contain the test destination");
    let destination = destination_parent.path().join("copy");
    let mut request = request(Operation::Copy, source.parent().unwrap(), destination_parent.path(), source.file_name().unwrap().as_ref(), &destination);
    let (outcome, _) = run(&request);
    assert_eq!(outcome.completion, Completion::Failed);
    assert!(!destination.exists());
    let source_after = fs::metadata(&source).expect("restat source fixture");
    let mount_after = fs::metadata(&mount).expect("restat nested mount fixture");
    assert_eq!((source_after.dev(), source_after.ino()), source_id);
    assert_eq!((mount_after.dev(), mount_after.ino(), mount_id), mount_id_before);
    request.options.cross_file_systems = true;
    let (outcome, _) = run(&request);
    assert_eq!(outcome.completion, Completion::Complete);
    let copied = destination.join(payload.path().strip_prefix(&source).expect("payload is inside the source fixture")).join("proof");
    assert_eq!(fs::read(copied).expect("read copied mounted payload"), b"nested mounted filesystem payload");
    assert_eq!(fs::read(payload.path().join("proof")).expect("read retained mounted payload"), b"nested mounted filesystem payload");
}
