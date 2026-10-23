use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use ish_fs::{Cancellation, Collision, Context, Event, Jobs, Operands, Operation, Options, Request, execute};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("transfer-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn request(&self, source: &str, destination: &str) -> Request {
        Request { operation: Operation::Copy, operands: Operands::Exact { source: source.into(), destination: destination.into() }, options: Options::default(), context: Context { cwd: self.0.clone(), home: None, scratch: self.0.clone() } }
    }
    fn run(&self, request: &Request) -> i32 {
        execute(request, &Cancellation::default(), &|event: Event| -> io::Result<()> { if let Event::Diagnostic(diagnostic) = event { eprintln!("{:?}: {}", diagnostic.path, diagnostic.message); } Ok(()) }).status()
    }
}
impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }
struct RestoreWritable(Vec<PathBuf>);
impl Drop for RestoreWritable {
    fn drop(&mut self) {
        for path in &self.0 {
            if path.exists() { fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap(); }
        }
    }
}

#[test]
fn copy_always_transfers_even_same_size_same_mtime() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("source"), b"new bytes").unwrap();
    fs::write(fixture.0.join("destination"), b"old bytes").unwrap();
    let source_time = fs::metadata(fixture.0.join("source")).unwrap().modified().unwrap();
    fs::File::options().write(true).open(fixture.0.join("destination")).unwrap().set_times(fs::FileTimes::new().set_modified(source_time)).unwrap();
    assert_eq!(fs::metadata(fixture.0.join("source")).unwrap().modified().unwrap(), fs::metadata(fixture.0.join("destination")).unwrap().modified().unwrap());
    let mut request = fixture.request("source", "destination");
    request.options.collision = Collision::Replace;
    assert_eq!(fixture.run(&request), 0);
    assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"new bytes");
}

#[test]
fn byte_paths_symlinks_and_hardlinks_are_preserved() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("source")).unwrap();
    fs::write(fixture.0.join("source/file"), b"ordinary bytes\0\xff").unwrap();
    fs::hard_link(fixture.0.join("source/file"), fixture.0.join("source/alias")).unwrap();
    symlink("file", fixture.0.join("source/link")).unwrap();
    let raw_name = std::ffi::OsStr::from_bytes(b"raw\xff\n");
    let raw_name_supported = match fs::write(fixture.0.join("source").join(raw_name), b"raw bytes\0\xff") {
        Ok(()) => {
            fs::hard_link(fixture.0.join("source").join(raw_name), fixture.0.join("source/raw-alias")).unwrap();
            symlink(raw_name, fixture.0.join("source/raw-link")).unwrap();
            true
        }
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!("raw-name path leg excluded: filesystem rejected malformed UTF-8");
            false
        }
        Err(error) => panic!("create raw-name source: {error}"),
    };
    assert_eq!(fixture.run(&fixture.request("source", "destination")), 0);
    assert_eq!(fs::read(fixture.0.join("destination/file")).unwrap(), b"ordinary bytes\0\xff");
    assert_eq!(fs::read_link(fixture.0.join("destination/link")).unwrap(), Path::new("file"));
    assert_eq!(fs::metadata(fixture.0.join("destination/file")).unwrap().ino(), fs::metadata(fixture.0.join("destination/alias")).unwrap().ino());
    if raw_name_supported {
        assert_eq!(fs::read(fixture.0.join("destination").join(raw_name)).unwrap(), b"raw bytes\0\xff");
        assert_eq!(fs::read_link(fixture.0.join("destination/raw-link")).unwrap(), Path::new(raw_name));
        assert_eq!(fs::metadata(fixture.0.join("destination").join(raw_name)).unwrap().ino(), fs::metadata(fixture.0.join("destination/raw-alias")).unwrap().ino());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn acl_readable_mode_zero_source_copies_with_its_mode_and_acl() {
    let fixture = Fixture::new();
    let source = fixture.0.join("source");
    let destination = fixture.0.join("destination");
    fs::write(&source, b"readable only by ACL").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
    let acl = Command::new("/bin/chmod")
        .arg("+a")
        .arg("everyone allow read,readattr,readextattr,readsecurity")
        .arg(&source)
        .output()
        .unwrap();
    assert!(acl.status.success(), "chmod ACL failed: {}", String::from_utf8_lossy(&acl.stderr));
    assert_eq!(fs::read(&source).unwrap(), b"readable only by ACL");

    assert_eq!(fixture.run(&fixture.request("source", "destination")), 0);
    assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0);
    assert_eq!(fs::read(&destination).unwrap(), b"readable only by ACL");

    let source_acl = Command::new("/bin/ls").arg("-lde").arg(&source).output().unwrap();
    let destination_acl = Command::new("/bin/ls").arg("-lde").arg(&destination).output().unwrap();
    assert!(source_acl.status.success());
    assert!(destination_acl.status.success());
    let source_acl = String::from_utf8(source_acl.stdout).unwrap();
    let destination_acl = String::from_utf8(destination_acl.stdout).unwrap();
    assert_eq!(source_acl.lines().skip(1).collect::<Vec<_>>(), destination_acl.lines().skip(1).collect::<Vec<_>>());
}

#[test]
fn replacement_is_atomic_and_leaves_other_destination_hardlinks() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("source"), b"new").unwrap();
    fs::write(fixture.0.join("destination"), b"old").unwrap();
    fs::hard_link(fixture.0.join("destination"), fixture.0.join("alias")).unwrap();
    let mut request = fixture.request("source", "destination"); request.options.collision = Collision::Replace;
    assert_eq!(fixture.run(&request), 0);
    assert_eq!(fs::read(fixture.0.join("alias")).unwrap(), b"old");
    assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"new");
}

#[test]
fn readable_readonly_files_and_directory_modes_work() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("source")).unwrap();
    fs::write(fixture.0.join("source/file"), b"readonly").unwrap();
    fs::set_permissions(fixture.0.join("source/file"), fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(fixture.0.join("source"), fs::Permissions::from_mode(0o555)).unwrap();
    assert_eq!(fixture.run(&fixture.request("source", "destination")), 0);
    assert_eq!(fs::metadata(fixture.0.join("destination/file")).unwrap().mode() & 0o777, 0o444);
    assert_eq!(fs::metadata(fixture.0.join("destination")).unwrap().mode() & 0o777, 0o555);
    fs::set_permissions(fixture.0.join("source"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(fixture.0.join("destination"), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn readonly_file_with_user_xattr_transfers_with_its_xattr() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("source")).unwrap();
    let source_file = fixture.0.join("source/file");
    fs::write(&source_file, b"readonly").unwrap();
    rustix::fs::setxattr(&source_file, "user.ish-fs-readonly", b"value", rustix::fs::XattrFlags::empty()).unwrap();
    fs::set_permissions(&source_file, fs::Permissions::from_mode(0o444)).unwrap();
    assert_eq!(fixture.run(&fixture.request("source", "destination")), 0);
    let destination_file = fixture.0.join("destination/file");
    assert_eq!(fs::metadata(&destination_file).unwrap().mode() & 0o777, 0o444);
    let mut value = [0; 16];
    let length = rustix::fs::getxattr(&destination_file, "user.ish-fs-readonly", &mut value).unwrap();
    assert_eq!(&value[..length], b"value");
}

#[test]
fn readonly_directory_with_user_xattr_transfers_with_its_xattr() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("source")).unwrap();
    fs::write(fixture.0.join("source/file"), b"contents").unwrap();
    rustix::fs::setxattr(fixture.0.join("source"), "user.ish-fs-readonly", b"value", rustix::fs::XattrFlags::empty()).unwrap();
    fs::set_permissions(fixture.0.join("source"), fs::Permissions::from_mode(0o555)).unwrap();
    let _restore = RestoreWritable(vec![fixture.0.join("source"), fixture.0.join("destination")]);
    assert_eq!(fixture.run(&fixture.request("source", "destination")), 0);
    assert_eq!(fs::read(fixture.0.join("destination/file")).unwrap(), b"contents");
    assert_eq!(fs::metadata(fixture.0.join("destination")).unwrap().mode() & 0o777, 0o555);
    let mut value = [0; 16];
    let length = rustix::fs::getxattr(fixture.0.join("destination"), "user.ish-fs-readonly", &mut value).unwrap();
    assert_eq!(&value[..length], b"value");
}

#[test]
fn wide_tree_bounds_queue_records_and_descriptors() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap();
    for index in 0_u32..1024 { fs::write(fixture.0.join(format!("source/{index}")), index.to_le_bytes()).unwrap(); }
    let mut request = fixture.request("source", "destination"); request.options.jobs = Some(Jobs::new(4).unwrap());
    assert_eq!(fixture.run(&request), 0);
    for index in 0_u32..1024 { assert_eq!(fs::read(fixture.0.join(format!("destination/{index}"))).unwrap(), index.to_le_bytes()); }
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
}

#[test]
fn deep_tree_iterative_no_fd_per_depth() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap();
    let mut path = fixture.0.join("source");
    for _ in 0..100 { path.push("d"); fs::create_dir(&path).unwrap(); }
    fs::write(path.join("file"), b"deep").unwrap();
    assert_eq!(fixture.run(&fixture.request("source", "destination")), 0);
    let relative = path.strip_prefix(fixture.0.join("source")).unwrap();
    assert_eq!(fs::read(fixture.0.join("destination").join(relative).join("file")).unwrap(), b"deep");
}

#[test]
fn dry_run_wide_tree_no_spool() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap();
    for index in 0_u32..1024 { fs::write(fixture.0.join(format!("source/{index}")), b"unchanged").unwrap(); }
    let mut request = fixture.request("source", "destination"); request.options.dry_run = true; request.context.scratch = fixture.0.join("absent-scratch");
    assert_eq!(fixture.run(&request), 0); assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
}

#[test]
fn merge_publishes_absent_subtree_once_and_preserves_existing_container_mode() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("source/tree/nested")).unwrap(); fs::write(fixture.0.join("source/tree/nested/file"), b"subtree").unwrap();
    fs::create_dir(fixture.0.join("destination")).unwrap(); fs::set_permissions(fixture.0.join("destination"), fs::Permissions::from_mode(0o711)).unwrap();
    let mut request = fixture.request("source", "destination"); request.options.merge = true;
    assert_eq!(fixture.run(&request), 0);
    assert_eq!(fs::read(fixture.0.join("destination/tree/nested/file")).unwrap(), b"subtree");
    assert_eq!(fs::metadata(fixture.0.join("destination")).unwrap().mode() & 0o777, 0o711);
}

#[test]
fn restrictive_umask_obeys_platform_staging_boundary() {
    if std::env::var_os("ISH_FS_TRANSFER_UMASK_CHILD").is_some() {
        let fixture = Fixture::new(); fs::create_dir_all(fixture.0.join("source/d")).unwrap(); fs::write(fixture.0.join("source/d/file"), b"preserved").unwrap();
        fs::write(fixture.0.join("foreign"), b"untouched").unwrap();
        let source_inode = fs::metadata(fixture.0.join("source")).unwrap().ino();
        let foreign_inode = fs::metadata(fixture.0.join("foreign")).unwrap().ino();
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o777));
        #[cfg(target_os = "linux")]
        {
            assert_eq!(fixture.run(&fixture.request("source", "destination")), 0);
            assert_eq!(fs::read(fixture.0.join("destination/d/file")).unwrap(), b"preserved");
        }
        #[cfg(target_os = "macos")]
        {
            let diagnostics = std::sync::Mutex::new(Vec::new());
            let result = execute(&fixture.request("source", "destination"), &Cancellation::default(), &|event| {
                if let Event::Diagnostic(diagnostic) = event { diagnostics.lock().unwrap().push(diagnostic.message); }
                Ok(())
            });
            assert_ne!(result.status(), 0);
            assert!(diagnostics.lock().unwrap().iter().any(|message| message.contains("restrictive umask")));
            assert!(!fixture.0.join("destination").exists());
            assert_eq!(fs::read(fixture.0.join("source/d/file")).unwrap(), b"preserved");
        }
        assert_eq!(fs::metadata(fixture.0.join("source")).unwrap().ino(), source_inode);
        assert_eq!(fs::metadata(fixture.0.join("foreign")).unwrap().ino(), foreign_inode);
        assert_eq!(fs::read(fixture.0.join("foreign")).unwrap(), b"untouched");
        #[cfg(target_os = "macos")]
        for entry in fs::read_dir(&fixture.0).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with(".fdu-quarantine-v2-") {
                fs::remove_dir(entry.path()).unwrap();
            }
        }
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "restrictive_umask_obeys_platform_staging_boundary", "--nocapture"]).env("ISH_FS_TRANSFER_UMASK_CHILD", "1").output().unwrap();
    assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
