use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};

use crate::platform::{self, MetadataTarget, Preservation};
use crate::policy::into_basename;
use crate::roots::{self, EntryKind, Identity, Requirement, Root};
use crate::{Cancellation, Completion, Context, Diagnostic, Event, EventSink, Jobs, TRANSFER_BUFFER_BYTES};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub enum SyncOperands {
    Exact { source: PathBuf, destination: PathBuf },
    Into { directory: PathBuf, sources: Vec<PathBuf> },
    Contents { source: PathBuf, directory: PathBuf },
}

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    pub delete: bool,
    pub dry_run: bool,
    pub jobs: Option<Jobs>,
}

#[derive(Clone, Debug)]
pub struct SyncRequest {
    pub operands: SyncOperands,
    pub options: SyncOptions,
    pub context: Context,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncProgress {
    pub examined_files: u64,
    pub transferred_files: u64,
    pub skipped_files: u64,
    pub deleted_files: u64,
    pub bytes_transferred: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncOutcome {
    pub progress: SyncProgress,
    pub completion: Completion,
}

impl SyncOutcome {
    pub const fn status(&self) -> i32 {
        match self.completion {
            Completion::Complete => 0,
            Completion::Failed => 1,
            Completion::InvalidRequest => 2,
            Completion::Cancelled => 130,
        }
    }
}

pub fn owner_lacks_directory_write(dir: BorrowedFd<'_>) -> io::Result<bool> {
    let stat = rustix::fs::fstat(dir)?;
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Ok(false);
    }
    Ok(stat.st_mode & 0o200 == 0)
}

pub fn execute(
    request: &SyncRequest,
    cancellation: &Cancellation,
    sink: &dyn EventSink,
) -> SyncOutcome {
    let mut session = SyncSession {
        request,
        cancellation,
        sink,
        progress: SyncProgress::default(),
        seen_hardlinks: HashSet::new(),
        buffer: vec![0u8; TRANSFER_BUFFER_BYTES],
        failed: false,
    };

    let cwd = match rustix::fs::open(
        &request.context.cwd,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) => {
            let _ = sink.emit(Event::Diagnostic(Diagnostic {
                path: request.context.cwd.clone(),
                message: format!("cannot open working directory: {error}"),
            }));
            return SyncOutcome {
                progress: session.progress,
                completion: Completion::Failed,
            };
        }
    };
    let cwd = cwd.as_fd();

    match &request.operands {
        SyncOperands::Into { directory, sources } => {
            let target_dir = resolve_path(&request.context.cwd, directory);
            let target_meta = match fs::symlink_metadata(&target_dir) {
                Ok(m) => m,
                Err(error) => {
                    let _ = sink.emit(Event::Diagnostic(Diagnostic {
                        path: directory.clone(),
                        message: format!("cannot access target directory: {error}"),
                    }));
                    return SyncOutcome {
                        progress: session.progress,
                        completion: Completion::Failed,
                    };
                }
            };
            if !target_meta.is_dir() {
                let _ = sink.emit(Event::Diagnostic(Diagnostic {
                    path: directory.clone(),
                    message: "target is not a directory".to_owned(),
                }));
                return SyncOutcome {
                    progress: session.progress,
                    completion: Completion::InvalidRequest,
                };
            }
            let mut mappings = Vec::with_capacity(sources.len());
            for source in sources {
                if cancellation.is_cancelled() {
                    return SyncOutcome {
                        progress: session.progress,
                        completion: Completion::Cancelled,
                    };
                }
                let basename = match into_basename(source) {
                    Ok(name) => name,
                    Err(error) => {
                        let _ = sink.emit(Event::Diagnostic(Diagnostic {
                            path: source.clone(),
                            message: format!("invalid source basename: {error}"),
                        }));
                        session.failed = true;
                        continue;
                    }
                };
                let src_path = resolve_path(&request.context.cwd, source);
                mappings.push(Mapping {
                    source: src_path,
                    destination: target_dir.join(basename),
                });
            }
            if let Err(diagnostic) = validate_mappings(cwd, &mappings) {
                let _ = sink.emit(Event::Diagnostic(diagnostic));
                return SyncOutcome {
                    progress: session.progress,
                    completion: Completion::InvalidRequest,
                };
            }
            for mapping in mappings {
                if cancellation.is_cancelled() {
                    return SyncOutcome {
                        progress: session.progress,
                        completion: Completion::Cancelled,
                    };
                }
                session.sync_pair(&mapping.source, &mapping.destination, false);
            }
        }
        SyncOperands::Contents { source, directory } => {
            let src_path = resolve_path(&request.context.cwd, source);
            let dst_path = resolve_path(&request.context.cwd, directory);
            match fs::symlink_metadata(&src_path) {
                Ok(m) if !m.is_dir() => {
                    let _ = sink.emit(Event::Diagnostic(Diagnostic {
                        path: source.clone(),
                        message: "--contents source must be a directory".to_owned(),
                    }));
                    return SyncOutcome {
                        progress: session.progress,
                        completion: Completion::InvalidRequest,
                    };
                }
                Err(error) => {
                    let _ = sink.emit(Event::Diagnostic(Diagnostic {
                        path: source.clone(),
                        message: format!("cannot access source directory: {error}"),
                    }));
                    return SyncOutcome {
                        progress: session.progress,
                        completion: Completion::Failed,
                    };
                }
                _ => {}
            }
            match fs::symlink_metadata(&dst_path) {
                Ok(m) if !m.is_dir() => {
                    let _ = sink.emit(Event::Diagnostic(Diagnostic {
                        path: directory.clone(),
                        message: "target is not a directory".to_owned(),
                    }));
                    return SyncOutcome {
                        progress: session.progress,
                        completion: Completion::InvalidRequest,
                    };
                }
                Err(error) => {
                    let _ = sink.emit(Event::Diagnostic(Diagnostic {
                        path: directory.clone(),
                        message: format!("cannot access target directory: {error}"),
                    }));
                    return SyncOutcome {
                        progress: session.progress,
                        completion: Completion::Failed,
                    };
                }
                _ => {}
            }
            let mapping = Mapping {
                source: src_path,
                destination: dst_path,
            };
            if let Err(diagnostic) = validate_mappings(cwd, std::slice::from_ref(&mapping)) {
                let _ = sink.emit(Event::Diagnostic(diagnostic));
                return SyncOutcome {
                    progress: session.progress,
                    completion: Completion::InvalidRequest,
                };
            }
            session.sync_pair(&mapping.source, &mapping.destination, true);
        }
        SyncOperands::Exact { source, destination } => {
            let mapping = Mapping {
                source: resolve_path(&request.context.cwd, source),
                destination: resolve_path(&request.context.cwd, destination),
            };
            if let Err(diagnostic) = validate_mappings(cwd, std::slice::from_ref(&mapping)) {
                let _ = sink.emit(Event::Diagnostic(diagnostic));
                return SyncOutcome {
                    progress: session.progress,
                    completion: Completion::InvalidRequest,
                };
            }
            session.sync_pair(&mapping.source, &mapping.destination, false);
        }
    }

    let completion = if cancellation.is_cancelled() {
        Completion::Cancelled
    } else if session.failed {
        Completion::Failed
    } else {
        Completion::Complete
    };

    SyncOutcome {
        progress: session.progress,
        completion,
    }
}

struct Mapping {
    source: PathBuf,
    destination: PathBuf,
}

struct ValidatedMapping {
    source_path: PathBuf,
    destination_path: PathBuf,
    source: Root,
    destination: Option<Root>,
}

fn validate_mappings(cwd: BorrowedFd<'_>, mappings: &[Mapping]) -> Result<(), Diagnostic> {
    let mut validated = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let Ok(source) = Root::establish(cwd, &mapping.source, Requirement::Existing) else {
            continue;
        };
        let destination = match Root::establish(cwd, &mapping.destination, Requirement::MayBeAbsent) {
            Ok(destination) => Some(destination),
            Err(_) => {
                if let Some(ancestor) = nearest_existing_directory(cwd, &mapping.destination)
                    && source
                        .is_ancestor_of(&ancestor)
                        .map_err(|error| validation_error(&mapping.source, error))?
                {
                    return Err(overlap_error(&mapping.source));
                }
                None
            }
        };
        if let Some(ref destination) = destination
            && (source
                .same_entry(destination)
                .map_err(|error| validation_error(&mapping.source, error))?
                || source
                    .is_ancestor_of(destination)
                    .map_err(|error| validation_error(&mapping.source, error))?
                || destination
                    .is_ancestor_of(&source)
                    .map_err(|error| validation_error(&mapping.destination, error))?)
        {
            return Err(overlap_error(&mapping.source));
        }
        validated.push(ValidatedMapping {
            source_path: mapping.source.clone(),
            destination_path: mapping.destination.clone(),
            source,
            destination,
        });
    }

    for (index, mapping) in validated.iter().enumerate() {
        for (other_index, other) in validated.iter().enumerate() {
            if index == other_index {
                continue;
            }
            if mapping
                .source
                .same_entry(&other.source)
                .map_err(|error| validation_error(&mapping.source_path, error))?
            {
                return Err(mapping_error(
                    &mapping.source_path,
                    "source operand is repeated",
                ));
            }
            if mapping
                .source
                .is_ancestor_of(&other.source)
                .map_err(|error| validation_error(&mapping.source_path, error))?
            {
                return Err(mapping_error(
                    &mapping.source_path,
                    "source operands overlap by directory ancestry",
                ));
            }
            if let Some(ref destination) = other.destination {
                if mapping
                    .source
                    .same_entry(destination)
                    .map_err(|error| validation_error(&mapping.source_path, error))?
                {
                    return Err(mapping_error(
                        &mapping.source_path,
                        "an output conflicts with another source operand",
                    ));
                }
                if mapping
                    .source
                    .is_ancestor_of(destination)
                    .map_err(|error| validation_error(&mapping.source_path, error))?
                {
                    return Err(overlap_error(&mapping.source_path));
                }
            }
            if let Some(ref destination) = mapping.destination {
                if destination
                    .is_ancestor_of(&other.source)
                    .map_err(|error| validation_error(&mapping.destination_path, error))?
                {
                    return Err(mapping_error(
                        &mapping.destination_path,
                        "destination lies within a source directory",
                    ));
                }
                if let Some(ref other_destination) = other.destination {
                    if destination
                        .same_entry(other_destination)
                        .map_err(|error| validation_error(&mapping.destination_path, error))?
                    {
                        return Err(mapping_error(
                            &mapping.destination_path,
                            "multiple sources map to the same output",
                        ));
                    }
                    if destination
                        .is_ancestor_of(other_destination)
                        .map_err(|error| validation_error(&mapping.destination_path, error))?
                    {
                        return Err(mapping_error(
                            &mapping.destination_path,
                            "destination directories overlap by directory ancestry",
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

fn nearest_existing_directory(cwd: BorrowedFd<'_>, path: &Path) -> Option<Root> {
    let mut current = path.to_path_buf();
    loop {
        if let Ok(root) = Root::establish(cwd, &current, Requirement::Existing)
            && root
                .observed
                .is_some_and(|observed| observed.identity.kind == EntryKind::Directory)
        {
            return Some(root);
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

fn validation_error(path: &Path, error: io::Error) -> Diagnostic {
    Diagnostic {
        path: path.to_path_buf(),
        message: format!("cannot validate sync mapping: {error}"),
    }
}

fn overlap_error(path: &Path) -> Diagnostic {
    mapping_error(path, "source and destination overlap by directory ancestry")
}

fn mapping_error(path: &Path, message: &str) -> Diagnostic {
    Diagnostic {
        path: path.to_path_buf(),
        message: message.to_owned(),
    }
}

struct SyncSession<'a> {
    request: &'a SyncRequest,
    cancellation: &'a Cancellation,
    sink: &'a dyn EventSink,
    progress: SyncProgress,
    seen_hardlinks: HashSet<Identity>,
    buffer: Vec<u8>,
    failed: bool,
}

impl SyncSession<'_> {
    fn sync_pair(&mut self, src_path: &Path, dst_path: &Path, contents_mode: bool) {
        if self.cancellation.is_cancelled() {
            return;
        }

        let src_meta = match fs::symlink_metadata(src_path) {
            Ok(m) => m,
            Err(error) => {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: src_path.to_path_buf(),
                    message: format!("cannot access source: {error}"),
                }));
                self.failed = true;
                return;
            }
        };

        if src_meta.is_symlink() {
            self.sync_symlink(src_path, dst_path);
        } else if src_meta.is_dir() {
            self.sync_directory(src_path, dst_path, contents_mode);
        } else if src_meta.is_file() {
            self.sync_file(src_path, dst_path, &src_meta);
        } else {
            let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                path: src_path.to_path_buf(),
                message: "unsupported special file type".to_owned(),
            }));
            self.failed = true;
        }
    }

    fn sync_symlink(&mut self, src_path: &Path, dst_path: &Path) {
        self.progress.examined_files += 1;
        let target = match fs::read_link(src_path) {
            Ok(t) => t,
            Err(error) => {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: src_path.to_path_buf(),
                    message: format!("cannot read symlink target: {error}"),
                }));
                self.failed = true;
                return;
            }
        };

        if let Ok(dst_meta) = fs::symlink_metadata(dst_path) {
            if dst_meta.is_symlink()
                && let Ok(existing_target) = fs::read_link(dst_path)
                && existing_target == target
            {
                self.progress.skipped_files += 1;
                let _ = self.sink.emit(Event::Progress(self.progress_event()));
                return;
            }
            if !self.request.options.dry_run {
                if dst_meta.is_dir() {
                    let _ = fs::remove_dir_all(dst_path);
                } else {
                    let _ = fs::remove_file(dst_path);
                }
            }
        }

        self.progress.transferred_files += 1;
        let _ = self.sink.emit(Event::Progress(self.progress_event()));

        if !self.request.options.dry_run {
            if let Some(parent) = dst_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(error) = std::os::unix::fs::symlink(&target, dst_path) {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: dst_path.to_path_buf(),
                    message: format!("failed to create symlink: {error}"),
                }));
                self.failed = true;
            }
        }
    }

    fn sync_file(&mut self, src_path: &Path, dst_path: &Path, src_meta: &fs::Metadata) {
        self.progress.examined_files += 1;

        if src_meta.nlink() > 1 {
            let identity = Identity {
                file: fdu_posix::identity::FileIdentity {
                    device: src_meta.dev(),
                    inode: src_meta.ino(),
                },
                kind: EntryKind::RegularFile,
            };
            if !self.seen_hardlinks.insert(identity) {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: src_path.to_path_buf(),
                    message: format!(
                        "hardlink group detected for {}; topology preservation is not supported in v1 without --split-hardlinks",
                        src_path.display()
                    ),
                }));
                self.failed = true;
                return;
            }
        }

        let dst_meta = fs::symlink_metadata(dst_path).ok();
        if let Some(ref dst_meta) = dst_meta
            && dst_meta.is_file()
            && dst_meta.len() == src_meta.len()
            && dst_meta.mtime() == src_meta.mtime()
            && dst_meta.mtime_nsec() == src_meta.mtime_nsec()
        {
            self.progress.skipped_files += 1;
            let _ = self.sink.emit(Event::Progress(self.progress_event()));
            return;
        }

        let size = src_meta.len();
        self.progress.transferred_files += 1;
        self.progress.bytes_transferred += size;
        let _ = self.sink.emit(Event::Progress(self.progress_event()));

        if self.request.options.dry_run {
            return;
        }

        let parent = dst_path.parent().unwrap_or_else(|| Path::new("."));
        let dst_file_name = match dst_path.file_name() {
            Some(name) => name,
            None => {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: dst_path.to_path_buf(),
                    message: "missing destination file name".to_owned(),
                }));
                self.failed = true;
                return;
            }
        };

        let parent_fd = match rustix::fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(_) => {
                let _ = fs::create_dir_all(parent);
                match rustix::fs::open(
                    parent,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(fd) => fd,
                    Err(error) => {
                        let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                            path: parent.to_path_buf(),
                            message: format!("cannot open destination directory: {error}"),
                        }));
                        self.failed = true;
                        return;
                    }
                }
            }
        };

        // Linux lets root create entries even when the owner write bit is clear.
        match owner_lacks_directory_write(parent_fd.as_fd()) {
            Ok(true) => {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: parent.to_path_buf(),
                    message: "destination directory does not grant its owner write permission".to_owned(),
                }));
                self.failed = true;
                return;
            }
            Ok(false) => {}
            Err(error) => {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: parent.to_path_buf(),
                    message: format!("cannot check destination directory permissions: {error}"),
                }));
                self.failed = true;
                return;
            }
        }

        let src_parent = src_path.parent().unwrap_or_else(|| Path::new("."));
        let src_file_name = match src_path.file_name() {
            Some(name) => name,
            None => {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: src_path.to_path_buf(),
                    message: "missing source file name".to_owned(),
                }));
                self.failed = true;
                return;
            }
        };

        let src_parent_fd = match rustix::fs::open(
            src_parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(error) => {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: src_parent.to_path_buf(),
                    message: format!("cannot open source parent directory: {error}"),
                }));
                self.failed = true;
                return;
            }
        };

        let is_replacement = dst_meta.is_some();
        let copy_result = if is_replacement {
            self.copy_file_staged_at(
                src_parent_fd.as_fd(),
                src_file_name,
                parent_fd.as_fd(),
                dst_file_name,
                size,
            )
        } else {
            self.copy_file_direct_at(
                src_parent_fd.as_fd(),
                src_file_name,
                parent_fd.as_fd(),
                dst_file_name,
                size,
            )
        };

        if let Err(error) = copy_result {
            let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                path: dst_path.to_path_buf(),
                message: format!("failed to copy file: {error}"),
            }));
            self.failed = true;
        }
    }

    fn direct_copy_static(
        src_parent_fd: BorrowedFd<'_>,
        src_name: &OsStr,
        dst_parent_fd: BorrowedFd<'_>,
        dst_name: &OsStr,
        length: u64,
        buffer: &mut [u8],
        cancellation: &Cancellation,
    ) -> io::Result<()> {
        let src_file = rustix::fs::openat(
            src_parent_fd,
            src_name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        let src_obs = roots::observe_fd(src_file.as_fd())?;
        let src_ext = platform::capture_metadata(MetadataTarget::Fd(src_file.as_fd()))?;

        #[cfg(target_os = "macos")]
        {
            if rustix::fs::fclonefileat(
                src_file.as_fd(),
                dst_parent_fd,
                dst_name,
                rustix::fs::CloneFlags::NOOWNERCOPY,
            )
            .is_ok()
                && let Ok(cloned_fd) = rustix::fs::openat(
                    dst_parent_fd,
                    dst_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
            {
                let _ = platform::apply_metadata(
                    MetadataTarget::Fd(cloned_fd.as_fd()),
                    &src_obs,
                    &src_ext,
                    Preservation::Copy,
                );
                return Ok(());
            }
        }

        let dst_fd = rustix::fs::openat(
            dst_parent_fd,
            dst_name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;

        let copy_result = (|| -> io::Result<()> {
            platform::copy_data(
                src_file.as_fd(),
                dst_fd.as_fd(),
                length,
                buffer,
                cancellation,
            )?;
            platform::apply_metadata(
                MetadataTarget::Fd(dst_fd.as_fd()),
                &src_obs,
                &src_ext,
                Preservation::Copy,
            )?;
            Ok(())
        })();

        if let Err(error) = copy_result {
            let _ = rustix::fs::unlinkat(dst_parent_fd, dst_name, AtFlags::empty());
            return Err(error);
        }

        Ok(())
    }

    fn staged_copy_static(
        src_parent_fd: BorrowedFd<'_>,
        src_name: &OsStr,
        dst_parent_fd: BorrowedFd<'_>,
        dst_name: &OsStr,
        length: u64,
        buffer: &mut [u8],
        cancellation: &Cancellation,
    ) -> io::Result<()> {
        let temp_name = format!(
            ".ish-sync-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        );

        let src_file = rustix::fs::openat(
            src_parent_fd,
            src_name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        let src_obs = roots::observe_fd(src_file.as_fd())?;
        let src_ext = platform::capture_metadata(MetadataTarget::Fd(src_file.as_fd()))?;

        #[cfg(target_os = "macos")]
        let mut cloned = false;
        #[cfg(target_os = "macos")]
        {
            if rustix::fs::fclonefileat(
                src_file.as_fd(),
                dst_parent_fd,
                &temp_name,
                rustix::fs::CloneFlags::NOOWNERCOPY,
            )
            .is_ok()
                && let Ok(cloned_fd) = rustix::fs::openat(
                    dst_parent_fd,
                    &temp_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
            {
                let _ = platform::apply_metadata(
                    MetadataTarget::Fd(cloned_fd.as_fd()),
                    &src_obs,
                    &src_ext,
                    Preservation::Copy,
                );
                cloned = true;
            }
        }

        #[cfg(target_os = "macos")]
        let copy_result: io::Result<()> = if cloned {
            Ok(())
        } else {
            let temp_fd = rustix::fs::openat(
                dst_parent_fd,
                &temp_name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?;
            let res = (|| -> io::Result<()> {
                platform::copy_data(
                    src_file.as_fd(),
                    temp_fd.as_fd(),
                    length,
                    buffer,
                    cancellation,
                )?;
                platform::apply_metadata(
                    MetadataTarget::Fd(temp_fd.as_fd()),
                    &src_obs,
                    &src_ext,
                    Preservation::Copy,
                )?;
                Ok(())
            })();
            drop(temp_fd);
            res
        };

        #[cfg(not(target_os = "macos"))]
        let copy_result: io::Result<()> = {
            let temp_fd = rustix::fs::openat(
                dst_parent_fd,
                &temp_name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?;
            let res = (|| -> io::Result<()> {
                platform::copy_data(
                    src_file.as_fd(),
                    temp_fd.as_fd(),
                    length,
                    buffer,
                    cancellation,
                )?;
                platform::apply_metadata(
                    MetadataTarget::Fd(temp_fd.as_fd()),
                    &src_obs,
                    &src_ext,
                    Preservation::Copy,
                )?;
                Ok(())
            })();
            drop(temp_fd);
            res
        };

        drop(src_file);

        if let Err(error) = copy_result {
            let _ = rustix::fs::unlinkat(dst_parent_fd, &temp_name, AtFlags::empty());
            return Err(error);
        }

        let rename_result = rustix::fs::renameat(
            dst_parent_fd,
            &temp_name,
            dst_parent_fd,
            dst_name,
        );

        if let Err(error) = rename_result {
            let _ = rustix::fs::unlinkat(dst_parent_fd, &temp_name, AtFlags::empty());
            return Err(error.into());
        }

        Ok(())
    }

    fn copy_file_direct_at(
        &mut self,
        src_parent_fd: BorrowedFd<'_>,
        src_name: &OsStr,
        dst_parent_fd: BorrowedFd<'_>,
        dst_name: &OsStr,
        length: u64,
    ) -> io::Result<()> {
        Self::direct_copy_static(
            src_parent_fd,
            src_name,
            dst_parent_fd,
            dst_name,
            length,
            &mut self.buffer,
            self.cancellation,
        )
    }

    fn copy_file_staged_at(
        &mut self,
        src_parent_fd: BorrowedFd<'_>,
        src_name: &OsStr,
        dst_parent_fd: BorrowedFd<'_>,
        dst_name: &OsStr,
        length: u64,
    ) -> io::Result<()> {
        Self::staged_copy_static(
            src_parent_fd,
            src_name,
            dst_parent_fd,
            dst_name,
            length,
            &mut self.buffer,
            self.cancellation,
        )
    }

    fn sync_directory(&mut self, src_dir: &Path, dst_dir: &Path, contents_mode: bool) {
        if self.cancellation.is_cancelled() {
            return;
        }

        let target_dir = dst_dir.to_path_buf();

        let initial_transferred = self.progress.transferred_files;
        let initial_deleted = self.progress.deleted_files;
        let target_meta = fs::symlink_metadata(&target_dir).ok();
        let target_dir_existed = target_meta.is_some();
        let target_was_symlink = target_meta.as_ref().is_some_and(|meta| meta.is_symlink());
        let mut target_is_dir = target_meta.as_ref().is_some_and(|meta| meta.is_dir());

        if !contents_mode {
            self.progress.examined_files += 1;
            if !target_is_dir
                && !self.request.options.dry_run
                && (target_meta.is_none() || target_was_symlink)
            {
                if target_was_symlink
                    && let Err(error) = fs::remove_file(&target_dir)
                {
                    let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                        path: target_dir.clone(),
                        message: format!("cannot replace destination symlink: {error}"),
                    }));
                    self.failed = true;
                    return;
                }
                if let Err(error) = fs::create_dir_all(&target_dir) {
                    let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                        path: target_dir.clone(),
                        message: format!("cannot create directory: {error}"),
                    }));
                    self.failed = true;
                    return;
                }
                if let Ok(src_meta) = fs::symlink_metadata(src_dir) {
                    let _ = fs::set_permissions(
                        &target_dir,
                        fs::Permissions::from_mode(src_meta.mode()),
                    );
                }
                target_is_dir = true;
            }
        }

        let src_entries = match fs::read_dir(src_dir) {
            Ok(entries) => entries,
            Err(error) => {
                let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                    path: src_dir.to_path_buf(),
                    message: format!("cannot read source directory: {error}"),
                }));
                self.failed = true;
                return;
            }
        };

        let src_dir_fd = rustix::fs::open(
            src_dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .ok();

        let dst_dir_fd = if target_is_dir {
            rustix::fs::open(
                &target_dir,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .ok()
        } else {
            None
        };

        let mut child_entries = Vec::new();

        for entry in src_entries {
            let entry = match entry {
                Ok(e) => e,
                Err(error) => {
                    let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                        path: src_dir.to_path_buf(),
                        message: format!("cannot read directory entry: {error}"),
                    }));
                    self.failed = true;
                    continue;
                }
            };
            child_entries.push(entry.file_name());
        }
        child_entries.sort();

        if self.request.options.delete && target_is_dir {
            match fs::read_dir(&target_dir) {
                Ok(dst_entries) => {
                    let mut extraneous = Vec::new();
                    for entry in dst_entries.flatten() {
                        let name = entry.file_name();
                        if child_entries.binary_search(&name).is_err() {
                            extraneous.push(entry.path());
                        }
                    }
                    extraneous.sort();
                    for path in extraneous {
                        if self.cancellation.is_cancelled() {
                            return;
                        }
                        self.delete_entry(&path);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    self.delete_failed(&target_dir, &error);
                }
            }
        }

        let mut files_to_transfer = Vec::new();

        for name in child_entries {
            if self.cancellation.is_cancelled() {
                return;
            }

            let (is_file, size, nlink, dev, ino, mtime, mtime_nsec) = if let Some(ref fd) = src_dir_fd {
                match rustix::fs::statat(fd.as_fd(), &name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(st) => {
                        let is_file = FileType::from_raw_mode(st.st_mode) == FileType::RegularFile;
                        #[allow(clippy::unnecessary_cast)]
                        (
                            is_file,
                            st.st_size as u64,
                            st.st_nlink as u64,
                            st.st_dev as u64,
                            st.st_ino as u64,
                            st.st_mtime as i64,
                            st.st_mtime_nsec as i64,
                        )
                    }
                    Err(error) => {
                        let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                            path: src_dir.join(&name),
                            message: format!("cannot access source: {error}"),
                        }));
                        self.failed = true;
                        continue;
                    }
                }
            } else {
                let child_src = src_dir.join(&name);
                match fs::symlink_metadata(&child_src) {
                    Ok(m) => (
                        m.is_file(),
                        m.len(),
                        m.nlink(),
                        m.dev(),
                        m.ino(),
                        m.mtime(),
                        m.mtime_nsec(),
                    ),
                    Err(error) => {
                        let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                            path: child_src,
                            message: format!("cannot access source: {error}"),
                        }));
                        self.failed = true;
                        continue;
                    }
                }
            };

            if is_file && src_dir_fd.is_some() && dst_dir_fd.is_some() && !self.request.options.dry_run {
                self.progress.examined_files += 1;

                if nlink > 1 {
                    let identity = Identity {
                        file: fdu_posix::identity::FileIdentity {
                            device: dev,
                            inode: ino,
                        },
                        kind: EntryKind::RegularFile,
                    };
                    if !self.seen_hardlinks.insert(identity) {
                        let path = src_dir.join(&name);
                        let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                            path: path.clone(),
                            message: format!(
                                "hardlink group detected for {}; topology preservation is not supported in v1 without --split-hardlinks",
                                path.display()
                            ),
                        }));
                        self.failed = true;
                        continue;
                    }
                }

                let dst_stat = dst_dir_fd.as_ref().and_then(|fd| {
                    rustix::fs::statat(fd.as_fd(), &name, AtFlags::SYMLINK_NOFOLLOW).ok()
                });

                #[allow(clippy::unnecessary_cast)]
                let skipped = if let Some(ref dst_st) = dst_stat
                    && FileType::from_raw_mode(dst_st.st_mode) == FileType::RegularFile
                    && dst_st.st_size as u64 == size
                    && dst_st.st_mtime as i64 == mtime
                    && dst_st.st_mtime_nsec as i64 == mtime_nsec
                {
                    self.progress.skipped_files += 1;
                    let _ = self.sink.emit(Event::Progress(self.progress_event()));
                    true
                } else {
                    false
                };

                if skipped {
                    continue;
                }

                files_to_transfer.push((name, size, dst_stat.is_some()));
            } else {
                self.sync_pair(&src_dir.join(&name), &target_dir.join(&name), false);
            }
        }

        if !files_to_transfer.is_empty() {
            let jobs = self.request.options.jobs.map_or(4, Jobs::get).min(files_to_transfer.len());
            if jobs <= 1 || files_to_transfer.len() < 4 {
                for (name, size, is_replacement) in files_to_transfer {
                    if self.cancellation.is_cancelled() {
                        return;
                    }
                    self.progress.transferred_files += 1;
                    self.progress.bytes_transferred += size;
                    let _ = self.sink.emit(Event::Progress(self.progress_event()));

                    let src_fd_ref = src_dir_fd.as_ref().unwrap().as_fd();
                    let dst_fd_ref = dst_dir_fd.as_ref().unwrap().as_fd();

                    let copy_result = if is_replacement {
                        self.copy_file_staged_at(
                            src_fd_ref,
                            &name,
                            dst_fd_ref,
                            &name,
                            size,
                        )
                    } else {
                        self.copy_file_direct_at(
                            src_fd_ref,
                            &name,
                            dst_fd_ref,
                            &name,
                            size,
                        )
                    };

                    if let Err(error) = copy_result {
                        let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                            path: target_dir.join(&name),
                            message: format!("failed to copy file: {error}"),
                        }));
                        self.failed = true;
                    }
                }
            } else {
                use std::sync::Mutex;
                let queue = Mutex::new(files_to_transfer);
                let first_error = Mutex::new(None::<(PathBuf, io::Error)>);
                let total_transferred = std::sync::atomic::AtomicU64::new(0);
                let total_bytes = std::sync::atomic::AtomicU64::new(0);

                let src_fd_ref = src_dir_fd.as_ref().unwrap().as_fd();
                let dst_fd_ref = dst_dir_fd.as_ref().unwrap().as_fd();
                let target_dir_ref = &target_dir;

                std::thread::scope(|scope| {
                    for _ in 0..jobs {
                        let queue = &queue;
                        let first_error = &first_error;
                        let total_transferred = &total_transferred;
                        let total_bytes = &total_bytes;
                        let cancellation = self.cancellation;

                        scope.spawn(move || {
                            let mut buffer = vec![0u8; TRANSFER_BUFFER_BYTES];
                            loop {
                                if cancellation.is_cancelled() {
                                    break;
                                }
                                let item = {
                                    let mut q = queue.lock().unwrap();
                                    q.pop()
                                };
                                let Some((name, size, is_replacement)) = item else {
                                    break;
                                };

                                let res = if is_replacement {
                                    Self::staged_copy_static(
                                        src_fd_ref,
                                        &name,
                                        dst_fd_ref,
                                        &name,
                                        size,
                                        &mut buffer,
                                        cancellation,
                                    )
                                } else {
                                    Self::direct_copy_static(
                                        src_fd_ref,
                                        &name,
                                        dst_fd_ref,
                                        &name,
                                        size,
                                        &mut buffer,
                                        cancellation,
                                    )
                                };

                                match res {
                                    Ok(()) => {
                                        total_transferred.fetch_add(1, Ordering::Relaxed);
                                        total_bytes.fetch_add(size, Ordering::Relaxed);
                                    }
                                    Err(err) => {
                                        let mut e = first_error.lock().unwrap();
                                        if e.is_none() {
                                            *e = Some((target_dir_ref.join(&name), err));
                                        }
                                    }
                                }
                            }
                        });
                    }
                });

                let transferred = total_transferred.load(Ordering::Relaxed);
                let bytes = total_bytes.load(Ordering::Relaxed);
                self.progress.transferred_files += transferred;
                self.progress.bytes_transferred += bytes;
                let _ = self.sink.emit(Event::Progress(self.progress_event()));

                if let Some((path, err)) = first_error.into_inner().unwrap() {
                    let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                        path,
                        message: format!("failed to copy file: {err}"),
                    }));
                    self.failed = true;
                }
            }
        }

        let children_modified = self.progress.transferred_files > initial_transferred
            || self.progress.deleted_files > initial_deleted;

        if !self.request.options.dry_run
            && (!target_dir_existed || children_modified)
            && target_dir.exists()
            && let Some(ref src_fd) = src_dir_fd
            && let Some(ref dst_fd) = dst_dir_fd
            && let Ok(obs) = roots::observe_fd(src_fd.as_fd())
            && let Ok(ext) = platform::capture_metadata(MetadataTarget::Fd(src_fd.as_fd()))
        {
            let _ = platform::apply_metadata(
                MetadataTarget::Fd(dst_fd.as_fd()),
                &obs,
                &ext,
                Preservation::Copy,
            );
        }
    }

    fn delete_entry(&mut self, path: &Path) {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.delete_failed(path, &error);
                return;
            }
        };

        if meta.is_dir() {
            let entries = match fs::read_dir(path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                Err(error) => {
                    self.delete_failed(path, &error);
                    return;
                }
            };
            for entry in entries {
                if self.cancellation.is_cancelled() {
                    return;
                }
                match entry {
                    Ok(entry) => self.delete_entry(&entry.path()),
                    Err(error) => self.delete_failed(path, &error),
                }
            }
            if self.request.options.dry_run {
                self.progress.deleted_files += 1;
                let _ = self.sink.emit(Event::Progress(self.progress_event()));
                return;
            }
            match fs::remove_dir(path) {
                Ok(()) => {
                    self.progress.deleted_files += 1;
                    let _ = self.sink.emit(Event::Progress(self.progress_event()));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => self.delete_failed(path, &error),
            }
        } else {
            if self.request.options.dry_run {
                self.progress.deleted_files += 1;
                let _ = self.sink.emit(Event::Progress(self.progress_event()));
                return;
            }
            match fs::remove_file(path) {
                Ok(()) => {
                    self.progress.deleted_files += 1;
                    let _ = self.sink.emit(Event::Progress(self.progress_event()));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => self.delete_failed(path, &error),
            }
        }
    }

    fn delete_failed(&mut self, path: &Path, error: &io::Error) {
        let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
            path: path.to_path_buf(),
            message: format!("cannot delete: {error}"),
        }));
        self.failed = true;
    }

    fn progress_event(&self) -> crate::Progress {
        crate::Progress {
            discovered_files: self.progress.examined_files,
            completed_files: self.progress.transferred_files,
            skipped_files: self.progress.skipped_files,
            failed_files: if self.failed { 1 } else { 0 },
            logical_bytes_cloned: 0,
            bytes_streamed: self.progress.bytes_transferred,
            cloned_files: 0,
            native_copied_files: 0,
            buffered_copied_files: self.progress.transferred_files,
        }
    }
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    let raw = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    normalize_trailing_slashes(&raw)
}

fn normalize_trailing_slashes(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() <= 1 || !bytes.ends_with(b"/") {
        return path.to_path_buf();
    }
    let end = bytes.iter().rposition(|b| *b != b'/').map(|i| i + 1).unwrap_or(1);
    PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end]))
}
