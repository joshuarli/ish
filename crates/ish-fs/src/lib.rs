#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ish-fs supports Linux and macOS");

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

mod engine;
pub use engine::execute;
pub(crate) mod platform;
pub(crate) mod policy;
pub(crate) mod roots;

pub const QUEUE_CAPACITY: usize = 256;
pub const TRANSFER_BUFFER_BYTES: usize = 256 * 1024;
pub const RECORD_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DESCRIPTORS: usize = 256;
pub const RESERVED_DESCRIPTORS: usize = 32;
pub const MIN_DESCRIPTORS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Copy,
    Move,
}

#[derive(Clone, Debug)]
pub enum Operands {
    Exact { source: PathBuf, destination: PathBuf },
    Into { directory: PathBuf, first: PathBuf, rest: Vec<PathBuf> },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Collision {
    #[default]
    Error,
    Skip,
    Replace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Jobs(std::num::NonZeroUsize);

impl Jobs {
    pub fn new(count: usize) -> io::Result<Self> {
        match std::num::NonZeroUsize::new(count) {
            Some(count) if count.get() <= 32 => Ok(Self(count)),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "jobs must be in 1..=32")),
        }
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub collision: Collision,
    pub merge: bool,
    pub dry_run: bool,
    pub durable: bool,
    pub cross_file_systems: bool,
    pub jobs: Option<Jobs>,
}

#[derive(Clone, Debug)]
pub struct Context {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub scratch: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub operation: Operation,
    pub operands: Operands,
    pub options: Options,
    pub context: Context,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelReason {
    User,
    Output,
}

#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicU8>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(2, Ordering::Release);
    }

    pub fn cancel_for_output(&self) {
        let _ = self.0.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn reason(&self) -> Option<CancelReason> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            1 => Some(CancelReason::Output),
            _ => Some(CancelReason::User),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Progress {
    pub discovered_files: u64,
    pub completed_files: u64,
    pub skipped_files: u64,
    pub failed_files: u64,
    pub logical_bytes_cloned: u64,
    /// File data bytes reported by native APIs or read by buffered copying; excludes metadata and physical I/O.
    pub bytes_streamed: u64,
    pub cloned_files: u64,
    pub native_copied_files: u64,
    pub buffered_copied_files: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Metrics {
    /// Transfer wall time includes any concurrent destination synchronization.
    pub transfer_elapsed: std::time::Duration,
    /// Summed destination synchronization calls can overlap each other and transfer time.
    pub destination_sync_work: std::time::Duration,
    /// Source retirement wall time includes source namespace synchronization.
    pub source_retirement_elapsed: std::time::Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    Complete,
    Failed,
    InvalidRequest,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub progress: Progress,
    pub completion: Completion,
    pub metrics: Metrics,
}

impl Outcome {
    pub const fn status(&self) -> i32 {
        match self.completion {
            Completion::Complete => 0,
            Completion::Failed => 1,
            Completion::InvalidRequest => 2,
            Completion::Cancelled => 130,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperandState {
    Planned,
    Skipped,
    SameEntry,
    Copied,
    Moved,
    FailedBeforePublication,
    DestinationPublishedSourceRetained,
    DestinationPartiallyPublished,
    SourcePartiallyRetired,
    SourceRemovedSyncFailed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct OperandOutcome {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub state: OperandState,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum Event {
    Progress(Progress),
    Operand(OperandOutcome),
    Diagnostic(Diagnostic),
}

pub trait EventSink: Sync {
    fn emit(&self, event: Event) -> io::Result<()>;
}

impl<F> EventSink for F
where
    F: Fn(Event) -> io::Result<()> + Sync,
{
    fn emit(&self, event: Event) -> io::Result<()> {
        self(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rustix::fd::AsFd;
    use rustix::fs::{self, Mode, OFlags};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    fn directory() -> PathBuf {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.work/tmp");
        std::fs::create_dir_all(&parent).unwrap();
        loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("ish-fs-foundation-{}-{sequence}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory: {error}"),
            }
        }
    }

    #[test]
    fn byte_path_copy_and_noreplace_preserve_late_destination() {
        let path = directory();
        std::fs::create_dir(path.join("real")).unwrap();
        symlink("real", path.join("alias")).unwrap();
        let bytes = b"captured bytes\0remain exact\xff";
        let raw_name = OsStr::from_bytes(b"payload-\xff\n");
        let raw_name_supported = match std::fs::write(path.join("real").join(raw_name), bytes) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
                eprintln!("raw-name path leg excluded: filesystem rejected malformed UTF-8");
                std::fs::write(path.join("real/payload"), bytes).unwrap();
                false
            }
            Err(error) => panic!("create path fixture: {error}"),
        };
        let name = if raw_name_supported { raw_name } else { OsStr::new("payload") };
        let cwd = fs::open(&path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
        let relative = Path::new("alias").join(name);
        let root = roots::Root::establish(cwd.as_fd(), &relative, roots::Requirement::Existing).unwrap();
        assert_eq!(root.observed.unwrap().identity.kind, roots::EntryKind::RegularFile);
        let source = fs::openat(&root.parent, &root.name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK, Mode::empty()).unwrap();
        let stage = fs::openat(cwd.as_fd(), "stage", OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR).unwrap();
        let mut buffer = [0_u8; 7];
        assert_eq!(platform::buffered_copy(source.as_fd(), stage.as_fd(), bytes.len() as u64, &mut buffer, &Cancellation::default()).unwrap(), bytes.len() as u64);
        std::fs::write(path.join("late"), b"keep existing inode").unwrap();
        let before = roots::observe(cwd.as_fd(), OsStr::new("late")).unwrap();
        let error = platform::rename_noreplace(cwd.as_fd(), OsStr::new("stage"), cwd.as_fd(), OsStr::new("late")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(roots::observe(cwd.as_fd(), OsStr::new("late")).unwrap().identity, before.identity);
        assert_eq!(std::fs::read(path.join("late")).unwrap(), b"keep existing inode");
        assert_eq!(std::fs::read(path.join("stage")).unwrap(), bytes);
        platform::rename_noreplace(cwd.as_fd(), OsStr::new("stage"), cwd.as_fd(), name).unwrap();
        assert_eq!(std::fs::read(path.join(name)).unwrap(), bytes);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn final_symlink_with_trailing_slash_remains_an_object() {
        let path = directory();
        std::fs::create_dir(path.join("target")).unwrap();
        symlink("target", path.join("link")).unwrap();
        let cwd = fs::open(&path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
        let root = roots::Root::establish(cwd.as_fd(), Path::new("link///"), roots::Requirement::Existing).unwrap();
        assert_eq!(root.observed.unwrap().identity.kind, roots::EntryKind::Symlink);
        assert_eq!(root.open_directory().unwrap_err().kind(), io::ErrorKind::NotADirectory);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn user_cancellation_wins_over_broken_output() {
        let cancellation = Cancellation::default();
        let worker = cancellation.clone();
        worker.cancel_for_output();
        assert_eq!(cancellation.reason(), Some(CancelReason::Output));
        cancellation.cancel();
        worker.cancel_for_output();
        assert_eq!(worker.reason(), Some(CancelReason::User));
    }

    #[test]
    fn jobs_rejects_zero_and_excess_parallelism() {
        assert!(Jobs::new(0).is_err());
        assert_eq!(Jobs::new(1).unwrap().get(), 1);
        assert_eq!(Jobs::new(32).unwrap().get(), 32);
        assert!(Jobs::new(33).is_err());
    }
}
