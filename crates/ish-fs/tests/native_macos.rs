#![cfg(target_os = "macos")]

use std::ffi::{CString, OsStr};
use std::fs::{self as std_fs, File};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustix::fs::{self, AtFlags, Mode, OFlags, Timestamps};

#[allow(dead_code)]
mod roots {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/roots.rs"));
}

#[derive(Default)]
struct Cancellation(std::cell::Cell<bool>);

impl Cancellation {
    fn is_cancelled(&self) -> bool {
        self.0.get()
    }

    fn cancel(&self) {
        self.0.set(true);
    }
}

#[allow(dead_code)]
mod platform {
    #[allow(dead_code, unused_imports)]
    mod shared {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/platform/mod.rs"));
    }

    pub(crate) use shared::*;

    #[allow(dead_code)]
    pub(crate) mod metadata {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/platform/metadata.rs"));
    }

    #[allow(dead_code)]
    pub(crate) mod macos {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/platform/macos.rs"));
    }
}

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("ish-fs workspace root");
        let parent = workspace.join(".work/tmp");
        std_fs::create_dir_all(&parent).expect("create fixture parent");
        loop {
            let index = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("ish-native-macos-{}-{index}", std::process::id()));
            match std_fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn parent_fd(&self) -> rustix::fd::OwnedFd {
        fs::open(
            &self.0,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open fixture directory")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std_fs::remove_dir_all(&self.0);
    }
}

struct FlagReset([i32; 2]);

impl Drop for FlagReset {
    fn drop(&mut self) {
        for descriptor in self.0 {
            // SAFETY: the reset guard is declared after both descriptors and drops before them.
            unsafe { libc::fchflags(descriptor, 0) };
        }
    }
}

#[test]
fn clone_uses_read_only_source_descriptor_and_preserves_0444_mode() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    std_fs::write(&source_path, b"clone bytes").expect("write source");
    std_fs::set_permissions(&source_path, std_fs::Permissions::from_mode(0o444))
        .expect("make source read-only");
    let source = fs::open(&source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .expect("open read-only source descriptor");
    let parent = fixture.parent_fd();

    match platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("clone")) {
        Ok(()) => {}
        Err(platform::AccelerationError::Unsupported) => {
            eprintln!("SKIP clone_uses_read_only_source_descriptor_and_preserves_0444_mode: filesystem does not support APFS clones");
            return;
        }
        Err(platform::AccelerationError::Io(error)) => panic!("clone failed: {error}"),
    }

    let clone_path = fixture.path("clone");
    assert_eq!(std_fs::read(&clone_path).expect("read cloned bytes"), b"clone bytes");
    assert_eq!(std_fs::metadata(&clone_path).unwrap().permissions().mode() & 0o7777, 0o444);
    let clone = fs::open(&clone_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .expect("open clone without write access");
    let metadata = platform::macos::capture_metadata(platform::MetadataTarget::Fd(clone.as_fd()))
        .expect("capture clone metadata from read-only descriptor");
    assert_eq!(metadata.flags & libc::UF_IMMUTABLE, 0);

    assert!(matches!(
        platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("../escape")),
        Err(platform::AccelerationError::Io(_))
    ));
}

#[test]
fn private_directory_opens_under_umask_0777() {
    const CHILD_ENV: &str = "ISH_PRIVATE_DIRECTORY_UMASK_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        let fixture = Fixture::new();
        let parent = fixture.parent_fd();
        // SAFETY: this subprocess runs only this test, so changing its process umask cannot affect other tests.
        let previous_umask = unsafe { libc::umask(0o777) };
        let result = (|| {
            fs::mkdirat(
                parent.as_fd(),
                OsStr::new("stage"),
                Mode::RUSR | Mode::WUSR | Mode::XUSR,
            )?;
            let created = roots::observe(parent.as_fd(), OsStr::new("stage"))?;
            assert_eq!(created.mode & 0o777, 0);
            let directory = platform::prepare_directory(
                parent.as_fd(),
                OsStr::new("stage"),
                created.identity,
            )?;
            assert_eq!(roots::observe_fd(directory.as_fd())?.mode & 0o777, 0o700);
            Ok::<_, io::Error>(())
        })();
        // SAFETY: restore the umask captured from this subprocess before returning.
        unsafe { libc::umask(previous_umask) };
        result.expect("open staged directory after mode was masked to 000");
        return;
    }

    let status = Command::new(std::env::current_exe().expect("test executable path"))
        .arg("--exact")
        .arg("private_directory_opens_under_umask_0777")
        .env(CHILD_ENV, "1")
        .status()
        .expect("run umask subprocess");
    assert!(status.success(), "umask subprocess failed with {status}");
}

#[test]
fn private_directory_rejects_replaced_entry_without_chmodding_it() {
    let fixture = Fixture::new();
    let parent = fixture.parent_fd();
    fs::mkdirat(
        parent.as_fd(),
        OsStr::new("stage"),
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )
    .expect("create owned staging directory");
    let expected = roots::observe(parent.as_fd(), OsStr::new("stage"))
        .expect("observe staging directory")
        .identity;
    fs::mkdirat(
        parent.as_fd(),
        OsStr::new("outside"),
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )
    .expect("create outside directory");
    let outside = fs::openat(
        parent.as_fd(),
        OsStr::new("outside"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open outside directory");
    fs::mkdirat(
        outside.as_fd(),
        OsStr::new("foreign"),
        Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
    )
    .expect("create foreign directory");
    fs::renameat(
        parent.as_fd(),
        OsStr::new("stage"),
        parent.as_fd(),
        OsStr::new("owned"),
    )
    .expect("move owned staging directory");
    fs::renameat(
        outside.as_fd(),
        OsStr::new("foreign"),
        parent.as_fd(),
        OsStr::new("stage"),
    )
    .expect("substitute foreign directory");

    assert!(platform::prepare_directory(
        parent.as_fd(),
        OsStr::new("stage"),
        expected,
    )
    .is_err());
    assert_eq!(
        roots::observe(parent.as_fd(), OsStr::new("stage"))
            .expect("observe foreign directory")
            .mode
            & 0o777,
        0o755
    );
}

#[test]
fn private_directory_clears_deny_acl_before_child_cleanup() {
    let fixture = Fixture::new();
    let parent = fixture.parent_fd();
    let stage_name = OsStr::new("stage");
    let stage_path = fixture.path("stage");
    fs::mkdirat(
        parent.as_fd(),
        stage_name,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )
    .expect("create owned staging directory");
    let staging = fs::openat(
        parent.as_fd(),
        stage_name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open staging directory before ACL restriction");
    std_fs::write(stage_path.join("child"), b"owned staging child").expect("create child");

    let acl = Command::new("/bin/chmod")
        .arg("+a")
        .arg("everyone deny delete_child,search")
        .arg(&stage_path)
        .output()
        .expect("run macOS chmod ACL command");
    assert!(acl.status.success(), "chmod ACL failed: {}", String::from_utf8_lossy(&acl.stderr));
    assert!(fs::unlinkat(staging.as_fd(), OsStr::new("child"), AtFlags::empty()).is_err());
    drop(staging);

    let expected = roots::observe(parent.as_fd(), stage_name)
        .expect("observe staging directory with deny ACL")
        .identity;
    let directory = platform::prepare_directory(parent.as_fd(), stage_name, expected)
        .expect("clear ACL and prepare private staging directory");
    fs::unlinkat(directory.as_fd(), OsStr::new("child"), AtFlags::empty())
        .expect("remove child after restrictive ACL was cleared");
}

#[test]
fn private_directory_prepares_foreign_owned_staging_directory() {
    if rustix::process::geteuid().as_raw() != 0 {
        eprintln!("SKIP private_directory_prepares_foreign_owned_staging_directory: changing ownership requires uid 0");
        return;
    }

    let fixture = Fixture::new();
    let parent = fixture.parent_fd();
    let stage_name = OsStr::new("stage");
    fs::mkdirat(
        parent.as_fd(),
        stage_name,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )
    .expect("create owned staging directory");
    let staging = fs::openat(
        parent.as_fd(),
        stage_name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open staging directory before ownership change");
    fs::openat(
        staging.as_fd(),
        "child",
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create staging child");
    fs::fchown(&staging, Some(rustix::process::Uid::from_raw(1000)), None)
        .expect("assign preserved source owner");
    let expected = roots::observe(parent.as_fd(), stage_name)
        .expect("observe foreign-owned staging directory")
        .identity;

    let directory = platform::prepare_directory(parent.as_fd(), stage_name, expected)
        .expect("prepare owned staging inode after move metadata changed its owner");
    assert_eq!(roots::observe_fd(directory.as_fd()).expect("observe prepared staging directory").uid, 1000);
    fs::unlinkat(directory.as_fd(), OsStr::new("child"), AtFlags::empty())
        .expect("clean staging child after move metadata changed its owner");
}

#[test]
fn sparse_native_copy_preserves_logical_bytes_and_trailing_hole() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let destination_path = fixture.path("destination");
    let length = 32 * 1024 * 1024_u64 + 4096;
    let source_file = File::create(&source_path).expect("create source");
    source_file.set_len(length).expect("create sparse source");
    source_file.write_at(b"leading extent", 4096).expect("write first extent");
    source_file
        .write_at(b"middle extent", 16 * 1024 * 1024)
        .expect("write second extent");
    source_file
        .write_at(b"last bytes", length - 4096)
        .expect("write final extent");
    drop(source_file);

    let source = fs::open(&source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .expect("open source");
    let destination = fs::open(
        &destination_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create destination");
    let mut scratch = vec![0_u8; 256 * 1024];
    let stats = platform::macos::copy_data(
        source.as_fd(),
        destination.as_fd(),
        length,
        &mut scratch,
        &Cancellation::default(),
    )
    .expect("copy sparse file");
    assert_eq!(stats.logical_bytes, length);
    assert!(matches!(stats.method, platform::CopyMethod::Native | platform::CopyMethod::Buffered));

    let mut source_reader = File::open(&source_path).expect("read source");
    let mut destination_reader = File::open(&destination_path).expect("read destination");
    let mut source_buffer = vec![0_u8; 64 * 1024];
    let mut destination_buffer = vec![0_u8; source_buffer.len()];
    loop {
        let source_read = source_reader.read(&mut source_buffer).expect("read source chunk");
        let destination_read = destination_reader
            .read(&mut destination_buffer)
            .expect("read destination chunk");
        assert_eq!(source_read, destination_read);
        assert_eq!(
            &source_buffer[..source_read],
            &destination_buffer[..destination_read]
        );
        if source_read == 0 {
            break;
        }
    }

    let source_metadata = std_fs::metadata(&source_path).expect("stat sparse source");
    let destination_metadata = std_fs::metadata(&destination_path).expect("stat sparse destination");
    assert_eq!(destination_metadata.len(), length);
    if matches!(stats.method, platform::CopyMethod::Native | platform::CopyMethod::Buffered) {
        assert!(
            destination_metadata.blocks() * 512 < length / 4,
            "sparse copy allocated {} bytes for {length} logical bytes",
            destination_metadata.blocks() * 512
        );
    }
    if stats.method == platform::CopyMethod::Buffered {
        assert!(stats.streamed_bytes < length);
    }
    assert!(source_metadata.blocks() < length / 512 / 4);
}

#[test]
fn metadata_round_trip_preserves_acl_xattrs_flags_and_timestamps() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let destination_path = fixture.path("destination");
    std_fs::write(&source_path, b"metadata payload").expect("write source");
    let acl = Command::new("/bin/chmod")
        .arg("+a")
        .arg("everyone allow read,readattr")
        .arg(&source_path)
        .output()
        .expect("run macOS chmod ACL command");
    assert!(acl.status.success(), "chmod ACL failed: {}", String::from_utf8_lossy(&acl.stderr));

    let source = fs::open(&source_path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .expect("open source");
    let attr_name = CString::new("com.example.ish").unwrap();
    fs::fsetxattr(source.as_fd(), attr_name.as_c_str(), b"\0xattr\xff", fs::XattrFlags::empty())
        .expect("set source xattr");
    fs::futimens(
        source.as_fd(),
        &Timestamps {
            last_access: rustix::fs::Timespec { tv_sec: 1_700_000_001, tv_nsec: 123_456_789 },
            last_modification: rustix::fs::Timespec { tv_sec: 1_700_000_002, tv_nsec: 987_654_321 },
        },
    )
    .expect("set source timestamps");
    let destination = fs::open(
        &destination_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create destination");
    let _flag_reset = FlagReset([source.as_raw_fd(), destination.as_raw_fd()]);
    let source_flags = libc::UF_NODUMP | libc::UF_IMMUTABLE;
    // SAFETY: `source` is a live descriptor accepted by fchflags.
    assert_eq!(unsafe { libc::fchflags(source.as_raw_fd(), source_flags) }, 0);
    let observation = roots::observe_fd(source.as_fd()).expect("observe source");
    let source_metadata = platform::macos::capture_metadata(platform::MetadataTarget::Fd(source.as_fd()))
        .expect("capture source metadata");
    assert!(source_metadata.acl.is_some());
    assert_eq!(source_metadata.flags, source_flags);
    platform::macos::apply_metadata(
        platform::MetadataTarget::Fd(destination.as_fd()),
        &observation,
        &source_metadata,
        platform::Preservation::Copy,
    )
    .expect("apply source metadata");
    platform::macos::apply_final_metadata(
        platform::MetadataTarget::Fd(destination.as_fd()),
        &source_metadata,
    )
    .expect("apply final file flags");

    let destination_metadata = platform::macos::capture_metadata(platform::MetadataTarget::Fd(destination.as_fd()))
        .expect("capture destination metadata");
    let destination_observation = roots::observe_fd(destination.as_fd()).expect("observe destination");
    assert_eq!(destination_metadata.xattrs.len(), source_metadata.xattrs.len());
    for (destination, source) in destination_metadata.xattrs.iter().zip(&source_metadata.xattrs) {
        assert_eq!(destination.name, source.name);
        assert_eq!(destination.value, source.value);
    }
    assert_eq!(destination_metadata.acl, source_metadata.acl);
    assert_eq!(destination_metadata.flags, source_metadata.flags);
    assert_eq!(destination_metadata.birthtime, source_metadata.birthtime);
    assert_eq!(destination_observation.atime, observation.atime);
    assert_eq!(destination_observation.mtime, observation.mtime);
}

#[test]
fn resource_fork_stream_copies_fork_without_materializing_it() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let destination_path = fixture.path("destination");
    std_fs::write(&source_path, b"data fork").expect("write source data fork");
    let source = fs::open(&source_path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .expect("open source");
    let fork_name = CString::new("com.apple.ResourceFork").unwrap();
    let fork = vec![0x6d; 32 * 1024];
    fs::fsetxattr(source.as_fd(), fork_name.as_c_str(), &fork, fs::XattrFlags::empty())
        .expect("set source resource fork");
    let source_metadata = platform::macos::capture_metadata(platform::MetadataTarget::Fd(source.as_fd()))
        .expect("capture source metadata");
    assert_eq!(source_metadata.resource_fork_size, Some(fork.len() as u64));

    let destination = fs::open(
        &destination_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create destination");
    platform::macos::copy_resource_fork(source.as_fd(), destination.as_fd())
        .expect("stream resource fork and attributes");
    let mut received = vec![0_u8; fork.len()];
    let received_length = fs::fgetxattr(destination.as_fd(), fork_name.as_c_str(), received.as_mut_slice())
        .expect("read copied resource fork");
    assert_eq!(received_length, fork.len());
    assert_eq!(received, fork);
}

#[test]
fn symlink_metadata_round_trip_uses_link_metadata() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    symlink("missing-source", fixture.path("source")).expect("create source symlink");
    symlink("missing-destination", fixture.path("destination")).expect("create destination symlink");
    let parent = fixture.parent_fd();
    fs::utimensat(
        parent.as_fd(),
        OsStr::new("source"),
        &Timestamps {
            last_access: rustix::fs::Timespec { tv_sec: 1_700_000_011, tv_nsec: 111_111_111 },
            last_modification: rustix::fs::Timespec { tv_sec: 1_700_000_022, tv_nsec: 222_222_222 },
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .expect("set source symlink timestamps");
    let source_metadata = platform::macos::capture_metadata(platform::MetadataTarget::Symlink {
        parent: parent.as_fd(),
        name: OsStr::new("source"),
    })
    .expect("capture source symlink metadata");
    let observation = roots::observe(parent.as_fd(), OsStr::new("source"))
        .expect("observe source symlink");
    platform::macos::apply_metadata(
        platform::MetadataTarget::Symlink {
            parent: parent.as_fd(),
            name: OsStr::new("destination"),
        },
        &observation,
        &source_metadata,
        platform::Preservation::Copy,
    )
    .expect("apply symlink metadata");
    let destination_observation = roots::observe(parent.as_fd(), OsStr::new("destination"))
        .expect("observe destination symlink");
    let destination_metadata = platform::macos::capture_metadata(platform::MetadataTarget::Symlink {
        parent: parent.as_fd(),
        name: OsStr::new("destination"),
    })
    .expect("capture destination symlink metadata");
    assert_eq!(destination_observation.atime, observation.atime);
    assert_eq!(destination_observation.mtime, observation.mtime);
    assert_eq!(destination_metadata.birthtime, source_metadata.birthtime);
}
