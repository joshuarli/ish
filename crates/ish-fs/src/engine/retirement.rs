use std::io;
use std::path::Path;

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs;
use fdu_posix::quarantine::{Ancestor, Boundary, Removal, Retirement, Session};

use super::anchored;
use super::records::Record;
use super::transfer::DurableReceipt;
use super::{Failure, Invocation};
use crate::roots::{self, EntryKind};
use crate::{OperandState};

pub(super) fn retire(invocation: &Invocation<'_>, receipt: DurableReceipt) -> Result<OperandState, Failure> {
    let _span = super::Span::new(invocation, super::SpanKind::Retirement);
    let session = Session::start(receipt.source_parent.as_fd()).map_err(|error| Failure { state: OperandState::DestinationPublishedSourceRetained, error })?;
    let result = retire_with_session(invocation, &receipt, &session);
    match (result, session.finish()) {
        (Ok(state), Ok(())) => Ok(state),
        (Ok(_), Err(error)) => Err(Failure { state: OperandState::SourceRemovedSyncFailed, error }),
        (Err(failure), Ok(())) => Err(failure),
        (Err(mut failure), Err(cleanup)) => { failure.error = io::Error::new(failure.error.kind(), format!("{}; journal cleanup failed: {cleanup}", failure.error)); Err(failure) },
    }
}

fn retire_with_session(invocation: &Invocation<'_>, receipt: &DurableReceipt, session: &Session) -> Result<OperandState, Failure> {
    let fail = |error| Failure { state: OperandState::DestinationPublishedSourceRetained, error };
    invocation.checkpoint().map_err(fail)?;
    invocation.fault("before_retire").map_err(fail)?;
    let mut cursor = 0;
    loop {
        let record = receipt.records.lock().unwrap().next(&mut cursor).map_err(fail)?;
        let Some(record) = record else { break; };
        validate_destination(invocation, receipt, &record).map_err(fail)?;
        let (parent, name) = source_parent(receipt, &record).map_err(fail)?;
        if record.observed.identity.kind == EntryKind::Directory {
            let directory = fs::openat(&parent, &name, anchored::DIRECTORY, fs::Mode::empty()).map_err(|error| fail(error.into()))?;
            Session::check_recovery(directory.as_fd()).map_err(fail)?;
        }
        let expected = invocation.effective(record.observed).map_err(fail)?;
        anchored::require_unchanged(parent.as_fd(), &name, expected).map_err(fail)?;
    }
    let mut removed = 0;
    let mut cursor = receipt.records.lock().unwrap().end();
    loop {
        let record = receipt.records.lock().unwrap().previous(&mut cursor).map_err(|error| failure(error, removed))?;
        let Some(record) = record else { break; };
        invocation.checkpoint().map_err(|error| failure(error, removed))?;
        let (parent, name) = source_parent(receipt, &record).map_err(|error| failure(error, removed))?;
        let expected = invocation.effective(record.observed).map_err(|error| failure(error, removed))?;
        anchored::require_unchanged(parent.as_fd(), &name, expected).map_err(|error| failure(error, removed))?;
        if !record.source.as_os_str().is_empty() {
            let parent_path = record.source.parent().unwrap_or_else(|| Path::new(""));
            let parent_record = receipt.directories.lock().unwrap().find(parent_path).map_err(|error| failure(error, removed))?.ok_or_else(|| failure(io::Error::other("source parent observation missing"), removed))?;
            let parent_expected = invocation.effective(parent_record.observed).map_err(|error| failure(error, removed))?;
            if !anchored::unchanged(parent_expected, roots::observe_fd(parent.as_fd()).map_err(|error| failure(error, removed))?) { return Err(failure(io::Error::other("source parent changed before retirement"), removed)); }
        }
        validate_destination(invocation, receipt, &record).map_err(|error| failure(error, removed))?;
        let linked_source = if matches!(expected.identity.kind, EntryKind::RegularFile | EntryKind::Symlink) && expected.links > 1 {
            Some(fdu_posix::identity::open_no_follow(parent.as_fd(), &name, expected.identity.kind).map_err(|error| failure(error, removed))?)
        } else { None };
        let mut parents = Vec::new();
        if !record.source.as_os_str().is_empty() {
            let root = receipt.directories.lock().unwrap().find(Path::new("")).map_err(|error| failure(error, removed))?.ok_or_else(|| failure(io::Error::other("source root record missing"), removed))?;
            parents.push(Ancestor { name: receipt.source_name.clone(), identity: root.observed.identity.file });
            let mut prefix = std::path::PathBuf::new();
            for component in record.source.parent().unwrap_or_else(|| Path::new("")).components() {
                let std::path::Component::Normal(name) = component else { return Err(failure(io::Error::other("invalid source ancestry"), removed)); };
                prefix.push(name);
                let ancestor = receipt.directories.lock().unwrap().find(&prefix).map_err(|error| failure(error, removed))?.ok_or_else(|| failure(io::Error::other("source ancestor record missing"), removed))?;
                parents.push(Ancestor { name: name.to_os_string(), identity: ancestor.observed.identity.file });
            }
        }
        let (crossed_root, parents) = mount_root(receipt, parents).map_err(|error| failure(error, removed))?;
        let crossed_session = if let Some((root, path)) = &crossed_root {
            if !invocation.request.options.cross_file_systems { return Err(failure(io::Error::other("crossed retirement mount requires --cross-file-systems"), removed)); }
            let before = roots::observe_fd(root.as_fd()).map_err(|error| failure(error, removed))?;
            let recorded = receipt.directories.lock().unwrap().find(path).map_err(|error| failure(error, removed))?.ok_or_else(|| failure(io::Error::other("crossed mount root observation missing"), removed))?;
            if !anchored::unchanged(invocation.effective(recorded.observed).map_err(|error| failure(error, removed))?, before) { return Err(failure(io::Error::other("crossed mount root changed before journal creation"), removed)); }
            let session = Session::start(root.as_fd()).map_err(|error| failure(error, removed))?;
            remember_session_root_change(invocation, before, root.as_fd(), 1).map_err(|error| failure(error, removed))?;
            Some(session)
        } else { None };
        let active_session = crossed_session.as_ref().unwrap_or(session);
        let request = Retirement::new(parents, name.clone(), expected.identity).map_err(|error| failure(error, removed))?;
        let sync_failure_state = if record.source.as_os_str().is_empty() { OperandState::SourceRemovedSyncFailed } else { OperandState::SourcePartiallyRetired };
        let mut captured = None;
        let parent_before = roots::observe_fd(parent.as_fd()).map_err(|error| failure(error, removed))?;
        let mut parent_sync_started = false;
        let result = active_session.remove(parent.as_fd(), request, |boundary, captured_parent, captured_name| {
            invocation.fault(match boundary {
                Boundary::IntentPublished => "journal_intent_published",
                Boundary::Published => "journal_published",
                Boundary::BeforeCapture => "journal_before_capture",
                Boundary::Captured => "journal_captured",
                Boundary::CaptureParentSynced => "journal_capture_parent_synced",
                Boundary::CaptureDirectorySynced => "journal_capture_directory_synced",
                Boundary::BeforeUnlink => "journal_before_unlink",
                Boundary::Unlinked => "journal_unlinked",
                Boundary::RemovalDirectorySynced => "journal_removal_directory_synced",
                Boundary::BeforeParentSync => "journal_before_parent_sync",
                Boundary::ParentSynced => "journal_parent_synced",
                Boundary::IntentRemoved => "journal_intent_removed",
                Boundary::IntentRemovalSynced => "journal_intent_removal_synced",
                Boundary::Restored => "journal_restored",
            })?;
            match boundary {
                Boundary::Restored => super::remember_restoration(invocation, captured.unwrap_or(expected), parent_before, captured_parent, captured_name),
                Boundary::BeforeCapture => {
                    invocation.fault("before_capture")?;
                    invocation.checkpoint()?;
                    anchored::require_unchanged(parent.as_fd(), &name, invocation.effective(expected)?)?;
                    validate_destination(invocation, receipt, &record)
                },
                Boundary::Captured => {
                    let actual = roots::observe(captured_parent, captured_name)?;
                    let mut comparable = expected;
                    comparable.ctime = actual.ctime;
                    if !anchored::unchanged(comparable, actual) { return Err(io::Error::other("captured source changed")); }
                    if linked_source.is_some() { invocation.remember_change(actual)?; }
                    captured = Some(actual);
                    invocation.fault("after_capture")?;
                    invocation.checkpoint()
                },
                Boundary::BeforeUnlink => {
                    invocation.fault("before_remove")?;
                    invocation.checkpoint()?;
                    anchored::require_unchanged(captured_parent, captured_name, captured.ok_or_else(|| io::Error::other("captured observation missing"))?)?;
                    validate_destination(invocation, receipt, &record)
                },
                Boundary::Unlinked => {
                    if let Some(linked_source) = &linked_source {
                        let actual = roots::observe_fd(linked_source.as_fd())?;
                        let mut expected_unlinked = captured.ok_or_else(|| io::Error::other("captured observation missing"))?;
                        expected_unlinked.links = expected_unlinked.links.checked_sub(1).ok_or_else(|| io::Error::other("source link count underflow"))?;
                        expected_unlinked.ctime = actual.ctime;
                        if !anchored::unchanged(expected_unlinked, actual) { return Err(io::Error::other("source hardlink changed during retirement")); }
                        invocation.remember_change(actual)?;
                    }
                    invocation.fault("after_remove")
                },
                Boundary::BeforeParentSync => { parent_sync_started = true; invocation.fault("source_parent_sync") },
                Boundary::ParentSynced => invocation.fault("source_parent_synced"),
                _ => Ok(()),
            }
        });
        let cleanup = if let Some(session) = &crossed_session {
            let root = &crossed_root.as_ref().unwrap().0;
            let before = roots::observe_fd(root.as_fd()).map_err(|error| failure(error, removed))?;
            session.finish().and_then(|()| remember_session_root_change(invocation, before, root.as_fd(), -1))
        } else { Ok(()) };
        let result = match (result, cleanup) {
            (result, Ok(())) => result,
            (Ok(result), Err(error)) => Err(fdu_posix::quarantine::RemovalError { removed: result == Removal::Removed, error }),
            (Err(mut failure), Err(cleanup)) => { failure.error = io::Error::new(failure.error.kind(), format!("{}; crossed-mount journal cleanup failed: {cleanup}", failure.error)); Err(failure) },
        };
        match result {
            Ok(Removal::Removed) => { removed += 1; },
            Ok(Removal::AlreadyAbsent | Removal::Changed) => return Err(failure(io::Error::other("source changed before capture"), removed)),
            Err(error) => {
                let source_path = if record.source.as_os_str().is_empty() { receipt.source_path.clone() } else { receipt.source_path.join(&record.source) };
                let message = if error.removed && parent_sync_started {
                    io::Error::new(error.error.kind(), format!("source entry {source_path:?} removed; parent synchronization failed at {:?}: {}", source_path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")), error.error))
                } else { io::Error::new(error.error.kind(), format!("source {source_path:?}, recovery root {:?}: {}", receipt.source_path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")), error.error)) };
                return Err(if error.removed { Failure { state: sync_failure_state, error: message } } else { failure(message, removed) });
            },
        }
        invocation.remember_change(roots::observe_fd(parent.as_fd()).map_err(|error| failure(error, removed))?).map_err(|error| failure(error, removed))?;

    }
    Ok(OperandState::Moved)
}

fn remember_session_root_change(invocation: &Invocation<'_>, before: roots::Observation, root: rustix::fd::BorrowedFd<'_>, links: i64) -> io::Result<()> {
    let actual = roots::observe_fd(root)?;
    let mut expected = before;
    expected.links = expected.links.checked_add_signed(links).ok_or_else(|| io::Error::other("session root link count overflow"))?;
    expected.size = actual.size;
    expected.mtime = actual.mtime;
    expected.ctime = actual.ctime;
    if !anchored::unchanged(expected, actual) { return Err(io::Error::other("mount root changed during journal namespace update")); }
    invocation.remember_change(actual)
}

type MountRoot = (OwnedFd, std::path::PathBuf);

fn mount_root(receipt: &DurableReceipt, parents: Vec<Ancestor>) -> io::Result<(Option<MountRoot>, Vec<Ancestor>)> {
    let mut current = fs::openat(&receipt.source_parent, ".", anchored::DIRECTORY, fs::Mode::empty())?;
    let mut mount = fdu_posix::identity::mount_identity(current.as_fd())?;
    let mut root = None;
    let mut ancestry = Vec::new();
    let mut path = std::path::PathBuf::new();
    for (index, parent) in parents.into_iter().enumerate() {
        if index != 0 { path.push(&parent.name); }
        let next = fs::openat(&current, &parent.name, anchored::DIRECTORY, fs::Mode::empty())?;
        if roots::observe_fd(next.as_fd())?.identity.file != parent.identity { return Err(io::Error::other("retirement ancestry changed while locating mount root")); }
        let next_mount = fdu_posix::identity::mount_identity(next.as_fd())?;
        if next_mount != mount {
            Session::check_recovery(next.as_fd())?;
            root = Some((fs::openat(&next, ".", anchored::DIRECTORY, fs::Mode::empty())?, path.clone()));
            ancestry.clear();
            mount = next_mount;
        } else { ancestry.push(parent); }
        current = next;
    }
    Ok((root, ancestry))
}

fn failure(error: io::Error, removed: usize) -> Failure {
    Failure { state: if removed == 0 { OperandState::DestinationPublishedSourceRetained } else { OperandState::SourcePartiallyRetired }, error }
}

fn source_parent(receipt: &DurableReceipt, record: &Record) -> io::Result<(rustix::fd::OwnedFd, std::ffi::OsString)> {
    if record.source.as_os_str().is_empty() {
        let parent = fs::openat(&receipt.source_parent, ".", super::anchored::DIRECTORY, rustix::fs::Mode::empty())?;
        Ok((parent, receipt.source_name.clone()))
    } else {
        let root = receipt.directories.lock().unwrap().find(Path::new(""))?.ok_or_else(|| io::Error::other("source root record missing"))?;
        anchored::identity(receipt.source_parent.as_fd(), &receipt.source_name, root.observed.identity)?;
        anchored::checked_parent(receipt.source_directory.as_ref().ok_or_else(|| io::Error::other("source directory descriptor missing"))?.as_fd(), &record.source, &receipt.directories, false).map_err(|error| io::Error::new(error.kind(), format!("retirement source traversal {:?}: {error}", record.source)))
    }
}

fn validate_destination(invocation: &Invocation<'_>, receipt: &DurableReceipt, record: &Record) -> io::Result<()> {
    let expected = receipt.destinations.lock().unwrap().find(&record.source)?.ok_or_else(|| io::Error::other("destination receipt entry missing"))?;
    let root_expected = receipt.directories.lock().unwrap().find(Path::new(""))?;
    if let Some(root) = root_expected { anchored::identity(receipt.destination_parent.as_fd(), &receipt.destination_name, root.published)?; }
    let (parent, name) = if record.source.as_os_str().is_empty() {
        (fs::openat(&receipt.destination_parent, ".", super::anchored::DIRECTORY, rustix::fs::Mode::empty())?, receipt.destination_name.clone())
    } else {
        anchored::checked_parent(receipt.destination_directory.as_ref().ok_or_else(|| io::Error::other("destination root descriptor missing"))?.as_fd(), &record.source, &receipt.directories, true)?
    };
    if expected.observed.identity.kind == EntryKind::Directory {
        let directory = fs::openat(&parent, &name, anchored::DIRECTORY, fs::Mode::empty())?;
        Session::check_recovery(directory.as_fd())?;
    }
    if !anchored::unchanged(invocation.effective(expected.observed)?, roots::observe(parent.as_fd(), &name)?) { return Err(io::Error::other("destination identity or version changed before source retirement")); }
    Ok(())
}
