use fdu_core::{EntryType, FileIdentity, NodeState, NodeStatus, ScanEvent, Tree};
use fdu_delete::{DeleteEvent, OutcomeKind, create_plan};
use fdu_posix::identity;
use fdu_scan::{RootAnchor, open_root_no_follow, start_indexed_scan};
use ish::foreground::Foreground;
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const SCAN_EVENT_CHANNEL_CAPACITY: usize = 512;
const SCAN_BATCH_CAPACITY: usize = 1024;
const SCAN_BATCH_POOL_CAPACITY: usize = 8;
const DELETE_CHANNEL_CAPACITY: usize = 8;
const PROGRESS_BAR_WIDTH: usize = 30;

pub(super) enum Command {
    Help,
    Run { paths: Vec<PathBuf> },
}

pub(super) fn parse_args(args: &[OsString]) -> Result<Command, String> {
    let mut positional_only = false;
    let mut help = false;
    let mut paths = Vec::new();
    for argument in args {
        let bytes = argument.as_os_str().as_bytes();
        if !positional_only {
            match bytes {
                b"--" => {
                    positional_only = true;
                    continue;
                }
                b"-h" | b"--help" => {
                    help = true;
                    continue;
                }
                b"--force" | b"--recursive" => continue,
                _ if is_force_recursive_option(bytes) => continue,
                _ if bytes.starts_with(b"-") => {
                    return Err(format!("unknown option: {}", argument.to_string_lossy()));
                }
                _ => {}
            }
        }
        paths.push(PathBuf::from(argument));
    }
    if help {
        return Ok(Command::Help);
    }
    if paths.is_empty() {
        return Err("requires at least one PATH".to_owned());
    }
    Ok(Command::Run { paths })
}

fn is_force_recursive_option(bytes: &[u8]) -> bool {
    bytes.len() > 1
        && bytes[0] == b'-'
        && bytes[1..].iter().all(|byte| matches!(byte, b'r' | b'R' | b'f'))
}

pub(super) fn run(paths: Vec<PathBuf>, home: Option<OsString>) -> io::Result<()> {
    let mut foreground = Foreground::start()?;
    let mut first_error = None;
    for path in paths {
        if foreground.cancellation().is_cancelled() {
            break;
        }
        if let Err(error) = remove_path(&path, home.as_deref(), &mut foreground)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = foreground.clear_progress() {
            if first_error.is_none() {
                first_error = Some(error);
            }
            break;
        }
    }
    foreground.poll();
    let cancelled_by_user = foreground.cancellation().reason() == Some(ish_fs::CancelReason::User);
    let finish = foreground.finish();
    if cancelled_by_user {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled"));
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    finish
}

fn remove_path(path: &Path, home: Option<&OsStr>, foreground: &mut Foreground) -> io::Result<()> {
    let (parent_path, name) = target_components(path)?;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            ));
        }
    };
    if metadata.file_type().is_dir() {
        remove_directory(path, &parent_path, &name, home, foreground)
    } else {
        let file_type = metadata.file_type();
        let expected_kind = if file_type.is_symlink() {
            EntryType::Symlink
        } else if file_type.is_file() {
            EntryType::RegularFile
        } else {
            EntryType::Other
        };
        remove_leaf(
            path,
            &parent_path,
            &name,
            FileIdentity { device: metadata.dev(), inode: metadata.ino() },
            expected_kind,
        )
    }
}

fn target_components(path: &Path) -> io::Result<(PathBuf, OsString)> {
    let bytes = path.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to remove the filesystem root",
        ));
    }
    let start = bytes[..end]
        .iter()
        .rposition(|byte| *byte == b'/')
        .map_or(0, |index| index + 1);
    let last = &bytes[start..end];
    if last == b"." || last == b".." {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to remove {}", path.display()),
        ));
    }
    let trimmed = Path::new(OsStr::from_bytes(&bytes[..end]));
    let name = trimmed.file_name().expect("a non-empty final component has a name");
    let parent = trimmed
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok((parent.to_path_buf(), name.to_os_string()))
}

fn open_parent(path: &Path) -> io::Result<OwnedFd> {
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| path_error(path, error.into()))
}

fn path_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

fn sync_error(path: &Path, error: Errno) -> io::Error {
    let error = io::Error::from(error);
    io::Error::new(
        error.kind(),
        format!("{} was removed; synchronization failed: {error}", path.display()),
    )
}

fn remove_leaf(
    path: &Path,
    parent_path: &Path,
    name: &OsStr,
    expected: FileIdentity,
    expected_kind: EntryType,
) -> io::Result<()> {
    let parent = open_parent(parent_path)?;
    let stat = match identity::stat_at(parent.as_fd(), name) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => {
            return Err(path_error(path, error.into()));
        }
    };
    let actual = identity::entry_identity(&stat)?;
    if actual.file != expected || actual.kind != expected_kind {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} changed before it could be removed", path.display()),
        ));
    }
    match rustix::fs::unlinkat(parent.as_fd(), name, AtFlags::empty()) {
        Ok(()) => {}
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => {
            return Err(path_error(path, error.into()));
        }
    }
    sync_removed_parent(&parent, path)
}

fn sync_removed_parent(parent: &OwnedFd, path: &Path) -> io::Result<()> {
    rustix::fs::fsync(parent.as_fd()).map_err(|error| sync_error(path, error))
}

fn remove_directory(
    path: &Path,
    parent_path: &Path,
    name: &OsStr,
    home: Option<&OsStr>,
    foreground: &mut Foreground,
) -> io::Result<()> {
    let root = open_root_no_follow(path).map_err(|error| describe_open_failure(path, error))?;
    let parent = open_parent(parent_path)?;
    ensure_not_mount_point(&root, parent.as_fd(), path)?;
    let cwd = std::env::current_dir()?;
    if let Some(reason) = protected_root_reason(&root, &cwd, home)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to remove {}: it is {reason}", path.display()),
        ));
    }
    let tree = build_index(&root, foreground)?;
    foreground.clear_progress()?;
    require_complete_scan_root(&tree).map_err(|error| {
        io::Error::other(format!("refusing to remove {}: {error}", path.display()))
    })?;
    let selected: Vec<fdu_core::NodeId> = tree.children(tree.root()).collect();
    if !selected.is_empty() {
        let tree = Arc::new(tree);
        let plan = create_plan(Arc::clone(&tree), &selected).map_err(|error| {
            let mut details = error
                .rejected
                .iter()
                .map(|rejection| match rejection.node {
                    Some(node) => format!("{}: {}", display_name(&tree, node), rejection.reason),
                    None => rejection.reason.clone(),
                })
                .collect::<Vec<_>>();
            details.sort();
            let details = details.join("; ");
            io::Error::other(format!("{error}: {details}"))
        })?;
        let total = plan.operation_count();
        let root_fd = root.try_clone_fd()?;
        let (sender, receiver) = mpsc::sync_channel(DELETE_CHANNEL_CAPACITY);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::spawn(move || {
            fdu_delete::execute(root_fd, plan, worker_cancelled, sender);
        });
        let mut report = DeletionReport::default();
        let mut receive_error = None;
        let mut finished = false;
        while !finished {
            let received = foreground.recv_timeout(&receiver);
            sync_cancel_flag(foreground, &cancelled);
            match received {
                Ok(DeleteEvent::Outcomes(outcomes)) => {
                    for outcome in outcomes {
                        report.record(&tree, outcome);
                    }
                    let completed = report.completed();
                    let line = deleting_progress(completed, total);
                    if let Err(error) = foreground.report_progress_line(&line) {
                        receive_error.get_or_insert(error);
                    }
                }
                Ok(DeleteEvent::Progress { .. }) => {}
                Ok(DeleteEvent::Failed { message }) => {
                    receive_error.get_or_insert_with(|| {
                        io::Error::other(format!("deletion stopped: {message}"))
                    });
                }
                Ok(DeleteEvent::Finished) => finished = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    receive_error.get_or_insert_with(|| {
                        io::Error::other("deletion worker stopped without finishing")
                    });
                    finished = true;
                }
            }
        }
        let worker_result = foreground
            .join(worker)
            .map_err(|_| io::Error::other("deletion worker terminated unexpectedly"));
        match (receive_error, worker_result) {
            (Some(receive_error), Err(worker_error)) => {
                return Err(report.with_operation_error(io::Error::other(format!(
                    "{receive_error}; {worker_error}"
                ))));
            }
            (Some(error), Ok(())) | (None, Err(error)) => {
                return Err(report.with_operation_error(error));
            }
            (None, Ok(())) => {}
        }
        foreground.clear_progress()?;
        if report.has_problems() {
            return Err(io::Error::other(report.failure_summary()));
        }
        if report.completed() != total {
            return Err(io::Error::other(format!(
                "deletion reported {} outcomes for {total} planned entries",
                report.completed()
            )));
        }
    }
    foreground.clear_progress()?;
    if foreground.cancellation().is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled"));
    }
    remove_root(&root, parent.as_fd(), name, path)
}

fn ensure_not_mount_point(
    root: &RootAnchor,
    parent: BorrowedFd<'_>,
    path: &Path,
) -> io::Result<()> {
    let root_mount = identity::mount_identity(root.as_fd())?;
    let parent_mount = identity::mount_identity(parent)?;
    if root_mount != parent_mount {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to remove {}: it is a mount point", path.display()),
        ));
    }
    Ok(())
}

fn remove_root(
    root: &RootAnchor,
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &Path,
) -> io::Result<()> {
    let stat = match identity::stat_at(parent, name) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => {
            return Err(path_error(path, error.into()));
        }
    };
    let actual = identity::entry_identity(&stat)?;
    if actual.file != root.identity || actual.kind != EntryType::Directory {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} changed before it could be removed", path.display()),
        ));
    }
    match rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR) {
        Ok(()) => {}
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => {
            return Err(path_error(path, error.into()));
        }
    }
    rustix::fs::fsync(parent).map_err(|error| sync_error(path, error))
}

#[derive(Default)]
struct DeletionReport {
    deleted: usize,
    removed_sync_failed: usize,
    already_absent: usize,
    changed: usize,
    failed: usize,
    not_attempted: usize,
    first_problem: Option<String>,
}

impl DeletionReport {
    fn record(&mut self, tree: &Tree, outcome: fdu_delete::DeleteOutcome) {
        match outcome.kind {
            OutcomeKind::Deleted => self.deleted += 1,
            OutcomeKind::DeletedSyncFailed => {
                self.removed_sync_failed += 1;
                self.first_problem.get_or_insert_with(|| {
                    let detail = outcome.message.unwrap_or_else(|| "entry removed; synchronization failed".to_owned());
                    format!("{}: {detail}", display_name(tree, outcome.node))
                });
            },
            OutcomeKind::AlreadyAbsent => self.already_absent += 1,
            OutcomeKind::Changed => {
                self.changed += 1;
                self.first_problem.get_or_insert_with(|| {
                    let detail = outcome.message.unwrap_or_else(|| "entry changed".to_owned());
                    format!("{}: {detail}", display_name(tree, outcome.node))
                });
            }
            OutcomeKind::Failed => {
                self.failed += 1;
                self.first_problem.get_or_insert_with(|| {
                    let detail = outcome.message.unwrap_or_else(|| "entry could not be removed".to_owned());
                    format!("{}: {detail}", display_name(tree, outcome.node))
                });
            }
            OutcomeKind::Cancelled => self.not_attempted += 1,
        }
    }

    fn completed(&self) -> usize {
        self.deleted + self.removed_sync_failed + self.already_absent + self.changed + self.failed + self.not_attempted
    }

    fn has_problems(&self) -> bool {
        self.removed_sync_failed != 0 || self.changed != 0 || self.failed != 0 || self.not_attempted != 0
    }

    fn with_operation_error(&self, error: io::Error) -> io::Error {
        if self.completed() == 0 { return error; }
        io::Error::new(error.kind(), format!("{}; {error}", self.failure_summary()))
    }

    fn failure_summary(&self) -> String {
        let Self { deleted, removed_sync_failed, already_absent, changed, failed, not_attempted, first_problem } = self;
        let mut summary = format!("Deleted {deleted} · {removed_sync_failed} removed with synchronization failures · {already_absent} already absent · {changed} changed · {failed} failed · {not_attempted} not attempted");
        if let Some(problem) = first_problem { summary.push_str(&format!("; first problem: {problem}")); }
        summary
    }
}

fn deleting_progress(completed: usize, total: usize) -> String {
    let fraction = if total == 0 { 1.0 } else { completed as f64 / total as f64 };
    let filled = ((fraction * PROGRESS_BAR_WIDTH as f64) as usize).min(PROGRESS_BAR_WIDTH);
    format!(
        "Deleting [{}{}] {:>3}% {completed}/{total}",
        "#".repeat(filled),
        "-".repeat(PROGRESS_BAR_WIDTH - filled),
        (fraction * 100.0) as usize,
    )
}

fn describe_open_failure(path: &Path, error: io::Error) -> io::Error {
    if last_path_component_is_symlink(path) {
        return io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a symlink; name the directory it points to instead", path.display()),
        );
    }
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}


fn last_path_component_is_symlink(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    end > 0
        && std::fs::symlink_metadata(Path::new(OsStr::from_bytes(&bytes[..end]))).is_ok_and(|metadata| {
            metadata.file_type().is_symlink()
        })
}






fn protected_root_reason(
    root: &RootAnchor,
    cwd: &Path,
    home: Option<&OsStr>,
) -> io::Result<Option<&'static str>> {
    let is_root = |candidate: &Path| -> io::Result<bool> {
        let metadata = std::fs::metadata(candidate)
            .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", candidate.display())))?;
        Ok(metadata.dev() == root.identity.device && metadata.ino() == root.identity.inode)
    };
    if is_root(Path::new("/"))? {
        return Ok(Some("the filesystem root"));
    }
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        match is_root(Path::new(home)) {
            Ok(true) => return Ok(Some("your home directory")),
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if is_root(cwd)? {
        return Ok(Some("the current directory"));
    }
    for parent in cwd.ancestors().skip(1) {
        if is_root(parent)? {
            return Ok(Some("a parent of the current directory"));
        }
    }
    Ok(None)
}

fn display_name(tree: &Tree, id: fdu_core::NodeId) -> String {
    match tree.name(id) {
        Some(name) => String::from_utf8_lossy(name).into_owned(),
        None => "<unknown>".to_owned(),
    }
}

fn sync_cancel_flag(foreground: &Foreground, cancelled: &AtomicBool) {
    if foreground.cancellation().is_cancelled() {
        cancelled.store(true, Ordering::Release);
    }
}


fn require_complete_scan_root(tree: &Tree) -> io::Result<()> {
    if tree.record(tree.root()).expect("root exists").state != NodeState::Complete {
        return Err(io::Error::other("root scan is incomplete"));
    }
    Ok(())
}




fn build_index(root: &RootAnchor, foreground: &mut Foreground) -> io::Result<Tree> {
    let mut tree = Tree::new(&root.name, root.identity, root.link_count)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "root path exceeds index limits"))?;
    let mut token_nodes = vec![Some(tree.root())];
    let (sender, receiver) = mpsc::sync_channel(SCAN_EVENT_CHANNEL_CAPACITY);
    let (batch_sender, batch_receiver) = mpsc::sync_channel(SCAN_BATCH_POOL_CAPACITY);
    for _ in 0..SCAN_BATCH_POOL_CAPACITY {
        let batch = fdu_core::EntryBatch::with_capacity(SCAN_BATCH_CAPACITY, SCAN_BATCH_CAPACITY * 24);
        if batch_sender.send(batch).is_err() {
            break;
        }
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let worker = start_indexed_scan(root.clone(), sender, batch_receiver, worker_cancelled);
    let mut fatal: Option<String> = None;
    let mut scanned = 0usize;
    let mut done = false;
    while !done {
        let received = foreground.recv_timeout(&receiver);
        sync_cancel_flag(foreground, &cancelled);
        match received {
            Ok(event) => match event {
                ScanEvent::Started { identity, .. } => {
                    if identity != tree.record(tree.root()).expect("root exists").identity {
                        tree.mark_incomplete_to_root(tree.root());
                        fatal.get_or_insert("scan root identity changed after it was opened".to_owned());
                    }
                }
                ScanEvent::Entries(mut batch) => {
                    scanned += batch.entries.len();
                    if let Err(error) = foreground.report_progress("Scanning...", scanned as u64, None) {
                        fatal.get_or_insert_with(|| error.to_string());
                    }
                    if let Err(error) = apply_batch(&mut tree, &mut token_nodes, &batch) {
                        fatal.get_or_insert_with(|| error.to_string());
                    }
                    batch.clear();
                    let _ = batch_sender.try_send(batch);
                }
                ScanEvent::DirectoryFinished { directory, complete } => {
                    finish_directory(&mut tree, &token_nodes, directory, complete);
                }
                ScanEvent::DirectoriesFinished(directories) => {
                    for (directory, complete) in directories {
                        finish_directory(&mut tree, &token_nodes, directory, complete);
                    }
                }
                ScanEvent::DirectoryExcluded { directory, reason } => {
                    if let Some(node) = token_nodes.get(directory.0 as usize).copied().flatten() {
                        tree.set_state(node, NodeStatus::Excluded(reason));
                        if let Some(parent) = tree.record(node).and_then(|record| record.parent()) {
                            tree.mark_incomplete_to_root(parent);
                        }
                    }
                }
                ScanEvent::DirectoryFailed { directory, .. } => {
                    if let Some(node) = token_nodes.get(directory.0 as usize).copied().flatten() {
                        tree.mark_incomplete_to_root(node);
                    } else {
                        tree.mark_incomplete_to_root(tree.root());
                        fatal.get_or_insert("scan error in an unknown directory".to_owned());
                    }
                }
                ScanEvent::Failed { message } => {
                    fatal.get_or_insert(format!("scan could not continue: {message}"));
                    tree.mark_incomplete_to_root(tree.root());
                }
                ScanEvent::Cancelled => {
                    fatal.get_or_insert("scan cancelled".to_owned());
                    tree.mark_scanning_incomplete();
                }
                ScanEvent::Finished => done = true,
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                fatal.get_or_insert("scanner stopped without finishing".to_owned());
                done = true;
            }
        }
    }
    foreground
        .join(worker)
        .map_err(|_| io::Error::other("scanner worker terminated unexpectedly"))?;
    if let Some(message) = fatal {
        return Err(io::Error::other(message));
    }
    if tree.record(tree.root()).is_some_and(|record| record.state == NodeState::Scanning) {
        tree.set_state(tree.root(), NodeStatus::Complete);
    }
    tree.shrink_to_fit();
    Ok(tree)
}

fn finish_directory(
    tree: &mut Tree,
    token_nodes: &[Option<fdu_core::NodeId>],
    directory: fdu_core::DirectoryToken,
    complete: bool,
) {
    let Some(node) = token_nodes.get(directory.0 as usize).copied().flatten() else {
        return;
    };
    let state = tree.record(node).map(|record| record.state);
    if complete {
        if state == Some(NodeState::Scanning) {
            tree.set_state(node, NodeStatus::Complete);
        }
    } else {
        tree.mark_incomplete_to_root(node);
    }
}




fn apply_batch(
    tree: &mut Tree,
    token_nodes: &mut Vec<Option<fdu_core::NodeId>>,
    batch: &fdu_core::EntryBatch,
) -> io::Result<()> {
    struct Run {
        parent: fdu_core::NodeId,
        apparent: u64,
        allocated: u64,
        fits: bool,
        problem: bool,
    }
    let mut run: Option<Run> = None;
    let finish = |tree: &mut Tree, run: Run| {
        if run.problem {
            tree.mark_incomplete_to_root(run.parent);
        }
        if !run.fits || !tree.add_to_ancestors(run.parent, run.apparent, run.allocated) {
            tree.mark_incomplete_to_root(run.parent);
        }
    };
    for entry in &batch.entries {
        let parent = token_nodes
            .get(entry.parent.0 as usize)
            .copied()
            .flatten()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "scanner sent entries for an unknown directory"))?;
        if run.as_ref().is_some_and(|run| run.parent != parent) {
            finish(tree, run.take().expect("run exists"));
        }
        let current = run.get_or_insert(Run { parent, apparent: 0, allocated: 0, fits: true, problem: false });
        let name = batch
            .names
            .get(entry.name.start as usize..entry.name.end as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "scanner sent an invalid filename range"))?;
        let id = tree
            .append(
                parent,
                name,
                entry.entry_type,
                entry.identity,
                entry.link_count,
                entry.apparent_bytes,
                entry.allocated_bytes,
                entry.state,
            )
            .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "index exhausted the supported number of entries"))?;
        if let Some(token) = entry.directory_token {
            let index = token.0 as usize;
            if token_nodes.len() <= index {
                token_nodes.resize(index + 1, None);
            }
            token_nodes[index] = Some(id);
        }
        match entry.state {
            NodeStatus::Complete => {
                if entry.entry_type != EntryType::Directory {
                    match (
                        current.apparent.checked_add(entry.apparent_bytes),
                        current.allocated.checked_add(entry.allocated_bytes),
                    ) {
                        (Some(apparent), Some(allocated)) => {
                            current.apparent = apparent;
                            current.allocated = allocated;
                        }
                        _ => current.fits = false,
                    }
                }
            }
            NodeStatus::Incomplete | NodeStatus::Excluded(_) => {
                current.problem = true;
            }
            NodeStatus::Scanning | NodeStatus::Stale => {
                current.problem = true;
            }
        }
    }
    if let Some(run) = run {
        finish(tree, run);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn removed_sync_failure_counts_as_completed_and_reports_the_warning() {
        let tree = Tree::new(b"selected", fdu_core::FileIdentity { device: 1, inode: 2 }, 2).unwrap();
        let mut report = DeletionReport::default();
        report.record(&tree, fdu_delete::DeleteOutcome { node: tree.root(), kind: OutcomeKind::DeletedSyncFailed, message: Some("entry removed; synchronization failed".into()) });
        assert_eq!(report.completed(), 1);
        assert_eq!(report.removed_sync_failed, 1);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.failed, 0);
        assert!(report.has_problems());
        let summary = report.failure_summary();
        assert!(summary.contains("1 removed with synchronization failures"));
        assert!(summary.contains("0 failed"));
        assert!(summary.contains("entry removed; synchronization failed"));
        let terminal_error = report.with_operation_error(io::Error::other("journal cleanup failed"));
        assert!(terminal_error.to_string().contains("1 removed with synchronization failures"));
        assert!(terminal_error.to_string().contains("journal cleanup failed"));
    }

    #[test]
    fn removes_directory_tree_and_root() {
        let base = scratch_path("tree");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("nested")).expect("nested directory is created");
        std::fs::write(root.join("nested/file"), b"content").expect("file is created");
        std::fs::write(root.join("top"), b"top").expect("top-level file is created");

        let result = run(vec![root.clone()], None);

        assert!(result.is_ok(), "rmrf failed: {result:?}");
        assert!(!root.exists(), "rmrf kept the selected directory");
        std::fs::remove_dir_all(&base).expect("test cleanup removes the scratch tree");
    }

    #[test]
    fn removes_files_symlinks_and_missing_paths() {
        let base = scratch_path("mixed");
        std::fs::create_dir_all(&base).expect("scratch base is created");
        let file = base.join("file");
        std::fs::write(&file, b"content").expect("file is created");
        let target = base.join("target");
        std::fs::create_dir_all(&target).expect("symlink target is created");
        std::fs::write(target.join("inside"), b"content").expect("target file is created");
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink is created");
        let missing = base.join("missing");

        let result = run(vec![file.clone(), link.clone(), missing], None);

        assert!(result.is_ok(), "rmrf failed: {result:?}");
        assert!(!file.exists(), "regular file was kept");
        assert!(!link.exists(), "symlink was kept");
        assert!(target.join("inside").is_file(), "symlink target was removed");
        std::fs::remove_dir_all(&base).expect("test cleanup removes the scratch tree");
    }

    #[test]
    fn protects_home_directory() {
        let base = scratch_path("protected");
        let home = base.join("home");
        std::fs::create_dir_all(home.join("nested")).expect("protected tree is created");
        std::fs::write(home.join("nested/file"), b"content").expect("file is created");

        let error = run(vec![home.clone()], Some(home.as_os_str().to_owned()))
            .expect_err("rmrf removed the home directory");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(home.join("nested/file").is_file(), "protected content was removed");
        std::fs::remove_dir_all(&base).expect("test cleanup removes the scratch tree");
    }

    #[test]
    fn parses_multiple_paths_and_force_flags() {
        let args = [
            OsString::from("-rf"),
            OsString::from("first"),
            OsString::from("--"),
            OsString::from("-second"),
        ];
        match parse_args(&args) {
            Ok(Command::Run { paths }) => {
                assert_eq!(paths, vec![PathBuf::from("first"), PathBuf::from("-second")]);
            }
            _ => panic!("multiple paths were not parsed"),
        }
        assert!(matches!(parse_args(&[OsString::from("--help")]), Ok(Command::Help)));
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&[OsString::from("--yes"), OsString::from("path")]).is_err());
    }

    fn scratch_path(label: &str) -> PathBuf {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
        std::fs::create_dir_all(&scratch).expect("scratch directory is available");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos();
        scratch.join(format!("ish-rmrf-{label}-{}-{unique}", std::process::id()))
    }
}

