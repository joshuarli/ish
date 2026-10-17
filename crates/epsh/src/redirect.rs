use std::fmt;
use std::sync::{Arc, Mutex};
use std::io::Write;
use std::os::unix::io::{AsRawFd, IntoRawFd, OwnedFd, RawFd};

use crate::ast::*;
use crate::error::ShellError;
use crate::eval::{OutputSink, RedirectOpen, RedirectOpenMode, Shell};
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
    pub(crate) backup_slot: Option<usize>,
    pub(crate) was_closed: bool,
    sink: Option<OutputSink>,
    flags: Option<rustix::io::FdFlags>,
}

#[derive(Default)]
pub(crate) struct SavedRedirections {
    fds: Vec<SavedFd>,
    previous_pending_stdin: Option<usize>,
    pending_stdin_changed: bool,
}

impl Shell {
    pub(crate) fn pending_stdin_fd(&self) -> Option<RawFd> {
        self.pending_stdin.map(|index| {
            self.redirection_fds[index]
                .as_ref()
                .expect("pending stdin slot is occupied")
                .lock().unwrap().as_ref().unwrap().as_raw_fd()
        })
    }

    fn hold_pending_stdin(&mut self, fd: OwnedFd) -> usize {
        let index = self.redirection_fds.len();
        self.redirection_fds.push(Some(Arc::new(Mutex::new(Some(fd)))));
        index
    }

    fn release_pending_stdin(&mut self, index: usize) {
        drop(
            self.redirection_fds[index]
                .take()
                .expect("pending stdin slot is occupied"),
        );
        self.trim_redirection_fds();
    }

    pub(crate) fn setup_redirections(
        &mut self,
        redirs: &[Redir],
    ) -> crate::error::Result<SavedRedirections> {
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
        let mut saved = SavedRedirections::default();
        match self.apply_redirections(redirs, &mut saved, saved_fd_min) {
            Ok(()) => Ok(saved),
            Err(error) => {
                let cleanup = self.restore_redirections(saved);
                self.combine_redirection_cleanup(Err(error), cleanup)
            }
        }
    }

    fn move_held_fd_for_target(
        &mut self,
        target_fd: RawFd,
        saved_fd_min: RawFd,
    ) -> crate::error::Result<()> {
        for descriptor in self.redirection_fds.iter().flatten() {
            let mut held = descriptor.lock().unwrap();
            let Some(current) = held.as_ref() else { continue; };
            if current.as_raw_fd() != target_fd { continue; }
            let moved = sys::duplicate_fd_above(target_fd, saved_fd_min)?;
            let old = std::mem::replace(held.as_mut().unwrap(), moved);
            sys::close(old.into_raw_fd())?;
            break;
        }
        Ok(())
    }

    fn combine_redirection_cleanup<T>(
        &self,
        primary: crate::error::Result<T>,
        cleanup: crate::error::Result<()>,
    ) -> crate::error::Result<T> {
        if matches!(&primary, Err(
            ShellError::Exit(_) | ShellError::Return(_) | ShellError::Break(_)
            | ShellError::Continue(_) | ShellError::Cancelled | ShellError::TimedOut
            | ShellError::Stopped { .. }
        )) {
            if let Err(error) = cleanup {
                self.err_msg(&format!("epsh: {error}"));
            }
            primary
        } else {
            combine_cleanup(primary, cleanup)
        }
    }

    pub(crate) fn trim_redirection_fds(&mut self) {
        while self.redirection_fds.last().is_some_and(Option::is_none) {
            self.redirection_fds.pop();
        }
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
        saved: &mut SavedRedirections,
        saved_fd_min: RawFd,
    ) -> crate::error::Result<()> {
        for redir in redirs {
            let target_fd = redir.fd;
            self.move_held_fd_for_target(target_fd, saved_fd_min)?;
            if target_fd == 0 {
                if saved.pending_stdin_changed {
                    if let Some(index) = self.pending_stdin.take() {
                        self.release_pending_stdin(index);
                    }
                } else {
                    saved.previous_pending_stdin = self.pending_stdin.take();
                    saved.pending_stdin_changed = true;
                }
            }
            let preserve_fd = !(target_fd == 0
                && matches!(
                    redir.kind,
                    RedirKind::HereDoc(_) | RedirKind::HereDocStrip(_)
                )
                && (self.stdout_sink.is_some() || self.stderr_sink.is_some()));

            if preserve_fd {
                let (backup_slot, was_closed) = match sys::duplicate_fd_above(target_fd, saved_fd_min)
                {
                    Ok(copy) => {
                        let index = self.redirection_fds.len();
                        self.redirection_fds.push(Some(Arc::new(Mutex::new(Some(copy)))));
                        (Some(index), false)
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EBADF) => (None, true),
                    Err(error) => return Err(ShellError::Io(error)),
                };

                saved.fds.push(SavedFd {
                    target_fd,
                    backup_slot,
                    was_closed,
                    sink: self.output_sink(target_fd),
                    flags: if was_closed { None } else { Some(sys::descriptor_flags(target_fd)?) },
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
                    self.set_output_sink(target_fd, None);
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
                    install_redirection(file, target_fd).map_err(ShellError::Io)?;
                    self.set_output_sink(target_fd, None);
                }
                RedirKind::DupInput(word) | RedirKind::DupOutput(word) => {
                    let fd_str = self.expand_string(word)?;
                    if fd_str == "-" {
                        sys::close(target_fd)?;
                        self.set_output_sink(target_fd, None);
                    } else if let Ok(source_fd) = fd_str.parse::<i32>() {
                        if self.redirection_fds.iter().any(|backup| {
                            backup.as_ref().is_some_and(|fd| fd.lock().unwrap().as_ref().is_some_and(|fd| fd.as_raw_fd() == source_fd))
                        }) {
                            return Err(ShellError::Io(std::io::Error::from_raw_os_error(libc::EBADF)));
                        }
                        let sink = self.output_sink(source_fd);
                        sys::dup2_result(source_fd, target_fd).map_err(ShellError::Io)?;
                        self.set_output_sink(target_fd, sink);
                    } else {
                        return Err(ShellError::Runtime {
                            msg: format!("{fd_str}: bad file descriptor"),
                            span: redir.span,
                        });
                    }
                }
                RedirKind::HereDoc(body) | RedirKind::HereDocStrip(body) => {
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
                    let mut file = sys::heredoc_file()?;
                    file.write_all(expanded.as_bytes())?;
                    std::io::Seek::rewind(&mut file)?;
                    let read_end = OwnedFd::from(file);

                    if target_fd == 0 && (self.stdout_sink.is_some() || self.stderr_sink.is_some())
                    {
                        self.pending_stdin = Some(self.hold_pending_stdin(read_end));
                    } else {
                        install_redirection(read_end, target_fd).map_err(ShellError::Io)?;
                    }
                    self.set_output_sink(target_fd, None);
                }
            }
        }

        Ok(())
    }

    pub(crate) fn restore_redirections(
        &mut self,
        saved: SavedRedirections,
    ) -> crate::error::Result<()> {
        let SavedRedirections {
            fds,
            previous_pending_stdin,
            pending_stdin_changed,
        } = saved;
        let mut errors = Vec::new();
        for s in fds.into_iter().rev() {
            self.set_output_sink(s.target_fd, s.sink);
            if let Some(index) = s.backup_slot {
                let copy = Arc::try_unwrap(self.redirection_fds[index].take().unwrap()).unwrap().into_inner().unwrap().unwrap().into_raw_fd();
                match sys::dup2_result(copy, s.target_fd) {
                    Ok(()) => {
                        if let Some(flags) = s.flags {
                            // SAFETY: dup2 just installed an open descriptor at the saved target.
                            let target = unsafe { std::os::fd::BorrowedFd::borrow_raw(s.target_fd) };
                            if let Err(error) = rustix::io::fcntl_setfd(target, flags) { errors.push(error.into()); }
                        }
                    }
                    Err(error) => errors.push(error),
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
        if pending_stdin_changed {
            if let Some(index) = self.pending_stdin.take() {
                self.release_pending_stdin(index);
            }
            self.pending_stdin = previous_pending_stdin;
        }
        self.trim_redirection_fds();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(restoration_error(errors))
        }
    }

    pub(crate) fn finish_redirections<T>(
        &mut self,
        primary: crate::error::Result<T>,
        saved: SavedRedirections,
    ) -> crate::error::Result<T> {
        let cleanup = self.restore_redirections(saved);
        self.combine_redirection_cleanup(primary, cleanup)
    }

    pub(crate) fn finish_saved_redirections<T>(
        &mut self,
        primary: crate::error::Result<T>,
        saved: SavedRedirections,
    ) -> crate::error::Result<T> {
        let SavedRedirections {
            fds,
            previous_pending_stdin,
            ..
        } = saved;
        if let Some(index) = previous_pending_stdin {
            self.release_pending_stdin(index);
        }
        let errors: Vec<_> = fds
            .into_iter()
            .filter_map(|state| state.backup_slot)
            .filter_map(|index| {
                let fd = Arc::try_unwrap(self.redirection_fds[index].take().unwrap()).unwrap().into_inner().unwrap().unwrap().into_raw_fd();
                sys::close(fd).err()
            })
            .collect();
        self.trim_redirection_fds();
        let cleanup = if errors.is_empty() {
            Ok(())
        } else {
            Err(restoration_error(errors))
        };
        self.combine_redirection_cleanup(primary, cleanup)
    }
}
