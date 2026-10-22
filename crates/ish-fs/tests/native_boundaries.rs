#![cfg(target_os = "linux")]

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
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
fn required_env(name: &str) -> std::ffi::OsString {
    env::var_os(name).unwrap_or_else(|| panic!("{name} must be set to a disposable fixture under this repo's .work; see docs/filesystem-testing.md"))
}
fn cross_root(source_device: u64) -> PathBuf {
    let value = required_env("ISH_FS_TEST_CROSS_ROOT");
    let work = workspace().join(".work");
    fs::create_dir_all(work.join("tmp")).expect("create workspace scratch");
    let work = fs::canonicalize(work).expect("canonicalize workspace .work");
    let root = fs::canonicalize(value).expect("canonicalize ISH_FS_TEST_CROSS_ROOT");
    assert!(root.starts_with(work), "ISH_FS_TEST_CROSS_ROOT must resolve under this repo's .work");
    assert!(fs::metadata(&root).expect("stat cross-device root").is_dir());
    let device = fs::metadata(&root).expect("stat cross-device root").dev();
    assert_ne!(device, source_device, "ISH_FS_TEST_CROSS_ROOT is on device {device}, same as workspace .work/tmp");
    root
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
#[ignore = "needs a distinct-device directory in ISH_FS_TEST_CROSS_ROOT; run benches/fs/native-fixtures.sh"]
fn native_boundary_real_exdev_move_preserves_and_syncs() {
    let fixture = local("exdev-source");
    let source_device = fs::metadata(fixture.path()).expect("stat source fixture").dev();
    let cross = cross_root(source_device);
    let destination_parent = OwnedDirectory::create(&cross, "exdev-destination");
    let source = fixture.path().join("source");
    let source_file = source.join("payload");
    fs::create_dir(&source).expect("create source tree");
    fs::write(&source_file, b"real cross-device move payload").expect("write source payload");
    let source_alias = source.join("payload-alias");
    fs::hard_link(&source_file, &source_alias).expect("hardlink source payload");
    fs::set_permissions(&source_file, fs::Permissions::from_mode(0o641)).expect("set source file mode");
    set_modified_time(&source_file);
    let source_metadata = fs::metadata(&source_file).expect("stat source payload");
    let source_alias_metadata = fs::metadata(&source_alias).expect("stat source hardlink alias");
    assert_eq!((source_metadata.dev(), source_metadata.ino()), (source_alias_metadata.dev(), source_alias_metadata.ino()));
    let source_link = source.join("link");
    let source_link_alias = source.join("link-alias");
    let link_target = Path::new("missing-relative-target");
    symlink(link_target, &source_link).expect("create source symlink");
    rfs::linkat(rfs::CWD, &source_link, rfs::CWD, &source_link_alias, AtFlags::empty()).expect("hardlink source symlink without following it");
    let original_link = fs::symlink_metadata(&source_link).expect("lstat source symlink");
    let original_link_alias = fs::symlink_metadata(&source_link_alias).expect("lstat source symlink alias");
    assert!(original_link.file_type().is_symlink());
    assert!(original_link_alias.file_type().is_symlink());
    assert_eq!((original_link.dev(), original_link.ino()), (original_link_alias.dev(), original_link_alias.ino()));
    fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).expect("set source directory mode");
    let source_owner = rustix::process::geteuid().as_raw();
    assert_eq!(source_metadata.uid(), source_owner);

    let probe = fixture.path().join("rename-probe");
    fs::write(&probe, b"probe").expect("write rename probe");
    let error = fs::rename(&probe, destination_parent.path().join("rename-probe")).expect_err("cross-device rename must fail");
    assert_eq!(error.raw_os_error(), Some(libc::EXDEV));
    assert!(probe.exists());

    let destination = destination_parent.path().join("moved");
    let (outcome, events) = run(&request(Operation::Move, fixture.path(), fixture.path(), Path::new("source"), &destination));
    assert_eq!(outcome.completion, Completion::Complete, "events: {events:?}");
    assert_eq!(outcome.status(), 0);
    assert!(moved(&events));
    assert!(!source.exists());
    let destination_file = destination.join("payload");
    assert_eq!(fs::read(&destination_file).expect("read moved payload"), b"real cross-device move payload");
    let copied = fs::metadata(&destination_file).expect("stat moved payload");
    assert_eq!(copied.mode() & 0o777, source_metadata.mode() & 0o777);
    assert_eq!((copied.mtime(), copied.mtime_nsec()), (source_metadata.mtime(), source_metadata.mtime_nsec()));
    assert_eq!(copied.uid(), source_owner);
    let copied_alias = fs::metadata(destination.join("payload-alias")).expect("stat moved hardlink alias");
    assert_eq!((copied.dev(), copied.ino()), (copied_alias.dev(), copied_alias.ino()));
    assert!(copied.nlink() >= 2);
    let copied_link = fs::symlink_metadata(destination.join("link")).expect("lstat moved symlink");
    let copied_link_alias = fs::symlink_metadata(destination.join("link-alias")).expect("lstat moved symlink alias");
    assert!(copied_link.file_type().is_symlink());
    assert!(copied_link_alias.file_type().is_symlink());
    assert_eq!((copied_link.dev(), copied_link.ino()), (copied_link_alias.dev(), copied_link_alias.ino()));
    assert_eq!(fs::read_link(destination.join("link")).expect("read moved symlink target"), link_target);
    assert_eq!(fs::read_link(destination.join("link-alias")).expect("read moved hardlinked symlink target"), link_target);
    assert_eq!(fs::metadata(&destination).expect("stat moved directory").mode() & 0o777, 0o751);
    assert!(outcome.metrics.destination_sync_work > std::time::Duration::ZERO);
    assert!(outcome.metrics.source_retirement_elapsed > std::time::Duration::ZERO);
}

#[test]
#[ignore = "needs a distinct-device directory in ISH_FS_TEST_CROSS_ROOT; run benches/fs/native-fixtures.sh"]
fn native_boundary_real_exdev_replacement_accounts_for_destination_aliases() {
    let fixture = local("exdev-replacement-source");
    let source_device = fs::metadata(fixture.path()).expect("stat source fixture").dev();
    let cross = cross_root(source_device);
    let destination_parent = OwnedDirectory::create(&cross, "exdev-replacement-destination");
    let source = fixture.path().join("source");
    let output = destination_parent.path().join("out");
    fs::create_dir(&source).expect("create source directory");
    fs::create_dir(&output).expect("create replacement directory");
    fs::write(source.join("a"), b"new first").expect("write first source");
    fs::write(source.join("b"), b"new second").expect("write second source");
    fs::write(output.join("a"), b"old shared destination").expect("write old destination");
    fs::hard_link(output.join("a"), output.join("b")).expect("hardlink second old destination");
    fs::hard_link(output.join("a"), output.join("retained")).expect("hardlink retained old destination");
    let mut regular_request = request(Operation::Move, fixture.path(), fixture.path(), Path::new("source/a"), &output);
    regular_request.options.collision = ish_fs::Collision::Replace;
    regular_request.operands = Operands::Into { directory: output.clone(), first: PathBuf::from("source/a"), rest: vec![PathBuf::from("source/b")] };
    let probe = fixture.path().join("rename-probe");
    fs::write(&probe, b"probe").expect("write rename probe");
    let error = fs::rename(&probe, output.join("rename-probe")).expect_err("replacement destination must be on another device");
    assert_eq!(error.raw_os_error(), Some(libc::EXDEV));

    let (outcome, events) = run(&regular_request);
    assert_eq!(outcome.completion, Completion::Complete, "events: {events:?}");
    assert_eq!(outcome.status(), 0);
    assert!(events.iter().filter_map(|event| match event { Event::Operand(outcome) => Some(outcome.state), _ => None }).all(|state| state == OperandState::Moved));
    assert!(!source.join("a").exists());
    assert!(!source.join("b").exists());
    assert_eq!(fs::read(output.join("a")).expect("read first replacement"), b"new first");
    assert_eq!(fs::read(output.join("b")).expect("read second replacement"), b"new second");
    assert_eq!(fs::read(output.join("retained")).expect("read retained alias"), b"old shared destination");
    assert_eq!(fs::metadata(output.join("retained")).expect("stat retained alias").nlink(), 1);
    assert!(outcome.metrics.destination_sync_work > std::time::Duration::ZERO);
    assert!(outcome.metrics.source_retirement_elapsed > std::time::Duration::ZERO);

    let symlink_sources = [source.join("link-a"), source.join("link-b")];
    let symlink_destinations = [output.join("link-a"), output.join("link-b")];
    symlink("new-target-a", &symlink_sources[0]).expect("create first symlink source");
    symlink("new-target-b", &symlink_sources[1]).expect("create second symlink source");
    symlink("old-target", &symlink_destinations[0]).expect("create old symlink destination");
    fs::hard_link(&symlink_destinations[0], &symlink_destinations[1]).expect("hardlink second symlink destination");
    let retained_symlink = output.join("link-retained");
    fs::hard_link(&symlink_destinations[0], &retained_symlink).expect("hardlink retained symlink destination");
    let mut symlink_request = request(Operation::Move, fixture.path(), fixture.path(), Path::new("source/link-a"), &output);
    symlink_request.options.collision = ish_fs::Collision::Replace;
    symlink_request.operands = Operands::Into { directory: output.clone(), first: PathBuf::from("source/link-a"), rest: vec![PathBuf::from("source/link-b")] };
    let (outcome, events) = run(&symlink_request);
    assert_eq!(outcome.completion, Completion::Complete, "events: {events:?}");
    assert_eq!(outcome.status(), 0);
    assert_eq!(events.iter().filter_map(|event| match event { Event::Operand(outcome) => Some(outcome.state), _ => None }).collect::<Vec<_>>(), [OperandState::Moved; 2]);
    for path in &symlink_sources {
        assert_eq!(fs::symlink_metadata(path).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
    assert_eq!(fs::read_link(&symlink_destinations[0]).expect("read first symlink destination"), Path::new("new-target-a"));
    assert_eq!(fs::read_link(&symlink_destinations[1]).expect("read second symlink destination"), Path::new("new-target-b"));
    assert_eq!(fs::read_link(&retained_symlink).expect("read retained symlink destination"), Path::new("old-target"));
    assert_eq!(fs::symlink_metadata(&retained_symlink).expect("stat retained symlink destination").nlink(), 1);
    assert_ne!(fs::symlink_metadata(&symlink_destinations[0]).expect("stat first symlink destination").ino(), fs::symlink_metadata(&symlink_destinations[1]).expect("stat second symlink destination").ino());
}

#[test]
#[ignore = "needs uid 0 with CAP_CHOWN and a distinct-device ISH_FS_TEST_CROSS_ROOT; see docs/filesystem-testing.md"]
fn native_boundary_foreign_readable_file_survives_real_exdev_move() {
    let foreign_uid = 65_534;
    assert_ne!(rustix::process::geteuid().as_raw(), foreign_uid, "uid 65534 is the effective uid and cannot represent a foreign-owner fixture");
    let fixture = local("foreign-source");
    let source_device = fs::metadata(fixture.path()).expect("stat source fixture").dev();
    let cross = cross_root(source_device);
    let destination_parent = OwnedDirectory::create(&cross, "foreign-destination");
    let source = fixture.path().join("source");
    fs::write(&source, b"foreign readable payload").expect("write source payload");
    rfs::chownat(rfs::CWD, &source, Some(rustix::process::Uid::from_raw(foreign_uid)), None, AtFlags::empty()).expect("assign foreign source owner");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o044)).expect("make foreign source readable");
    let original = fs::metadata(&source).expect("stat foreign source");
    assert_eq!(original.uid(), foreign_uid);
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
#[ignore = "needs uid 0 with CAP_CHOWN and a distinct-device ISH_FS_TEST_CROSS_ROOT; see docs/filesystem-testing.md"]
fn native_boundary_foreign_owner_denial_retains_real_exdev_source() {
    const CHILD: &str = "ISH_FS_TEST_FOREIGN_OWNER_DENIAL_CHILD";
    const FIXTURE: &str = "ISH_FS_TEST_FOREIGN_OWNER_DENIAL_FIXTURE";
    const DESTINATION_PARENT: &str = "ISH_FS_TEST_FOREIGN_OWNER_DENIAL_DESTINATION_PARENT";
    const CHILD_UID: &str = "ISH_FS_TEST_FOREIGN_OWNER_DENIAL_UID";
    const CHILD_GID: &str = "ISH_FS_TEST_FOREIGN_OWNER_DENIAL_GID";
    const FOREIGN_UID: u32 = 65_534;

    if env::var_os(CHILD).is_some() {
        let fixture = PathBuf::from(env::var_os(FIXTURE).expect("foreign-owner fixture path"));
        let destination_parent = PathBuf::from(env::var_os(DESTINATION_PARENT).expect("foreign-owner destination parent"));
        let child_uid = env::var(CHILD_UID).expect("foreign-owner child uid").parse::<u32>().expect("numeric foreign-owner child uid");
        let child_gid = env::var(CHILD_GID).expect("foreign-owner child gid").parse::<u32>().expect("numeric foreign-owner child gid");
        assert_eq!(rustix::process::geteuid().as_raw(), child_uid, "subprocess must run without root privileges");
        assert_eq!(rustix::process::getegid().as_raw(), child_gid, "subprocess must run with the fixture group");

        let source = fixture.join("source");
        let original = fs::metadata(&source).expect("stat foreign-owner source in child");
        assert_eq!((original.uid(), original.gid(), original.mode() & 0o777), (FOREIGN_UID, child_gid, 0o044));
        assert_eq!(fs::read(&source).expect("read foreign-owner source in child"), b"foreign owner denial payload");

        let ownership_probe = destination_parent.join("ownership-probe");
        fs::write(&ownership_probe, b"probe").expect("write ownership probe");
        let error = rfs::chownat(rfs::CWD, &ownership_probe, Some(rustix::process::Uid::from_raw(FOREIGN_UID)), None, AtFlags::empty())
            .expect_err("unprivileged child cannot assign the foreign source owner");
        assert_eq!(error, rustix::io::Errno::PERM, "ownership failure must be the real unprivileged limitation");
        fs::remove_file(&ownership_probe).expect("remove ownership probe");

        let destination = destination_parent.join("moved");
        let (outcome, events) = run(&request(Operation::Move, &fixture, &destination_parent, Path::new("source"), &destination));
        assert_eq!(outcome.completion, Completion::Failed, "events: {events:?}");
        assert_eq!(outcome.status(), 1);
        assert!(!moved(&events));
        assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.path == Path::new("source") && !diagnostic.message.is_empty())), "ownership failure must be reported: {events:?}");
        assert!(events.iter().any(|event| matches!(event, Event::Operand(result) if result.source == Path::new("source") && result.state == OperandState::FailedBeforePublication)), "ownership failure must not claim publication: {events:?}");
        assert!(!destination.exists(), "ownership failure must not publish a complete-looking destination");
        let retained = fs::metadata(&source).expect("foreign-owner source remains after denied move");
        assert_eq!((retained.dev(), retained.ino()), (original.dev(), original.ino()));
        assert_eq!((retained.uid(), retained.gid(), retained.mode() & 0o777), (FOREIGN_UID, child_gid, 0o044));
        assert_eq!(fs::read(&source).expect("read retained foreign-owner payload"), b"foreign owner denial payload");
        return;
    }

    let fixture = local("foreign-owner-denial-source");
    let source_device = fs::metadata(fixture.path()).expect("stat source fixture").dev();
    let cross = cross_root(source_device);
    let destination_parent = OwnedDirectory::create(&cross, "foreign-owner-denial-destination");

    let foreign_owner = rustix::process::Uid::from_raw(FOREIGN_UID);
    let child_owner = fs::metadata(workspace()).expect("stat workspace owner");
    let (child_uid, child_gid) = if rustix::process::geteuid().as_raw() == 0 {
        (
            if child_owner.uid() == 0 || child_owner.uid() == FOREIGN_UID { FOREIGN_UID - 1 } else { child_owner.uid() },
            if child_owner.gid() == FOREIGN_UID { FOREIGN_UID - 1 } else { child_owner.gid() },
        )
    } else {
        (rustix::process::geteuid().as_raw(), rustix::process::getegid().as_raw())
    };
    let source = fixture.path().join("source");
    fs::write(&source, b"foreign owner denial payload").expect("write source payload");
    rfs::chownat(rfs::CWD, &source, Some(foreign_owner), Some(rustix::process::Gid::from_raw(child_gid)), AtFlags::empty()).expect("assign foreign ownership-denial source owner");
    assert!(child_uid != 0 && child_uid != FOREIGN_UID, "no distinct unprivileged workspace owner is available for the foreign-owner denial child");
    for directory in [fixture.path(), destination_parent.path()] {
        rfs::chownat(rfs::CWD, directory, Some(rustix::process::Uid::from_raw(child_uid)), Some(rustix::process::Gid::from_raw(child_gid)), AtFlags::empty()).expect("assign unprivileged ownership-denial fixture directory owner");
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).expect("make ownership-denial fixture directory traversable");
    }
    fs::set_permissions(&source, fs::Permissions::from_mode(0o044)).expect("make foreign source readable");
    let original = fs::metadata(&source).expect("stat foreign-owner source");
    assert_eq!((original.uid(), original.gid(), original.mode() & 0o777), (FOREIGN_UID, child_gid, 0o044));

    let rename_probe = fixture.path().join("rename-probe");
    fs::write(&rename_probe, b"probe").expect("write rename probe");
    let error = fs::rename(&rename_probe, destination_parent.path().join("rename-probe")).expect_err("cross-device rename must fail");
    assert_eq!(error.raw_os_error(), Some(libc::EXDEV));
    assert!(rename_probe.exists());
    fs::remove_file(rename_probe).expect("remove rename probe");

    let mut child = Command::new(std::env::current_exe().expect("test executable path"));
    child
        .arg("--ignored")
        .arg("--exact")
        .arg("native_boundary_foreign_owner_denial_retains_real_exdev_source")
        .arg("--nocapture")
        .env(CHILD, "1")
        .env(FIXTURE, fixture.path())
        .env(DESTINATION_PARENT, destination_parent.path())
        .env(CHILD_UID, child_uid.to_string())
        .env(CHILD_GID, child_gid.to_string());
    if rustix::process::geteuid().as_raw() == 0 {
        child.gid(child_gid).uid(child_uid);
    }
    let output = child.output().expect("run foreign-owner move without privilege");
    assert!(output.status.success(), "foreign-owner denial child failed (stdout: {}, stderr: {})", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let source_after = fs::metadata(&source).expect("foreign-owner source retained after child move");
    assert_eq!((source_after.dev(), source_after.ino()), (original.dev(), original.ino()));
    assert_eq!((source_after.uid(), source_after.gid(), source_after.mode() & 0o777), (FOREIGN_UID, child_gid, 0o044));
    assert_eq!(fs::read(&source).expect("read retained source payload"), b"foreign owner denial payload");
    assert!(!destination_parent.path().join("moved").exists());
}

#[test]
#[ignore = "needs mounted fixtures in ISH_FS_TEST_NESTED_* and ISH_FS_TEST_CROSS_ROOT; run benches/fs/native-fixtures.sh"]
fn native_boundary_nested_mount_requires_opt_in() {
    let source = required_env("ISH_FS_TEST_NESTED_SOURCE");
    let tmpfs = required_env("ISH_FS_TEST_NESTED_MOUNT");
    let bind = required_env("ISH_FS_TEST_NESTED_BIND");
    let work = workspace().join(".work");
    fs::create_dir_all(work.join("tmp")).expect("create workspace scratch");
    let work = fs::canonicalize(work).expect("canonicalize workspace .work");
    let source = fs::canonicalize(source).expect("canonicalize nested-mount source fixture");
    let tmpfs = fs::canonicalize(tmpfs).expect("canonicalize nested tmpfs fixture");
    let bind = fs::canonicalize(bind).expect("canonicalize nested bind fixture");
    assert!(source.starts_with(&work) && tmpfs.starts_with(&source) && bind.starts_with(&source));
    assert_ne!(source, tmpfs);
    assert_ne!(source, bind);
    assert_ne!(tmpfs, bind);
    let source_meta = fs::metadata(&source).expect("stat source fixture");
    let tmpfs_meta = fs::metadata(&tmpfs).expect("stat nested tmpfs fixture");
    let bind_meta = fs::metadata(&bind).expect("stat nested bind fixture");
    assert!(fs::symlink_metadata(&source).expect("lstat source fixture").is_dir());
    assert!(fs::symlink_metadata(&tmpfs).expect("lstat nested tmpfs fixture").is_dir());
    assert!(fs::symlink_metadata(&bind).expect("lstat nested bind fixture").is_dir());
    assert_eq!(source_meta.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(tmpfs_meta.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(bind_meta.uid(), rustix::process::geteuid().as_raw());
    let source_fd = File::open(&source).expect("open source fixture");
    let tmpfs_fd = File::open(&tmpfs).expect("open nested tmpfs fixture");
    let bind_fd = File::open(&bind).expect("open nested bind fixture");
    let source_mount = platform::linux::mount_id(source_fd.as_fd()).expect("read source mount ID");
    let tmpfs_mount = platform::linux::mount_id(tmpfs_fd.as_fd()).expect("read nested tmpfs mount ID");
    let bind_mount = platform::linux::mount_id(bind_fd.as_fd()).expect("read nested bind mount ID");
    assert_ne!(source_mount, tmpfs_mount, "tmpfs fixture must have a distinct mount ID");
    assert_ne!(source_mount, bind_mount, "bind fixture must have a distinct mount ID");
    assert_ne!(source_meta.dev(), tmpfs_meta.dev(), "tmpfs fixture must have a distinct device ID");
    assert_eq!(source_meta.dev(), bind_meta.dev(), "bind fixture must retain its source device ID");
    let source_id = (source_meta.dev(), source_meta.ino(), source_mount);
    let tmpfs_id_before = (tmpfs_meta.dev(), tmpfs_meta.ino(), tmpfs_mount);
    let bind_id_before = (bind_meta.dev(), bind_meta.ino(), bind_mount);
    let root_payload = source.join("root-proof");
    let tmpfs_payload = tmpfs.join("tmpfs-proof");
    let bind_payload = bind.join("bind-proof");
    fs::write(&root_payload, b"ordinary source payload").expect("write ordinary source payload");
    fs::write(&tmpfs_payload, b"nested tmpfs payload").expect("write nested tmpfs payload");
    fs::write(&bind_payload, b"nested bind payload").expect("write nested bind payload");
    for mount in [&tmpfs, &bind] {
        let error = fs::remove_dir(mount).expect_err("a mounted source directory cannot be unlinked");
        assert_eq!(error.raw_os_error(), Some(libc::EBUSY), "mountpoint removal must fail with EBUSY");
    }

    let destination_parent = local("nested-destination");
    assert!(!destination_parent.path().starts_with(&source), "source fixture must not contain the test destination");
    for nested_tree in ["tmpfs-tree", "bind-tree"] {
        let nested_destination = destination_parent.path().join(format!("{nested_tree}-copy"));
        let nested_request = request(Operation::Copy, &source, destination_parent.path(), Path::new(nested_tree), &nested_destination);
        let (outcome, events) = run(&nested_request);
        assert_eq!(outcome.completion, Completion::Failed, "default traversal must refuse {nested_tree}");
        assert!(!nested_destination.exists(), "default traversal must not publish {nested_tree}");
        assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("nested source mount requires --cross-file-systems"))));
    }
    let destination = destination_parent.path().join("copy");
    let mut copy_request = request(Operation::Copy, source.parent().unwrap(), destination_parent.path(), source.file_name().unwrap().as_ref(), &destination);
    let (outcome, events) = run(&copy_request);
    assert_eq!(outcome.completion, Completion::Failed);
    assert!(!destination.exists());
    assert!(events.iter().any(|event| matches!(event, Event::Diagnostic(diagnostic) if diagnostic.message.contains("nested source mount requires --cross-file-systems"))));
    let source_after = fs::metadata(&source).expect("restat source fixture");
    let tmpfs_after = fs::metadata(&tmpfs).expect("restat nested tmpfs fixture");
    let bind_after = fs::metadata(&bind).expect("restat nested bind fixture");
    assert_eq!((source_after.dev(), source_after.ino(), platform::linux::mount_id(File::open(&source).expect("reopen source fixture").as_fd()).expect("restat source mount ID")), source_id);
    assert_eq!((tmpfs_after.dev(), tmpfs_after.ino(), platform::linux::mount_id(File::open(&tmpfs).expect("reopen nested tmpfs fixture").as_fd()).expect("restat nested tmpfs mount ID")), tmpfs_id_before);
    assert_eq!((bind_after.dev(), bind_after.ino(), platform::linux::mount_id(File::open(&bind).expect("reopen nested bind fixture").as_fd()).expect("restat nested bind mount ID")), bind_id_before);
    copy_request.options.cross_file_systems = true;
    let (outcome, _) = run(&copy_request);
    assert_eq!(outcome.completion, Completion::Complete);
    assert_eq!(fs::read(destination.join("root-proof")).expect("read copied ordinary payload"), b"ordinary source payload");
    assert_eq!(fs::read(destination.join("tmpfs-tree/tmpfs-mounted/tmpfs-proof")).expect("read copied tmpfs payload"), b"nested tmpfs payload");
    assert_eq!(fs::read(destination.join("bind-tree/bind-mounted/bind-proof")).expect("read copied bind payload"), b"nested bind payload");
    assert_eq!(fs::read(&tmpfs_payload).expect("read retained tmpfs payload"), b"nested tmpfs payload");
    assert_eq!(fs::read(&bind_payload).expect("read retained bind payload"), b"nested bind payload");

    let source_device = source_meta.dev();
    let cross = cross_root(source_device);
    let move_parent = OwnedDirectory::create(&cross, "nested-move-destination");
    let move_destination = move_parent.path().join("moved");
    let mut move_request = request(Operation::Move, source.parent().unwrap(), destination_parent.path(), source.file_name().unwrap().as_ref(), &move_destination);
    move_request.options.cross_file_systems = true;
    let (outcome, events) = run(&move_request);
    assert_eq!(outcome.completion, Completion::Failed);
    assert_eq!(outcome.status(), 1);
    assert!(events.iter().any(|event| matches!(event, Event::Operand(operand) if operand.state == OperandState::SourcePartiallyRetired)));
    assert!(move_destination.is_dir(), "durably published destination must remain after partial retirement");
    assert!(source.is_dir(), "the source root must remain when a nested mountpoint cannot be retired");
    assert!(tmpfs.is_dir() && bind.is_dir(), "mounted source names must remain after partial retirement");
    assert_eq!(fs::read(move_destination.join("root-proof")).expect("read moved ordinary payload"), b"ordinary source payload");
    assert_eq!(fs::read(move_destination.join("tmpfs-tree/tmpfs-mounted/tmpfs-proof")).expect("read moved tmpfs payload"), b"nested tmpfs payload");
    assert_eq!(fs::read(move_destination.join("bind-tree/bind-mounted/bind-proof")).expect("read moved bind payload"), b"nested bind payload");
    match fs::read(&root_payload) {
        Ok(bytes) => assert_eq!(bytes, b"ordinary source payload"),
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
    }
    assert_eq!(platform::linux::mount_id(File::open(&tmpfs).expect("reopen retained tmpfs mountpoint").as_fd()).expect("read retained tmpfs mount ID"), tmpfs_mount);
    assert_eq!(platform::linux::mount_id(File::open(&bind).expect("reopen retained bind mountpoint").as_fd()).expect("read retained bind mount ID"), bind_mount);
}

#[test]
#[ignore = "needs a bind mount in ISH_FS_TEST_ALIAS_*; run benches/fs/native-fixtures.sh"]
fn native_boundary_bind_exdev_then_same_mount_alias_preserves_topology() {
    let origin = required_env("ISH_FS_TEST_ALIAS_ORIGIN");
    let bind = required_env("ISH_FS_TEST_ALIAS_BIND");
    let work = fs::canonicalize(workspace().join(".work")).expect("canonicalize workspace .work");
    let origin = fs::canonicalize(origin).expect("canonicalize alias origin");
    let bind = fs::canonicalize(bind).expect("canonicalize alias bind");
    assert!(origin.starts_with(&work) && bind.starts_with(&work));
    let origin_stat = fs::metadata(&origin).expect("stat alias origin");
    let bind_stat = fs::metadata(&bind).expect("stat alias bind");
    assert_eq!((origin_stat.dev(), origin_stat.ino()), (bind_stat.dev(), bind_stat.ino()));
    let origin_mount = platform::linux::mount_id(File::open(&origin).expect("open alias origin").as_fd()).expect("read origin mount ID");
    let bind_mount = platform::linux::mount_id(File::open(&bind).expect("open alias bind").as_fd()).expect("read bind mount ID");
    assert_ne!(origin_mount, bind_mount);
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum AliasOrder { LeafTransferThenLeaf, DirectoryRenameThenLeaf, LeafTransferThenDirectory, DirectoryTransferThenLeaf }
    for scenario in [AliasOrder::LeafTransferThenLeaf, AliasOrder::DirectoryRenameThenLeaf, AliasOrder::LeafTransferThenDirectory, AliasOrder::DirectoryTransferThenLeaf] {
        for symbolic in [false, true] {
            let source = OwnedDirectory::create(&origin, "alias-source");
            let destination = local("alias-destination");
            let destination_mount = platform::linux::mount_id(File::open(destination.path()).expect("open alias destination").as_fd()).expect("read destination mount ID");
            assert_eq!(origin_mount, destination_mount);
            let bound_source = bind.join(source.path().file_name().expect("source basename"));
            let probe = source.path().join("rename-probe");
            fs::write(&probe, b"probe").expect("write rename probe");
            let target_probe = destination.path().join("rename-probe");
            let cross_error = fs::rename(bound_source.join("rename-probe"), &target_probe).expect_err("bind-to-origin rename must return EXDEV");
            assert_eq!(cross_error.raw_os_error(), Some(libc::EXDEV));
            fs::rename(&probe, &target_probe).expect("origin-to-destination rename must be available");
            fs::remove_file(target_probe).expect("remove completed rename probe");
            let first_in_tree = matches!(scenario, AliasOrder::DirectoryRenameThenLeaf | AliasOrder::DirectoryTransferThenLeaf);
            let second_in_tree = scenario == AliasOrder::LeafTransferThenDirectory;
            if first_in_tree || second_in_tree { fs::create_dir(source.path().join("tree")).expect("create aliased source tree"); }
            let first_name = if first_in_tree { "tree/member" } else { "first" };
            let second_name = if second_in_tree { "tree/member" } else { "second" };
            let first = source.path().join(first_name);
            let second = source.path().join(second_name);
            if symbolic { symlink("dangling", &first).expect("create source symlink"); }
            else { fs::write(&first, b"mixed mount payload").expect("write source payload"); }
            rfs::linkat(rfs::CWD, &first, rfs::CWD, &second, AtFlags::empty()).expect("hardlink source object without following symlinks");
            let original = fs::symlink_metadata(&first).expect("lstat source object");
            assert_eq!(original.ino(), fs::symlink_metadata(&second).expect("lstat source alias").ino());
            assert_eq!(original.nlink(), 2);
            let request = Request {
                operation: Operation::Move,
                operands: Operands::Into { directory: destination.path().to_path_buf(), first: match scenario { AliasOrder::DirectoryRenameThenLeaf => source.path().join("tree"), AliasOrder::DirectoryTransferThenLeaf => bound_source.join("tree"), _ => bound_source.join("first") }, rest: vec![match scenario { AliasOrder::DirectoryRenameThenLeaf => bound_source.join("second"), AliasOrder::LeafTransferThenDirectory => source.path().join("tree"), _ => second.clone() }] },
                options: Options::default(),
                context: Context { cwd: workspace(), home: None, scratch: workspace().join(".work/tmp") },
            };
            let (outcome, events) = run(&request);
            assert_eq!(outcome.status(), 0, "scenario={scenario:?}, symbolic={symbolic}, events: {events:?}");
            let copied = fs::symlink_metadata(destination.path().join(first_name)).expect("lstat first output");
            let alias = fs::symlink_metadata(destination.path().join(second_name)).expect("lstat alias output");
            assert_eq!((copied.dev(), copied.ino()), (alias.dev(), alias.ino()), "scenario={scenario:?}, symbolic={symbolic}");
            if scenario == AliasOrder::DirectoryRenameThenLeaf { assert_eq!(copied.ino(), original.ino()); } else { assert_ne!(copied.ino(), original.ino()); }
            assert!(fs::symlink_metadata(&first).is_err());
            assert!(fs::symlink_metadata(&second).is_err());
            if symbolic {
                assert_eq!(fs::read_link(destination.path().join(second_name)).expect("read moved symlink"), PathBuf::from("dangling"));
            } else {
                assert_eq!(fs::read(destination.path().join(second_name)).expect("read moved payload"), b"mixed mount payload");
                assert_eq!(outcome.progress.cloned_files + outcome.progress.native_copied_files + outcome.progress.buffered_copied_files, u64::from(scenario != AliasOrder::DirectoryRenameThenLeaf));
            }
        }
    }
}
