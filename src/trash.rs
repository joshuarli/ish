use fdu_core::{EntryType, NodeState, NodeStatus, ScanEvent, Tree};
use fdu_delete::{DeleteEvent, OutcomeKind, create_plan};
use fdu_scan::{RootAnchor, open_root_no_follow, start_indexed_scan};
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const SCAN_EVENT_CHANNEL_CAPACITY: usize = 512;
const SCAN_BATCH_CAPACITY: usize = 1024;
const SCAN_BATCH_POOL_CAPACITY: usize = 8;
const DELETE_CHANNEL_CAPACITY: usize = 8;
const PROGRESS_REDRAW_INTERVAL: Duration = Duration::from_millis(50);
const PROGRESS_BAR_WIDTH: usize = 30;




struct Progress {
    enabled: bool,
    drawn: bool,
    last_draw: Option<Instant>,
}

impl Progress {
    fn new() -> Self {
        Self { enabled: io::stderr().is_terminal(), drawn: false, last_draw: None }
    }

    
    fn scanning(&mut self, entries: usize) {
        self.draw(|| format!("Scanning... {entries} entries"));
    }

    fn deleting(&mut self, completed: usize, total: usize) {
        self.draw(|| {
            let fraction = if total == 0 { 1.0 } else { completed as f64 / total as f64 };
            let filled = ((fraction * PROGRESS_BAR_WIDTH as f64) as usize).min(PROGRESS_BAR_WIDTH);
            format!(
                "Deleting [{}{}] {:>3}% {completed}/{total}",
                "#".repeat(filled),
                "-".repeat(PROGRESS_BAR_WIDTH - filled),
                (fraction * 100.0) as usize,
            )
        });
    }

    fn draw(&mut self, line: impl FnOnce() -> String) {
        if !self.enabled || self.last_draw.is_some_and(|last| last.elapsed() < PROGRESS_REDRAW_INTERVAL) {
            return;
        }
        self.last_draw = Some(Instant::now());
        self.drawn = true;
        if write!(io::stderr(), "\r\x1b[2K{}", line())
            .and_then(|()| io::stderr().flush())
            .is_err()
        {
            self.enabled = false;
        }
    }

    fn clear(&mut self) {
        if self.drawn {
            let cleared = write!(io::stderr(), "\r\x1b[2K")
                .and_then(|()| io::stderr().flush())
                .is_ok();
            self.drawn = false;
            self.enabled &= cleared;
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.clear();
    }
}









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
    let mut progress = Progress::new();
    let tree = build_index(&root, &mut progress)?;
    progress.clear();
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
        )?;
    }
    let root_fd = root.try_clone_fd()?;
    let (sender, receiver) = mpsc::sync_channel(DELETE_CHANNEL_CAPACITY);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = thread::spawn(move || {
        fdu_delete::execute(root_fd, plan, cancelled, sender);
    });
    let mut deleted = 0usize;
    let mut already_absent = 0usize;
    let mut changed = 0usize;
    let mut failed = 0usize;
    let mut not_attempted = 0usize;
    let mut first_problem: Option<String> = None;
    let mut receive_error = None;
    loop {
        match receiver.recv() {
            Ok(DeleteEvent::Outcomes(outcomes)) => {
                for outcome in outcomes {
                    match outcome.kind {
                        OutcomeKind::Deleted => deleted += 1,
                        OutcomeKind::AlreadyAbsent => already_absent += 1,
                        OutcomeKind::Changed => {
                            changed += 1;
                            first_problem.get_or_insert_with(|| {
                                let detail = outcome.message.unwrap_or_else(|| "entry changed".to_owned());
                                format!("{}: {detail}", display_name(&tree, outcome.node))
                            });
                        }
                        OutcomeKind::Failed => {
                            failed += 1;
                            first_problem.get_or_insert_with(|| {
                                let detail = outcome.message.unwrap_or_else(|| "entry could not be removed".to_owned());
                                format!("{}: {detail}", display_name(&tree, outcome.node))
                            });
                        }
                        OutcomeKind::Cancelled => not_attempted += 1,
                    }
                }
            }
            Ok(DeleteEvent::Progress { completed, total, .. }) => progress.deleting(completed, total),
            Ok(DeleteEvent::Failed { message }) => {
                receive_error = Some(io::Error::other(format!("deletion stopped: {message}")));
                break;
            }
            Ok(DeleteEvent::Finished) => break,
            Err(_) => {
                receive_error = Some(io::Error::other("deletion worker stopped without finishing"));
                break;
            }
        }
    }
    let worker_result = worker
        .join()
        .map_err(|_| io::Error::other("deletion worker terminated unexpectedly"));
    match (receive_error, worker_result) {
        (Some(receive_error), Err(worker_error)) => {
            return Err(io::Error::other(format!("{receive_error}; {worker_error}")).into());
        }
        (Some(error), Ok(())) | (None, Err(error)) => return Err(error.into()),
        (None, Ok(())) => {}
    }
    progress.clear();
    if changed == 0 && failed == 0 && not_attempted == 0 {
        if deleted + already_absent != total {
            return Err(io::Error::other(format!(
                "deletion reported {} outcomes for {total} planned entries",
                deleted + already_absent
            ))
            .into());
        }
        return Ok(());
    }
    let mut summary = format!(
        "Deleted {deleted} · {already_absent} already absent · {changed} changed · {failed} failed · {not_attempted} not attempted"
    );
    if let Some(problem) = first_problem {
        summary.push_str(&format!("; first problem: {problem}"));
    }
    Err(io::Error::other(summary).into())
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
) -> io::Result<()> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "cannot ask for confirmation without a terminal; pass --yes to delete without asking",
        ));
    }
    verify_confirmation_target(root, selected_path)?;
    verify_confirmation_target(root, resolved_path)?;
    let mut stderr = io::stderr().lock();
    write!(
        stderr,
        "Permanently delete {top_level} entries ({total} including nested) below {}? \
         The directory itself is kept. Type 'yes' to continue: ",
        resolved_path.display()
    )?;
    stderr.flush()?;
    if read_confirmation(&mut io::stdin().lock())? {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Interrupted, "not confirmed; nothing was deleted"))
    }
}



fn read_confirmation(input: &mut impl BufRead) -> io::Result<bool> {
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(answer.trim() == "yes")
}

fn display_name(tree: &Tree, id: fdu_core::NodeId) -> String {
    match tree.name(id) {
        Some(name) => String::from_utf8_lossy(name).into_owned(),
        None => "<unknown>".to_owned(),
    }
}


fn require_complete_scan_root(tree: &Tree) -> io::Result<()> {
    if tree.record(tree.root()).expect("root exists").state != NodeState::Complete {
        return Err(io::Error::other("root scan is incomplete"));
    }
    Ok(())
}




fn build_index(root: &RootAnchor, progress: &mut Progress) -> io::Result<Tree> {
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
    let worker = start_indexed_scan(root.clone(), sender, batch_receiver, cancelled);
    let mut fatal: Option<String> = None;
    let mut scanned = 0usize;
    let mut done = false;
    let scan_result = (|| -> io::Result<()> {
        while !done {
        let event = receiver
            .recv()
            .map_err(|_| io::Error::other("scanner stopped without finishing"))?;
        match event {
            ScanEvent::Started { identity, .. } => {
                if identity != tree.record(tree.root()).expect("root exists").identity {
                    tree.mark_incomplete_to_root(tree.root());
                    fatal.get_or_insert("scan root identity changed after it was opened".to_owned());
                }
            }
            ScanEvent::Entries(mut batch) => {
                scanned += batch.entries.len();
                progress.scanning(scanned);
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
        }
        }
        Ok(())
    })();
    let worker_result = worker
        .join()
        .map_err(|_| io::Error::other("scanner worker terminated unexpectedly"));
    match (scan_result, worker_result) {
        (Err(scan_error), Err(worker_error)) => {
            return Err(io::Error::other(format!("{scan_error}; {worker_error}")));
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
        (Ok(()), Ok(())) => {}
    }
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

