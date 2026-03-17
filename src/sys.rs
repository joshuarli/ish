//! Platform-specific syscall wrappers.
//!
//! Linux: uses pipe2 (atomic CLOEXEC/NONBLOCK), close_range (bulk fd close).
//! macOS: falls back to pipe+fcntl, getdtablesize loop.

use std::os::fd::RawFd;

/// Create a pipe with O_CLOEXEC on both ends.
/// Linux: 1 syscall (pipe2). macOS: 3 syscalls (pipe + 2x fcntl).
pub fn pipe_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let mut fds = [0i32; 2];

    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for &fd in &fds {
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
    }

    Ok((fds[0], fds[1]))
}

/// Create a pipe with O_CLOEXEC | O_NONBLOCK on both ends.
/// Used for the signal self-pipe.
/// Linux: 1 syscall. macOS: 5 syscalls (pipe + 4x fcntl).
pub fn pipe_nonblock_cloexec() -> Result<(RawFd, RawFd), std::io::Error> {
    let mut fds = [0i32; 2];

    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for &fd in &fds {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
    }

    Ok((fds[0], fds[1]))
}

/// Close all file descriptors >= `min_fd`.
/// Called in child after pipe/redirect setup, before exec.
/// Prevents fd leaks from parent into child processes.
///
/// Linux 5.9+: single close_range syscall.
/// macOS: iterates to getdtablesize().
pub fn close_fds_from(min_fd: RawFd) {
    #[cfg(target_os = "linux")]
    unsafe {
        // close_range(2) — Linux 5.9+. Silently ignored on older kernels (ENOSYS).
        libc::syscall(libc::SYS_close_range, min_fd as u32, u32::MAX, 0u32);
    }

    #[cfg(target_os = "macos")]
    {
        let max = unsafe { libc::getdtablesize() };
        for fd in min_fd..max {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

/// Spawn a child for command substitution using posix_spawn.
/// Avoids fork's page-table copy — on Linux, posix_spawn uses
/// clone(CLONE_VFORK|CLONE_VM) internally.
///
/// Child: stdin=/dev/null, stdout=pipe_w, stderr=inherited, signals=default.
/// Returns (pid, pipe_read_fd).
pub fn spawn_command_subst(cmd: &str) -> Result<(libc::pid_t, RawFd), std::io::Error> {
    use std::ffi::CString;

    let (pipe_r, pipe_w) = pipe_cloexec()?;

    // File actions: stdout → pipe, stdin → /dev/null
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

    // Attributes: reset all signals to default
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

    unsafe extern "C" {
        static environ: *const *mut libc::c_char;
    }

    let mut pid: libc::pid_t = 0;
    let rc = unsafe {
        libc::posix_spawnp(
            &mut pid,
            sh.as_ptr(),
            &file_actions,
            &attrs,
            argv.as_ptr(),
            environ,
        )
    };

    unsafe {
        libc::posix_spawn_file_actions_destroy(&mut file_actions);
        libc::posix_spawnattr_destroy(&mut attrs);
        libc::close(pipe_w);
    }

    if rc != 0 {
        unsafe { libc::close(pipe_r) };
        return Err(std::io::Error::from_raw_os_error(rc));
    }

    Ok((pid, pipe_r))
}
