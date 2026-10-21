#![cfg(target_os = "macos")]

use std::ffi::{CString, OsStr};
use std::fs::{self as std_fs, File};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rustix::fs::{self, AtFlags, Mode, OFlags, Timestamps};

#[allow(dead_code)]
mod roots {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/roots.rs"));
}

#[derive(Default)]
struct Cancellation(AtomicBool);

impl Cancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
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

    let clone = match platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("clone")) {
        Ok(Some(clone)) => clone,
        Ok(None) => {
            eprintln!("SKIP clone_uses_read_only_source_descriptor_and_preserves_0444_mode: filesystem does not support APFS clones");
            return;
        }
        Err(error) => panic!("clone failed: {error}"),
    };

    let clone_path = fixture.path("clone");
    assert_eq!(std_fs::read(&clone_path).expect("read cloned bytes"), b"clone bytes");
    assert_eq!(std_fs::metadata(&source_path).unwrap().permissions().mode() & 0o7777, 0o444);
    assert_eq!(std_fs::metadata(&clone_path).unwrap().permissions().mode() & 0o700, 0o700);
    assert_eq!(roots::observe_fd(clone.descriptor.as_fd()).unwrap().identity, clone.identity);
    let metadata = platform::macos::capture_metadata(platform::MetadataTarget::Fd(clone.descriptor.as_fd()))
        .expect("capture clone metadata from read-only descriptor");
    assert_eq!(metadata.flags & libc::UF_IMMUTABLE, 0);

    assert!(matches!(
        platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("../escape")),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput
    ));
}

#[test]
fn clone_clears_inherited_blocking_flags_from_private_clone() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    std_fs::write(&source_path, b"immutable source bytes").expect("write source");
    let source = fs::open(&source_path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .expect("open source");
    let _flag_reset = FlagReset([source.as_raw_fd(), source.as_raw_fd()]);
    // SAFETY: `source` is a live descriptor accepted by fchflags.
    assert_eq!(unsafe { libc::fchflags(source.as_raw_fd(), libc::UF_IMMUTABLE) }, 0);
    let parent = fixture.parent_fd();
    let clone = match platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("clone")) {
        Ok(Some(clone)) => clone,
        Ok(None) => {
            eprintln!("SKIP clone_clears_inherited_blocking_flags_from_private_clone: filesystem does not support APFS clones");
            return;
        }
        Err(error) => panic!("clone failed: {error}"),
    };
    let _clone_flag_reset = FlagReset([source.as_raw_fd(), clone.descriptor.as_raw_fd()]);
    assert_ne!(fs::fstat(source.as_fd()).unwrap().st_flags & libc::UF_IMMUTABLE, 0);
    let blocking_flags = libc::UF_IMMUTABLE | libc::SF_IMMUTABLE | libc::UF_APPEND | libc::SF_APPEND;
    assert_eq!(fs::fstat(clone.descriptor.as_fd()).unwrap().st_flags & blocking_flags, 0);
    let mut received = [0_u8; 22];
    let read = rustix::io::pread(clone.descriptor.as_fd(), &mut received, 0).expect("read normalized clone");
    assert_eq!(&received[..read], b"immutable source bytes");
}

#[test]
fn clone_does_not_normalize_an_existing_read_only_destination() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let destination_path = fixture.path("destination");
    std_fs::write(&source_path, b"source bytes").expect("write source");
    std_fs::write(&destination_path, b"destination bytes").expect("write destination");
    std_fs::set_permissions(&destination_path, std_fs::Permissions::from_mode(0o044))
        .expect("make destination readable by group and others only");
    let source = fs::open(&source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .expect("open source");
    let parent = fixture.parent_fd();
    let result = platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("destination"));
    match result {
        Ok(None) => eprintln!("SKIP clone_does_not_normalize_an_existing_read_only_destination: filesystem does not support APFS clones"),
        Ok(Some(_)) => panic!("clone replaced an existing destination"),
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::AlreadyExists),
    }
    assert_eq!(std_fs::read(&destination_path).expect("read destination bytes"), b"destination bytes");
    assert_eq!(std_fs::metadata(&destination_path).unwrap().permissions().mode() & 0o7777, 0o044);
}

#[test]
fn clone_normalizes_foreign_0044_and_inherited_deny_read_before_open() {
    const CHILD_ENV: &str = "ISH_CLONE_ACCESS_CHILD";
    const SOURCE_ENV: &str = "ISH_CLONE_ACCESS_SOURCE";
    const STAGING_ENV: &str = "ISH_CLONE_ACCESS_STAGING";
    if let Some(source_path) = std::env::var_os(SOURCE_ENV) {
        let staging_path = std::env::var_os(STAGING_ENV).expect("staging path for clone child");
        if std::env::var_os(CHILD_ENV).is_some() && unsafe { libc::geteuid() } == 0 {
            // SAFETY: this subprocess is dedicated to this test and drops its supplementary groups and ids before touching fixtures.
            assert_eq!(unsafe { libc::setgroups(0, std::ptr::null()) }, 0);
            assert_eq!(unsafe { libc::setgid(501) }, 0);
            assert_eq!(unsafe { libc::setuid(501) }, 0);
        }
        assert_clone_opens_after_normalization(Path::new(&source_path), Path::new(&staging_path));
        return;
    }

    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let staging_path = fixture.path("stage");
    std_fs::write(&source_path, b"foreign readable source").expect("write source");
    std_fs::set_permissions(&source_path, std_fs::Permissions::from_mode(0o044))
        .expect("make source readable by group and others only");
    std_fs::create_dir(&staging_path).expect("create staging directory");
    std_fs::set_permissions(&staging_path, std_fs::Permissions::from_mode(0o700))
        .expect("make staging directory private");

    for (path, acl) in [
        (&source_path, "everyone allow read,readattr"),
        (&staging_path, "everyone deny read,file_inherit,only_inherit"),
    ] {
        let output = Command::new("/bin/chmod")
            .arg("+a")
            .arg(acl)
            .arg(path)
            .output()
            .expect("run macOS chmod ACL command");
        assert!(output.status.success(), "chmod ACL failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    if unsafe { libc::geteuid() } == 0 {
        let source = fs::open(&source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .expect("open source before changing its owner");
        fs::fchown(&source, Some(rustix::process::Uid::from_raw(0)), Some(rustix::process::Gid::from_raw(0)))
            .expect("make source foreign to the clone caller");
        let staging = fs::open(
            &staging_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open staging directory before changing its owner");
        fs::fchown(&staging, Some(rustix::process::Uid::from_raw(501)), Some(rustix::process::Gid::from_raw(501)))
            .expect("give the unprivileged clone caller a private staging directory");
        fs::fchmod(&staging, Mode::from_bits_truncate(0o700)).expect("keep staging directory private");
        let output = Command::new(std::env::current_exe().expect("test executable path"))
            .arg("--exact")
            .arg("clone_normalizes_foreign_0044_and_inherited_deny_read_before_open")
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .env(SOURCE_ENV, &source_path)
            .env(STAGING_ENV, &staging_path)
            .output()
            .expect("run clone permissions subprocess without root privileges");
        assert!(output.status.success(), "clone permissions subprocess failed: {}", String::from_utf8_lossy(&output.stderr));
        let child_output = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        if child_output.contains("SKIP") {
            eprintln!("{child_output}");
        }
    } else {
        assert_clone_opens_after_normalization(&source_path, &staging_path);
    }
}

#[test]
fn clone_uses_acl_read_grant_when_source_mode_denies_owner() {
    const CHILD_ENV: &str = "ISH_CLONE_ACL_CHILD";
    const SOURCE_ENV: &str = "ISH_CLONE_ACL_SOURCE";
    const STAGING_ENV: &str = "ISH_CLONE_ACL_STAGING";
    if let Some(source_path) = std::env::var_os(SOURCE_ENV) {
        let staging_path = std::env::var_os(STAGING_ENV).expect("staging path for ACL child");
        if std::env::var_os(CHILD_ENV).is_some() && unsafe { libc::geteuid() } == 0 {
            // SAFETY: this subprocess is dedicated to this test and drops its supplementary groups and ids before touching fixtures.
            assert_eq!(unsafe { libc::setgroups(0, std::ptr::null()) }, 0);
            assert_eq!(unsafe { libc::setgid(501) }, 0);
            assert_eq!(unsafe { libc::setuid(501) }, 0);
        }
        assert_ne!(unsafe { libc::geteuid() }, 0, "ACL source access must be checked without root privileges");
        let source_path = Path::new(&source_path);
        let staging_path = Path::new(&staging_path);
        let source = fs::open(source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .expect("open source using its ACL read grant");
        let source_stat = fs::fstat(source.as_fd()).unwrap();
        assert_eq!(source_stat.st_mode as u32 & 0o777, 0);
        if std::env::var_os(CHILD_ENV).is_some() {
            assert_ne!(source_stat.st_uid, unsafe { libc::geteuid() as u32 });
        }
        let parent = fs::open(
            staging_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open staging directory");
        let clone = match platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("clone")) {
            Ok(Some(clone)) => clone,
            Ok(None) => {
                eprintln!("SKIP clone_uses_acl_read_grant_when_source_mode_denies_owner: filesystem does not support APFS clones");
                return;
            }
            Err(error) => panic!("clone failed using source ACL read grant: {error}"),
        };
        let mut received = [0_u8; 20];
        let read = rustix::io::pread(clone.descriptor.as_fd(), &mut received, 0)
            .expect("read clone opened after normalization");
        assert_eq!(&received[..read], b"ACL readable source");
        return;
    }

    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let staging_path = fixture.path("stage");
    std_fs::write(&source_path, b"ACL readable source").expect("write source");
    std_fs::set_permissions(&source_path, std_fs::Permissions::from_mode(0o000))
        .expect("remove all mode read permissions");
    add_acl(&source_path, "everyone allow read,readattr");
    std_fs::create_dir(&staging_path).expect("create staging directory");
    std_fs::set_permissions(&staging_path, std_fs::Permissions::from_mode(0o700))
        .expect("make staging directory private");

    if unsafe { libc::geteuid() } == 0 {
        let source = fs::open(&source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .expect("open source before changing its owner");
        fs::fchown(&source, Some(rustix::process::Uid::from_raw(0)), Some(rustix::process::Gid::from_raw(0)))
            .expect("make source foreign to the clone caller");
        let staging = fs::open(
            &staging_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open staging directory before changing its owner");
        fs::fchown(&staging, Some(rustix::process::Uid::from_raw(501)), Some(rustix::process::Gid::from_raw(501)))
            .expect("give the unprivileged clone caller a private staging directory");
        fs::fchmod(&staging, Mode::from_bits_truncate(0o700)).expect("keep staging directory private");
        let output = Command::new(std::env::current_exe().expect("test executable path"))
            .arg("--exact")
            .arg("clone_uses_acl_read_grant_when_source_mode_denies_owner")
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .env(SOURCE_ENV, &source_path)
            .env(STAGING_ENV, &staging_path)
            .output()
            .expect("run ACL source test without root privileges");
        assert!(output.status.success(), "ACL source subprocess failed: {}", String::from_utf8_lossy(&output.stderr));
        let child_output = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        if child_output.contains("SKIP") {
            eprintln!("{child_output}");
        }
    } else {
        let source = fs::open(&source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .expect("open source using its ACL read grant");
        assert_eq!(fs::fstat(source.as_fd()).unwrap().st_mode as u32 & 0o777, 0);
        let parent = fixture.parent_fd();
        let clone = match platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("clone")) {
            Ok(Some(clone)) => clone,
            Ok(None) => {
                eprintln!("SKIP clone_uses_acl_read_grant_when_source_mode_denies_owner: filesystem does not support APFS clones");
                return;
            }
            Err(error) => panic!("clone failed using source ACL read grant: {error}"),
        };
        let mut received = [0_u8; 20];
        let read = rustix::io::pread(clone.descriptor.as_fd(), &mut received, 0)
            .expect("read clone opened after normalization");
        assert_eq!(&received[..read], b"ACL readable source");
    }
}

fn assert_clone_opens_after_normalization(source_path: &Path, staging_path: &Path) {
    let source = fs::open(source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .expect("open ACL-readable source");
    if std::env::var_os("ISH_CLONE_ACCESS_CHILD").is_some() {
        assert_ne!(unsafe { libc::geteuid() }, 0, "clone access must be checked without root privileges");
        assert_ne!(roots::observe_fd(source.as_fd()).unwrap().uid, unsafe { libc::geteuid() as u32 });
    }
    let parent = fs::open(
        staging_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open staging directory");
    let clone = match platform::macos::try_clone_to_dir(source.as_fd(), parent.as_fd(), OsStr::new("clone")) {
        Ok(Some(clone)) => clone,
        Ok(None) => {
            eprintln!("SKIP clone_normalizes_foreign_0044_and_inherited_deny_read_before_open: filesystem does not support APFS clones");
            return;
        }
        Err(error) => panic!("clone failed before read-only open: {error}"),
    };
    assert_eq!(clone.identity, roots::observe_fd(clone.descriptor.as_fd()).unwrap().identity);
    assert_eq!(roots::observe_fd(clone.descriptor.as_fd()).unwrap().mode & 0o700, 0o700);
    let mut bytes = [0_u8; 32];
    let read = rustix::io::pread(clone.descriptor.as_fd(), &mut bytes, 0).expect("read normalized clone");
    assert_eq!(&bytes[..read], b"foreign readable source");
    assert_eq!(std_fs::metadata(source_path).unwrap().permissions().mode() & 0o777, 0o044);
}

fn add_acl(path: &Path, acl: &str) {
    let output = Command::new("/bin/chmod")
        .arg("+a")
        .arg(acl)
        .arg(path)
        .output()
        .expect("run macOS chmod ACL command");
    assert!(output.status.success(), "chmod ACL failed: {}", String::from_utf8_lossy(&output.stderr));
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
        assert_eq!(stats.streamed_bytes, length);
    }
    assert!(source_metadata.blocks() < length / 512 / 4);
}

#[test]
fn sparse_buffered_fallback_reports_read_bytes_and_preserves_exact_layout() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let destination_path = fixture.path("destination");
    let length = 16 * 1024 * 1024_u64 + 4096;
    let source_file = File::create(&source_path).expect("create sparse source");
    source_file.set_len(length).expect("set sparse source length");
    source_file.write_at(b"first extent", 4096).expect("write first extent");
    source_file.write_at(b"last extent", length - 2048).expect("write trailing extent");
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
    let stats = platform::macos::copy_data_with_native_copy(
        source.as_fd(),
        destination.as_fd(),
        length,
        &mut scratch,
        &Cancellation::default(),
        None,
        |_| Err(rustix::io::Errno::NOTSUP.into()),
    )
    .expect("copy through forced sparse fallback");
    assert_eq!(stats.method, platform::CopyMethod::Buffered);
    assert_eq!(stats.logical_bytes, length);
    assert_eq!(stats.streamed_bytes, length);

    let mut source_reader = File::open(&source_path).expect("read source");
    let mut destination_reader = File::open(&destination_path).expect("read destination");
    let mut source_buffer = vec![0_u8; 64 * 1024];
    let mut destination_buffer = vec![0_u8; source_buffer.len()];
    loop {
        let source_read = source_reader.read(&mut source_buffer).expect("read source chunk");
        let destination_read = destination_reader.read(&mut destination_buffer).expect("read destination chunk");
        assert_eq!(source_read, destination_read);
        assert_eq!(&source_buffer[..source_read], &destination_buffer[..destination_read]);
        if source_read == 0 {
            break;
        }
    }

    let source_metadata = std_fs::metadata(&source_path).expect("stat source allocation");
    let destination_metadata = std_fs::metadata(&destination_path).expect("stat destination allocation");
    assert_eq!(source_metadata.len(), length);
    assert_eq!(destination_metadata.len(), length);
    assert!(source_metadata.blocks() * 512 < length / 4);
    assert!(destination_metadata.blocks() * 512 < length / 4);
}

#[test]
fn native_data_copy_cancellation_stops_at_progress_callback() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let destination_path = fixture.path("destination");
    let source_bytes = vec![0x5a; 8 * 1024 * 1024];
    std_fs::write(&source_path, &source_bytes).expect("write source");
    let source = fs::open(&source_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .expect("open source");
    let destination = fs::open(
        &destination_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create destination");
    let cancellation = Cancellation::default();
    let progress_events = AtomicUsize::new(0);
    let cancel_on_progress = || {
        progress_events.fetch_add(1, Ordering::Relaxed);
        cancellation.cancel();
    };
    let mut scratch = vec![0_u8; 256 * 1024];
    let error = platform::macos::copy_data_with_native_copy(
        source.as_fd(),
        destination.as_fd(),
        source_bytes.len() as u64,
        &mut scratch,
        &cancellation,
        Some(&cancel_on_progress),
        |hook| platform::macos::native_data_copy(source.as_fd(), destination.as_fd(), &cancellation, hook),
    )
    .expect_err("native copy must stop after callback cancellation");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(cancellation.is_cancelled());
    assert!(progress_events.load(Ordering::Relaxed) > 0);
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
    let mut scratch = vec![0_u8; 256 * 1024];
    platform::macos::copy_resource_fork(
        source.as_fd(),
        destination.as_fd(),
        &mut scratch,
        &Cancellation::default(),
    )
        .expect("stream resource fork and attributes");
    let mut received = vec![0_u8; fork.len()];
    let received_length = fs::fgetxattr(destination.as_fd(), fork_name.as_c_str(), received.as_mut_slice())
        .expect("read copied resource fork");
    assert_eq!(received_length, fork.len());
    assert_eq!(received, fork);
}

#[test]
fn resource_fork_precancellation_leaves_source_and_target_unchanged() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let destination_path = fixture.path("destination");
    std_fs::write(&source_path, b"source data fork").expect("write source");
    std_fs::write(&destination_path, b"existing destination").expect("write destination");
    std_fs::set_permissions(&destination_path, std_fs::Permissions::from_mode(0o640))
        .expect("set destination mode");
    let source = fs::open(&source_path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .expect("open source");
    let destination = fs::open(&destination_path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .expect("open destination");
    let fork_name = CString::new("com.apple.ResourceFork").unwrap();
    let fork = b"source resource fork";
    match fs::fsetxattr(source.as_fd(), fork_name.as_c_str(), fork, fs::XattrFlags::empty()) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == libc::ENOTSUP => {
            eprintln!("SKIP resource_fork_precancellation_leaves_source_and_target_unchanged: resource forks are unsupported on this filesystem");
            return;
        }
        Err(error) => panic!("set source resource fork: {error}"),
    }
    let before = std_fs::metadata(&destination_path).expect("stat destination before cancellation");
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let mut scratch = vec![0_u8; 256 * 1024];

    let error = platform::macos::copy_resource_fork(
        source.as_fd(),
        destination.as_fd(),
        &mut scratch,
        &cancellation,
    )
    .expect_err("pre-cancelled resource fork copy must stop before target changes");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(std_fs::read(&destination_path).expect("read destination"), b"existing destination");
    let after = std_fs::metadata(&destination_path).expect("stat destination after cancellation");
    assert_eq!(after.len(), before.len());
    assert_eq!(after.permissions().mode() & 0o7777, before.permissions().mode() & 0o7777);
    assert_eq!(after.mtime(), before.mtime());
    assert_eq!(after.mtime_nsec(), before.mtime_nsec());
    let destination_metadata = platform::macos::capture_metadata(platform::MetadataTarget::Fd(destination.as_fd()))
        .expect("capture destination after cancellation");
    assert_eq!(destination_metadata.resource_fork_size, None);
    let mut source_fork = [0_u8; 32];
    let source_fork_length = fs::fgetxattr(source.as_fd(), fork_name.as_c_str(), &mut source_fork)
        .expect("read original source resource fork");
    assert_eq!(source_fork_length, fork.len());
    assert_eq!(&source_fork[..source_fork_length], fork);
}

#[test]
fn resource_fork_copy_cancellation_stops_at_progress_callback() {
    let fixture = Fixture::new();
    let source_path = fixture.path("source");
    let destination_path = fixture.path("destination");
    std_fs::write(&source_path, b"data fork").expect("write source");
    let source = fs::open(&source_path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .expect("open source");
    let destination = fs::open(
        &destination_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create destination");
    let fork_name = CString::new("com.apple.ResourceFork").unwrap();
    let fork = vec![0x6d; 8 * 1024 * 1024];
    match fs::fsetxattr(source.as_fd(), fork_name.as_c_str(), &fork, fs::XattrFlags::empty()) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == libc::ENOTSUP => {
            eprintln!("SKIP resource_fork_copy_cancellation_stops_at_progress_callback: resource forks are unsupported on this filesystem");
            return;
        }
        Err(error) => panic!("set source resource fork: {error}"),
    }
    let cancellation = Cancellation::default();
    let progress_events = AtomicUsize::new(0);
    let cancel_on_progress = || {
        progress_events.fetch_add(1, Ordering::Relaxed);
        cancellation.cancel();
    };
    let mut scratch = vec![0_u8; 256 * 1024];
    let error = platform::macos::copy_resource_fork_inner(
        source.as_fd(),
        destination.as_fd(),
        &mut scratch,
        &cancellation,
        Some(&cancel_on_progress),
    )
    .expect_err("resource fork copy must stop after callback cancellation");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(cancellation.is_cancelled());
    assert!(progress_events.load(Ordering::Relaxed) > 0);
    let metadata = platform::macos::capture_metadata(platform::MetadataTarget::Fd(destination.as_fd()))
        .expect("capture partially copied resource fork");
    assert_eq!(metadata.resource_fork_size, Some(scratch.len() as u64));
    let mut received = [0_u8; 32];
    let read = fs::fgetxattr(destination.as_fd(), fork_name.as_c_str(), &mut received)
        .expect("read first copied resource fork bytes");
    assert_eq!(read, received.len());
    assert_eq!(received, [0x6d; 32]);
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
