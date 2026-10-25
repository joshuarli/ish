//! PTY-based integration tests for the ish binary.
//!
//! Spawns ish in a real pseudo-terminal and drives it with keystrokes,
//! asserting on the visible terminal output. This tests the full shell loop
//! including raw mode, prompt rendering, line editing, completion, and history.
//! Screen assertions use `ptytest`'s independent terminal state, so terminal
//! behavior is checked without a second parser in this consumer.

use ish::history::History;
use ptytest::{
    CommandSpec, ExitStatus, ProtocolProfile, PtyTest, Scenario, Size, TerminalBaseline, TestEnv,
};
use std::os::unix::ffi::OsStringExt;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

mod common;
use common::ish_binary;

// ---------------------------------------------------------------------------
// PTY harness
// ---------------------------------------------------------------------------

struct PtyShell {
    terminal: RefCell<PtyTest>,
    terminal_baseline: TerminalBaseline,
    _home: Rc<TempDir>,
    startup_output: String,
    pending_output: RefCell<Vec<u8>>,
    output_offset: Cell<usize>,
}

struct TempDir(PathBuf);

const READY_SLEEP_SCRIPT: &str = "#!/bin/sh\nprintf 'pty-child-ready\\n'\nexec /bin/sleep 60\n";

impl TempDir {
    fn new(prefix: &str) -> Self {
        Self(common::scratch_dir(prefix))
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

impl PtyShell {
    /// Spawn ish in a PTY with an isolated HOME directory.
    fn spawn() -> Self {
        Self::spawn_with_setup(&[], &[], &[], 24, 80, None, |_| {})
    }

    /// Spawn with files pre-created in HOME and optional history entries.
    fn spawn_with_opts(files: &[(&str, &str)], history: &[&str]) -> Self {
        Self::spawn_with_setup(files, history, &[], 24, 80, None, |_| {})
    }

    /// Spawn with a custom terminal size.
    fn spawn_with_size(files: &[(&str, &str)], history: &[&str], rows: u16, cols: u16) -> Self {
        Self::spawn_with_setup(files, history, &[], rows, cols, None, |_| {})
    }

    fn spawn_with_setup<F>(
        files: &[(&str, &str)],
        history: &[&str],
        extra_env: &[(&str, &str)],
        rows: u16,
        cols: u16,
        cwd_rel: Option<&str>,
        setup: F,
    ) -> Self
    where
        F: FnOnce(&Path),
    {
        let home = Self::build_home(files, history, setup);
        Self::spawn_in_home(home, extra_env, rows, cols, cwd_rel)
    }

    fn build_home<F>(files: &[(&str, &str)], history: &[&str], setup: F) -> Rc<TempDir>
    where
        F: FnOnce(&Path),
    {
        let home = Rc::new(TempDir::new("ish_pty_test"));

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

        setup(home.path());

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
        home
    }

    /// Spawn without terminal protocol validation, which rejects the OSC 52
    /// clipboard sequences these tests read from the raw output.
    fn spawn_reading_clipboard(files: &[(&str, &str)], rows: u16, cols: u16) -> Self {
        let home = Self::build_home(files, &[], |_| {});
        Self::spawn_in_home_with_profile(home, &[], rows, cols, None, ProtocolProfile::disabled())
    }

    fn spawn_in_home(
        home: Rc<TempDir>,
        extra_env: &[(&str, &str)],
        rows: u16,
        cols: u16,
        cwd_rel: Option<&str>,
    ) -> Self {
        Self::spawn_in_home_with_profile(
            home,
            extra_env,
            rows,
            cols,
            cwd_rel,
            ProtocolProfile::xterm_minimal_v1(),
        )
    }

    fn spawn_in_home_with_profile(
        home: Rc<TempDir>,
        extra_env: &[(&str, &str)],
        rows: u16,
        cols: u16,
        cwd_rel: Option<&str>,
        profile: ProtocolProfile,
    ) -> Self {
        let home_path = home.path().to_str().unwrap().to_string();
        let binary = ish_binary();
        let temp_path = home.path().join(".tmp");
        std::fs::create_dir_all(&temp_path).unwrap();
        let cwd = cwd_rel
            .map(|rel| home.path().join(rel))
            .unwrap_or_else(|| home.path().to_path_buf());
        let mut command = CommandSpec::new(binary)
            .current_dir(&cwd)
            .env("HOME", &home_path)
            .env("USER", "testuser")
            .env("PWD", &cwd)
            .env("TMPDIR", &temp_path)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        for (key, value) in extra_env {
            command = command.env(key, value);
        }
        let environment = if cfg!(target_os = "linux") {
            TestEnv::hermetic_ascii()
        } else {
            TestEnv::hermetic_utf8("C.UTF-8")
        }
        .expect("a supported hermetic locale must be available on PTY platforms")
        .env("HOME", &home_path)
        .env("USER", "testuser")
        .env("PWD", &cwd)
        .env("TMPDIR", &temp_path)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        let scenario = Scenario::new("ish interactive shell")
            .expect("valid scenario label")
            .command(command)
            .size(Size::new(cols, rows).expect("non-zero shell size"))
            .environment(environment)
            .protocol_profile(profile);
        let terminal = PtyTest::spawn(scenario).expect("spawn ish through ptytest");
        let terminal_baseline = terminal.terminal_baseline();
        let mut sh = PtyShell {
            terminal: RefCell::new(terminal),
            terminal_baseline,
            _home: home,
            startup_output: String::new(),
            pending_output: RefCell::new(Vec::new()),
            output_offset: Cell::new(0),
        };

        sh.startup_output = sh.wait_for("$ ", 3000);
        assert!(
            sh.startup_output.contains("$ "),
            "ish did not render its initial prompt within 3s: {:?}",
            sh.startup_output
        );
        sh
    }

    fn home_path(&self) -> &Path {
        self._home.path()
    }

    fn startup_output(&self) -> &str {
        &self.startup_output
    }

    /// Send raw bytes to the shell.
    fn send(&self, input: &[u8]) {
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_secs(5));
        terminal
            .send_bytes(deadline, input)
            .expect("PTY write failed");
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

    /// Send Ctrl+Delete as xterm `ESC [ 3 ; 5 ~`.
    fn ctrl_delete(&self) {
        self.send(b"\x1b[3;5~");
    }

    fn resize(&self, rows: u16, cols: u16) {
        self.terminal
            .borrow_mut()
            .resize(Size::new(cols, rows).expect("non-zero shell size"))
            .expect("resize shell PTY");
    }

    /// Read all available output, waiting up to `timeout_ms` for data.
    fn read_timeout(&self, timeout_ms: u64) -> String {
        let mut buf = std::mem::take(&mut *self.pending_output.borrow_mut());
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_millis(timeout_ms));
        terminal.drain(deadline).expect("drain shell output");
        let output_length = terminal.raw_output().len();
        if output_length == self.output_offset.get() {
            let _ = terminal
                .wait_for_output(deadline)
                .expect("wait for shell output");
        }
        let _ = terminal
            .wait_for_quiescence(deadline, std::time::Duration::from_millis(50))
            .expect("wait for shell output quiescence");
        terminal.drain(deadline).expect("drain shell output");
        let output = terminal.raw_output();
        let offset = self.output_offset.get();
        buf.extend_from_slice(output.get(offset..).unwrap_or_default());
        self.output_offset.set(output.len());

        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Wait for terminal output to become quiescent without using a generic
    /// sleep. The supplied bound is an observable terminal-I/O deadline.
    fn wait_for_quiescence(&self, timeout_ms: u64) {
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_millis(timeout_ms));
        terminal.drain(deadline).expect("drain shell output");
        let _ = terminal
            .wait_for_quiescence(deadline, std::time::Duration::from_millis(timeout_ms))
            .expect("wait for shell output quiescence");
    }

    fn drain_available_output(&self) {
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::ZERO);
        terminal.drain(deadline).expect("drain shell output");
    }

    fn screen(&self) -> ptytest::ScreenSnapshot {
        self.terminal.borrow().screen()
    }

    fn wait_for_screen<F>(
        &self,
        description: &str,
        timeout_ms: u64,
        predicate: F,
    ) -> ptytest::ScreenSnapshot
    where
        F: Fn(&ptytest::ScreenSnapshot) -> bool,
    {
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_millis(timeout_ms));
        let screen = terminal
            .wait_for_screen(deadline, description, predicate)
            .expect("wait for screen state");
        self.output_offset.set(terminal.raw_output().len());
        self.pending_output.borrow_mut().clear();
        screen
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

            if let Some(marker_start) = accumulated.find(marker) {
                let marker_end = marker_start + marker.len();
                let suffix = accumulated.split_off(marker_end);
                self.pending_output
                    .borrow_mut()
                    .extend_from_slice(suffix.as_bytes());
                accumulated.push_str(&suffix);
                return accumulated;
            }
        }

        panic!("timed out after {timeout_ms}ms waiting for {marker:?}; output: {accumulated:?}");
    }

    fn wait_for_prompt_after_line_advance(&self, timeout_ms: u64) -> String {
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

            if let Some(prompt_end) = prompt_end_after_last_newline(&accumulated) {
                return self.finish_at_prompt(accumulated, prompt_end);
            }
        }

        panic!("timed out after {timeout_ms}ms waiting for the shell prompt; output: {accumulated:?}");
    }

    /// Wait until Enter has been processed and the shell advanced to a new line.
    fn wait_for_line_advance(&self, timeout_ms: u64) -> String {
        self.wait_for("\n", timeout_ms)
    }

    fn run_command(&self, cmd: &str) -> String {
        self.type_str(cmd);
        self.enter();
        let mut accumulated = self.wait_for_line_advance(1000);
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

            if let Some(prompt_end) = prompt_end_after_last_newline(&accumulated) {
                return self.finish_at_prompt(accumulated, prompt_end);
            }
        }

        panic!("timed out waiting for the prompt after running {cmd:?}; output: {accumulated:?}");
    }

    fn finish_at_prompt(&self, mut output: String, prompt_end: usize) -> String {
        let suffix = output.split_off(prompt_end);
        self.pending_output
            .borrow_mut()
            .extend_from_slice(suffix.as_bytes());
        output
    }

    fn wait_for_exit(&self, timeout_ms: u64) {
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_millis(timeout_ms));
        let status = terminal.wait_for_exit(deadline).expect("wait for ish exit");
        if status == ExitStatus::Code(0) {
            terminal
                .assert_terminal_restored(&self.terminal_baseline)
                .expect("normal ish exit restores applicable terminal modes");
        }
    }

    fn wait_for_exit_status(&self, timeout_ms: u64) -> ExitStatus {
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_millis(timeout_ms));
        terminal.wait_for_exit(deadline).expect("wait for ish exit")
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
                    } else if next == '7' || next == '8' {
                        chars.next();
                    }
                }
                continue;
            }
            result.push(c);
        }
        result
    }
}

fn snapshot_text(screen: &ptytest::ScreenSnapshot) -> String {
    let mut lines = (0..screen.row_count())
        .map(|row| screen.row(row).unwrap_or_default().trim_end().to_owned())
        .collect::<Vec<_>>();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn active_prompt_text(screen: &ptytest::ScreenSnapshot) -> String {
    let cursor_row = usize::from(screen.cursor().row);
    let rows = (0..screen.row_count())
        .map(|row| screen.row(row).unwrap_or_default().trim_end().to_owned())
        .collect::<Vec<_>>();
    let first = (0..=cursor_row)
        .rev()
        .find(|&row| rows[row].contains("$ "))
        .unwrap_or_else(|| cursor_row.saturating_sub(4));
    let last = (cursor_row + 4).min(screen.row_count().saturating_sub(1));
    let mut active = String::new();
    for (row, text) in rows.iter().enumerate().take(last + 1).skip(first) {
        if row > first && screen.row_wrapped(row - 1) != Some(true) {
            active.push(' ');
        }
        active.push_str(text);
    }
    active
}

fn prompt_end_after_last_newline(output: &str) -> Option<usize> {
    let prompt_line = output.rfind('\n')? + 1;
    output[prompt_line..]
        .find("$ ")
        .map(|offset| prompt_line + offset + 2)
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
    run_trace_with_initial_output(sh, rows, cols, steps, "")
}

fn run_trace_with_initial_output(
    sh: &PtyShell,
    rows: u16,
    cols: u16,
    steps: &[TraceStep<'_>],
    _initial_output: &str,
) -> Vec<TraceFrame> {
    let mut frames = Vec::with_capacity(steps.len());

    for step in steps {
        let raw = match step.input {
            TraceInput::Bytes(bytes) => {
                sh.send(bytes);
                if step.settle_ms > 0 {
                    sh.wait_for_quiescence(step.settle_ms);
                }
                sh.read_timeout(step.read_ms)
            }
            TraceInput::Text(text) => {
                sh.type_str(text);
                if step.settle_ms > 0 {
                    sh.wait_for_quiescence(step.settle_ms);
                }
                sh.read_timeout(step.read_ms)
            }
        };
        let screen = sh.screen();
        assert_eq!(screen.size(), Size::new(cols, rows).unwrap());
        frames.push(TraceFrame {
            label: step.label,
            raw,
            snapshot: ScreenSnapshot {
                visible: snapshot_text(&screen),
                cursor_row: usize::from(screen.cursor().row),
                cursor_col: usize::from(screen.cursor().column),
            },
        });
    }

    frames
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

fn normalize_screen_text(screen: &str) -> String {
    screen.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact_screen_text(screen: &str) -> String {
    screen.chars().filter(|ch| !ch.is_whitespace()).collect()
}

impl Drop for PtyShell {
    fn drop(&mut self) {
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_secs(2));
        let _ = terminal.finish(deadline);
    }
}

fn assert_native_fs_status(sh: &PtyShell, expected: i32) {
    let output = clean(&sh.run_command("echo native-fs-status-$?"));
    assert!(
        output
            .lines()
            .any(|line| line.trim() == format!("native-fs-status-{expected}")),
        "expected native filesystem status {expected}: {output:?}"
    );
}

fn assert_native_fs_parent_refusal(output: &str) {
    let output = clean(output);
    assert!(
        output.contains("requires the original shell process"),
        "expected parent-process refusal diagnostic: {output:?}"
    );
}

#[cfg(target_os = "linux")]
fn native_fs_tree(home: &Path, name: &str, count: usize) -> PathBuf {
    let root = home.join(name);
    std::fs::create_dir(&root).unwrap();
    let contents = (0..4096).map(|index| index as u8).collect::<Vec<_>>();
    for index in 0..count {
        std::fs::write(root.join(format!("file-{index:05}")), &contents).unwrap();
    }
    root
}

fn native_fs_cancellable_tree(home: &Path, name: &str) -> PathBuf {
    let root = home.join(name);
    std::fs::create_dir(&root).unwrap();
    let contents = [0x4f; 512];
    for index in 0..2048 {
        std::fs::write(root.join(format!("file-{index:05}")), contents).unwrap();
    }
    let mut payload = std::fs::File::create(root.join("payload")).unwrap();
    let contents = vec![0x6d; 1024 * 1024];
    for _ in 0..8 {
        std::io::Write::write_all(&mut payload, &contents).unwrap();
    }
    root
}

fn native_fs_wait_for_staging(sh: &PtyShell, command: &str) -> PathBuf {
    sh.type_str(command);
    sh.enter();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        sh.drain_available_output();
        let mut stage = None;
        for session in std::fs::read_dir(sh.home_path()).unwrap().flatten() {
            if !session.file_name().to_string_lossy().starts_with(".fdu-quarantine-v2-") {
                continue;
            }
            for entry in std::fs::read_dir(session.path()).into_iter().flatten().flatten() {
                if entry.file_name().to_string_lossy().ends_with(".entry")
                    && entry.file_type().is_ok_and(|kind| kind.is_dir())
                    && std::fs::read_dir(entry.path()).is_ok_and(|mut entries| entries.next().is_some())
                {
                    stage = Some(entry.path());
                    break;
                }
            }
            if stage.is_some() {
                break;
            }
        }
        if let Some(stage) = stage {
            return stage;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native filesystem staging did not begin for {command:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn native_fs_copy_and_move_lists_keep_successes_and_continue_after_failures() {
    let sh = PtyShell::spawn_with_opts(
        &[
            ("first", "first bytes\0"),
            ("second", "second bytes\n"),
            ("third", "third bytes\0"),
        ],
        &[],
    );

    sh.run_command("cp first first-copy && mv second second-moved");
    assert_eq!(std::fs::read(sh.home_path().join("first-copy")).unwrap(), b"first bytes\0");
    assert_eq!(std::fs::read(sh.home_path().join("second-moved")).unwrap(), b"second bytes\n");
    assert!(!sh.home_path().join("second").exists());
    assert_native_fs_status(&sh, 0);

    sh.run_command("cp missing fallback-copy || mv third third-moved");
    assert!(!sh.home_path().join("fallback-copy").exists());
    assert_eq!(std::fs::read(sh.home_path().join("third-moved")).unwrap(), b"third bytes\0");
    assert_native_fs_status(&sh, 0);

    sh.run_command("mv missing failed-move && cp first skipped-copy");
    assert!(!sh.home_path().join("failed-move").exists());
    assert!(!sh.home_path().join("skipped-copy").exists());
    assert_native_fs_status(&sh, 1);
}

#[test]
fn native_fs_exact_targets_and_into_directories_preserve_real_bytes() {
    let sh = PtyShell::spawn_with_setup(&[], &[], &[], 24, 80, None, |home| {
        let bytes = [0, 255, 13, 10, 0, 42];
        for name in ["exact-source", "into-source", "move-source", "move-into-source"] {
            std::fs::write(home.join(name), bytes).unwrap();
        }
        std::fs::create_dir(home.join("copy-into")).unwrap();
        std::fs::create_dir(home.join("move-into")).unwrap();
    });

    sh.run_command("cp exact-source exact-target");
    sh.run_command("cp --into copy-into into-source");
    sh.run_command("mv move-source move-target");
    sh.run_command("mv --into move-into move-into-source");

    let bytes = [0, 255, 13, 10, 0, 42];
    for path in [
        sh.home_path().join("exact-target"),
        sh.home_path().join("copy-into/into-source"),
        sh.home_path().join("move-target"),
        sh.home_path().join("move-into/move-into-source"),
    ] {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    for path in ["move-source", "move-into-source"] {
        assert!(!sh.home_path().join(path).exists());
    }
}

#[test]
fn native_fs_redirections_prevent_failed_transfers_and_capture_stderr() {
    let sh = PtyShell::spawn_with_setup(&[], &[], &[], 24, 80, None, |home| {
        std::fs::write(home.join("source"), b"redirected bytes\0\xff").unwrap();
    });
    sh.run_command("cp source blocked > absent-directory/output");
    assert!(!sh.home_path().join("blocked").exists());
    assert_native_fs_status(&sh, 2);

    sh.run_command("cp missing never-created 2> copy-error");
    let error = std::fs::read_to_string(sh.home_path().join("copy-error")).unwrap();
    assert!(
        error.contains("No such file or directory"),
        "stderr redirection did not capture the diagnostic: {error:?}"
    );
    assert!(!sh.home_path().join("never-created").exists());
    assert_native_fs_status(&sh, 1);

    let output = sh.run_command("cp --verbose source verbose-copy 2> copy-verbose");
    let verbose = std::fs::read_to_string(sh.home_path().join("copy-verbose")).unwrap();
    assert!(verbose.contains("copied source to verbose-copy"), "verbose stderr was not redirected: {verbose:?}");
    assert!(!clean(&output).contains("ish: cp: copied source to verbose-copy"));
    assert_eq!(std::fs::read(sh.home_path().join("verbose-copy")).unwrap(), b"redirected bytes\0\xff");
    assert_native_fs_status(&sh, 0);
}

#[test]
fn native_fs_parent_placement_accepts_command_alias_expanded_name_and_argument_substitution() {
    let sh = PtyShell::spawn_with_opts(&[("source", "parent bytes\0")], &[]);

    sh.run_command("command cp \"$(echo source)\" command-copy");
    sh.run_command("alias native-copy cp");
    sh.run_command("native-copy source alias-copy");
    sh.run_command("export NATIVE_FS_COMMAND=cp");
    sh.run_command("$NATIVE_FS_COMMAND source expanded-copy");

    for path in ["command-copy", "alias-copy", "expanded-copy"] {
        assert_eq!(std::fs::read(sh.home_path().join(path)).unwrap(), b"parent bytes\0");
    }
    assert_native_fs_status(&sh, 0);
}

#[test]
fn native_fs_standalone_negation_runs_in_the_shell_process() {
    let sh = PtyShell::spawn_with_opts(&[("source", "negated copy bytes")], &[]);

    sh.run_command("! cp source negated-copy");
    assert_eq!(std::fs::read(sh.home_path().join("negated-copy")).unwrap(), b"negated copy bytes");
    assert_native_fs_status(&sh, 1);

    sh.run_command("! mv absent-source negated-move");
    assert!(!sh.home_path().join("negated-move").exists());
    assert_native_fs_status(&sh, 0);

    sh.run_command("! mv negated-copy negated-move-success");
    assert!(!sh.home_path().join("negated-copy").exists());
    assert_eq!(
        std::fs::read(sh.home_path().join("negated-move-success")).unwrap(),
        b"negated copy bytes"
    );
    assert_native_fs_status(&sh, 1);

    sh.run_command("export NATIVE_FS_COMMAND=cp");
    sh.run_command(
        "! command command $NATIVE_FS_COMMAND negated-move-success nested-negated-copy > nested-negated-redirection",
    );
    assert_eq!(
        std::fs::read(sh.home_path().join("nested-negated-copy")).unwrap(),
        b"negated copy bytes"
    );
    assert!(sh.home_path().join("nested-negated-redirection").exists());
    assert_native_fs_status(&sh, 1);

    sh.run_command("alias negated-copy-command cp");
    sh.run_command("! negated-copy-command source alias-negated-copy");
    assert_eq!(
        std::fs::read(sh.home_path().join("alias-negated-copy")).unwrap(),
        b"negated copy bytes"
    );
    assert_native_fs_status(&sh, 1);
}

#[test]
fn native_fs_static_dynamic_and_nested_fork_contexts_refuse_before_redirections() {
    let sh = PtyShell::spawn_with_opts(&[("source", "must stay untouched")], &[]);

    let output = sh.run_command("/bin/echo ignored | cp source static-copy > static-redirection");
    assert_native_fs_parent_refusal(&output);
    assert!(!sh.home_path().join("static-copy").exists());
    assert!(!sh.home_path().join("static-redirection").exists());
    assert_native_fs_status(&sh, 2);

    sh.run_command("export NATIVE_FS_COMMAND=cp");
    let output = sh.run_command("/bin/echo ignored | $NATIVE_FS_COMMAND source dynamic-copy > dynamic-redirection");
    assert_native_fs_parent_refusal(&output);
    assert!(!sh.home_path().join("dynamic-copy").exists());
    assert!(!sh.home_path().join("dynamic-redirection").exists());
    assert_native_fs_status(&sh, 2);

    let output = sh.run_command("echo \"$(cp source nested-copy > nested-redirection)\"");
    assert_native_fs_parent_refusal(&output);
    assert!(!sh.home_path().join("nested-copy").exists());
    assert!(!sh.home_path().join("nested-redirection").exists());
    assert_native_fs_status(&sh, 0);

    let output = sh.run_command("echo \"$(echo $(cp source deep-copy > deep-redirection))\"");
    assert_native_fs_parent_refusal(&output);
    assert!(!sh.home_path().join("deep-copy").exists());
    assert!(!sh.home_path().join("deep-redirection").exists());
    assert_native_fs_status(&sh, 0);
}

#[test]
fn native_fs_quick_no_progress_copy_stays_quiet() {
    let sh = PtyShell::spawn_with_opts(&[("source", "tiny bytes")], &[]);
    let output = clean(&sh.run_command("cp source copy"));
    assert!(!output.contains("files, "), "quick copy printed progress: {output:?}");
    assert_eq!(std::fs::read(sh.home_path().join("copy")).unwrap(), b"tiny bytes");

    let output = clean(&sh.run_command("cp --no-progress source quiet-copy"));
    assert!(!output.contains("files, "), "--no-progress printed progress: {output:?}");
    assert_eq!(std::fs::read(sh.home_path().join("quiet-copy")).unwrap(), b"tiny bytes");
}

#[test]
fn native_fs_ctrl_c_during_copy_returns_130_to_a_usable_prompt() {
    let sh = PtyShell::spawn_with_setup(&[], &[], &[], 24, 80, None, |home| {
        native_fs_cancellable_tree(home, "source-tree");
    });
    let modes = terminal_state(&sh).modes;
    let stage = native_fs_wait_for_staging(&sh, "cp -r --jobs 1 source-tree copy-tree");
    assert!(!sh.home_path().join("copy-tree").exists());
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(10000);
    assert_native_fs_status(&sh, 130);
    assert!(!stage.exists(), "cancelled copy left its staging tree");
    assert!(sh.home_path().join("source-tree/file-00000").exists());
    assert!(!sh.home_path().join("copy-tree").exists());
    assert!(
        std::fs::read_dir(sh.home_path().join(".tmp")).unwrap().next().is_none(),
        "cancelled copy left engine scratch entries"
    );
    assert_eq!(terminal_state(&sh).modes, modes);
    let output = clean(&sh.run_command("/bin/echo native-fs-alive"));
    assert!(output.contains("native-fs-alive"), "shell did not run a command after cancellation: {output:?}");
}

#[test]
fn native_fs_ctrl_z_is_ignored_during_copy_and_external_job_control_still_works() {
    let sh = PtyShell::spawn_with_setup(
        &[("bin/sleep", READY_SLEEP_SCRIPT)],
        &[],
        &[],
        24,
        80,
        None,
        |home| {
            native_fs_cancellable_tree(home, "source-tree");
        },
    );
    let modes = terminal_state(&sh).modes;
    let stage = native_fs_wait_for_staging(&sh, "cp -r --jobs 1 source-tree copy-tree");
    assert!(!sh.home_path().join("copy-tree").exists());
    sh.ctrl_z();
    let output = sh.wait_for_prompt_after_line_advance(15000);
    assert!(!output.contains("stopped:"), "native copy was suspended by Ctrl+Z: {output:?}");
    assert_native_fs_status(&sh, 0);
    assert!(!stage.exists(), "completed copy retained its staging name");
    assert_eq!(
        std::fs::read(sh.home_path().join("copy-tree/file-00000")).unwrap(),
        [0x4f; 512]
    );
    assert_eq!(terminal_state(&sh).modes, modes);

    sh.type_str("./bin/sleep");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();
    sh.wait_for("stopped:", 3000);
    sh.wait_for_prompt_after_line_advance(3000);
    sh.type_str("fg");
    sh.enter();
    sh.wait_for("resuming:", 3000);
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(3000);
}

#[test]
fn native_fs_sigterm_and_sighup_cancel_and_restore_the_terminal() {
    for (signal, status) in [(libc::SIGTERM, 143), (libc::SIGHUP, 129)] {
        let sh = PtyShell::spawn_with_setup(&[], &[], &[], 24, 80, None, |home| {
            native_fs_cancellable_tree(home, "source-tree");
        });
        let stage = native_fs_wait_for_staging(&sh, "cp -r --jobs 1 source-tree copy-tree");
        assert!(!sh.home_path().join("copy-tree").exists());
        let mut terminal = sh.terminal.borrow_mut();
        terminal.signal(signal).unwrap();
        let deadline = terminal.deadline(std::time::Duration::from_secs(10));
        assert_eq!(
            terminal.wait_for_exit(deadline).expect("native filesystem signal exits ish"),
            ExitStatus::Code(status)
        );
        terminal
            .assert_terminal_restored(&sh.terminal_baseline)
            .expect("native filesystem signal restores the terminal");
        drop(terminal);
        assert!(!stage.exists(), "signalled copy left its staging tree");
        assert!(sh.home_path().join("source-tree/file-00000").exists());
        assert!(!sh.home_path().join("copy-tree").exists());
        assert!(
            std::fs::read_dir(sh.home_path().join(".tmp")).unwrap().next().is_none(),
            "native filesystem signal left engine scratch entries"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_fs_broken_stderr_cancels_bounded_work_and_keeps_the_prompt_usable() {
    let sh = PtyShell::spawn_with_setup(&[], &[], &[], 24, 80, None, |home| {
        native_fs_tree(home, "source-tree", 1024);
        std::fs::create_dir(home.join("copy-into")).unwrap();
    });

    sh.run_command("cp --verbose --jobs 4 --into copy-into source-tree/* 2>/dev/full");
    assert_native_fs_status(&sh, 1);
    assert!(
        std::fs::read_dir(sh.home_path().join(".tmp")).unwrap().next().is_none(),
        "broken stderr left engine scratch entries"
    );
    let output = clean(&sh.run_command("/bin/echo native-fs-after-output-error"));
    assert!(output.contains("native-fs-after-output-error"), "shell did not return after broken stderr: {output:?}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn prompt_appears_on_startup() {
    let sh = PtyShell::spawn();
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
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
fn prompt_does_not_share_a_line_with_unterminated_external_output() {
    let sh = PtyShell::spawn();
    sh.run_command("/usr/bin/printf no-newline");
    let screen = snapshot_text(&sh.screen());

    assert!(
        screen.lines().any(|line| line == "no-newline"),
        "unterminated command output was not preserved: {screen:?}"
    );
    assert!(
        !screen.lines().any(|line| {
            line.find("no-newline")
                .zip(line.rfind('$'))
                .is_some_and(|(output, prompt)| output < prompt)
        }),
        "prompt was rendered on the unterminated output line: {screen:?}"
    );
}

#[test]
fn broken_interpreter_reports_bad_interpreter() {
    let sh = PtyShell::spawn_with_setup(&[], &[], &[], 24, 80, None, |home| {
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("badscript");
        let interp = bin.join("missing-interp");
        std::fs::write(&script, format!("#!{}\n", interp.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    });
    let out = sh.run_command("./bin/badscript");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("bad interpreter") && text.contains("missing-interp"),
        "expected bash-style bad interpreter message, got: {text:?}"
    );
}

#[test]
fn pwd_builtin() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("pwd");
    let text = PtyShell::strip_ansi(&out);
    let expected = sh.home_path().display().to_string();
    assert!(
        text.lines().any(|line| line.trim() == expected),
        "expected {expected:?} in pwd output: {text:?}"
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
    sh.wait_for_quiescence(100);
    sh.ctrl_d();
    // Use a generous timeout: on loaded CI runners (macOS-15 in particular)
    // process scheduling can delay the shell processing the ^D by several seconds.
    sh.wait_for_exit(10_000);
}

#[test]
fn exit_command() {
    let sh = PtyShell::spawn();
    sh.type_str("exit");
    sh.enter();
    sh.wait_for_exit(3000);
}

#[test]
fn ctrl_c_cancels_input() {
    let sh = PtyShell::spawn();
    sh.type_str("some partial input");
    sh.ctrl_c();
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    // After Ctrl+C, should see ^C and a new prompt
    assert!(text.contains("^C"), "expected ^C in output: {text:?}");
    assert!(
        text.contains("$ "),
        "expected new prompt after ^C: {text:?}"
    );
}

#[test]
fn sigint_during_editing_cancels_the_line() {
    let sh = PtyShell::spawn();
    sh.type_str("echo this-line-is-cancelled");
    sh.terminal.borrow_mut().signal(libc::SIGINT).unwrap();
    // A fresh prompt must appear without submitting anything: the partial
    // line is discarded with ^C, exactly like Ctrl+C.
    let out = sh.wait_for_prompt_after_line_advance(4000);
    assert!(
        PtyShell::strip_ansi(&out).contains("^C"),
        "expected ^C after SIGINT: {out:?}"
    );
    let out = sh.run_command("echo sigint-cancel-marker");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("sigint-cancel-marker"),
        "shell did not recover after SIGINT: {text:?}"
    );
    assert!(
        !text.contains("this-line-is-cancelled"),
        "SIGINT did not discard the partial line: {text:?}"
    );
}

#[test]
fn line_editing_backspace() {
    let sh = PtyShell::spawn();
    sh.type_str("echo helloo");
    sh.backspace();
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello"),
        "expected 'hello' in output: {text:?}"
    );
}

#[test]
fn line_editing_up_down_navigate_wrapped_input() {
    // A narrow terminal makes both the prompt and input span multiple rows.
    // Up/Down should move within the visual input grid, not enter history.
    let sh = PtyShell::spawn_with_size(&[], &[], 24, 30);
    sh.type_str("echo 012345678901234567890123456789");
    sh.read_timeout(500);
    let before = sh.screen();
    assert!(before.cursor().row > 0, "input did not wrap: {before:?}");

    sh.up();
    sh.read_timeout(500);
    let after_up = sh.screen();
    assert_eq!(after_up.cursor().row + 1, before.cursor().row);

    sh.down();
    sh.read_timeout(500);
    let after_down = sh.screen();
    assert_eq!(after_down.cursor().row, before.cursor().row);
    assert_eq!(after_down.cursor().column, before.cursor().column);
}

#[test]
fn line_editing_ctrl_u() {
    let sh = PtyShell::spawn();
    sh.type_str("this will be killed");
    sh.ctrl_u();
    sh.type_str("echo survived");
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
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
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("keep"), "expected 'keep' in output: {text:?}");
}

#[test]
fn line_editing_ctrl_delete() {
    let sh = PtyShell::spawn();
    sh.type_str("echo alpha beta");
    sh.ctrl_delete();
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("\r\nalpha\r\n"),
        "expected command output after ctrl-delete: {text:?}"
    );
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
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("yanked: hello world"),
        "expected 'yanked: hello world' in output: {text:?}"
    );
}

#[test]
fn pipeline() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("echo 'abc def ghi' | /usr/bin/tr ' ' '\\n' | /usr/bin/wc -l");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains('3'), "expected '3' in output: {text:?}");
}

#[test]
fn and_or_list() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("true && echo yes");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("yes"), "expected 'yes' in output: {text:?}");
}

#[test]
fn or_list() {
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
fn l_lists_files() {
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
fn exported_var_reaches_external_commands() {
    let sh = PtyShell::spawn();
    sh.run_command("export MY_VAR=hello_world");
    let out = sh.run_command("env");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("MY_VAR=hello_world"),
        "expected exported var in external command env: {text:?}"
    );
}

#[test]
fn set_var_reaches_external_commands() {
    let sh = PtyShell::spawn();
    sh.run_command("set TEST_VAR hello_world");
    let out = sh.run_command("env");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("TEST_VAR=hello_world"),
        "expected set var in external command env: {text:?}"
    );
}

#[test]
fn set_var_joins_multiple_value_words() {
    let sh = PtyShell::spawn();
    sh.run_command("set GREETING hello world");
    let out = sh.run_command("echo $GREETING");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello world"),
        "expected joined value: {text:?}"
    );
}

#[test]
fn set_no_args_lists_env_vars() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("set");
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("PATH="), "expected env vars listed: {text:?}");
}

#[test]
fn set_option_and_positional_forms_are_refused_without_target_effects() {
    let sh = PtyShell::spawn();
    for operands in ["-e", "+e", "-o pipefail", "-o noexec", "-", "--", "7 value", "NAME=value", "-e NAME value"] {
        let target = sh.home_path().join("set-target");
        std::fs::write(&target, b"preserved").unwrap();
        let output = PtyShell::strip_ansi(&sh.run_command(&format!("set {operands} >set-target")));
        assert!(output.contains("expected a variable name"), "missing set refusal: {operands}: {output:?}");
        assert_policy_status(&sh, 2);
        assert_eq!(std::fs::read(&target).unwrap(), b"preserved");
    }
    sh.run_command("set OPERAND -e");
    sh.run_command("command -p command -- set \"$OPERAND\" >expanded-set-target");
    assert_policy_status(&sh, 2);
    assert!(!sh.home_path().join("expanded-set-target").exists());
    let output = PtyShell::strip_ansi(&sh.run_command("false; printf 'still-executing'"));
    assert!(output.contains("still-executing"));
    sh.run_command("set EMPTY");
    sh.run_command("set OPTIONS -e +e --");
    sh.run_command("printf '[%s][%s]' \"$EMPTY\" \"$OPTIONS\" >allowed-set-values");
    assert_eq!(std::fs::read(sh.home_path().join("allowed-set-values")).unwrap(), b"[][-e +e --]");
}

fn assert_policy_status(sh: &PtyShell, expected: i32) {
    let output = PtyShell::strip_ansi(&sh.run_command("printf 'policy-status-%s\\n' \"$?\""));
    assert!(output.lines().any(|line| line.trim() == format!("policy-status-{expected}")), "wrong policy status: {output:?}");
}

#[test]
fn exec_and_trap_refusals_preserve_the_shell_and_target_files() {
    let sh = PtyShell::spawn();
    for (name, operands) in [("exec", "/bin/true"), ("trap", "ignored TERM")] {
        sh.run_command(&format!("set BLOCKED {name}"));
        sh.run_command(&format!("alias blocked_{name} {name}"));
        sh.run_command(&format!("/bin/touch static-prior; command -- {name} {operands} >static-target"));
        assert_policy_status(&sh, 2);
        assert!(!sh.home_path().join("static-prior").exists());
        assert!(!sh.home_path().join("static-target").exists());
        for target in [name.to_owned(), format!("'{name}'"), format!("blocked_{name}"), "$BLOCKED".to_owned(), "command -p command -- $BLOCKED".to_owned()] {
            std::fs::write(sh.home_path().join("refused-target"), b"preserved").unwrap();
            let output = PtyShell::strip_ansi(&sh.run_command(&format!("{target} {operands} >refused-target")));
            assert!(output.contains("not supported") || output.contains("not available"), "missing refusal for {target}: {output:?}");
            assert_policy_status(&sh, 2);
            assert_eq!(std::fs::read(sh.home_path().join("refused-target")).unwrap(), b"preserved");
        }
        sh.run_command(&format!("$BLOCKED {operands} >pipeline-target | /bin/cat"));
        assert_policy_status(&sh, 2);
        assert!(!sh.home_path().join("pipeline-target").exists());
        sh.run_command(&format!("printf '%s' \"$(command command $BLOCKED {operands} >substitution-target)\" >outer-target"));
        assert_policy_status(&sh, 2);
        assert!(!sh.home_path().join("substitution-target").exists());
        assert!(!sh.home_path().join("outer-target").exists());
    }
    let output = PtyShell::strip_ansi(&sh.run_command("printf 'shell-alive'"));
    assert!(output.contains("shell-alive"));
    let mut terminal = sh.terminal.borrow_mut();
    terminal.signal(libc::SIGTERM).unwrap();
    let deadline = terminal.deadline(std::time::Duration::from_secs(3));
    assert_eq!(terminal.wait_for_exit(deadline).unwrap(), ExitStatus::Code(143));
    terminal.assert_terminal_restored(&sh.terminal_baseline).unwrap();
}

#[test]
fn builtin_lookup_completion_and_explicit_paths_share_the_policy() {
    let script = "#!/bin/sh\nprintf explicit-exec\nprintf entered >explicit-marker\n";
    let sh = PtyShell::spawn_with_opts(&[("bin/exec", script), ("bin/trap", script)], &[]);
    sh.run_command(&format!("set PATH {}/bin", sh.home_path().display()));
    for query in ["command -v exec", "command -V trap"] {
        sh.run_command(&format!("{query} >query-output 2>query-error"));
        assert_policy_status(&sh, 1);
        assert!(std::fs::read(sh.home_path().join("query-output")).unwrap().is_empty());
    }
    for query in ["w exec", "which trap", "type exec"] {
        let output = PtyShell::strip_ansi(&sh.run_command(query));
        assert!(output.contains("not found"), "refused command was advertised: {output:?}");
        assert_policy_status(&sh, 1);
    }
    sh.run_command("command -v exit >supported-lookup");
    assert_eq!(std::fs::read(sh.home_path().join("supported-lookup")).unwrap(), b"exit\n");
    sh.run_command("command exit >refused-exit");
    assert_policy_status(&sh, 2);
    assert!(!sh.home_path().join("refused-exit").exists());
    sh.type_str("exe");
    sh.tab();
    sh.wait_for_quiescence(100);
    assert!(!active_prompt_text(&sh.screen()).contains("exec"), "refused PATH builtin appeared in completion");
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(2000);
    sh.type_str("ech");
    sh.tab();
    sh.wait_for_quiescence(100);
    assert!(active_prompt_text(&sh.screen()).contains("echo"), "approved builtin was missing from completion");
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(2000);
    sh.run_command("./bin/exec");
    assert_eq!(std::fs::read(sh.home_path().join("explicit-marker")).unwrap(), b"entered");
    std::fs::create_dir(sh.home_path().join("exec")).unwrap();
    sh.run_command("set PATH");
    sh.run_command("exec");
    assert_policy_status(&sh, 2);
    sh.run_command("pwd >cwd-after-refusal");
    assert_eq!(std::fs::read_to_string(sh.home_path().join("cwd-after-refusal")).unwrap().trim(), sh.home_path().to_str().unwrap());
}

#[test]
fn approved_builtin_classes_remain_available() {
    let sh = PtyShell::spawn_with_opts(&[("sub/.keep", "")], &[]);
    for line in [
        "echo output", "printf '%s' output", "pwd", "true", "false", ":",
        "test 1 = 1", "[ 1 = 1 ]", "kill -0 $$", "export POLICY_VAR=value",
        "unset POLICY_VAR", "set POLICY_VAR -value +value", "command -p -- printf output",
        "cd sub && pwd", "history; echo listed",
    ] {
        sh.run_command(line);
        assert_policy_status(&sh, if line == "false" { 1 } else { 0 });
    }
}

#[test]
fn unset_removes_var_from_children() {
    let sh = PtyShell::spawn();
    sh.run_command("export TMP_UNSET_VAR=present");
    assert!(PtyShell::strip_ansi(&sh.run_command("env")).contains("TMP_UNSET_VAR=present"));
    sh.run_command("unset TMP_UNSET_VAR");
    let out = PtyShell::strip_ansi(&sh.run_command("env"));
    assert!(
        !out.contains("TMP_UNSET_VAR=present"),
        "unset var should not reach children: {out:?}"
    );
}

#[test]
fn unset_removes_os_env_var_set_by_set() {
    let sh = PtyShell::spawn();
    sh.run_command("set TMP_OS_VAR some_value");
    assert!(PtyShell::strip_ansi(&sh.run_command("env")).contains("TMP_OS_VAR=some_value"));
    sh.run_command("unset TMP_OS_VAR");
    let out = PtyShell::strip_ansi(&sh.run_command("env"));
    assert!(
        !out.contains("TMP_OS_VAR=some_value"),
        "unset should remove OS env var from children: {out:?}"
    );
}

#[test]
fn unset_in_same_line_not_inherited_by_children() {
    let sh = PtyShell::spawn();
    sh.run_command("export TMP_SAME_LINE=present");
    let out = PtyShell::strip_ansi(&sh.run_command("unset TMP_SAME_LINE; env"));
    assert!(
        !out.contains("TMP_SAME_LINE=present"),
        "unset in the same line must not leak to children: {out:?}"
    );
}

#[test]
fn unset_removes_ambient_environment_from_children() {
    let sh = PtyShell::spawn_with_setup(
        &[],
        &[],
        &[
            ("TMP_AMBIENT_VAR", "ambient"),
            ("1ISH_AMBIENT", "invalid-name"),
        ],
        24,
        80,
        None,
        |_| {},
    );
    let before = PtyShell::strip_ansi(&sh.run_command("env"));
    assert!(before.contains("TMP_AMBIENT_VAR=ambient"));
    assert!(before.contains("1ISH_AMBIENT=invalid-name"));

    let out = PtyShell::strip_ansi(&sh.run_command("unset TMP_AMBIENT_VAR; env | grep AMBIENT"));
    assert!(
        !out.contains("TMP_AMBIENT_VAR=ambient"),
        "stale env leaked: {out:?}"
    );
    assert!(
        out.contains("1ISH_AMBIENT=invalid-name"),
        "inherited non-shell-name env entry was lost: {out:?}"
    );
}

#[test]
fn unset_removes_ambient_environment_in_pipeline_children() {
    let sh = PtyShell::spawn_with_setup(
        &[],
        &[],
        &[("TMP_PIPE_AMBIENT", "ambient")],
        24,
        80,
        None,
        |_| {},
    );
    let out = PtyShell::strip_ansi(&sh.run_command("unset TMP_PIPE_AMBIENT; env | grep TMP_PIPE"));
    assert!(
        !out.contains("TMP_PIPE_AMBIENT=ambient"),
        "stale pipeline env leaked: {out:?}"
    );
}

#[test]
fn compound_list_export_reaches_child() {
    let sh = PtyShell::spawn();
    let out = PtyShell::strip_ansi(&sh.run_command("cd /tmp && export TMP_COMPOUND=value; env"));
    assert!(
        out.contains("TMP_COMPOUND=value"),
        "compound export was lost: {out:?}"
    );
}

#[test]
fn store_home_drives_interactive_cd() {
    let sh = PtyShell::spawn();
    sh.run_command("set HOME /tmp");
    let out = PtyShell::strip_ansi(&sh.run_command("cd; pwd"));
    assert!(out.contains("/tmp"), "cd did not use store HOME: {out:?}");
}

#[test]
fn unset_oldpwd_blocks_interactive_cd_minus() {
    let sh = PtyShell::spawn();
    sh.run_command("cd /tmp");
    sh.run_command("unset OLDPWD");
    let out = PtyShell::strip_ansi(&sh.run_command("cd -"));
    assert!(
        out.contains("no previous directory"),
        "cd - used stale process OLDPWD: {out:?}"
    );
}

#[test]
fn prefix_assignment_reaches_child_but_does_not_persist() {
    let sh = PtyShell::spawn();
    let out = PtyShell::strip_ansi(&sh.run_command("TMP_PREFIX=value env"));
    assert!(
        out.contains("TMP_PREFIX=value"),
        "prefix assignment should reach the child: {out:?}"
    );
    let out = PtyShell::strip_ansi(&sh.run_command("env"));
    assert!(
        !out.contains("TMP_PREFIX=value"),
        "prefix assignment must not persist: {out:?}"
    );
}

#[test]
fn set_path_affects_command_lookup() {
    let sh = PtyShell::spawn_with_opts(&[("bin/mytool", "#!/bin/sh\necho mytool-ran\n")], &[]);
    let bin = sh.home_path().join("bin");
    sh.run_command(&format!("export PATH={}", bin.display()));
    let out = PtyShell::strip_ansi(&sh.run_command("mytool"));
    assert!(
        out.contains("mytool-ran"),
        "expected script found via exported PATH: {out:?}"
    );
}

#[test]
fn which_reflects_exported_path() {
    let sh = PtyShell::spawn_with_opts(&[("bin/mytool", "#!/bin/sh\necho mytool-ran\n")], &[]);
    let bin = sh.home_path().join("bin");
    sh.run_command(&format!("export PATH={}", bin.display()));
    let out = PtyShell::strip_ansi(&sh.run_command("which mytool"));
    assert!(
        out.contains("mytool"),
        "which should resolve through the exported PATH: {out:?}"
    );
}

#[test]
fn which_uses_store_path_not_os_env() {
    // `export PATH` only changes epsh's store. ish's own command lookup must
    // consult the store, not ish's (stale) process environment.
    let sh = PtyShell::spawn();
    let empty = sh.home_path().join("empty-bin");
    sh.run_command(&format!("mkdir -p {}", empty.display()));
    sh.run_command(&format!("export PATH={}", empty.display()));
    let out = PtyShell::strip_ansi(&sh.run_command("which ls"));
    assert!(
        out.contains("not found"),
        "which must use the exported (store) PATH, not the OS env: {out:?}"
    );
    let out = PtyShell::strip_ansi(&sh.run_command("which /bin/ls"));
    assert!(
        out.contains("/bin/ls"),
        "absolute path lookup must still work: {out:?}"
    );
}

#[test]
fn command_lookup_classifies_ish_commands_with_empty_path() {
    let sh = PtyShell::spawn_with_setup(&[("source", "external copy bytes")], &[], &[], 24, 80, None, |home| {
        std::fs::create_dir(home.join("destination-directory")).unwrap();
    });
    sh.run_command("export PATH=");

    let output = clean(&sh.run_command("command -v cp"));
    assert!(output.lines().any(|line| line.trim() == "cp"), "{output:?}");
    assert_native_fs_status(&sh, 0);

    let output = clean(&sh.run_command("command -V mv"));
    assert!(output.lines().any(|line| line.trim() == "mv is a shell builtin"), "{output:?}");
    assert_native_fs_status(&sh, 0);

    let output = clean(&sh.run_command("command -v /bin/cp"));
    assert!(output.lines().any(|line| line.trim() == "/bin/cp"), "{output:?}");
    assert_native_fs_status(&sh, 0);

    sh.run_command("/bin/cp source destination-directory");
    assert_eq!(
        std::fs::read(sh.home_path().join("destination-directory/source")).unwrap(),
        b"external copy bytes"
    );
    assert_native_fs_status(&sh, 0);
}

#[test]
fn tilde_expansion() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("echo ~");
    let text = PtyShell::strip_ansi(&out);
    let expected = sh.home_path().display().to_string();
    assert!(
        text.lines().any(|line| line.trim() == expected),
        "expected home directory {expected:?}: {text:?}"
    );
}

#[test]
fn history_up_arrow() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo from_global"]);
    sh.run_command("echo local_one");
    sh.run_command("echo local_two");
    // Wait for each redraw before sending the next escape sequence. The macOS
    // PTY runner can otherwise coalesce rapid writes with the previous redraw.
    for expected in ["echo local_two", "echo local_one", "echo from_global"] {
        let mut redraw = String::new();
        for _ in 0..10 {
            sh.up();
            redraw = normalize_screen_text(&PtyShell::strip_ansi(&sh.read_timeout(500)));
            if redraw.contains(expected) {
                break;
            }
        }
        assert!(
            redraw.contains(expected),
            "expected history redraw {expected:?}: {redraw:?}"
        );
    }
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("from_global"),
        "expected startup history entry after local entries: {text:?}"
    );
}

#[test]
fn history_up_narrow_repaint_clears_wrapped_rows() {
    let sh = PtyShell::spawn_with_size(&[], &[], 24, 12);
    sh.run_command("echo ok");
    sh.run_command("echo WRAPMARK12345678901234567890");
    sh.run_command("echo newer");
    // Up first traverses the wrapped visual rows of each recalled command;
    // keep moving until the oldest entry is reached.
    for _ in 0..32 {
        sh.up();
    }
    let screen = {
        let mut terminal = sh.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_secs(2));
        terminal
            .wait_for_screen(deadline, "recalled oldest history entry", |screen| {
                let active = active_prompt_text(screen);
                normalize_screen_text(&active).contains("echo ok")
                    && !active.contains("WRAPMARK")
                    && !active.contains("newer")
            })
            .expect("timed out recalling oldest history entry")
    };
    assert_eq!(screen.size(), Size::new(12, 24).unwrap());
    let active = active_prompt_text(&screen);
    let normalized = normalize_screen_text(&active);
    assert!(
        normalized.contains("echo ok"),
        "expected final prompt to show `echo ok`: {active:?}"
    );
    assert!(
        !active.contains("WRAPMARK"),
        "wrapped history entry leaked into final prompt region: {active:?}"
    );
    assert!(
        !active.contains("newer"),
        "newer history entry leaked into final prompt region: {active:?}"
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
    sh.wait_for_quiescence(200);

    // Accept with Enter
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    // The selected command should be on the line — enter executes it
    assert!(text.contains("beta"), "expected 'beta' in output: {text:?}");
}

#[test]
fn history_ctrl_r_includes_later_global_entries() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo startup"]);
    let hist_path = sh.home_path().join(".local/share/ish/history");
    let mut other_shell = History::load_from(hist_path).unwrap();
    other_shell.add("echo later_global").unwrap();

    sh.ctrl_r();
    let out = sh.wait_for("search:", 2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("search:"), "expected search pager: {text:?}");
    assert!(
        text.contains("later_global"),
        "later global entry missing from initial history pager: {text:?}"
    );

    sh.type_str("later");
    sh.wait_for_quiescence(200);
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("echo later_global"),
        "later global entry missing from Ctrl+R acceptance: {text:?}"
    );
}

#[test]
fn history_ctrl_r_escape_cancels() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo secret"]);
    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    sh.type_str("secret");
    sh.wait_for_quiescence(200);
    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history search closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });
    sh.enter();
    let out2 = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out2);
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
    sh.wait_for_quiescence(100);
    sh.type_str("c");
    sh.wait_for_quiescence(100);
    sh.backspace();
    sh.wait_for_quiescence(100);
    sh.type_str("c");
    sh.wait_for_quiescence(100);
    sh.down();
    sh.up();
    sh.wait_for_quiescence(200);

    let screen = snapshot_text(&sh.screen());
    assert_screen_contains_once(&screen, "search:");
    assert_screen_contains_once(&screen, "abc1");
    assert_screen_contains_once(&screen, "abc2");
    assert_screen_contains_once(&screen, "abc3");

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history search closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });
}

#[test]
fn history_search_selection_preserves_cursor_after_typing() {
    let history = &[
        "abcdefghi0",
        "abcdefghi1",
        "abcdefghi2",
        "abcdefghi3",
        "abcdefghi4",
        "abcdefghi5",
        "abcdefghi6",
        "abcdefghi7",
        "abcdefghi8",
        "abcdefghi9",
    ];
    let sh = PtyShell::spawn_with_size(&[], history, 24, 10);
    let fill_cmd = (1..=14)
        .map(|i| format!("echo fill{i:02}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut initial_output = sh.startup_output().to_string();
    initial_output.push_str(&sh.run_command(&fill_cmd));
    let frames = run_trace_with_initial_output(
        &sh,
        24,
        10,
        &[
            TraceStep {
                label: "open search",
                input: TraceInput::Bytes(b"\x12"),
                settle_ms: 0,
                read_ms: 500,
            },
            TraceStep {
                label: "type abc",
                input: TraceInput::Text("abc"),
                settle_ms: 100,
                read_ms: 500,
            },
            TraceStep {
                label: "select next",
                input: TraceInput::Bytes(b"\x1b[B"),
                settle_ms: 100,
                read_ms: 500,
            },
            TraceStep {
                label: "type x",
                input: TraceInput::Text("x"),
                settle_ms: 100,
                read_ms: 500,
            },
        ],
        &initial_output,
    );
    let query_frame = &frames[1];
    let selection_frame = &frames[2];
    assert_eq!(
        selection_frame.snapshot.cursor_row, query_frame.snapshot.cursor_row,
        "selection redraw moved the visual cursor to a different row: query={:?} selection={:?}",
        query_frame.snapshot, selection_frame.snapshot
    );
    assert_eq!(
        selection_frame.snapshot.cursor_col, query_frame.snapshot.cursor_col,
        "selection redraw moved the visual cursor to a different column: query={:?} selection={:?}",
        query_frame.snapshot, selection_frame.snapshot
    );

    let typed_frame = &frames[3];
    assert_eq!(
        typed_frame.snapshot.cursor_row, query_frame.snapshot.cursor_row,
        "typing after selection left the visual cursor on the wrong row: {:?}",
        typed_frame.snapshot
    );
    assert_eq!(
        typed_frame.snapshot.cursor_col,
        query_frame.snapshot.cursor_col + 1,
        "typing after selection left the visual cursor at the wrong column: {:?}",
        typed_frame.snapshot
    );
    assert!(
        typed_frame.snapshot.visible.contains("search: ab\ncx"),
        "the query buffer should contain the typed character: {:?}",
        typed_frame.snapshot
    );
    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history search closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });
}

#[test]
fn history_accept_reanchors_prompt_before_typing() {
    let history = &[
        "echo history-one-abcdefghijklmnop",
        "echo history-two-abcdefghijklmnop",
    ];
    let sh = PtyShell::spawn_with_size(&[], history, 8, 20);
    let fill_cmd = (1..=6)
        .map(|i| format!("echo fill{i:02}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut initial_output = sh.startup_output().to_string();
    initial_output.push_str(&sh.run_command(&fill_cmd));

    let frames = run_trace_with_initial_output(
        &sh,
        8,
        20,
        &[
            TraceStep {
                label: "open search",
                input: TraceInput::Bytes(b"\x12"),
                settle_ms: 0,
                read_ms: 500,
            },
            TraceStep {
                label: "query",
                input: TraceInput::Text("history"),
                settle_ms: 100,
                read_ms: 500,
            },
            TraceStep {
                label: "select second result",
                input: TraceInput::Bytes(b"\x1b[B"),
                settle_ms: 100,
                read_ms: 500,
            },
            TraceStep {
                label: "accept",
                input: TraceInput::Bytes(b"\r"),
                settle_ms: 100,
                read_ms: 500,
            },
            TraceStep {
                label: "type after accept",
                input: TraceInput::Text("x"),
                settle_ms: 100,
                read_ms: 500,
            },
        ],
        &initial_output,
    );

    let frame = frames.last().unwrap();
    let accept_frame = &frames[3];
    assert!(
        !accept_frame.raw.starts_with("\x1b8\x1b[J"),
        "history exit should clear from the live pager cursor, not a stale saved anchor: {:?}",
        accept_frame.raw
    );
    assert!(
        !frame.snapshot.visible.contains("search:"),
        "history header remained after accepting a result: {:?}",
        frame.snapshot
    );
    assert!(
        compact_screen_text(&frame.snapshot.visible)
            .contains(&compact_screen_text("echo history-one-abcdefghijklmnopx")),
        "typing after history acceptance should start from the selected command: {:?}",
        frame.snapshot
    );

    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(2000);
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

    let screen = snapshot_text(&sh.screen());
    assert_screen_contains_once(&screen, "search:");
    assert!(
        screen.contains("hist12"),
        "expected recent history entry in pager: {screen:?}"
    );
    assert!(
        screen.contains("hist11"),
        "expected second recent history entry in pager: {screen:?}"
    );

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history search closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });
}

#[test]
fn history_ctrl_r_scrolls_when_selection_passes_last_visible_entry() {
    let history = &[
        "hist01", "hist02", "hist03", "hist04", "hist05", "hist06", "hist07", "hist08", "hist09",
        "hist10", "hist11", "hist12",
    ];
    let sh = PtyShell::spawn_with_size(&[], history, 8, 20);

    sh.ctrl_r();
    let mut out = sh.wait_for("search:", 2000);
    for _ in 0..11 {
        sh.down();
    }
    out.push_str(&sh.read_timeout(500));

    let screen = snapshot_text(&sh.screen());
    assert!(
        screen.contains("hist01"),
        "expected the last history entry: {screen:?}"
    );
    assert!(
        !screen.contains("hist12"),
        "pager did not scroll down: {screen:?}"
    );

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history search closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });
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
        let header_row = frame
            .snapshot
            .visible
            .lines()
            .position(|line| line.contains("search:"))
            .expect("search header should be visible");
        assert_eq!(
            frame.snapshot.cursor_row, header_row,
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

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history search closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });
}

#[test]
fn tab_completion_files() {
    let sh = PtyShell::spawn_with_opts(
        &[("alpha.txt", ""), ("bravo.txt", ""), ("charlie.txt", "")],
        &[],
    );
    sh.type_str("echo al");
    sh.tab();
    sh.wait_for_quiescence(300);
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
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
fn tab_completion_first_tab_has_no_selection_second_tab_selects_first() {
    let sh = PtyShell::spawn_with_opts(&[("aaa.txt", ""), ("aab.txt", ""), ("aac.txt", "")], &[]);
    sh.type_str("echo aa");
    sh.tab();
    sh.wait_for_screen("completion grid", 2000, |screen| screen.contains("aaa.txt"));
    sh.wait_for_quiescence(150);
    let first = snapshot_text(&sh.screen());
    assert_eq!(
        first.matches("aaa.txt").count(),
        1,
        "first tab should show the grid without previewing the first item into the prompt line: {first:?}"
    );

    sh.tab();
    sh.wait_for_screen("completion preview", 2000, |screen| active_prompt_text(screen).contains("aaa.txt"));
    sh.wait_for_quiescence(150);
    let second = snapshot_text(&sh.screen());
    assert_eq!(
        second.matches("aaa.txt").count(),
        2,
        "second tab should activate selection on the first item and preview it in the prompt line: {second:?}"
    );

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("completion grid closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("aaa.txt")
    });
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
    sh.wait_for_quiescence(150);
    sh.type_str("a");
    sh.wait_for_quiescence(100);
    sh.down();
    sh.down();
    sh.up();
    sh.wait_for_quiescence(200);

    let screen = snapshot_text(&sh.screen());
    let flattened = screen.replace('\n', "");
    assert_eq!(
        flattened.matches("aaa.txt").count(),
        2,
        "selected completion should appear once in the preview line and once in the grid: {screen:?}"
    );
    assert_screen_contains_once(&screen, "aab.txt");
    assert_screen_contains_once(&screen, "aac.txt");

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("completion grid closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("aaa.txt")
    });
}

#[test]
fn tab_completion_directory() {
    let sh = PtyShell::spawn_with_opts(&[("mydir/.keep", "")], &[]);
    sh.type_str("cd my");
    sh.tab();
    sh.wait_for_quiescence(300);
    sh.enter();
    sh.wait_for_prompt_after_line_advance(2000);
    // After cd, pwd should show mydir
    let out2 = sh.run_command("pwd");
    let text = PtyShell::strip_ansi(&out2);
    assert!(text.contains("mydir"), "expected to be in mydir: {text:?}");
}

#[test]
fn tab_completion_escape_restores_typed_prefix() {
    let sh = PtyShell::spawn_with_opts(&[("alpha.txt", ""), ("alpine.txt", "")], &[]);
    sh.type_str("echo al");
    sh.tab();
    sh.wait_for_quiescence(200);
    sh.escape();
    sh.type_str("x");
    sh.enter();
    sh.wait_for_prompt_after_line_advance(2000);
    let screen = snapshot_text(&sh.screen());
    let text = normalize_screen_text(&screen);
    assert!(
        text.contains("al"),
        "escape should restore the typed prefix, not keep the preview: {screen:?}"
    );
    assert!(
        !text.contains("alpha.txt") && !text.contains("alpine.txt"),
        "completion preview should not remain committed after escape: {screen:?}"
    );
}

#[test]
fn tab_completion_narrowing_does_not_autoaccept() {
    let sh = PtyShell::spawn_with_opts(&[("signal.rs", ""), ("sys.rs", "")], &[]);
    sh.type_str("echo s");
    sh.tab();
    sh.wait_for_quiescence(150);
    sh.type_str("y");
    sh.wait_for_quiescence(150);
    sh.escape();
    sh.type_str("x");
    sh.enter();
    sh.wait_for_prompt_after_line_advance(2000);
    let screen = snapshot_text(&sh.screen());
    let text = normalize_screen_text(&screen);
    assert!(
        text.contains("sy"),
        "typing should refine the base text without auto-accepting the remaining match: {screen:?}"
    );
    assert!(
        !text.contains("sys.rs"),
        "the single remaining match should stay a preview until Enter confirms it: {screen:?}"
    );
}

#[test]
fn tab_completion_with_wide_dir_name_restores_prompt_cursor() {
    let sh = PtyShell::spawn_with_size(
        &[
            ("Sync/M/Music/bandcamp/Altered States/.keep", ""),
            ("Sync/M/Music/bandcamp/Another Language/.keep", ""),
            ("Sync/M/Music/bandcamp/Dreamage/.keep", ""),
            (
                "Sync/M/Music/bandcamp/黑馬河的兒子 The Son of Black Horse River/.keep",
                "",
            ),
            ("Sync/M/Music/bandcamp/Arigto - Lungs/.keep", ""),
            (
                "Sync/M/Music/bandcamp/01 - Charlotte de Witte - Sehnsucht (Original Mix) [56812652].mp3",
                "",
            ),
            (
                "Sync/M/Music/bandcamp/Floating Points, Pharoah Sanders & The London Symphony Orchestra - Promises [Movement 6] [FQdLWlvgHOg].m4a",
                "",
            ),
        ],
        &[],
        24,
        80,
    );

    let cmd = "/Applications/Play.app/Contents/MacOS/play ~/Sync/M/Music/bandcamp/";
    let frames = run_trace(
        &sh,
        24,
        80,
        &[
            TraceStep {
                label: "type cmd",
                input: TraceInput::Text(cmd),
                settle_ms: 150,
                read_ms: 600,
            },
            TraceStep {
                label: "tab",
                input: TraceInput::Bytes(b"\t"),
                settle_ms: 200,
                read_ms: 800,
            },
            TraceStep {
                label: "tab again",
                input: TraceInput::Bytes(b"\t"),
                settle_ms: 200,
                read_ms: 800,
            },
        ],
    );

    let before_tab = &frames[0];
    let first_tab = &frames[1];
    let second_tab = frames.last().unwrap();
    assert_frame_contains_once(first_tab, "Altered States/");
    assert_frame_contains_once(first_tab, "Another Language/");
    assert_frame_contains_once(first_tab, "Dreamage/");
    assert!(
        first_tab
            .snapshot
            .visible
            .contains("黑馬河的兒子 The Son of Black Horse River/"),
        "expected wide directory entry in completion grid: {:?}",
        first_tab.snapshot
    );
    assert_eq!(
        first_tab.snapshot.cursor_row, before_tab.snapshot.cursor_row,
        "expected prompt cursor row to stay on the typed line after the first tab: before={:?} after={:?}",
        before_tab.snapshot, first_tab.snapshot
    );
    assert_eq!(
        first_tab.snapshot.cursor_col, before_tab.snapshot.cursor_col,
        "first tab should not preview a selection into the prompt line: before={:?} after={:?}",
        before_tab.snapshot, first_tab.snapshot
    );

    assert_frame_contains_once(second_tab, "Altered States/");
    assert_frame_contains_once(second_tab, "Another Language/");
    assert_frame_contains_once(second_tab, "Dreamage/");
    assert!(
        second_tab
            .snapshot
            .visible
            .contains("黑馬河的兒子 The Son of Black Horse River/"),
        "expected wide directory entry in completion grid after second tab: {:?}",
        second_tab.snapshot
    );
    assert!(
        second_tab
            .snapshot
            .visible
            .replace('\n', "")
            .contains("~/'Sync/M/Music/bandcamp/01 - Charlotte de Witte"),
        "second tab should activate selection and show the live preview: {:?}",
        second_tab.snapshot
    );
    let grid_row = second_tab
        .snapshot
        .visible
        .lines()
        .position(|line| line.contains("Altered States/"))
        .expect("completion grid should be visible");
    assert!(
        second_tab.snapshot.cursor_row < grid_row,
        "prompt cursor should remain above the completion grid: before={:?} after={:?}",
        before_tab.snapshot,
        second_tab.snapshot
    );

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("completion grid closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("Altered States/")
    });
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
    sh.wait_for_quiescence(200);

    sh.resize(24, 12);
    sh.wait_for_quiescence(200);

    let screen = snapshot_text(&sh.screen());
    assert_screen_contains_once(&screen, "aaa.txt");
    assert_screen_contains_once(&screen, "aab.txt");
    assert_screen_contains_once(&screen, "aac.txt");

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("completion grid closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("aaa.txt")
    });
}

#[test]
fn normal_resize_reanchors_wrapped_prompt() {
    let sh = PtyShell::spawn_with_size(&[], &[], 12, 80);
    sh.run_command("printf '\\n\\n'");

    sh.type_str("echo resize-test-abcdefghijklmnopqrstuvwxyz");
    sh.read_timeout(800);

    sh.resize(12, 52);
    sh.wait_for_quiescence(200);
    let resize_output = sh.read_timeout(800);
    assert!(
        resize_output.contains("\x1b[H\x1b[2J"),
        "resize should invalidate the old terminal region before repainting: {resize_output:?}"
    );

    sh.type_str("x");
    sh.read_timeout(800);
    let screen = sh.screen();
    assert_eq!(screen.size(), Size::new(52, 12).unwrap());
    let visible = snapshot_text(&screen);

    let prompt_count = visible.matches("testuser@").count();
    assert_eq!(
        prompt_count, 1,
        "resize should repaint one prompt, not duplicate the reflowed prompt: {:?}",
        screen
    );
    assert!(
        visible
            .replace('\n', "")
            .contains("echo resize-test-abcdefghijklmnopqrstuvwxyzx"),
        "resized prompt lost or duplicated input: {:?}",
        screen
    );

    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(2000);
}

#[test]
fn history_resize_rerenders_pager() {
    let sh = PtyShell::spawn_with_size(&[], &["abc1", "abc2", "abc3"], 24, 20);
    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    sh.type_str("abc");
    sh.wait_for_quiescence(200);

    sh.resize(24, 10);
    sh.wait_for_quiescence(200);

    let screen = snapshot_text(&sh.screen());
    assert_screen_contains_once(&screen, "search:");
    assert_screen_contains_once(&screen, "abc1");
    assert_screen_contains_once(&screen, "abc2");
    assert_screen_contains_once(&screen, "abc3");

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history pager closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });
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
    sh.wait_for_quiescence(50);
    sh.type_str(" ");
    sh.wait_for_quiescence(200);
    let after_first = sh.read_timeout(300);
    let first_text = PtyShell::strip_ansi(&after_first);
    eprintln!("AFTER FIRST SPACE: {first_text:?}");
    // Type another space — should NOT re-expand
    sh.type_str(" ");
    sh.wait_for_quiescence(200);
    let after_second = sh.read_timeout(300);
    let second_text = PtyShell::strip_ansi(&after_second);
    eprintln!("AFTER SECOND SPACE: {second_text:?}");
    // Cancel the line
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(2000);

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
    sh.wait_for_quiescence(50);
    sh.type_str(" ");
    sh.wait_for_quiescence(200);
    let after_first = sh.read_timeout(300);
    let first_text = PtyShell::strip_ansi(&after_first);
    eprintln!("CONFIG AFTER FIRST SPACE: {first_text:?}");
    // Type another space
    sh.type_str(" ");
    sh.wait_for_quiescence(200);
    let after_second = sh.read_timeout(300);
    let second_text = PtyShell::strip_ansi(&after_second);
    eprintln!("CONFIG AFTER SECOND SPACE: {second_text:?}");
    // Third space
    sh.type_str(" ");
    sh.wait_for_quiescence(200);
    let after_third = sh.read_timeout(300);
    let third_text = PtyShell::strip_ansi(&after_third);
    eprintln!("CONFIG AFTER THIRD SPACE: {third_text:?}");
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(2000);

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
    sh.wait_for_quiescence(50);
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
fn alias_preserves_quoted_word() {
    let sh = PtyShell::spawn();
    sh.run_command(r#"alias greet echo "hello world""#);
    let out = sh.run_command("greet");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("hello world"),
        "expected quoted alias word to stay intact: {text:?}"
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
    let raw = sh.wait_for_prompt_after_line_advance(2000);
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
fn source_is_refused() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("source foo.sh");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("`source` is not supported"),
        "expected refusal: {text:?}"
    );
}

#[test]
fn expanded_targets_obey_ish_execution_policy() {
    let sh = PtyShell::spawn();
    let home = sh.home_path();
    let eval_marker = home.join("eval-marker");
    let source_marker = home.join("source-marker");
    let dot_marker = home.join("dot-marker");
    let command_marker = home.join("command-marker");
    let nested_marker = home.join("nested-marker");
    let source_script = home.join("source-target.sh");
    let dot_script = home.join("dot-target.sh");
    std::fs::write(
        &source_script,
        format!("/usr/bin/touch {}\n", source_marker.display()),
    )
    .unwrap();
    std::fs::write(
        &dot_script,
        format!("/usr/bin/touch {}\n", dot_marker.display()),
    )
    .unwrap();

    let refused = [
        (
            format!(
                "set TARGET eval; $TARGET /usr/bin/touch {}",
                eval_marker.display()
            ),
            &eval_marker,
        ),
        (
            format!("set TARGET source; $TARGET {}", source_script.display()),
            &source_marker,
        ),
        (
            format!("set TARGET .; $TARGET {}", dot_script.display()),
            &dot_marker,
        ),
        (
            format!(
                "set TARGET eval; command $TARGET /usr/bin/touch {}",
                command_marker.display()
            ),
            &command_marker,
        ),
    ];

    for (line, marker) in refused {
        sh.run_command(&line);
        assert!(!marker.exists(), "forbidden target changed {}", marker.display());
        let status = PtyShell::strip_ansi(&sh.run_command("echo policy-status-$?"));
        assert!(
            status.lines().any(|line| line.trim() == "policy-status-2"),
            "expected dynamic target status 2 after {line:?}, got {status:?}"
        );
    }

    let redirection_target = home.join("redirection-target-marker");
    let redirection_file = home.join("redirection-file");
    sh.run_command(&format!(
        "set TARGET eval; $TARGET /usr/bin/touch {} > {}",
        redirection_target.display(),
        redirection_file.display()
    ));
    assert!(!redirection_target.exists(), "forbidden eval target ran");
    assert!(!redirection_file.exists(), "forbidden eval redirection opened");
    let status = PtyShell::strip_ansi(&sh.run_command("echo policy-status-$?"));
    assert!(
        status.lines().any(|line| line.trim() == "policy-status-2"),
        "expected redirection refusal status 2, got {status:?}"
    );

    sh.run_command(&format!(
        "set TARGET eval; echo $(echo $(command $TARGET /usr/bin/touch {}))",
        nested_marker.display()
    ));
    assert!(
        !nested_marker.exists(),
        "nested command substitution ran a forbidden target"
    );

    sh.run_command("set TARGET exit; $TARGET 23");
    let status = PtyShell::strip_ansi(&sh.run_command("echo policy-status-$?"));
    assert!(
        status.lines().any(|line| line.trim() == "policy-status-2"),
        "expected InShell builtin target status 2, got {status:?}"
    );

    let ordinary_marker = home.join("ordinary-command-marker");
    sh.run_command(&format!(
        "set TARGET touch; $TARGET {}",
        ordinary_marker.display()
    ));
    assert!(ordinary_marker.exists(), "dynamic external command did not run");

    let subdir = home.join("policy-subdir");
    std::fs::create_dir(&subdir).unwrap();
    sh.run_command("set TARGET cd");
    let output = PtyShell::strip_ansi(&sh.run_command("$TARGET policy-subdir && pwd"));
    let expected_cwd = subdir.to_string_lossy().into_owned();
    assert!(
        output.lines().any(|line| line.trim() == expected_cwd),
        "dynamic Hybrid builtin did not run in a list: {output:?}"
    );

    std::fs::write(home.join("native-list-sentinel"), b"").unwrap();
    sh.run_command("set TARGET l");
    let output = PtyShell::strip_ansi(&sh.run_command(&format!(
        "$TARGET {}; true",
        home.display()
    )));
    assert!(
        output.contains("native-list-sentinel"),
        "dynamic native builtin did not run in a list: {output:?}"
    );
    sh.run_command("/bin/printf input | $TARGET . >native-pipeline-target");
    assert_policy_status(&sh, 2);
    assert!(!subdir.join("native-pipeline-target").exists());
}

#[test]
fn ctrl_l_clears_screen() {
    let sh = PtyShell::spawn();
    let mut out = sh.run_command("echo before_clear");
    let _ = sh.read_timeout(200);
    sh.type_str("echo after_clear");
    sh.ctrl_l();
    out.push_str(&sh.wait_for("after_clear", 2000));
    sh.wait_for_quiescence(200);
    let screen = snapshot_text(&sh.screen());
    let normalized = normalize_screen_text(&screen);
    assert!(
        out.contains("\x1b[H") || out.contains("\x1b[2J"),
        "expected screen clear sequence: {out:?}"
    );
    assert!(
        !screen.contains("before_clear"),
        "clear should remove prior output from visible screen: {screen:?}"
    );
    assert!(
        normalized.contains("echo after_clear"),
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
    let out2 = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out2);
    assert!(
        text.contains("HELLO"),
        "expected uppercased output: {text:?}"
    );
}

#[test]
fn multiline_preserves_unicode_quotes_and_backslash_newlines() {
    let sh = PtyShell::spawn();
    sh.type_str("printf '%s' 'é");
    sh.enter();
    sh.wait_for_line_advance(1000);
    sh.type_str("日本\\");
    sh.enter();
    sh.wait_for_line_advance(1000);
    sh.type_str("語' > quoted");
    sh.enter();
    sh.wait_for_prompt_after_line_advance(2000);
    assert_eq!(std::fs::read(sh.home_path().join("quoted")).unwrap(), "é\n日本\\\n語".as_bytes());

    sh.type_str("printf '%s' é\\");
    sh.enter();
    sh.wait_for_line_advance(1000);
    sh.type_str("語 > escaped");
    sh.enter();
    sh.wait_for_prompt_after_line_advance(2000);
    assert_eq!(std::fs::read(sh.home_path().join("escaped")).unwrap(), "é語".as_bytes());
}

#[test]
fn interactive_final_history_replay_restores_the_recorded_directory() {
    let sh = PtyShell::spawn_with_setup(
        &[], &[], &[], 24, 120, Some("parent space'quote"),
        |home| std::fs::create_dir_all(home.join("parent space'quote/child")).unwrap(),
    );
    sh.run_command("cd child");
    sh.up();
    sh.enter();
    sh.wait_for_prompt_after_line_advance(2000);
    sh.run_command("pwd > replay-pwd");
    let child = sh.home_path().join("parent space'quote/child");
    assert_eq!(
        std::fs::read_to_string(child.join("replay-pwd")).unwrap().trim(),
        std::fs::canonicalize(child).unwrap().to_str().unwrap(),
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
    sh.wait_for_quiescence(200);
    let out = sh.read_timeout(600);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("./bin/upper"),
        "expected multiline completion to expand on continuation line: {text:?}"
    );
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(2000);
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
    sh.wait_for_quiescence(150);
    sh.down();
    sh.up();
    sh.wait_for_quiescence(200);

    let screen = snapshot_text(&sh.screen());
    let (_, picker) = screen
        .split_once("dirs:\n")
        .expect("directory picker should be visible");
    assert_screen_contains_once(picker, "~/one");
    assert_screen_contains_once(picker, "~/two");

    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("directory picker closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("dirs:")
    });
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
    let raw = sh.wait_for_prompt_after_line_advance(2000);
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
    // Regression: `cd ~/subdir` was broken because change_directory used format!("{}{rest}", home)
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
fn implicit_cd_quoted_path() {
    let sh = PtyShell::spawn_with_opts(&[("space dir/.keep", "")], &[]);
    sh.run_command("'space dir'");
    let out = sh.run_command("pwd");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("space dir"),
        "quoted single path should implicit-cd: {text:?}"
    );
}

#[test]
fn path_binary_wins_over_implicit_cd() {
    let sh = PtyShell::spawn_with_opts(
        &[("foo/.keep", ""), ("bin/foo", "#!/bin/sh\necho path-foo-ran\n")],
        &[],
    );
    let bin = sh.home_path().join("bin");
    sh.run_command(&format!("export PATH={}", bin.display()));
    let out = PtyShell::strip_ansi(&sh.run_command("foo"));
    assert!(out.contains("path-foo-ran"), "expected PATH binary: {out:?}");

    let out = PtyShell::strip_ansi(&sh.run_command("pwd"));
    let foo_dir = sh.home_path().join("foo").display().to_string();
    assert!(!out.contains(&foo_dir), "expected to stay in HOME: {out:?}");
}

#[test]
fn l_tilde_subdir() {
    // Regression: `l ~/subdir` passed the literal "~/subdir" to list_directory without tilde
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
fn l_glob_expansion() {
    let sh = PtyShell::spawn_with_opts(
        &[
            ("rust-toolchain.toml", ""),
            ("toolbox.txt", ""),
            ("Cargo.toml", ""),
        ],
        &[],
    );
    let out = sh.run_command("l *tool*");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("rust-toolchain.toml") && text.contains("toolbox.txt"),
        "expected builtin glob expansion: {text:?}"
    );
    assert!(
        !text.contains("Cargo.toml"),
        "builtin glob should not include non-matching files: {text:?}"
    );
    assert!(
        !text.contains("No such file or directory"),
        "builtin glob should not pass the literal pattern through: {text:?}"
    );
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
    sh.run_command("/bin/echo unique_cmd_12345");
    // Now up arrow should recall it
    sh.up();
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        text.contains("unique_cmd_12345"),
        "expected history recall: {text:?}"
    );
}

#[test]
fn history_survives_abrupt_termination_and_restart() {
    let sh = PtyShell::spawn();
    sh.run_command("/bin/echo durable_before_kill");
    sh.type_str("/bin/sh -c 'kill -KILL $PPID'");
    sh.enter();
    sh.wait_for_exit(3000);

    let restarted = PtyShell::spawn_in_home(sh._home.clone(), &[], 24, 80, None);
    restarted.ctrl_r();
    restarted.wait_for("search:", 2000);
    restarted.type_str("durable_before_kill");
    restarted.wait_for_quiescence(200);
    restarted.enter();
    let text = PtyShell::strip_ansi(&restarted.wait_for_prompt_after_line_advance(2000));
    assert!(
        text.contains("/bin/echo durable_before_kill"),
        "lost durable command: {text:?}"
    );
    let h = History::load_from(restarted.home_path().join(".local/share/ish/history")).unwrap();
    assert!(h.prefix_search("/bin/sh -c 'kill -KILL", 0).is_some());
}

#[test]
fn history_recovers_partial_append_after_abrupt_termination() {
    let sh = PtyShell::spawn();
    sh.run_command("/bin/echo durable_before_partial_append");
    sh.type_str(
        "/bin/sh -c 'printf \"\\001\\002\\003\" >> .local/share/ish/history.log; kill -KILL $PPID'",
    );
    sh.enter();
    sh.wait_for_exit(3000);

    let log = sh.home_path().join(".local/share/ish/history.log");
    assert!(std::fs::read(&log).unwrap().ends_with(&[1, 2, 3]));
    let restarted = PtyShell::spawn_in_home(sh._home.clone(), &[], 24, 80, None);
    assert!(!restarted.startup_output().contains("history:"));
    let text =
        PtyShell::strip_ansi(&restarted.run_command("/bin/echo durable_after_partial_append"));
    assert!(
        !text.contains("command was not saved to history"),
        "partial append prevented saving: {text:?}"
    );
    let h = History::load_from(restarted.home_path().join(".local/share/ish/history")).unwrap();
    assert_eq!(
        h.prefix_search("/bin/echo durable_before_partial_append", 0),
        Some("/bin/echo durable_before_partial_append")
    );
    assert_eq!(
        h.prefix_search("/bin/echo durable_after_partial_append", 0),
        Some("/bin/echo durable_after_partial_append")
    );
}

#[test]
fn history_shared_shell_processes_keep_all_commands() {
    let first = PtyShell::spawn();
    let second = PtyShell::spawn_in_home(first._home.clone(), &[], 24, 80, None);
    for item in 0..2 {
        first.type_str(&format!("/bin/echo first_process_{item}"));
        second.type_str(&format!("/bin/echo second_process_{item}"));
        first.enter();
        second.enter();
        first.wait_for_line_advance(2000);
        second.wait_for_line_advance(2000);
        first.wait_for_prompt_after_line_advance(2000);
        second.wait_for_prompt_after_line_advance(2000);
    }
    first.type_str("exit");
    first.enter();
    first.wait_for_exit(3000);
    second.run_command("/bin/echo second_process_after_exit");

    second.ctrl_r();
    second.wait_for("search:", 2000);
    second.type_str("first_process_1");
    second.wait_for_quiescence(200);
    second.enter();
    let text = PtyShell::strip_ansi(&second.wait_for_prompt_after_line_advance(2000));
    assert!(
        text.contains("/bin/echo first_process_1"),
        "missing other process command: {text:?}"
    );

    let third = PtyShell::spawn_in_home(first._home.clone(), &[], 24, 80, None);
    let text = PtyShell::strip_ansi(&third.run_command("history"));
    for command in [
        "first_process_0",
        "first_process_1",
        "second_process_0",
        "second_process_1",
        "second_process_after_exit",
    ] {
        assert!(text.contains(command), "missing {command}: {text:?}");
    }
}

#[test]
fn history_write_failure_is_visible_and_command_still_runs() {
    let sh = PtyShell::spawn();
    let log = sh.home_path().join(".local/share/ish/history.log");
    let saved_log = sh.home_path().join("saved-history.log");
    std::fs::rename(&log, &saved_log).unwrap();
    std::fs::create_dir(&log).unwrap();
    let text = PtyShell::strip_ansi(&sh.run_command(r"printf 'executed_%s\n' despite_failure"));
    assert!(
        text.contains("command was not saved to history"),
        "missing history failure diagnostic: {text:?}"
    );
    assert!(
        text.contains("executed_despite_failure"),
        "command did not execute: {text:?}"
    );
    std::fs::remove_dir(&log).unwrap();
    std::fs::rename(saved_log, log).unwrap();
    let h = History::load_from(sh.home_path().join(".local/share/ish/history")).unwrap();
    assert!(h.prefix_search("printf", 0).is_none());
}

#[test]
fn history_read_only_commands_preserve_another_writer() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo startup"]);
    let path = sh.home_path().join(".local/share/ish/history");
    let mut writer = History::load_from(path.clone()).unwrap();
    writer.add("echo concurrent_history_marker").unwrap();
    sh.run_command("history");
    sh.run_command("history > history-output");
    let h = History::load_from(path).unwrap();
    assert_eq!(
        h.prefix_search("echo concurrent_history_marker", 0),
        Some("echo concurrent_history_marker")
    );
    let output = std::fs::read_to_string(sh.home_path().join("history-output")).unwrap();
    assert!(output.contains("echo concurrent_history_marker"));
}

#[test]
fn history_reset_reopens_storage_in_existing_shell_processes() {
    let first = PtyShell::spawn_with_opts(&[], &["echo before_reset_marker"]);
    let second = PtyShell::spawn_in_home(first._home.clone(), &[], 24, 80, None);
    first.run_command("history reset");
    let text = PtyShell::strip_ansi(&second.run_command("history"));
    assert!(
        !text.contains("before_reset_marker"),
        "stale process restored reset history: {text:?}"
    );

    second.run_command("/bin/echo after_reset_second_process");
    first.run_command("/bin/echo after_reset_first_process");
    let h = History::load_from(first.home_path().join(".local/share/ish/history")).unwrap();
    assert!(h.prefix_search("echo before_reset_marker", 0).is_none());
    assert_eq!(
        h.prefix_search("/bin/echo after_reset_second_process", 0),
        Some("/bin/echo after_reset_second_process")
    );
    assert_eq!(
        h.prefix_search("/bin/echo after_reset_first_process", 0),
        Some("/bin/echo after_reset_first_process")
    );
}

#[test]
fn history_readers_keep_startup_log_after_home_changes() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo startup_log_marker"]);
    std::fs::create_dir(sh.home_path().join("new-home")).unwrap();
    sh.run_command(&format!(
        "set HOME {}",
        sh.home_path().join("new-home").display()
    ));
    let text = PtyShell::strip_ansi(&sh.run_command("history"));
    assert!(
        text.contains("echo startup_log_marker"),
        "history listing lost original log: {text:?}"
    );
    sh.run_command("history > history-after-home-change");
    let output = std::fs::read_to_string(sh.home_path().join("history-after-home-change")).unwrap();
    assert!(output.contains("echo startup_log_marker"));
    assert!(
        !sh.home_path()
            .join("new-home/.local/share/ish/history.log")
            .exists()
    );
}

#[test]
fn history_reset_refuses_pipeline_and_command_substitution() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo preserved_history_marker"]);
    for command in ["history reset | /bin/cat", "echo $(history reset)"] {
        let text = PtyShell::strip_ansi(&sh.run_command(command));
        assert!(
            text.contains("requires the original shell process"),
            "missing history refusal diagnostic: {text:?}"
        );
        let h = History::load_from(sh.home_path().join(".local/share/ish/history")).unwrap();
        assert_eq!(
            h.prefix_search("echo preserved_history_marker", 0),
            Some("echo preserved_history_marker")
        );
    }
}

#[test]
fn history_up_ignores_later_global_entries() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo startup"]);
    let mut writer = History::load_from(sh.home_path().join(".local/share/ish/history")).unwrap();
    writer.add("echo later_global").unwrap();
    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history search closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });
    sh.up();
    sh.enter();
    let text = PtyShell::strip_ansi(&sh.wait_for_prompt_after_line_advance(2000));
    assert!(
        text.contains("echo startup"),
        "missing startup recall: {text:?}"
    );
    assert!(
        !text.contains("later_global"),
        "global write changed Up recall: {text:?}"
    );
}

#[test]
fn history_help() {
    let sh = PtyShell::spawn();
    let text = PtyShell::strip_ansi(&sh.run_command("history -h"));
    assert!(
        text.contains("Usage: history [reset]"),
        "expected history help text: {text:?}"
    );
}

#[test]
fn history_autosuggest_ignores_later_global_entries() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo startup"]);
    let hist_path = sh.home_path().join(".local/share/ish/history");
    let mut other_shell = History::load_from(hist_path).unwrap();
    other_shell.add("echo later_global").unwrap();

    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    let before_escape = sh.screen();
    sh.escape();
    sh.wait_for_screen("history search closed", 2000, |screen| {
        screen != &before_escape && !screen.contains("search:")
    });

    sh.type_str("echo l");
    sh.right();
    sh.enter();
    let out = sh.wait_for_prompt_after_line_advance(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        !text.contains("later_global"),
        "later global history entry leaked into autosuggest acceptance: {text:?}"
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

fn denv_trust_key(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut key = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    key
}

/// SHA-256 hex of `path`'s bytes via the system tool, so the tests check the
/// on-disk trust format independently of the implementation's own hasher.
fn sha256_hex_of_file(path: &Path) -> String {
    let out = ["sha256sum", "shasum"]
        .iter()
        .find_map(|tool| {
            let mut cmd = std::process::Command::new(tool);
            if *tool == "shasum" {
                cmd.args(["-a", "256"]);
            }
            cmd.arg(path).output().ok().filter(|o| o.status.success())
        })
        .expect("sha256sum or shasum is required by the denv tests");
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

/// Grants trust to a `.envrc` or `.env` in the on-disk format: the file is
/// keyed by its canonical path and holds the SHA-256 of its content.
fn allow_envrc(home: &Path, rel: &str) {
    let file = home.join(rel).canonicalize().unwrap();
    let allow_dir = home.join(".local/share/denv/allow");
    std::fs::create_dir_all(&allow_dir).unwrap();
    std::fs::write(allow_dir.join(denv_trust_key(&file)), sha256_hex_of_file(&file)).unwrap();
}

fn spawn_with_allowed(files: &[(&str, &str)], allowed: &[&str]) -> PtyShell {
    PtyShell::spawn_with_setup(files, &[], &[], 24, 80, None, |home| {
        for rel in allowed {
            allow_envrc(home, rel);
        }
    })
}

fn spawn_with_allowed_envrc(files: &[(&str, &str)], rel_envrc: &str) -> PtyShell {
    PtyShell::spawn_with_setup(files, &[], &[], 24, 80, None, |home| {
        allow_envrc(home, rel_envrc);
    })
}

fn spawn_in_dir_with_allowed_envrc(
    files: &[(&str, &str)],
    rel_envrc: &str,
    cwd_rel: &str,
) -> PtyShell {
    PtyShell::spawn_with_setup(files, &[], &[], 24, 80, Some(cwd_rel), |home| {
        allow_envrc(home, rel_envrc);
    })
}

#[test]
fn denv_loads_allowed_envrc_on_cd() {
    let sh = spawn_with_allowed_envrc(
        &[("project/.envrc", "export DENV_TEST_VAR='loaded'\n")],
        "project/.envrc",
    );
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(
        text.contains("loaded"),
        "expected envrc var after cd: {text:?}"
    );
}

#[test]
fn denv_loads_dotenv_on_cd_only_after_allow() {
    let sh = PtyShell::spawn_with_opts(
        &[("project/.env", "DENV_TEST_VAR=loaded\nPATH=/nonexistent\n")],
        &[],
    );
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("echo =$DENV_TEST_VAR= $PATH"));
    assert!(
        text.contains("==") && !text.contains("/nonexistent"),
        "un-allowed .env must not change the environment: {text:?}"
    );

    sh.run_command("denv allow");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(
        text.contains("loaded"),
        "expected dotenv var after denv allow: {text:?}"
    );
}

#[test]
fn denv_unloads_on_leave() {
    let sh = spawn_with_allowed_envrc(
        &[("project/.envrc", "export DENV_TEST_VAR='active'\n")],
        "project/.envrc",
    );
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(text.contains("active"), "should be loaded: {text:?}");

    sh.run_command("cd ..");
    let text = PtyShell::strip_ansi(&sh.run_command("echo =$DENV_TEST_VAR="));
    assert!(
        text.contains("=="),
        "var should be unset after leaving: {text:?}"
    );
}

fn denv_active_records(sh: &PtyShell) -> Vec<String> {
    let dir = sh.home_path().join(".local/share/denv");
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("active_"))
        .collect()
}

#[test]
fn denv_exit_command_inside_project_removes_active_record() {
    let sh = spawn_with_allowed_envrc(
        &[("project/.envrc", "export DENV_TEST_VAR='active'\n")],
        "project/.envrc",
    );
    sh.run_command("cd project");
    assert_eq!(denv_active_records(&sh).len(), 1);

    sh.type_str("exit");
    sh.enter();
    sh.wait_for_exit(3000);

    assert!(denv_active_records(&sh).is_empty());
}

#[test]
fn denv_ctrl_d_inside_project_removes_active_record() {
    let sh = spawn_with_allowed_envrc(
        &[("project/.envrc", "export DENV_TEST_VAR='active'\n")],
        "project/.envrc",
    );
    sh.run_command("cd project");
    assert_eq!(denv_active_records(&sh).len(), 1);

    sh.ctrl_d();
    sh.wait_for_exit(10_000);

    assert!(denv_active_records(&sh).is_empty());
}

#[test]
fn denv_allow_applies_env() {
    let sh = PtyShell::spawn_with_opts(
        &[("project/.envrc", "export DENV_TEST_VAR='allowed'\n")],
        &[],
    );
    sh.run_command("cd project");
    let blocked = PtyShell::strip_ansi(&sh.run_command("echo =$__DENV_DIRTY="));
    assert!(
        blocked.contains("=1="),
        "expected dirty state before allow: {blocked:?}"
    );

    sh.run_command("denv allow");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(
        text.contains("allowed"),
        "expected var after denv allow: {text:?}"
    );
}

#[test]
fn denv_deny_removes_env_and_marks_dirty() {
    let sh = spawn_with_allowed_envrc(
        &[("project/.envrc", "export DENV_TEST_VAR='loaded'\n")],
        "project/.envrc",
    );
    sh.run_command("cd project");
    sh.run_command("denv deny");
    let text = PtyShell::strip_ansi(&sh.run_command("echo =$DENV_TEST_VAR=:$__DENV_DIRTY="));
    assert!(
        text.contains("==:1"),
        "expected var unset and dirty after deny: {text:?}"
    );
}

#[test]
fn denv_startup_loads_allowed_dotenv_in_initial_cwd() {
    let sh = PtyShell::spawn_with_setup(
        &[("project/.env", "DENV_TEST_VAR=from_startup\n")],
        &[],
        &[],
        24,
        80,
        Some("project"),
        |home| allow_envrc(home, "project/.env"),
    );
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(
        text.contains("from_startup"),
        "expected allowed dotenv to load on startup: {text:?}"
    );
}

#[test]
fn denv_startup_loads_allowed_envrc_in_initial_cwd() {
    let sh = spawn_in_dir_with_allowed_envrc(
        &[("project/.envrc", "export DENV_TEST_VAR='from_startup'\n")],
        "project/.envrc",
        "project",
    );
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(
        text.contains("from_startup"),
        "expected envrc to load on startup: {text:?}"
    );
}

#[test]
fn denv_dotenv_overrides_envrc() {
    let sh = spawn_with_allowed(
        &[
            (
                "project/.envrc",
                "export SHARED='from_envrc'\nexport ENVRC_ONLY='1'\n",
            ),
            ("project/.env", "SHARED=from_dotenv\nDOTENV_ONLY=1\n"),
        ],
        &["project/.envrc", "project/.env"],
    );
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $SHARED $ENVRC_ONLY $DOTENV_ONLY"));
    assert!(
        text.contains("from_dotenv"),
        "expected dotenv override: {text:?}"
    );
    assert!(
        text.contains("1 1"),
        "expected both unique vars present: {text:?}"
    );
}

#[test]
fn denv_reload_after_reallow_picks_up_envrc_edit() {
    let sh = spawn_with_allowed_envrc(
        &[("project/.envrc", "export DENV_TEST_VAR='old'\n")],
        "project/.envrc",
    );
    sh.run_command("cd project");

    std::fs::write(
        sh.home_path().join("project/.envrc"),
        "export DENV_TEST_VAR='updated'\n",
    )
    .unwrap();

    sh.run_command("denv allow");
    sh.run_command("denv reload");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(
        text.contains("updated"),
        "expected updated var after reload: {text:?}"
    );
}

#[test]
fn denv_edit_envrc_invalidates_trust() {
    let sh = spawn_with_allowed_envrc(
        &[("project/.envrc", "export DENV_TEST_VAR='old'\n")],
        "project/.envrc",
    );
    sh.run_command("cd project");

    std::fs::write(
        sh.home_path().join("project/.envrc"),
        "export DENV_TEST_VAR='changed'\n",
    )
    .unwrap();

    let reload = PtyShell::strip_ansi(&sh.run_command("denv reload"));
    assert!(
        reload.contains("blocked"),
        "expected blocked warning after edit: {reload:?}"
    );
    let text = PtyShell::strip_ansi(&sh.run_command("echo =$DENV_TEST_VAR=:$__DENV_DIRTY="));
    assert!(
        text.contains("==:1"),
        "expected dirty state after invalidation: {text:?}"
    );
}

#[test]
fn denv_restores_preexisting_var_on_leave() {
    let sh = PtyShell::spawn_with_setup(
        &[("project/.envrc", "export EXISTING='inside'\n")],
        &[],
        &[("EXISTING", "outside")],
        24,
        80,
        None,
        |home| allow_envrc(home, "project/.envrc"),
    );
    sh.run_command("cd project");
    let inside = PtyShell::strip_ansi(&sh.run_command("echo $EXISTING"));
    assert!(
        inside.contains("inside"),
        "expected overridden value in project: {inside:?}"
    );

    sh.run_command("cd ..");
    let outside = PtyShell::strip_ansi(&sh.run_command("echo $EXISTING"));
    assert!(
        outside.contains("outside"),
        "expected original value restored: {outside:?}"
    );
}

#[test]
fn denv_path_add_relative_dir() {
    let sh = spawn_with_allowed_envrc(
        &[
            ("project/.envrc", "PATH_add bin\n"),
            ("project/bin/tool", "#!/bin/sh\nexit 0\n"),
        ],
        "project/.envrc",
    );
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $PATH"));
    assert!(
        text.contains(
            &sh.home_path()
                .join("project/bin")
                .to_string_lossy()
                .into_owned()
        ),
        "expected PATH_add to prepend project/bin: {text:?}"
    );
}

#[test]
fn denv_dotenv_helper_rejects_the_entire_candidate() {
    let sh = spawn_with_allowed(
        &[
            ("project/.envrc", "export BEFORE=1\ndotenv || true\nexport AFTER=1\n"),
            ("project/.env", "FROM_ENV=loaded\n"),
        ],
        &["project/.envrc", "project/.env"],
    );
    let entered = PtyShell::strip_ansi(&sh.run_command("cd project"));
    assert!(entered.contains("dotenv") && entered.contains("removed"), "{entered:?}");
    let text = PtyShell::strip_ansi(&sh.run_command("echo =$FROM_ENV=:$BEFORE=:$AFTER=:$__DENV_DIRTY="));
    assert!(
        text.contains("==:=:=:1="),
        "rejected candidate leaked variables or failed to mark dirty: {text:?}"
    );
    assert!(sh.run_command("echo still-usable").contains("still-usable"));
}

#[test]
fn denv_automatic_dotenv_loads_without_a_helper() {
    let sh = spawn_with_allowed(
        &[
            ("project/.envrc", "export AFTER=1\n"),
            ("project/.env", "FROM_ENV=loaded\n"),
        ],
        &["project/.envrc", "project/.env"],
    );
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $FROM_ENV $AFTER"));
    assert!(text.contains("loaded 1"), "{text:?}");
}

#[test]
fn denv_automatic_dotenv_keeps_shell_text_as_data_and_revokes_edits() {
    let sh = spawn_with_allowed(
        &[
            ("project/.envrc", "export DENV_PAIR=envrc\n"),
            ("project/.env", "DENV_PAIR=dotenv\nDENV_LITERAL=$(touch marker); `touch marker`; \"quoted\"\n"),
        ],
        &["project/.envrc", "project/.env"],
    );
    sh.run_command("cd project");
    let loaded = PtyShell::strip_ansi(&sh.run_command("echo $DENV_PAIR $DENV_LITERAL"));
    assert!(loaded.contains("dotenv $(touch marker); `touch marker`; \"quoted\""), "{loaded:?}");
    assert!(!sh.home_path().join("project/marker").exists());
    std::fs::write(sh.home_path().join("project/.env"), "DENV_PAIR=edited\n").unwrap();
    let blocked = PtyShell::strip_ansi(&sh.run_command("denv reload"));
    assert!(blocked.contains("blocked"), "{blocked:?}");
    let rejected = PtyShell::strip_ansi(&sh.run_command("echo =$DENV_PAIR=:$DENV_LITERAL=:$__DENV_DIRTY="));
    assert!(rejected.contains("==:=:1="), "{rejected:?}");
    sh.run_command("denv allow");
    assert!(sh.run_command("echo $DENV_PAIR").contains("edited"));
    sh.run_command("denv deny");
    let denied = PtyShell::strip_ansi(&sh.run_command("echo =$DENV_PAIR=:$__DENV_DIRTY="));
    assert!(denied.contains("==:1="), "{denied:?}");
}

#[test]
fn denv_rejects_parent_helpers_after_parent_revocation_or_edit() {
    for bash in [false, true] {
        for state in ["untrusted", "denied", "edited"] {
            let child_source = format!("{}export CANDIDATE=before\nsource_up || true\nexport CANDIDATE=after\n", if bash { "#!/usr/bin/env bash\n" } else { "" });
            let allowed = if state == "untrusted" {
                vec!["parent/project/.envrc"]
            } else {
                vec!["parent/.envrc", "parent/project/.envrc"]
            };
            let sh = spawn_with_allowed(
                &[
                    ("parent/.envrc", "export PARENT_VALUE=old\nprintf touched > parent-marker\n"),
                    ("parent/project/.envrc", &child_source),
                ],
                &allowed,
            );
            if state == "denied" {
                sh.run_command("cd parent");
                sh.run_command("denv deny");
                sh.run_command("cd ..");
                std::fs::remove_file(sh.home_path().join("parent/parent-marker")).unwrap();
            } else if state == "edited" {
                std::fs::write(sh.home_path().join("parent/.envrc"), "export PARENT_VALUE=changed\nprintf touched > parent-marker\n").unwrap();
            }
            let entered = PtyShell::strip_ansi(&sh.run_command("cd parent/project"));
            assert!(entered.contains("source_up") && entered.contains("removed"), "{state}: {entered:?}");
            let text = PtyShell::strip_ansi(&sh.run_command("echo =$PARENT_VALUE=:$CANDIDATE=:$__DENV_DIRTY="));
            assert!(text.contains("==:=:1="), "{state}: {text:?}");
            assert!(!sh.home_path().join("parent/project/parent-marker").exists());
            assert!(!sh.home_path().join("parent/parent-marker").exists());
        }
    }
}

#[test]
fn denv_rejects_changed_shared_file_helpers() {
    for bash in [false, true] {
        let source = format!("{}export BEFORE=1\nsource_env shared || true\nexport AFTER=1\n", if bash { "#!/usr/bin/env bash\n" } else { "" });
        let sh = spawn_with_allowed(
            &[
                ("project/.envrc", &source),
                ("project/shared", "export SHARED=original\n"),
            ],
            &["project/.envrc"],
        );
        std::fs::write(sh.home_path().join("project/shared"), "export SHARED=edited\nprintf touched > shared-marker\n").unwrap();
        let entered = PtyShell::strip_ansi(&sh.run_command("cd project"));
        assert!(entered.contains("source_env") && entered.contains("removed"), "{entered:?}");
        let text = PtyShell::strip_ansi(&sh.run_command("echo =$BEFORE=:$AFTER=:$SHARED=:$__DENV_DIRTY="));
        assert!(text.contains("==:=:=:1="), "{text:?}");
        assert!(!sh.home_path().join("project/shared-marker").exists());
    }
}

#[test]
fn denv_allow_without_any_env_file_fails() {
    let sh = PtyShell::spawn_with_opts(&[("project/note.txt", "x\n")], &[]);
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("denv allow"));
    assert!(
        text.contains("no .envrc or .env"),
        "expected allow failure without env files: {text:?}"
    );
}

#[test]
fn ctrl_c_interrupts_running_child() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(3000);

    let out = sh.run_command("echo alive");
    assert!(out.contains("alive"), "shell unresponsive after ^C: {out:?}");
}

#[test]
fn ctrl_d_with_stopped_job_warns_then_exits() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();
    sh.wait_for("stopped:", 3000);
    sh.wait_for_prompt_after_line_advance(3000);

    sh.ctrl_d();
    let out = sh.wait_for("suspended job", 3000);
    assert!(out.contains("Ctrl+D again"), "expected warning: {out:?}");
    sh.ctrl_d();
    sh.wait_for_exit(3000);
}

#[test]
fn exit_twice_with_stopped_job_warns_then_exits() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();
    sh.wait_for("stopped:", 3000);
    sh.wait_for_prompt_after_line_advance(3000);

    sh.type_str("exit");
    sh.enter();
    let out = sh.wait_for("suspended job", 3000);
    assert!(out.contains("Exit again"), "expected warning: {out:?}");
    sh.wait_for_prompt_after_line_advance(3000);
    sh.type_str("exit");
    sh.enter();
    sh.wait_for_exit(3000);
}

#[test]
fn confirmed_exit_with_stopped_job_keeps_the_requested_status() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();
    sh.wait_for("stopped:", 3000);
    sh.wait_for_prompt_after_line_advance(3000);

    sh.type_str("exit 3");
    sh.enter();
    sh.wait_for("suspended job", 3000);
    sh.wait_for_prompt_after_line_advance(3000);
    sh.type_str("exit 3");
    sh.enter();
    assert_eq!(sh.wait_for_exit_status(3000), ExitStatus::Code(3));
}

#[test]
fn pipeline_suspend_and_resume_retains_every_stage() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep | cat");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();
    let stopped = sh.wait_for("stopped:", 3000);
    assert!(stopped.contains("stopped:"), "pipeline stop was lost: {stopped:?}");
    sh.wait_for_prompt_after_line_advance(3000);
    sh.type_str("fg");
    sh.enter();
    let resumed = sh.wait_for("resuming:", 3000);
    assert!(resumed.contains("resuming:"), "pipeline job was lost: {resumed:?}");
    sh.wait_for_quiescence(100);
    sh.ctrl_c();
    let prompt = sh.wait_for_prompt_after_line_advance(3000);
    assert!(prompt.contains("$ "), "pipeline did not finish: {prompt:?}");
    assert!(sh.run_command("echo pipeline-alive").contains("pipeline-alive"));
}

#[test]
fn native_handlers_refuse_child_execution_before_target_redirections() {
    let sh = PtyShell::spawn();
    for name in ["l", "c", "history"] {
        for command in [
            format!("command {name} >native-target | /bin/cat"),
            format!("echo $(command {name} >native-target)"),
        ] {
            let output = PtyShell::strip_ansi(&sh.run_command(&command));
            assert!(output.contains("requires the original shell process"), "native callback entered a child: {command}: {output:?}");
            assert!(!sh.home_path().join("native-target").exists());
        }
    }
    assert!(PtyShell::strip_ansi(&sh.run_command("printf parent-safe | /bin/cat")).contains("parent-safe"));
    assert!(PtyShell::strip_ansi(&sh.run_command("echo $(printf substitution-safe)")).contains("substitution-safe"));
    sh.run_command("l >native-parent");
    assert!(std::fs::read_to_string(sh.home_path().join("native-parent")).unwrap().contains(".config"));
}

#[test]
fn external_input_redirections_preserve_foreground_terminal_ownership() {
    let sh = PtyShell::spawn_with_opts(&[("input", "redirected-input\n")], &[]);
    let output = PtyShell::strip_ansi(&sh.run_command("/bin/cat <input"));
    assert!(output.contains("redirected-input"), "input redirection broke terminal handoff: {output:?}");
    sh.run_command("/bin/sh -c 'exit 0' 0<&-");
    let output = PtyShell::strip_ansi(&sh.run_command("echo $?"));
    assert!(output.lines().any(|line| line.trim() == "0"), "closed input broke command execution: {output:?}");
}

#[test]
fn ordinary_exit_148_does_not_create_a_suspended_job() {
    let sh = PtyShell::spawn_with_opts(&[("bin/exit148", "#!/bin/sh\nexit 148\n")], &[]);
    sh.run_command("./bin/exit148");
    let output = PtyShell::strip_ansi(&sh.run_command("fg"));
    assert!(output.contains("no suspended job"), "numeric status created a job: {output:?}");
}

#[test]
fn occupied_job_slot_restores_new_job_terminal_modes_before_continuing() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();
    sh.wait_for("stopped:", 3000);
    sh.wait_for_prompt_after_line_advance(3000);
    let baseline = stty_state(&sh.run_command("stty -g"));
    sh.run_command("sh -c 'stty raw -echo; stty -g >raw-before; kill -STOP $$; stty -g >raw-after; stty sane'");
    assert_eq!(
        std::fs::read(sh.home_path().join("raw-before")).unwrap(),
        std::fs::read(sh.home_path().join("raw-after")).unwrap()
    );
    assert_eq!(stty_state(&sh.run_command("stty -g")), baseline);
    sh.type_str("fg");
    sh.enter();
    sh.wait_for("resuming:", 3000);
    sh.wait_for_quiescence(100);
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(3000);
}

#[test]
fn stopped_pipeline_discards_the_remaining_list() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep | /bin/cat; /bin/touch forbidden-remainder");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();
    sh.wait_for("stopped:", 3000);
    sh.wait_for_prompt_after_line_advance(3000);
    sh.type_str("fg");
    sh.enter();
    sh.wait_for("resuming:", 3000);
    sh.wait_for_quiescence(100);
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(3000);
    assert!(!sh.home_path().join("forbidden-remainder").exists());
}

#[test]
fn job_suspend_and_resume() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();

    let out = sh.wait_for("stopped:", 3000);
    assert!(
        out.contains("stopped:") && out.contains("sleep"),
        "expected stop notification: {out:?}"
    );
    sh.wait_for_prompt_after_line_advance(3000);

    sh.type_str("fg");
    sh.enter();

    let out = sh.wait_for("resuming:", 3000);
    assert!(
        out.contains("resuming:") && out.contains("sleep"),
        "expected resume notification: {out:?}"
    );

    sh.wait_for_quiescence(100);
    sh.ctrl_c();

    let out = sh.wait_for_prompt_after_line_advance(3000);
    assert!(
        out.contains("$ "),
        "shell did not return a prompt after fg: {out:?}"
    );

    let out = sh.run_command("echo alive");
    assert!(
        out.contains("alive"),
        "shell unresponsive after job resume: {out:?}"
    );
}

#[test]
fn pipeline_suspend_and_resume_keeps_job_owned() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep | cat");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();

    let out = sh.wait_for("stopped:", 3000);
    assert!(out.contains("stopped:"), "expected pipeline stop notification: {out:?}");
    sh.wait_for_prompt_after_line_advance(3000);

    sh.type_str("fg");
    sh.enter();
    let out = sh.wait_for("resuming:", 3000);
    assert!(out.contains("resuming:"), "expected pipeline resume notification: {out:?}");
    sh.wait_for_quiescence(100);
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(3000);

    let out = sh.run_command("echo alive");
    assert!(out.contains("alive"), "shell did not recover after pipeline resume: {out:?}");
}

fn stty_state(output: &str) -> String {
    if let Some(start) = output.find("gfmt1:") {
        return output[start..]
            .split_whitespace()
            .next()
            .expect("stty state follows gfmt1 marker")
            .to_string();
    }
    output
        .split(|c: char| !(c.is_ascii_hexdigit() || c == ':'))
        .find(|token| token.matches(':').count() >= 8)
        .unwrap_or_else(|| panic!("no `stty -g` state in {output:?}"))
        .to_string()
}

#[test]
fn killed_child_in_raw_mode_does_not_leak_into_the_next_command() {
    let sh = PtyShell::spawn();
    let baseline = stty_state(&sh.run_command("stty -g"));
    sh.run_command("sh -c 'stty raw -echo; kill -KILL $$'");
    let next = stty_state(&sh.run_command("stty -g"));
    assert_eq!(next, baseline, "killed child's raw mode reached the next command");
}

#[test]
fn stopped_child_in_raw_mode_restores_baseline_then_fg_restores_its_own_mode() {
    let sh = PtyShell::spawn();
    let baseline = stty_state(&sh.run_command("stty -g"));
    sh.run_command("sh -c 'stty raw -echo; kill -STOP $$'");
    let at_stop = stty_state(&sh.run_command("stty -g"));
    assert_eq!(at_stop, baseline, "stopped child's raw mode reached the prompt");
    sh.run_command("fg");
    let after_fg = stty_state(&sh.run_command("stty -g"));
    assert_eq!(after_fg, baseline, "resumed child's raw mode reached the next command after fg");
}

fn clipboard_writes(raw: &str) -> Vec<Vec<u8>> {
    const PREFIX: &str = "\x1b]52;c;";
    let mut writes = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find(PREFIX) {
        let payload = &rest[start + PREFIX.len()..];
        let end = payload.find('\x07').expect("OSC 52 is terminated");
        writes.push(decode_base64(&payload[..end]));
        rest = &payload[end..];
    }
    writes
}

fn decode_base64(encoded: &str) -> Vec<u8> {
    let value = |byte: u8| match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        other => panic!("invalid base64 byte {other}"),
    };
    let mut out = Vec::new();
    let mut bits = 0u32;
    let mut count = 0;
    for byte in encoded.bytes().take_while(|&byte| byte != b'=') {
        bits = bits << 6 | u32::from(value(byte));
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    out
}

fn copied_by_question_mark(sh: &PtyShell) -> Vec<u8> {
    sh.type_str("?");
    sh.enter();
    let output = sh.wait_for_prompt_after_line_advance(3000);
    let mut writes = clipboard_writes(&output);
    assert_eq!(writes.len(), 1, "expected one clipboard write: {output:?}");
    writes.remove(0)
}

#[test]
fn question_mark_copies_terminal_output_of_pipelines_and_children() {
    let sh = PtyShell::spawn_reading_clipboard(&[], 24, 80);
    sh.run_command("echo hello | tr a-z A-Z");
    assert_eq!(copied_by_question_mark(&sh), b"HELLO\n");

    sh.run_command("sh -c 'echo out; echo err >&2'");
    assert_eq!(copied_by_question_mark(&sh), b"out\nerr\n");
}

#[test]
fn question_mark_keeps_children_on_a_terminal_of_the_current_size() {
    let sh = PtyShell::spawn_reading_clipboard(&[], 10, 40);
    sh.run_command("sh -c 'test -t 1 && test -t 2 && stty size <&1'");
    assert_eq!(copied_by_question_mark(&sh), b"10 40\n");
}

#[test]
fn running_command_sees_a_resize_through_its_output_terminal() {
    let script =
        "#!/bin/sh\ntrap 'stty size <&1' WINCH\necho ready\nwhile :; do sleep 0.05; done\n";
    let sh = PtyShell::spawn_reading_clipboard(&[("bin/winch", script)], 24, 80);
    sh.type_str("./bin/winch");
    sh.enter();
    sh.wait_for("ready", 3000);
    sh.terminal
        .borrow_mut()
        .resize(Size::new(50, 12).expect("non-zero size"))
        .expect("resize terminal");
    sh.wait_for("12 50", 3000);
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(3000);
    let copied = copied_by_question_mark(&sh);
    assert!(copied.starts_with(b"ready\n") && copied.ends_with(b"12 50\n"), "{copied:?}");
}

#[test]
fn question_mark_excludes_redirected_streams_and_strips_terminal_controls() {
    let sh = PtyShell::spawn_reading_clipboard(&[], 24, 80);
    sh.run_command("echo shown; echo hidden > hidden.txt; echo err 2>&1 >&2");
    assert_eq!(copied_by_question_mark(&sh), b"shown\nerr\n");

    sh.run_command("printf '\\033[31mred\\033[0m\\n'");
    assert_eq!(copied_by_question_mark(&sh), b"red\n");

    sh.run_command("echo hidden > hidden.txt");
    sh.type_str("?");
    sh.enter();
    let output = sh.wait_for("wrote no text", 3000);
    assert!(clipboard_writes(&output).is_empty(), "{output:?}");
    assert!(sh.run_command("echo $?").contains('1'));
}

#[test]
fn question_mark_does_not_replace_the_output_it_copies() {
    let sh = PtyShell::spawn_reading_clipboard(&[], 24, 80);
    sh.run_command("echo kept");
    assert_eq!(copied_by_question_mark(&sh), b"kept\n");
    assert_eq!(copied_by_question_mark(&sh), b"kept\n");
}

#[test]
fn question_mark_follows_a_suspended_job_to_completion() {
    let script = "#!/bin/sh\nprintf 'before\\n'\nkill -STOP $$\nprintf 'after\\n'\n";
    let sh = PtyShell::spawn_reading_clipboard(&[("bin/stopper", script)], 24, 80);
    sh.type_str("./bin/stopper");
    sh.enter();
    sh.wait_for("stopped:", 3000);
    sh.wait_for_prompt_after_line_advance(3000);
    sh.type_str("fg");
    sh.enter();
    sh.wait_for("after", 3000);
    sh.wait_for_prompt_after_line_advance(3000);
    assert_eq!(copied_by_question_mark(&sh), b"before\nafter\n");
}

#[test]
fn output_beyond_the_capture_limit_still_reaches_the_terminal_and_the_shell_continues() {
    let sh = PtyShell::spawn_reading_clipboard(&[], 24, 80);
    sh.type_str("seq 1 400000");
    sh.enter();
    sh.wait_for("400000", 20000);
    sh.wait_for_prompt_after_line_advance(5000);
    assert!(sh.run_command("echo alive").contains("alive"));
}

/// Paste a 9 KiB payload via bracketed paste and verify the shell
/// instantly rejects it with "[paste exceeded 8KB limit]".
#[test]
fn bracketed_paste_over_limit_rejected() {
    let content: String = "x".repeat(9 * 1024);
    assert!(content.len() > 8192);

    let sh = PtyShell::spawn();
    sh.send(b"echo ");

    let mut payload = Vec::with_capacity(6 + content.len() + 6);
    payload.extend_from_slice(b"\x1b[200~");
    payload.extend_from_slice(content.as_bytes());
    payload.extend_from_slice(b"\x1b[201~");

    sh.send(&payload);

    let out = sh.wait_for("\x07", 5000);
    let clean = PtyShell::strip_ansi(&out);

    assert!(
        !clean.contains("[paste exceeded 8KB limit]"),
        "a rejected paste must not insert text into the line: {clean:?}"
    );
    assert!(
        !clean.contains("xxxxx"),
        "paste content appeared on screen — limit bypassed: {clean:?}"
    );
    let out2 = sh.run_command("kept");
    let clean2 = PtyShell::strip_ansi(&out2);
    assert!(
        clean2.lines().any(|line| line.trim() == "kept"),
        "line was altered by the rejected paste: {clean2:?}"
    );
}

#[test]
fn bracketed_paste_large_markdown_rejected() {
    let content = format!(
        "# Test document\n\n{}",
        "- Synthetic content for the paste limit test.\n".repeat(256)
    );
    assert!(content.len() > 8192);

    let sh = PtyShell::spawn();

    let mut payload = Vec::with_capacity(6 + content.len() + 6);
    payload.extend_from_slice(b"\x1b[200~");
    payload.extend_from_slice(content.as_bytes());
    payload.extend_from_slice(b"\x1b[201~");

    sh.send(&payload);

    let out = sh.wait_for("\x07", 5000);
    let clean = PtyShell::strip_ansi(&out);
    assert!(
        !clean.contains("# Test document"),
        "paste content appeared on screen: {clean:?}"
    );

    let out = sh.run_command("echo ok");
    let clean = PtyShell::strip_ansi(&out);
    assert!(clean.contains("ok"), "shell unresponsive after paste: {clean:?}");
}

/// Paste a small payload under the 1 KiB limit and verify the shell accepts
/// it — the text should appear in the line buffer ready for editing.
#[test]
fn bracketed_paste_under_limit_accepted() {
    let sh = PtyShell::spawn();
    let content = "echo hello world";
    assert!(content.len() <= 1024);

    let mut payload = Vec::with_capacity(6 + content.len() + 6);
    payload.extend_from_slice(b"\x1b[200~");
    payload.extend_from_slice(content.as_bytes());
    payload.extend_from_slice(b"\x1b[201~");

    sh.send(&payload);

    // The paste should be sitting in the line buffer — press Enter to run it.
    let out = sh.run_command(""); // just Enter
    let clean = PtyShell::strip_ansi(&out);
    assert!(
        clean.contains("hello world"),
        "paste was not accepted into line buffer: {clean:?}"
    );
}

/// Paste exactly 1024 bytes — right at the limit — which should still be
/// accepted (limit is > 1024, not >=).
#[test]
fn bracketed_paste_exactly_at_limit_accepted() {
    let sh = PtyShell::spawn();
    let content: String = "x".repeat(1024);
    assert_eq!(content.len(), 1024);

    let mut payload = Vec::with_capacity(6 + content.len() + 6);
    payload.extend_from_slice(b"\x1b[200~");
    payload.extend_from_slice(content.as_bytes());
    payload.extend_from_slice(b"\x1b[201~");

    sh.send(&payload);

    // The 1024 'x's should be in the line buffer (not rejected).  Verify
    // by appending " ok" via normal typing and running with echo.
    sh.type_str(" ok");
    let out = sh.run_command("");
    let clean = PtyShell::strip_ansi(&out);
    assert!(
        !clean.contains("[paste exceeded 8KB limit]"),
        "1024-byte paste was wrongly rejected: {clean:?}"
    );
}

#[test]
fn interactive_final_typeahead_after_enter_survives_the_next_prompt() {
    let sh = PtyShell::spawn();
    sh.send(b"echo first > first\recho second > second\recho $((12345+67890))\r");
    sh.wait_for("80235", 3000);

    assert!(sh.home_path().join("first").exists());
    assert!(sh.home_path().join("second").exists());
}

#[test]
fn interactive_final_sigterm_interrupts_oversized_paste_drain() {
    let sh = PtyShell::spawn();
    let mut payload = Vec::with_capacity(6 + 9 * 1024);
    payload.extend_from_slice(b"\x1b[200~");
    payload.extend(std::iter::repeat_n(b'x', 9 * 1024));
    sh.send(&payload);
    sh.wait_for_quiescence(100);

    let mut terminal = sh.terminal.borrow_mut();
    terminal.signal(libc::SIGTERM).unwrap();
    let deadline = terminal.deadline(std::time::Duration::from_secs(2));
    let status = terminal.wait_for_exit(deadline).expect("ish exits after SIGTERM");
    assert_eq!(status, ExitStatus::Code(143));
    terminal
        .assert_terminal_restored(&sh.terminal_baseline)
        .expect("terminal modes are restored after SIGTERM during paste");
}

#[test]
fn interactive_final_failed_shell_builtins_update_question_status() {
    let sh = PtyShell::spawn();
    let failed_cd = sh.run_command("cd missing-directory");
    assert!(failed_cd.contains("missing-directory"));
    let status = PtyShell::strip_ansi(&sh.run_command("echo builtin-status-$?"));
    assert!(
        status.lines().any(|line| line.trim() == "builtin-status-1"),
        "expected failed cd status 1, got {status:?}"
    );

    let rejected = sh.run_command("z project && true");
    assert!(
        rejected.contains("z changes shell state"),
        "expected out-of-line z refusal, got {rejected:?}"
    );
    let status = PtyShell::strip_ansi(&sh.run_command("echo rejected-status-$?"));
    assert!(
        status.lines().any(|line| line.trim() == "rejected-status-2"),
        "expected rejected command status 2, got {status:?}"
    );
}

#[test]
fn interactive_final_z_matches_quoted_directory_history() {
    let sh = PtyShell::spawn_with_setup(&[], &[], &[], 24, 120, None, |home| {
        std::fs::create_dir(home.join("project space")).unwrap();
    });
    sh.run_command("cd 'project space'");
    sh.run_command("cd ..");
    let output = PtyShell::strip_ansi(&sh.run_command("z project"));
    assert!(
        !output.contains("no match"),
        "quoted directory history should match z: {output:?}"
    );
    sh.run_command("pwd > z-pwd");
    let target = sh.home_path().join("project space");
    assert_eq!(
        std::fs::read_to_string(target.join("z-pwd")).unwrap().trim(),
        std::fs::canonicalize(target).unwrap().to_str().unwrap(),
    );
}

/// `cd` earlier in a list changes epsh's working directory, not the
/// process's; external commands later in the list must still run there.
#[test]
fn external_commands_run_in_the_directory_a_list_cd_selected() {
    let sh = PtyShell::spawn_with_opts(&[("sub/.keep", "")], &[]);
    let out = PtyShell::strip_ansi(&sh.run_command("cd sub && /bin/pwd"));
    let sub = std::fs::canonicalize(sh.home_path().join("sub")).unwrap();
    assert!(
        out.contains(&*sub.to_string_lossy()),
        "expected {} in: {out:?}",
        sub.display()
    );
}

fn clean(out: &str) -> String {
    PtyShell::strip_ansi(out)
}

fn terminal_state(sh: &PtyShell) -> ptytest::TerminalState {
    sh.terminal.borrow().terminal_state()
}

// ---------------------------------------------------------------------------
// Signals, job slot, dispatch refusals, dump privacy
// ---------------------------------------------------------------------------

#[test]
fn sigterm_and_sighup_restore_the_terminal_and_exit_with_the_signal_status() {
    for (name, status) in [("TERM", 143), ("HUP", 129)] {
        let sh = PtyShell::spawn();
        // The signal is queued while the child runs and read at the next
        // prompt, which is where the shell restores the terminal and exits.
        sh.type_str(&format!("/bin/sh -c 'kill -{name} $PPID'"));
        sh.enter();
        let mut terminal = sh.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_secs(5));
        let exit = terminal.wait_for_exit(deadline).expect("ish exits on SIG{name}");
        assert_eq!(exit, ExitStatus::Code(status), "SIG{name}");
        terminal
            .assert_terminal_restored(&sh.terminal_baseline)
            .unwrap_or_else(|e| panic!("terminal not restored after SIG{name}: {e:?}"));
    }
}

#[test]
fn fatal_crash_signals_restore_the_terminal_and_exit_with_the_signal_status() {
    // Expect 128+signo computed from the platform's own signal numbers:
    // SIGBUS is 10 on Darwin but 7 on Linux, so a hardcoded table would
    // assert the wrong status on one of them.
    for signal in [
        libc::SIGABRT,
        libc::SIGFPE,
        libc::SIGILL,
        libc::SIGTRAP,
        libc::SIGSEGV,
        libc::SIGBUS,
    ] {
        let status = 128 + signal;
        let sh = PtyShell::spawn();
        // Type a partial line so the shell sits in raw mode with bracketed
        // paste enabled when the signal lands.
        sh.type_str("echo about-to-crash");
        let mut terminal = sh.terminal.borrow_mut();
        terminal.signal(signal).unwrap();
        let deadline = terminal.deadline(std::time::Duration::from_secs(5));
        let exit = terminal
            .wait_for_exit(deadline)
            .expect("ish exits on fatal signal");
        assert_eq!(exit, ExitStatus::Code(status), "signal {signal}");
        terminal
            .assert_terminal_restored(&sh.terminal_baseline)
            .unwrap_or_else(|e| panic!("terminal not restored after signal {signal}: {e:?}"));
    }
}

#[test]
fn second_ctrl_z_is_refused_and_the_first_job_stays_resumable() {
    let sh = PtyShell::spawn_with_opts(&[("bin/sleep", READY_SLEEP_SCRIPT)], &[]);
    sh.type_str("./bin/sleep");
    sh.enter();
    sh.wait_for("pty-child-ready", 3000);
    sh.ctrl_z();
    sh.wait_for("stopped:", 3000);
    sh.wait_for_prompt_after_line_advance(3000);

    sh.type_str("sh -c 'kill -STOP $$; echo continued$((1+1))'");
    sh.enter();
    let out = sh.wait_for("continued2", 3000);
    assert!(out.contains("already suspended"), "{out:?}");
    sh.wait_for_prompt_after_line_advance(3000);

    sh.type_str("fg");
    sh.enter();
    let out = sh.wait_for("resuming: ./bin/sleep", 3000);
    assert!(out.contains("sleep"), "{out:?}");
    sh.wait_for_quiescence(100);
    sh.ctrl_c();
    sh.wait_for_prompt_after_line_advance(3000);
}

#[test]
fn state_builtin_in_a_list_and_undocumented_syntax_are_refused_without_running_anything() {
    let sh = PtyShell::spawn();
    for (line, message) in [
        ("z nothing; echo ran$((1+1))", "only command"),
        ("fg && echo ran$((1+1))", "only command"),
        ("sleep 1 & echo ran$((1+1))", "background jobs"),
        ("if true; then echo ran$((1+1)); fi", "if commands"),
        ("eval echo ran$((1+1))", "`eval` is not supported"),
        ("echo ${HOME:-ran$((1+1))}", "parameter operators"),
    ] {
        let out = clean(&sh.run_command(line));
        assert!(out.contains(message), "{line}: {out}");
        assert!(!out.contains("ran2"), "{line} ran: {out}");
    }
    let out = clean(&sh.run_command("echo ok${HOME}; echo ran$((1+1)) | cat"));
    assert!(out.contains("ran2"), "documented syntax must still run: {out}");
}

#[test]
fn ctrl_p_dump_is_private_and_omits_typed_text() {
    use std::os::unix::fs::PermissionsExt;
    let sh = PtyShell::spawn();
    sh.type_str("secret-token-123");
    sh.send(b"\x10");
    sh.ctrl_c();
    let output = PtyShell::strip_ansi(&sh.wait_for_prompt_after_line_advance(2000));
    assert!(output.contains("^C"), "expected Ctrl+C after Ctrl+P: {output:?}");
    assert!(output.contains("$ "), "expected prompt after Ctrl+C: {output:?}");
    let dir = sh.home_path().join(".cache/ish");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    let dumps: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("dump-"))
        .collect();
    assert_eq!(dumps.len(), 1, "{dumps:?}");
    assert_eq!(mode(&dumps[0]), 0o600);
    let text = std::fs::read_to_string(&dumps[0]).unwrap();
    assert!(text.contains("line.bytes: 16"), "{text}");
    assert!(!text.contains("secret-token-123"), "{text}");
}

#[test]
fn native_listing_and_standalone_cd_preserve_private_use_and_raw_path_bytes() {
    use std::ffi::OsString;
    let raw_directory_created = Cell::new(false);
    let sh = PtyShell::spawn_with_setup(&[], &[], &[], 24, 80, None, |home| {
        for (directory, marker) in [
            (OsString::from("\u{e000}\u{e080}"), "codec-path-marker"),
            (OsString::from("\u{e080}"), "pua-collision-marker"),
        ] {
            let directory = home.join(directory);
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join(marker), b"").unwrap();
        }
        let raw_directory = home.join(OsString::from_vec(vec![0x80]));
        let created = match std::fs::create_dir(&raw_directory) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => false,
            Err(error) => panic!("{error}"),
        };
        if created {
            std::fs::write(raw_directory.join("raw-byte-marker"), b"").unwrap();
        }
        raw_directory_created.set(created);
    });

    let out = clean(&sh.run_command("l \"$HOME/\u{e000}\u{e080}\""));
    assert!(out.contains("codec-path-marker"), "{out}");
    let out = clean(&sh.run_command("cd \"$HOME/\u{e000}\u{e080}\""));
    assert!(!out.contains("No such file"), "{out}");
    let out = clean(&sh.run_command("l"));
    assert!(out.contains("codec-path-marker"), "{out}");
    sh.run_command("cd \"$HOME\"");

    let out = clean(&sh.run_command("cd \"$HOME/\u{e080}\""));
    assert!(!out.contains("No such file"), "{out}");
    let out = clean(&sh.run_command("l"));
    assert!(out.contains("pua-collision-marker"), "{out}");
    sh.run_command("cd \"$HOME\"");

    if raw_directory_created.get() {
        let out = clean(&sh.run_command("l \"$HOME/$(printf '\\0200')\""));
        assert!(out.contains("raw-byte-marker"), "{out}");
        let out = clean(&sh.run_command("cd \"$HOME/$(printf '\\0200')\""));
        assert!(!out.contains("No such file"), "{out}");
        let out = clean(&sh.run_command("l"));
        assert!(out.contains("raw-byte-marker"), "{out}");
        sh.run_command("cd \"$HOME\"");
    } else {
        let out = clean(&sh.run_command("cd \"$HOME/$(printf '\\0200')\""));
        assert!(out.contains("ish: cd:"), "{out}");
        let out = clean(&sh.run_command("l"));
        assert!(!out.contains("pua-collision-marker"), "{out}");
    }

    sh.run_command("HOME_ORIGINAL=\"$HOME\"");
    sh.run_command("HOME=\"$HOME/$(printf '\\0200')\"");
    if raw_directory_created.get() {
        sh.run_command("cd");
        let out = clean(&sh.run_command("l"));
        assert!(out.contains("raw-byte-marker"), "{out}");
    } else {
        let out = clean(&sh.run_command("cd"));
        assert!(out.contains("ish: cd:"), "{out}");
        let out = clean(&sh.run_command("l"));
        assert!(!out.contains("pua-collision-marker"), "{out}");
    }
    let out = clean(&sh.run_command("z"));
    assert!(out.contains("ish: z: HOME is not UTF-8"), "{out}");
    sh.type_str("cd ~/");
    sh.tab();
    let out = clean(&sh.read_timeout(300));
    assert!(out.contains("ish: completion: HOME is not UTF-8"), "{out}");
    sh.ctrl_c();
    if !raw_directory_created.get() {
        sh.run_command("HOME=\"$HOME_ORIGINAL\"");
        sh.run_command("cd");
    }
    sh.send(b"\x10");
    sh.wait_for_quiescence(200);
    let dump_home = if raw_directory_created.get() {
        sh.home_path().join(OsString::from_vec(vec![0x80]))
    } else {
        sh.home_path().to_path_buf()
    };
    let dump_dir = dump_home.join(".cache/ish");
    assert!(std::fs::read_dir(dump_dir).unwrap().any(|entry| {
        entry.unwrap().file_name().to_string_lossy().starts_with("dump-")
    }));
}

#[test]
fn prompt_identity_displays_unicode_and_replaces_only_invalid_display_bytes() {
    let sh = PtyShell::spawn();
    let out = clean(&sh.run_command("USER='\u{e080}'"));
    assert!(out.contains("\u{e080}@"), "{out}");
    assert!(!out.contains('\u{e000}'), "{out}");
    let out = clean(&sh.run_command("USER=$(printf '\\0200')"));
    assert!(out.contains("\u{fffd}@"), "{out}");
}

#[test]
fn native_copy_uses_current_store_and_prefix_tmpdir_values() {
    let sh = PtyShell::spawn_with_opts(&[("source/file", "selected scratch"), ("scratch/.keep", "")], &[]);
    sh.run_command("set TMPDIR scratch");
    sh.run_command("TMPDIR=missing cp source refused-copy");
    assert_native_fs_status(&sh, 1);
    assert!(!sh.home_path().join("refused-copy").exists());
    sh.run_command("cp source accepted-copy");
    assert_native_fs_status(&sh, 0);
    assert_eq!(std::fs::read(sh.home_path().join("accepted-copy/file")).unwrap(), b"selected scratch");
    sh.run_command("/bin/sh -c 'printf %s \"$TMPDIR\"' >selected-tmpdir");
    assert_eq!(std::fs::read(sh.home_path().join("selected-tmpdir")).unwrap(), b"scratch");
}
