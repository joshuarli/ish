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
    last: Option<OsString>,
}

pub(super) fn plan(invocation: &Invocation<'_>, source: &Root, destination: &Root, action: Action) -> io::Result<OperandState> {
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
    let mut frames = vec![Frame { name: OsString::new(), observed: source.observed.unwrap(), destination: if action == Action::Merge { destination.observed } else { None }, last: None }];
    let mut bytes = std::mem::size_of::<Frame>();
    while !frames.is_empty() {
        invocation.checkpoint()?;
        let (directory, target, relative) = reopen(&source_root, target_root.as_ref(), &frames)?;
        let index = frames.len() - 1;
        if !anchored::unchanged(frames[index].observed, roots::observe_fd(directory.as_fd())?) { return Err(io::Error::other("source directory changed during dry-run")); }
        let mut next = None::<OsString>;
        for entry in fs::Dir::read_from(&directory)? {
            invocation.checkpoint()?;
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." || frames[index].last.as_ref().is_some_and(|last| name <= last.as_os_str()) { continue; }
            if next.as_ref().is_none_or(|next| name < next.as_os_str()) { next = Some(name.to_os_string()); }
        }
        let Some(name) = next else {
            let frame = frames.pop().unwrap();
            bytes -= std::mem::size_of::<Frame>() + frame.name.len() + frame.last.as_ref().map_or(0, |name| name.len());
            continue;
        };
        bytes -= frames[index].last.as_ref().map_or(0, |name| name.len());
        bytes += name.len();
        frames[index].last = Some(name.clone());
        let observed = roots::observe(directory.as_fd(), &name)?;
        let target_observed = if let Some(target) = &target {
            match roots::observe(target.as_fd(), &name) { Ok(observed) => Some(observed), Err(error) if error.kind() == io::ErrorKind::NotFound => None, Err(error) => return Err(error) }
        } else { None };
        let action = policy::decide(observed, target_observed.map(Target::Existing).unwrap_or(Target::Absent), &invocation.request.options)?;
        let path = relative.join(&name);
        invocation.update(|progress| { progress.discovered_files += 1; if action == Action::Skip { progress.skipped_files += 1; } });
        invocation.emit(Event::Operand(OperandOutcome { source: source.path.join(&path), destination: destination.path.join(&path), state: if action == Action::Skip { OperandState::Skipped } else { OperandState::Planned } }));
        if action == Action::Skip || action == Action::SameEntry { continue; }
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
            bytes += std::mem::size_of::<Frame>() + name.len();
            if bytes > RECORD_MEMORY_BYTES / 2 { return Err(io::Error::other("dry-run traversal exceeds record memory budget")); }
            frames.push(Frame { name, observed, destination: if action == Action::Merge { target_observed } else { None }, last: None });
        }
    }
    Ok(OperandState::Planned)
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
