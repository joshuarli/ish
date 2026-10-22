use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{self, Mode};

use super::Invocation;
use super::anchored::{self, DIRECTORY};
use crate::policy::{self, Action, Target};
use crate::roots::{self, EntryKind, Observation, Root};
use crate::{Event, OperandOutcome, OperandState, Operation, RECORD_MEMORY_BYTES};

struct Frame {
    name: OsString,
    observed: Observation,
    destination: Option<Observation>,
    children: Option<Vec<OsString>>,
    next_child: usize,
}

pub(super) fn plan(invocation: &Invocation<'_>, source: &Root, destination: &Root, action: Action) -> io::Result<OperandState> {
    #[cfg(test)]
    return plan_inner(invocation, source, destination, action, &mut || {});
    #[cfg(not(test))]
    plan_inner(invocation, source, destination, action)
}

fn plan_inner(
    invocation: &Invocation<'_>,
    source: &Root,
    destination: &Root,
    action: Action,
    #[cfg(test)]
    entry_observed: &mut impl FnMut(),
) -> io::Result<OperandState> {
    invocation.checkpoint()?;
    if source.observed.unwrap().identity.kind != EntryKind::Directory {
        if source.observed.unwrap().identity.kind == EntryKind::Other && invocation.request.operation == Operation::Copy { return Err(io::Error::new(io::ErrorKind::Unsupported, "special objects cannot be copied")); }
        invocation.update(|progress| progress.discovered_files += 1);
        return Ok(OperandState::Planned);
    }
    let source_root = source.open_directory()?;
    let target_root = if action == Action::Merge { Some(destination.open_directory()?) } else { None };
    let source_mount = crate::platform::mount_id(source_root.as_fd())?;
    let destination_mount = target_root.as_ref().map(|fd| crate::platform::mount_id(fd.as_fd())).transpose()?;
    let mut frames = Vec::new();
    frames.try_reserve(1).map_err(io::Error::other)?;
    let mut bytes = frames.capacity() * std::mem::size_of::<Frame>();
    frames.push(Frame { name: OsString::new(), observed: source.observed.unwrap(), destination: if action == Action::Merge { destination.observed } else { None }, children: None, next_child: 0 });
    while !frames.is_empty() {
        invocation.checkpoint()?;
        let (directory, target, relative) = reopen(&source_root, target_root.as_ref(), &frames)?;
        let index = frames.len() - 1;
        if !anchored::unchanged(frames[index].observed, roots::observe_fd(directory.as_fd())?) { return Err(io::Error::other("source directory changed during dry-run")); }
        if frames[index].children.is_none() {
            let mut children = Vec::new();
            for entry in fs::Dir::read_from(&directory)? {
                invocation.checkpoint()?;
                let entry = entry?;
                #[cfg(test)]
                entry_observed();
                let name = OsStr::from_bytes(entry.file_name().to_bytes());
                if name == "." || name == ".." { continue; }
                let previous_capacity = children.capacity();
                children.try_reserve(1).map_err(io::Error::other)?;
                add_memory(&mut bytes, (children.capacity() - previous_capacity) * std::mem::size_of::<OsString>())?;
                let mut owned_name = OsString::new();
                owned_name.try_reserve_exact(name.as_bytes().len()).map_err(io::Error::other)?;
                add_memory(&mut bytes, owned_name.capacity())?;
                owned_name.push(name);
                children.push(owned_name);
            }
            frames[index].children = Some(children);
        }
        let next_child = frames[index].next_child;
        let next = frames[index].children.as_mut().unwrap().get_mut(next_child).map(std::mem::take);
        if next.is_some() { frames[index].next_child += 1; }
        let Some(name) = next else {
            let frame = frames.pop().unwrap();
            bytes -= frame.name.capacity();
            if let Some(children) = frame.children {
                bytes -= children.capacity() * std::mem::size_of::<OsString>();
                bytes -= children.iter().map(OsString::capacity).sum::<usize>();
            }
            continue;
        };
        let observed = roots::observe(directory.as_fd(), &name)?;
        let target_observed = if let Some(target) = &target {
            match roots::observe(target.as_fd(), &name) { Ok(observed) => Some(observed), Err(error) if error.kind() == io::ErrorKind::NotFound => None, Err(error) => return Err(error) }
        } else { None };
        let action = policy::decide(observed, target_observed.map(Target::Existing).unwrap_or(Target::Absent), &invocation.request.options)?;
        let path = relative.join(&name);
        invocation.update(|progress| { progress.discovered_files += 1; if action == Action::Skip { progress.skipped_files += 1; } });
        invocation.emit(Event::Operand(OperandOutcome { source: source.path.join(&path), destination: destination.path.join(&path), state: if action == Action::Skip { OperandState::Skipped } else { OperandState::Planned } }));
        if action == Action::Skip || action == Action::SameEntry { bytes -= name.capacity(); continue; }
        if observed.identity.kind == EntryKind::Other && invocation.request.operation == Operation::Copy { return Err(io::Error::new(io::ErrorKind::Unsupported, "special objects cannot be copied")); }
        if observed.identity.kind == EntryKind::Directory {
            let child = fs::openat(&directory, &name, DIRECTORY, Mode::empty())?;
            if roots::observe_fd(child.as_fd())?.identity != observed.identity { return Err(io::Error::other("source directory substituted during dry-run")); }
            if !invocation.request.options.cross_file_systems && crate::platform::mount_id(child.as_fd())? != source_mount { return Err(io::Error::other("nested source mount requires --cross-file-systems")); }
            if action == Action::Merge {
                let target = fs::openat(target.as_ref().unwrap(), &name, DIRECTORY, Mode::empty())?;
                if roots::observe_fd(target.as_fd())?.identity != target_observed.unwrap().identity { return Err(io::Error::other("destination directory substituted during dry-run")); }
                if !invocation.request.options.cross_file_systems && Some(crate::platform::mount_id(target.as_fd())?) != destination_mount { return Err(io::Error::other("nested destination mount requires --cross-file-systems")); }
            }
            let previous_capacity = frames.capacity();
            frames.try_reserve(1).map_err(io::Error::other)?;
            add_memory(&mut bytes, (frames.capacity() - previous_capacity) * std::mem::size_of::<Frame>())?;
            frames.push(Frame { name, observed, destination: if action == Action::Merge { target_observed } else { None }, children: None, next_child: 0 });
        } else {
            bytes -= name.capacity();
        }
    }
    Ok(OperandState::Planned)
}

fn add_memory(bytes: &mut usize, amount: usize) -> io::Result<()> {
    *bytes = bytes.checked_add(amount).ok_or_else(|| io::Error::other("dry-run traversal memory accounting overflow"))?;
    if *bytes > RECORD_MEMORY_BYTES / 2 { return Err(io::Error::other("dry-run traversal exceeds record memory budget")); }
    Ok(())
}

fn reopen(source: &OwnedFd, destination: Option<&OwnedFd>, frames: &[Frame]) -> io::Result<(OwnedFd, Option<OwnedFd>, PathBuf)> {
    let mut source = fs::openat(source, ".", DIRECTORY, Mode::empty())?;
    let mut target = destination.map(|fd| fs::openat(fd, ".", DIRECTORY, Mode::empty())).transpose()?;
    let mut relative = PathBuf::new();
    for frame in &frames[1..] {
        let next = fs::openat(&source, &frame.name, DIRECTORY, Mode::empty())?;
        if roots::observe_fd(next.as_fd())?.identity != frame.observed.identity { return Err(io::Error::other("dry-run source ancestor changed")); }
        source = next;
        target = match (&target, frame.destination) {
            (Some(target), Some(observed)) => {
                let next = fs::openat(target, &frame.name, DIRECTORY, Mode::empty())?;
                if roots::observe_fd(next.as_fd())?.identity != observed.identity { return Err(io::Error::other("dry-run destination ancestor changed")); }
                Some(next)
            },
            _ => None,
        };
        relative.push(&frame.name);
    }
    Ok((source, target, relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Invocation, records::IndexedRecords};
    use crate::{Cancellation, Completion, Context, Event, Metrics, OperandState, Options, Operands, Progress, Request};
    use rustix::fs::OFlags;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::path::Path;
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let unique = SystemTime::now().duration_since(UNIX_EPOCH).expect("read fixture clock").as_nanos();
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/tmp").join(format!("dry-run-count-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create dry-run count fixture");
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).expect("remove dry-run count fixture");
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct EntrySnapshot {
        device: u64,
        inode: u64,
        links: u64,
        mode: u32,
        uid: u32,
        gid: u32,
        size: u64,
        modified: (i64, i64),
        changed: (i64, i64),
        bytes: Option<Vec<u8>>,
    }

    fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, EntrySnapshot> {
        fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, EntrySnapshot>) {
            let metadata = std::fs::symlink_metadata(path).expect("stat dry-run snapshot entry");
            let bytes = metadata.is_file().then(|| std::fs::read(path).expect("read dry-run snapshot file"));
            entries.insert(
                path.strip_prefix(root).expect("strip dry-run snapshot root").to_path_buf(),
                EntrySnapshot {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    links: metadata.nlink(),
                    mode: metadata.mode(),
                    uid: metadata.uid(),
                    gid: metadata.gid(),
                    size: metadata.size(),
                    modified: (metadata.mtime(), metadata.mtime_nsec()),
                    changed: (metadata.ctime(), metadata.ctime_nsec()),
                    bytes,
                },
            );
            if metadata.is_dir() {
                for entry in std::fs::read_dir(path).expect("read dry-run snapshot directory") {
                    visit(root, &entry.expect("read dry-run snapshot entry").path(), entries);
                }
            }
        }

        let mut entries = BTreeMap::new();
        visit(root, root, &mut entries);
        entries
    }

    #[test]
    fn public_dry_run_skip_merge_counts_wide_deep_entries_without_changes() {
        const BRANCHES: usize = 32;
        const FILES_PER_BRANCH: usize = 32;
        const DEPTH: usize = 8;
        const DISCOVERED: usize = BRANCHES * (FILES_PER_BRANCH + DEPTH * 2) + BRANCHES;
        const SKIPPED: usize = BRANCHES * (FILES_PER_BRANCH + DEPTH);

        let fixture = Fixture::new();
        let cwd = fixture.0.join("cwd");
        let source_root = cwd.join("source");
        let destination_root = cwd.join("destination");
        std::fs::create_dir(&cwd).expect("create fixture cwd");
        std::fs::create_dir(&source_root).expect("create source directory");
        std::fs::create_dir(&destination_root).expect("create destination directory");
        for branch in 0..BRANCHES {
            let branch_name = format!("branch-{branch:02}");
            let source_branch = source_root.join(&branch_name);
            let destination_branch = destination_root.join(&branch_name);
            std::fs::create_dir(&source_branch).expect("create source branch");
            std::fs::create_dir(&destination_branch).expect("create destination branch");
            for index in 0..FILES_PER_BRANCH {
                let name = format!("item-{index:02}");
                std::fs::write(source_branch.join(&name), format!("source {branch} {index}"))
                    .expect("write source leaf");
                std::fs::write(destination_branch.join(name), format!("destination {branch} {index}"))
                    .expect("write colliding destination leaf");
            }
            std::fs::write(destination_branch.join("destination-only"), b"keep")
                .expect("write destination-only leaf");
            let mut source = source_branch.join("deep");
            let mut destination = destination_branch.join("deep");
            std::fs::create_dir(&source).expect("create source chain");
            std::fs::create_dir(&destination).expect("create destination chain");
            for depth in 0..DEPTH {
                std::fs::write(source.join("leaf"), format!("source {branch} {depth}"))
                    .expect("write source chain leaf");
                std::fs::write(destination.join("leaf"), format!("destination {branch} {depth}"))
                    .expect("write colliding destination chain leaf");
                if depth + 1 < DEPTH {
                    let name = format!("d{depth:02}");
                    source.push(&name);
                    destination.push(name);
                    std::fs::create_dir(&source).expect("create source chain level");
                    std::fs::create_dir(&destination).expect("create destination chain level");
                }
            }
        }

        let before = snapshot_tree(&cwd);
        let missing_scratch = fixture.0.join("missing-scratch");
        let request = Request {
            operation: Operation::Copy,
            operands: Operands::Exact { source: PathBuf::from("source"), destination: PathBuf::from("destination") },
            options: Options {
                collision: crate::Collision::Skip,
                merge: true,
                dry_run: true,
                jobs: Some(crate::Jobs::new(2).expect("valid dry-run worker count")),
                ..Options::default()
            },
            context: Context { cwd: cwd.clone(), home: None, scratch: missing_scratch.clone() },
        };
        let operand_events = AtomicUsize::new(0);
        let skipped_events = AtomicUsize::new(0);
        let sink = |event: Event| {
            if let Event::Operand(operand) = event {
                operand_events.fetch_add(1, Ordering::Relaxed);
                if operand.state == OperandState::Skipped {
                    skipped_events.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok::<(), io::Error>(())
        };
        let outcome = crate::execute(&request, &Cancellation::default(), &sink);

        assert_eq!(outcome.completion, Completion::Complete);
        assert_eq!(outcome.progress.discovered_files, DISCOVERED as u64);
        assert_eq!(outcome.progress.skipped_files, SKIPPED as u64);
        assert_eq!(outcome.progress.completed_files, 0);
        assert_eq!(outcome.progress.failed_files, 0);
        assert_eq!(operand_events.load(Ordering::Relaxed), DISCOVERED + 1);
        assert_eq!(skipped_events.load(Ordering::Relaxed), SKIPPED);
        assert_eq!(snapshot_tree(&cwd), before);
        assert!(!missing_scratch.exists());
        assert_eq!(std::fs::read_dir(&fixture.0).expect("read fixture namespace").count(), 1);
    }

    #[test]
    fn dry_run_reads_each_wide_directory_entry_once() {
        for width in [16, 64, 256, 1024, 4096] {
            let fixture = Fixture::new();
            let cwd = fixture.0.join("cwd");
            std::fs::create_dir(&cwd).expect("create fixture cwd");
            std::fs::create_dir(cwd.join("source")).expect("create source directory");
            for index in 0..width {
                std::fs::File::create(cwd.join("source").join(format!("item-{index:04}"))).expect("create source entry");
            }
            let cwd_fd = fs::open(&cwd, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).expect("open fixture cwd");
            let request = Request {
                operation: Operation::Copy,
                operands: Operands::Exact { source: PathBuf::from("source"), destination: PathBuf::from("destination") },
                options: Options { dry_run: true, ..Options::default() },
                context: Context { cwd: cwd.clone(), home: None, scratch: fixture.0.join("missing-scratch") },
            };
            let cancellation = Cancellation::default();
            let sink = |_: Event| Ok::<(), io::Error>(());
            let invocation = Invocation {
                request: &request,
                cancellation: &cancellation,
                sink: &sink,
                cwd: cwd_fd.as_fd(),
                progress: Mutex::new(Progress::default()),
                hardlinks: Mutex::new(None::<IndexedRecords>),
                self_changes: Mutex::new(None),
                hardlink_members: Mutex::new(std::collections::HashMap::new()),
                deferred_flags: Mutex::new(None),
                active_hardlinks: Mutex::new(std::collections::HashSet::new()),
                hardlink_ready: std::sync::Condvar::new(),
                renamed_trees: Mutex::new(None),
                track_renamed_trees: false,
                renamed_tree_cursor: std::sync::atomic::AtomicU64::new(0),
                copied_hardlinks: AtomicBool::new(false),
                unverified_directory_rename: AtomicBool::new(false),
                root_discovery_active: Mutex::new(false),
                root_discovery_ready: std::sync::Condvar::new(),
                stopped: AtomicBool::new(false),
                jobs: 1,
                metrics: Mutex::new(Metrics::default()),
                descriptor_reservation: AtomicUsize::new(0),
                hook: None,
            };
            let source = Root::establish(invocation.cwd, Path::new("source"), roots::Requirement::Existing).expect("establish source root");
            let destination = Root::establish(invocation.cwd, Path::new("destination"), roots::Requirement::MayBeAbsent).expect("establish destination root");
            let mut enumerated = 0;
            assert_eq!(
                plan_inner(&invocation, &source, &destination, Action::Transfer, &mut || enumerated += 1).expect("plan dry run"),
                OperandState::Planned,
            );
            assert_eq!(enumerated, width + 2, "width={width}");
            assert!(!fixture.0.join("missing-scratch").exists());
            assert!(!cwd.join("destination").exists());
        }
    }
}
