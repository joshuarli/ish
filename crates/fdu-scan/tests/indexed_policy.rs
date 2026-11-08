use fdu_core::{
    DirectoryToken, EntryType, NodeId, NodeState, NodeStatus, ScanEvent, Tree,
};
use fdu_scan::{
    open_root_no_follow, start_indexed_scan_with_exclusions,
    RootAnchor, ScanExclusions,
};
use hashbrown::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

static TEMP_ID: AtomicUsize = AtomicUsize::new(0);
const BATCH_SIZE: usize = 1024;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> io::Result<Self> {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.work/tmp");
        fs::create_dir_all(&scratch)?;
        loop {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = scratch.join(format!("fdu-indexed-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct IndexedResult {
    tree: Tree,
    errors: Vec<String>,
    #[allow(dead_code)]
    batches: Vec<usize>,
    #[allow(dead_code)]
    directory_finish_batch_sizes: Vec<usize>,
}

fn finish_directory(
    tree: &mut Tree,
    tokens: &[Option<NodeId>],
    finished: &mut HashSet<DirectoryToken>,
    directory: DirectoryToken,
    complete: bool,
) -> io::Result<()> {
    let out_of_order = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message.to_owned());
    let node = tokens
        .get(directory.0 as usize)
        .copied()
        .flatten()
        .ok_or_else(|| out_of_order("a directory finished before its entry was reported"))?;
    if !finished.insert(directory) {
        return Err(out_of_order("a directory finished twice"));
    }
    for child in tree.children(node) {
        if tree.record(child).unwrap().entry_type == EntryType::Directory
            && tree.record(child).unwrap().state == NodeState::Scanning
        {
            return Err(out_of_order("a directory finished before a directory inside it"));
        }
    }
    if complete && tree.record(node).unwrap().state == NodeState::Scanning {
        tree.set_state(node, NodeStatus::Complete);
    } else if !complete {
        tree.mark_incomplete_to_root(node);
    }
    Ok(())
}

fn scan_to_tree(root: RootAnchor, capacity: usize) -> io::Result<IndexedResult> {
    scan_to_tree_with_exclusions(root, capacity, ScanExclusions::default())
}

fn scan_to_tree_with_exclusions(
    root: RootAnchor,
    capacity: usize,
    exclusions: ScanExclusions,
) -> io::Result<IndexedResult> {
    let mut tree = Tree::new(&root.name, root.identity, root.link_count)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "root index name too long"))?;
    assert!(tree.set_modified_time(tree.root(), root.modified_time()));
    let mut tokens = vec![Some(tree.root())];
    let (event_sender, events) = mpsc::sync_channel(capacity);
    let (returned_batch_sender, returned_batches) = mpsc::sync_channel(8);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = start_indexed_scan_with_exclusions(
        root,
        exclusions,
        event_sender,
        returned_batches,
        Arc::clone(&cancelled),
    );

    let mut batches = Vec::new();
    let mut directory_finish_batch_sizes = Vec::new();
    let mut finished = HashSet::new();
    let mut errors = Vec::new();
    let out_of_order = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message.to_owned());
    let mut saw_root_listed = false;

    loop {
        let event = match events.recv_timeout(Duration::from_secs(15)) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancelled.store(true, Ordering::Relaxed);
                let _ = worker.join();
                return Err(io::Error::new(io::ErrorKind::TimedOut, "indexed scan stalled"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match event {
            ScanEvent::Started { .. } => {}
            ScanEvent::Entries(batch) => {
                batches.push(batch.entries.len());
                for entry in &batch.entries {
                    let parent = tokens
                        .get(entry.parent.0 as usize)
                        .copied()
                        .flatten()
                        .ok_or_else(|| out_of_order("entry reported before its directory was registered"))?;
                    let name = &batch.names[entry.name.start as usize..entry.name.end as usize];
                    let id = tree
                        .append(
                            parent,
                            name,
                            entry.entry_type,
                            entry.identity,
                            entry.link_count,
                            entry.apparent_bytes,
                            entry.allocated_bytes,
                            match (entry.state, entry.directory_token) {
                                (NodeStatus::Complete, Some(_)) => NodeStatus::Scanning,
                                (state, _) => state,
                            },
                        )
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tree full"))?;
                    assert!(tree.set_modified_time(id, entry.modified_time));
                    if let Some(token) = entry.directory_token {
                        if tokens.len() <= token.0 as usize {
                            tokens.resize(token.0 as usize + 1, None);
                        }
                        tokens[token.0 as usize] = Some(id);
                    }
                }
                let _ = returned_batch_sender.send(batch);
            }
            ScanEvent::RootListed => {
                saw_root_listed = true;
            }
            ScanEvent::DirectoryFinished { directory, complete } => {
                directory_finish_batch_sizes.push(1);
                finish_directory(&mut tree, &tokens, &mut finished, directory, complete)?;
            }
            ScanEvent::DirectoriesFinished(batch) => {
                directory_finish_batch_sizes.push(batch.len());
                for (directory, complete) in batch {
                    finish_directory(&mut tree, &tokens, &mut finished, directory, complete)?;
                }
            }
            ScanEvent::DirectoryExcluded { directory, reason } => {
                let node = tokens
                    .get(directory.0 as usize)
                    .copied()
                    .flatten()
                    .ok_or_else(|| out_of_order("directory excluded before its entry was reported"))?;
                tree.set_state(node, NodeStatus::Excluded(reason));
            }
            ScanEvent::DirectoryFailed { message, .. } | ScanEvent::Failed { message } => {
                errors.push(message);
            }
            ScanEvent::DirectoryWaiting { .. } => {}
            ScanEvent::Cancelled | ScanEvent::Finished => {}
        }
    }
    worker.join().expect("scan worker finishes");
    assert!(saw_root_listed, "RootListed was not emitted");
    Ok(IndexedResult {
        tree,
        errors,
        batches,
        directory_finish_batch_sizes,
    })
}

#[test]
fn indexed_scan_reports_tree_entries_and_root_listed() -> io::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.0.join("root");
    fs::create_dir(&root)?;
    fs::create_dir(root.join("sub"))?;
    fs::write(root.join("sub/file.txt"), b"sample")?;
    fs::write(root.join("top.bin"), b"top")?;

    let anchor = open_root_no_follow(&root)?;
    let result = scan_to_tree(anchor, BATCH_SIZE)?;
    assert!(result.errors.is_empty());
    assert_eq!(result.tree.children(result.tree.root()).count(), 2);
    Ok(())
}

#[test]
fn indexed_scan_with_exclusions_omits_excluded_paths() -> io::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.0.join("root");
    fs::create_dir(&root)?;
    fs::create_dir(root.join("keep"))?;
    fs::create_dir(root.join("omit"))?;
    fs::write(root.join("keep/file.txt"), b"kept")?;
    fs::write(root.join("omit/file.txt"), b"omitted")?;

    let exclusions = ScanExclusions::from_paths(vec![PathBuf::from("omit")])?;
    let anchor = open_root_no_follow(&root)?;
    let result = scan_to_tree_with_exclusions(anchor, BATCH_SIZE, exclusions)?;
    assert!(result.errors.is_empty());

    let child_names: Vec<_> = result
        .tree
        .children(result.tree.root())
        .map(|id| result.tree.name(id).unwrap())
        .collect();
    assert!(child_names.contains(&b"keep".as_slice()));
    assert!(!child_names.contains(&b"omit".as_slice()));
    Ok(())
}
