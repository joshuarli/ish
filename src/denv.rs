//! denv integration — automatic .envrc/.env loading on cd.
//!
//! On shell start and every cd, checks if the environment needs updating
//! via a fast-path (file mtimes + sentinel), then calls `denv export bash`
//! and applies the set/unset commands to the current process environment.

use std::path::Path;

/// Initialize denv. Returns true if denv is available and active.
pub fn init() -> bool {
    if crate::exec::scan_path("denv").is_none() {
        return false;
    }

    let pid = std::process::id();
    // SAFETY: single-threaded shell
    unsafe {
        std::env::set_var("__DENV_PID", pid.to_string());
        std::env::set_var("__DENV_SHELL", "bash");
    }

    let data_dir = std::env::var("DENV_DATA_DIR")
        .or_else(|_| std::env::var("XDG_DATA_HOME").map(|d| format!("{d}/denv")))
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}/.local/share/denv")
        });
    unsafe {
        std::env::set_var("__DENV_SENTINEL", format!("{data_dir}/active_{pid}"));
    }

    // Initial export deferred to first cd — avoids startup subprocess.
    true
}

/// Called after cd. Checks fast-path, runs export if needed.
/// Returns true if PATH was modified.
pub fn on_cd() -> bool {
    if fast_path_ok() {
        return false;
    }
    run_export()
}

/// Handle `denv allow|deny|reload`. Returns Some(path_modified) if handled.
pub fn command(args: &[String]) -> Option<bool> {
    match args.first().map(|s| s.as_str()) {
        Some("allow" | "deny" | "reload") => Some(run_denv_and_apply(args)),
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
    if pwd != dir && !pwd.starts_with(&format!("{dir}/")) {
        return false;
    }

    // Sentinel file must exist
    if !Path::new(&sentinel).exists() {
        return false;
    }

    let sentinel_mt = file_mtime(&sentinel);
    let envrc_path = format!("{dir}/.envrc");
    let dotenv_path = format!("{dir}/.env");

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

fn run_export() -> bool {
    run_denv_and_apply(&["export".to_string(), "bash".to_string()])
}

/// Run `denv <args>`, capture stdout, apply export/unset lines.
/// Returns true if PATH was modified.
///
/// Uses fork+exec directly (not std::process::Command) because the shell's
/// signal handlers and process group setup require explicit cleanup in the child.
fn run_denv_and_apply(args: &[String]) -> bool {
    let old_path = std::env::var("PATH").ok();

    // Build command string for /bin/sh -c
    let cmd = format!("denv {}", args.join(" "));

    // Create pipe for capturing stdout
    let (pipe_r, pipe_w) = match crate::sys::pipe_cloexec() {
        Ok(fds) => fds,
        Err(_) => return false,
    };

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(pipe_r);
            libc::close(pipe_w);
        }
        return false;
    }

    if pid == 0 {
        // Child: redirect stdout to pipe, stdin from /dev/null, reset signals, exec
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
    unsafe { libc::close(pipe_w) };

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
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }

    apply_bash_output(&output);
    std::env::var("PATH").ok() != old_path
}

/// Parse `export KEY='value';` and `unset KEY;` lines, apply to env.
fn apply_bash_output(output: &str) {
    for line in output.lines() {
        let line = line.trim_end_matches(';');
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some(eq) = rest.find('=') {
                let key = &rest[..eq];
                let value = unquote_bash(&rest[eq + 1..]);
                // SAFETY: single-threaded shell
                unsafe { std::env::set_var(key, &value) };
            }
        } else if let Some(key) = line.strip_prefix("unset ") {
            unsafe { std::env::remove_var(key.trim()) };
        }
    }
}

/// Unquote a bash single-quoted string: `'value'` → `value`, `'\''` → `'`.
fn unquote_bash(s: &str) -> String {
    let s = s.strip_prefix('\'').unwrap_or(s);
    let s = s.strip_suffix('\'').unwrap_or(s);
    s.replace("'\\''", "'")
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
    fn apply_export_sets_var() {
        apply_bash_output("export _DENV_TEST_A='hello_world';");
        assert_eq!(std::env::var("_DENV_TEST_A").unwrap(), "hello_world");
        unsafe { std::env::remove_var("_DENV_TEST_A") };
    }

    #[test]
    fn apply_unset_removes_var() {
        unsafe { std::env::set_var("_DENV_TEST_B", "exists") };
        apply_bash_output("unset _DENV_TEST_B;");
        assert!(std::env::var("_DENV_TEST_B").is_err());
    }

    #[test]
    fn apply_multi_line() {
        apply_bash_output(
            "export _DENV_TEST_C='one';\nexport _DENV_TEST_D='two';\nunset _DENV_TEST_E;",
        );
        assert_eq!(std::env::var("_DENV_TEST_C").unwrap(), "one");
        assert_eq!(std::env::var("_DENV_TEST_D").unwrap(), "two");
        assert!(std::env::var("_DENV_TEST_E").is_err());
        unsafe {
            std::env::remove_var("_DENV_TEST_C");
            std::env::remove_var("_DENV_TEST_D");
        }
    }

    #[test]
    fn apply_value_with_embedded_quote() {
        apply_bash_output("export _DENV_TEST_F='it'\\''s here';");
        assert_eq!(std::env::var("_DENV_TEST_F").unwrap(), "it's here");
        unsafe { std::env::remove_var("_DENV_TEST_F") };
    }

    #[test]
    fn apply_empty_value() {
        apply_bash_output("export _DENV_TEST_G='';");
        assert_eq!(std::env::var("_DENV_TEST_G").unwrap(), "");
        unsafe { std::env::remove_var("_DENV_TEST_G") };
    }

    #[test]
    fn fast_path_fails_without_state() {
        // No __DENV_STATE set → fast path should fail
        unsafe { std::env::remove_var("__DENV_STATE") };
        assert!(!fast_path_ok());
    }
}
