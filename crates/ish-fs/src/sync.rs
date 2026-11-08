use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{AtFlags, Mode, OFlags};

use crate::platform::{self, MetadataTarget, Preservation};
use crate::policy::into_basename;
use crate::roots::{self, EntryKind, Identity};
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
                let dst_path = target_dir.join(basename);
                session.sync_pair(&src_path, &dst_path, false);
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
            session.sync_pair(&src_path, &dst_path, true);
        }
        SyncOperands::Exact { source, destination } => {
            let src_path = resolve_path(&request.context.cwd, source);
            let dst_path = resolve_path(&request.context.cwd, destination);
            session.sync_pair(&src_path, &dst_path, false);
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
            if dst_meta.is_symlink() {
                if let Ok(existing_target) = fs::read_link(dst_path) {
                    if existing_target == target {
                        self.progress.skipped_files += 1;
                        let _ = self.sink.emit(Event::Progress(self.progress_event()));
                        return;
                    }
                }
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
        if let Some(ref dst_meta) = dst_meta {
            if dst_meta.is_file()
                && dst_meta.len() == src_meta.len()
                && dst_meta.mtime() == src_meta.mtime()
                && dst_meta.mtime_nsec() == src_meta.mtime_nsec()
            {
                self.progress.skipped_files += 1;
                let _ = self.sink.emit(Event::Progress(self.progress_event()));
                return;
            }
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
            {
                if let Ok(cloned_fd) = rustix::fs::openat(
                    dst_parent_fd,
                    dst_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    let _ = platform::apply_metadata(
                        MetadataTarget::Fd(cloned_fd.as_fd()),
                        &src_obs,
                        &src_ext,
                        Preservation::Copy,
                    );
                    return Ok(());
                }
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
            {
                if let Ok(cloned_fd) = rustix::fs::openat(
                    dst_parent_fd,
                    &temp_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    let _ = platform::apply_metadata(
                        MetadataTarget::Fd(cloned_fd.as_fd()),
                        &src_obs,
                        &src_ext,
                        Preservation::Copy,
                    );
                    cloned = true;
                }
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
        let target_dir_existed = target_dir.exists();

        if !contents_mode {
            self.progress.examined_files += 1;
            if !target_dir_existed {
                if !self.request.options.dry_run {
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
                }
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

        let mut src_names = HashSet::new();
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
            let name = entry.file_name();
            src_names.insert(name.clone());
            child_entries.push(name);
        }
        child_entries.sort();

        if self.request.options.delete && target_dir.exists() {
            if let Ok(dst_entries) = fs::read_dir(&target_dir) {
                let mut extraneous = Vec::new();
                for entry in dst_entries.flatten() {
                    let name = entry.file_name();
                    if !src_names.contains(&name) {
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
        }

        let src_dir_fd = rustix::fs::open(
            src_dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ).ok();

        let dst_dir_fd = if target_dir.exists() {
            rustix::fs::open(
                &target_dir,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            ).ok()
        } else {
            None
        };

        let mut files_to_transfer = Vec::new();

        for name in child_entries {
            if self.cancellation.is_cancelled() {
                return;
            }
            let child_src = src_dir.join(&name);
            let child_dst = target_dir.join(&name);

            let child_src_meta = match fs::symlink_metadata(&child_src) {
                Ok(m) => m,
                Err(error) => {
                    let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                        path: child_src,
                        message: format!("cannot access source: {error}"),
                    }));
                    self.failed = true;
                    continue;
                }
            };

            if child_src_meta.is_file() && src_dir_fd.is_some() && dst_dir_fd.is_some() && !self.request.options.dry_run {
                self.progress.examined_files += 1;
                let size = child_src_meta.len();

                if child_src_meta.nlink() > 1 {
                    let identity = Identity {
                        file: fdu_posix::identity::FileIdentity {
                            device: child_src_meta.dev(),
                            inode: child_src_meta.ino(),
                        },
                        kind: EntryKind::RegularFile,
                    };
                    if !self.seen_hardlinks.insert(identity) {
                        let _ = self.sink.emit(Event::Diagnostic(Diagnostic {
                            path: child_src.clone(),
                            message: format!(
                                "hardlink group detected for {}; topology preservation is not supported in v1 without --split-hardlinks",
                                child_src.display()
                            ),
                        }));
                        self.failed = true;
                        continue;
                    }
                }

                let dst_meta = fs::symlink_metadata(&child_dst).ok();
                if let Some(ref dst_meta) = dst_meta {
                    if dst_meta.is_file()
                        && dst_meta.len() == child_src_meta.len()
                        && dst_meta.mtime() == child_src_meta.mtime()
                        && dst_meta.mtime_nsec() == child_src_meta.mtime_nsec()
                    {
                        self.progress.skipped_files += 1;
                        let _ = self.sink.emit(Event::Progress(self.progress_event()));
                        continue;
                    }
                }

                files_to_transfer.push((name, child_src, child_dst, size, dst_meta.is_some()));
            } else {
                self.sync_pair(&child_src, &child_dst, false);
            }
        }

        if !files_to_transfer.is_empty() {
            let jobs = self.request.options.jobs.map_or(4, Jobs::get).min(files_to_transfer.len());
            if jobs <= 1 || files_to_transfer.len() < 4 {
                for (name, _child_src, child_dst, size, is_replacement) in files_to_transfer {
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
                            path: child_dst,
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
                                let Some((name, _child_src, child_dst, size, is_replacement)) = item else {
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
                                            *e = Some((child_dst, err));
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

        if !self.request.options.dry_run && (!target_dir_existed || children_modified) && target_dir.exists() {
            if let Some(ref src_fd) = src_dir_fd {
                if let Some(ref dst_fd) = dst_dir_fd {
                    if let Ok(obs) = roots::observe_fd(src_fd.as_fd()) {
                        if let Ok(ext) = platform::capture_metadata(MetadataTarget::Fd(src_fd.as_fd())) {
                            let _ = platform::apply_metadata(
                                MetadataTarget::Fd(dst_fd.as_fd()),
                                &obs,
                                &ext,
                                Preservation::Copy,
                            );
                        }
                    }
                }
            }
        }
    }

    fn delete_entry(&mut self, path: &Path) {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };

        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    self.delete_entry(&entry.path());
                }
            }
            self.progress.deleted_files += 1;
            let _ = self.sink.emit(Event::Progress(self.progress_event()));
            if !self.request.options.dry_run {
                let _ = fs::remove_dir(path);
            }
        } else {
            self.progress.deleted_files += 1;
            let _ = self.sink.emit(Event::Progress(self.progress_event()));
            if !self.request.options.dry_run {
                let _ = fs::remove_file(path);
            }
        }
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
