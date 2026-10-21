#![cfg(target_os = "linux")]

use std::ffi::{OsStr, OsString};
use std::fs::{self as stdfs, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

#[allow(dead_code)]
#[path = "../src/roots.rs"]
mod roots;
#[allow(dead_code)]
#[path = "../src/platform/mod.rs"]
mod platform;

pub use ish_fs::Cancellation;

use platform::{ExtendedMetadata, MetadataTarget, Preservation};
use rustix::fs::{self, IFlags, Mode, OFlags};
use rustix::io::Errno;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let work = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crate worktree ancestor")
            .join(".work/tmp");
        stdfs::create_dir_all(&work).expect("create shared test scratch");
        loop {
            let index = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = work.join(format!("native-linux-{}-{index}", std::process::id()));
            match stdfs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create native test directory: {error}"),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        stdfs::remove_dir_all(&self.0).expect("remove owned native test directory");
    }
}

fn open_source(path: &Path) -> File {
    File::open(path).expect("open source")
}

fn open_destination(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create destination")
}

fn copy(source: &File, destination: &File, length: u64, scratch: &mut [u8]) -> platform::CopyStats {
    platform::copy_data(
        source.as_fd(),
        destination.as_fd(),
        length,
        scratch,
        &Cancellation::default(),
    )
    .expect("copy captured source length")
}

fn unsupported_errno(error: Errno) -> bool {
    matches!(error, Errno::NOSYS | Errno::NOTSUP | Errno::XDEV | Errno::INVAL | Errno::NOTTY)
}

fn try_set_user_xattr(fd: std::os::fd::BorrowedFd<'_>, name: &[u8], value: &[u8]) -> bool {
    match fs::fsetxattr(fd, OsStr::from_bytes(name), value, fs::XattrFlags::empty()) {
        Ok(()) => true,
        Err(error) if unsupported_errno(error) || error == Errno::PERM => {
            eprintln!("native Linux xattr unavailable on fixture filesystem: {error}");
            false
        }
        Err(error) => panic!("set source xattr: {error}"),
    }
}

#[test]
fn native_linux_copy_uses_a_supported_kernel_method_and_restores_source_offset() {
    let directory = TestDirectory::new();
    let source_path = directory.path().join("source");
    let destination_path = directory.path().join("destination");
    let expected = (0..512 * 1024)
        .map(|index| ((index * 31 + 7) % 251) as u8)
        .collect::<Vec<_>>();
    stdfs::write(&source_path, &expected).expect("write source bytes");
    let source = open_source(&source_path);
    let destination = open_destination(&destination_path);
    let reflink = platform::linux::try_reflink(source.as_fd(), destination.as_fd());
    match reflink {
        Ok(()) => eprintln!("native Linux FICLONE is supported on the fixture filesystem"),
        Err(platform::AccelerationError::Unsupported) => {
            let error = fs::ioctl_ficlone(destination.as_fd(), source.as_fd()).unwrap_err();
            assert!(unsupported_errno(error));
            eprintln!("native Linux FICLONE unavailable: {error}");
        }
        Err(platform::AccelerationError::Io(error)) => {
            panic!("native Linux FICLONE failed unexpectedly: {error}")
        }
    }

    stdfs::remove_file(&destination_path).expect("remove reflink probe destination");
    let destination = open_destination(&destination_path);
    let mut source = source;
    let initial_offset = 19;
    source.seek(SeekFrom::Start(initial_offset)).expect("set source offset");
    let mut scratch = vec![0; 31 * 1024];
    let stats = platform::copy_data(
        source.as_fd(),
        destination.as_fd(),
        expected.len() as u64,
        &mut scratch,
        &Cancellation::default(),
    )
    .expect("copy exact source length");
    assert_eq!(stdfs::read(destination_path).expect("read destination"), expected);
    assert_eq!(source.stream_position().expect("read source offset"), initial_offset);
    assert_eq!(stats.logical_bytes, expected.len() as u64);
    match stats.method {
        platform::CopyMethod::Clone => assert_eq!(stats.streamed_bytes, 0),
        platform::CopyMethod::Native | platform::CopyMethod::Buffered => {
            assert_eq!(stats.streamed_bytes, expected.len() as u64)
        }
    }
}

#[test]
fn native_linux_partial_copy_range_failure_restarts_without_old_bytes() {
    let directory = TestDirectory::new();
    let source_path = directory.path().join("source");
    let destination_path = directory.path().join("destination");
    let expected = vec![0x5a; 8 * 1024 * 1024 + 131_071];
    stdfs::write(&source_path, &expected).expect("write large source");
    let source = open_source(&source_path);
    let destination = open_destination(&destination_path);
    let probe = open_destination(&directory.path().join("copy-range-probe"));
    let mut probe_input = 0;
    let mut probe_output = 0;
    match fs::copy_file_range(
        source.as_fd(),
        Some(&mut probe_input),
        probe.as_fd(),
        Some(&mut probe_output),
        1,
    ) {
        Ok(1) => eprintln!("native Linux copy_file_range is supported on the fixture filesystem"),
        Ok(0) => panic!("native Linux copy_file_range returned zero for a nonempty source"),
        Ok(_) => panic!("one-byte copy_file_range probe copied an unexpected length"),
        Err(error) if unsupported_errno(error) => {
            eprintln!("native Linux copy_file_range unavailable: {error}");
            return;
        }
        Err(error) => panic!("native Linux copy_file_range probe failed: {error}"),
    }
    platform::linux::skip_reflink_for_test(true);
    platform::linux::fail_copy_range_after_for_test(Some(1024 * 1024));
    let mut scratch = vec![0; 64 * 1024];
    let stats = copy(&source, &destination, expected.len() as u64, &mut scratch);
    let fault_triggered = platform::linux::copy_range_fault_triggered_for_test();
    platform::linux::fail_copy_range_after_for_test(None);
    platform::linux::skip_reflink_for_test(false);
    assert!(fault_triggered, "the unsupported error must follow partial native progress");
    assert_eq!(stats.method, platform::CopyMethod::Buffered);
    assert_eq!(stats.streamed_bytes, expected.len() as u64);
    assert_eq!(stdfs::read(destination_path).expect("read restarted copy"), expected);
}

#[test]
fn native_linux_sparse_copy_keeps_leading_mixed_and_trailing_holes() {
    let directory = TestDirectory::new();
    let source_path = directory.path().join("sparse-source");
    let destination_path = directory.path().join("sparse-destination");
    let length = 16 * 1024 * 1024;
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&source_path)
        .expect("create sparse source");
    writer.set_len(length).expect("set sparse source length");
    writer.seek(SeekFrom::Start(2 * 1024 * 1024)).expect("seek first extent");
    writer.write_all(&vec![0x31; 64 * 1024]).expect("write first extent");
    writer.seek(SeekFrom::Start(9 * 1024 * 1024)).expect("seek second extent");
    writer.write_all(&vec![0x92; 64 * 1024]).expect("write second extent");
    drop(writer);

    let source_metadata = stdfs::metadata(&source_path).expect("stat sparse source");
    let allocated = source_metadata.blocks() * 512;
    if allocated >= length / 2 {
        eprintln!("sparse fixture filesystem allocated most of the source; native sparse assertion unavailable");
        return;
    }

    let source = open_source(&source_path);
    let destination = open_destination(&destination_path);
    platform::linux::skip_reflink_for_test(true);
    let mut scratch = vec![0; 64 * 1024];
    let stats = copy(&source, &destination, length, &mut scratch);
    platform::linux::skip_reflink_for_test(false);
    assert_eq!(stdfs::read(&destination_path).expect("read sparse destination"), stdfs::read(&source_path).expect("read sparse source"));
    assert!(stdfs::metadata(&destination_path).expect("stat sparse destination").blocks() * 512 < length / 2);
    assert_eq!(stats.logical_bytes, length);
}

#[test]
fn native_linux_sparse_zero_scan_fallback_preserves_holes() {
    let directory = TestDirectory::new();
    let source_path = directory.path().join("sparse-source");
    let destination_path = directory.path().join("sparse-destination");
    let length = 8 * 1024 * 1024;
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&source_path)
        .expect("create sparse source");
    writer.set_len(length).expect("set sparse source length");
    writer.seek(SeekFrom::Start(1024 * 1024)).expect("seek sparse data");
    writer.write_all(b"data with surrounding holes").expect("write sparse data");
    drop(writer);
    let source_allocated = stdfs::metadata(&source_path).expect("stat source").blocks() * 512;
    if source_allocated >= length / 2 {
        eprintln!("sparse fixture filesystem allocated most of the source; zero-scan assertion unavailable");
        return;
    }

    let source = open_source(&source_path);
    let destination = open_destination(&destination_path);
    platform::linux::skip_reflink_for_test(true);
    platform::linux::make_sparse_seek_unsupported_for_test(true);
    let mut scratch = vec![0; 32 * 1024];
    let stats = copy(&source, &destination, length, &mut scratch);
    platform::linux::make_sparse_seek_unsupported_for_test(false);
    platform::linux::skip_reflink_for_test(false);
    assert_eq!(stats.method, platform::CopyMethod::Buffered);
    assert_eq!(stdfs::read(&destination_path).expect("read destination"), stdfs::read(&source_path).expect("read source"));
    assert!(stdfs::metadata(&destination_path).expect("stat destination").blocks() * 512 < length / 2);
}

#[test]
fn native_linux_sparse_zero_scan_trims_mixed_large_buffer_edges() {
    let directory = TestDirectory::new();
    let source_path = directory.path().join("sparse-source");
    let destination_path = directory.path().join("sparse-destination");
    let length = 256 * 1024_u64;
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&source_path)
        .expect("create sparse source");
    writer.set_len(length).expect("set sparse source length");
    writer.seek(SeekFrom::Start(64 * 1024)).expect("seek sparse data");
    writer.write_all(b"x").expect("write sparse data");
    drop(writer);
    let source_allocated = stdfs::metadata(&source_path).expect("stat source").blocks() * 512;
    if source_allocated >= length / 4 {
        eprintln!("sparse fixture filesystem allocated too much of the source; zero-scan assertion unavailable");
        return;
    }

    let source = open_source(&source_path);
    let destination = open_destination(&destination_path);
    platform::linux::skip_reflink_for_test(true);
    platform::linux::make_sparse_seek_unsupported_for_test(true);
    let mut scratch = vec![0; length as usize];
    let stats = copy(&source, &destination, length, &mut scratch);
    platform::linux::make_sparse_seek_unsupported_for_test(false);
    platform::linux::skip_reflink_for_test(false);

    assert_eq!(stats.method, platform::CopyMethod::Buffered);
    assert_eq!(stdfs::metadata(&destination_path).expect("stat destination").len(), length);
    assert_eq!(stdfs::read(&destination_path).expect("read destination"), stdfs::read(&source_path).expect("read source"));
    assert!(stdfs::metadata(&destination_path).expect("stat destination").blocks() * 512 < length / 4);
}

#[test]
fn native_linux_copy_rejects_short_sources_and_cancelled_work() {
    let directory = TestDirectory::new();
    let source_path = directory.path().join("source");
    let destination_path = directory.path().join("destination");
    stdfs::write(&source_path, b"short").expect("write source");
    let source = open_source(&source_path);
    let destination = open_destination(&destination_path);
    let mut scratch = [0; 4];
    let error = platform::copy_data(
        source.as_fd(),
        destination.as_fd(),
        6,
        &mut scratch,
        &Cancellation::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);

    let cancellation = Cancellation::default();
    cancellation.cancel();
    let error = platform::copy_data(
        source.as_fd(),
        destination.as_fd(),
        5,
        &mut scratch,
        &cancellation,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
}

fn acl_xattr() -> Vec<u8> {
    let entries = [
        (0x01_u16, 0x07_u16, u32::MAX),
        (0x02, 0x04, 1234),
        (0x04, 0x05, u32::MAX),
        (0x10, 0x05, u32::MAX),
        (0x20, 0x00, u32::MAX),
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

#[test]
fn native_linux_metadata_preserves_xattrs_acl_mode_and_timestamps() {
    let directory = TestDirectory::new();
    let source_path = directory.path().join("source");
    let destination_path = directory.path().join("destination");
    stdfs::write(&source_path, b"metadata payload").expect("write source");
    let source = OpenOptions::new().read(true).write(true).open(&source_path).expect("open source");
    let user_xattr = b"user.ish-fs-native";
    if !try_set_user_xattr(source.as_fd(), user_xattr, b"source-value") {
        return;
    }
    let acl = acl_xattr();
    match fs::fsetxattr(
        source.as_fd(),
        OsStr::from_bytes(b"system.posix_acl_access"),
        &acl,
        fs::XattrFlags::empty(),
    ) {
        Ok(()) => {}
        Err(error) if unsupported_errno(error) || error == Errno::PERM || error == Errno::INVAL => {
            eprintln!("native Linux POSIX access ACL unavailable on fixture filesystem: {error}");
            return;
        }
        Err(error) => panic!("set source ACL: {error}"),
    }
    stdfs::set_permissions(&source_path, stdfs::Permissions::from_mode(0o4750))
        .expect("set source mode");
    let source_observation = roots::observe_fd(source.as_fd()).expect("observe source");
    let extended = platform::capture_metadata(MetadataTarget::Fd(source.as_fd()))
        .expect("capture source metadata");
    assert_eq!(extended.acl, Some(acl.clone()));

    let destination = open_destination(&destination_path);
    assert!(try_set_user_xattr(
        destination.as_fd(),
        b"user.ish-fs-stale",
        b"remove-this",
    ));
    platform::apply_metadata(
        MetadataTarget::Fd(destination.as_fd()),
        &source_observation,
        &extended,
        Preservation::Copy,
    )
    .expect("apply copied metadata");
    let destination_observation = roots::observe_fd(destination.as_fd()).expect("observe destination");
    assert_eq!(destination_observation.mode & 0o7777, source_observation.mode & 0o7777);
    assert_eq!(destination_observation.atime, source_observation.atime);
    assert_eq!(destination_observation.mtime, source_observation.mtime);
    let mut value = [0; 64];
    let length = fs::fgetxattr(destination.as_fd(), OsStr::from_bytes(user_xattr), &mut value)
        .expect("read destination xattr");
    assert_eq!(&value[..length], b"source-value");
    assert_eq!(
        fs::fgetxattr(
            destination.as_fd(),
            OsStr::from_bytes(b"user.ish-fs-stale"),
            &mut value,
        )
        .unwrap_err(),
        Errno::NODATA,
    );
    let mut destination_acl = vec![0; acl.len()];
    let length = fs::fgetxattr(
        destination.as_fd(),
        OsStr::from_bytes(b"system.posix_acl_access"),
        &mut destination_acl,
    )
    .expect("read destination ACL");
    destination_acl.truncate(length);
    assert_eq!(destination_acl, acl);

    let mut wrong_owner = source_observation;
    wrong_owner.uid = wrong_owner.uid.wrapping_add(1);
    let restricted_path = directory.path().join("restricted");
    let restricted = open_destination(&restricted_path);
    platform::apply_metadata(
        MetadataTarget::Fd(restricted.as_fd()),
        &wrong_owner,
        &ExtendedMetadata {
            xattrs: vec![platform::Xattr {
                name: OsString::from("security.capability"),
                value: vec![1],
            }],
            acl: None,
            flags: 0,
        },
        Preservation::Copy,
    )
    .expect("apply normalized copy metadata");
    assert_eq!(roots::observe_fd(restricted.as_fd()).unwrap().mode & 0o7777, 0o0750);
    assert_eq!(
        fs::fgetxattr(
            restricted.as_fd(),
            OsStr::from_bytes(b"security.capability"),
            &mut value,
        )
        .unwrap_err(),
        Errno::NODATA,
    );
}

#[test]
fn native_linux_symlink_metadata_uses_parent_fd_and_nofollow_operations() {
    let directory = TestDirectory::new();
    let source_path = directory.path().join("source-link");
    let destination_path = directory.path().join("destination-link");
    symlink("missing-target", &source_path).expect("create source symlink");
    let parent = fs::open(
        directory.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open parent directory");
    let source_name = OsStr::new("source-link");
    match fs::lsetxattr(&source_path, OsStr::from_bytes(b"user.ish-fs-link"), b"link-value", fs::XattrFlags::empty()) {
        Ok(()) => {}
        Err(error) if unsupported_errno(error) || error == Errno::PERM => {
            eprintln!("native Linux symlink xattrs unavailable on fixture filesystem: {error}");
            return;
        }
        Err(error) => panic!("set symlink xattr: {error}"),
    }
    let source_observation = roots::observe(parent.as_fd(), source_name).expect("observe source link");
    let source_metadata = platform::capture_metadata(MetadataTarget::Symlink {
        parent: parent.as_fd(),
        name: source_name,
    })
    .expect("capture source symlink metadata");
    assert_eq!(source_metadata.xattrs.len(), 1);
    assert_eq!(source_metadata.xattrs[0].name, OsString::from("user.ish-fs-link"));
    symlink("temporary", &destination_path).expect("create destination symlink");
    platform::apply_metadata(
        MetadataTarget::Symlink {
            parent: parent.as_fd(),
            name: OsStr::new("destination-link"),
        },
        &source_observation,
        &source_metadata,
        Preservation::Copy,
    )
    .expect("apply destination symlink metadata");
    assert_eq!(stdfs::read_link(&destination_path).expect("read destination link"), PathBuf::from("temporary"));
    let destination_observation = roots::observe(parent.as_fd(), OsStr::new("destination-link"))
        .expect("observe destination link");
    assert_eq!(destination_observation.atime, source_observation.atime);
    assert_eq!(destination_observation.mtime, source_observation.mtime);
    let mut value = [0; 64];
    let length = fs::lgetxattr(
        &destination_path,
        OsStr::from_bytes(b"user.ish-fs-link"),
        &mut value,
    )
    .expect("read destination symlink xattr");
    assert_eq!(&value[..length], b"link-value");
}

#[test]
fn native_linux_finalization_rejects_unrepresentable_symlink_flags() {
    let directory = TestDirectory::new();
    let link_path = directory.path().join("link");
    symlink("target", &link_path).expect("create symlink");
    let parent = fs::open(
        directory.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open parent directory");
    let metadata = ExtendedMetadata {
        xattrs: vec![],
        acl: None,
        flags: IFlags::IMMUTABLE.bits(),
    };
    let error = platform::apply_final_metadata(
        MetadataTarget::Symlink {
            parent: parent.as_fd(),
            name: OsStr::new("link"),
        },
        &metadata,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn native_linux_mount_identity_comes_from_statx() {
    let root = File::open("/").expect("open root filesystem");
    let mount_id = platform::mount_id(root.as_fd()).expect("read root mount ID");
    assert_ne!(mount_id, 0);
    let shared_memory = match File::open("/dev/shm") {
        Ok(file) => file,
        Err(error) => {
            eprintln!("distinct mount fixture unavailable: {error}");
            return;
        }
    };
    let other_mount_id = platform::mount_id(shared_memory.as_fd()).expect("read /dev/shm mount ID");
    if mount_id == other_mount_id {
        eprintln!("no second mount identity available; /dev/shm shares root mount ID {mount_id}");
    } else {
        eprintln!("native Linux mount IDs distinguish root {mount_id} and /dev/shm {other_mount_id}");
    }
}

#[test]
fn native_linux_prepare_directory_with_umask_0777() {
    const CHILD: &str = "ISH_FS_UMASK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = Command::new(std::env::current_exe().expect("current test executable"))
            .arg("--exact")
            .arg("native_linux_prepare_directory_with_umask_0777")
            .arg("--nocapture")
            .env(CHILD, "1")
            .status()
            .expect("run isolated restrictive-umask test");
        assert!(status.success(), "restrictive-umask child failed: {status}");
        return;
    }

    let directory = TestDirectory::new();
    let parent = File::open(directory.path()).expect("open staging parent");
    let name = OsStr::new("stage");
    // SAFETY: this child process runs only this test, so changing its process umask is isolated.
    let previous_umask = unsafe { libc::umask(0o777) };
    let created = fs::mkdirat(parent.as_fd(), name, Mode::from_raw_mode(0o777));
    // SAFETY: restoring the mask returned by the immediately preceding umask call is valid.
    unsafe { libc::umask(previous_umask) };
    created.expect("create staging directory under restrictive umask");

    let initial = roots::observe(parent.as_fd(), name).expect("observe mode-zero directory");
    assert_eq!(initial.mode & 0o777, 0);
    let prepared = platform::prepare_directory(parent.as_fd(), name, initial.identity)
        .expect("prepare owned staging directory");
    assert_eq!(fs::fstat(prepared.as_fd()).expect("stat prepared directory").st_mode & 0o777, 0o700);
    fs::openat(
        prepared.as_fd(),
        "child",
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .expect("use prepared descriptor as a searchable parent");
}

#[test]
fn native_linux_prepare_directory_rejects_substituted_symlink() {
    let directory = TestDirectory::new();
    let parent = File::open(directory.path()).expect("open staging parent");
    let original = directory.path().join("stage");
    let outside = directory.path().join("outside");
    stdfs::create_dir(&original).expect("create original staging directory");
    stdfs::create_dir(&outside).expect("create outside directory");
    stdfs::set_permissions(&outside, stdfs::Permissions::from_mode(0o500))
        .expect("restrict outside directory");
    let expected = roots::observe(parent.as_fd(), OsStr::new("stage"))
        .expect("observe original staging directory")
        .identity;

    stdfs::remove_dir(&original).expect("remove original staging directory");
    symlink(&outside, &original).expect("substitute symlink to outside directory");
    assert!(
        platform::prepare_directory(parent.as_fd(), OsStr::new("stage"), expected).is_err()
    );
    assert_eq!(
        stdfs::metadata(&outside).expect("stat outside directory").permissions().mode() & 0o777,
        0o500
    );
}
