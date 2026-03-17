use crate::alias::AliasMap;
use crate::builtin;
use crate::error::Error;
use crate::expand;
use crate::job::Job;
use crate::parse::{self, CommandLine, Connector, PipedCommand, Redirect, RedirectKind};
use crate::signal;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::path::PathBuf;

/// Execute a full command line (segments with connectors).
/// Returns the exit status of the last executed pipeline.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    cmdline: &CommandLine,
    aliases: &AliasMap,
    job: &mut Option<Job>,
    orig_termios: &libc::termios,
    home: &str,
    prev_dir: &mut Option<String>,
    path_cache: &mut HashMap<String, PathBuf>,
    session_log: &mut String,
) -> i32 {
    let mut last_status = 0i32;

    for (i, (pipeline, _connector)) in cmdline.segments.iter().enumerate() {
        // Check the connector from the PREVIOUS segment
        if i > 0
            && let Some((_, Some(prev_conn))) = cmdline.segments.get(i - 1)
        {
            match prev_conn {
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
            path_cache,
            session_log,
        );
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
    path_cache: &mut HashMap<String, PathBuf>,
    session_log: &mut String,
) -> i32 {
    // Expand all commands
    let mut expanded = Vec::new();
    for pcmd in commands {
        let mut argv = pcmd.cmd.argv.clone();

        // Alias expansion (first word only, no recursion)
        if let Some(first) = argv.first() {
            let clean_first = parse::unescape(first);
            if let Some(alias_args) = aliases.get(&clean_first) {
                argv.remove(0);
                let mut new_argv: Vec<String> = alias_args.iter().map(|s| s.to_string()).collect();
                new_argv.extend(argv);
                argv = new_argv;
            }
        }

        // Expand words (tilde, vars, globs)
        let mut exec_subst =
            |cmd: &str| -> Result<String, Error> { capture_command_output(cmd, orig_termios) };
        match expand::expand_argv(&argv, home, &mut exec_subst) {
            Ok(expanded_argv) => {
                if expanded_argv.is_empty() {
                    return 0;
                }
                let mut redirects = Vec::new();
                for r in &pcmd.cmd.redirects {
                    let target = expand::expand_word(&r.target, home, &mut exec_subst)
                        .map(|v| v.join(" "))
                        .unwrap_or_else(|_| parse::unescape(&r.target));
                    redirects.push(Redirect {
                        kind: r.kind,
                        target,
                    });
                }
                expanded.push((expanded_argv, redirects, pcmd.pipe_stderr));
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
        let (argv, redirects, _) = &expanded[0];
        let cmd_name = &argv[0];
        if builtin::is_special_builtin(cmd_name) {
            return builtin::run_special(
                cmd_name,
                &argv[1..],
                redirects,
                prev_dir,
                home,
                job,
                path_cache,
                session_log,
            );
        }
        // Output-only builtins as single commands: run in-process too
        if builtin::is_builtin(cmd_name) {
            return builtin::run_output(cmd_name, &argv[1..], redirects);
        }
    }

    // Fork/exec pipeline
    let n = expanded.len();
    let mut prev_read: RawFd = -1;
    let mut pids = Vec::with_capacity(n);
    let mut pgid: libc::pid_t = 0;
    let cmd_text: String = expanded
        .iter()
        .map(|(argv, _, _)| argv.join(" "))
        .collect::<Vec<_>>()
        .join(" | ");

    for (i, (argv, redirects, pipe_stderr)) in expanded.iter().enumerate() {
        let is_last = i == n - 1;

        // Create pipe unless last command
        let (pipe_r, pipe_w) = if !is_last {
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                eprintln!("ish: pipe() failed");
                return 1;
            }
            (fds[0], fds[1])
        } else {
            (-1, -1)
        };

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
                *pipe_stderr,
                argv,
                redirects,
                orig_termios,
                path_cache,
            );
            // child_setup does not return
        }

        // -- Parent --
        if i == 0 {
            pgid = pid;
        }
        // Set child's pgid (race: also done in child)
        unsafe {
            libc::setpgid(pid, pgid);
        }

        if prev_read != -1 {
            unsafe {
                libc::close(prev_read);
            }
        }
        if pipe_w != -1 {
            unsafe {
                libc::close(pipe_w);
            }
        }
        prev_read = pipe_r;
        pids.push(pid);
    }

    // Give foreground to pipeline's process group
    unsafe {
        libc::tcsetpgrp(0, pgid);
    }

    // Wait for all children
    let mut last_status = 0i32;
    let mut stopped = false;
    for &pid in &pids {
        let mut wstatus = 0i32;
        loop {
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

    // Reclaim foreground
    unsafe {
        libc::tcsetpgrp(0, libc::getpgrp());
    }

    if stopped {
        *job = Some(Job {
            pgid,
            cmd: cmd_text,
        });
        // Notify user
        eprintln!("\nish: job suspended: {}", job.as_ref().unwrap().cmd);
        return 148; // 128 + SIGTSTP(20) = 148
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
fn capture_command_output(cmd: &str, orig_termios: &libc::termios) -> Result<String, Error> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::msg("pipe() failed for command substitution"));
    }
    let (pipe_r, pipe_w) = (fds[0], fds[1]);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(pipe_r);
            libc::close(pipe_w);
        }
        return Err(Error::msg("fork() failed for command substitution"));
    }

    if pid == 0 {
        // Child: redirect stdout to pipe, exec via /bin/sh
        unsafe {
            libc::close(pipe_r);
            libc::dup2(pipe_w, 1);
            libc::close(pipe_w);
            // Restore terminal
            libc::tcsetattr(0, libc::TCSANOW, orig_termios);
            signal::restore_defaults();
        }
        let sh = CString::new("/bin/sh").unwrap();
        let c_flag = CString::new("-c").unwrap();
        let c_cmd = CString::new(cmd).unwrap();
        let argv = [
            sh.as_ptr(),
            c_flag.as_ptr(),
            c_cmd.as_ptr(),
            std::ptr::null(),
        ];
        unsafe {
            libc::execvp(sh.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }

    // Parent
    unsafe {
        libc::close(pipe_w);
    }

    let mut output = String::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(pipe_r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
            output.push_str(s);
        }
    }
    unsafe {
        libc::close(pipe_r);
    }

    let mut wstatus = 0i32;
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
    orig_termios: &libc::termios,
    path_cache: &HashMap<String, PathBuf>,
) -> ! {
    unsafe {
        // Process group
        let my_pgid = if index == 0 { 0 } else { pgid };
        libc::setpgid(0, my_pgid);

        // Restore terminal settings and signal defaults
        libc::tcsetattr(0, libc::TCSANOW, orig_termios);
        signal::restore_defaults();

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

        // Check for output-only builtins in pipeline
        if builtin::is_builtin(&argv[0]) {
            let status = builtin::run_output(&argv[0], &argv[1..], &[]);
            libc::_exit(status);
        }

        // Exec
        let path = find_in_path(&argv[0], path_cache);
        let c_path = match CString::new(path.as_str()) {
            Ok(c) => c,
            Err(_) => {
                eprintln!("ish: invalid command name");
                libc::_exit(127);
            }
        };
        let c_args: Vec<CString> = argv
            .iter()
            .map(|a| CString::new(a.as_str()).unwrap_or_else(|_| CString::new("").unwrap()))
            .collect();
        let c_argv: Vec<*const libc::c_char> = c_args
            .iter()
            .map(|a| a.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        libc::execvp(c_path.as_ptr(), c_argv.as_ptr());

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

fn apply_redirect(r: &Redirect) {
    let open_write = |path: &str, flags: i32| unsafe {
        let c = CString::new(path).unwrap();
        libc::open(c.as_ptr(), flags, 0o644)
    };
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
                unsafe {
                    libc::dup2(fd, 1);
                    libc::dup2(fd, 2);
                    libc::close(fd);
                }
            }
        }
    }
}

/// Look up command in PATH cache, fall back to direct PATH scan.
pub fn find_in_path(cmd: &str, cache: &HashMap<String, PathBuf>) -> String {
    // Absolute or relative path
    if cmd.contains('/') {
        return cmd.to_string();
    }

    // Check cache
    if let Some(path) = cache.get(cmd) {
        return path.to_string_lossy().to_string();
    }

    // Live scan
    if let Some(path) = scan_path(cmd) {
        return path.to_string_lossy().to_string();
    }

    cmd.to_string()
}

pub fn scan_path(cmd: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(':') {
        let full = PathBuf::from(dir).join(cmd);
        if let Ok(meta) = full.metadata() {
            use std::os::unix::fs::PermissionsExt;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Some(full);
            }
        }
    }
    None
}

/// Rebuild the PATH cache.
pub fn rebuild_path_cache(cache: &mut HashMap<String, PathBuf>) {
    cache.clear();
    let path_var = match std::env::var("PATH") {
        Ok(p) => p,
        Err(_) => return,
    };
    for dir in path_var.split(':') {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(name)
                    && let Ok(meta) = entry.metadata()
                {
                    use std::os::unix::fs::PermissionsExt;
                    if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                        e.insert(entry.path());
                    }
                }
            }
        }
    }
}

/// Resume a suspended job in the foreground.
pub fn resume_job(job: &mut Option<Job>) -> i32 {
    let j = match job.take() {
        Some(j) => j,
        None => {
            eprintln!("ish: fg: no suspended job");
            return 1;
        }
    };

    eprintln!("ish: resuming: {}", j.cmd);

    unsafe {
        // Give the job the foreground
        libc::tcsetpgrp(0, j.pgid);
        // Send SIGCONT
        libc::killpg(j.pgid, libc::SIGCONT);
    }

    // Wait
    let mut wstatus = 0i32;
    loop {
        let r = unsafe { libc::waitpid(-j.pgid, &mut wstatus, libc::WUNTRACED) };
        if r < 0 {
            break;
        }
        if libc::WIFEXITED(wstatus) || libc::WIFSIGNALED(wstatus) {
            break;
        }
        if libc::WIFSTOPPED(wstatus) {
            // Stopped again
            *job = Some(Job {
                pgid: j.pgid,
                cmd: j.cmd,
            });
            eprintln!("\nish: job suspended again");
            break;
        }
    }

    // Reclaim foreground
    unsafe {
        libc::tcsetpgrp(0, libc::getpgrp());
    }

    if libc::WIFEXITED(wstatus) {
        libc::WEXITSTATUS(wstatus)
    } else if libc::WIFSIGNALED(wstatus) {
        128 + libc::WTERMSIG(wstatus)
    } else {
        148
    }
}
