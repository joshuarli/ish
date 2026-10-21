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

fn try_run(root: &Path, tree: Tree, selected: &[NodeId]) -> Result<Vec<DeleteOutcome>, String> {
    let plan = create_plan(Arc::new(tree), selected).unwrap();
    let (sender, receiver) = mpsc::sync_channel(64);
    let descriptor: OwnedFd = fs::File::open(root).unwrap().into();
    execute(descriptor, plan, Arc::new(AtomicBool::new(false)), sender);
    let mut outcomes = Vec::new();
    for event in receiver {
        match event {
            DeleteEvent::Outcomes(batch) => outcomes.extend(batch),
            DeleteEvent::Failed { message } => return Err(message),
            DeleteEvent::Progress { .. } => {}
            DeleteEvent::Finished => return Ok(outcomes),
        }
    }
    panic!("deletion ended without Finished");
}

fn run(root: &Path, tree: Tree, selected: &[NodeId]) -> Vec<DeleteOutcome> {
    try_run(root, tree, selected).unwrap_or_else(|message| panic!("deletion failed: {message}"))
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

const WORKSPACE: &str = ".fdu-quarantine-v1-00112233445566778899aabbccddeeff";
const HEADER_MAGIC: &[u8] = b"fdu quarantine\0\x01";
const INTENT_MAGIC: &[u8] = b"fdu capture\0\x01";

fn identity_bytes(identity: FileIdentity) -> Vec<u8> {
    let mut bytes = identity.device.to_le_bytes().to_vec();
    bytes.extend_from_slice(&identity.inode.to_le_bytes());
    bytes
}

fn header(root: FileIdentity) -> Vec<u8> {
    let mut bytes = HEADER_MAGIC.to_vec();
    bytes.extend(identity_bytes(root));
    bytes
}

fn intent(root: FileIdentity, parents: &[(&str, FileIdentity)], name: &str) -> Vec<u8> {
    let mut bytes = INTENT_MAGIC.to_vec();
    bytes.extend(identity_bytes(root));
    bytes.extend((parents.len() as u32).to_le_bytes());
    for (parent, identity) in parents {
        bytes.extend((parent.len() as u32).to_le_bytes());
        bytes.extend(parent.as_bytes());
        bytes.extend(identity_bytes(*identity));
    }
    bytes.extend((name.len() as u32).to_le_bytes());
    bytes.extend(name.as_bytes());
    bytes
}

fn workspace(root: &Path) -> PathBuf {
    let path = root.join(WORKSPACE);
    fs::create_dir(&path).unwrap();
    fs::write(path.join("header"), header(identity(root))).unwrap();
    path
}

fn delete_victim(root: &Path) -> Result<Vec<DeleteOutcome>, String> {
    fs::write(root.join("victim"), b"victim").unwrap();
    let mut tree = new_tree(root);
    let victim = append_to_root(&mut tree, b"victim", EntryType::RegularFile, identity(&root.join("victim")));
    try_run(root, tree, &[victim])
}

fn assert_recovered_then_idempotent(root: &Path, result: Result<Vec<DeleteOutcome>, String>) {
    let message = result.unwrap_err();
    assert!(message.contains("interrupted deletion recovered"), "{message}");
    assert_eq!(fs::read(root.join("victim")).unwrap(), b"victim", "recovery must not delete the selection");
    assert!(!root.join(WORKSPACE).exists());
    let outcomes = {
        let mut tree = new_tree(root);
        let victim = append_to_root(&mut tree, b"victim", EntryType::RegularFile, identity(&root.join("victim")));
        run(root, tree, &[victim])
    };
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].kind, OutcomeKind::Deleted);
}

#[test]
fn completed_intent_is_cleared_without_touching_the_selection() {
    let scratch = Scratch::new("completed");
    let root = &scratch.0;
    let workspace = workspace(root);
    fs::write(workspace.join("3.intent"), intent(identity(root), &[], "gone")).unwrap();

    let result = delete_victim(root);

    assert_recovered_then_idempotent(root, result);
    assert!(!root.join("gone").exists());
    assert_eq!(entries(root), Vec::<String>::new());
}

#[test]
fn pending_capture_is_restored_to_its_recorded_path() {
    let scratch = Scratch::new("restore");
    let root = &scratch.0;
    fs::create_dir(root.join("sub")).unwrap();
    let workspace = workspace(root);
    fs::write(workspace.join("4.entry"), b"captured").unwrap();
    fs::write(workspace.join("4.intent"), intent(identity(root), &[("sub", identity(&root.join("sub")))], "original")).unwrap();
    fs::write(workspace.join("5.entry"), b"top").unwrap();
    fs::write(workspace.join("5.intent"), intent(identity(root), &[], "top-level")).unwrap();
    fs::write(workspace.join("pending-6"), b"torn").unwrap();

    let result = delete_victim(root);

    let message = result.unwrap_err();
    assert!(message.contains("interrupted deletion recovered"), "{message}");
    assert_eq!(fs::read(root.join("sub/original")).unwrap(), b"captured");
    assert_eq!(fs::read(root.join("top-level")).unwrap(), b"top");
    assert!(!root.join(WORKSPACE).exists());
    fs::remove_file(root.join("sub/original")).unwrap();
    fs::remove_file(root.join("top-level")).unwrap();
    fs::remove_dir(root.join("sub")).unwrap();
    fs::remove_file(root.join("victim")).unwrap();
    assert_eq!(delete_victim(root).unwrap()[0].kind, OutcomeKind::Deleted);
}

#[test]
fn capture_is_not_restored_over_a_recreated_original() {
    let scratch = Scratch::new("occupied");
    let root = &scratch.0;
    let workspace = workspace(root);
    fs::write(workspace.join("4.entry"), b"captured").unwrap();
    fs::write(workspace.join("4.intent"), intent(identity(root), &[], "original")).unwrap();
    fs::write(root.join("original"), b"newer").unwrap();

    let message = delete_victim(root).unwrap_err();

    assert!(message.contains("retained at scan root/"), "{message}");
    assert_eq!(fs::read(root.join("original")).unwrap(), b"newer");
    assert_eq!(fs::read(workspace.join("4.entry")).unwrap(), b"captured");
    assert!(workspace.join("4.intent").exists());
    assert_eq!(fs::read(root.join("victim")).unwrap(), b"victim");
}

#[test]
fn capture_whose_ancestor_identity_changed_is_retained() {
    let scratch = Scratch::new("ancestor");
    let root = &scratch.0;
    fs::create_dir(root.join("sub")).unwrap();
    let recorded = identity(&root.join("sub"));
    fs::create_dir(root.join("other")).unwrap();
    fs::remove_dir(root.join("sub")).unwrap();
    fs::rename(root.join("other"), root.join("sub")).unwrap();
    assert_ne!(identity(&root.join("sub")), recorded);
    let workspace = workspace(root);
    fs::write(workspace.join("4.entry"), b"captured").unwrap();
    fs::write(workspace.join("4.intent"), intent(identity(root), &[("sub", recorded)], "original")).unwrap();

    let message = delete_victim(root).unwrap_err();

    assert!(message.contains("retained at scan root/"), "{message}");
    assert_eq!(fs::read(workspace.join("4.entry")).unwrap(), b"captured");
    assert_eq!(entries(&root.join("sub")), Vec::<String>::new());
    assert_eq!(fs::read(root.join("victim")).unwrap(), b"victim");
}

fn assert_rejected_untouched(root: &Path, expect: &str) {
    let mut before = tree_snapshot(root);
    before.retain(|(path, _)| path != "victim");
    let message = delete_victim(root).unwrap_err();
    assert!(message.contains(expect), "{message}");
    assert!(message.contains("retained at scan root/"), "{message}");
    let mut after = tree_snapshot(root);
    after.retain(|(path, _)| path != "victim");
    assert_eq!(before, after);
    assert_eq!(fs::read(root.join("victim")).unwrap(), b"victim");
}

fn tree_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                out.push((name, Vec::new()));
                walk(base, &path, out);
            } else {
                out.push((name, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn damaged_header_is_rejected_and_nothing_is_removed() {
    let scratch = Scratch::new("bad-header");
    let root = &scratch.0;
    let workspace = workspace(root);
    fs::write(workspace.join("header"), b"garbage").unwrap();
    fs::write(workspace.join("4.entry"), b"captured").unwrap();
    fs::write(workspace.join("4.intent"), intent(identity(root), &[], "original")).unwrap();

    assert_rejected_untouched(root, "damaged quarantine header");
    assert!(!root.join("original").exists());
}

#[test]
fn header_for_another_root_is_rejected() {
    let scratch = Scratch::new("foreign-header");
    let root = &scratch.0;
    let workspace = workspace(root);
    fs::write(workspace.join("header"), header(FileIdentity { device: 1, inode: 2 })).unwrap();
    fs::write(workspace.join("4.entry"), b"captured").unwrap();
    fs::write(workspace.join("4.intent"), intent(identity(root), &[], "original")).unwrap();

    assert_rejected_untouched(root, "root identity");
    assert!(!root.join("original").exists());
}

#[test]
fn quarantine_lookalike_without_a_header_is_rejected() {
    let scratch = Scratch::new("headerless");
    let root = &scratch.0;
    fs::create_dir(root.join(WORKSPACE)).unwrap();
    fs::write(root.join(WORKSPACE).join("user-file"), b"user data").unwrap();

    assert_rejected_untouched(root, "ambiguous quarantine directory");
}

#[test]
fn unrecognized_workspace_entry_is_rejected_and_kept() {
    let scratch = Scratch::new("unknown-entry");
    let root = &scratch.0;
    let workspace = workspace(root);
    fs::write(workspace.join("notes.txt"), b"unknown").unwrap();

    assert_rejected_untouched(root, "unrecognized quarantine entry");
}

#[test]
fn damaged_intents_retain_their_capture() {
    let good = intent(FileIdentity { device: 0, inode: 0 }, &[], "original");
    let mut trailing = good.clone();
    trailing.push(0);
    let mut escaping = intent(FileIdentity { device: 0, inode: 0 }, &[], "x");
    let at = escaping.len() - 1;
    escaping[at] = b'/';
    let cases: [(&str, Vec<u8>); 4] = [
        ("truncated", good[..good.len() - 3].to_vec()),
        ("trailing", trailing),
        ("version", [b"fdu capture\0\x09".as_slice(), &good[INTENT_MAGIC.len()..]].concat()),
        ("component", escaping),
    ];
    for (label, bytes) in cases {
        let scratch = Scratch::new(&format!("intent-{label}"));
        let root = &scratch.0;
        let workspace = workspace(root);
        fs::write(workspace.join("4.entry"), b"captured").unwrap();
        fs::write(workspace.join("4.intent"), &bytes).unwrap();
        fs::write(root.join("victim"), b"victim").unwrap();

        assert_rejected_untouched(root, "");
        assert_eq!(fs::read(workspace.join("4.entry")).unwrap(), b"captured", "{label}");
        assert!(!root.join("original").exists(), "{label}");
    }
}

#[test]
fn intent_naming_another_root_is_not_applied() {
    let scratch = Scratch::new("intent-root");
    let root = &scratch.0;
    let workspace = workspace(root);
    fs::write(workspace.join("4.entry"), b"captured").unwrap();
    fs::write(workspace.join("4.intent"), intent(FileIdentity { device: 1, inode: 2 }, &[], "original")).unwrap();

    assert_rejected_untouched(root, "root identity changed");
    assert!(!root.join("original").exists());
}

#[test]
fn intent_claiming_more_ancestors_than_bytes_is_rejected() {
    let scratch = Scratch::new("intent-count");
    let root = &scratch.0;
    let workspace = workspace(root);
    let mut bytes = intent(identity(root), &[], "original");
    let count_at = INTENT_MAGIC.len() + 16;
    bytes[count_at..count_at + 4].copy_from_slice(&1000u32.to_le_bytes());
    fs::write(workspace.join("4.entry"), b"captured").unwrap();
    fs::write(workspace.join("4.intent"), &bytes).unwrap();

    assert_rejected_untouched(root, "invalid recovery ancestor count");
    assert_eq!(fs::read(workspace.join("4.entry")).unwrap(), b"captured");
    assert!(!root.join("original").exists());
}

#[test]
fn non_numeric_capture_id_is_rejected_and_kept() {
    let scratch = Scratch::new("capture-id");
    let root = &scratch.0;
    let workspace = workspace(root);
    fs::write(workspace.join("abc.entry"), b"captured").unwrap();
    fs::write(workspace.join("abc.intent"), intent(identity(root), &[], "original")).unwrap();

    assert_rejected_untouched(root, "invalid capture identifier");
    assert_eq!(fs::read(workspace.join("abc.entry")).unwrap(), b"captured");
    assert!(!root.join("original").exists());
}
