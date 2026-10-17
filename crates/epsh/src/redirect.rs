use std::fmt;
use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

use crate::ast::*;
use crate::error::ShellError;
use crate::eval::{RedirectOpen, RedirectOpenMode, Shell};
use crate::shell_bytes::ShellBytes;

use crate::sys;

#[derive(Debug)]
struct RedirectionRestoreFailure(Vec<std::io::Error>);

impl fmt::Display for RedirectionRestoreFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("failed to restore redirections: ")?;
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RedirectionRestoreFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.first().map(|error| error as _)
    }
}

#[derive(Debug)]
struct RedirectionCleanupFailure {
    primary: ShellError,
    cleanup: ShellError,
}

impl fmt::Display for RedirectionCleanupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}; cleanup failed: {}",
            self.primary, self.cleanup
        )
    }
}

impl std::error::Error for RedirectionCleanupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
    }
}

fn combine_cleanup<T>(
    primary: crate::error::Result<T>,
    cleanup: crate::error::Result<()>,
) -> crate::error::Result<T> {
    match (primary, cleanup) {
        (primary, Ok(())) => primary,
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => {
            let kind = match &primary {
                ShellError::Io(error) => error.kind(),
                _ => std::io::ErrorKind::Other,
            };
            Err(ShellError::Io(std::io::Error::new(
                kind,
                RedirectionCleanupFailure { primary, cleanup },
            )))
        }
    }
}

fn restoration_error(errors: Vec<std::io::Error>) -> ShellError {
    let kind = errors[0].kind();
    ShellError::Io(std::io::Error::new(
        kind,
        RedirectionRestoreFailure(errors),
    ))
}

fn install_redirection(file: OwnedFd, target_fd: RawFd) -> std::io::Result<()> {
    if file.as_raw_fd() == target_fd {
        rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())?;
        let _ = file.into_raw_fd();
        Ok(())
    } else {
        sys::dup2_result(file.as_raw_fd(), target_fd)
    }
}

pub(crate) struct SavedFd {
    pub(crate) target_fd: RawFd,
    pub(crate) saved_copy: Option<RawFd>,
    pub(crate) was_closed: bool,
}

impl Shell {
    pub(crate) fn setup_redirections(
        &mut self,
        redirs: &[Redir],
    ) -> crate::error::Result<Vec<SavedFd>> {
        let saved_fd_min = redirs
            .iter()
            .map(|redir| redir.fd)
            .max()
            .unwrap_or(9)
            .max(9)
            .checked_add(1)
            .ok_or_else(|| {
                ShellError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "redirection descriptor leaves no room for saved descriptors",
                ))
            })?;
        let mut saved = Vec::new();
        match self.apply_redirections(redirs, &mut saved, saved_fd_min) {
            Ok(()) => Ok(saved),
            Err(error) => combine_cleanup(Err(error), self.restore_redirections(saved)),
        }
    }

    fn move_saved_descriptor_for_source(
        &self,
        saved: &mut [SavedFd],
        source_fd: RawFd,
        saved_fd_min: RawFd,
    ) -> crate::error::Result<()> {
        let Some(index) = saved
            .iter()
            .position(|state| state.saved_copy == Some(source_fd))
        else {
            return Ok(());
        };
        let min_fd = source_fd
            .checked_add(1)
            .map(|fd| fd.max(saved_fd_min))
            .ok_or_else(|| {
                ShellError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "redirection source descriptor leaves no room for saved descriptors",
                ))
            })?;
        let moved = sys::duplicate_fd_above(source_fd, min_fd).map_err(ShellError::Io)?;
        saved[index].saved_copy = Some(moved.into_raw_fd());
        sys::close(source_fd)?;
        Ok(())
    }

    fn open_writable_redirection(
        &mut self,
        path: &std::path::Path,
        mode: RedirectOpenMode,
        target_fd: RawFd,
    ) -> std::io::Result<std::os::fd::OwnedFd> {
        if let Some(handler) = self.redirect_open_handler.as_mut() {
            let request = RedirectOpen {
                path,
                mode,
                fd: target_fd,
            };
            if let Some(result) = handler(&request) {
                return result;
            }
        }
        let mut options = std::fs::OpenOptions::new();
        match mode {
            RedirectOpenMode::Truncate => options.write(true).create(true).truncate(true),
            RedirectOpenMode::Append => options.append(true).create(true),
            RedirectOpenMode::ReadWrite => options.read(true).write(true).create(true),
        };
        options.open(path).map(Into::into)
    }

    fn apply_redirections(
        &mut self,
        redirs: &[Redir],
        saved: &mut Vec<SavedFd>,
        saved_fd_min: RawFd,
    ) -> crate::error::Result<()> {
        for redir in redirs {
            let target_fd = redir.fd;
            let preserve_fd = !(target_fd == 0
                && matches!(
                    redir.kind,
                    RedirKind::HereDoc(_) | RedirKind::HereDocStrip(_)
                )
                && (self.stdout_sink.is_some() || self.stderr_sink.is_some()));

            if preserve_fd {
                let (saved_copy, was_closed) = match sys::duplicate_fd_above(target_fd, saved_fd_min)
                {
                    Ok(copy) => (Some(std::os::fd::IntoRawFd::into_raw_fd(copy)), false),
                    Err(error) if error.raw_os_error() == Some(libc::EBADF) => (None, true),
                    Err(error) => return Err(ShellError::Io(error)),
                };

                saved.push(SavedFd {
                    target_fd,
                    saved_copy,
                    was_closed,
                });
            }

            match &redir.kind {
                RedirKind::Input(word) => {
                    let filename = self.expand_string(word)?;
                    let filepath =
                        self.resolve_path_bytes(&ShellBytes::from_str_lossless(&filename));
                    let file = std::fs::File::open(&filepath).map_err(|e| {
                        self.err_msg(&format!("{filename}: {e}"));
                        ShellError::Io(e)
                    })?;
                    install_redirection(file.into(), target_fd).map_err(ShellError::Io)?;
                }
                RedirKind::Output(word)
                | RedirKind::Clobber(word)
                | RedirKind::Append(word)
                | RedirKind::ReadWrite(word) => {
                    let mode = match redir.kind {
                        RedirKind::Append(_) => RedirectOpenMode::Append,
                        RedirKind::ReadWrite(_) => RedirectOpenMode::ReadWrite,
                        _ => RedirectOpenMode::Truncate,
                    };
                    let filename = self.expand_string(word)?;
                    let filepath =
                        self.resolve_path_bytes(&ShellBytes::from_str_lossless(&filename));
                    let file = self
                        .open_writable_redirection(&filepath, mode, target_fd)
                        .map_err(|e| {
                            self.err_msg(&format!("{filename}: {e}"));
                            ShellError::Io(e)
                        })?;
                    install_redirection(file.into(), target_fd).map_err(ShellError::Io)?;
                }
                RedirKind::DupInput(word) | RedirKind::DupOutput(word) => {
                    let fd_str = self.expand_string(word)?;
                    if fd_str == "-" {
                        sys::close(target_fd)?;
                    } else if let Ok(source_fd) = fd_str.parse::<i32>() {
                        self.move_saved_descriptor_for_source(saved, source_fd, saved_fd_min)?;
                        sys::dup2_result(source_fd, target_fd).map_err(ShellError::Io)?;
                    } else {
                        return Err(ShellError::Runtime {
                            msg: format!("{fd_str}: bad file descriptor"),
                            span: redir.span,
                        });
                    }
                }
                RedirKind::HereDoc(body) | RedirKind::HereDocStrip(body) => {
                    let mut fds = [0i32; 2];
                    // SAFETY: fds is a valid 2-element array for pipe() to write into.
                    sys::pipe(fds.as_mut_ptr());
                    // SAFETY: fds[1] is a valid fd just returned by pipe().
                    let write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };
                    // SAFETY: fds[0] is a valid fd just returned by pipe().
                    let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };

                    let expanded = match body {
                        HereDocBody::Literal(s) => s.clone(),
                        HereDocBody::Parsed(parts) => {
                            let word = Word {
                                parts: parts.clone(),
                                span: redir.span,
                            };
                            crate::shell_bytes::ShellBytes::from_str_lossless(
                                &self.expand_string(&word)?,
                            )
                        }
                    };
                    let _ = (&write_end).write_all(expanded.as_bytes());
                    drop(write_end);

                    if target_fd == 0 && (self.stdout_sink.is_some() || self.stderr_sink.is_some())
                    {
                        if let Some(old) = self.pending_stdin.take() {
                            // SAFETY: old is a valid fd we own.
                            sys::close(old)?;
                        }
                        self.pending_stdin = Some(read_end.into_raw_fd());
                    } else {
                        install_redirection(read_end.into(), target_fd).map_err(ShellError::Io)?;
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn restore_redirections(&self, saved: Vec<SavedFd>) -> crate::error::Result<()> {
        let mut errors = Vec::new();
        for s in saved.into_iter().rev() {
            if let Some(copy) = s.saved_copy {
                if let Err(error) = sys::dup2_result(copy, s.target_fd) {
                    errors.push(error);
                }
                if let Err(error) = sys::close(copy) {
                    errors.push(error);
                }
            } else if s.was_closed {
                match sys::descriptor_is_open(s.target_fd) {
                    Ok(()) => {
                        if let Err(error) = sys::close(s.target_fd) {
                            errors.push(error);
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EBADF) => {}
                    Err(error) => errors.push(error),
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(restoration_error(errors))
        }
    }

    pub(crate) fn finish_redirections<T>(
        &self,
        primary: crate::error::Result<T>,
        saved: Vec<SavedFd>,
    ) -> crate::error::Result<T> {
        combine_cleanup(primary, self.restore_redirections(saved))
    }

    pub(crate) fn finish_saved_redirections<T>(
        &self,
        primary: crate::error::Result<T>,
        saved: Vec<SavedFd>,
    ) -> crate::error::Result<T> {
        let errors: Vec<_> = saved
            .into_iter()
            .filter_map(|state| state.saved_copy)
            .filter_map(|fd| sys::close(fd).err())
            .collect();
        let cleanup = if errors.is_empty() {
            Ok(())
        } else {
            Err(restoration_error(errors))
        };
        combine_cleanup(primary, cleanup)
    }
}
