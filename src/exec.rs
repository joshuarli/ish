use crate::alias::AliasMap;
use crate::builtin;
use crate::error::Error;
use crate::expand;
use crate::job::{Continuation, Job};
use crate::parse::{self, CommandLine, Connector, PipedCommand, Redirect, RedirectKind};
use crate::signal;
use crate::sys;
use std::collections::HashSet;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::path::PathBuf;

/// Check if a string is a `NAME=VALUE` variable assignment.
/// Returns the byte position of `=` if the prefix is a valid variable name.
pub fn var_assignment_pos(s: &str) -> Option<usize> {
    let eq = s.find('=')?;
    if eq == 0 {
        return None;
    }
    let name = &s.as_bytes()[..eq];
    if !name[0].is_ascii_alphabetic() && name[0] != b'_' {
        return None;
    }
    if name.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_') {
        Some(eq)
    } else {
        None
    }
}

/// Split leading `NAME=VALUE` entries from argv, returning them separately.
fn split_env_prefix(argv: &mut Vec<String>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    while let Some(entry) = argv.first() {
        if let Some(eq) = var_assignment_pos(entry) {
            let name = entry[..eq].to_string();
            let val = entry[eq + 1..].to_string();
            env.push((name, val));
            argv.remove(0);
        } else {
            break;
        }
    }
    env
}

/// Temporarily set env vars, returning the previous values for restore.
fn set_env_temp(env: &[(String, String)]) -> Vec<(String, Option<String>)> {
    env.iter()
        .map(|(k, v)| {
            let old = std::env::var(k).ok();
            crate::shell_setenv(k, v);
            (k.clone(), old)
        })
        .collect()
}

/// Restore env vars saved by set_env_temp.
fn restore_env(saved: &[(String, Option<String>)]) {
    for (k, old) in saved {
        match old {
            Some(v) => crate::shell_setenv(k, v),
            None => crate::shell_unsetenv(k),
        }
    }
}

/// A single command in an expanded pipeline, ready for execution.
struct ExpandedCommand {
    argv: Vec<String>,
    redirects: Vec<Redirect>,
    pipe_stderr: bool,
    env: Vec<(String, String)>,
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: fd is a valid file descriptor (checked >= 0).
        unsafe {
            libc::close(fd);
        }
    }
}

/// Execute a full command line (segments with connectors).
/// Returns the exit status of the last executed pipeline.
///
/// `entry`: if resuming a continuation, provides the resumed pipeline's exit status
/// and the connector linking it to the first segment. `None` for normal execution.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    cmdline: &CommandLine,
    entry: Option<(i32, Connector)>,
    aliases: &AliasMap,
    job: &mut Option<Job>,
    orig_termios: &libc::termios,
    home: &str,
    prev_dir: &mut Option<String>,
    session_log: &mut String,
    prev_status: i32,
) -> i32 {
    let mut last_status = entry.map_or(prev_status, |(s, _)| s);

    for (i, (pipeline, _connector)) in cmdline.segments.iter().enumerate() {
        // Check connector: for i=0 use entry connector, else use previous segment's connector
        let prev_conn = if i == 0 {
            entry.map(|(_, c)| c)
        } else {
            cmdline.segments[i - 1].1
        };

        if let Some(conn) = prev_conn {
            match conn {
                Connector::And if last_status != 0 => continue,
                Connector::Or if last_status == 0 => continue,
                Connector::Semi | Connector::And | Connector::Or => {}
            }
        }

        last_status = execute_pipeline(
            &pipeline.commands,
            aliases,
            job,
            orig_termios,
            home,
            prev_dir,
            session_log,
            last_status,
        );

        // If pipeline was suspended, set full command text and save continuation
        if last_status == 148 {
            if let Some(j) = job {
                j.cmd = format_segments(&cmdline.segments);
                if let Some(connector) = cmdline.segments[i].1
                    && i + 1 < cmdline.segments.len()
                {
                    j.continuation = Some(Continuation {
                        connector,
                        segments: cmdline.segments[i + 1..].to_vec(),
                    });
                }
            }
            return 148;
        }
    }

    last_status
}

#[allow(clippy::too_many_arguments)]
fn execute_pipeline(
    commands: &[PipedCommand],
    aliases: &AliasMap,
    job: &mut Option<Job>,
    orig_termios: &libc::termios,
    home: &str,
    prev_dir: &mut Option<String>,
    session_log: &mut String,
    last_status: i32,
) -> i32 {
    // Expand all commands
    let mut expanded = Vec::new();
    for pcmd in commands {
        let mut argv = pcmd.cmd.argv.clone();

        // Alias expansion (first non-assignment word only, no recursion)
        let cmd_idx = argv
            .iter()
            .position(|w| var_assignment_pos(&parse::unescape(w)).is_none());
        if let Some(idx) = cmd_idx {
            let clean = parse::unescape(&argv[idx]);
            if let Some(alias_args) = aliases.get(&clean) {
                argv.remove(idx);
                for (j, a) in alias_args.iter().enumerate() {
                    argv.insert(idx + j, a.to_string());
                }
            }
        }

        // Expand words (tilde, vars, globs)
        let mut exec_subst =
            |cmd: &str| -> Result<String, Error> { capture_command_output(cmd, orig_termios) };
        match expand::expand_argv(&argv, home, &mut exec_subst, last_status) {
            Ok(mut expanded_argv) => {
                // Split leading NAME=VALUE prefix assignments
                let env = split_env_prefix(&mut expanded_argv);
                if expanded_argv.is_empty() {
                    // Bare FOO=bar without a command is a no-op.
                    // Use `export FOO=bar` to set environment variables.
                    return 0;
                }
                let mut redirects = Vec::new();
                for r in &pcmd.cmd.redirects {
                    let target = expand::expand_word(&r.target, home, &mut exec_subst, last_status)
                        .map(|v| v.join(" "))
                        .unwrap_or_else(|_| parse::unescape(&r.target));
                    redirects.push(Redirect {
                        kind: r.kind,
                        target,
                    });
                }
                expanded.push(ExpandedCommand {
                    argv: expanded_argv,
                    redirects,
                    pipe_stderr: pcmd.pipe_stderr,
                    env,
                });
            }
            Err(e) => {
                eprintln!("ish: {e}");
                return 1;
            }
        }
    }

    if expanded.is_empty() {
        return 0;
    }

    // Single command, no pipe: check for builtins that modify state
    if expanded.len() == 1 {
        let cmd = &expanded[0];
        let cmd_name = &cmd.argv[0];
        if builtin::is_special_builtin(cmd_name) {
            let saved = set_env_temp(&cmd.env);
            let status = builtin::run_special(
                cmd_name,
                &cmd.argv[1..],
                &cmd.redirects,
                prev_dir,
                home,
                job,
                session_log,
            );
            restore_env(&saved);
            return status;
        }
        // Output-only builtins as single commands: run in-process too
        if builtin::is_builtin(cmd_name) {
            let saved = set_env_temp(&cmd.env);
            let status = builtin::run_output(cmd_name, &cmd.argv[1..], &cmd.redirects);
            restore_env(&saved);
            return status;
        }
    }

    // Fork/exec pipeline
    let n = expanded.len();
    let mut prev_read: RawFd = -1;
    let mut pids = Vec::with_capacity(n);
    let mut pgid: libc::pid_t = 0;
    let cmd_text: String = expanded
        .iter()
        .map(|cmd| {
            let mut parts: Vec<String> = cmd.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
            parts.extend(cmd.argv.iter().cloned());
            parts.join(" ")
        })
        .collect::<Vec<_>>()
        .join(" | ");

    for (i, cmd) in expanded.iter().enumerate() {
        let is_last = i == n - 1;

        // Create pipe unless last command (O_CLOEXEC: auto-closed on exec)
        let (pipe_r, pipe_w) = if !is_last {
            match sys::pipe_cloexec() {
                Ok(fds) => fds,
                Err(_) => {
                    eprintln!("ish: pipe() failed");
                    return 1;
                }
            }
        } else {
            (-1, -1)
        };

        // SAFETY: fork() in single-threaded process. Child inherits fds and
        // memory; parent continues with the returned pid.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            eprintln!("ish: fork() failed");
            return 1;
        }

        if pid == 0 {
            // -- Child --
            child_setup(
                i,
                pgid,
                prev_read,
                pipe_r,
                pipe_w,
                cmd.pipe_stderr,
                &cmd.argv,
                &cmd.redirects,
                &cmd.env,
                orig_termios,
            );
            // child_setup does not return
        }

        // -- Parent --
        if i == 0 {
            pgid = pid;
        }
        // SAFETY: Set child's pgid (race: also done in child to avoid TOCTOU).
        unsafe {
            libc::setpgid(pid, pgid);
        }

        close_fd(prev_read);
        close_fd(pipe_w);
        prev_read = pipe_r;
        pids.push(pid);
    }

    // SAFETY: Give foreground to pipeline's process group so it receives
    // terminal signals (SIGINT, SIGTSTP). pgid is valid (set above).
    unsafe {
        libc::tcsetpgrp(0, pgid);
    }

    // Wait for all children
    let mut last_status = 0i32;
    let mut stopped = false;
    for &pid in &pids {
        let mut wstatus = 0i32;
        loop {
            // SAFETY: waitpid with WUNTRACED returns on exit, signal, or stop.
            let r = unsafe { libc::waitpid(pid, &mut wstatus, libc::WUNTRACED) };
            if r < 0 {
                break;
            }
            if libc::WIFEXITED(wstatus) || libc::WIFSIGNALED(wstatus) {
                break;
            }
            if libc::WIFSTOPPED(wstatus) {
                stopped = true;
                break;
            }
        }
        if pid == *pids.last().unwrap() {
            last_status = wstatus;
        }
    }

    if stopped {
        // SAFETY: Save the job's terminal attributes BEFORE reclaiming foreground.
        // This preserves settings from programs like vim/less that modify termios.
        // zeroed() is valid for termios; tcgetattr fills it from the terminal.
        let mut job_termios: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::tcgetattr(0, &mut job_termios) };

        // SAFETY: Reclaim foreground for the shell's process group.
        unsafe { libc::tcsetpgrp(0, libc::getpgrp()) };

        *job = Some(Job {
            pgid,
            cmd: cmd_text,
            termios: job_termios,
            continuation: None, // filled by execute() if remaining segments exist
        });
        eprintln!("\nish: job suspended: {}", job.as_ref().unwrap().cmd);
        return 148; // 128 + SIGTSTP(20) = 148
    }

    // SAFETY: Reclaim foreground for the shell after pipeline completes.
    unsafe {
        libc::tcsetpgrp(0, libc::getpgrp());
    }

    if libc::WIFEXITED(last_status) {
        libc::WEXITSTATUS(last_status)
    } else if libc::WIFSIGNALED(last_status) {
        128 + libc::WTERMSIG(last_status)
    } else {
        1
    }
}

/// Execute a command string and capture its stdout (for command substitution).
/// Uses posix_spawn to avoid fork's page-table copy — on Linux this uses
/// clone(CLONE_VFORK|CLONE_VM) internally.
fn capture_command_output(cmd: &str, _orig_termios: &libc::termios) -> Result<String, Error> {
    let (pid, pipe_r) = sys::spawn_command_subst(cmd)
        .map_err(|e| Error::msg(format!("command substitution: {e}")))?;

    let mut output = String::new();
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: Reading from a valid pipe fd into a stack buffer.
        let n = unsafe { libc::read(pipe_r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
                output.push_str(s);
            }
        } else if n == 0 {
            break; // EOF
        } else {
            // n < 0: error — retry on EINTR, break on other errors
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
    }
    // SAFETY: Close the pipe read end and wait for child to exit.
    unsafe {
        libc::close(pipe_r);
    }

    let mut wstatus = 0i32;
    // SAFETY: Wait for the spawned child to exit. pid is valid from posix_spawn.
    unsafe {
        libc::waitpid(pid, &mut wstatus, 0);
    }

    Ok(output)
}

/// Child process setup: pgid, pipes, redirects, exec. Does not return.
#[allow(clippy::too_many_arguments)]
fn child_setup(
    index: usize,
    pgid: libc::pid_t,
    prev_read: RawFd,
    pipe_r: RawFd,
    pipe_w: RawFd,
    pipe_stderr: bool,
    argv: &[String],
    redirects: &[Redirect],
    env: &[(String, String)],
    orig_termios: &libc::termios,
) -> ! {
    // SAFETY: This entire block runs in a forked child process.
    // We set up the process group, restore terminal settings,
    // wire up pipes/redirects, then exec or _exit. No return.
    unsafe {
        // Process group
        let my_pgid = if index == 0 { 0 } else { pgid };
        libc::setpgid(0, my_pgid);

        // Restore terminal settings and signal defaults
        libc::tcsetattr(0, libc::TCSANOW, orig_termios);
        signal::restore_defaults();

        // Set prefix environment variables (VAR=VALUE cmd)
        for (name, val) in env {
            crate::shell_setenv(name, val);
        }

        // Pipe: stdin from previous pipe
        if prev_read != -1 {
            libc::dup2(prev_read, 0);
            libc::close(prev_read);
        }

        // Pipe: stdout (and maybe stderr) to next pipe
        if pipe_w != -1 {
            libc::dup2(pipe_w, 1);
            if pipe_stderr {
                libc::dup2(pipe_w, 2);
            }
            libc::close(pipe_w);
        }
        if pipe_r != -1 {
            libc::close(pipe_r);
        }

        // Apply redirections
        for r in redirects {
            apply_redirect(r);
        }

        // Close any inherited fds above stderr (defense-in-depth).
        // Our pipes are O_CLOEXEC so exec would close them, but builtins
        // call _exit() not exec, and this catches any other leaked fds.
        sys::close_fds_from(3);

        // Check for output-only builtins in pipeline
        if builtin::is_builtin(&argv[0]) {
            let status = builtin::run_output(&argv[0], &argv[1..], &[]);
            libc::_exit(status);
        }

        // Exec — on Linux uses execveat (no path string construction),
        // on macOS uses execvp.
        let c_cmd = match CString::new(argv[0].as_str()) {
            Ok(c) => c,
            Err(_) => {
                eprintln!("ish: invalid command name");
                libc::_exit(127);
            }
        };
        let c_args: Vec<CString> = match argv
            .iter()
            .map(|a| CString::new(a.as_str()))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(args) => args,
            Err(_) => {
                eprintln!("ish: argument contains NUL byte");
                libc::_exit(126);
            }
        };
        let c_argv: Vec<*const libc::c_char> = c_args
            .iter()
            .map(|a| a.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        sys::exec_command(&c_cmd, c_argv.as_ptr());

        // exec failed
        let err = std::io::Error::last_os_error();
        eprintln!("ish: {}: {err}", argv[0]);
        libc::_exit(if err.raw_os_error() == Some(libc::ENOENT) {
            127
        } else {
            126
        });
    }
}

/// Apply a single redirect in the child process. Called after fork, before exec.
/// Failures are silently ignored (child will exec or _exit shortly).
fn apply_redirect(r: &Redirect) {
    // SAFETY: open() with a valid CString path. Called in forked child.
    let open_write = |path: &str, flags: i32| unsafe {
        let c = CString::new(path).unwrap();
        libc::open(c.as_ptr(), flags, 0o644)
    };
    // SAFETY: dup2 + close to wire fd to target. Both fds are valid.
    let dup_close = |fd: i32, target: i32| unsafe {
        libc::dup2(fd, target);
        libc::close(fd);
    };

    match r.kind {
        RedirectKind::Out => {
            let fd = open_write(&r.target, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC);
            if fd >= 0 {
                dup_close(fd, 1);
            }
        }
        RedirectKind::Append => {
            let fd = open_write(&r.target, libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND);
            if fd >= 0 {
                dup_close(fd, 1);
            }
        }
        RedirectKind::In => {
            // SAFETY: open() for read with valid CString. In forked child.
            let fd = unsafe {
                let c = CString::new(r.target.as_str()).unwrap();
                libc::open(c.as_ptr(), libc::O_RDONLY, 0)
            };
            if fd >= 0 {
                dup_close(fd, 0);
            }
        }
        RedirectKind::Err => {
            let fd = open_write(&r.target, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC);
            if fd >= 0 {
                dup_close(fd, 2);
            }
        }
        RedirectKind::All => {
            let fd = open_write(&r.target, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC);
            if fd >= 0 {
                // SAFETY: Redirect both stdout and stderr to the opened fd.
                unsafe {
                    libc::dup2(fd, 1);
                    libc::dup2(fd, 2);
                    libc::close(fd);
                }
            }
        }
    }
}

/// Reconstruct display text from parsed segments: `sleep 2 && echo hi`.
fn format_segments(segments: &[(parse::Pipeline, Option<Connector>)]) -> String {
    let mut out = String::new();
    for (i, (pipeline, connector)) in segments.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        for (j, pc) in pipeline.commands.iter().enumerate() {
            if j > 0 {
                out.push_str(if pc.pipe_stderr { " &| " } else { " | " });
            }
            let words: Vec<String> = pc.cmd.argv.iter().map(|a| parse::unescape(a)).collect();
            out.push_str(&words.join(" "));
        }
        if let Some(conn) = connector {
            match conn {
                Connector::And => out.push_str(" &&"),
                Connector::Or => out.push_str(" ||"),
                Connector::Semi => out.push(';'),
            }
        }
    }
    out
}

pub fn scan_path(cmd: &str) -> Option<PathBuf> {
    // SAFETY: Single-threaded shell. getenv returns a pointer into the
    // environment block, valid until the variable is next modified.
    let path_bytes = unsafe {
        let ptr = libc::getenv(c"PATH".as_ptr());
        if ptr.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(ptr).to_bytes()
    };

    // Stack buffer for "dir/cmd\0" — avoids all PathBuf/String allocations
    let mut buf = [0u8; 4096];
    let cmd_bytes = cmd.as_bytes();

    for dir in path_bytes.split(|&b| b == b':') {
        if dir.is_empty() {
            continue;
        }
        let total = dir.len() + 1 + cmd_bytes.len();
        if total >= buf.len() {
            continue;
        }
        buf[..dir.len()].copy_from_slice(dir);
        buf[dir.len()] = b'/';
        buf[dir.len() + 1..total].copy_from_slice(cmd_bytes);
        buf[total] = 0; // NUL terminator for stat

        // SAFETY: buf is NUL-terminated, stat writes into a stack struct.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::stat(buf.as_ptr() as *const libc::c_char, &mut st) } == 0
            && (st.st_mode & libc::S_IFREG != 0)
            && (st.st_mode & 0o111 != 0)
        {
            use std::os::unix::ffi::OsStrExt;
            return Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..total])));
        }
    }
    None
}

/// Resume a suspended job in the foreground.
/// Returns (exit_status, optional_continuation).
/// If the job had remaining segments from a compound command, the continuation
/// is returned so the caller can execute it.
pub fn resume_job(job: &mut Option<Job>) -> (i32, Option<Continuation>) {
    let j = match job.take() {
        Some(j) => j,
        None => {
            eprintln!("ish: fg: no suspended job");
            return (1, None);
        }
    };

    let continuation = j.continuation;
    eprintln!("ish: resuming: {}", j.cmd);

    // SAFETY: Give the job the foreground and restore its terminal settings.
    // This is critical for programs like vim/less that set raw mode —
    // without this, they'd resume with the shell's terminal settings.
    // j.pgid and j.termios are valid (saved when job was suspended).
    unsafe {
        libc::tcsetpgrp(0, j.pgid);
        libc::tcsetattr(0, libc::TCSADRAIN, &j.termios);
        libc::killpg(j.pgid, libc::SIGCONT);
    }

    // Wait
    let mut wstatus = 0i32;
    loop {
        // SAFETY: Wait for any process in the job's pgid. WUNTRACED lets us
        // detect if the job is stopped again (e.g., Ctrl+Z in resumed vim).
        let r = unsafe { libc::waitpid(-j.pgid, &mut wstatus, libc::WUNTRACED) };
        if r < 0 {
            break;
        }
        if libc::WIFEXITED(wstatus) || libc::WIFSIGNALED(wstatus) {
            break;
        }
        if libc::WIFSTOPPED(wstatus) {
            // Stopped again — save terminal attrs before reclaiming.
            // Continuation stays on the job for next fg.
            // SAFETY: zeroed() valid for termios; tcgetattr fills it.
            let mut job_termios: libc::termios = unsafe { std::mem::zeroed() };
            unsafe { libc::tcgetattr(0, &mut job_termios) };

            *job = Some(Job {
                pgid: j.pgid,
                cmd: j.cmd,
                termios: job_termios,
                continuation,
            });
            eprintln!("\nish: job suspended again");

            // SAFETY: Reclaim foreground for the shell after job re-suspended.
            unsafe { libc::tcsetpgrp(0, libc::getpgrp()) };
            return (148, None);
        }
    }

    // SAFETY: Reclaim foreground for the shell after resumed job exits.
    unsafe {
        libc::tcsetpgrp(0, libc::getpgrp());
    }

    let status = if libc::WIFEXITED(wstatus) {
        libc::WEXITSTATUS(wstatus)
    } else if libc::WIFSIGNALED(wstatus) {
        128 + libc::WTERMSIG(wstatus)
    } else {
        1
    };

    (status, continuation)
}

// -- PATH Cache --

/// FNV-1a hash for short strings (command names). Inline, no dependencies.
fn fnv1a(s: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in s {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// O(1) command existence cache built from $PATH directories.
/// Stores FNV-1a hashes of executable names in a `HashSet<u64>`.
pub struct PathCache {
    commands: HashSet<u64>,
    path_hash: u64,
}

impl Default for PathCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PathCache {
    pub fn new() -> Self {
        Self {
            commands: HashSet::new(),
            path_hash: 0,
        }
    }

    /// Check if `cmd` is an executable on $PATH.
    /// Rebuilds the cache if $PATH has changed since the last check.
    pub fn contains(&mut self, cmd: &str) -> bool {
        self.ensure_fresh();
        self.commands.contains(&fnv1a(cmd.as_bytes()))
    }

    /// Rebuild the cache if $PATH has changed.
    fn ensure_fresh(&mut self) {
        // SAFETY: Single-threaded shell. getenv returns a pointer into the
        // environment block, valid until the variable is next modified.
        let path_bytes = unsafe {
            let ptr = libc::getenv(c"PATH".as_ptr());
            if ptr.is_null() {
                if self.path_hash != 0 {
                    self.commands.clear();
                    self.path_hash = 0;
                }
                return;
            }
            std::ffi::CStr::from_ptr(ptr).to_bytes()
        };

        let current_hash = fnv1a(path_bytes);
        if current_hash == self.path_hash && !self.commands.is_empty() {
            return;
        }
        self.path_hash = current_hash;
        self.rebuild(path_bytes);
    }

    /// Scan each directory in $PATH using libc opendir/readdir, insert hashes
    /// of executable filenames.
    fn rebuild(&mut self, path_bytes: &[u8]) {
        self.commands.clear();

        let mut pathbuf = [0u8; 4096];

        for dir in path_bytes.split(|&b| b == b':') {
            if dir.is_empty() {
                continue;
            }
            // NUL-terminate the directory path for opendir
            if dir.len() >= pathbuf.len() {
                continue;
            }
            pathbuf[..dir.len()].copy_from_slice(dir);
            pathbuf[dir.len()] = 0;

            // SAFETY: pathbuf is NUL-terminated. opendir returns NULL on failure.
            let dp = unsafe { libc::opendir(pathbuf.as_ptr() as *const libc::c_char) };
            if dp.is_null() {
                continue;
            }

            loop {
                // SAFETY: readdir returns entries from an open directory handle.
                let entry = unsafe { libc::readdir(dp) };
                if entry.is_null() {
                    break;
                }

                // SAFETY: d_name is a NUL-terminated C string within the dirent.
                let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
                let name_bytes = name.to_bytes();

                // Skip . and ..
                if name_bytes == b"." || name_bytes == b".." {
                    continue;
                }

                // Build "dir/name\0" for stat
                let total = dir.len() + 1 + name_bytes.len();
                if total >= pathbuf.len() {
                    continue;
                }
                // dir is already at pathbuf[..dir.len()]
                pathbuf[dir.len()] = b'/';
                pathbuf[dir.len() + 1..total].copy_from_slice(name_bytes);
                pathbuf[total] = 0;

                // SAFETY: pathbuf is NUL-terminated. stat fills a stack struct.
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::stat(pathbuf.as_ptr() as *const libc::c_char, &mut st) } != 0 {
                    continue;
                }

                // Regular file with at least one execute bit
                if (st.st_mode & libc::S_IFREG != 0) && (st.st_mode & 0o111 != 0) {
                    self.commands.insert(fnv1a(name_bytes));
                }
            }

            // SAFETY: Close the directory handle opened above.
            unsafe {
                libc::closedir(dp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_assignment_detection() {
        assert_eq!(var_assignment_pos("FOO=bar"), Some(3));
        assert_eq!(var_assignment_pos("A="), Some(1));
        assert_eq!(var_assignment_pos("_X=1"), Some(2));
        assert_eq!(var_assignment_pos("PATH=/usr/bin"), Some(4));
        // Not assignments
        assert_eq!(var_assignment_pos("=bar"), None);
        assert_eq!(var_assignment_pos("echo"), None);
        assert_eq!(var_assignment_pos("1FOO=bar"), None);
        assert_eq!(var_assignment_pos("FOO-X=bar"), None);
    }

    #[test]
    fn env_prefix_splitting() {
        let mut argv = vec!["FOO=bar".into(), "echo".into(), "hi".into()];
        let env = split_env_prefix(&mut argv);
        assert_eq!(env, vec![("FOO".into(), "bar".into())]);
        assert_eq!(argv, vec!["echo", "hi"]);

        // Multiple assignments
        let mut argv = vec!["A=1".into(), "B=2".into(), "cmd".into()];
        let env = split_env_prefix(&mut argv);
        assert_eq!(env.len(), 2);
        assert_eq!(argv, vec!["cmd"]);

        // No assignments
        let mut argv = vec!["echo".into(), "FOO=bar".into()];
        let env = split_env_prefix(&mut argv);
        assert!(env.is_empty());
        assert_eq!(argv, vec!["echo", "FOO=bar"]);

        // Bare assignment (all entries are assignments)
        let mut argv = vec!["FOO=bar".into()];
        let env = split_env_prefix(&mut argv);
        assert_eq!(env, vec![("FOO".into(), "bar".into())]);
        assert!(argv.is_empty());
    }
}
