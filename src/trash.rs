use fdu_core::{EntryType, NodeState, NodeStatus, ScanEvent, Tree};
use fdu_delete::{DeleteEvent, OutcomeKind, create_plan};
use fdu_scan::{RootAnchor, open_root_no_follow, start_indexed_scan};
use ish::foreground::Foreground;
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
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
    Run { path: PathBuf, assume_yes: bool },
}

pub(super) fn parse_args(args: &[OsString]) -> Result<Command, String> {
    let mut assume_yes = false;
    let mut positional_only = false;
    let mut help = false;
    let mut path = None;
    for argument in args {
        let bytes = argument.as_os_str().as_bytes();
        if !positional_only {
            match bytes {
                b"--" => {
                    positional_only = true;
                    continue;
                }
                b"--yes" => {
                    assume_yes = true;
                    continue;
                }
                b"-h" | b"--help" => {
                    help = true;
                    continue;
                }
                _ if bytes.starts_with(b"-") => {
                    return Err(format!("unknown option: {}", argument.to_string_lossy()));
                }
                _ => {}
            }
        }
        if path.is_some() {
            return Err("only one path may be given".to_owned());
        }
        path = Some(PathBuf::from(argument));
    }
    if help {
        return Ok(Command::Help);
    }
    let path = path.ok_or_else(|| "requires an explicit PATH".to_owned())?;
    Ok(Command::Run { path, assume_yes })
}

pub(super) fn run(
    path: PathBuf,
    assume_yes: bool,
    home: Option<OsString>,
) -> io::Result<()> {
    let mut foreground = Foreground::start()?;
    let result = run_with_foreground(path, assume_yes, home, &mut foreground);
    foreground.poll();
    let cancelled_by_user = foreground.cancellation().reason() == Some(ish_fs::CancelReason::User);
    let finish = foreground.finish();
    if cancelled_by_user {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled"));
    }
    result?;
    finish
}

fn run_with_foreground(
    path: PathBuf,
    assume_yes: bool,
    home: Option<OsString>,
    foreground: &mut Foreground,
) -> io::Result<()> {
    let root = open_root_no_follow(&path).map_err(|error| describe_open_failure(&path, error))?;
    let confirmation_path = (!assume_yes)
        .then(|| resolve_confirmation_path(&root, &path))
        .transpose()?;
    let cwd = std::env::current_dir()?;
    if let Some(reason) = protected_root_reason(&root, &cwd, home.as_deref())? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to empty {}: it is {reason}", path.display()),
        ));
    }
    let tree = build_index(&root, foreground)?;
    foreground.clear_progress()?;
    require_complete_scan_root(&tree).map_err(|error| {
        io::Error::other(format!("refusing to empty {}: {error}", path.display()))
    })?;
    let selected: Vec<fdu_core::NodeId> = tree.children(tree.root()).collect();
    if selected.is_empty() {
        return Ok(());
    }
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
    if !assume_yes {
        confirm_interactively(
            &root,
            &path,
            confirmation_path.as_deref().expect("confirmation path is resolved"),
            selected.len(),
            total,
            foreground,
        )?;
    }
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
                receive_error.get_or_insert_with(|| io::Error::other(format!("deletion stopped: {message}")));
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
            return Err(report.with_operation_error(io::Error::other(format!("{receive_error}; {worker_error}"))));
        }
        (Some(error), Ok(())) | (None, Err(error)) => return Err(report.with_operation_error(error)),
        (None, Ok(())) => {}
    }
    foreground.clear_progress()?;
    if !report.has_problems() {
        if report.completed() != total {
            return Err(io::Error::other(format!("deletion reported {} outcomes for {total} planned entries", report.completed())));
        }
        return Ok(());
    }
    Err(io::Error::other(report.failure_summary()))
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
    for parent in cwd.ancestors().skip(1) {
        if is_root(parent)? {
            return Ok(Some("a parent of the current directory"));
        }
    }
    Ok(None)
}

fn resolve_confirmation_path(root: &RootAnchor, path: &Path) -> io::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)?;
    verify_confirmation_target(root, path)?;
    verify_confirmation_target(root, &resolved)?;
    Ok(resolved)
}

fn verify_confirmation_target(root: &RootAnchor, path: &Path) -> io::Result<()> {
    if last_path_component_is_symlink(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected path became a symlink after it was opened",
        ));
    }
    let metadata = std::fs::metadata(path)?;
    if metadata.dev() != root.identity.device || metadata.ino() != root.identity.inode {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "selected path no longer resolves to the opened directory",
        ));
    }
    Ok(())
}

fn confirm_interactively(
    root: &RootAnchor,
    selected_path: &Path,
    resolved_path: &Path,
    top_level: usize,
    total: usize,
    foreground: &mut Foreground,
) -> io::Result<()> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "cannot ask for confirmation without a terminal; pass --yes to delete without asking",
        ));
    }
    verify_confirmation_target(root, selected_path)?;
    verify_confirmation_target(root, resolved_path)?;
    let prompt = write!(
        io::stderr().lock(),
        "Permanently delete {top_level} entries ({total} including nested) below {}? \
         The directory itself is kept. Type 'yes' to continue: ",
        resolved_path.display()
    )
    .and_then(|()| io::stderr().flush());
    if let Err(error) = prompt {
        foreground.cancellation().cancel_for_output();
        return Err(error);
    }
    if foreground
        .read_confirmation_line()?
        .is_some_and(|answer| answer.trim() == "yes")
    {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "not confirmed; nothing was deleted"))
    }
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
    use std::ffi::CStr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::Command;
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
    fn assume_yes_deletes_contents_and_keeps_the_selected_directory() {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
        std::fs::create_dir_all(&scratch).expect("scratch directory is available");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos();
        let root = scratch.join(format!("ish-trash-{}-{unique}", std::process::id()));
        std::fs::create_dir(&root).expect("trash root is created");
        std::fs::create_dir(root.join("nested")).expect("nested directory is created");
        std::fs::write(root.join("nested/file"), b"content").expect("file is created");

        let result = run(root.clone(), true, None);

        assert!(result.is_ok(), "trash failed: {result:?}");
        assert!(root.is_dir());
        assert_eq!(
            std::fs::read_dir(&root)
                .expect("selected directory remains readable")
                .count(),
            0
        );
        std::fs::remove_dir(&root).expect("selected directory is removed by test cleanup");
    }

    #[test]
    fn interactive_confirmation_deletes_after_yes_and_preserves_after_no() {
        const CHILD: &str = "ISH_TRASH_INTERACTIVE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = Command::new(std::env::current_exe().expect("test binary is available"))
                .arg("--exact")
                .arg("trash::tests::interactive_confirmation_deletes_after_yes_and_preserves_after_no")
                .env(CHILD, "1")
                .status()
                .expect("interactive trash test child starts");
            assert!(status.success());
            return;
        }
        let (yes_root, no_root, cancel_root) = test_roots();
        let yes = with_pty_stdin(b"yes\n", || run(yes_root.clone(), false, None))
            .expect("yes confirmation uses a PTY");
        assert!(yes.is_ok(), "yes confirmation failed: {yes:?}");
        assert!(yes_root.is_dir());
        assert_eq!(std::fs::read_dir(&yes_root).unwrap().count(), 0);

        let no = with_pty_stdin(b"no\n", || run(no_root.clone(), false, None))
            .expect("no confirmation uses a PTY");
        assert_eq!(no.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(no_root.join("nested/file").is_file());

        let cancelled = with_pty_stdin(b"", || {
            let interrupter = thread::spawn(|| {
                thread::sleep(std::time::Duration::from_millis(500));
                // SAFETY: raising SIGINT delivers it to this test process.
                assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
            });
            let result = run(cancel_root.clone(), false, None);
            interrupter.join().expect("interrupt thread completes");
            result
        })
        .expect("cancel confirmation uses a PTY");
        assert_eq!(cancelled.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(cancel_root.join("nested/file").is_file());
        std::fs::remove_dir_all(&yes_root).expect("yes root is removed by test cleanup");
        std::fs::remove_dir_all(&no_root).expect("no root is removed by test cleanup");
        std::fs::remove_dir_all(&cancel_root).expect("cancelled root is removed by test cleanup");
    }

    fn test_roots() -> (PathBuf, PathBuf, PathBuf) {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
        std::fs::create_dir_all(&scratch).expect("scratch directory is available");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos();
        let base = scratch.join(format!("ish-trash-confirm-{}-{unique}", std::process::id()));
        let yes_root = base.join("yes");
        let no_root = base.join("no");
        let cancel_root = base.join("cancel");
        for root in [&yes_root, &no_root, &cancel_root] {
            std::fs::create_dir_all(root.join("nested")).expect("confirmation root is created");
            std::fs::write(root.join("nested/file"), b"content").expect("file is created");
        }
        (yes_root, no_root, cancel_root)
    }

    fn with_pty_stdin<T>(input: &[u8], operation: impl FnOnce() -> T) -> io::Result<T> {
        // SAFETY: posix_openpt returns a new descriptor on success.
        let master_raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if master_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: master_raw is a newly opened descriptor owned by this function.
        let master = unsafe { OwnedFd::from_raw_fd(master_raw) };
        // SAFETY: master is an open pseudo-terminal master.
        if unsafe { libc::grantpt(master.as_raw_fd()) } != 0
            // SAFETY: master is an open pseudo-terminal master.
            || unsafe { libc::unlockpt(master.as_raw_fd()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ptsname returns a NUL-terminated name for the open master descriptor.
        let slave_name = unsafe { libc::ptsname(master.as_raw_fd()) };
        if slave_name.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: slave_name remains valid until the next ptsname call and is copied immediately.
        let slave_name = unsafe { CStr::from_ptr(slave_name) }.to_owned();
        // SAFETY: slave_name is a valid PTY path.
        let slave_raw = unsafe {
            libc::open(
                slave_name.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        if slave_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: slave_raw is a newly opened descriptor owned by this function.
        let slave = unsafe { OwnedFd::from_raw_fd(slave_raw) };
        let mut terminal_settings = rustix::termios::tcgetattr(&slave)?;
        terminal_settings.local_modes.remove(rustix::termios::LocalModes::ECHO);
        rustix::termios::tcsetattr(
            &slave,
            rustix::termios::OptionalActions::Now,
            &terminal_settings,
        )?;
        // SAFETY: dup returns a new descriptor for stdin on success.
        let original_raw = unsafe { libc::dup(0) };
        if original_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: original_raw is a newly duplicated descriptor owned by this function.
        let original = unsafe { OwnedFd::from_raw_fd(original_raw) };
        // SAFETY: slave is open and descriptor 0 is replaced with its terminal endpoint.
        if unsafe { libc::dup2(slave.as_raw_fd(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let _restore = StdinRestore(original);
        let mut written = 0;
        while written < input.len() {
            // SAFETY: input[written..] is a valid readable buffer and master is open.
            let count = unsafe {
                libc::write(
                    master.as_raw_fd(),
                    input[written..].as_ptr().cast::<libc::c_void>(),
                    input.len() - written,
                )
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            written += count as usize;
        }
        Ok(operation())
    }

    struct StdinRestore(OwnedFd);

    impl Drop for StdinRestore {
        fn drop(&mut self) {
            // SAFETY: the saved descriptor is open and descriptor 0 remains the PTY slave.
            let _ = unsafe { libc::dup2(self.0.as_raw_fd(), 0) };
        }
    }
}

