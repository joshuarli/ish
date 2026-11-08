use fdu_core::{EntryType, ExclusionReason, FileIdentity, NodeId, NodeStatus, Tree};
use fdu_delete::{create_browser_plan, execute, DeleteEvent, DeleteOutcome, DeletionPlan, Disposition, OutcomeKind};
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

fn test_tree() -> (Arc<Tree>, NodeId, NodeId, NodeId) {
    let identity = FileIdentity { device: 1, inode: 1 };
    let mut tree = Tree::new(b"root", identity, 1).unwrap();
    let directory = tree
        .append(tree.root(), b"dir", EntryType::Directory, FileIdentity { device: 1, inode: 2 }, 2, 10, 512, NodeStatus::Complete)
        .unwrap();
    let file = tree
        .append(directory, b"file", EntryType::RegularFile, FileIdentity { device: 1, inode: 3 }, 1, 10, 512, NodeStatus::Complete)
        .unwrap();
    let second = tree
        .append(directory, b"second", EntryType::RegularFile, FileIdentity { device: 1, inode: 4 }, 1, 0, 0, NodeStatus::Complete)
        .unwrap();
    tree.set_state(tree.root(), NodeStatus::Complete);
    (Arc::new(tree), directory, file, second)
}

#[test]
fn browser_plan_emits_directory_descendants_before_their_parent() {
    let (tree, directory, file, second) = test_tree();
    let plan = create_browser_plan(tree, &[directory]).unwrap();
    assert_eq!(plan.targets, vec![file, second, directory]);
    assert!(plan.skipped.is_empty());
    assert_eq!(plan.apparent_bytes, 10);
}

#[test]
fn browser_scan_errors_inside_a_directory_skip_only_that_entry() {
    let (tree, directory, file, second) = test_tree();
    let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
    mutable.set_state(file, NodeStatus::Incomplete);
    mutable.set_state(directory, NodeStatus::Incomplete);
    mutable.set_state(mutable.root(), NodeStatus::Incomplete);
    let plan = create_browser_plan(Arc::new(mutable), &[directory]).unwrap();
    assert_eq!(plan.targets, vec![second]);
    assert_eq!(plan.roots, vec![directory]);
    assert_eq!(plan.disposition.as_ref().unwrap()[directory.index()], Disposition::Enter);
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].node, file);
    assert_eq!(plan.skipped[0].reason, "scan is incomplete");
}

#[test]
fn browser_directory_with_nothing_deletable_is_one_skipped_entry() {
    let (tree, directory, file, second) = test_tree();
    let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
    mutable.set_state(file, NodeStatus::Incomplete);
    mutable.set_state(second, NodeStatus::Incomplete);
    mutable.set_state(directory, NodeStatus::Incomplete);
    let top_level = mutable
        .append(mutable.root(), b"top", EntryType::RegularFile, FileIdentity { device: 1, inode: 9 }, 1, 5, 512, NodeStatus::Complete)
        .unwrap();
    let plan = create_browser_plan(Arc::new(mutable), &[directory, top_level]).unwrap();
    assert_eq!(plan.targets, vec![top_level]);
    assert_eq!(plan.roots, vec![top_level]);
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].node, directory);
}

#[test]
fn browser_selection_with_nothing_deletable_is_rejected_with_each_reason() {
    let (tree, directory, file, _) = test_tree();
    let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
    mutable.set_state(file, NodeStatus::Incomplete);
    mutable.set_state(directory, NodeStatus::Excluded(ExclusionReason::MountBoundary));
    let rejection = create_browser_plan(Arc::new(mutable), &[directory]).err().expect("nothing to delete");
    assert_eq!(rejection.rejected.len(), 1);
    assert_eq!(rejection.rejected[0].node, Some(directory));
    assert_eq!(rejection.rejected[0].reason, "mount boundary is protected");
}

#[test]
fn browser_incomplete_root_scan_still_plans_complete_entries() {
    let (tree, _, file, _) = test_tree();
    let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
    mutable.set_state(mutable.root(), NodeStatus::Incomplete);
    let plan = create_browser_plan(Arc::new(mutable), &[file]).unwrap();
    assert_eq!(plan.targets, vec![file]);
}

#[test]
fn browser_running_scan_plans_finished_subtrees_and_skips_directories_still_scanning() {
    let (tree, directory, file, second) = test_tree();
    let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
    let scanning = mutable
        .append(mutable.root(), b"scanning", EntryType::Directory, FileIdentity { device: 1, inode: 9 }, 2, 0, 0, NodeStatus::Scanning)
        .unwrap();
    mutable.set_state(mutable.root(), NodeStatus::Scanning);
    let plan = create_browser_plan(Arc::new(mutable), &[directory, scanning]).unwrap();
    assert_eq!(plan.targets, vec![file, second, directory]);
    assert_eq!(plan.disposition.as_ref().unwrap()[scanning.index()], Disposition::Retain);
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].node, scanning);
    assert_eq!(plan.skipped[0].reason, "scan is still in progress");
}

#[test]
fn browser_excluded_entries_are_skipped_and_never_entered() {
    for reason in [ExclusionReason::MountBoundary, ExclusionReason::UnsupportedAlias] {
        let (tree, directory, file, second) = test_tree();
        let mut mutable = Arc::try_unwrap(tree).ok().unwrap();
        let nested = mutable
            .append(directory, b"nested", EntryType::Directory, FileIdentity { device: 1, inode: 8 }, 2, 0, 0, NodeStatus::Excluded(reason))
            .unwrap();
        let plan = create_browser_plan(Arc::new(mutable), &[directory]).unwrap();
        assert_eq!(plan.targets, vec![file, second]);
        assert_eq!(plan.disposition.as_ref().unwrap()[nested.index()], Disposition::Retain);
        assert_eq!(plan.disposition.as_ref().unwrap()[directory.index()], Disposition::Enter);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].node, nested);
    }
}

static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

struct TempRoot {
    parent: PathBuf,
    root: PathBuf,
}

impl TempRoot {
    fn new() -> io::Result<Self> {
        loop {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let parent = std::env::temp_dir().join(format!("fdu-browser-delete-{}-{id}", std::process::id()));
            match fs::create_dir(&parent) {
                Ok(()) => {
                    let root = parent.join("root");
                    fs::create_dir(&root)?;
                    return Ok(Self { parent, root });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.parent);
    }
}

fn execute_plan(root: &Path, plan: DeletionPlan) -> Vec<DeleteOutcome> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(2);
    let root = fs::File::open(root).unwrap();
    let root_fd = OwnedFd::from(root);
    let worker = std::thread::spawn(move || {
        execute(root_fd, plan, cancelled, sender);
    });
    let mut outcomes = Vec::new();
    loop {
        match receiver.recv_timeout(Duration::from_secs(10)).expect("deletion worker reports completion") {
            DeleteEvent::Outcomes(batch) => outcomes.extend(batch),
            DeleteEvent::Failed { message } => panic!("unexpected worker failure: {message}"),
            DeleteEvent::Progress { .. } | DeleteEvent::Diagnostic { .. } => {}
            DeleteEvent::Finished => break,
        }
    }
    worker.join().expect("deletion worker exits");
    outcomes
}

#[test]
fn browser_selective_deletion_deletes_complete_and_preserves_incomplete_under_same_dir() -> io::Result<()> {
    let temp = TempRoot::new()?;
    let selected = temp.root.join("selected");
    fs::create_dir_all(selected.join("unreadable"))?;
    fs::create_dir_all(selected.join("clean/inner"))?;
    fs::write(selected.join("top"), b"1")?;
    fs::write(selected.join("clean/inner/leaf"), b"2")?;
    fs::write(selected.join("unreadable/known"), b"3")?;
    fs::write(selected.join("unreadable/unscanned"), b"4")?;

    let root_meta = fs::symlink_metadata(&temp.root)?;
    let mut tree = Tree::new(
        b"root",
        FileIdentity { device: root_meta.dev(), inode: root_meta.ino() },
        1,
    ).unwrap();

    let sel_meta = fs::symlink_metadata(&selected)?;
    let sel_id = tree.append(
        tree.root(),
        b"selected",
        EntryType::Directory,
        FileIdentity { device: sel_meta.dev(), inode: sel_meta.ino() },
        1, 0, 0, NodeStatus::Incomplete,
    ).unwrap();

    let top_meta = fs::symlink_metadata(selected.join("top"))?;
    tree.append(
        sel_id,
        b"top",
        EntryType::RegularFile,
        FileIdentity { device: top_meta.dev(), inode: top_meta.ino() },
        1, 1, 512, NodeStatus::Complete,
    ).unwrap();

    let clean_meta = fs::symlink_metadata(selected.join("clean"))?;
    let clean_id = tree.append(
        sel_id,
        b"clean",
        EntryType::Directory,
        FileIdentity { device: clean_meta.dev(), inode: clean_meta.ino() },
        1, 0, 0, NodeStatus::Complete,
    ).unwrap();

    let inner_meta = fs::symlink_metadata(selected.join("clean/inner"))?;
    let inner_id = tree.append(
        clean_id,
        b"inner",
        EntryType::Directory,
        FileIdentity { device: inner_meta.dev(), inode: inner_meta.ino() },
        1, 0, 0, NodeStatus::Complete,
    ).unwrap();

    let leaf_meta = fs::symlink_metadata(selected.join("clean/inner/leaf"))?;
    tree.append(
        inner_id,
        b"leaf",
        EntryType::RegularFile,
        FileIdentity { device: leaf_meta.dev(), inode: leaf_meta.ino() },
        1, 1, 512, NodeStatus::Complete,
    ).unwrap();

    let unread_meta = fs::symlink_metadata(selected.join("unreadable"))?;
    let unread_id = tree.append(
        sel_id,
        b"unreadable",
        EntryType::Directory,
        FileIdentity { device: unread_meta.dev(), inode: unread_meta.ino() },
        1, 0, 0, NodeStatus::Incomplete,
    ).unwrap();

    let known_meta = fs::symlink_metadata(selected.join("unreadable/known"))?;
    tree.append(
        unread_id,
        b"known",
        EntryType::RegularFile,
        FileIdentity { device: known_meta.dev(), inode: known_meta.ino() },
        1, 1, 512, NodeStatus::Complete,
    ).unwrap();

    let unscanned_meta = fs::symlink_metadata(selected.join("unreadable/unscanned"))?;
    tree.append(
        unread_id,
        b"unscanned",
        EntryType::RegularFile,
        FileIdentity { device: unscanned_meta.dev(), inode: unscanned_meta.ino() },
        1, 1, 512, NodeStatus::Incomplete,
    ).unwrap();

    tree.set_state(tree.root(), NodeStatus::Incomplete);

    let plan = create_browser_plan(Arc::new(tree), &[sel_id]).unwrap();
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].reason, "scan is incomplete");

    let outcomes = execute_plan(&temp.root, plan);
    assert!(outcomes.iter().all(|item| item.kind == OutcomeKind::Deleted), "{outcomes:?}");

    // "top", "clean", "inner", "leaf", and "known" should be deleted!
    assert!(!selected.join("top").exists());
    assert!(!selected.join("clean").exists());
    assert!(!selected.join("unreadable/known").exists());

    // Incomplete entries must be preserved!
    assert!(selected.exists());
    assert!(selected.join("unreadable").exists());
    assert_eq!(fs::read(selected.join("unreadable/unscanned"))?, b"4");

    Ok(())
}
