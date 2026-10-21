use std::io;
use std::path::Path;

use rustix::fd::AsFd;
use rustix::fs::{self, AtFlags};

use super::anchored;
use super::records::{self, Record};
use super::transfer::DurableReceipt;
use super::{Failure, Invocation, unique_name};
use crate::roots::{self, EntryKind};
use crate::{OperandState};

pub(super) fn retire(invocation: &Invocation<'_>, receipt: DurableReceipt) -> Result<OperandState, Failure> {
    let _span = super::Span::new(invocation, super::SpanKind::Retirement);
    let fail = |error| Failure { state: OperandState::DestinationPublishedSourceRetained, error };
    invocation.checkpoint().map_err(fail)?;
    invocation.fault("before_retire").map_err(fail)?;
    let mut cursor = 0;
    loop {
        let record = receipt.records.lock().unwrap().next(&mut cursor).map_err(fail)?;
        let Some(record) = record else { break; };
        validate_destination(invocation, &receipt, &record).map_err(fail)?;
        let (parent, name) = source_parent(&receipt, &record).map_err(fail)?;
        let expected = invocation.effective(record.observed).map_err(fail)?;
        anchored::require_unchanged(parent.as_fd(), &name, expected).map_err(fail)?;
    }
    let mut removed = 0;
    let mut cursor = receipt.records.lock().unwrap().end();
    loop {
        let record = receipt.records.lock().unwrap().previous(&mut cursor).map_err(|error| failure(error, removed))?;
        let Some(record) = record else { break; };
        invocation.checkpoint().map_err(|error| failure(error, removed))?;
        let (parent, name) = source_parent(&receipt, &record).map_err(|error| failure(error, removed))?;
        let expected = invocation.effective(record.observed).map_err(|error| failure(error, removed))?;
        anchored::require_unchanged(parent.as_fd(), &name, expected).map_err(|error| failure(error, removed))?;
        if !record.source.as_os_str().is_empty() {
            let parent_path = record.source.parent().unwrap_or_else(|| Path::new(""));
            let parent_record = receipt.directories.lock().unwrap().find(parent_path).map_err(|error| failure(error, removed))?.ok_or_else(|| failure(io::Error::other("source parent observation missing"), removed))?;
            let parent_expected = invocation.effective(parent_record.observed).map_err(|error| failure(error, removed))?;
            if !anchored::unchanged(parent_expected, roots::observe_fd(parent.as_fd()).map_err(|error| failure(error, removed))?) { return Err(failure(io::Error::other("source parent changed before retirement"), removed)); }
        }
        validate_destination(invocation, &receipt, &record).map_err(|error| failure(error, removed))?;
        let linked_source = if matches!(expected.identity.kind, EntryKind::RegularFile | EntryKind::Symlink) && expected.links > 1 {
            let flags = if expected.identity.kind == EntryKind::Symlink {
                #[cfg(target_os = "linux")]
                { rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC }
                #[cfg(target_os = "macos")]
                { rustix::fs::OFlags::from_bits_retain(libc::O_SYMLINK as u32) | rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC }
            } else { super::anchored::READ_FILE };
            Some(fs::openat(&parent, &name, flags, rustix::fs::Mode::empty()).map_err(|error| failure(error.into(), removed))?)
        } else { None };
        let capture = unique_name("retired");
        let intent = unique_name("intent");
        let parent_path = if record.source.as_os_str().is_empty() {
            receipt.source_path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")).to_path_buf()
        } else {
            match record.source.parent().filter(|path| !path.as_os_str().is_empty()) { Some(parent) => receipt.source_path.join(parent), None => receipt.source_path.clone() }
        };
        let removed_path = if record.source.as_os_str().is_empty() { receipt.source_path.clone() } else { receipt.source_path.join(&record.source) };
        let sync_failure_state = if record.source.as_os_str().is_empty() { OperandState::SourceRemovedSyncFailed } else { OperandState::SourcePartiallyRetired };
        let capture_path = parent_path.join(&capture);
        let intent_path = parent_path.join(&intent);
        let intent_record = Record { source: std::path::PathBuf::from(&name), destination: std::path::PathBuf::from(&capture), observed: expected, published: record.published, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 };
        let intent_identity = records::write_intent(parent.as_fd(), &intent, &intent_record).map_err(|error| failure(error, removed))?;
        let intent_observed = records::validate_intent(parent.as_fd(), &intent, intent_identity).map_err(|error| failure(error, removed))?;
        if intent_observed != intent_record { return Err(failure(io::Error::other("source capture intent changed"), removed)); }
        if let Err(error) = invocation.fault("before_capture").and_then(|()| crate::platform::rename_noreplace(parent.as_fd(), &name, parent.as_fd(), &capture)) {
            return Err(failure(clean_intent(parent.as_fd(), &intent, intent_identity, error, &intent_path), removed));
        }
        let captured = roots::observe(parent.as_fd(), &capture).map_err(|error| failure(error, removed))?;
        let mut comparable = expected;
        comparable.ctime = captured.ctime;
        if !anchored::unchanged(comparable, captured) {
            let restoration = restore(parent.as_fd(), &capture, &name, expected.identity);
            let error = match restoration {
                Ok(()) => clean_intent(parent.as_fd(), &intent, intent_identity, io::Error::other("captured source changed; restored original name"), &intent_path),
                Err(error) => io::Error::other(format!("captured source changed; retained at {capture_path:?}, intent {intent_path:?}; restoration failed: {error}")),
            };
            return Err(failure(error, removed));
        }
        if linked_source.is_some() { invocation.remember_change(captured).map_err(|error| failure(error, removed))?; }
        if let Err(error) = invocation.fault("after_capture").and_then(|()| invocation.checkpoint()) {
            let restoration = restore(parent.as_fd(), &capture, &name, expected.identity);
            let primary = io::Error::new(error.kind(), format!("{error}; source capture {capture_path:?}, intent {intent_path:?}; restoration {restoration:?}"));
            let primary = if restoration.is_ok() { clean_intent(parent.as_fd(), &intent, intent_identity, primary, &intent_path) } else { primary };
            return Err(failure(primary, removed));
        }
        if let Err(error) = anchored::require_unchanged(parent.as_fd(), &capture, captured).and_then(|()| validate_destination(invocation, &receipt, &record)) {
            let restoration = restore(parent.as_fd(), &capture, &name, expected.identity);
            let primary = io::Error::new(error.kind(), format!("{error}; retained source {capture_path:?}, intent {intent_path:?}; restoration {restoration:?}"));
            return Err(failure(if restoration.is_ok() { clean_intent(parent.as_fd(), &intent, intent_identity, primary, &intent_path) } else { primary }, removed));
        }
        let flags = if expected.identity.kind == EntryKind::Directory { AtFlags::REMOVEDIR } else { AtFlags::empty() };
        if let Err(error) = invocation.fault("before_remove").and_then(|()| anchored::require_unchanged(parent.as_fd(), &capture, captured)).and_then(|()| validate_destination(invocation, &receipt, &record)).and_then(|()| fs::unlinkat(&parent, &capture, flags).map_err(Into::into)) {
            let restoration = restore(parent.as_fd(), &capture, &name, expected.identity);
            let primary = io::Error::other(format!("source removal failed: {error}; captured {capture_path:?}, intent {intent_path:?}; restoration {restoration:?}"));
            return Err(failure(if restoration.is_ok() { clean_intent(parent.as_fd(), &intent, intent_identity, primary, &intent_path) } else { primary }, removed));
        }
        removed += 1;
        invocation.fault("after_remove").map_err(|error| Failure { state: sync_failure_state, error: io::Error::new(error.kind(), format!("{error}; source entry {removed_path:?} removed before directory synchronization; intent retained at {intent_path:?}")) })?;
        if let Some(linked_source) = &linked_source {
            let updated = roots::observe_fd(linked_source.as_fd()).map_err(|error| failure(error, removed))?;
            let mut expected_unlinked = captured;
            expected_unlinked.links -= 1;
            expected_unlinked.ctime = updated.ctime;
            if !anchored::unchanged(expected_unlinked, updated) { return Err(failure(io::Error::other("source hardlink changed during retirement"), removed)); }
            invocation.remember_change(updated).map_err(|error| failure(error, removed))?;
        }
        remove_intent(parent.as_fd(), &intent, intent_identity).map_err(|error| failure(io::Error::new(error.kind(), format!("source retired; intent retained at {intent_path:?}: {error}")), removed))?;
        let parent_observed = roots::observe_fd(parent.as_fd()).map_err(|error| failure(error, removed))?;
        invocation.remember_change(parent_observed).map_err(|error| failure(error, removed))?;
        invocation.fault("source_parent_sync").and_then(|()| crate::platform::sync_directory(parent.as_fd())).and_then(|()| invocation.fault("source_parent_synced")).map_err(|error| Failure { state: sync_failure_state, error: io::Error::new(error.kind(), format!("source entry {removed_path:?} removed; parent synchronization failed at {parent_path:?}: {error}")) })?;
    }
    Ok(OperandState::Moved)
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
        anchored::parent(receipt.source_directory.as_ref().ok_or_else(|| io::Error::other("source directory descriptor missing"))?.as_fd(), &record.source, &receipt.directories, false).map_err(|error| io::Error::new(error.kind(), format!("retirement source traversal {:?}: {error}", record.source)))
    }
}

fn remove_intent(parent: rustix::fd::BorrowedFd<'_>, name: &std::ffi::OsStr, identity: roots::Identity) -> io::Result<()> {
    anchored::identity(parent, name, identity)?;
    fs::unlinkat(parent, name, AtFlags::empty()).map_err(Into::into)
}

fn validate_destination(invocation: &Invocation<'_>, receipt: &DurableReceipt, record: &Record) -> io::Result<()> {
    let expected = receipt.destinations.lock().unwrap().find(&record.source)?.ok_or_else(|| io::Error::other("destination receipt entry missing"))?;
    let root_expected = receipt.directories.lock().unwrap().find(Path::new(""))?;
    if let Some(root) = root_expected { anchored::identity(receipt.destination_parent.as_fd(), &receipt.destination_name, root.published)?; }
    let (parent, name) = if record.source.as_os_str().is_empty() {
        (fs::openat(&receipt.destination_parent, ".", super::anchored::DIRECTORY, rustix::fs::Mode::empty())?, receipt.destination_name.clone())
    } else {
        anchored::parent(receipt.destination_directory.as_ref().ok_or_else(|| io::Error::other("destination root descriptor missing"))?.as_fd(), &record.source, &receipt.directories, true)?
    };
    if !anchored::unchanged(invocation.effective(expected.observed)?, roots::observe(parent.as_fd(), &name)?) { return Err(io::Error::other("destination identity or version changed before source retirement")); }
    Ok(())
}

fn restore(parent: rustix::fd::BorrowedFd<'_>, capture: &std::ffi::OsStr, name: &std::ffi::OsStr, identity: roots::Identity) -> io::Result<()> {
    anchored::identity(parent, capture, identity)?;
    crate::platform::rename_noreplace(parent, capture, parent, name)
}

fn clean_intent(parent: rustix::fd::BorrowedFd<'_>, name: &std::ffi::OsStr, identity: roots::Identity, primary: io::Error, path: &Path) -> io::Error {
    match remove_intent(parent, name, identity).and_then(|()| crate::platform::sync_directory(parent)) {
        Ok(()) => primary,
        Err(error) => io::Error::new(primary.kind(), format!("{primary}; intent cleanup incomplete at {path:?}: {error}")),
    }
}
