mod anchored;
mod records;
mod transfer;
mod retirement;
mod dry_run;

use std::ffi::OsString;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{self, Mode, OFlags};

use crate::policy::{self, Action, PlannedOperand};
use crate::roots::{self, EntryKind, Observation, Requirement, Root};
use crate::{CancelReason, Cancellation, Completion, Diagnostic, Event, EventSink, MAX_DESCRIPTORS, MIN_DESCRIPTORS, OperandOutcome, OperandState, Operation, Outcome, Progress, RESERVED_DESCRIPTORS, Request};
use records::{Record, IndexedRecords};

const THREAD_STACK_BYTES: usize = 512 * 1024;

#[cfg(test)]
type Hook = std::sync::Arc<dyn Fn(&str) -> io::Result<()> + Send + Sync>;

static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

pub(super) fn unique_name(purpose: &str) -> OsString {
    OsString::from(format!(".ish-fs-{purpose}-{}-{}", std::process::id(), NEXT_NAME.fetch_add(1, Ordering::Relaxed)))
}

pub(super) fn cancelled(cancellation: &Cancellation) -> io::Result<()> {
    if cancellation.is_cancelled() { Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled")) } else { Ok(()) }
}

struct Invocation<'a> {
    request: &'a Request,
    cancellation: &'a Cancellation,
    sink: &'a dyn EventSink,
    cwd: BorrowedFd<'a>,
    progress: Mutex<Progress>,
    hardlinks: Mutex<Option<IndexedRecords>>,
    self_changes: Mutex<Option<IndexedRecords>>,
    stopped: AtomicBool,
    jobs: usize,
    metrics: Mutex<crate::Metrics>,
    descriptor_reservation: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    hook: Option<Hook>,
}

impl Invocation<'_> {
    fn fault(&self, boundary: &str) -> io::Result<()> {
        #[cfg(test)]
        if let Some(hook) = &self.hook { return hook(boundary); }
        let _ = boundary;
        Ok(())
    }

    fn sync_file(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        self.fault("destination_file_sync")?;
        let start = std::time::Instant::now();
        let result = crate::platform::sync_file(fd);
        self.metrics.lock().unwrap().destination_sync_work += start.elapsed();
        result?;
        self.fault("destination_file_synced")
    }

    fn sync_directory(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        self.fault("destination_directory_sync")?;
        let start = std::time::Instant::now();
        let result = crate::platform::sync_directory(fd);
        self.metrics.lock().unwrap().destination_sync_work += start.elapsed();
        result?;
        self.fault("destination_directory_synced")
    }

    fn emit(&self, event: Event) {
        if self.sink.emit(event).is_err() {
            self.cancellation.cancel_for_output();
            self.stopped.store(true, Ordering::Release);
        }
    }

    fn checkpoint(&self) -> io::Result<()> {
        cancelled(self.cancellation)?;
        if self.stopped.load(Ordering::Acquire) { Err(io::Error::other("transfer stopped after an error")) } else { Ok(()) }
    }

    fn update(&self, update: impl FnOnce(&mut Progress)) {
        let progress = { let mut progress = self.progress.lock().unwrap(); update(&mut progress); *progress };
        self.emit(Event::Progress(progress));
    }

    fn effective(&self, observed: Observation) -> io::Result<Observation> {
        if let Some(records) = self.self_changes.lock().unwrap().as_mut()
            && let Some(record) = records.find_identity(observed.identity)? {
                let found = record.observed;
                if observed.identity.kind == EntryKind::Directory {
                    if observed.mode != found.mode || observed.uid != found.uid || observed.gid != found.gid { return Err(io::Error::other("source metadata changed")); }
                } else if observed.size != found.size || observed.mode != found.mode || observed.uid != found.uid || observed.gid != found.gid || observed.mtime != found.mtime {
                    return Err(io::Error::other("source content or metadata changed"));
                }
                return Ok(found);
        }
        Ok(observed)
    }

    fn remember_change(&self, observed: Observation) -> io::Result<()> {
        let mut changes = self.self_changes.lock().unwrap();
        if changes.is_none() { *changes = Some(IndexedRecords::new(&self.request.context.scratch)?); }
        changes.as_mut().unwrap().push(&Record { source: PathBuf::new(), destination: PathBuf::new(), observed, published: observed.identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 })
    }

    fn require_descriptors(&self, needed: usize) -> io::Result<()> {
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current.unwrap_or(u64::MAX);
        let budget = usize::try_from(limit.saturating_sub(RESERVED_DESCRIPTORS as u64)).unwrap_or(usize::MAX).min(MAX_DESCRIPTORS);
        if needed + self.descriptor_reservation.load(Ordering::Acquire) > budget { return Err(io::Error::other("descriptor budget cannot admit this transfer phase; source retained")); }
        Ok(())
    }

    fn index_descriptors(&self) -> usize {
        2 * (usize::from(self.hardlinks.lock().unwrap().is_some()) + usize::from(self.self_changes.lock().unwrap().is_some()))
    }

    fn durable(&self) -> bool { self.request.operation == Operation::Move || self.request.options.durable }
}

fn resource_jobs(request: &Request) -> io::Result<usize> {
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current.unwrap_or(u64::MAX);
    let budget = usize::try_from(limit.saturating_sub(RESERVED_DESCRIPTORS as u64)).unwrap_or(usize::MAX).min(MAX_DESCRIPTORS);
    if budget < MIN_DESCRIPTORS { return Err(io::Error::other("fewer than 16 descriptors remain after the reservation")); }
    let descriptor_jobs = ((budget.saturating_sub(10)) / 6).clamp(1, 32);
    let memory_jobs = memory_jobs()?;
    let allowed = descriptor_jobs.min(memory_jobs);
    if allowed == 0 {
        return Err(io::Error::new(if request.options.jobs.is_some() { io::ErrorKind::InvalidInput } else { io::ErrorKind::OutOfMemory }, "address-space budget cannot admit the transfer queue and workers"));
    }
    match request.options.jobs {
        Some(jobs) if jobs.get() > allowed => Err(io::Error::new(io::ErrorKind::InvalidInput, "jobs exceeds descriptor or memory budget")),
        Some(jobs) => Ok(jobs.get()),
        None => Ok(std::thread::available_parallelism().map(usize::from).unwrap_or(1).min(4).min(allowed)),
    }
}

pub fn execute(request: &Request, cancellation: &Cancellation, sink: &dyn EventSink) -> Outcome {
    let mut completion = Completion::Complete;
    let cwd = match fs::open(&request.context.cwd, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()) {
        Ok(cwd) => cwd,
        Err(error) => return early_error(request, cancellation, sink, error.into()),
    };
    let jobs = match resource_jobs(request) {
        Ok(jobs) => jobs,
        Err(error) => return early_error(request, cancellation, sink, error),
    };
    let plan = match policy::preflight(request, cwd.as_fd()) {
        Ok(plan) => plan,
        Err(error) => return early_error(request, cancellation, sink, error),
    };
    if let Err(error) = validate_hardlink_devices(&plan.operands) { return early_error(request, cancellation, sink, error); }
    let invocation = Invocation { request, cancellation, sink, cwd: cwd.as_fd(), progress: Mutex::new(Progress::default()), hardlinks: Mutex::new(None), self_changes: Mutex::new(None), stopped: AtomicBool::new(false), jobs, metrics: Mutex::new(crate::Metrics::default()), descriptor_reservation: std::sync::atomic::AtomicUsize::new(0), #[cfg(test)] hook: HOOK.with(|hook| hook.borrow().clone()) };
    for operand in &plan.operands {
        if cancellation.is_cancelled() {
            invocation.emit(Event::Operand(OperandOutcome { source: operand.source.clone(), destination: operand.destination.clone(), state: OperandState::Cancelled }));
            continue;
        }
        let result = execute_operand(&invocation, operand, plan.into_directory.as_ref().map(AsFd::as_fd));
        let state = match result {
            Ok(state) => state,
            Err(failure) => {
                completion = Completion::Failed;
                invocation.update(|progress| progress.failed_files += 1);
                invocation.emit(Event::Diagnostic(Diagnostic { path: operand.source.clone(), message: failure.error.to_string() }));
                if cancellation.is_cancelled() && failure.state == OperandState::FailedBeforePublication { OperandState::Cancelled } else { failure.state }
            }
        };
        invocation.emit(Event::Operand(OperandOutcome { source: operand.source.clone(), destination: operand.destination.clone(), state }));
        invocation.emit(Event::Progress(*invocation.progress.lock().unwrap()));
        invocation.stopped.store(false, Ordering::Release);
    }
    match cancellation.reason() {
        Some(CancelReason::User) => completion = Completion::Cancelled,
        Some(CancelReason::Output) => completion = Completion::Failed,
        None => {},
    }
    Outcome { progress: *invocation.progress.lock().unwrap(), completion, metrics: *invocation.metrics.lock().unwrap() }
}

fn early_error(request: &Request, cancellation: &Cancellation, sink: &dyn EventSink, error: io::Error) -> Outcome {
    let mut completion = if error.kind() == io::ErrorKind::InvalidInput { Completion::InvalidRequest } else { Completion::Failed };
    if sink.emit(Event::Diagnostic(Diagnostic { path: request.context.cwd.clone(), message: error.to_string() })).is_err() { cancellation.cancel_for_output(); }
    if cancellation.reason() == Some(CancelReason::User) { completion = Completion::Cancelled; }
    Outcome { progress: Progress::default(), completion, metrics: crate::Metrics::default() }
}

struct Failure {
    state: OperandState,
    error: io::Error,
}
impl From<rustix::io::Errno> for Failure {
    fn from(error: rustix::io::Errno) -> Self { io::Error::from(error).into() }
}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self { Self { state: OperandState::FailedBeforePublication, error } }
}

fn execute_operand(invocation: &Invocation<'_>, operand: &PlannedOperand, into: Option<BorrowedFd<'_>>) -> Result<OperandState, Failure> {
    invocation.checkpoint()?;
    if operand.action == Action::Skip {
        invocation.update(|progress| progress.skipped_files += 1);
        return Ok(OperandState::Skipped);
    }
    if operand.action == Action::SameEntry { return Ok(OperandState::SameEntry); }
    let mut source = Root::establish(invocation.cwd, &operand.source, Requirement::Existing)?;
    let destination = match into {
        Some(parent) => Root::establish_at(parent, &operand.destination_name, &operand.destination, Requirement::MayBeAbsent)?,
        None => Root::establish(invocation.cwd, &operand.destination, Requirement::MayBeAbsent)?,
    };
    if roots::observe_fd(source.parent.as_fd())?.identity != operand.source_parent || roots::observe_fd(destination.parent.as_fd())?.identity != operand.destination_parent {
        return Err(io::Error::other("operand parent identity changed after preflight").into());
    }
    let effective = invocation.effective(operand.source_observed)?;
    if !source.observed.is_some_and(|observed| anchored::unchanged(effective, observed)) || destination.observed != operand.destination_observed {
        return Err(io::Error::other("operand changed after preflight").into());
    }
    source.observed = Some(effective);
    if invocation.request.options.dry_run { return dry_run::plan(invocation, &source, &destination, operand.action).map_err(Into::into); }
    if invocation.request.operation == Operation::Move && operand.action != Action::Merge {
        match rename_object(invocation, &source, &destination, operand.action) {
            Ok(state) => return Ok(state),
            Err(failure) if failure.error.raw_os_error() == Some(libc::EXDEV) => {},
            Err(failure) => return Err(failure),
        }
    }
    if operand.action == Action::Merge {
        transfer::merge(invocation, &source, &destination)
    } else {
        let receipt = transfer::whole(invocation, &source, &destination, operand.action)?;
        match receipt { Some(receipt) => retirement::retire(invocation, receipt), None => Ok(OperandState::Copied) }
    }
}

fn rename_object(invocation: &Invocation<'_>, source: &Root, destination: &Root, action: Action) -> Result<OperandState, Failure> {
    invocation.checkpoint()?;
    invocation.fault("rename")?;
    anchored::require_unchanged(source.parent.as_fd(), &source.name, source.observed.unwrap())?;
    match action {
        Action::Replace => {
            let expected = destination.observed.ok_or_else(|| io::Error::other("replacement target is absent"))?;
            anchored::require_unchanged(destination.parent.as_fd(), &destination.name, expected)?;
            fs::renameat(&source.parent, &source.name, &destination.parent, &destination.name)?;
        },
        Action::Transfer => crate::platform::rename_noreplace(source.parent.as_fd(), &source.name, destination.parent.as_fd(), &destination.name)?,
        _ => return Err(io::Error::other("invalid rename action").into()),
    }
    let expected = source.observed.unwrap();
    let actual = roots::observe(destination.parent.as_fd(), &destination.name).map_err(|error| Failure { state: OperandState::SourcePartiallyRetired, error })?;
    if actual.identity != expected.identity {
        return Err(Failure { state: OperandState::SourcePartiallyRetired, error: io::Error::other("renamed object identity changed") });
    }
    if actual.identity.kind == EntryKind::RegularFile && actual.links > 1 {
        invocation.remember_change(actual).map_err(|error| Failure { state: OperandState::SourcePartiallyRetired, error })?;
        let mut groups = invocation.hardlinks.lock().unwrap();
        if groups.is_none() { *groups = Some(IndexedRecords::new(&invocation.request.context.scratch)?); }
        groups.as_mut().unwrap().push(&Record { source: destination.path.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf(), destination: PathBuf::from(&destination.name), observed: actual, published: actual.identity, output: Some(actual), parent: Some(roots::observe_fd(destination.parent.as_fd())?.identity), anchor: Some(roots::observe_fd(destination.parent.as_fd())?.identity), ancestry: Vec::new(), final_flags: 0 })?;
    }
    invocation.update(|progress| { progress.discovered_files += 1; progress.completed_files += 1; });
    if invocation.request.options.durable {
        invocation.sync_directory(destination.parent.as_fd()).and_then(|()| crate::platform::sync_directory(source.parent.as_fd())).map_err(|error| Failure { state: OperandState::SourceRemovedSyncFailed, error })?;
    }
    Ok(OperandState::Moved)
}

#[cfg(test)]
thread_local! {
    static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests;

enum SpanKind { Transfer, Retirement }
struct Span<'a, 'b> { invocation: &'a Invocation<'b>, kind: SpanKind, start: std::time::Instant }
impl<'a, 'b> Span<'a, 'b> {
    fn new(invocation: &'a Invocation<'b>, kind: SpanKind) -> Self { Self { invocation, kind, start: std::time::Instant::now() } }
}
impl Drop for Span<'_, '_> {
    fn drop(&mut self) {
        let mut metrics = self.invocation.metrics.lock().unwrap();
        match self.kind { SpanKind::Transfer => metrics.transfer_elapsed += self.start.elapsed(), SpanKind::Retirement => metrics.source_retirement_elapsed += self.start.elapsed() }
    }
}

fn validate_hardlink_devices(operands: &[PlannedOperand]) -> io::Result<()> {
    let mut known = std::collections::HashMap::new();
    for (index, operand) in operands.iter().enumerate() {
        if matches!(operand.action, Action::Skip | Action::SameEntry) || operand.source_observed.identity.kind != EntryKind::RegularFile || operand.source_observed.links < 2 { continue; }
        let identity = operand.source_observed.identity;
        let device = operand.destination_parent.device;
        if let Some(previous) = known.get(&identity) {
            if *previous != device { return Err(io::Error::new(io::ErrorKind::Unsupported, "hardlink group cannot span destination filesystems")); }
        } else if known.len() < 4096 {
            known.insert(identity, device);
        } else {
            for previous in &operands[..index] {
                if !matches!(previous.action, Action::Skip | Action::SameEntry) && previous.source_observed.identity == identity && previous.destination_parent.device != device {
                    return Err(io::Error::new(io::ErrorKind::Unsupported, "hardlink group cannot span destination filesystems"));
                }
            }
        }
    }
    Ok(())
}

fn memory_jobs() -> io::Result<usize> {
    let Some(limit) = rustix::process::getrlimit(rustix::process::Resource::As).current else { return Ok(32); };
    #[cfg(target_os = "linux")]
    let used = {
        let mut status = String::new();
        std::fs::File::open("/proc/self/status")?.take(64 * 1024).read_to_string(&mut status)?;
        let size = status.lines().find_map(|line| line.strip_prefix("VmSize:")).and_then(|line| line.split_whitespace().next()).ok_or_else(|| io::Error::other("address-space usage is unavailable"))?;
        size.parse::<u64>().map_err(|_| io::Error::other("address-space usage is malformed"))?.checked_mul(1024).ok_or_else(|| io::Error::other("address-space usage overflow"))?
    };
    #[cfg(target_os = "macos")]
    let used = crate::platform::address_space_bytes()?;
    let available = limit.saturating_sub(used).saturating_sub(crate::RECORD_MEMORY_BYTES as u64 + THREAD_STACK_BYTES as u64);
    let worker_bytes = THREAD_STACK_BYTES + crate::TRANSFER_BUFFER_BYTES + 4 * crate::platform::METADATA_CAPTURE_BYTES;
    Ok((available / worker_bytes as u64).min(32) as usize)
}
