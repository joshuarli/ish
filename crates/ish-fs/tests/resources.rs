use std::fs::{self, File};
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use ish_fs::{Cancellation, Context, Event, Operands, Operation, Options, Request, execute};
#[cfg(target_os = "linux")]
use ish_fs::Jobs;
use rustix::fs::{self as rfs, Mode, OFlags};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
const WIDE_FILES: u32 = 768;
const WORK_NAME_BYTES: usize = 64;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let work = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        fs::create_dir_all(&work).unwrap();
        let path = work.join(format!("{name}-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn request(&self, options: Options) -> Request {
        Request {
            operation: Operation::Copy,
            operands: Operands::Exact { source: "source".into(), destination: "destination".into() },
            options,
            context: Context { cwd: self.0.clone(), home: None, scratch: self.0.clone() },
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
        let baseline = self.0.with_extension("descriptor-baseline");
        if baseline.exists() { fs::remove_file(baseline).unwrap(); }
    }
}

fn open_directory_at(parent: BorrowedFd<'_>, name: &str) -> OwnedFd {
    rfs::openat(parent, name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap()
}

fn create_directory_at(parent: BorrowedFd<'_>, name: &str) -> OwnedFd {
    rfs::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR).unwrap();
    open_directory_at(parent, name)
}

fn create_file_at(parent: BorrowedFd<'_>, name: &str, bytes: &[u8]) {
    let fd = rfs::openat(parent, name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR).unwrap();
    File::from(fd).write_all(bytes).unwrap();
}

fn fixture_tree(root: &Path, wide_files: u32) {
    fs::create_dir(root.join("source")).unwrap();
    let source = rfs::open(root.join("source"), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    for index in 0..wide_files {
        let name = format!("wide-{index:05}");
        create_file_at(source.as_fd(), &name, &wide_payload(index));
    }

    let mut directory = source;
    let mut path_bytes = 0;
    let mut index = 0;
    while path_bytes <= 4096 {
        let name = format!("deep-{index:03}-{}", "x".repeat(WORK_NAME_BYTES - 9));
        path_bytes += name.len() + 1;
        directory = create_directory_at(directory.as_fd(), &name);
        index += 1;
    }
    assert!(path_bytes > 4096);
    create_file_at(directory.as_fd(), "terminal", b"deep content beyond PATH_MAX\0\xff");
}

fn wide_payload(index: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(&index.to_le_bytes());
    bytes.extend_from_slice(b"\0ish-fs-resource-content\xff\n");
    bytes
}

fn verify_tree(root: &Path, wide_files: u32) {
    let destination = rfs::open(root.join("destination"), OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    for index in 0..wide_files {
        let name = format!("wide-{index:05}");
        assert_eq!(read_file_at(destination.as_fd(), &name), wide_payload(index));
    }

    let mut directory = destination;
    let mut path_bytes = 0;
    let mut index = 0;
    while path_bytes <= 4096 {
        let name = format!("deep-{index:03}-{}", "x".repeat(WORK_NAME_BYTES - 9));
        path_bytes += name.len() + 1;
        directory = open_directory_at(directory.as_fd(), &name);
        index += 1;
    }
    assert!(path_bytes > 4096);
    assert_eq!(read_file_at(directory.as_fd(), "terminal"), b"deep content beyond PATH_MAX\0\xff");
}

fn read_file_at(parent: BorrowedFd<'_>, name: &str) -> Vec<u8> {
    let fd = rfs::openat(parent, name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).unwrap();
    let mut bytes = Vec::new();
    File::from(fd).read_to_end(&mut bytes).unwrap();
    bytes
}

fn run(request: &Request) -> i32 {
    execute(request, &Cancellation::default(), &|_: Event| Ok(())).status()
}

#[test]
fn iterative_transfer_preserves_deep_and_wide_tree_beyond_path_limit() {
    let fixture = Fixture::new("ish-fs-resource-tree");
    fixture_tree(&fixture.0, WIDE_FILES);
    let status = run(&fixture.request(Options::default()));
    assert_eq!(status, 0);
    verify_tree(&fixture.0, WIDE_FILES);
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct ProcessSamples {
    descriptors: usize,
    rss_bytes: u64,
    count: usize,
}

#[cfg(target_os = "linux")]
fn sample_process(child: &mut Child) -> io::Result<(ExitStatus, ProcessSamples)> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut samples = ProcessSamples::default();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, samples));
        }
        let process = PathBuf::from(format!("/proc/{}/", child.id()));
        if let Ok(entries) = fs::read_dir(process.join("fd")) {
            samples.descriptors = samples.descriptors.max(entries.count());
        }
        if let Ok(status) = fs::read_to_string(process.join("status"))
            && let Some(line) = status.lines().find(|line| line.starts_with("VmRSS:"))
        {
            let kib = line.split_whitespace().nth(1).and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
            samples.rss_bytes = samples.rss_bytes.max(kib.saturating_mul(1024));
        }
        samples.count += 1;
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "resource probe exceeded 20 seconds"));
        }
        std::thread::yield_now();
    }
}

#[cfg(target_os = "linux")]
fn start_child(test_name: &str, mode: &str, fixture: &Path, baseline: Option<&Path>) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--ignored", "--nocapture"])
        .env("ISH_FS_RESOURCE_CHILD", mode)
        .env("ISH_FS_RESOURCE_FIXTURE", fixture)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(baseline) = baseline { command.env("ISH_FS_DESCRIPTOR_BASELINE", baseline); }
    command.spawn().unwrap()
}

#[cfg(target_os = "linux")]
fn set_soft_limit(resource: rustix::process::Resource, current: u64) {
    let existing = rustix::process::getrlimit(resource);
    assert!(existing.maximum.is_none_or(|maximum| current <= maximum), "host hard limit is below the fixture limit");
    rustix::process::setrlimit(resource, rustix::process::Rlimit { current: Some(current), maximum: existing.maximum }).unwrap();
}

#[cfg(target_os = "linux")]
fn assert_only_source(root: &Path) {
    let entries = fs::read_dir(root).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>();
    assert_eq!(entries, ["source"]);
}

#[cfg(target_os = "linux")]
fn process_descriptor_count() -> usize {
    fs::read_dir("/proc/self/fd").unwrap().count().saturating_sub(1)
}

#[cfg(target_os = "linux")]
#[test]
fn jobs_bounds_and_resource_rejection() {
    let fixture = Fixture::new("ish-fs-resource-limits");
    fixture_tree(&fixture.0, WIDE_FILES);
    let baseline_path = fixture.0.with_extension("descriptor-baseline");
    let mut child = start_child("child_descriptor_budget_probe", "descriptors", &fixture.0, Some(&baseline_path));
    let (status, samples) = sample_process(&mut child).unwrap();
    assert!(status.success(), "descriptor child exited with {status}");
    assert!(samples.count > 0, "no /proc samples were collected");
    let baseline = fs::read_to_string(&baseline_path).unwrap().parse::<usize>().unwrap();
    fs::remove_file(&baseline_path).unwrap();
    let engine_descriptors = samples.descriptors.saturating_sub(baseline);
    eprintln!("descriptor probe: baseline {baseline}, observed total peak {}, engine-owned delta {engine_descriptors}, sampled peak RSS {} bytes across {} samples; polling can miss short peaks", samples.descriptors, samples.rss_bytes, samples.count);
    assert!(engine_descriptors <= 16, "observed {engine_descriptors} engine-owned descriptors with RLIMIT_NOFILE=48, budget=16");
    verify_tree(&fixture.0, WIDE_FILES);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "subprocess fixture for descriptor limits"]
fn child_descriptor_budget_probe() {
    let Some(mode) = std::env::var_os("ISH_FS_RESOURCE_CHILD") else { return; };
    if mode != "descriptors" { return; }
    let root = PathBuf::from(std::env::var_os("ISH_FS_RESOURCE_FIXTURE").unwrap());
    let baseline_path = PathBuf::from(std::env::var_os("ISH_FS_DESCRIPTOR_BASELINE").unwrap());
    fs::write(&baseline_path, process_descriptor_count().to_string()).unwrap();
    set_soft_limit(rustix::process::Resource::Nofile, 48);
    let request = Request {
        operation: Operation::Copy,
        operands: Operands::Exact { source: "source".into(), destination: "destination".into() },
        options: Options { jobs: Some(Jobs::new(2).unwrap()), ..Options::default() },
        context: Context { cwd: root.clone(), home: None, scratch: root.clone() },
    };
    assert_eq!(run(&request), 2, "explicit jobs beyond the descriptor budget must be invalid");
    assert_only_source(&root);

    set_soft_limit(rustix::process::Resource::Nofile, 47);
    let request = Request { options: Options::default(), ..request };
    assert_eq!(run(&request), 1, "fewer than 16 reserved descriptors must fail operationally");
    assert_only_source(&root);

    set_soft_limit(rustix::process::Resource::Nofile, 48);
    let request = Request { options: Options { jobs: Some(Jobs::new(1).unwrap()), ..Options::default() }, ..request };
    assert_eq!(run(&request), 0, "one worker must fit the exact 16-descriptor budget");
    assert!(root.join("destination").is_dir());
}

#[cfg(target_os = "linux")]
fn virtual_size_bytes() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let kib = status.lines().find(|line| line.starts_with("VmSize:")).unwrap().split_whitespace().nth(1).unwrap().parse::<u64>().unwrap();
    kib * 1024
}

#[cfg(target_os = "linux")]
#[test]
fn memory_budget_rejects_unaffordable_explicit_parallelism_before_mutation() {
    let fixture = Fixture::new("ish-fs-memory-limit");
    fixture_tree(&fixture.0, 16);
    let mut child = start_child("child_memory_budget_probe", "memory", &fixture.0, None);
    let (status, samples) = sample_process(&mut child).unwrap();
    eprintln!("memory probe: RLIMIT_AS left 16 MiB above process VmSize; sampled peak RSS {} bytes across {} samples; polling can miss short peaks", samples.rss_bytes, samples.count);
    assert!(samples.count > 0, "no /proc samples were collected");
    assert!(status.success(), "memory child exited with {status}");
    verify_tree(&fixture.0, 16);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "subprocess fixture for address-space limits"]
fn child_memory_budget_probe() {
    let Some(mode) = std::env::var_os("ISH_FS_RESOURCE_CHILD") else { return; };
    if mode != "memory" { return; }
    let root = PathBuf::from(std::env::var_os("ISH_FS_RESOURCE_FIXTURE").unwrap());
    let limit = virtual_size_bytes().saturating_add(16 * 1024 * 1024);
    set_soft_limit(rustix::process::Resource::As, limit);
    let request = Request {
        operation: Operation::Copy,
        operands: Operands::Exact { source: "source".into(), destination: "destination".into() },
        options: Options { jobs: Some(Jobs::new(32).unwrap()), ..Options::default() },
        context: Context { cwd: root.clone(), home: None, scratch: root.clone() },
    };
    assert_eq!(run(&request), 2, "explicit jobs above the address-space budget must be invalid");
    assert_only_source(&root);

    set_soft_limit(rustix::process::Resource::As, limit.saturating_add(128 * 1024 * 1024));
    let request = Request { options: Options { jobs: Some(Jobs::new(1).unwrap()), ..Options::default() }, ..request };
    assert_eq!(run(&request), 0, "one worker must fit the constrained address-space budget");
    verify_tree(&root, 16);
}

#[cfg(target_os = "linux")]
#[test]
fn metadata_memory_budget_rejects_parallelism_before_mutation() {
    let fixture = Fixture::new("ish-fs-metadata-memory-limit");
    fs::write(fixture.0.join("source"), b"metadata admission payload").unwrap();
    let mut child = start_child("child_metadata_memory_budget_probe", "metadata-memory", &fixture.0, None);
    let (status, samples) = sample_process(&mut child).unwrap();
    assert!(samples.count > 0, "no /proc samples were collected");
    assert!(status.success(), "metadata memory child exited with {status}");
    assert_only_source(&fixture.0);
    assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"metadata admission payload");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "subprocess fixture for metadata address-space admission"]
fn child_metadata_memory_budget_probe() {
    let Some(mode) = std::env::var_os("ISH_FS_RESOURCE_CHILD") else { return; };
    if mode != "metadata-memory" { return; }
    let root = PathBuf::from(std::env::var_os("ISH_FS_RESOURCE_FIXTURE").unwrap());
    set_soft_limit(rustix::process::Resource::As, virtual_size_bytes().saturating_add(96 * 1024 * 1024));
    let request = Request {
        operation: Operation::Copy,
        operands: Operands::Exact { source: "source".into(), destination: "destination".into() },
        options: Options { jobs: Some(Jobs::new(32).unwrap()), ..Options::default() },
        context: Context { cwd: root.clone(), home: None, scratch: root.clone() },
    };
    assert_eq!(run(&request), 2, "explicit jobs must reserve metadata capture and application memory before mutation");
    assert_only_source(&root);
}
