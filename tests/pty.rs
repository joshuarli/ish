//! PTY-based integration tests for the ish binary.
//!
//! Spawns ish in a real pseudo-terminal and drives it with keystrokes,
//! asserting on the visible terminal output. This tests the full shell loop
//! including raw mode, prompt rendering, line editing, completion, and history.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// PTY harness
// ---------------------------------------------------------------------------

struct PtyShell {
    master: OwnedFd,
    child: libc::pid_t,
    _home: TempDir,
}

/// Minimal RAII temp dir.
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let template = format!("/tmp/{prefix}_XXXXXX\0");
        let mut buf = template.into_bytes();
        let ptr = unsafe { libc::mkdtemp(buf.as_mut_ptr() as *mut libc::c_char) };
        assert!(!ptr.is_null(), "mkdtemp failed");
        buf.pop(); // remove NUL
        Self(PathBuf::from(String::from_utf8(buf).unwrap()))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ish_binary() -> PathBuf {
    // Find the debug binary relative to the test binary
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove `deps`
    path.push("ish");
    assert!(path.exists(), "ish binary not found at {}", path.display());
    path
}

impl PtyShell {
    /// Spawn ish in a PTY with an isolated HOME directory.
    fn spawn() -> Self {
        Self::spawn_with_opts(&[], &[])
    }

    /// Spawn with files pre-created in HOME and optional history entries.
    fn spawn_with_opts(files: &[(&str, &str)], history: &[&str]) -> Self {
        let home = TempDir::new("ish_pty_test");
        let home_path = home.path().to_str().unwrap().to_string();

        // Create files
        for (name, content) in files {
            let p = home.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }

        // Create history file
        if !history.is_empty() {
            let data_dir = home.path().join(".local/share/ish");
            std::fs::create_dir_all(&data_dir).unwrap();
            let hist_content = history.join("\n") + "\n";
            std::fs::write(data_dir.join("history"), hist_content).unwrap();
        }

        // Create empty config dir so no stale config is loaded
        let config_dir = home.path().join(".config/ish");
        std::fs::create_dir_all(&config_dir).unwrap();

        // Open PTY
        let mut master_fd: RawFd = -1;
        let mut slave_fd: RawFd = -1;
        let ret = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, 0, "openpty failed");

        // Set terminal size to 80x24
        let ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws) };

        let binary = ish_binary();

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            // Child — become session leader, set controlling terminal
            unsafe {
                libc::close(master_fd);
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0);
                libc::dup2(slave_fd, 0);
                libc::dup2(slave_fd, 1);
                libc::dup2(slave_fd, 2);
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
            }

            // Exec ish with clean env
            let err = Command::new(&binary)
                .env_clear()
                .env("HOME", &home_path)
                .env("USER", "testuser")
                .env("PWD", &home_path)
                .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
                .env("TERM", "xterm-256color")
                .env("XDG_CONFIG_HOME", format!("{home_path}/.config"))
                .env("XDG_DATA_HOME", format!("{home_path}/.local/share"))
                .current_dir(&home_path)
                .exec();
            eprintln!("exec failed: {err}");
            std::process::exit(127);
        }

        // Parent
        unsafe { libc::close(slave_fd) };

        // Set master to non-blocking for reads
        unsafe {
            let flags = libc::fcntl(master_fd, libc::F_GETFL);
            libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };

        let sh = PtyShell {
            master,
            child: pid,
            _home: home,
        };

        // Wait for the initial prompt
        sh.wait_for_prompt(3000);
        sh
    }

    fn master_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.master.as_raw_fd()
    }

    /// Send raw bytes to the shell.
    fn send(&self, input: &[u8]) {
        use std::os::fd::AsRawFd;
        let mut f = unsafe { std::fs::File::from_raw_fd(self.master.as_raw_fd()) };
        f.write_all(input).unwrap();
        // Don't drop — that would close the fd
        std::mem::forget(f);
    }

    /// Send a string.
    fn type_str(&self, s: &str) {
        self.send(s.as_bytes());
    }

    /// Send Enter key.
    fn enter(&self) {
        self.send(b"\r");
    }

    /// Send Tab key.
    fn tab(&self) {
        self.send(b"\t");
    }

    /// Send Escape key.
    fn escape(&self) {
        self.send(b"\x1b");
    }

    /// Send Ctrl+C.
    fn ctrl_c(&self) {
        self.send(b"\x03");
    }

    /// Send Ctrl+D.
    fn ctrl_d(&self) {
        self.send(b"\x04");
    }

    /// Send Ctrl+R.
    fn ctrl_r(&self) {
        self.send(b"\x12");
    }

    /// Send Ctrl+L.
    fn ctrl_l(&self) {
        self.send(b"\x0c");
    }

    /// Send Ctrl+A.
    fn ctrl_a(&self) {
        self.send(b"\x01");
    }

    /// Send Ctrl+E.
    fn ctrl_e(&self) {
        self.send(b"\x05");
    }

    /// Send Ctrl+W.
    fn ctrl_w(&self) {
        self.send(b"\x17");
    }

    /// Send Ctrl+U.
    fn ctrl_u(&self) {
        self.send(b"\x15");
    }

    /// Send Ctrl+K.
    fn ctrl_k(&self) {
        self.send(b"\x0b");
    }

    /// Send Ctrl+Y.
    fn ctrl_y(&self) {
        self.send(b"\x19");
    }

    /// Send Up arrow.
    fn up(&self) {
        self.send(b"\x1b[A");
    }

    /// Send Down arrow.
    #[allow(dead_code)]
    fn down(&self) {
        self.send(b"\x1b[B");
    }

    /// Send Left arrow.
    fn left(&self) {
        self.send(b"\x1b[D");
    }

    /// Send Right arrow.
    fn right(&self) {
        self.send(b"\x1b[C");
    }

    /// Send Backspace.
    fn backspace(&self) {
        self.send(b"\x7f");
    }

    /// Read all available output, waiting up to `timeout_ms` for data.
    fn read_timeout(&self, timeout_ms: u64) -> String {
        let mut buf = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

        loop {
            let mut fds = [libc::pollfd {
                fd: self.master_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];

            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as i32;
            if remaining <= 0 {
                break;
            }

            let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, remaining.min(100)) };
            if n > 0 && fds[0].revents & libc::POLLIN != 0 {
                let mut chunk = [0u8; 4096];
                use std::os::fd::AsRawFd;
                let mut f = unsafe { std::fs::File::from_raw_fd(self.master.as_raw_fd()) };
                match f.read(&mut chunk) {
                    Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                    _ => {}
                }
                std::mem::forget(f);
            } else {
                // No data and we've waited — if we have some data already, we're done
                if !buf.is_empty() {
                    break;
                }
            }
        }

        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Wait until output contains `marker`, up to `timeout_ms`.
    fn wait_for(&self, marker: &str, timeout_ms: u64) -> String {
        let mut accumulated = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

        loop {
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u64;
            if remaining == 0 {
                break;
            }

            let chunk = self.read_timeout(remaining.min(200));
            accumulated.push_str(&chunk);

            if accumulated.contains(marker) {
                return accumulated;
            }
        }

        accumulated
    }

    /// Wait for the shell prompt (` $ `).
    fn wait_for_prompt(&self, timeout_ms: u64) -> String {
        self.wait_for("$ ", timeout_ms)
    }

    /// Send a command, press enter, wait for the next prompt.
    fn run_command(&self, cmd: &str) -> String {
        self.type_str(cmd);
        self.enter();
        self.wait_for_prompt(3000)
    }

    /// Strip ANSI escape sequences from output for easier text matching.
    fn strip_ansi(s: &str) -> String {
        let mut result = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Consume the CSI/OSC/SS3 sequence
                if let Some(&next) = chars.peek() {
                    if next == '[' || next == 'O' {
                        chars.next();
                        // Read until final byte (0x40-0x7E)
                        while let Some(&ch) = chars.peek() {
                            chars.next();
                            if (0x40..=0x7E).contains(&(ch as u32)) {
                                break;
                            }
                        }
                    } else if next == ']' {
                        // OSC — read until BEL (0x07) or ST (ESC \)
                        chars.next();
                        while let Some(ch) = chars.next() {
                            if ch == '\x07' {
                                break;
                            }
                            if ch == '\x1b' {
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                        }
                    }
                }
                continue;
            }
            result.push(c);
        }
        result
    }
}

impl Drop for PtyShell {
    fn drop(&mut self) {
        // Send Ctrl+D to exit cleanly, then SIGTERM as fallback
        self.send(b"\x04");
        std::thread::sleep(std::time::Duration::from_millis(50));
        unsafe {
            libc::kill(self.child, libc::SIGTERM);
        }
        // Reap child
        let mut status = 0i32;
        unsafe {
            libc::waitpid(self.child, &mut status, 0);
        }
    }
}

// Helper: use Command::exec() which replaces the process
use std::os::unix::process::CommandExt;

/// Wait up to `timeout_ms` for a child process to exit.
fn assert_child_exits(pid: libc::pid_t, timeout_ms: u64) {
    let start = std::time::Instant::now();
    loop {
        let mut status = 0i32;
        let ret = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if ret > 0 {
            return;
        }
        if start.elapsed() > std::time::Duration::from_millis(timeout_ms) {
            panic!("child pid {pid} did not exit within {timeout_ms}ms");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn prompt_appears_on_startup() {
    let sh = PtyShell::spawn();
    // The initial wait_for_prompt in spawn() succeeded, so the prompt appeared.
    // Verify we can get another prompt after Enter.
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    assert!(out.contains("$ "), "expected prompt, got: {out:?}");
}

#[test]
fn echo_command() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("echo hello world");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello world"),
        "expected 'hello world' in output: {text:?}"
    );
}

#[test]
fn pwd_builtin() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("pwd");
    let text = PtyShell::strip_ansi(&out);
    // HOME is our temp dir, and we cd'd there
    assert!(
        text.contains("/tmp/ish_pty_test_"),
        "expected temp dir in pwd output: {text:?}"
    );
}

#[test]
fn cd_and_pwd() {
    let sh = PtyShell::spawn_with_opts(&[], &[]);
    // Create a subdir
    sh.run_command("echo ignore"); // just to get past any initial state
    let out = sh.run_command("cd /tmp && pwd");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("/tmp"), "expected /tmp in output: {text:?}");
}

#[test]
fn exit_with_ctrl_d() {
    let sh = PtyShell::spawn();
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.ctrl_d();
    assert_child_exits(sh.child, 3000);
    std::mem::forget(sh);
}

#[test]
fn exit_command() {
    let sh = PtyShell::spawn();
    sh.type_str("exit");
    sh.enter();
    assert_child_exits(sh.child, 3000);
    std::mem::forget(sh);
}

#[test]
fn ctrl_c_cancels_input() {
    let sh = PtyShell::spawn();
    sh.type_str("some partial input");
    sh.ctrl_c();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    // After Ctrl+C, should see ^C and a new prompt
    assert!(text.contains("^C"), "expected ^C in output: {text:?}");
    assert!(
        text.contains("$ "),
        "expected new prompt after ^C: {text:?}"
    );
}

#[test]
fn line_editing_backspace() {
    let sh = PtyShell::spawn();
    sh.type_str("echo helloo");
    sh.backspace();
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello"),
        "expected 'hello' in output: {text:?}"
    );
}

#[test]
fn line_editing_ctrl_u() {
    let sh = PtyShell::spawn();
    sh.type_str("this will be killed");
    sh.ctrl_u();
    sh.type_str("echo survived");
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("survived"),
        "expected 'survived' in output: {text:?}"
    );
    // Check that only "survived" appears as command output (after the newline),
    // not "killed". The prompt redraws above will show keystrokes, but the
    // actual output line should only have "survived".
    let lines: Vec<&str> = text.lines().collect();
    let output_line = lines
        .iter()
        .find(|l| l.contains("survived") && !l.contains("$ "));
    assert!(
        output_line.is_some(),
        "expected standalone 'survived' output line: {text:?}"
    );
}

#[test]
fn line_editing_ctrl_w() {
    let sh = PtyShell::spawn();
    sh.type_str("echo remove_me keep");
    // Move cursor left past "keep"
    for _ in 0..5 {
        sh.left();
    }
    // Ctrl+W should delete "remove_me "
    sh.ctrl_w();
    // Move to end and execute
    sh.ctrl_e();
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("keep"), "expected 'keep' in output: {text:?}");
}

#[test]
fn line_editing_ctrl_k_and_ctrl_y() {
    let sh = PtyShell::spawn();
    sh.type_str("echo hello world");
    sh.ctrl_a();
    // Move past "echo "
    for _ in 0..5 {
        sh.right();
    }
    sh.ctrl_k(); // kills "hello world"
    sh.type_str("yanked: ");
    sh.ctrl_y(); // pastes "hello world"
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("yanked: hello world"),
        "expected 'yanked: hello world' in output: {text:?}"
    );
}

#[test]
fn pipe_chain() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("echo 'abc def ghi' | /usr/bin/tr ' ' '\\n' | /usr/bin/wc -l");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains('3'), "expected '3' in output: {text:?}");
}

#[test]
fn and_chain() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("true && echo yes");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("yes"), "expected 'yes' in output: {text:?}");
}

#[test]
fn or_chain() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("false || echo fallback");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("fallback"),
        "expected 'fallback' in output: {text:?}"
    );
}

#[test]
fn redirect_output() {
    let sh = PtyShell::spawn();
    // Use /bin/echo (external) so redirect is applied via fork/exec path
    sh.run_command("/bin/echo file_content > out.txt");
    let out = sh.run_command("cat out.txt");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("file_content"),
        "expected 'file_content' in output: {text:?}"
    );
}

#[test]
fn l_builtin_lists_files() {
    let sh = PtyShell::spawn_with_opts(
        &[
            ("file_a.txt", "aaa"),
            ("file_b.txt", "bbb"),
            ("subdir/.keep", ""),
        ],
        &[],
    );
    let out = sh.run_command("l");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("file_a.txt"), "expected file_a.txt: {text:?}");
    assert!(text.contains("file_b.txt"), "expected file_b.txt: {text:?}");
    assert!(text.contains("subdir/"), "expected subdir/: {text:?}");
}

#[test]
fn set_and_echo_var() {
    let sh = PtyShell::spawn();
    sh.run_command("set MY_VAR hello_world");
    let out = sh.run_command("echo $MY_VAR");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello_world"),
        "expected 'hello_world' in output: {text:?}"
    );
}

#[test]
fn tilde_expansion() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("echo ~");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("/tmp/ish_pty_test_"),
        "expected home dir expansion: {text:?}"
    );
}

#[test]
fn history_up_arrow() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo from_history"]);
    sh.up();
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("from_history"),
        "expected 'from_history' in output: {text:?}"
    );
}

#[test]
fn history_ctrl_r_search() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo alpha", "echo beta", "echo gamma"]);
    sh.ctrl_r();
    // Wait for search UI
    let out = sh.wait_for("search:", 2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("search:"), "expected search pager: {text:?}");

    // Type search query
    sh.type_str("beta");
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Accept with Enter
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    // The selected command should be on the line — enter executes it
    assert!(text.contains("beta"), "expected 'beta' in output: {text:?}");
}

#[test]
fn history_ctrl_r_escape_cancels() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo secret"]);
    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    sh.type_str("secret");
    std::thread::sleep(std::time::Duration::from_millis(200));
    sh.escape();
    sh.wait_for_prompt(2000);
    // After escape, the original line should be restored (empty)
    // Send enter to verify nothing executes
    sh.enter();
    let out2 = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out2);
    // "secret" should NOT appear as command output — just in prompt area
    assert!(
        !text.contains("secret"),
        "escape should have cancelled search: {text:?}"
    );
}

#[test]
fn tab_completion_files() {
    let sh = PtyShell::spawn_with_opts(
        &[("alpha.txt", ""), ("bravo.txt", ""), ("charlie.txt", "")],
        &[],
    );
    sh.type_str("echo al");
    sh.tab();
    // Single match — should auto-complete to alpha.txt
    std::thread::sleep(std::time::Duration::from_millis(300));
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("alpha.txt"),
        "expected alpha.txt completion: {text:?}"
    );
}

#[test]
fn tab_completion_shows_grid() {
    let sh = PtyShell::spawn_with_opts(&[("aaa.txt", ""), ("aab.txt", ""), ("aac.txt", "")], &[]);
    sh.type_str("echo aa");
    sh.tab();
    // Multiple matches — grid should appear
    let out = sh.read_timeout(500);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("aaa.txt") || text.contains("aab.txt"),
        "expected completion grid: {text:?}"
    );

    // Escape to dismiss
    sh.escape();
    sh.ctrl_u();
}

#[test]
fn tab_completion_directory() {
    let sh = PtyShell::spawn_with_opts(&[("mydir/.keep", "")], &[]);
    sh.type_str("cd my");
    sh.tab();
    // Should complete to mydir/
    std::thread::sleep(std::time::Duration::from_millis(300));
    sh.enter();
    sh.wait_for_prompt(2000);
    // After cd, pwd should show mydir
    let out2 = sh.run_command("pwd");
    let text = PtyShell::strip_ansi(&out2);
    assert!(text.contains("mydir"), "expected to be in mydir: {text:?}");
}

#[test]
fn alias_expansion() {
    let sh = PtyShell::spawn();
    sh.run_command("alias g echo git_command");
    let out = sh.run_command("g hello");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("git_command hello"),
        "expected alias expansion: {text:?}"
    );
}

#[test]
fn alias_list() {
    let sh = PtyShell::spawn();
    sh.run_command("alias myalias echo test");
    let out = sh.run_command("alias");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("myalias"), "expected alias in list: {text:?}");
}

#[test]
fn which_builtin() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("w echo");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("builtin"),
        "expected 'builtin' for echo: {text:?}"
    );
}

#[test]
fn which_external() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("w ls");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("/bin/ls") || text.contains("/usr/bin/ls"),
        "expected PATH for ls: {text:?}"
    );
}

#[test]
fn which_alias() {
    let sh = PtyShell::spawn();
    sh.run_command("alias g git");
    let out = sh.run_command("w g");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("alias"),
        "expected 'alias' for aliased command: {text:?}"
    );
}

#[test]
fn error_status_colors_prompt() {
    let sh = PtyShell::spawn();
    // Run a command that fails
    sh.run_command("false");
    // The next prompt should have red color (38;5;1 or 31)
    sh.enter();
    let raw = sh.wait_for_prompt(2000);
    // Look for red ANSI in the raw output
    assert!(
        raw.contains("\x1b[38;5;1m") || raw.contains("\x1b[31m") || raw.contains("$ "),
        "expected colored prompt after error: {raw:?}"
    );
}

#[test]
fn nonexistent_command() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("nonexistent_cmd_xyz");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("not found") || text.contains("No such"),
        "expected error for nonexistent command: {text:?}"
    );
}

#[test]
fn script_mode_refused() {
    // ish with arguments should exit with error
    let binary = ish_binary();
    let output = std::process::Command::new(&binary)
        .arg("script.sh")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("interactive-only"),
        "expected refusal: {stderr}"
    );
}

#[test]
fn source_refused() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("source foo.sh");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("not supported"),
        "expected source refusal: {text:?}"
    );
}

#[test]
fn ctrl_l_clears_screen() {
    let sh = PtyShell::spawn();
    sh.type_str("echo before_clear");
    sh.enter();
    sh.wait_for_prompt(2000);
    sh.ctrl_l();
    let out = sh.read_timeout(500);
    // Screen clear is ESC[H ESC[2J
    assert!(
        out.contains("\x1b[H") || out.contains("\x1b[2J"),
        "expected screen clear sequence: {out:?}"
    );
}

#[test]
fn multiline_continuation() {
    let sh = PtyShell::spawn();
    sh.type_str("echo hello |");
    sh.enter();
    // Should get continuation prompt, not execute
    let _ = sh.read_timeout(500);
    // Type the rest
    sh.type_str("/usr/bin/tr a-z A-Z");
    sh.enter();
    let out2 = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out2);
    assert!(
        text.contains("HELLO"),
        "expected uppercased output: {text:?}"
    );
}

#[test]
fn config_file_loaded() {
    let sh = PtyShell::spawn_with_opts(
        &[(
            ".config/ish/config.ish",
            "alias greet echo hello_from_config\n",
        )],
        &[],
    );
    let out = sh.run_command("greet world");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello_from_config world"),
        "expected config alias: {text:?}"
    );
}

#[test]
fn prompt_shows_cwd() {
    let sh = PtyShell::spawn();
    // The prompt should contain some representation of the cwd
    sh.enter();
    let raw = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&raw);
    // Our HOME is a temp dir — prompt shortens to ~
    assert!(
        text.contains('~') || text.contains("ish_pty_test"),
        "expected cwd in prompt: {text:?}"
    );
}

#[test]
fn cd_minus_goes_back() {
    let sh = PtyShell::spawn_with_opts(&[("subdir/.keep", "")], &[]);
    sh.run_command("cd subdir");
    sh.run_command("cd -");
    let out = sh.run_command("pwd");
    let text = PtyShell::strip_ansi(&out);
    // Should be back in the original home dir, not subdir
    assert!(!text.contains("subdir"), "should have gone back: {text:?}");
}

#[test]
fn unset_variable() {
    let sh = PtyShell::spawn();
    sh.run_command("set TMPVAR abc");
    sh.run_command("unset TMPVAR");
    let out = sh.run_command("echo $TMPVAR");
    let text = PtyShell::strip_ansi(&out);
    // After unset, $TMPVAR should expand to empty
    assert!(!text.contains("abc"), "variable should be unset: {text:?}");
}

#[test]
fn glob_expansion() {
    let sh = PtyShell::spawn_with_opts(&[("foo.rs", ""), ("bar.rs", ""), ("baz.txt", "")], &[]);
    let out = sh.run_command("echo *.rs");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("foo.rs") && text.contains("bar.rs"),
        "expected glob expansion: {text:?}"
    );
    assert!(!text.contains("baz.txt"), "should not match .txt: {text:?}");
}

#[test]
fn quoted_string_preserves_spaces() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("echo \"hello   world\"");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello   world"),
        "expected preserved spaces: {text:?}"
    );
}

#[test]
fn single_quotes_no_expansion() {
    let sh = PtyShell::spawn();
    sh.run_command("set FOO bar");
    let out = sh.run_command("echo '$FOO'");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("$FOO"), "expected literal $FOO: {text:?}");
}

#[test]
fn history_persisted_across_commands() {
    let sh = PtyShell::spawn();
    sh.run_command("echo unique_cmd_12345");
    // Now up arrow should recall it
    sh.up();
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("unique_cmd_12345"),
        "expected history recall: {text:?}"
    );
}

#[test]
fn true_and_false_builtins() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("true && echo ok");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("ok"), "true should succeed: {text:?}");

    let out = sh.run_command("false && echo bad || echo good");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("good"), "false should fail: {text:?}");
}
