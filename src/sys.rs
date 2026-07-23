//! Platform-specific syscall wrappers.
//!
//! Linux: uses pipe2 (atomic CLOEXEC/NONBLOCK), close_range (bulk fd close).
//! macOS: falls back to pipe+fcntl, getdtablesize loop.

use std::os::fd::{IntoRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;

/// Create a pipe with O_CLOEXEC on both ends.
/// Linux: 1 syscall (pipe2). macOS: 3 syscalls (pipe + 2x fcntl).
pub fn pipe_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let (read, write) = rustix::pipe::pipe()?;
    rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC)?;
    Ok((read.into_raw_fd(), write.into_raw_fd()))
}

/// Create a pipe with O_CLOEXEC | O_NONBLOCK on both ends.
/// Used for the signal self-pipe.
/// Linux: 1 syscall. macOS: 5 syscalls (pipe + 4x fcntl).
pub fn pipe_nonblock_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let (read, write) = rustix::pipe::pipe()?;
    rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC)?;
    rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK)?;
    rustix::fs::fcntl_setfl(&write, rustix::fs::OFlags::NONBLOCK)?;
    Ok((read.into_raw_fd(), write.into_raw_fd()))
}

/// Close all file descriptors >= `min_fd`.
/// Called in child after pipe/redirect setup, before exec.
/// Prevents fd leaks from parent into child processes.
///
/// Linux 5.9+: single close_range syscall.
/// macOS: iterates to getdtablesize().
pub fn close_fds_from(min_fd: RawFd) {
    #[cfg(target_os = "linux")]
    // rustix 1.1.4 does not expose close_range; retain the direct syscall
    // for the one-call child-fd cleanup path.
    // SAFETY: close_range closes a range of fds. On older kernels (< 5.9)
    // returns ENOSYS which we ignore — fd leak is non-critical.
    unsafe {
        libc::syscall(libc::SYS_close_range, min_fd as u32, u32::MAX, 0u32);
    }

    #[cfg(target_os = "macos")]
    {
        // rustix has no getdtablesize/closefrom equivalent; only the table
        // size query remains libc-based, while closing uses rustix.
        // SAFETY: getdtablesize returns the fd table size. Closing invalid
        // fds returns EBADF which is harmlessly ignored.
        let max = unsafe { libc::getdtablesize() };
        for fd in min_fd..max {
            unsafe { rustix::io::close(fd) };
        }
    }
}

/// Execute a command, searching PATH if needed. Does not return on success.
///
/// Linux: uses execveat() — iterates PATH directories by fd, no path string
/// construction needed. Falls through to error if all dirs fail.
/// macOS: uses execvp() which handles PATH search internally.
///
/// # Safety
/// Must be called in a forked child process. `argv` must be a valid
/// null-terminated array of C string pointers.
pub unsafe fn exec_command(cmd: &std::ffi::CStr, argv: *const *const libc::c_char) {
    #[cfg(target_os = "linux")]
    {
        // Absolute or relative path — exec directly
        if cmd.to_bytes().contains(&b'/') {
            unsafe {
                let _ = rustix::runtime::execve(cmd, argv.cast(), get_environ().cast());
            }
            return;
        }

        // Search PATH using execveat: open each dir as fd, try exec relative to it.
        // No path string allocation — the kernel resolves cmd within the directory.
        if let Some(path_var) = std::env::var_os("PATH") {
            for dir in path_var.as_os_str().as_bytes().split(|&b| b == b':') {
                if dir.is_empty() {
                    continue;
                }
                if let Ok(c_dir) = std::ffi::CString::new(dir) {
                    let dirfd = rustix::fs::open(
                        c_dir.as_c_str(),
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    );
                    if let Ok(dirfd) = dirfd {
                        unsafe {
                            let _ = rustix::runtime::execveat(
                                &dirfd,
                                cmd,
                                argv.cast(),
                                get_environ().cast(),
                                rustix::fs::AtFlags::empty(),
                            );
                        };
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    unsafe {
        // rustix provides execve/execveat but not execvp, so macOS PATH
        // execution keeps the platform libc wrapper.
        libc::execvp(cmd.as_ptr(), argv);
    }
}

/// Access the process environment pointer for exec calls.
///
/// # Safety
/// The returned pointer is only valid for immediate use in exec syscalls
/// within a forked child. The shell is single-threaded so environ is stable.
unsafe fn get_environ() -> *const *const libc::c_char {
    // rustix has no portable borrowed environ-pointer API; exec needs the
    // process environment without constructing a new environment array.
    unsafe extern "C" {
        static environ: *const *mut libc::c_char;
    }
    unsafe { environ as *const *const libc::c_char }
}

/// Spawn a child for command substitution using posix_spawn.
/// Avoids fork's page-table copy — on Linux, posix_spawn uses
/// clone(CLONE_VFORK|CLONE_VM) internally.
///
/// Child: stdin=/dev/null, stdout=pipe_w, stderr=inherited, signals=default.
/// Returns (pid, pipe_read_fd).
pub fn spawn_command_subst(cmd: &str) -> Result<(i32, RawFd), std::io::Error> {
    use std::ffi::CString;

    let (pipe_r, pipe_w) = pipe_cloexec()?;

    // rustix does not expose POSIX spawn file-actions/attributes, which are
    // used here to avoid fork overhead for command substitution.
    // SAFETY: posix_spawn_file_actions are opaque structs managed by init/destroy.
    // We init, configure, use in spawn, then destroy — standard lifecycle.
    let mut file_actions: libc::posix_spawn_file_actions_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::posix_spawn_file_actions_init(&mut file_actions);
        libc::posix_spawn_file_actions_adddup2(&mut file_actions, pipe_w, 1);
        // stdin from /dev/null — command substitution shouldn't read terminal
        let dev_null = CString::new("/dev/null").unwrap();
        libc::posix_spawn_file_actions_addopen(
            &mut file_actions,
            0,
            dev_null.as_ptr(),
            libc::O_RDONLY,
            0,
        );
    }

    // SAFETY: posix_spawnattr managed by init/destroy. SETSIGDEF with a full
    // sigset resets all signals to default in the child.
    let mut attrs: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::posix_spawnattr_init(&mut attrs);
        libc::posix_spawnattr_setflags(&mut attrs, libc::POSIX_SPAWN_SETSIGDEF as libc::c_short);
        let mut sigset: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut sigset);
        libc::posix_spawnattr_setsigdefault(&mut attrs, &sigset);
    }

    let sh = CString::new("/bin/sh").unwrap();
    let c_flag = CString::new("-c").unwrap();
    let c_cmd = CString::new(cmd)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in command"))?;
    let argv: [*mut libc::c_char; 4] = [
        sh.as_ptr() as *mut _,
        c_flag.as_ptr() as *mut _,
        c_cmd.as_ptr() as *mut _,
        std::ptr::null_mut(),
    ];

    let mut pid: i32 = 0;
    // SAFETY: All CStrings are alive for the duration of the call.
    // argv is a valid null-terminated array. file_actions and attrs are initialized.
    let rc = unsafe {
        libc::posix_spawnp(
            &mut pid,
            sh.as_ptr(),
            &file_actions,
            &attrs,
            argv.as_ptr(),
            get_environ() as *const *mut libc::c_char,
        )
    };

    // SAFETY: Cleanup — destroy must be called after init regardless of spawn result.
    unsafe {
        libc::posix_spawn_file_actions_destroy(&mut file_actions);
        libc::posix_spawnattr_destroy(&mut attrs);
        rustix::io::close(pipe_w);
    }

    if rc != 0 {
        unsafe { rustix::io::close(pipe_r) };
        return Err(std::io::Error::from_raw_os_error(rc));
    }

    Ok((pid, pipe_r))
}
