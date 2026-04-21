//! denv integration — automatic .envrc/.env loading on shell start and cd.
//!
//! On shell start and every cd, checks if the environment needs updating
//! via a fast-path (file mtimes + sentinel), then calls `denv export bash`
//! and applies the set/unset commands to the current process environment.

use std::path::Path;

/// A single environment variable change from denv.
pub enum EnvChange {
    Set(String, String),
    Unset(String),
}

/// Initialize denv. Returns true if denv is available and active.
pub fn init() -> bool {
    if crate::path::scan_path("denv").is_none() {
        return false;
    }

    let pid = std::process::id();
    crate::shell_setenv("__DENV_PID", &pid.to_string());
    crate::shell_setenv("__DENV_SHELL", "bash");

    let data_dir = std::env::var("DENV_DATA_DIR")
        .or_else(|_| std::env::var("XDG_DATA_HOME").map(|d| format!("{d}/denv")))
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}/.local/share/denv")
        });
    crate::shell_setenv("__DENV_SENTINEL", &format!("{data_dir}/active_{pid}"));

    true
}

/// Called once after init to load the current directory's environment.
pub fn on_startup() -> Vec<EnvChange> {
    run_export()
}

/// Called after cd. Checks fast-path, runs export if needed.
/// Returns environment changes (already applied to process env).
pub fn on_cd() -> Vec<EnvChange> {
    if fast_path_ok() {
        return Vec::new();
    }
    run_export()
}

/// Handle `denv allow|deny|reload`. Returns Some(changes) if handled.
pub fn command(args: &[String]) -> Option<Vec<EnvChange>> {
    match args.first().map(|s| s.as_str()) {
        Some("allow") | Some("reload") => {
            let mut changes = run_denv_and_apply(args);
            // denv's shell hook reconciles allow/reload via a later
            // `denv export bash`. ish has no prompt hook, so do that here.
            changes.extend(run_export());
            Some(changes)
        }
        Some("deny") => Some(run_denv_and_apply(args)),
        _ => None,
    }
}

/// Fast-path: check cached state to skip the denv subprocess.
/// Mirrors the fish hook's __denv_export fast-path logic.
fn fast_path_ok() -> bool {
    let state = match std::env::var("__DENV_STATE") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let sentinel = match std::env::var("__DENV_SENTINEL") {
        Ok(s) => s,
        Err(_) => return false,
    };

    // State format: "{envrc_mtime} {dotenv_mtime} {dir}"
    let mut parts = state.splitn(3, ' ');
    let envrc_mt: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let dotenv_mt: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let dir = match parts.next() {
        Some(d) => d,
        None => return false,
    };

    // Must be in or under the cached directory
    let pwd = std::env::var("PWD").unwrap_or_default();
    if pwd != dir
        && !(pwd.len() > dir.len()
            && pwd.as_bytes().get(dir.len()) == Some(&b'/')
            && pwd.starts_with(dir))
    {
        return false;
    }

    // Sentinel file must exist
    if !Path::new(&sentinel).exists() {
        return false;
    }

    let sentinel_mt = file_mtime(&sentinel);
    // Build paths without format! — reuse a single String buffer.
    let mut path_buf = String::with_capacity(dir.len() + 8);
    path_buf.push_str(dir);
    path_buf.push_str("/.envrc");
    let envrc_path = path_buf.clone();
    path_buf.truncate(dir.len());
    path_buf.push_str("/.env");
    let dotenv_path = path_buf;

    // Neither file newer than sentinel
    if file_mtime(&envrc_path) > sentinel_mt {
        return false;
    }
    if file_mtime(&dotenv_path) > sentinel_mt {
        return false;
    }

    // If file was loaded (mtime > 0) but since deleted, must reload
    if envrc_mt != 0 && !Path::new(&envrc_path).exists() {
        return false;
    }
    if dotenv_mt != 0 && !Path::new(&dotenv_path).exists() {
        return false;
    }

    true
}

fn file_mtime(path: &str) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

fn run_export() -> Vec<EnvChange> {
    run_denv_and_apply(&["export".to_string(), "bash".to_string()])
}

/// Run `denv <args>`, capture stdout, parse and apply export/unset lines.
/// Returns the list of changes (already applied to process env).
///
/// Uses fork+exec directly (not std::process::Command) because the shell's
/// signal handlers and process group setup require explicit cleanup in the child.
fn run_denv_and_apply(args: &[String]) -> Vec<EnvChange> {
    // Build command string for /bin/sh -c
    let cmd = format!("denv {}", args.join(" "));

    // Create pipe for capturing stdout
    let (pipe_r, pipe_w) = match crate::sys::pipe_cloexec() {
        Ok(fds) => fds,
        Err(_) => return Vec::new(),
    };

    // SAFETY: fork() in single-threaded process. Child inherits fds/memory.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(pipe_r);
            libc::close(pipe_w);
        }
        return Vec::new();
    }

    if pid == 0 {
        // SAFETY: Forked child — redirect stdout to pipe, stdin from /dev/null,
        // reset signals, then exec /bin/sh. Does not return on success.
        unsafe {
            libc::close(pipe_r);
            libc::dup2(pipe_w, 1);
            libc::close(pipe_w);
            let dev_null = std::ffi::CString::new("/dev/null").unwrap();
            let null_fd = libc::open(dev_null.as_ptr(), libc::O_RDONLY);
            if null_fd >= 0 {
                libc::dup2(null_fd, 0);
                libc::close(null_fd);
            }

            // Reset ALL signals to default (critical for child correctness)
            crate::signal::restore_defaults();

            let sh = std::ffi::CString::new("/bin/sh").unwrap();
            let c_flag = std::ffi::CString::new("-c").unwrap();
            let c_cmd = std::ffi::CString::new(cmd.as_str()).unwrap();
            let argv: [*const libc::c_char; 4] = [
                sh.as_ptr(),
                c_flag.as_ptr(),
                c_cmd.as_ptr(),
                std::ptr::null(),
            ];
            libc::execv(sh.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }

    // Parent: close write end, read output, wait for child
    // SAFETY: pipe_w is a valid fd from pipe_cloexec.
    unsafe { libc::close(pipe_w) };

    let mut output = String::new();
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: Reading from valid pipe fd into stack buffer.
        let n = unsafe { libc::read(pipe_r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
                output.push_str(s);
            }
        } else if n == 0 {
            break; // EOF
        } else {
            // n < 0: retry on EINTR, break on other errors
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
    }
    // SAFETY: Close pipe and reap child. pid is valid from fork above.
    unsafe {
        libc::close(pipe_r);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }

    let changes = parse_bash_output(&output);
    for change in &changes {
        match change {
            EnvChange::Set(k, v) => crate::shell_setenv(k, v),
            EnvChange::Unset(k) => crate::shell_unsetenv(k),
        }
    }
    changes
}

/// Parse `export KEY='value';` and `unset KEY;` lines into changes.
fn parse_bash_output(output: &str) -> Vec<EnvChange> {
    let mut changes = Vec::new();
    for line in output.lines() {
        let line = line.trim_end_matches(';');
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some(eq) = rest.find('=') {
                let key = rest[..eq].to_string();
                let value = unquote_bash(&rest[eq + 1..]);
                changes.push(EnvChange::Set(key, value));
            }
        } else if let Some(key) = line.strip_prefix("unset ") {
            changes.push(EnvChange::Unset(key.trim().to_string()));
        }
    }
    changes
}

/// Benchmark-only: parse denv output lines without mutating environment.
/// Returns the number of directives parsed.
pub fn apply_bash_output_bench(output: &str) -> usize {
    parse_bash_output(output).len()
}

/// Unquote a bash single-quoted string: `'value'` → `value`, `'\''` → `'`.
fn unquote_bash(s: &str) -> String {
    let s = s.strip_prefix('\'').unwrap_or(s);
    let s = s.strip_suffix('\'').unwrap_or(s);
    // Avoid allocation when no embedded quotes (the common case)
    if s.contains("'\\''") {
        s.replace("'\\''", "'")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquote_simple() {
        assert_eq!(unquote_bash("'hello'"), "hello");
    }

    #[test]
    fn unquote_with_inner_quote() {
        assert_eq!(unquote_bash("'it'\\''s'"), "it's");
    }

    #[test]
    fn unquote_empty() {
        assert_eq!(unquote_bash("''"), "");
    }

    #[test]
    fn unquote_no_quotes() {
        assert_eq!(unquote_bash("bare"), "bare");
    }

    #[test]
    fn unquote_path_with_spaces() {
        assert_eq!(unquote_bash("'/path/to/my dir'"), "/path/to/my dir");
    }

    #[test]
    fn parse_export() {
        let changes = parse_bash_output("export _DENV_TEST_A='hello_world';");
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            EnvChange::Set(k, v) => {
                assert_eq!(k, "_DENV_TEST_A");
                assert_eq!(v, "hello_world");
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn parse_unset() {
        let changes = parse_bash_output("unset _DENV_TEST_B;");
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            EnvChange::Unset(k) => assert_eq!(k, "_DENV_TEST_B"),
            _ => panic!("expected Unset"),
        }
    }

    #[test]
    fn parse_multi_line() {
        let changes = parse_bash_output(
            "export _DENV_TEST_C='one';\nexport _DENV_TEST_D='two';\nunset _DENV_TEST_E;",
        );
        assert_eq!(changes.len(), 3);
        match &changes[0] {
            EnvChange::Set(k, v) => {
                assert_eq!(k, "_DENV_TEST_C");
                assert_eq!(v, "one");
            }
            _ => panic!("expected Set"),
        }
        match &changes[1] {
            EnvChange::Set(k, v) => {
                assert_eq!(k, "_DENV_TEST_D");
                assert_eq!(v, "two");
            }
            _ => panic!("expected Set"),
        }
        match &changes[2] {
            EnvChange::Unset(k) => assert_eq!(k, "_DENV_TEST_E"),
            _ => panic!("expected Unset"),
        }
    }

    #[test]
    fn parse_value_with_embedded_quote() {
        let changes = parse_bash_output("export _DENV_TEST_F='it'\\''s here';");
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            EnvChange::Set(_, v) => assert_eq!(v, "it's here"),
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn parse_empty_value() {
        let changes = parse_bash_output("export _DENV_TEST_G='';");
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            EnvChange::Set(_, v) => assert_eq!(v, ""),
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn fast_path_fails_without_state() {
        // No __DENV_STATE set → fast path should fail
        unsafe { std::env::remove_var("__DENV_STATE") };
        assert!(!fast_path_ok());
    }
}
