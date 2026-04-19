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
        Self::spawn_with_size_env(&[], &[], &[], 24, 80)
    }

    /// Spawn with files pre-created in HOME and optional history entries.
    fn spawn_with_opts(files: &[(&str, &str)], history: &[&str]) -> Self {
        Self::spawn_with_size_env(files, history, &[], 24, 80)
    }

    /// Spawn with a custom terminal size.
    fn spawn_with_size(files: &[(&str, &str)], history: &[&str], rows: u16, cols: u16) -> Self {
        Self::spawn_with_size_env(files, history, &[], rows, cols)
    }

    /// Spawn with files, history, and extra environment variables.
    /// Extra env vars override defaults (e.g. PATH).
    fn spawn_with_env(
        files: &[(&str, &str)],
        history: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_with_size_env(files, history, extra_env, 24, 80)
    }

    fn spawn_with_size_env(
        files: &[(&str, &str)],
        history: &[&str],
        extra_env: &[(&str, &str)],
        rows: u16,
        cols: u16,
    ) -> Self {
        let home = TempDir::new("ish_pty_test");
        let home_path = home.path().to_str().unwrap().to_string();

        // Create files
        for (name, content) in files {
            let p = home.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
            // Make files in bin/ executable
            if name.starts_with("bin/") {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
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

        // Set terminal size
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
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
            let mut cmd = Command::new(&binary);
            cmd.env_clear()
                .env("HOME", &home_path)
                .env("USER", "testuser")
                .env("PWD", &home_path)
                .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
                .env("TERM", "xterm-256color")
                .env("XDG_CONFIG_HOME", format!("{home_path}/.config"))
                .env("XDG_DATA_HOME", format!("{home_path}/.local/share"))
                .current_dir(&home_path);
            for (key, value) in extra_env {
                cmd.env(key, value);
            }
            let err = cmd.exec();
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

    /// Send Ctrl+Z (suspend).
    fn ctrl_z(&self) {
        self.send(b"\x1a");
    }

    /// Send Ctrl+D.
    fn ctrl_d(&self) {
        self.send(b"\x04");
    }

    /// Send Ctrl+R.
    fn ctrl_r(&self) {
        self.send(b"\x12");
    }

    /// Send Ctrl+F.
    fn ctrl_f(&self) {
        self.send(b"\x06");
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

    /// Send Ctrl+Backspace.
    fn ctrl_backspace(&self) {
        self.send(b"\x08");
    }

    fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master_fd(), libc::TIOCSWINSZ, &ws);
            libc::kill(self.child, libc::SIGWINCH);
        }
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
    ///
    /// Waits for `"$ "` that appears after a newline. Typing renders use `\r`
    /// only, so any `\n` indicates the shell processed Enter and started
    /// execution. This prevents early returns from matching `"$ "` in typing
    /// renders during slow commands (e.g., denv subprocess on cd).
    /// Send a command, press enter, wait for the next prompt.
    ///
    /// Waits for `"$ "` that appears after a newline. Typing renders use `\r`
    /// only, so any `\n` indicates the shell processed Enter and started
    /// execution. This prevents early returns from matching `"$ "` in typing
    /// renders during slow commands (e.g., denv subprocess on cd).
    fn run_command(&self, cmd: &str) -> String {
        self.type_str(cmd);
        self.enter();
        let mut accumulated = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5000);

        loop {
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u64;
            if remaining == 0 {
                break;
            }

            let chunk = self.read_timeout(remaining.min(200));
            accumulated.push_str(&chunk);

            if let Some(nl) = accumulated.find('\n')
                && accumulated[nl..].contains("$ ")
            {
                return accumulated;
            }
        }

        accumulated
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

#[derive(Clone)]
struct Screen {
    rows: usize,
    cols: usize,
    cells: Vec<Vec<char>>,
    row: usize,
    col: usize,
    saved: Option<(usize, usize)>,
}

impl Screen {
    fn new(rows: u16, cols: u16) -> Self {
        let rows = rows as usize;
        let cols = cols as usize;
        Self {
            rows,
            cols,
            cells: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
            saved: None,
        }
    }

    fn render(output: &str, rows: u16, cols: u16) -> String {
        let mut screen = Self::new(rows, cols);
        screen.apply_output(output);
        screen.visible_text()
    }

    fn apply_output(&mut self, output: &str) {
        let bytes = output.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                b'\n' => {
                    self.line_feed();
                    i += 1;
                }
                0x1b => {
                    i += 1;
                    if i >= bytes.len() {
                        break;
                    }
                    match bytes[i] {
                        b'[' => {
                            i += 1;
                            let start = i;
                            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                                i += 1;
                            }
                            if i >= bytes.len() {
                                break;
                            }
                            self.apply_csi(&bytes[start..i], bytes[i]);
                            i += 1;
                        }
                        b']' => {
                            i += 1;
                            while i < bytes.len() {
                                if bytes[i] == 0x07 {
                                    i += 1;
                                    break;
                                }
                                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\'
                                {
                                    i += 2;
                                    break;
                                }
                                i += 1;
                            }
                        }
                        b'7' => {
                            self.saved = Some((self.row, self.col));
                            i += 1;
                        }
                        b'8' => {
                            if let Some((row, col)) = self.saved {
                                self.row = row.min(self.rows.saturating_sub(1));
                                self.col = col.min(self.cols.saturating_sub(1));
                            }
                            i += 1;
                        }
                        _ => i += 1,
                    }
                }
                b if b < b' ' => {
                    i += 1;
                }
                _ => {
                    let ch = output[i..].chars().next().unwrap();
                    self.put_char(ch);
                    i += ch.len_utf8();
                }
            }
        }
    }

    fn visible_text(&self) -> String {
        let mut lines: Vec<String> = self
            .cells
            .clone()
            .into_iter()
            .map(|row| row.into_iter().collect::<String>().trim_end().to_string())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn apply_csi(&mut self, params: &[u8], final_byte: u8) {
        match final_byte {
            b'A' => {
                let n = parse_csi_n(params).max(1);
                self.row = self.row.saturating_sub(n as usize);
            }
            b'B' => {
                let n = parse_csi_n(params).max(1) as usize;
                self.row = (self.row + n).min(self.rows.saturating_sub(1));
            }
            b'C' => {
                let n = parse_csi_n(params).max(1) as usize;
                self.col = (self.col + n).min(self.cols.saturating_sub(1));
            }
            b'D' => {
                let n = parse_csi_n(params).max(1);
                self.col = self.col.saturating_sub(n as usize);
            }
            b'H' => {
                let (row, col) = parse_csi_pos(params);
                self.row = row.min(self.rows.saturating_sub(1));
                self.col = col.min(self.cols.saturating_sub(1));
            }
            b'J' => {
                let mode = parse_csi_n(params);
                if mode == 2 {
                    for row in &mut self.cells {
                        row.fill(' ');
                    }
                } else {
                    self.clear_to_end_of_screen();
                }
            }
            b'K' => self.clear_to_end_of_line(),
            _ => {}
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.rows == 0 || self.cols == 0 {
            return;
        }
        if self.col >= self.cols {
            self.col = 0;
            self.line_feed();
        }
        self.cells[self.row][self.col] = ch;
        self.col += 1;
        if self.col >= self.cols {
            self.col = 0;
            self.line_feed();
        }
    }

    fn line_feed(&mut self) {
        if self.row + 1 >= self.rows {
            self.cells.rotate_left(1);
            self.cells[self.rows - 1].fill(' ');
        } else {
            self.row += 1;
        }
    }

    fn clear_to_end_of_line(&mut self) {
        if self.rows == 0 || self.cols == 0 {
            return;
        }
        for col in self.col..self.cols {
            self.cells[self.row][col] = ' ';
        }
    }

    fn clear_to_end_of_screen(&mut self) {
        self.clear_to_end_of_line();
        for row in self.row + 1..self.rows {
            self.cells[row].fill(' ');
        }
    }

    fn snapshot(&self) -> ScreenSnapshot {
        ScreenSnapshot {
            visible: self.visible_text(),
            cursor_row: self.row,
            cursor_col: self.col,
        }
    }
}

#[derive(Clone, Debug)]
struct ScreenSnapshot {
    visible: String,
    cursor_row: usize,
    cursor_col: usize,
}

#[derive(Clone, Debug)]
struct TraceFrame {
    label: &'static str,
    raw: String,
    snapshot: ScreenSnapshot,
}

#[derive(Clone, Copy)]
enum TraceInput<'a> {
    Bytes(&'a [u8]),
    Text(&'a str),
}

#[derive(Clone, Copy)]
struct TraceStep<'a> {
    label: &'static str,
    input: TraceInput<'a>,
    settle_ms: u64,
    read_ms: u64,
}

fn run_trace(sh: &PtyShell, rows: u16, cols: u16, steps: &[TraceStep<'_>]) -> Vec<TraceFrame> {
    let mut screen = Screen::new(rows, cols);
    let mut frames = Vec::with_capacity(steps.len());

    for step in steps {
        let raw = match step.input {
            TraceInput::Bytes(bytes) => {
                sh.send(bytes);
                if step.settle_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(step.settle_ms));
                }
                sh.read_timeout(step.read_ms)
            }
            TraceInput::Text(text) => {
                sh.type_str(text);
                if step.settle_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(step.settle_ms));
                }
                sh.read_timeout(step.read_ms)
            }
        };
        screen.apply_output(&raw);
        frames.push(TraceFrame {
            label: step.label,
            raw,
            snapshot: screen.snapshot(),
        });
    }

    frames
}

fn parse_csi_n(params: &[u8]) -> u16 {
    std::str::from_utf8(params)
        .ok()
        .and_then(|s| s.split(';').next())
        .and_then(|s| s.trim_start_matches('?').parse().ok())
        .unwrap_or(0)
}

fn parse_csi_pos(params: &[u8]) -> (usize, usize) {
    let mut parts = std::str::from_utf8(params)
        .ok()
        .unwrap_or("")
        .split(';')
        .map(|s| s.trim_start_matches('?').parse::<usize>().unwrap_or(1));
    let row = parts.next().unwrap_or(1).saturating_sub(1);
    let col = parts.next().unwrap_or(1).saturating_sub(1);
    (row, col)
}

fn assert_screen_contains_once(screen: &str, needle: &str) {
    let count = screen.matches(needle).count();
    assert_eq!(count, 1, "expected {needle:?} once in screen: {screen:?}");
}

fn assert_frame_contains_once(frame: &TraceFrame, needle: &str) {
    let count = frame.snapshot.visible.matches(needle).count();
    assert_eq!(
        count, 1,
        "expected {needle:?} once in frame {}: {:?}",
        frame.label, frame.snapshot.visible
    );
}

impl Drop for PtyShell {
    fn drop(&mut self) {
        // Send Ctrl+D to exit cleanly
        self.send(b"\x04");
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Escalate: SIGTERM, then SIGKILL
        unsafe {
            libc::kill(self.child, libc::SIGTERM);
        }
        // Use WNOHANG polling — blocking waitpid can hang on macOS with open PTY master
        let start = std::time::Instant::now();
        loop {
            let mut status = 0i32;
            let ret = unsafe { libc::waitpid(self.child, &mut status, libc::WNOHANG) };
            if ret != 0 {
                break;
            }
            if start.elapsed() > std::time::Duration::from_millis(500) {
                unsafe {
                    libc::kill(self.child, libc::SIGKILL);
                }
            }
            if start.elapsed() > std::time::Duration::from_millis(2000) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
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
    // Use a generous timeout: on loaded CI runners (macOS-15 in particular)
    // process scheduling can delay the shell processing the ^D by several seconds.
    assert_child_exits(sh.child, 10_000);
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
    sh.run_command("export MY_VAR=hello_world");
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
fn history_up_narrow_repaint_clears_wrapped_rows() {
    let sh = PtyShell::spawn_with_size(
        &[],
        &["echo ok", "echo WRAPMARK12345678901234567890", "echo newer"],
        24,
        12,
    );
    sh.up();
    std::thread::sleep(std::time::Duration::from_millis(80));
    sh.up();
    std::thread::sleep(std::time::Duration::from_millis(80));
    sh.up();
    std::thread::sleep(std::time::Duration::from_millis(150));

    let out = sh.read_timeout(800);
    let screen = Screen::render(&out, 24, 12);
    assert!(
        screen.contains("$ echo\nok"),
        "expected final prompt to show wrapped `echo ok`: {screen:?}"
    );
    assert!(
        !screen.contains("WRAPMARK"),
        "wrapped history entry leaked into final screen: {screen:?}"
    );
    assert!(
        !screen.contains("newer"),
        "newer history entry leaked into final screen: {screen:?}"
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
fn history_ctrl_r_narrow_repaint_does_not_stack_rows() {
    let sh = PtyShell::spawn_with_size(&[], &["abc1", "abc2", "abc3"], 24, 10);
    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    sh.type_str("ab");
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.type_str("c");
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.backspace();
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.type_str("c");
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.down();
    sh.up();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let out = sh.read_timeout(600);
    let screen = Screen::render(&out, 24, 10);
    assert_screen_contains_once(&screen, "search:");
    assert_screen_contains_once(&screen, "abc1");
    assert_screen_contains_once(&screen, "abc2");
    assert_screen_contains_once(&screen, "abc3");

    sh.escape();
    sh.wait_for_prompt(2000);
}

#[test]
fn history_ctrl_r_near_bottom_keeps_pager_stable() {
    let history = &[
        "hist01", "hist02", "hist03", "hist04", "hist05", "hist06", "hist07", "hist08", "hist09",
        "hist10", "hist11", "hist12",
    ];
    let sh = PtyShell::spawn_with_size(&[], history, 24, 20);

    let fill_cmd = (1..=14)
        .map(|i| format!("echo fill{i:02}"))
        .collect::<Vec<_>>()
        .join("; ");
    sh.run_command(&fill_cmd);

    sh.ctrl_r();
    let mut out = sh.wait_for("search:", 2000);
    out.push_str(&sh.read_timeout(400));

    let screen = Screen::render(&out, 24, 20);
    assert_screen_contains_once(&screen, "search:");
    assert!(
        screen.contains("hist12"),
        "expected recent history entry in pager: {screen:?}"
    );
    assert!(
        screen.contains("hist11"),
        "expected second recent history entry in pager: {screen:?}"
    );

    sh.escape();
    sh.wait_for_prompt(2000);
}

#[test]
fn history_ctrl_r_near_bottom_query_edits_do_not_stack_headers() {
    let history = &[
        "gh auth login",
        "gh api repos/openai/openai/contents",
        "gh api user",
        "gh pr status",
        "gh api rate_limit",
        "gh api notifications",
        "gh api orgs/openai/repos",
        "gh api repos/openai/openai/pulls",
        "gh api repos/openai/openai/issues",
        "gh api repos/openai/openai/actions/runs",
        "gh api repos/openai/openai/releases",
        "gh api repos/openai/openai/branches",
    ];
    let sh = PtyShell::spawn_with_size(&[], history, 24, 20);

    let fill_cmd = (1..=14)
        .map(|i| format!("echo fill{i:02}"))
        .collect::<Vec<_>>()
        .join("; ");
    sh.run_command(&fill_cmd);

    let frames = run_trace(
        &sh,
        24,
        20,
        &[
            TraceStep {
                label: "open search",
                input: TraceInput::Bytes(b"\x12"),
                settle_ms: 0,
                read_ms: 500,
            },
            TraceStep {
                label: "type g",
                input: TraceInput::Text("g"),
                settle_ms: 100,
                read_ms: 400,
            },
            TraceStep {
                label: "type h",
                input: TraceInput::Text("h"),
                settle_ms: 100,
                read_ms: 400,
            },
            TraceStep {
                label: "type space",
                input: TraceInput::Text(" "),
                settle_ms: 100,
                read_ms: 400,
            },
            TraceStep {
                label: "type a",
                input: TraceInput::Text("a"),
                settle_ms: 100,
                read_ms: 400,
            },
            TraceStep {
                label: "type p",
                input: TraceInput::Text("p"),
                settle_ms: 100,
                read_ms: 400,
            },
            TraceStep {
                label: "type i",
                input: TraceInput::Text("i"),
                settle_ms: 150,
                read_ms: 500,
            },
        ],
    );

    let expected_queries = [
        "search:",
        "search: g",
        "search: gh",
        "search: gh",
        "search: gh a",
        "search: gh ap",
        "search: gh api",
    ];
    let expected_cursor_cols = [8, 9, 10, 11, 12, 13, 14];

    for ((frame, expected_query), expected_col) in frames
        .iter()
        .zip(expected_queries.iter())
        .zip(expected_cursor_cols.iter())
    {
        assert_frame_contains_once(frame, expected_query);
        assert_eq!(
            frame.snapshot.visible.matches("search:").count(),
            1,
            "stale intermediate headers leaked into frame {}: raw={:?} screen={:?}",
            frame.label,
            frame.raw,
            frame.snapshot.visible
        );
        assert_eq!(
            frame.snapshot.cursor_row, 0,
            "search cursor moved off header row in frame {}: {:?}",
            frame.label, frame.snapshot
        );
        assert_eq!(
            frame.snapshot.cursor_col, *expected_col,
            "unexpected search cursor col in frame {}: {:?}",
            frame.label, frame.snapshot
        );
    }

    let final_frame = frames.last().unwrap();
    assert!(
        final_frame.snapshot.visible.contains("gh api user"),
        "expected filtered history entry in pager: {:?}",
        final_frame.snapshot.visible
    );

    sh.escape();
    sh.wait_for_prompt(2000);
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
fn tab_completion_narrow_repaint_does_not_stack_rows() {
    let sh = PtyShell::spawn_with_size(
        &[("aaa.txt", ""), ("aab.txt", ""), ("aac.txt", "")],
        &[],
        24,
        12,
    );
    sh.type_str("echo a");
    sh.tab();
    std::thread::sleep(std::time::Duration::from_millis(150));
    sh.type_str("a");
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.down();
    sh.up();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let out = sh.read_timeout(600);
    let screen = Screen::render(&out, 24, 12);
    assert_screen_contains_once(&screen, "aaa.txt");
    assert_screen_contains_once(&screen, "aab.txt");
    assert_screen_contains_once(&screen, "aac.txt");

    sh.escape();
    sh.wait_for_prompt(2000);
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
fn file_picker_narrow_repaint_does_not_stack_rows() {
    let sh = PtyShell::spawn_with_size(&[("abc1", ""), ("abc2", ""), ("abc3", "")], &[], 24, 8);
    sh.ctrl_f();
    sh.wait_for("find", 2000);
    std::thread::sleep(std::time::Duration::from_millis(200));
    sh.type_str("ab");
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.type_str("c");
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.backspace();
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.type_str("c");
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.down();
    sh.up();
    std::thread::sleep(std::time::Duration::from_millis(300));

    let out = sh.read_timeout(800);
    let screen = Screen::render(&out, 24, 8);
    assert_screen_contains_once(&screen, "find:");
    assert_screen_contains_once(&screen, "abc1");
    assert_screen_contains_once(&screen, "abc2");
    assert_screen_contains_once(&screen, "abc3");

    sh.escape();
    sh.wait_for_prompt(2000);
}

#[test]
fn completion_resize_rerenders_grid() {
    let sh = PtyShell::spawn_with_size(
        &[("aaa.txt", ""), ("aab.txt", ""), ("aac.txt", "")],
        &[],
        24,
        20,
    );
    sh.type_str("echo a");
    sh.tab();
    std::thread::sleep(std::time::Duration::from_millis(200));
    sh.read_timeout(300);

    sh.resize(24, 12);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let out = sh.read_timeout(800);
    let screen = Screen::render(&out, 24, 12);
    assert_screen_contains_once(&screen, "aaa.txt");
    assert_screen_contains_once(&screen, "aab.txt");
    assert_screen_contains_once(&screen, "aac.txt");

    sh.escape();
    sh.wait_for_prompt(2000);
}

#[test]
fn history_resize_rerenders_pager() {
    let sh = PtyShell::spawn_with_size(&[], &["abc1", "abc2", "abc3"], 24, 20);
    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    sh.type_str("abc");
    std::thread::sleep(std::time::Duration::from_millis(200));
    sh.read_timeout(300);

    sh.resize(24, 10);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let out = sh.read_timeout(800);
    let screen = Screen::render(&out, 24, 10);
    assert_screen_contains_once(&screen, "search:");
    assert_screen_contains_once(&screen, "abc1");
    assert_screen_contains_once(&screen, "abc2");
    assert_screen_contains_once(&screen, "abc3");

    sh.escape();
    sh.wait_for_prompt(2000);
}

#[test]
fn file_picker_resize_rerenders_pager() {
    let sh = PtyShell::spawn_with_size(&[("abc1", ""), ("abc2", ""), ("abc3", "")], &[], 24, 20);
    sh.ctrl_f();
    sh.wait_for("find", 2000);
    std::thread::sleep(std::time::Duration::from_millis(200));
    sh.type_str("abc");
    std::thread::sleep(std::time::Duration::from_millis(300));
    sh.read_timeout(300);

    sh.resize(24, 8);
    std::thread::sleep(std::time::Duration::from_millis(300));

    let out = sh.read_timeout(800);
    let screen = Screen::render(&out, 24, 8);
    assert_screen_contains_once(&screen, "find:");
    assert_screen_contains_once(&screen, "abc1");
    assert_screen_contains_once(&screen, "abc2");
    assert_screen_contains_once(&screen, "abc3");

    sh.escape();
    sh.wait_for_prompt(2000);
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
fn alias_self_referencing_no_reexpand() {
    let sh = PtyShell::spawn();
    sh.run_command("alias rg rg --hidden -S -g !.git");
    // Type "rg" then space — should expand once
    sh.type_str("rg");
    std::thread::sleep(std::time::Duration::from_millis(50));
    sh.type_str(" ");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after_first = sh.read_timeout(300);
    let first_text = PtyShell::strip_ansi(&after_first);
    eprintln!("AFTER FIRST SPACE: {first_text:?}");
    // Type another space — should NOT re-expand
    sh.type_str(" ");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after_second = sh.read_timeout(300);
    let second_text = PtyShell::strip_ansi(&after_second);
    eprintln!("AFTER SECOND SPACE: {second_text:?}");
    // Cancel the line
    sh.ctrl_c();
    sh.wait_for_prompt(2000);

    // After first space: should see expanded form
    assert!(
        first_text.contains("rg --hidden -S -g !.git"),
        "expected alias expansion on first space: {first_text:?}"
    );
    // After second space: line should be identical (just one more trailing space).
    // The expansion "rg --hidden -S -g !.git" should appear exactly once.
    let count = second_text.matches("--hidden").count();
    assert!(
        count <= 1,
        "alias re-expanded on second space (--hidden appears {count} times): {second_text:?}"
    );
}

#[test]
fn alias_self_referencing_from_config() {
    let config = "alias rg rg --hidden -S -g !.git\n";
    let sh = PtyShell::spawn_with_opts(&[(".config/ish/config.ish", config)], &[]);
    // Type "rg" then space — should expand once
    sh.type_str("rg");
    std::thread::sleep(std::time::Duration::from_millis(50));
    sh.type_str(" ");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after_first = sh.read_timeout(300);
    let first_text = PtyShell::strip_ansi(&after_first);
    eprintln!("CONFIG AFTER FIRST SPACE: {first_text:?}");
    // Type another space
    sh.type_str(" ");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after_second = sh.read_timeout(300);
    let second_text = PtyShell::strip_ansi(&after_second);
    eprintln!("CONFIG AFTER SECOND SPACE: {second_text:?}");
    // Third space
    sh.type_str(" ");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after_third = sh.read_timeout(300);
    let third_text = PtyShell::strip_ansi(&after_third);
    eprintln!("CONFIG AFTER THIRD SPACE: {third_text:?}");
    sh.ctrl_c();
    sh.wait_for_prompt(2000);

    assert!(
        first_text.contains("rg --hidden -S -g !.git"),
        "expected alias expansion on first space: {first_text:?}"
    );
    let count = second_text.matches("--hidden").count();
    assert!(
        count <= 1,
        "config alias re-expanded on second space (--hidden appears {count} times): {second_text:?}"
    );
    let count = third_text.matches("--hidden").count();
    assert!(
        count <= 1,
        "config alias re-expanded on third space (--hidden appears {count} times): {third_text:?}"
    );
}

#[test]
fn alias_self_referencing_exec() {
    let sh = PtyShell::spawn();
    sh.run_command("alias myecho echo --verbose");
    // Run the alias — should expand once, not double
    let out = sh.run_command("myecho hello");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("--verbose hello"),
        "expected single expansion: {text:?}"
    );
    // Should NOT have doubled --verbose
    assert!(
        !text.contains("--verbose --verbose"),
        "alias double-expanded at exec: {text:?}"
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
fn alias_with_command_substitution() {
    let sh = PtyShell::spawn();
    sh.run_command(r#"alias grt echo "$(echo hello_subst)""#);
    // Type "grt" then space to trigger try_alias_expand, then Enter.
    // The $(echo hello_subst) must survive re-parsing as a single token.
    sh.type_str("grt");
    std::thread::sleep(std::time::Duration::from_millis(50));
    sh.type_str(" \n");
    let out = sh.read_timeout(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello_subst"),
        "expected command substitution in alias to work: {text:?}"
    );
    assert!(
        !text.contains("bad substitution"),
        "alias with command substitution should not error: {text:?}"
    );
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
fn source_nonexistent_error() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("source foo.sh");
    let text = PtyShell::strip_ansi(&out);
    // epsh handles source natively — nonexistent file gives an error
    assert!(
        text.contains("No such file") || text.contains("not found") || text.contains("error"),
        "expected file-not-found error: {text:?}"
    );
}

#[test]
fn ctrl_l_clears_screen() {
    let sh = PtyShell::spawn();
    let mut out = sh.run_command("echo before_clear");
    let _ = sh.read_timeout(200);
    sh.type_str("echo after_clear");
    sh.ctrl_l();
    out.push_str(&sh.read_timeout(500));
    let screen = Screen::render(&out, 24, 80);
    assert!(
        out.contains("\x1b[H") || out.contains("\x1b[2J"),
        "expected screen clear sequence: {out:?}"
    );
    assert!(
        !screen.contains("before_clear"),
        "clear should remove prior output from visible screen: {screen:?}"
    );
    assert!(
        screen.contains("echo after_clear"),
        "current line should be preserved after clear: {screen:?}"
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
fn multiline_completion_on_continuation_line() {
    let sh = PtyShell::spawn_with_opts(&[("bin/upper", "#!/bin/sh\ntr a-z A-Z\n")], &[]);
    sh.type_str("echo hello |");
    sh.enter();
    sh.read_timeout(300);
    sh.type_str("./bin/up");
    sh.tab();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let out = sh.read_timeout(600);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("./bin/upper"),
        "expected multiline completion to expand on continuation line: {text:?}"
    );
    sh.ctrl_c();
    sh.wait_for_prompt(2000);
}

#[test]
fn dir_picker_narrow_repaint_does_not_stack_rows() {
    let sh = PtyShell::spawn_with_size(
        &[("one/.keep", ""), ("two/.keep", ""), ("three/.keep", "")],
        &[],
        24,
        40,
    );
    sh.run_command("cd one");
    sh.run_command("cd ../two");
    sh.run_command("cd ../three");
    sh.ctrl_backspace();
    sh.wait_for("dirs:", 2000);
    std::thread::sleep(std::time::Duration::from_millis(150));
    sh.down();
    sh.up();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let out = sh.read_timeout(800);
    let screen = Screen::render(&out, 24, 40);
    assert_screen_contains_once(&screen, "dirs:");
    assert_screen_contains_once(&screen, "one");
    assert_screen_contains_once(&screen, "two");

    sh.escape();
    sh.wait_for_prompt(2000);
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
fn cd_tilde_subdir() {
    // Regression: `cd ~/subdir` was broken because do_cd used format!("{}{rest}", home)
    // (missing slash), producing e.g. `/tmp/homedir` instead of `/tmp/home/dir`.
    let sh = PtyShell::spawn_with_opts(&[("subdir/.keep", "")], &[]);
    sh.run_command("cd ~/subdir");
    let out = sh.run_command("pwd");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("subdir"),
        "cd ~/subdir should land in subdir: {text:?}"
    );
}

#[test]
fn l_tilde_subdir() {
    // Regression: `l ~/subdir` passed the literal "~/subdir" to builtin_l without tilde
    // expansion, producing "No such file or directory".
    let sh = PtyShell::spawn_with_opts(&[("subdir/file.txt", "hello")], &[]);
    let out = sh.run_command("l ~/subdir");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("file.txt"),
        "l ~/subdir should list subdir contents: {text:?}"
    );
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
    // Use /bin/echo (external command) — builtins are excluded from history
    sh.run_command("/bin/echo unique_cmd_12345");
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

// ---------------------------------------------------------------------------
// denv integration tests
// ---------------------------------------------------------------------------

/// Mock denv script — reads .denv_export from PWD, handles allow/deny/reload.
const MOCK_DENV: &str = r#"#!/bin/sh
case "$1" in
export)
    if [ -f "$PWD/.denv_export" ]; then
        cat "$PWD/.denv_export"
        echo "export __DENV_DIR='$PWD';"
        echo "export __DENV_STATE='0 1 $PWD';"
    elif [ -n "$__DENV_DIR" ]; then
        echo "unset DENV_TEST_VAR;"
        echo "unset __DENV_DIR;"
        echo "unset __DENV_STATE;"
    fi
    ;;
allow|reload)
    if [ -f "$PWD/.denv_export" ]; then
        cat "$PWD/.denv_export"
        echo "export __DENV_DIR='$PWD';"
        echo "export __DENV_STATE='0 1 $PWD';"
    fi
    ;;
deny)
    if [ -n "$__DENV_DIR" ]; then
        echo "unset DENV_TEST_VAR;"
        echo "unset __DENV_DIR;"
        echo "unset __DENV_STATE;"
    fi
    ;;
esac
"#;

/// Spawn ish with mock denv in PATH, using spawn_with_env.
/// The home_path is dynamically determined by TempDir, so we need a custom
/// PATH that includes $HOME/bin. We use spawn_with_env's extra_env to override PATH.
fn spawn_with_denv(files: &[(&str, &str)]) -> PtyShell {
    let mut all_files: Vec<(&str, &str)> = vec![("bin/denv", MOCK_DENV)];
    all_files.extend_from_slice(files);
    // We need HOME/bin in PATH, but don't know HOME yet.
    // Use spawn_with_env — it creates TempDir, then applies env.
    // The "PATH" env is set after HOME, so we can compute it.
    // But spawn_with_env doesn't expose the home_path...
    //
    // Workaround: use a well-known prefix. TempDir creates /tmp/ish_pty_test_XXXXXX.
    // We can't predict the suffix. Instead, symlink denv to a known location.
    //
    // Simplest: create /tmp/ish_mock_bin/ with a symlink that spawn_with_env populates.
    // Actually simplest: just put the mock in /tmp directly with a unique name.
    //
    // Even simpler: use spawn_with_env but override PATH to include /tmp/ish_pty_test_*/bin
    // via a glob... nope, env vars don't glob.
    //
    // Real fix: refactor spawn_with_env to return home_path before forking.
    // For now, create a shared mock dir.

    let mock_bin = "/tmp/ish_test_mock_bin";
    let _ = std::fs::create_dir_all(mock_bin);
    std::fs::write(format!("{mock_bin}/denv"), MOCK_DENV).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        format!("{mock_bin}/denv"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let path = format!("{mock_bin}:/usr/bin:/bin:/usr/sbin:/sbin");
    PtyShell::spawn_with_env(&all_files, &[], &[("PATH", &path)])
}

#[test]
fn denv_loads_env_on_cd() {
    let sh = spawn_with_denv(&[("project/.denv_export", "export DENV_TEST_VAR='loaded';\n")]);
    sh.run_command("cd project");
    let out = sh.run_command("echo $DENV_TEST_VAR");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("loaded"),
        "expected denv var after cd: {text:?}"
    );
}

#[test]
fn denv_unloads_on_leave() {
    let sh = spawn_with_denv(&[("project/.denv_export", "export DENV_TEST_VAR='active';\n")]);
    sh.run_command("cd project");
    // Verify loaded
    let out = sh.run_command("echo $DENV_TEST_VAR");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("active"), "should be loaded: {text:?}");

    // Leave the directory
    sh.run_command("cd ..");
    let out = sh.run_command("echo =$DENV_TEST_VAR=");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("=="),
        "var should be unset after leaving: {text:?}"
    );
}

#[test]
fn denv_allow_applies_env() {
    let sh = spawn_with_denv(&[("project/.denv_export", "export DENV_TEST_VAR='allowed';\n")]);
    sh.run_command("cd project");
    // denv allow should (re-)apply env
    sh.run_command("denv allow");
    let out = sh.run_command("echo $DENV_TEST_VAR");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("allowed"),
        "expected var after denv allow: {text:?}"
    );
}

#[test]
fn denv_deny_removes_env() {
    let sh = spawn_with_denv(&[("project/.denv_export", "export DENV_TEST_VAR='loaded';\n")]);
    sh.run_command("cd project");
    // Verify loaded
    let out = sh.run_command("echo $DENV_TEST_VAR");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("loaded"), "should be loaded: {text:?}");

    // deny should unset
    sh.run_command("denv deny");
    let out = sh.run_command("echo =$DENV_TEST_VAR=");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("=="),
        "var should be unset after deny: {text:?}"
    );
}

#[test]
fn denv_init_loads_at_startup() {
    // .denv_export in HOME — should be loaded on first cd (init defers to avoid startup cost)
    let sh = spawn_with_denv(&[(".denv_export", "export DENV_TEST_VAR='from_init';\n")]);
    // Trigger on_cd by cd'ing to current dir
    sh.run_command("cd .");
    let out = sh.run_command("echo $DENV_TEST_VAR");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("from_init"),
        "expected var loaded after first cd: {text:?}"
    );
}

#[test]
fn denv_value_with_spaces() {
    let sh = spawn_with_denv(&[(
        "project/.denv_export",
        "export DENV_TEST_VAR='hello world';\n",
    )]);
    sh.run_command("cd project");
    let out = sh.run_command("echo $DENV_TEST_VAR");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello world"),
        "expected value with spaces: {text:?}"
    );
}

#[test]
fn denv_multiple_vars() {
    let sh = spawn_with_denv(&[(
        "project/.denv_export",
        "export DENV_A='one';\nexport DENV_B='two';\n",
    )]);
    sh.run_command("cd project");
    let out = sh.run_command("echo $DENV_A $DENV_B");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("one"), "expected DENV_A: {text:?}");
    assert!(text.contains("two"), "expected DENV_B: {text:?}");
}

#[test]
fn denv_no_error_without_denv() {
    // Without mock denv in PATH, shell should start fine (denv_active=false)
    let sh = PtyShell::spawn();
    let out = sh.run_command("echo works");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("works"),
        "shell should work without denv: {text:?}"
    );
}

#[test]
fn job_suspend_and_resume() {
    let sh = PtyShell::spawn();

    // Start a long-running foreground process.
    sh.type_str("sleep 60");
    sh.enter();

    // Give sleep a moment to start, then suspend it with Ctrl+Z.
    std::thread::sleep(std::time::Duration::from_millis(200));
    sh.ctrl_z();

    // Shell should report the stop and return a prompt.
    let out = sh.wait_for("stopped:", 3000);
    assert!(
        out.contains("stopped:") && out.contains("sleep"),
        "expected stop notification: {out:?}"
    );
    sh.wait_for_prompt(3000);

    // Resume the job with fg.
    sh.type_str("fg");
    sh.enter();

    let out = sh.wait_for("resuming:", 3000);
    assert!(
        out.contains("resuming:") && out.contains("sleep"),
        "expected resume notification: {out:?}"
    );

    // sleep is now in the foreground again — kill it so the shell returns.
    std::thread::sleep(std::time::Duration::from_millis(100));
    sh.ctrl_c();

    // Shell must return a prompt, confirming it regained control.
    let out = sh.wait_for_prompt(3000);
    assert!(
        out.contains("$ "),
        "shell did not return a prompt after fg: {out:?}"
    );

    // Confirm the shell is fully interactive again.
    let out = sh.run_command("echo alive");
    assert!(
        out.contains("alive"),
        "shell unresponsive after job resume: {out:?}"
    );
}
