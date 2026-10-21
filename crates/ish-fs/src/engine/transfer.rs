use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::os::unix::ffi::OsStrExt;
use std::sync::{Mutex, mpsc};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, AtFlags, Mode, OFlags};

use super::anchored::{self, DIRECTORY, READ_FILE};
use super::records::{Record, Records, IndexedRecords};
use super::{Failure, Invocation, rename_object, unique_name};
use crate::platform::{self, CopyMethod, MetadataTarget, Preservation};
use crate::policy::{self, Action, Target};
use crate::roots::{self, EntryKind, Identity, Observation, Root};
use crate::{Diagnostic, Event, EventSink, OperandState, Operation, QUEUE_CAPACITY, TRANSFER_BUFFER_BYTES};

const MAX_WORK_PATH: usize = 64 * 1024;

pub(super) struct DurableReceipt {
    pub source_parent: OwnedFd,
    pub source_name: OsString,
    pub source_path: PathBuf,
    pub source_directory: Option<OwnedFd>,
    pub records: Mutex<Records>,
    pub directories: Mutex<Records>,
    pub destination_parent: OwnedFd,
    pub destination_name: OsString,
    pub destination_directory: Option<OwnedFd>,
    pub destinations: Mutex<Records>,
}

struct Stage<'a> {
    parent: BorrowedFd<'a>,
    name: OsString,
    observed: Observation,
    entries: Mutex<Records>,
    directories: Mutex<Records>,
    directory: Option<OwnedFd>,
    published: bool,
    sink: &'a dyn EventSink,
    display: PathBuf,
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if self.published { return; }
        let mut failure = None;
        if let Some(directory) = &self.directory {
            if let Err(error) = fs::fchmod(directory, Mode::RUSR | Mode::WUSR | Mode::XUSR) { failure = Some(io::Error::from(error)); }
            let mut directory_cursor = 0;
            loop {
                let record = match self.directories.lock().unwrap().next(&mut directory_cursor) {
                    Ok(Some(record)) => record,
                    Ok(None) => break,
                    Err(error) => { failure.get_or_insert(error); break; },
                };
                if record.source.as_os_str().is_empty() { continue; }
                let restoration = anchored::parent(directory.as_fd(), &record.source, &self.directories, true).and_then(|(parent, name)| platform::prepare_directory(parent.as_fd(), &name, record.published).map(drop));
                if let Err(error) = restoration { failure.get_or_insert(error); }
            }
            let records = self.entries.get_mut().unwrap();
            let mut cursor = records.end();
            loop {
                let record = match records.previous(&mut cursor) {
                    Ok(Some(record)) => record,
                    Ok(None) => break,
                    Err(error) => { failure.get_or_insert(error); break; },
                };
                if record.source.as_os_str().is_empty() { continue; }
                let cleanup = anchored::parent(directory.as_fd(), &record.source, &self.directories, true).and_then(|(parent, name)| cleanup_owned(parent.as_fd(), &name, record.published));
                if let Err(error) = cleanup { failure.get_or_insert(error); }
            }
        }
        match cleanup_owned(self.parent, &self.name, self.observed.identity) {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.observed.identity.inode == 0 => {},
            Err(error) => { failure.get_or_insert(error); },
        }
        if let Some(error) = failure {
            let _ = self.sink.emit(Event::Diagnostic(Diagnostic { path: self.display.clone(), message: format!("staging cleanup incomplete; retained location {:?}, expected identity {:?}: {error}", self.display, self.observed.identity) }));
        }
    }
}

fn cleanup_owned(parent: BorrowedFd<'_>, name: &OsStr, identity: Identity) -> io::Result<()> {
    anchored::identity(parent, name, identity)?;
    fs::unlinkat(parent, name, if identity.kind == EntryKind::Directory { AtFlags::REMOVEDIR } else { AtFlags::empty() }).map_err(Into::into)
}

fn duplicate(parent: BorrowedFd<'_>) -> io::Result<OwnedFd> { fs::openat(parent, ".", DIRECTORY, Mode::empty()).map_err(Into::into) }

fn new_stage<'a>(invocation: &Invocation<'a>, destination: &'a Root, source: Observation) -> io::Result<Stage<'a>> {
    let entries = Mutex::new(Records::new(&invocation.request.context.scratch)?);
    let directories = Mutex::new(Records::new(&invocation.request.context.scratch)?);
    let name = unique_name("stage");
    if source.identity.kind == EntryKind::Directory {
        fs::mkdirat(&destination.parent, &name, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
        let observed = roots::observe(destination.parent.as_fd(), &name).map_err(|error| io::Error::new(error.kind(), format!("staging created but identity unavailable at {:?}: {error}", destination.path.parent().unwrap_or_else(|| Path::new(".")).join(&name))))?;
        let mut stage = Stage { parent: destination.parent.as_fd(), name: name.clone(), observed, entries, directories, directory: None, published: false, sink: invocation.sink, display: destination.path.parent().unwrap_or_else(|| Path::new(".")).join(&name) };
        let directory = platform::prepare_directory(stage.parent, &stage.name, observed.identity)?;
        if roots::observe_fd(directory.as_fd())?.identity != observed.identity { return Err(io::Error::other("staging directory substituted")); }
        stage.directory = Some(directory);
        Ok(stage)
    } else {
        let placeholder = Observation { identity: Identity { device: 0, inode: 0, kind: source.identity.kind }, ..source };
        Ok(Stage { parent: destination.parent.as_fd(), name: name.clone(), observed: placeholder, entries, directories, directory: None, published: false, sink: invocation.sink, display: destination.path.parent().unwrap_or_else(|| Path::new(".")).join(&name) })
    }
}

pub(super) fn whole(invocation: &Invocation<'_>, source: &Root, destination: &Root, action: Action) -> Result<Option<DurableReceipt>, Failure> {
    let _span = super::Span::new(invocation, super::SpanKind::Transfer);
    let observed = source.observed.ok_or_else(|| io::Error::other("source disappeared"))?;
    if observed.identity.kind == EntryKind::Other { return Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported source object for transfer").into()); }
    if invocation.request.operation == Operation::Move { invocation.require_descriptors(24)?; }
    invocation.require_descriptors((if observed.identity.kind == EntryKind::Directory { 10 + 6 * invocation.jobs } else { 8 }) + invocation.index_descriptors())?;
    invocation.update(|progress| progress.discovered_files += 1);
    let mut stage = new_stage(invocation, destination, observed)?;
    let source_directory = if observed.identity.kind == EntryKind::Directory { Some(source.open_directory()?) } else { None };
    let transferred = if let Some(directory) = &source_directory {
        copy_tree(invocation, source, destination, directory.as_fd(), &stage)
    } else {
        let record = transfer_leaf(invocation, LeafSource { parent: source.parent.as_fd(), name: &source.name, observed, mount: None }, LeafDestination { parent: stage.parent, name: &stage.name, visible: &destination.path, placement: &Placement { root: stage.parent, base: destination.path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf(), relative: PathBuf::from(&stage.name), directories: None } }, &mut vec![0; TRANSFER_BUFFER_BYTES]);
        match record {
            Ok(record) => {
                stage.observed = roots::observe(stage.parent, &stage.name)?;
                stage.entries.lock().unwrap().push(&Record { source: PathBuf::new(), destination: PathBuf::new(), ..record })?;
                Ok(())
            },
            Err(error) => Err(error),
        }
    };
    if let Err(error) = transferred { return Err(error.into()); }
    invocation.checkpoint()?;
    anchored::require_unchanged(source.parent.as_fd(), &source.name, observed)?;
    invocation.fault("before_publish")?;
    validate_sources(source, source_directory.as_ref(), &stage)?;
    validate_final_access(&stage)?;
    anchored::identity(stage.parent, &stage.name, stage.observed.identity)?;
    match action {
        Action::Transfer => platform::rename_noreplace(stage.parent, &stage.name, destination.parent.as_fd(), &destination.name)?,
        Action::Replace => {
            anchored::require_unchanged(destination.parent.as_fd(), &destination.name, destination.observed.ok_or_else(|| io::Error::other("replacement observation missing"))?)?;
            fs::renameat(stage.parent, &stage.name, &destination.parent, &destination.name)?;
        },
        _ => return Err(io::Error::other("invalid whole-object publication action").into()),
    }
    stage.published = true;
    let publication_failure = |error| Failure { state: OperandState::DestinationPublishedSourceRetained, error };
    anchored::identity(destination.parent.as_fd(), &destination.name, stage.observed.identity).map_err(publication_failure)?;
    invocation.fault("after_publish").map_err(publication_failure)?;
    finalize_published(invocation, source, destination, &stage).map_err(publication_failure)?;
    if invocation.durable() { invocation.sync_directory(destination.parent.as_fd()).map_err(publication_failure)?; }
    refresh_hardlinks(invocation, source, destination, &stage).map_err(publication_failure)?;
    if invocation.request.operation == Operation::Copy { return Ok(None); }
    invocation.require_descriptors(24).map_err(publication_failure)?;
    let source_parent = duplicate(source.parent.as_fd()).map_err(publication_failure)?;
    let destination_parent = duplicate(destination.parent.as_fd()).map_err(publication_failure)?;
    let destination_directory = if observed.identity.kind == EntryKind::Directory { Some(open_published_directory(destination, stage.observed.identity).map_err(publication_failure)?) } else { None };
    let destinations = snapshot_destinations(invocation, destination, destination_directory.as_ref(), &stage.entries, &stage.directories).map_err(publication_failure)?;
    invocation.fault("after_durable").map_err(publication_failure)?;
    let entries = std::mem::replace(stage.entries.get_mut().unwrap(), Records::new(&invocation.request.context.scratch).map_err(publication_failure)?);
    let directories = std::mem::replace(stage.directories.get_mut().unwrap(), Records::new(&invocation.request.context.scratch).map_err(publication_failure)?);
    Ok(Some(DurableReceipt { source_parent, source_name: source.name.clone(), source_path: source.path.clone(), source_directory, records: Mutex::new(entries), directories: Mutex::new(directories), destination_parent, destination_name: destination.name.clone(), destination_directory, destinations: Mutex::new(destinations) }))
}

fn finalize_published(invocation: &Invocation<'_>, source: &Root, destination: &Root, stage: &Stage<'_>) -> io::Result<()> {
    let mut cursor = stage.entries.lock().unwrap().end();
    loop {
        let Some(record) = stage.entries.lock().unwrap().previous(&mut cursor)? else { break; };
        if record.final_flags == 0 { continue; }
        invocation.checkpoint()?;
        let (parent, name) = if record.source.as_os_str().is_empty() { (duplicate(destination.parent.as_fd())?, destination.name.clone()) } else { anchored::parent(stage.directory.as_ref().unwrap().as_fd(), &record.source, &stage.directories, true)? };
        anchored::identity(parent.as_fd(), &name, record.published)?;
        let metadata = platform::ExtendedMetadata { flags: record.final_flags, ..platform::ExtendedMetadata::default() };
        if record.published.kind == EntryKind::Symlink {
            platform::apply_final_metadata(MetadataTarget::Symlink { parent: parent.as_fd(), name: &name }, &metadata)?;
            if invocation.durable() { invocation.sync_directory(parent.as_fd())?; }
        } else {
            let fd = fs::openat(&parent, &name, READ_FILE, Mode::empty())?;
            if roots::observe_fd(fd.as_fd())?.identity != record.published { return Err(io::Error::other("published entry identity changed")); }
            platform::apply_final_metadata(MetadataTarget::Fd(fd.as_fd()), &metadata)?;
            if invocation.durable() {
                if record.published.kind == EntryKind::Directory { invocation.sync_directory(fd.as_fd())?; } else { invocation.sync_file(fd.as_fd())?; }
            }
            if record.observed.identity.kind == EntryKind::RegularFile
                && let Some(groups) = invocation.hardlinks.lock().unwrap().as_mut()
                && let Some(mut group) = groups.find_identity(record.observed.identity)? {
                group.output = Some(roots::observe_fd(fd.as_fd())?);
                groups.push(&group)?;
            }
        }
    }
    let _ = source;
    Ok(())
}

fn validate_final_access(stage: &Stage<'_>) -> io::Result<()> {
    let mut cursor = 0;
    while let Some(record) = stage.entries.lock().unwrap().next(&mut cursor)? {
        if record.final_flags == 0 { continue; }
        let (parent, name) = if record.source.as_os_str().is_empty() { (duplicate(stage.parent)?, stage.name.clone()) } else { anchored::parent(stage.directory.as_ref().unwrap().as_fd(), &record.source, &stage.directories, true)? };
        anchored::identity(parent.as_fd(), &name, record.published)?;
        if record.published.kind != EntryKind::Symlink {
            let fd = fs::openat(&parent, &name, READ_FILE, Mode::empty()).map_err(|error| io::Error::new(io::ErrorKind::Unsupported, format!("deferred flags cannot be finalized through preserved access permissions: {error}")))?;
            if roots::observe_fd(fd.as_fd())?.identity != record.published { return Err(io::Error::other("staging entry changed before final metadata")); }
        }
    }
    Ok(())
}

fn refresh_hardlinks(invocation: &Invocation<'_>, _source: &Root, destination: &Root, stage: &Stage<'_>) -> io::Result<()> {
    let mut entries = stage.entries.lock().unwrap();
    let mut cursor = 0;
    let mut groups = invocation.hardlinks.lock().unwrap();
    if groups.is_none() { return Ok(()); }
    let groups = groups.as_mut().unwrap();
    while let Some(mut record) = entries.next(&mut cursor)? {
        if record.observed.identity.kind != EntryKind::RegularFile { continue; }
        let Some(existing) = groups.find_identity(record.observed.identity)? else { continue; };
        let (base, relative) = if record.source.as_os_str().is_empty() {
            (destination.path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf(), PathBuf::from(&destination.name))
        } else { (destination.path.clone(), record.source.clone()) };
        record.source = base;
        record.destination = relative;
        record.output = if stage.observed.identity.kind != EntryKind::Directory { Some(roots::observe(destination.parent.as_fd(), &destination.name)?) } else { existing.output };
        groups.push(&record)?;
    }
    Ok(())
}

struct Work {
    path: PathBuf,
    observed: Observation,
}

fn copy_tree(invocation: &Invocation<'_>, source: &Root, destination: &Root, source_fd: BorrowedFd<'_>, stage: &Stage<'_>) -> io::Result<()> {
    let observed = source.observed.unwrap();
    let root_record = Record { source: PathBuf::new(), destination: PathBuf::new(), observed, published: stage.observed.identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 };
    stage.entries.lock().unwrap().push(&root_record)?;
    stage.directories.lock().unwrap().push(&root_record)?;
    let target_fd = stage.directory.as_ref().unwrap().as_fd();
    let source_mount = platform::mount_id(source_fd)?;
    let failure = Mutex::new(None::<io::Error>);
    let finished = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::sync_channel::<Work>(QUEUE_CAPACITY);
    let receiver = Mutex::new(receiver);
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..invocation.jobs {
            let receiver = &receiver;
            let failure = &failure;
            let finished = &finished;
            let worker = std::thread::Builder::new().name("ish-fs-transfer".into()).stack_size(super::THREAD_STACK_BYTES).spawn_scoped(scope, move || {
                let _finished = Finished { count: finished, stopped: &invocation.stopped };
                let mut buffer = vec![0; TRANSFER_BUFFER_BYTES];
                loop {
                    let work = match receiver.lock().unwrap().recv() { Ok(work) => work, Err(_) => break };
                    if invocation.checkpoint().is_err() { continue; }
                    let result = (|| {
                        let (source_parent, source_name) = anchored::parent(source_fd, &work.path, &stage.directories, false)?;
                        let (target_parent, target_name) = anchored::parent(target_fd, &work.path, &stage.directories, true)?;
                        let visible = destination.path.parent().unwrap_or_else(|| Path::new(".")).join(&stage.name).join(&work.path);
                        let result = transfer_leaf(invocation, LeafSource { parent: source_parent.as_fd(), name: &source_name, observed: work.observed, mount: Some(source_mount) }, LeafDestination { parent: target_parent.as_fd(), name: &target_name, visible: &visible, placement: &Placement { root: target_fd, base: destination.path.parent().unwrap_or_else(|| Path::new(".")).join(&stage.name), relative: work.path.clone(), directories: Some(&stage.directories) } }, &mut buffer)?;
                        stage.entries.lock().unwrap().push(&Record { source: work.path.clone(), destination: work.path, ..result })
                    })();
                    if let Err(error) = result {
                        invocation.stopped.store(true, Ordering::Release);
                        let mut first = failure.lock().unwrap();
                        if first.is_none() { *first = Some(error); }
                    }
                }
            });
            match worker {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    invocation.stopped.store(true, Ordering::Release);
                    *failure.lock().unwrap() = Some(error);
                    drop(sender);
                    for worker in workers { if worker.join().is_err() { invocation.emit(Event::Diagnostic(Diagnostic { path: source.path.clone(), message: "transfer thread panicked while joining failed initialization".into() })); } }
                    return;
                },
            }
        }
        let failure_ref = &failure;
        let finished_ref = &finished;
        let directories = &stage.directories;
        let producer = std::thread::Builder::new().name("ish-fs-discovery".into()).stack_size(super::THREAD_STACK_BYTES).spawn_scoped(scope, move || {
            let _finished = Finished { count: finished_ref, stopped: &invocation.stopped };
            let result = (|| {
                let mut cursor = 0;
                loop {
                    invocation.checkpoint()?;
                    let next = directories.lock().unwrap().next(&mut cursor)?;
                    let Some(record) = next else { break; };
                    let source_directory = anchored::open_directory(source_fd, &record.source, directories, false)?;
                    if !anchored::unchanged(record.observed, roots::observe_fd(source_directory.as_fd())?) { return Err(io::Error::other("source directory changed before enumeration")); }
                    let target_directory = anchored::open_directory(target_fd, &record.source, directories, true)?;
                    for entry in fs::Dir::read_from(&source_directory)? {
                        invocation.checkpoint()?;
                        let entry = entry?;
                        let bytes = entry.file_name().to_bytes();
                        if bytes == b"." || bytes == b".." { continue; }
                        let name = OsStr::from_bytes(bytes);
                        let path = record.source.join(name);
                        if path.as_os_str().len() > MAX_WORK_PATH { return Err(io::Error::other("traversal path exceeds bounded work-record limit")); }
                        let observed = roots::observe(source_directory.as_fd(), name)?;
                        invocation.update(|progress| progress.discovered_files += 1);
                        if observed.identity.kind == EntryKind::Directory {
                            let child = fs::openat(&source_directory, name, DIRECTORY, Mode::empty())?;
                            if roots::observe_fd(child.as_fd())?.identity != observed.identity { return Err(io::Error::other("discovered directory substituted")); }
                            if !invocation.request.options.cross_file_systems && platform::mount_id(child.as_fd())? != source_mount { return Err(io::Error::other("nested source mount requires --cross-file-systems")); }
                            drop(child);
                            fs::mkdirat(&target_directory, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
                            let published = roots::observe(target_directory.as_fd(), name)?.identity;
                            let record = Record { source: path.clone(), destination: path, observed, published, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 };
                            stage.entries.lock().unwrap().push(&record)?;
                            drop(platform::prepare_directory(target_directory.as_fd(), name, published)?);
                            directories.lock().unwrap().push(&record)?;
                        } else {
                            enqueue(&sender, Work { path, observed }, invocation)?;
                        }
                    }
                    if !anchored::unchanged(record.observed, roots::observe_fd(source_directory.as_fd())?) { return Err(io::Error::other("source directory changed during enumeration")); }
                }
                Ok(())
            })();
            if let Err(error) = result {
                invocation.stopped.store(true, Ordering::Release);
                let mut first = failure_ref.lock().unwrap();
                if first.is_none() { *first = Some(error); }
            }
            drop(sender);
        });
        let producer = match producer {
            Ok(producer) => Some(producer),
            Err(error) => {
                invocation.stopped.store(true, Ordering::Release);
                *failure.lock().unwrap() = Some(error);
                finished.fetch_add(1, Ordering::Release);
                None
            },
        };
        while finished.load(Ordering::Acquire) < invocation.jobs + 1 {
            invocation.emit(Event::Progress(*invocation.progress.lock().unwrap()));
            std::thread::park_timeout(Duration::from_millis(25));
        }
        if let Some(producer) = producer && producer.join().is_err() { failure.lock().unwrap().get_or_insert_with(|| io::Error::other("discovery thread panicked")); }
        for worker in workers {
            if worker.join().is_err() { failure.lock().unwrap().get_or_insert_with(|| io::Error::other("transfer thread panicked")); }
        }
    });
    if let Some(error) = failure.into_inner().unwrap() { return Err(error); }
    invocation.checkpoint()?;
    finalize_directories(invocation, source_fd, target_fd, stage)
}

fn finalize_directories(invocation: &Invocation<'_>, source_fd: BorrowedFd<'_>, target_fd: BorrowedFd<'_>, stage: &Stage<'_>) -> io::Result<()> {
    let mut cursor = stage.directories.lock().unwrap().end();
    loop {
        let record = stage.directories.lock().unwrap().previous(&mut cursor)?;
        let Some(record) = record else { break; };
        invocation.checkpoint()?;
        let source = anchored::open_directory(source_fd, &record.source, &stage.directories, false)?;
        if !anchored::unchanged(record.observed, roots::observe_fd(source.as_fd())?) { return Err(io::Error::other("source directory changed before metadata preservation")); }
        let target = anchored::open_directory(target_fd, &record.source, &stage.directories, true)?;
        let metadata = platform::capture_metadata(MetadataTarget::Fd(source.as_fd()))?;
        if platform::requires_final_metadata(&metadata) { stage.entries.lock().unwrap().set_final_flags(&record.source, metadata.flags)?; }
        platform::apply_metadata(MetadataTarget::Fd(target.as_fd()), &record.observed, &metadata, preservation(invocation))?;
        if invocation.durable() { invocation.sync_directory(target.as_fd())?; }
        invocation.update(|progress| progress.completed_files += 1);
    }
    Ok(())
}

fn preservation(invocation: &Invocation<'_>) -> Preservation {
    if invocation.request.operation == Operation::Move { Preservation::Move } else { Preservation::Copy }
}

struct LeafSource<'a> { parent: BorrowedFd<'a>, name: &'a OsStr, observed: Observation, mount: Option<u64> }
struct LeafDestination<'a> { parent: BorrowedFd<'a>, name: &'a OsStr, visible: &'a Path, placement: &'a Placement<'a> }

fn transfer_leaf(invocation: &Invocation<'_>, source: LeafSource<'_>, target: LeafDestination<'_>, buffer: &mut [u8]) -> io::Result<Record> {
    let LeafSource { parent: source_parent, name: source_name, observed, mount: source_mount } = source;
    let LeafDestination { parent: target_parent, name: target_name, visible: visible_target, placement } = target;
    invocation.checkpoint()?;
    invocation.fault("before_copy")?;
    anchored::require_unchanged(source_parent, source_name, observed)?;
    if observed.identity.kind == EntryKind::Other { return Err(io::Error::new(io::ErrorKind::Unsupported, "special objects cannot be transferred")); }
    let source_file = if observed.identity.kind == EntryKind::RegularFile {
        let source = fs::openat(source_parent, source_name, READ_FILE, Mode::empty())?;
        if !anchored::unchanged(observed, roots::observe_fd(source.as_fd())?) { return Err(io::Error::other("source changed while opening")); }
        if let Some(mount) = source_mount && !invocation.request.options.cross_file_systems && platform::mount_id(source.as_fd())? != mount { return Err(io::Error::other("nested source file mount requires --cross-file-systems")); }
        Some(source)
    } else { None };

    let mut groups = if observed.identity.kind == EntryKind::RegularFile {
        let groups = invocation.hardlinks.lock().unwrap();
        if observed.links > 1 || groups.is_some() { Some(groups) } else { None }
    } else { None };
    if let Some(groups) = groups.as_mut() {
        if groups.is_none() {
            let existing_self_changes = 2 * usize::from(invocation.self_changes.lock().unwrap().is_some());
            invocation.require_descriptors((if source_mount.is_some() { 10 + 6 * invocation.jobs } else { 8 }) + existing_self_changes + 2)?;
            **groups = Some(IndexedRecords::new(&invocation.request.context.scratch)?);
        }
        if let Some(representative) = groups.as_mut().unwrap().find_identity(observed.identity)? {
            if !anchored::unchanged(invocation.effective(representative.observed)?, observed) { return Err(io::Error::other("hardlink source version changed after representative transfer")); }
            let root = representative_root(invocation, &representative, target_parent, placement)?;
            let expected_output = representative.output.ok_or_else(|| io::Error::other("hardlink representative version missing"))?;
            anchored::require_unchanged(root.parent.as_fd(), &root.name, expected_output)?;
            if representative.published.device != roots::observe_fd(target_parent)?.identity.device { return Err(io::Error::new(io::ErrorKind::Unsupported, "hardlink group cannot span destination filesystems")); }
            fs::linkat(&root.parent, &root.name, target_parent, target_name, AtFlags::empty())?;
            let linked = roots::observe(target_parent, target_name)?;
            let published = linked.identity;
            let mut expected_linked = expected_output;
            expected_linked.links += 1;
            expected_linked.ctime = linked.ctime;
            if !anchored::unchanged(expected_linked, linked) { return Err(with_cleanup(io::Error::other("hardlink representative changed during linking"), cleanup_owned(target_parent, target_name, published), visible_target)); }
            let mut updated_representative = representative.clone();
            updated_representative.output = Some(linked);
            groups.as_mut().unwrap().push(&updated_representative)?;
            if let Err(error) = anchored::require_unchanged(source_parent, source_name, observed) { return Err(with_cleanup(error, cleanup_owned(target_parent, target_name, published), visible_target)); }
            invocation.update(|progress| progress.completed_files += 1);
            return Ok(Record { source: placement.base.clone(), destination: placement.relative.clone(), observed, published, output: Some(roots::observe(target_parent, target_name)?), parent: Some(roots::observe_fd(target_parent)?.identity), anchor: Some(roots::observe_fd(placement.root)?.identity), ancestry: placement.ancestry()?, final_flags: 0 });
        }
    }
    let mut final_flags = 0;
    let created = match observed.identity.kind {
        EntryKind::RegularFile => {
            let source = source_file.unwrap();
            let metadata = platform::capture_metadata(MetadataTarget::Fd(source.as_fd()))?;
            if platform::requires_final_metadata(&metadata) { final_flags = metadata.flags; }
            #[cfg(target_os = "macos")]
            let cloned = match platform::try_clone_to_dir(source.as_fd(), target_parent, target_name) {
                Ok(()) => true,
                Err(platform::AccelerationError::Unsupported) => false,
                Err(platform::AccelerationError::Io(error)) => return Err(error.into()),
            };
            #[cfg(not(target_os = "macos"))]
            let cloned = false;
            let cloned_identity = if cloned {
                Some(roots::observe(target_parent, target_name).map_err(|error| io::Error::new(error.kind(), format!("cloned staging retained at {visible_target:?}; identity unavailable: {error}")))?.identity)
            } else { None };
            let opened = if cloned { fs::openat(target_parent, target_name, READ_FILE, Mode::empty()) } else { fs::openat(target_parent, target_name, OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::RUSR | Mode::WUSR) };
            let target = match opened {
                Ok(target) => target,
                Err(error) => return Err(match cloned_identity { Some(identity) => with_cleanup(error.into(), cleanup_owned(target_parent, target_name, identity), visible_target), None => error.into() }),
            };
            let published = roots::observe_fd(target.as_fd()).map_err(|error| io::Error::new(error.kind(), format!("staging retained at {visible_target:?}; descriptor identity unavailable: {error}")))?.identity;
            if cloned_identity.is_some_and(|identity| identity != published) { return Err(io::Error::other(format!("cloned staging substituted at {visible_target:?}; ambiguous object retained"))); }
            let result = (|| {
                if !cloned {
                    invocation.fault("during_copy")?;
                    let stats = platform::copy_data(source.as_fd(), target.as_fd(), observed.size, buffer, invocation.cancellation)?;
                    invocation.update(|progress| { progress.logical_bytes_cloned += if stats.method == CopyMethod::Clone { stats.logical_bytes } else { 0 }; progress.bytes_streamed += stats.streamed_bytes;
                        match stats.method { CopyMethod::Clone => progress.cloned_files += 1, CopyMethod::Native => progress.native_copied_files += 1, CopyMethod::Buffered => progress.buffered_copied_files += 1 } });
                } else { invocation.update(|progress| { progress.logical_bytes_cloned += observed.size; progress.cloned_files += 1; }); }
                #[cfg(target_os = "macos")]
                if !cloned && metadata.resource_fork_size.is_some() { platform::copy_resource_fork(source.as_fd(), target.as_fd())?; }
                invocation.fault("after_copy")?;
                if platform::reports_dense_allocation(source.as_fd(), target.as_fd(), observed.size)? {
                    invocation.emit(Event::Diagnostic(Diagnostic {
                        path: PathBuf::from(source_name),
                        message: "warning: destination reports dense allocation; sparse storage could not be verified".into(),
                    }));
                }
                invocation.checkpoint()?;
                anchored::require_unchanged(source_parent, source_name, observed)?;
                platform::apply_metadata(MetadataTarget::Fd(target.as_fd()), &observed, &metadata, preservation(invocation))?;
                invocation.fault("after_metadata")?;
                invocation.fault("during_sync")?;
                if invocation.durable() { invocation.sync_file(target.as_fd())?; }
                anchored::identity(target_parent, target_name, published)?;
                Ok(published)
            })();
            match result { Ok(published) => published, Err(error) => { let cleanup = cleanup_owned(target_parent, target_name, published); return Err(with_cleanup(error, cleanup, visible_target)); } }
        },
        EntryKind::Symlink => {
            let metadata = platform::capture_metadata(MetadataTarget::Symlink { parent: source_parent, name: source_name })?;
            if platform::requires_final_metadata(&metadata) { final_flags = metadata.flags; }
            let target = fs::readlinkat(source_parent, source_name, Vec::new())?;
            anchored::require_unchanged(source_parent, source_name, observed)?;
            fs::symlinkat(target.as_c_str(), target_parent, target_name)?;
            let published = roots::observe(target_parent, target_name)?.identity;
            let result = platform::apply_metadata(MetadataTarget::Symlink { parent: target_parent, name: target_name }, &observed, &metadata, preservation(invocation)).and_then(|()| anchored::require_unchanged(source_parent, source_name, observed));
            if let Err(error) = result { return Err(with_cleanup(error, cleanup_owned(target_parent, target_name, published), visible_target)); }
            published
        },
        _ => return Err(io::Error::other("invalid leaf kind")),
    };
    let record = Record { source: placement.base.clone(), destination: placement.relative.clone(), observed, published: created, output: Some(roots::observe(target_parent, target_name)?), parent: Some(roots::observe_fd(target_parent)?.identity), anchor: Some(roots::observe_fd(placement.root)?.identity), ancestry: if observed.links > 1 { placement.ancestry()? } else { Vec::new() }, final_flags };
    if observed.links > 1 && let Some(groups) = groups.as_mut() { groups.as_mut().unwrap().push(&record)?; }
    invocation.update(|progress| progress.completed_files += 1);
    Ok(record)
}

fn with_cleanup(primary: io::Error, cleanup: io::Result<()>, path: &Path) -> io::Error {
    match cleanup { Ok(()) => primary, Err(error) => io::Error::new(primary.kind(), format!("{primary}; staging retained at {path:?}: {error}")) }
}

pub(super) fn merge(invocation: &Invocation<'_>, source: &Root, destination: &Root) -> Result<OperandState, Failure> {
    invocation.require_descriptors(12 + invocation.index_descriptors())?;
    let _reservation = DescriptorReservation::new(invocation, 10);
    let source_directory = source.open_directory()?;
    let destination_directory = destination.open_directory()?;
    let source_mount = platform::mount_id(source_directory.as_fd())?;
    let destination_mount = platform::mount_id(destination_directory.as_fd())?;
    let directories = Mutex::new(Records::new(&invocation.request.context.scratch)?);
    let records = Mutex::new(Records::new(&invocation.request.context.scratch)?);
    let mut skipped = Records::new(&invocation.request.context.scratch)?;
    let mut pending = Records::new(&invocation.request.context.scratch)?;
    let root_record = Record { source: PathBuf::new(), destination: PathBuf::new(), observed: source.observed.unwrap(), published: destination.observed.unwrap().identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 };
    directories.lock().unwrap().push(&root_record)?;
    records.lock().unwrap().push(&root_record)?;
    pending.push(&root_record)?;
    let mut changed = false;
    let mut renamed = false;
    let result: Result<(), Failure> = (|| {
        let mut cursor = 0;
        loop {
            invocation.checkpoint()?;
            let Some(record) = pending.next(&mut cursor)? else { break; };
            let source_parent = anchored::open_directory(source_directory.as_fd(), &record.source, &directories, false)?;
            let destination_parent = anchored::open_directory(destination_directory.as_fd(), &record.source, &directories, true)?;
            let expected = invocation.effective(record.observed)?;
            if !anchored::unchanged(expected, roots::observe_fd(source_parent.as_fd())?) { return Err(io::Error::other("merge source directory changed").into()); }
            let mut children = Records::new(&invocation.request.context.scratch)?;
            for entry in fs::Dir::read_from(&source_parent)? {
                invocation.checkpoint()?;
                let entry = entry?;
                let name = OsStr::from_bytes(entry.file_name().to_bytes());
                if name == "." || name == ".." { continue; }
                let observed = roots::observe(source_parent.as_fd(), name)?;
                children.push(&Record { source: PathBuf::from(name), destination: PathBuf::new(), observed, published: observed.identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 })?;
            }
            if !anchored::unchanged(expected, roots::observe_fd(source_parent.as_fd())?) { return Err(io::Error::other("merge source directory changed during discovery").into()); }
            let mut child_cursor = 0;
            while let Some(child) = children.next(&mut child_cursor)? {
                invocation.checkpoint()?;
                let name = child.source.as_os_str();
                let source_observed = invocation.effective(child.observed)?;
                anchored::require_unchanged(source_parent.as_fd(), name, source_observed)?;
                let relative = record.source.join(name);
                if relative.as_os_str().len() > MAX_WORK_PATH { return Err(io::Error::other("merge traversal path exceeds bounded record limit").into()); }
                let destination_observed = match roots::observe(destination_parent.as_fd(), name) {
                    Ok(observed) => Some(observed),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                };
                if source_observed.identity.kind == EntryKind::Directory {
                    let child = fs::openat(&source_parent, name, DIRECTORY, Mode::empty())?;
                    if roots::observe_fd(child.as_fd())?.identity != source_observed.identity { return Err(io::Error::other("source merge directory substituted").into()); }
                    if !invocation.request.options.cross_file_systems && platform::mount_id(child.as_fd())? != source_mount { return Err(io::Error::other("nested source mount requires --cross-file-systems").into()); }
                }
                if destination_observed.is_some_and(|observed| observed.identity.kind == EntryKind::Directory) {
                    let child = fs::openat(&destination_parent, name, DIRECTORY, Mode::empty())?;
                    if roots::observe_fd(child.as_fd())?.identity != destination_observed.unwrap().identity { return Err(io::Error::other("destination merge directory substituted").into()); }
                    if !invocation.request.options.cross_file_systems && platform::mount_id(child.as_fd())? != destination_mount { return Err(io::Error::other("nested destination mount requires --cross-file-systems").into()); }
                }
                let action = policy::decide(source_observed, destination_observed.map(Target::Existing).unwrap_or(Target::Absent), &invocation.request.options)?;
                invocation.update(|progress| progress.discovered_files += 1);
                match action {
                    Action::Skip | Action::SameEntry => {
                        invocation.update(|progress| progress.skipped_files += 1);
                        skipped.push(&Record { source: relative.clone(), destination: relative, observed: source_observed, published: destination_observed.map(|observed| observed.identity).unwrap_or(source_observed.identity), output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 })?;
                    },
                    Action::Merge => {
                        let child = Record { source: relative.clone(), destination: relative, observed: source_observed, published: destination_observed.unwrap().identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 };
                        records.lock().unwrap().push(&child)?;
                        directories.lock().unwrap().push(&child)?;
                        pending.push(&child)?;
                    },
                    Action::Transfer | Action::Replace => {
                        let child_source = Root::establish_at(source_parent.as_fd(), name, &source.path.join(&relative), roots::Requirement::Existing)?;
                        let child_destination = Root::establish_at(destination_parent.as_fd(), name, &destination.path.join(&relative), roots::Requirement::MayBeAbsent)?;
                        if child_source.observed != Some(source_observed) || child_destination.observed != destination_observed { return Err(io::Error::other("merge child changed during planning").into()); }
                        if invocation.request.operation == Operation::Move {
                            match rename_object(invocation, &child_source, &child_destination, action) {
                                Ok(_) => {
                                    changed = true;
                                    renamed = true;
                                    invocation.remember_change(roots::observe_fd(source_parent.as_fd())?)?;
                                    continue;
                                },
                                Err(failure) if failure.error.raw_os_error() == Some(libc::EXDEV) => {},
                                Err(mut failure) => {
                                    if matches!(failure.state, OperandState::SourceRemovedSyncFailed | OperandState::SourcePartiallyRetired) { renamed = true; changed = true; }
                                    failure.error = io::Error::new(failure.error.kind(), format!("source {:?}, destination {:?}: {}", child_source.path, child_destination.path, failure.error));
                                    return Err(failure);
                                },
                            }
                        }
                        let receipt = whole(invocation, &child_source, &child_destination, action).map_err(|mut failure| {
                            failure.error = io::Error::new(failure.error.kind(), format!("source {:?}, destination {:?}: {}", child_source.path, child_destination.path, failure.error));
                            failure
                        })?;
                        changed = true;
                        let Some(receipt) = receipt else { continue; };
                        let mut child_cursor = 0;
                        while let Some(mut child) = receipt.directories.lock().unwrap().next(&mut child_cursor)? {
                            child.source = relative.join(&child.source);
                            child.destination = relative.join(&child.destination);
                            directories.lock().unwrap().push(&child)?;
                        }
                        let mut child_cursor = 0;
                        while let Some(mut child) = receipt.records.lock().unwrap().next(&mut child_cursor)? {
                            child.source = relative.join(&child.source);
                            child.destination = relative.join(&child.destination);
                            records.lock().unwrap().push(&child)?;
                        }
                    },
                }
            }
            let expected = invocation.effective(record.observed)?;
            if !anchored::unchanged(expected, roots::observe_fd(source_parent.as_fd())?) { return Err(io::Error::other("merge source directory changed during enumeration").into()); }
        }
        Ok(())
    })();
    if let Err(mut failure) = result {
        if renamed { failure.state = OperandState::SourcePartiallyRetired; }
        else if changed || failure.state == OperandState::DestinationPublishedSourceRetained { failure.state = OperandState::DestinationPartiallyPublished; }
        return Err(failure);
    }
    if invocation.request.operation == Operation::Copy { return Ok(OperandState::Copied); }
    let mut transferable = Records::new(&invocation.request.context.scratch)?;
    let mut cursor = 0;
    while let Some(record) = records.lock().unwrap().next(&mut cursor)? {
        let mut retain = false;
        if record.observed.identity.kind == EntryKind::Directory {
            let mut skipped_cursor = 0;
            while let Some(skipped) = skipped.next(&mut skipped_cursor)? {
                if skipped.source.starts_with(&record.source) { retain = true; break; }
            }
        }
        if !retain { transferable.push(&record)?; }
    }
    if transferable.end() == 0 { return Ok(if changed { OperandState::Moved } else { OperandState::Skipped }); }
    let transferable = Mutex::new(transferable);
    let destinations = snapshot_destinations(invocation, destination, Some(&destination_directory), &transferable, &directories)?;
    invocation.require_descriptors(24).map_err(|error| Failure { state: if renamed { OperandState::SourcePartiallyRetired } else if changed { OperandState::DestinationPartiallyPublished } else { OperandState::FailedBeforePublication }, error })?;
    let receipt = DurableReceipt { source_parent: duplicate(source.parent.as_fd())?, source_name: source.name.clone(), source_path: source.path.clone(), source_directory: Some(source_directory), records: transferable, directories, destination_parent: duplicate(destination.parent.as_fd())?, destination_name: destination.name.clone(), destination_directory: Some(destination_directory), destinations: Mutex::new(destinations) };
    super::retirement::retire(invocation, receipt)
}

fn open_published_directory(destination: &Root, identity: Identity) -> io::Result<OwnedFd> {
    let fd = fs::openat(&destination.parent, &destination.name, DIRECTORY, Mode::empty())?;
    if roots::observe_fd(fd.as_fd())?.identity != identity { return Err(io::Error::other("published root identity changed")); }
    Ok(fd)
}

fn snapshot_destinations(invocation: &Invocation<'_>, destination: &Root, root: Option<&OwnedFd>, entries: &Mutex<Records>, directories: &Mutex<Records>) -> io::Result<Records> {
    let mut snapshots = Records::new(&invocation.request.context.scratch)?;
    let mut cursor = 0;
    while let Some(record) = entries.lock().unwrap().next(&mut cursor)? {
        let (parent, name) = if record.source.as_os_str().is_empty() { (duplicate(destination.parent.as_fd())?, destination.name.clone()) } else { anchored::parent(root.ok_or_else(|| io::Error::other("published directory missing"))?.as_fd(), &record.source, directories, true)? };
        let observed = roots::observe(parent.as_fd(), &name)?;
        if observed.identity != record.published { return Err(io::Error::other("destination identity changed before receipt")); }
        snapshots.push(&Record { source: record.source, destination: record.destination, observed, published: observed.identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 })?;
    }
    Ok(snapshots)
}

fn validate_sources(source: &Root, root: Option<&OwnedFd>, stage: &Stage<'_>) -> io::Result<()> {
    let mut cursor = 0;
    while let Some(record) = stage.entries.lock().unwrap().next(&mut cursor)? {
        let (parent, name) = if record.source.as_os_str().is_empty() { (duplicate(source.parent.as_fd())?, source.name.clone()) } else { anchored::parent(root.ok_or_else(|| io::Error::other("source directory missing"))?.as_fd(), &record.source, &stage.directories, false)? };
        anchored::require_unchanged(parent.as_fd(), &name, record.observed)?;
    }
    Ok(())
}

struct Placement<'a> {
    root: BorrowedFd<'a>,
    base: PathBuf,
    relative: PathBuf,
    directories: Option<&'a Mutex<Records>>,
}

impl Placement<'_> {
    fn ancestry(&self) -> io::Result<Vec<Identity>> {
        let mut result = Vec::new();
        let mut path = PathBuf::new();
        for component in self.relative.parent().unwrap_or_else(|| Path::new("")).components() {
            path.push(component.as_os_str());
            let directory = self.directories.ok_or_else(|| io::Error::other("hardlink ancestry records missing"))?.lock().unwrap().find(&path)?.ok_or_else(|| io::Error::other("hardlink ancestry identity missing"))?;
            result.push(directory.published);
        }
        Ok(result)
    }
}

fn representative_root(invocation: &Invocation<'_>, representative: &Record, target_parent: BorrowedFd<'_>, placement: &Placement<'_>) -> io::Result<Root> {
    let name = representative.destination.file_name().ok_or_else(|| io::Error::other("hardlink representative has no name"))?;
    if representative.parent == Some(roots::observe_fd(target_parent)?.identity) {
        return Root::establish_at(target_parent, name, &representative.source.join(&representative.destination), roots::Requirement::Existing);
    }
    let expected_root = representative.anchor.ok_or_else(|| io::Error::other("hardlink anchor missing"))?;
    let mut parent = if roots::observe_fd(placement.root)?.identity == expected_root {
        duplicate(placement.root)?
    } else {
        let root = Root::establish(invocation.cwd, &representative.source, roots::Requirement::Existing)?;
        if root.observed.unwrap().identity != expected_root { return Err(io::Error::other("hardlink anchor identity changed")); }
        root.open_directory()?
    };
    let components = representative.destination.parent().unwrap_or_else(|| Path::new("")).components();
    if components.clone().count() != representative.ancestry.len() { return Err(io::Error::other("hardlink ancestry length changed")); }
    for (component, identity) in components.zip(&representative.ancestry) {
        let std::path::Component::Normal(name) = component else { return Err(io::Error::other("invalid hardlink ancestry component")); };
        let next = fs::openat(&parent, name, DIRECTORY, Mode::empty())?;
        if roots::observe_fd(next.as_fd())?.identity != *identity { return Err(io::Error::other("hardlink ancestry identity changed")); }
        parent = next;
    }
    if representative.parent != Some(roots::observe_fd(parent.as_fd())?.identity) { return Err(io::Error::other("hardlink representative parent changed")); }
    Root::establish_at(parent.as_fd(), name, &representative.source.join(&representative.destination), roots::Requirement::Existing)
}

struct Finished<'a> { count: &'a AtomicUsize, stopped: &'a std::sync::atomic::AtomicBool }
impl Drop for Finished<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() { self.stopped.store(true, Ordering::Release); }
        self.count.fetch_add(1, Ordering::Release);
    }
}

fn enqueue(sender: &mpsc::SyncSender<Work>, mut work: Work, invocation: &Invocation<'_>) -> io::Result<()> {
    loop {
        invocation.checkpoint()?;
        match sender.try_send(work) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => return Err(io::Error::other("transfer workers stopped")),
            Err(mpsc::TrySendError::Full(pending)) => { work = pending; std::thread::park_timeout(Duration::from_millis(5)); },
        }
    }
}


struct DescriptorReservation<'a, 'b> { invocation: &'a Invocation<'b>, count: usize }
impl<'a, 'b> DescriptorReservation<'a, 'b> {
    fn new(invocation: &'a Invocation<'b>, count: usize) -> Self { invocation.descriptor_reservation.fetch_add(count, Ordering::AcqRel); Self { invocation, count } }
}
impl Drop for DescriptorReservation<'_, '_> { fn drop(&mut self) { self.invocation.descriptor_reservation.fetch_sub(self.count, Ordering::AcqRel); } }
