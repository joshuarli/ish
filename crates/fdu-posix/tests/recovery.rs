use std::fs;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use fdu_posix::identity::{self, EntryIdentity, EntryType, FileIdentity};
use fdu_posix::quarantine::{Boundary, Retirement, Session};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        fs::create_dir_all(&base).unwrap();
        let path = base.join(format!("quarantine-v2-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn root(&self) -> fs::File { fs::File::open(&self.0).unwrap() }
}
impl Drop for Fixture {
    fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); }
}
fn expected(path: &Path) -> EntryIdentity {
    let stat = fs::symlink_metadata(path).unwrap();
    EntryIdentity { file: FileIdentity { device: stat.dev(), inode: stat.ino() }, kind: if stat.file_type().is_symlink() { EntryType::Symlink } else { EntryType::RegularFile } }
}

#[test]
#[ignore = "subprocess fixture for journal interruption"]
fn crash_child() {
    let Some(root) = std::env::var_os("ISH32_CRASH_ROOT") else { return; };
    if std::env::var_os("ISH32_RESTRICTIVE_UMASK").is_some() {
        rustix::process::umask(rustix::fs::Mode::RWXU | rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);
    }
    let boundary = std::env::var("ISH32_CRASH_BOUNDARY").unwrap();
    let root = PathBuf::from(root);
    let fd = fs::File::open(&root).unwrap();
    let session = Session::start(fd.as_fd()).unwrap();
    let request = Retirement::new(Vec::new(), "source".into(), expected(&root.join("source"))).unwrap();
    session.remove(fd.as_fd(), request, |point, _, _| {
        if format!("{point:?}") == boundary {
            println!("ready");
            std::io::stdout().flush().unwrap();
            loop { std::thread::park(); }
        }
        Ok(())
    }).unwrap();
    panic!("crash boundary was not reached");
}

#[test]
fn killed_retirement_under_restrictive_umask_recovers_records() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("source"), b"data").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--ignored", "--nocapture"])
        .env("ISH32_CRASH_ROOT", &fixture.0)
        .env("ISH32_CRASH_BOUNDARY", "BeforeCapture")
        .env("ISH32_RESTRICTIVE_UMASK", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        assert_ne!(output.read_line(&mut line).unwrap(), 0, "child ended before publishing its intent");
        if line.trim() == "ready" { break; }
        line.clear();
    }
    let directory = fs::read_dir(&fixture.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().unwrap().to_string_lossy().starts_with(".fdu-quarantine-v2-"))
        .unwrap();
    for record in [directory.join("header"), directory.join("0.intent")] {
        assert_eq!(fs::metadata(record).unwrap().permissions().mode() & 0o777, 0o600);
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let root = fixture.root();
    let error = Session::check_recovery(root.as_fd()).unwrap_err();
    assert!(error.to_string().contains("interrupted deletion recovered"), "{error}");
    assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"data");
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    Session::check_recovery(root.as_fd()).unwrap();
}

#[test]
fn killed_retirement_restores_only_before_unlink_and_never_replays_deletion() {
    for boundary in [Boundary::IntentPublished, Boundary::BeforeCapture, Boundary::Captured, Boundary::CaptureParentSynced, Boundary::CaptureDirectorySynced, Boundary::BeforeUnlink, Boundary::Unlinked, Boundary::RemovalDirectorySynced, Boundary::BeforeParentSync, Boundary::ParentSynced, Boundary::IntentRemoved, Boundary::IntentRemovalSynced] {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("source"), b"data").unwrap();
        fs::hard_link(fixture.0.join("source"), fixture.0.join("alias")).unwrap();
        fs::write(fixture.0.join("destination"), b"published data").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap()).args(["--exact", "crash_child", "--ignored", "--nocapture"]).env("ISH32_CRASH_ROOT", &fixture.0).env("ISH32_CRASH_BOUNDARY", format!("{boundary:?}")).stdout(Stdio::piped()).spawn().unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert_ne!(output.read_line(&mut line).unwrap(), 0, "child ended before {boundary:?}");
            if line.trim() == "ready" { break; }
            line.clear();
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        let root = fixture.root();
        let result = Session::check_recovery(root.as_fd());
        assert!(result.unwrap_err().to_string().contains("interrupted deletion recovered"), "{boundary:?}");
        let removed = matches!(boundary, Boundary::Unlinked | Boundary::RemovalDirectorySynced | Boundary::BeforeParentSync | Boundary::ParentSynced | Boundary::IntentRemoved | Boundary::IntentRemovalSynced);
        assert_eq!(fixture.0.join("source").exists(), !removed, "{boundary:?}");
        assert_eq!(fs::read(fixture.0.join("alias")).unwrap(), b"data");
        assert_eq!(fs::read(fixture.0.join("destination")).unwrap(), b"published data");
        if !removed { assert_eq!(fs::metadata(fixture.0.join("source")).unwrap().ino(), fs::metadata(fixture.0.join("alias")).unwrap().ino()); }
        Session::check_recovery(root.as_fd()).unwrap();
    }
}

#[test]
fn current_records_validate_version_owner_mode_mount_and_captured_identity() {
    for mutation in ["version", "owner", "mode", "mount", "identity", "record-kind", "extra"] {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("source"), b"captured").unwrap();
        let root = fixture.root();
        let session = Session::start(root.as_fd()).unwrap();
        let directory = fixture.0.join(session.name());
        let request = Retirement::new(Vec::new(), "source".into(), expected(&fixture.0.join("source"))).unwrap();
        let error = session.remove(root.as_fd(), request, |point, _, _| {
            if point == Boundary::BeforeUnlink {
                fs::write(fixture.0.join("source"), b"live").unwrap();
                return Err(std::io::Error::other("interrupted"));
            }
            Ok(())
        }).unwrap_err();
        assert!(!error.removed);
        drop(session);
        let header = directory.join("header");
        let record = directory.join("0.intent");
        match mutation {
            "mode" => fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap(),
            "identity" => { fs::rename(directory.join("0.entry"), directory.join("held")).unwrap(); fs::write(directory.join("0.entry"), b"substitute").unwrap(); },
            "record-kind" => {
                let mut bytes = fs::read(&record).unwrap();
                let index = bytes.len() - 18;
                bytes[index] = 99;
                fs::write(&record, bytes).unwrap();
            },
            "extra" => { fs::write(directory.join("unrelated"), b"retain").unwrap(); },
            _ => {
                let mut bytes = fs::read(&header).unwrap();
                let index = match mutation { "version" => 15, "owner" => 32, "mount" => bytes.len() - 1, _ => unreachable!() };
                bytes[index] ^= 0xff;
                fs::write(&header, bytes).unwrap();
            },
        }
        let error = Session::check_recovery(root.as_fd()).unwrap_err();
        assert!(error.to_string().contains("retained"), "{mutation}: {error}");
        assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"live");
        assert!(directory.join("0.entry").exists());
    }
}

#[test]
fn lock_conflict_is_nonblocking_and_symlink_hardlinks_are_never_followed() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("target"), b"target bytes").unwrap();
    std::os::unix::fs::symlink("target", fixture.0.join("source")).unwrap();
    fs::hard_link(fixture.0.join("source"), fixture.0.join("alias")).unwrap();
    let root = fixture.root();
    let session = Session::start(root.as_fd()).unwrap();
    assert!(Session::start(root.as_fd()).err().unwrap().to_string().contains("another deletion"));
    let request = Retirement::new(Vec::new(), "source".into(), expected(&fixture.0.join("source"))).unwrap();
    session.remove(root.as_fd(), request, |_, _, _| Ok(())).unwrap();
    session.finish().unwrap();
    assert_eq!(fs::read_link(fixture.0.join("alias")).unwrap(), Path::new("target"));
    assert_eq!(fs::read(fixture.0.join("target")).unwrap(), b"target bytes");
    assert_eq!(identity::entry_type(&identity::stat_at(root.as_fd(), std::ffi::OsStr::new("alias")).unwrap()), EntryType::Symlink);
}

#[test]
fn ancestor_substitution_retains_the_capture_and_live_replacement() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("parent")).unwrap();
    fs::write(fixture.0.join("parent/source"), b"captured").unwrap();
    let parent = fs::File::open(fixture.0.join("parent")).unwrap();
    let parent_identity = identity::directory_state(parent.as_fd()).unwrap().0;
    let root = fixture.root();
    let session = Session::start(root.as_fd()).unwrap();
    let directory = fixture.0.join(session.name());
    let request = Retirement::new(vec![fdu_posix::quarantine::Ancestor { name: "parent".into(), identity: parent_identity }], "source".into(), expected(&fixture.0.join("parent/source"))).unwrap();
    session.remove(parent.as_fd(), request, |point, _, _| {
        if point == Boundary::BeforeUnlink {
            fs::rename(fixture.0.join("parent"), fixture.0.join("original-parent")).unwrap();
            fs::create_dir(fixture.0.join("parent")).unwrap();
            fs::write(fixture.0.join("parent/source"), b"replacement").unwrap();
            return Err(std::io::Error::other("parent substituted"));
        }
        Ok(())
    }).unwrap_err();
    drop(session);
    let result = Session::check_recovery(root.as_fd()).unwrap_err();
    assert!(result.to_string().contains("parent identity or mount changed"));
    assert_eq!(fs::read(fixture.0.join("parent/source")).unwrap(), b"replacement");
    assert_eq!(fs::read(directory.join("0.entry")).unwrap(), b"captured");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs ISH_FS_TEST_ALIAS_ORIGIN and ISH_FS_TEST_ALIAS_BIND from native-fixtures.sh"]
fn recorded_mount_identity_rejects_recovery_through_a_bind_mount() {
    let origin = PathBuf::from(std::env::var_os("ISH_FS_TEST_ALIAS_ORIGIN").expect("bind origin fixture"));
    let alias = PathBuf::from(std::env::var_os("ISH_FS_TEST_ALIAS_BIND").expect("bind alias fixture"));
    let name = format!("ish32-mount-{}", std::process::id());
    let path = origin.join(&name);
    fs::create_dir(&path).unwrap();
    let fixture = Fixture(path);
    let root = fixture.root();
    fs::write(fixture.0.join("source"), b"captured").unwrap();
    let session = Session::start(root.as_fd()).unwrap();
    let directory = fixture.0.join(session.name());
    let request = Retirement::new(Vec::new(), "source".into(), expected(&fixture.0.join("source"))).unwrap();
    session.remove(root.as_fd(), request, |point, _, _| {
        if point == Boundary::BeforeUnlink {
            fs::write(fixture.0.join("source"), b"live").unwrap();
            return Err(std::io::Error::other("interrupted"));
        }
        Ok(())
    }).unwrap_err();
    drop(session);
    let bound = fs::File::open(alias.join(name)).unwrap();
    assert_eq!(identity::directory_state(root.as_fd()).unwrap().0, identity::directory_state(bound.as_fd()).unwrap().0);
    assert_ne!(identity::mount_identity(root.as_fd()).unwrap(), identity::mount_identity(bound.as_fd()).unwrap());
    let error = Session::check_recovery(bound.as_fd()).unwrap_err();
    assert!(error.to_string().contains("mount identity changed"), "{error}");
    assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"live");
    assert_eq!(fs::read(directory.join("0.entry")).unwrap(), b"captured");
}

#[test]
fn restored_pre_unlink_error_does_not_claim_a_retained_capture() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("source"), b"retained at original name").unwrap();
    let root = fixture.root();
    let session = Session::start(root.as_fd()).unwrap();
    let request = Retirement::new(Vec::new(), "source".into(), expected(&fixture.0.join("source"))).unwrap();
    let error = session.remove(root.as_fd(), request, |boundary, _, _| {
        if boundary == Boundary::BeforeUnlink { return Err(std::io::Error::other("authorization failed")); }
        Ok(())
    }).unwrap_err();
    assert!(!error.removed);
    assert_eq!(error.to_string(), "authorization failed");
    assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"retained at original name");
    assert_eq!(fs::read_dir(fixture.0.join(session.name())).unwrap().count(), 1);
    session.finish().unwrap();
}

#[test]
fn inherited_pre_exec_descriptor_does_not_extend_a_finished_session_lock() {
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::new();
    let root = fixture.root();
    let session = Session::start(root.as_fd()).unwrap();
    session.finish().unwrap();
    let (mut parent, child) = UnixStream::pair().unwrap();
    parent.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    let process = std::thread::spawn(move || {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "crash_child", "--ignored", "--nocapture"]).stdout(Stdio::null());
        // SAFETY: the child hook performs only descriptor reads and writes before exec.
        unsafe {
            command.pre_exec(move || {
                rustix::io::write(child.as_fd(), b"ready")?;
                let mut release = [0u8; 1];
                rustix::io::read(child.as_fd(), &mut release)?;
                Ok(())
            });
        }
        let mut process = command.spawn().unwrap();
        process.wait().unwrap()
    });
    let mut ready = [0u8; 5];
    parent.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    drop(session);
    let replacement = Session::start(root.as_fd());
    parent.write_all(b"x").unwrap();
    assert!(process.join().unwrap().success());
    let replacement = replacement.unwrap();
    assert!(Session::start(root.as_fd()).is_err());
    replacement.finish().unwrap();
}
