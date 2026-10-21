use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};

use crate::roots::{EntryKind, Identity, Observation, Requirement, Root, observe_fd};
use crate::{Collision, Operation, Operands, Options, Request, RECORD_MEMORY_BYTES};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    Transfer,
    Replace,
    Merge,
    Skip,
    SameEntry,
}

#[derive(Clone, Debug)]
pub(crate) struct PlannedOperand {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
    pub(crate) destination_name: OsString,
    pub(crate) source_parent: Identity,
    pub(crate) destination_parent: Identity,
    pub(crate) source_observed: Observation,
    pub(crate) destination_observed: Option<Observation>,
    pub(crate) action: Action,
}

#[derive(Debug)]
pub(crate) struct Preflight {
    pub(crate) operands: Vec<PlannedOperand>,
    pub(crate) into_directory: Option<OwnedFd>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct EntryKey {
    parent: Identity,
    name: OsString,
}

pub(crate) enum Target {
    Absent,
    SameEntry,
    Existing(Observation),
}

struct Mapping {
    source: PathBuf,
    destination: PathBuf,
    destination_name: Option<OsString>,
}

pub(crate) fn preflight(request: &Request, cwd: BorrowedFd<'_>) -> io::Result<Preflight> {
    let (mappings, into_directory) = mappings(request, cwd)?;
    let mut operands = Vec::with_capacity(mappings.len());
    let mut outputs = HashMap::with_capacity(mappings.len());
    let mut source_entries = HashMap::with_capacity(mappings.len());

    for mapping in mappings {
        let source = mapping.source;
        let destination = mapping.destination;
        let source_root = Root::establish(cwd, &source, Requirement::Existing)?;
        let source_observed = source_root.observed.ok_or_else(|| io::Error::other("source disappeared during preflight"))?;
        let destination_root = establish_destination(
            cwd,
            into_directory.as_ref(),
            &destination,
            mapping.destination_name.as_deref(),
            Requirement::MayBeAbsent,
        )?;
        let source_parent = source_root.parent_identity()?;
        let destination_parent = destination_root.parent_identity()?;
        let source_key = EntryKey { parent: source_parent, name: source_root.name.clone() };
        let destination_key = EntryKey { parent: destination_parent, name: destination_root.name.clone() };

        let index = operands.len();
        if source_entries.insert(source_key.clone(), index).is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "source operand is repeated"));
        }
        if outputs.insert(destination_key, index).is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "multiple sources map to the same output"));
        }

        operands.push(PlannedOperand {
            source,
            destination,
            destination_name: destination_root.name.clone(),
            source_parent,
            destination_parent,
            source_observed,
            destination_observed: destination_root.observed,
            action: Action::Transfer,
        });
    }

    if source_entries
        .iter()
        .any(|(source, source_index)| outputs.get(source).is_some_and(|destination_index| destination_index != source_index))
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "an output conflicts with another source operand"));
    }

    validate_ancestry(cwd, into_directory.as_ref(), &operands)?;
    validate_move_roots(request, cwd, &operands)?;

    for operand in &mut operands {
        let source = reestablish(cwd, &operand.source, Requirement::Existing, operand.source_parent, Some(operand.source_observed))?;
        let destination = reestablish_destination(cwd, into_directory.as_ref(), operand)?;
        let target = if source.same_entry(&destination)? {
            Target::SameEntry
        } else if let Some(observed) = operand.destination_observed {
            Target::Existing(observed)
        } else {
            Target::Absent
        };
        operand.action = decide(operand.source_observed, target, &request.options)?;
    }

    Ok(Preflight { operands, into_directory })
}

pub(crate) fn decide(source: Observation, target: Target, options: &Options) -> io::Result<Action> {
    let destination = match target {
        Target::Absent => return Ok(Action::Transfer),
        Target::SameEntry => return Ok(Action::SameEntry),
        Target::Existing(destination) => destination,
    };

    if options.merge
        && source.identity != destination.identity
        && source.identity.kind == EntryKind::Directory
        && destination.identity.kind == EntryKind::Directory
    {
        return Ok(Action::Merge);
    }

    if options.collision == Collision::Skip {
        return Ok(Action::Skip);
    }

    if source.identity == destination.identity {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "distinct names identify the same object"));
    }

    if options.collision == Collision::Replace
        && matches!(
            (source.identity.kind, destination.identity.kind),
            (EntryKind::RegularFile, EntryKind::RegularFile) | (EntryKind::Symlink, EntryKind::Symlink)
        )
    {
        return Ok(Action::Replace);
    }

    Err(io::Error::new(io::ErrorKind::AlreadyExists, "destination exists and the selected policy does not permit replacement"))
}

fn mappings(request: &Request, cwd: BorrowedFd<'_>) -> io::Result<(Vec<Mapping>, Option<OwnedFd>)> {
    match &request.operands {
        Operands::Exact { source, destination } => {
            let mut mappings = Vec::with_capacity(1);
            let mut memory_bytes = 0;
            push_mapping(&mut mappings, &mut memory_bytes, source.clone(), destination.clone(), None)?;
            Ok((mappings, None))
        }
        Operands::Into { directory, first, rest } => {
            let into_root = Root::establish(cwd, directory, Requirement::Existing)?;
            let into_directory = into_root.open_directory_following_final()?;
            let sources = std::iter::once(first).chain(rest.iter());
            let mut mappings = Vec::new();
            let mut memory_bytes = 0;
            for source in sources {
                let name = into_basename(source)?;
                let mut destination = directory.clone();
                destination.push(name);
                push_mapping(&mut mappings, &mut memory_bytes, source.clone(), destination, Some(name.to_os_string()))?;
            }
            Ok((mappings, Some(into_directory)))
        }
    }
}

fn push_mapping(
    mappings: &mut Vec<Mapping>,
    memory_bytes: &mut usize,
    source: PathBuf,
    destination: PathBuf,
    destination_name: Option<OsString>,
) -> io::Result<()> {
    let fixed_bytes = std::mem::size_of::<PlannedOperand>()
        .checked_add(std::mem::size_of::<Mapping>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<EntryKey>() * 2))
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<usize>() * 8))
        .ok_or_else(|| io::Error::other("preflight mapping budget overflow"))?;
    let source_bytes = source.as_os_str().as_bytes().len().checked_mul(2);
    let destination_bytes = destination.as_os_str().as_bytes().len().checked_mul(3);
    *memory_bytes = memory_bytes
        .checked_add(fixed_bytes)
        .and_then(|size| size.checked_add(source_bytes?))
        .and_then(|size| size.checked_add(destination_bytes?))
        .ok_or_else(|| io::Error::other("preflight mapping budget overflow"))?;
    if *memory_bytes > RECORD_MEMORY_BYTES {
        return Err(io::Error::other("preflight mappings exceed the operation record budget"));
    }
    mappings.push(Mapping { source, destination, destination_name });
    Ok(())
}

fn establish_destination(
    cwd: BorrowedFd<'_>,
    into_directory: Option<&OwnedFd>,
    path: &Path,
    name: Option<&OsStr>,
    requirement: Requirement,
) -> io::Result<Root> {
    match (into_directory, name) {
        (Some(parent), Some(name)) => Root::establish_at(parent.as_fd(), name, path, requirement),
        (None, None) => Root::establish(cwd, path, requirement),
        _ => Err(io::Error::other("mapping parent and destination name disagree")),
    }
}

fn into_basename(source: &Path) -> io::Result<&OsStr> {
    let bytes = source.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--into source has no unambiguous basename"));
    }
    let Some(end) = bytes.iter().rposition(|byte| *byte != b'/').map(|index| index + 1) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--into source has no unambiguous basename"));
    };
    let trimmed = &bytes[..end];
    let basename = trimmed.rsplit(|byte| *byte == b'/').next().unwrap_or_default();
    if basename.is_empty() || basename == b"." || basename == b".." {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--into source has no unambiguous basename"));
    }
    Ok(OsStr::from_bytes(basename))
}

fn validate_ancestry(cwd: BorrowedFd<'_>, into_directory: Option<&OwnedFd>, operands: &[PlannedOperand]) -> io::Result<()> {
    let mut source_directories: HashMap<Identity, Vec<usize>> = HashMap::new();
    let mut destination_directories: HashMap<Identity, Vec<usize>> = HashMap::new();

    for (index, operand) in operands.iter().enumerate() {
        if operand.source_observed.identity.kind == EntryKind::Directory {
            source_directories.entry(operand.source_observed.identity).or_default().push(index);
        }
        if let Some(destination) = operand.destination_observed
            && destination.identity.kind == EntryKind::Directory
        {
            destination_directories.entry(destination.identity).or_default().push(index);
        }
    }

    for (identity, source_indices) in &source_directories {
        let Some(destination_indices) = destination_directories.get(identity) else {
            continue;
        };
        for source_index in source_indices {
            for destination_index in destination_indices {
                if source_index != destination_index {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "source and destination identify the same directory"));
                }
                let operand = &operands[*source_index];
                let source = reestablish(cwd, &operand.source, Requirement::Existing, operand.source_parent, Some(operand.source_observed))?;
                let destination = reestablish_destination(cwd, into_directory, operand)?;
                if !source.same_entry(&destination)? {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "source and destination identify the same directory"));
                }
            }
        }
    }

    for (index, operand) in operands.iter().enumerate() {
        let source = reestablish(cwd, &operand.source, Requirement::Existing, operand.source_parent, Some(operand.source_observed))?;
        let destination = reestablish_destination(cwd, into_directory, operand)?;
        let source_key = entry_key(&source)?;
        let destination_key = entry_key(&destination)?;
        let same_mapping_entry = source_key == destination_key;

        if operand.source_observed.identity.kind == EntryKind::Directory
            && source_directories.get(&operand.source_observed.identity).is_some_and(|indices| indices.len() > 1)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "source directory operands overlap"));
        }

        if source.parent_chain_contains(|identity| {
            source_directories.get(&identity).is_some_and(|indices| indices.iter().any(|other| *other != index))
        })? {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "source operands overlap by directory ancestry"));
        }

        if source.parent_chain_contains(|identity| {
            destination_directories.get(&identity).is_some_and(|indices| {
                indices.iter().any(|other| *other != index || !same_mapping_entry)
            })
        })? {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "source and destination overlap by directory ancestry"));
        }

        if !same_mapping_entry
            && operand.source_observed.identity.kind == EntryKind::Directory
            && destination.observed.is_some_and(|observed| observed.identity == operand.source_observed.identity)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "source and destination identify the same directory"));
        }

        if destination.parent_chain_contains(|identity| {
            source_directories.get(&identity).is_some_and(|indices| {
                indices.iter().any(|other| *other != index || !same_mapping_entry)
            })
        })? {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "destination lies within a source directory"));
        }

        if let Some(destination) = destination.observed.filter(|observed| observed.identity.kind == EntryKind::Directory)
            && destination_directories
                .get(&destination.identity)
                .is_some_and(|indices| indices.iter().any(|other| *other != index))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "destination directory is mapped more than once"));
        }
    }

    for operand in operands {
        let source = reestablish(cwd, &operand.source, Requirement::Existing, operand.source_parent, Some(operand.source_observed))?;
        let destination = reestablish_destination(cwd, into_directory, operand)?;
        let source_key = entry_key(&source)?;
        let destination_key = entry_key(&destination)?;
        if source_key == destination_key {
            continue;
        }
        if source.is_ancestor_of(&destination)? || destination.is_ancestor_of(&source)? {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "source and destination overlap by directory ancestry"));
        }
    }

    Ok(())
}

fn validate_move_roots(request: &Request, cwd: BorrowedFd<'_>, operands: &[PlannedOperand]) -> io::Result<()> {
    if request.operation != Operation::Move {
        return Ok(());
    }
    let home = request
        .context
        .home
        .as_deref()
        .map(|path| Root::establish(cwd, path, Requirement::Existing)?.open_directory_following_final())
        .transpose()?;
    let home_identity = home.as_ref().map(|directory| observe_fd(directory.as_fd()).map(|observed| observed.identity)).transpose()?;
    for operand in operands {
        if operand.source_observed.identity.kind != EntryKind::Directory {
            continue;
        }
        let source = reestablish(cwd, &operand.source, Requirement::Existing, operand.source_parent, Some(operand.source_observed))?;
        if source.parent_identity()? == operand.destination_parent && source.name == operand.destination_name {
            continue;
        }
        let parent_identity = source.parent_identity()?;
        let is_mount_root = source.is_mount_root()?;
        let protects_cwd = source.is_ancestor_of_fd(cwd)?;
        let protects_home = home_identity.is_some_and(|identity| identity == operand.source_observed.identity);
        if operand.source_observed.identity == parent_identity
            || is_mount_root
            || protects_cwd
            || protects_home
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "move source is a protected filesystem or shell root"));
        }
    }

    Ok(())
}

fn reestablish(
    cwd: BorrowedFd<'_>,
    path: &Path,
    requirement: Requirement,
    expected_parent: Identity,
    expected_observed: Option<Observation>,
) -> io::Result<Root> {
    let root = Root::establish(cwd, path, requirement)?;
    if root.parent_identity()? != expected_parent || root.observed != expected_observed {
        return Err(io::Error::other("path identity changed during preflight"));
    }
    Ok(root)
}

fn entry_key(root: &Root) -> io::Result<EntryKey> {
    Ok(EntryKey { parent: root.parent_identity()?, name: root.name.clone() })
}

fn reestablish_destination(cwd: BorrowedFd<'_>, into_directory: Option<&OwnedFd>, operand: &PlannedOperand) -> io::Result<Root> {
    let root = establish_destination(
        cwd,
        into_directory,
        &operand.destination,
        into_directory.map(|_| operand.destination_name.as_os_str()),
        Requirement::MayBeAbsent,
    )?;
    if root.parent_identity()? != operand.destination_parent || root.observed != operand.destination_observed {
        return Err(io::Error::other("destination identity changed during preflight"));
    }
    Ok(root)
}
