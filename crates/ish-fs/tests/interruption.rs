use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ish_fs::{Cancellation, Context, Event, Jobs, Operands, Operation, Options, Request, execute};
use rustix::fs::{self as rfs, Mode, OFlags};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
const FILES: u32 = 64;
const BYTES_PER_FILE: usize = 512 * 1024;
const MARKER_NAME: &str = ".owned-copy-marker";
const MARKER_CONTENT: &[u8] = b"ish-fs-private-stage-fixture-v1\0";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let work = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        fs::create_dir_all(&work).unwrap();
        let path = work.join(format!("ish-fs-interrupt-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("source")).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn create_file_at(parent: BorrowedFd<'_>, name: &str, bytes: &[u8]) {
    let fd = rfs::openat(parent, name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR).unwrap();
    File::from(fd).write_all(bytes).unwrap();
}

fn payload(index: u32) -> Vec<u8> {
    let mut bytes = vec![0; BYTES_PER_FILE];
    let seed = index.to_le_bytes();
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = seed[offset % seed.len()] ^ (offset as u8).wrapping_mul(31);
    }
    bytes
}

fn build_source(root: &Path) {
    let source = rfs::open(root.join("source"), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    create_file_at(source.as_fd(), MARKER_NAME, MARKER_CONTENT);
    for index in 0..FILES {
        create_file_at(source.as_fd(), &format!("payload-{index:03}"), &payload(index));
    }
}

fn request(root: &Path) -> Request {
    Request {
        operation: Operation::Copy,
        operands: Operands::Exact { source: "source".into(), destination: "destination".into() },
        options: Options { jobs: Some(Jobs::new(1).unwrap()), ..Options::default() },
        context: Context { cwd: root.to_path_buf(), home: None, scratch: root.to_path_buf() },
    }
}

fn read_at(parent: BorrowedFd<'_>, name: &str) -> Vec<u8> {
    let fd = rfs::openat(parent, name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    let mut bytes = Vec::new();
    File::from(fd).read_to_end(&mut bytes).unwrap();
    bytes
}

fn verify_complete_copy(root: &Path) {
    let source = rfs::open(root.join("source"), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    let destination = rfs::open(root.join("destination"), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    assert_eq!(read_at(destination.as_fd(), MARKER_NAME), MARKER_CONTENT);
    for index in 0..FILES {
        let name = format!("payload-{index:03}");
        let source_bytes = read_at(source.as_fd(), &name);
        let destination_bytes = read_at(destination.as_fd(), &name);
        assert_eq!(destination_bytes, source_bytes, "{name}");
    }
}

fn staged_copy(root: &Path) -> Option<(PathBuf, PathBuf, Vec<u8>)> {
    let prefix = ".fdu-quarantine-v2-";
    let stages = fs::read_dir(root).ok()?;
    for entry in stages.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(prefix) { continue; }
        let stage = entry.path().join("0.entry");
        let stage_entries = fs::read_dir(&stage).ok()?;
        for child in stage_entries.flatten() {
            let metadata = fs::symlink_metadata(child.path()).ok()?;
            if metadata.file_type().is_file() && metadata.len() > 0 {
                let bytes = fs::read(child.path()).ok()?;
                if !bytes.is_empty() { return Some((stage, child.path(), bytes)); }
            }
        }
    }
    None
}

fn start_child(root: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_copy_for_sigkill", "--ignored", "--nocapture"])
        .env("ISH_FS_KILL_FIXTURE", root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn killing_an_inflight_copy_never_exposes_a_partial_final_file() {
    let fixture = Fixture::new();
    build_source(&fixture.0);
    let mut child = start_child(&fixture.0);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut readiness = None;
    while Instant::now() < deadline {
        if let Some(staged) = staged_copy(&fixture.0) {
            readiness = Some(staged);
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "copy child exited before staging readiness: {status}");
            verify_complete_copy(&fixture.0);
            panic!("copy completed before the parent observed an in-flight staged file");
        }
        std::thread::yield_now();
    }
    let Some((stage, staged_file, staged_bytes)) = readiness else {
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("no nonempty staged file appeared within 20 seconds");
    };
    assert!(child.try_wait().unwrap().is_none(), "copy exited after staging readiness and before SIGKILL");
    let source_name = staged_file.file_name().unwrap();
    let source_bytes = fs::read(fixture.0.join("source").join(source_name)).unwrap();
    assert!(source_bytes.starts_with(&staged_bytes), "staging bytes do not match the source prefix");

    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    let source = rfs::open(fixture.0.join("source"), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    assert_eq!(read_at(source.as_fd(), MARKER_NAME), MARKER_CONTENT);
    for index in 0..FILES {
        let name = format!("payload-{index:03}");
        assert_eq!(read_at(source.as_fd(), &name), payload(index), "source {name} was not retained");
    }
    assert_eq!(read_at(source.as_fd(), source_name.to_str().unwrap()), source_bytes);
    if fixture.0.join("destination").exists() {
        verify_complete_copy(&fixture.0);
    }

    let metadata = fs::symlink_metadata(&stage).unwrap();
    assert!(metadata.file_type().is_dir());
    assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(metadata.mode() & 0o777, 0o700);
    for entry in fs::read_dir(&stage).unwrap() {
        let entry = entry.unwrap();
        let entry_metadata = fs::symlink_metadata(entry.path()).unwrap();
        assert!(entry_metadata.file_type().is_file(), "unexpected staged object: {:?}", entry.file_name());
        assert_eq!(entry_metadata.uid(), rustix::process::geteuid().as_raw());
        if entry.file_name() == MARKER_NAME {
            assert_eq!(fs::read(entry.path()).unwrap(), MARKER_CONTENT);
        }
    }
}

#[test]
#[ignore = "subprocess fixture for process interruption"]
fn child_copy_for_sigkill() {
    let Some(root) = std::env::var_os("ISH_FS_KILL_FIXTURE") else { return; };
    let root = PathBuf::from(root);
    assert_eq!(execute(&request(&root), &Cancellation::default(), &|_: Event| Ok(())).status(), 0);
}
