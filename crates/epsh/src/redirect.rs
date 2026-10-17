use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use crate::ast::*;
use crate::error::ShellError;
use crate::eval::{RedirectOpen, RedirectOpenMode, Shell};
use crate::shell_bytes::ShellBytes;

use crate::sys;

/// Saved file descriptor for restoration after redirections.
pub(crate) struct SavedFd {
    pub(crate) target_fd: RawFd,
    pub(crate) saved_copy: Option<RawFd>,
    /// The target was not open before the redirection, so restoration closes it.
    pub(crate) was_closed: bool,
}

impl Shell {
    /// Apply redirections left to right. Returns saved FD state for
    /// restoration. When a later redirection fails, the descriptors changed by
    /// earlier ones are restored before the error is returned.
    pub(crate) fn setup_redirections(
        &mut self,
        redirs: &[Redir],
    ) -> crate::error::Result<Vec<SavedFd>> {
        let mut saved = Vec::new();
        match self.apply_redirections(redirs, &mut saved) {
            Ok(()) => Ok(saved),
            Err(e) => {
                self.restore_redirections(saved);
                Err(e)
            }
        }
    }

    /// Open a writable redirection target, letting an installed provider own
    /// the open. The default open matches `File::create`, append, and `<>`.
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
                // SAFETY: target_fd is only borrowed for the duplicate; an
                // unopened descriptor reports EBADF.
                let target = unsafe { std::os::fd::BorrowedFd::borrow_raw(target_fd) };
                let (saved_copy, was_closed) = match rustix::io::fcntl_dupfd_cloexec(target, 10) {
                    Ok(copy) => (Some(std::os::fd::IntoRawFd::into_raw_fd(copy)), false),
                    Err(e) => (None, e == rustix::io::Errno::BADF),
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
                    // SAFETY: file fd is valid from File::open; target_fd is the redirect target.
                    sys::dup2(file.as_raw_fd(), target_fd);
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
                    // SAFETY: file is an open descriptor owned for this call; target_fd is the redirect target.
                    sys::dup2(file.as_raw_fd(), target_fd);
                }
                RedirKind::DupInput(word) | RedirKind::DupOutput(word) => {
                    let fd_str = self.expand_string(word)?;
                    if fd_str == "-" {
                        // SAFETY: target_fd is a valid fd number from the redirect syntax.
                        sys::close(target_fd);
                    } else if let Ok(source_fd) = fd_str.parse::<i32>() {
                        // SAFETY: source_fd is user-specified (may fail at OS level); target_fd is from redirect syntax.
                        sys::dup2(source_fd, target_fd);
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
                    let read_fd = fds[0];

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

                    // When sinks are active the shell is embedded in a multi-threaded
                    // process. Calling dup2(read_fd, 0) in the parent would replace the
                    // process-wide fd 0 and race with other threads reading stdin (e.g.
                    // nerv's keyboard input loop). Instead, stash the read fd and let
                    // eval_external pass it via cmd.stdin() after fork — only the child
                    // inherits it. For builtins/functions that need stdin, the dup2 path
                    // is still used since they run in-process on this thread.
                    if target_fd == 0 && (self.stdout_sink.is_some() || self.stderr_sink.is_some())
                    {
                        // Close any previously pending stdin (shouldn't happen in practice).
                        if let Some(old) = self.pending_stdin.take() {
                            // SAFETY: old is a valid fd we own.
                            sys::close(old);
                        }
                        self.pending_stdin = Some(read_fd);
                        // No SavedFd entry needed: we never touched fd 0.
                    } else {
                        // SAFETY: read_fd is valid from pipe(); target_fd is the redirect target.
                        sys::dup2(read_fd, target_fd);
                        sys::close(read_fd);
                    }
                }
            }
        }

        Ok(())
    }

    /// Restore file descriptors after redirections.
    pub(crate) fn restore_redirections(&self, saved: Vec<SavedFd>) {
        for s in saved.into_iter().rev() {
            if let Some(copy) = s.saved_copy {
                // SAFETY: copy is a valid fd from fcntl_dupfd_cloexec; target_fd is the original fd being restored.
                sys::dup2(copy, s.target_fd);
                sys::close(copy);
            } else if s.was_closed
                // SAFETY: only queried; a redirection that failed before its
                // dup2 left the descriptor closed, and closing it again would
                // report EBADF.
                && rustix::io::fcntl_getfd(unsafe {
                    std::os::fd::BorrowedFd::borrow_raw(s.target_fd)
                })
                .is_ok()
            {
                sys::close(s.target_fd);
            }
        }
    }
}
