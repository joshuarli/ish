use fdu_core::{EntryType, FileIdentity, NodeId, NodeStatus, Tree};
use fdu_delete::{create_plan, execute, DeleteEvent, DeleteOutcome, OutcomeKind};
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp");
        fs::create_dir_all(&base).unwrap();
        let path = base.join(format!("fdu-delete-{name}-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn identity(path: &Path) -> FileIdentity {
    let metadata = fs::symlink_metadata(path).unwrap();
    FileIdentity { device: metadata.dev(), inode: metadata.ino() }
}

fn new_tree(root: &Path) -> Tree {
    let mut tree = Tree::new(b"root", identity(root), 2).unwrap();
    assert!(tree.set_state(tree.root(), NodeStatus::Complete));
    tree
}

fn append(tree: &mut Tree, parent: NodeId, name: &[u8], entry_type: EntryType, identity: FileIdentity) -> NodeId {
    tree.append(parent, name, entry_type, identity, 1, 0, 0, NodeStatus::Complete).unwrap()
}

fn append_to_root(tree: &mut Tree, name: &[u8], entry_type: EntryType, identity: FileIdentity) -> NodeId {
    let root = tree.root();
    append(tree, root, name, entry_type, identity)
}

fn run(root: &Path, tree: Tree, selected: &[NodeId]) -> Vec<DeleteOutcome> {
    let plan = create_plan(Arc::new(tree), selected).unwrap();
    let (sender, receiver) = mpsc::sync_channel(64);
    let descriptor: OwnedFd = fs::File::open(root).unwrap().into();
    execute(descriptor, plan, Arc::new(AtomicBool::new(false)), sender);
    let mut outcomes = Vec::new();
    for event in receiver {
        match event {
            DeleteEvent::Outcomes(batch) => outcomes.extend(batch),
            DeleteEvent::Failed { message } => panic!("deletion failed: {message}"),
            DeleteEvent::Progress { .. } => {}
            DeleteEvent::Finished => return outcomes,
        }
    }
    panic!("deletion ended without Finished");
}

fn entries(root: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(root).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn deleting_a_file_and_a_directory_leaves_no_quarantine_behind() {
    let scratch = Scratch::new("delete");
    let root = &scratch.0;
    fs::write(root.join("file"), b"bytes").unwrap();
    fs::write(root.join("keep"), b"keep").unwrap();
    fs::create_dir(root.join("dir")).unwrap();
    fs::write(root.join("dir/inner"), b"inner").unwrap();
    let mut tree = new_tree(root);
    let file = append_to_root(&mut tree, b"file", EntryType::RegularFile, identity(&root.join("file")));
    let dir = append_to_root(&mut tree, b"dir", EntryType::Directory, identity(&root.join("dir")));
    append(&mut tree, dir, b"inner", EntryType::RegularFile, identity(&root.join("dir/inner")));

    let outcomes = run(root, tree, &[file, dir]);

    assert_eq!(outcomes.len(), 3);
    assert!(outcomes.iter().all(|outcome| outcome.kind == OutcomeKind::Deleted), "{outcomes:?}");
    assert_eq!(entries(root), ["keep"]);
}

#[test]
fn entry_replaced_after_indexing_is_restored_not_deleted() {
    let scratch = Scratch::new("changed");
    let root = &scratch.0;
    fs::write(root.join("file"), b"indexed").unwrap();
    let mut tree = new_tree(root);
    let file = append_to_root(&mut tree, b"file", EntryType::RegularFile, identity(&root.join("file")));
    fs::write(root.join("incoming"), b"replacement").unwrap();
    fs::rename(root.join("incoming"), root.join("file")).unwrap();
    assert_ne!(identity(&root.join("file")), tree.record(file).unwrap().identity);

    let outcomes = run(root, tree, &[file]);

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].kind, OutcomeKind::Changed);
    assert_eq!(fs::read(root.join("file")).unwrap(), b"replacement");
    assert_eq!(entries(root), ["file"]);
}

#[test]
fn entry_already_removed_is_reported_absent() {
    let scratch = Scratch::new("absent");
    let root = &scratch.0;
    fs::write(root.join("file"), b"bytes").unwrap();
    let mut tree = new_tree(root);
    let file = append_to_root(&mut tree, b"file", EntryType::RegularFile, identity(&root.join("file")));
    fs::remove_file(root.join("file")).unwrap();

    let outcomes = run(root, tree, &[file]);

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].kind, OutcomeKind::AlreadyAbsent);
    assert!(entries(root).is_empty());
}

#[test]
fn directory_with_unindexed_child_is_restored_with_its_contents() {
    let scratch = Scratch::new("unindexed");
    let root = &scratch.0;
    fs::create_dir(root.join("dir")).unwrap();
    fs::write(root.join("dir/indexed"), b"indexed").unwrap();
    let mut tree = new_tree(root);
    let dir = append_to_root(&mut tree, b"dir", EntryType::Directory, identity(&root.join("dir")));
    append(&mut tree, dir, b"indexed", EntryType::RegularFile, identity(&root.join("dir/indexed")));
    fs::write(root.join("dir/late"), b"late").unwrap();

    let outcomes = run(root, tree, &[dir]);

    let directory = outcomes.iter().find(|outcome| outcome.node == dir).unwrap();
    assert_eq!(directory.kind, OutcomeKind::Failed);
    assert!(directory.message.as_deref().unwrap().contains("unindexed children"), "{directory:?}");
    assert_eq!(fs::read(root.join("dir/late")).unwrap(), b"late");
    assert_eq!(entries(root), ["dir"]);
}
