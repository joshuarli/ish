use fdu_core::{EntryType, ExclusionReason, NodeId, NodeRecord, NodeState, Tree};
use hashbrown::HashSet;
use std::fmt;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

const OUTCOME_BATCH_SIZE: usize = 2048;


const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

pub struct DeletionPlan {
    tree: Arc<Tree>,
    pub roots: Vec<NodeId>,
    targets: Vec<NodeId>,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
}

impl DeletionPlan {
    pub fn operation_count(&self) -> usize {
        self.targets.len()
    }

    pub fn tree(&self) -> &Arc<Tree> {
        &self.tree
    }
}




#[derive(Debug)]
pub struct Rejection {
    pub node: Option<NodeId>,
    pub reason: String,
}

#[derive(Debug)]
pub struct PlanError {
    pub rejected: Vec<Rejection>,
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} selected entr{} cannot be deleted", self.rejected.len(), if self.rejected.len() == 1 { "y" } else { "ies" })
    }
}

impl std::error::Error for PlanError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutcomeKind {
    Deleted,
    DeletedSyncFailed,
    AlreadyAbsent,
    Changed,
    Failed,
    Cancelled,
}

#[derive(Debug)]
pub struct DeleteOutcome {
    pub node: NodeId,
    pub kind: OutcomeKind,
    pub message: Option<String>,
}

#[derive(Debug)]
pub enum DeleteEvent {
    Progress { completed: usize, total: usize, current: NodeId },
    Outcomes(Vec<DeleteOutcome>),
    Failed { message: String },
    Finished,
}





pub fn create_plan(tree: Arc<Tree>, selected: &[NodeId]) -> Result<DeletionPlan, PlanError> {
    let mut rejected = Vec::new();
    if selected.is_empty() {
        rejected.push(Rejection { node: None, reason: "no entries are selected".to_owned() });
    }
    let mut unique = Vec::with_capacity(selected.len());
    let mut seen = HashSet::with_capacity(selected.len());
    for id in selected {
        if seen.insert(*id) {
            unique.push(*id);
        }
    }
    for id in &unique {
        let reject = |reason: &str| Rejection { node: Some(*id), reason: reason.to_owned() };
        match tree.record(*id) {
            None => rejected.push(reject("absent from the index")),
            Some(record) if *id == tree.root() || record.entry_type == EntryType::Root => {
                rejected.push(reject("the scan root is protected"));
            }
            Some(_) if has_selected_ancestor(&tree, &seen, *id) => {
                rejected.push(reject("already covered by a selected parent directory"));
            }
            Some(record) => {
                if let Some(reason) = ineligible_reason(&tree, *id, record.entry_type) {
                    rejected.push(reject(reason));
                }
            }
        }
    }
    if !unique.is_empty() && tree.record(tree.root()).expect("root record exists").state != NodeState::Complete {
        rejected.push(Rejection { node: None, reason: "root scan is not complete".to_owned() });
    }
    if !rejected.is_empty() {
        return Err(PlanError { rejected });
    }

    let overflow = |what: &str| PlanError {
        rejected: vec![Rejection { node: None, reason: format!("selected {what} byte total exceeds u64") }],
    };
    let mut targets = Vec::new();
    let mut apparent_bytes = 0u64;
    let mut allocated_bytes = 0u64;
    for root in &unique {
        let record = tree.record(*root).expect("validated deletion root");
        apparent_bytes = apparent_bytes.checked_add(record.apparent_bytes).ok_or_else(|| overflow("apparent"))?;
        allocated_bytes = allocated_bytes.checked_add(record.allocated_bytes).ok_or_else(|| overflow("allocated"))?;
        append_postorder(&tree, *root, &mut targets);
    }
    Ok(DeletionPlan {
        tree,
        roots: unique,
        targets,
        apparent_bytes,
        allocated_bytes,
    })
}

fn has_selected_ancestor(tree: &Tree, selected: &HashSet<NodeId>, id: NodeId) -> bool {
    let mut current = tree.record(id).and_then(|record| record.parent());
    while let Some(ancestor) = current {
        if selected.contains(&ancestor) {
            return true;
        }
        current = tree.record(ancestor).and_then(|record| record.parent());
    }
    false
}

fn ineligible_reason(tree: &Tree, id: NodeId, entry_type: EntryType) -> Option<&'static str> {
    let record = tree.record(id)?;
    match record.state {
        NodeState::Excluded(ExclusionReason::MountBoundary) => return Some("mount boundary is protected"),
        NodeState::Excluded(ExclusionReason::UnsupportedAlias) => return Some("unsupported directory alias is protected"),
        NodeState::Complete => {}
        NodeState::Scanning => return Some("scan is still in progress"),
        NodeState::Incomplete => return Some("scan is incomplete"),
        NodeState::Stale => return Some("entry totals are stale; rescan first"),
        NodeState::Tombstone => return Some("entry has already been removed from the index"),
    }
    if entry_type != EntryType::Directory {
        return None;
    }
    let mut pending = vec![WalkFrame::Children {
        next: record.first_child(),
        directories: false,
    }];
    while let Some(WalkFrame::Children { next, directories }) = pending.pop() {
        let Some(child) = next else {
            continue;
        };
        let Some(child_record) = tree.record(child) else {
            return Some("directory index is invalid");
        };
        pending.push(WalkFrame::Children {
            next: child_record.next_sibling(),
            directories,
        });
        if child_record.entry_type == EntryType::Directory {
            pending.push(WalkFrame::Children {
                next: child_record.first_child(),
                directories: false,
            });
        }
        match child_record.state {
            NodeState::Complete => {}
            NodeState::Excluded(ExclusionReason::MountBoundary) => return Some("subtree contains an excluded mount boundary"),
            NodeState::Excluded(ExclusionReason::UnsupportedAlias) => return Some("subtree contains an unsupported directory alias"),
            NodeState::Scanning => return Some("subtree scan is incomplete"),
            NodeState::Incomplete => return Some("subtree contains scan errors"),
            NodeState::Stale => return Some("subtree totals are stale; rescan first"),
            NodeState::Tombstone => return Some("subtree changed during a deletion operation"),
        }
    }
    None
}

enum WalkFrame {
    Enter(NodeId),
    Children { next: Option<NodeId>, directories: bool },
    Exit(NodeId),
}

fn append_postorder(tree: &Tree, root: NodeId, output: &mut Vec<NodeId>) {
    let mut stack = vec![WalkFrame::Enter(root)];
    while let Some(frame) = stack.pop() {
        match frame {
            WalkFrame::Enter(id) => {
                let record = tree.record(id).expect("validated deletion subtree");
                if record.entry_type == EntryType::Directory {
                    stack.push(WalkFrame::Exit(id));
                    stack.push(WalkFrame::Children {
                        next: record.first_child(),
                        directories: true,
                    });
                    stack.push(WalkFrame::Children {
                        next: record.first_child(),
                        directories: false,
                    });
                } else {
                    output.push(id);
                }
            }
            WalkFrame::Children { next: Some(child), directories } => {
                let Some(record) = tree.record(child) else {
                    continue;
                };
                stack.push(WalkFrame::Children {
                    next: record.next_sibling(),
                    directories,
                });
                if (record.entry_type == EntryType::Directory) == directories {
                    stack.push(WalkFrame::Enter(child));
                }
            }
            WalkFrame::Children { next: None, .. } => {}
            WalkFrame::Exit(id) => output.push(id),
        }
    }
}






pub fn execute(
    root: OwnedFd,
    plan: DeletionPlan,
    cancelled: Arc<AtomicBool>,
    sender: SyncSender<DeleteEvent>,
) {
    posix::run(root, plan, cancelled, sender);
}

mod posix {
    use fdu_posix::quarantine;
    use fdu_posix::identity::{DIRECTORY_FLAGS, MountIdentity, directory_state, identity, entry_type, retry_errno};
    use super::*;
    use rustix::fs::{self, AtFlags, Mode, Stat};
    use rustix::io::Errno;
    use std::collections::BTreeMap;
    use std::os::fd::{AsFd, BorrowedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::Mutex;
    use std::time::Instant;

    const DELETE_THREADS: usize = 16;

    
    
    struct DirContext {
        fd: OwnedFd,
        node: NodeId,
    }

    struct DirectoryWalkFrame {
        node: NodeId,
        parent: NodeId,
        children: Vec<NodeId>,
        next_child: usize,
    }

    struct Deletion<'a> {
        tree: &'a Tree,
        quarantine: &'a quarantine::Session,
        root_mount: MountIdentity,
        cancelled: &'a AtomicBool,
        sender: &'a SyncSender<DeleteEvent>,
        after_operation: &'a (dyn Fn(usize) + Sync),
        recorded: Vec<AtomicBool>,
        outcomes: Mutex<Vec<DeleteOutcome>>,
        completed: AtomicUsize,
        total: usize,
        started: Instant,
        last_progress_nanos: AtomicU64,
        receiver_gone: AtomicBool,
    }

    pub(super) fn run(
        root: OwnedFd,
        plan: DeletionPlan,
        cancelled: Arc<AtomicBool>,
        sender: SyncSender<DeleteEvent>,
    ) {
        run_with(root, plan, cancelled, sender, DELETE_THREADS, |_| {});
    }

    pub(super) fn run_with(
        root: OwnedFd,
        plan: DeletionPlan,
        cancelled: Arc<AtomicBool>,
        sender: SyncSender<DeleteEvent>,
        threads: usize,
        after_operation: impl Fn(usize) + Sync,
    ) {
        run_inner(root, plan, cancelled, sender, threads, after_operation);
    }

    fn run_inner(
        root: OwnedFd,
        plan: DeletionPlan,
        cancelled: Arc<AtomicBool>,
        sender: SyncSender<DeleteEvent>,
        threads: usize,
        after_operation: impl Fn(usize) + Sync,
    ) {
        let tree: &Tree = &plan.tree;
        let root_record = tree.record(tree.root()).expect("tree has root");
        let expected_root = root_record.identity;
        let root_mount = match directory_state(root.as_fd()) {
            Ok((identity, mount)) if identity == expected_root => mount,
            Ok(_) => {
                send_failure(&sender, "scan root identity changed before deletion");
                return;
            }
            Err(error) => {
                send_failure(&sender, &error.to_string());
                return;
            }
        };
        let quarantine = match quarantine::Session::start(root.as_fd()) {
            Ok(session) => session,
            Err(error) => { send_failure(&sender, &error.to_string()); return; }
        };
        let deletion = Deletion {
            tree,
            quarantine: &quarantine,
            root_mount,
            cancelled: &cancelled,
            sender: &sender,
            after_operation: &after_operation,
            recorded: (0..tree.len()).map(|_| AtomicBool::new(false)).collect(),
            outcomes: Mutex::new(Vec::with_capacity(OUTCOME_BATCH_SIZE)),
            completed: AtomicUsize::new(0),
            total: plan.targets.len(),
            started: Instant::now(),
            last_progress_nanos: AtomicU64::new(0),
            receiver_gone: AtomicBool::new(false),
        };

        
        
        let mut groups: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
        for selected in &plan.roots {
            let parent = tree
                .record(*selected)
                .and_then(NodeRecord::parent)
                .expect("deletion targets have parents");
            groups.entry(parent.index()).or_default().push(*selected);
        }
        let root_context = match root.try_clone() {
            Ok(fd) => Arc::new(DirContext { fd, node: tree.root() }),
            Err(error) => {
                send_failure(&sender, &error.to_string());
                return;
            }
        };
        let pool = match rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(|index| format!("fdu-delete-{index}"))
            .build()
        {
            Ok(pool) => pool,
            Err(error) => {
                send_failure(&sender, &error.to_string());
                return;
            }
        };
        let deletion = &deletion;
        pool.scope(|scope| {
            for (parent, selected) in groups {
                let root_context = Arc::clone(&root_context);
                scope.spawn(move |scope| {
                    let parent = NodeId::from_index(parent).expect("indexed parent");
                    delete_group(deletion, &root_context, parent, selected, scope);
                });
            }
        });
        drop(pool);

        if deletion.receiver_gone.load(Ordering::Relaxed) {
            return;
        }
        
        let mut outcomes = std::mem::take(&mut *deletion.outcomes.lock().expect("outcomes are not poisoned"));
        for node in plan.targets.iter().copied() {
            if !deletion.recorded[node.index()].swap(true, Ordering::Relaxed) {
                outcomes.push(DeleteOutcome { node, kind: OutcomeKind::Cancelled, message: None });
                if outcomes.len() >= OUTCOME_BATCH_SIZE && !flush(&sender, &mut outcomes) {
                    return;
                }
            }
        }
        if !flush(&sender, &mut outcomes) {
            return;
        }
        if let Err(error) = quarantine.finish() {
            send_failure(&sender, &error.to_string());
            return;
        }
        let _ = sender.send(DeleteEvent::Finished);
    }

    impl Deletion<'_> {
        fn stopped(&self) -> bool {
            self.cancelled.load(Ordering::Relaxed) || self.receiver_gone.load(Ordering::Relaxed) || self.quarantine.recovery_required()
        }

        fn record(&self, node: NodeId, kind: OutcomeKind, message: Option<String>) {
            if self.recorded[node.index()].swap(true, Ordering::Relaxed) {
                return;
            }
            let completed = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
            (self.after_operation)(completed);
            let batch = {
                let mut outcomes = self.outcomes.lock().expect("outcomes are not poisoned");
                outcomes.push(DeleteOutcome { node, kind, message });
                (outcomes.len() >= OUTCOME_BATCH_SIZE)
                    .then(|| std::mem::replace(&mut *outcomes, Vec::with_capacity(OUTCOME_BATCH_SIZE)))
            };
            if let Some(batch) = batch {
                if self.sender.send(DeleteEvent::Outcomes(batch)).is_err() {
                    self.receiver_gone.store(true, Ordering::Relaxed);
                }
            }
            self.report_progress(completed, node);
        }

        
        
        fn report_progress(&self, completed: usize, current: NodeId) {
            let now = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let interval = u64::try_from(PROGRESS_INTERVAL.as_nanos()).unwrap_or(u64::MAX);
            let last = self.last_progress_nanos.load(Ordering::Relaxed);
            if now.saturating_sub(last) < interval
                || self
                    .last_progress_nanos
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_err()
            {
                return;
            }
            match self.sender.try_send(DeleteEvent::Progress { completed, total: self.total, current }) {
                
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    self.receiver_gone.store(true, Ordering::Relaxed);
                }
            }
        }

        
        
        fn record_below(&self, node: NodeId, kind: OutcomeKind, message: &str) {
            let mut stack = vec![node];
            while let Some(current) = stack.pop() {
                for child in self.tree.children(current) {
                    if self.tree.record(child).is_some_and(|record| record.entry_type == EntryType::Directory) {
                        stack.push(child);
                    }
                    self.record(child, kind, Some(message.to_owned()));
                }
            }
        }
    }

    fn delete_group<'scope>(
        deletion: &'scope Deletion<'_>,
        root_context: &Arc<DirContext>,
        parent: NodeId,
        selected: Vec<NodeId>,
        scope: &rayon::Scope<'scope>,
    ) {
        let context = match open_context(deletion, root_context, parent) {
            Ok(context) => context,
            Err(message) => {
                for node in selected {
                    deletion.record(node, OutcomeKind::Failed, Some(message.clone()));
                    deletion.record_below(node, OutcomeKind::Failed, &message);
                }
                return;
            }
        };
        for node in selected {
            if deletion.stopped() {
                return;
            }
            let is_directory = deletion
                .tree
                .record(node)
                .is_some_and(|record| record.entry_type == EntryType::Directory);
            if is_directory {
                let context = Arc::clone(&context);
                scope.spawn(move |_| delete_directory(deletion, context, node));
            } else {
                delete_file(deletion, context.fd.as_fd(), node);
            }
        }
    }

    fn open_context(
        deletion: &Deletion<'_>,
        root_context: &Arc<DirContext>,
        node: NodeId,
    ) -> Result<Arc<DirContext>, String> {
        let tree = deletion.tree;
        let mut path = Vec::new();
        let mut current = Some(node);
        while let Some(next) = current {
            if next == tree.root() {
                break;
            }
            path.push(next);
            current = tree.record(next).and_then(NodeRecord::parent);
            if current.is_none() {
                return Err("parent missing from index".to_owned());
            }
        }
        let mut fd = root_context.fd.try_clone().map_err(|error| error.to_string())?;
        let mut current = root_context.node;
        for next in path.into_iter().rev() {
            fd = open_checked(deletion, fd.as_fd(), next).map_err(|(_, message)| message)?;
            current = next;
        }
        Ok(Arc::new(DirContext { fd, node: current }))
    }

    fn open_checked(
        deletion: &Deletion<'_>,
        parent: BorrowedFd<'_>,
        node: NodeId,
    ) -> Result<OwnedFd, (OutcomeKind, String)> {
        let record = deletion.tree.record(node).expect("path node exists");
        let name = deletion.tree.name(node).ok_or((OutcomeKind::Failed, "directory name missing".to_owned()))?;
        let fd = match retry_errno(|| fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Err((OutcomeKind::AlreadyAbsent, "directory is already gone".to_owned())),
            Err(error) => {
                if let Ok(latest) = stat_at(parent, name) {
                    if identity(&latest).ok() != Some(record.identity) || entry_type(&latest) != record.entry_type {
                        return Err((OutcomeKind::Changed, "directory was replaced during validation".to_owned()));
                    }
                }
                return Err((OutcomeKind::Failed, io::Error::from(error).to_string()));
            }
        };
        let (found, mount) = directory_state(fd.as_fd()).map_err(|error| (OutcomeKind::Failed, error.to_string()))?;
        if found != record.identity {
            return Err((OutcomeKind::Changed, "directory was replaced".to_owned()));
        }
        if mount != deletion.root_mount {
            return Err((OutcomeKind::Changed, "directory became a mount boundary".to_owned()));
        }
        Ok(fd)
    }

    fn delete_directory(
        deletion: &Deletion<'_>,
        parent: Arc<DirContext>,
        node: NodeId,
    ) {
        if deletion.stopped() {
            return;
        }
        let mut current = match open_checked(deletion, parent.fd.as_fd(), node) {
            Ok(fd) => fd,
            Err((kind, message)) => {
                deletion.record(node, kind, Some(message.clone()));
                let below = if kind == OutcomeKind::AlreadyAbsent { OutcomeKind::AlreadyAbsent } else { OutcomeKind::Failed };
                deletion.record_below(node, below, &message);
                return;
            }
        };
        let mut stack = vec![directory_walk_frame(deletion.tree, node, parent.node)];

        loop {
            if deletion.stopped() {
                return;
            }

            let child = {
                let frame = stack.last_mut().expect("directory frame exists");
                let child = frame.children.get(frame.next_child).copied();
                frame.next_child += usize::from(child.is_some());
                child
            };
            if let Some(child) = child {
                if deletion.tree.record(child).expect("indexed child").entry_type == EntryType::Directory {
                    match open_checked(deletion, current.as_fd(), child) {
                        Ok(directory) => {
                            let parent_node = stack.last().expect("parent directory frame exists").node;
                            current = directory;
                            stack.push(directory_walk_frame(deletion.tree, child, parent_node));
                        }
                        Err((kind, message)) => {
                            deletion.record(child, kind, Some(message.clone()));
                            let below = if kind == OutcomeKind::AlreadyAbsent { OutcomeKind::AlreadyAbsent } else { OutcomeKind::Failed };
                            deletion.record_below(child, below, &message);
                        }
                    }
                } else {
                    delete_file(deletion, current.as_fd(), child);
                }
                continue;
            }

            let frame = stack.last().expect("directory frame exists");
            let finished = frame.node;
            if stack.len() == 1 {
                remove_directory(deletion, parent.fd.as_fd(), finished);
                return;
            }

            let expected_parent = frame.parent;
            match open_parent_checked(deletion, current.as_fd(), expected_parent) {
                Ok(parent_fd) => {
                    remove_directory(deletion, parent_fd.as_fd(), finished);
                    stack.pop();
                    current = parent_fd;
                }
                Err((kind, message)) => {
                    let selected = stack[0].node;
                    deletion.record(finished, kind, Some(message.clone()));
                    deletion.record(selected, OutcomeKind::Failed, Some(message.clone()));
                    deletion.record_below(selected, OutcomeKind::Failed, &message);
                    return;
                }
            }
        }
    }

    fn directory_walk_frame(tree: &Tree, node: NodeId, parent: NodeId) -> DirectoryWalkFrame {
        let mut children: Vec<_> = tree.children(node).collect();
        children.sort_unstable_by_key(|child| {
            let record = tree.record(*child).expect("indexed child");
            (record.entry_type == EntryType::Directory, record.identity.inode)
        });
        DirectoryWalkFrame { node, parent, children, next_child: 0 }
    }

    fn open_parent_checked(
        deletion: &Deletion<'_>,
        child: BorrowedFd<'_>,
        parent: NodeId,
    ) -> Result<OwnedFd, (OutcomeKind, String)> {
        let fd = retry_errno(|| fs::openat(child, c"..", DIRECTORY_FLAGS, Mode::empty()))
            .map_err(|error| (OutcomeKind::Failed, io::Error::from(error).to_string()))?;
        let (found, mount) = directory_state(fd.as_fd()).map_err(|error| (OutcomeKind::Failed, error.to_string()))?;
        let expected = deletion.tree.record(parent).expect("parent directory is indexed");
        if found != expected.identity {
            return Err((OutcomeKind::Changed, "parent directory changed during deletion".to_owned()));
        }
        if mount != deletion.root_mount {
            return Err((OutcomeKind::Changed, "parent became a mount boundary".to_owned()));
        }
        Ok(fd)
    }

    fn remove_directory(deletion: &Deletion<'_>, parent_fd: BorrowedFd<'_>, node: NodeId) {
        let record = deletion.tree.record(node).expect("deletion target exists");
        let name = deletion.tree.name(node).expect("indexed entry has name");
        let latest = match stat_at(parent_fd, name) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return deletion.record(node, OutcomeKind::AlreadyAbsent, None),
            Err(error) => return deletion.record(node, OutcomeKind::Failed, Some(io::Error::from(error).to_string())),
        };
        match identity(&latest) {
            Ok(found) if found == record.identity && entry_type(&latest) == record.entry_type => {}
            Ok(_) => {
                return deletion.record(
                    node,
                    OutcomeKind::Changed,
                    Some("entry changed immediately before removal".to_owned()),
                );
            }
            Err(error) => return deletion.record(node, OutcomeKind::Failed, Some(error.to_string())),
        }
        capture_and_remove(deletion, parent_fd, name, node, AtFlags::REMOVEDIR);
    }

    fn delete_file(deletion: &Deletion<'_>, parent_fd: BorrowedFd<'_>, node: NodeId) {
        let record = deletion.tree.record(node).expect("deletion target exists");
        let name = deletion.tree.name(node).expect("indexed entry has name");
        let current = match stat_at(parent_fd, name) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return deletion.record(node, OutcomeKind::AlreadyAbsent, None),
            Err(error) => return deletion.record(node, OutcomeKind::Failed, Some(io::Error::from(error).to_string())),
        };
        match identity(&current) {
            Ok(found) if found == record.identity && entry_type(&current) == record.entry_type => {}
            Ok(_) => {
                return deletion.record(
                    node,
                    OutcomeKind::Changed,
                    Some("entry was replaced since scanning".to_owned()),
                );
            }
            Err(error) => return deletion.record(node, OutcomeKind::Failed, Some(error.to_string())),
        }
        capture_and_remove(deletion, parent_fd, name, node, AtFlags::empty());
    }

    fn capture_and_remove(deletion: &Deletion<'_>, parent_fd: BorrowedFd<'_>, name: &[u8], node: NodeId, flags: AtFlags) {
        let _ = flags;
        let mut parents = Vec::new();
        let mut current = deletion.tree.record(node).and_then(NodeRecord::parent);
        while let Some(parent) = current {
            if parent == deletion.tree.root() { break; }
            let record = deletion.tree.record(parent).expect("indexed ancestor");
            parents.push(quarantine::Ancestor { name: std::ffi::OsString::from(std::ffi::OsStr::from_bytes(deletion.tree.name(parent).expect("indexed ancestor name"))), identity: record.identity });
            current = record.parent();
        }
        parents.reverse();
        let record = deletion.tree.record(node).expect("indexed target");
        let request = quarantine::Retirement::new(parents, std::ffi::OsString::from(std::ffi::OsStr::from_bytes(name)), fdu_posix::identity::EntryIdentity { file: record.identity, kind: record.entry_type });
        let result = request.map_err(|error| quarantine::RemovalError { removed: false, error }).and_then(|request| deletion.quarantine.remove(parent_fd, request, |boundary, _, _| {
            if matches!(boundary, quarantine::Boundary::BeforeCapture | quarantine::Boundary::BeforeUnlink) && deletion.stopped() { return Err(io::Error::new(io::ErrorKind::Interrupted, "deletion cancelled")); }
            Ok(())
        }));
        let (kind, message) = removal_outcome(result);
        deletion.record(node, kind, message);
    }

    fn removal_outcome(result: Result<quarantine::Removal, quarantine::RemovalError>) -> (OutcomeKind, Option<String>) {
        match result {
            Ok(quarantine::Removal::Removed) => (OutcomeKind::Deleted, None),
            Ok(quarantine::Removal::AlreadyAbsent) => (OutcomeKind::AlreadyAbsent, None),
            Ok(quarantine::Removal::Changed) => (OutcomeKind::Changed, None),
            Err(error) => (if error.removed { OutcomeKind::DeletedSyncFailed } else { OutcomeKind::Failed }, Some(error.to_string())),
        }
    }

    fn stat_at(parent: BorrowedFd<'_>, name: &[u8]) -> Result<Stat, Errno> {
        retry_errno(|| fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW))
    }


    fn flush(sender: &SyncSender<DeleteEvent>, outcomes: &mut Vec<DeleteOutcome>) -> bool {
        if outcomes.is_empty() {
            return true;
        }
        let values = std::mem::replace(outcomes, Vec::with_capacity(OUTCOME_BATCH_SIZE));
        sender.send(DeleteEvent::Outcomes(values)).is_ok()
    }

    fn send_failure(sender: &SyncSender<DeleteEvent>, message: &str) {
        let _ = sender.send(DeleteEvent::Failed {
            message: message.to_owned(),
        });
        let _ = sender.send(DeleteEvent::Finished);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn removed_entry_with_sync_error_is_reported_removed_with_failure() {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
            std::fs::create_dir_all(&base).unwrap();
            let path = base.join(format!("fdu-sync-failure-{}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("source"), b"removed bytes").unwrap();
            let root = std::fs::File::open(&path).unwrap();
            let expected = fdu_posix::identity::entry_identity(&stat_at(root.as_fd(), b"source").unwrap()).unwrap();
            let session = quarantine::Session::start(root.as_fd()).unwrap();
            let request = quarantine::Retirement::new(Vec::new(), "source".into(), expected).unwrap();
            let result = session.remove(root.as_fd(), request, |boundary, _, _| {
                if boundary == quarantine::Boundary::Unlinked { return Err(io::Error::other("injected synchronization failure")); }
                Ok(())
            });
            let (kind, message) = removal_outcome(result);
            assert_eq!(kind, OutcomeKind::DeletedSyncFailed);
            assert!(message.unwrap().contains("injected synchronization failure"));
            assert!(!path.join("source").exists());
            assert!(session.finish().is_err());
            drop(session);
            std::fs::remove_dir_all(path).unwrap();
        }
    }

}

