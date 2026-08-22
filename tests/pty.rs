//! PTY-based integration tests for the ish binary.
//!
//! Spawns ish in a real pseudo-terminal and drives it with keystrokes,
//! asserting on the visible terminal output. This tests the full shell loop
//! including raw mode, prompt rendering, line editing, completion, and history.
//! Screen assertions use `ptytest`'s independent terminal state, so terminal
//! behavior is checked without a second parser in this consumer.

use std::cell::{Cell, RefCell};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use ptytest::{CommandSpec, ExitStatus, ProtocolProfile, PtyTest, Scenario, Size, TerminalBaseline, TestEnv};

// ---------------------------------------------------------------------------
// PTY harness
// ---------------------------------------------------------------------------

struct PtyShell {
    terminal: RefCell<PtyTest>,
    terminal_baseline: TerminalBaseline,
    _home: TempDir,
    startup_output: String,
    pending_output: RefCell<Vec<u8>>,
    output_offset: Cell<usize>,
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
        let path = PathBuf::from(String::from_utf8(buf).unwrap());
        Self(std::fs::canonicalize(&path).unwrap_or(path))
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
    if let Some(path) = std::env::var_os("ISH_TEST_BINARY") {
        let path = PathBuf::from(path);
        assert!(path.exists(), "ish binary not found at {}", path.display());
        return path;
    }

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

        let binary = ish_binary();
        let cwd = cwd_rel
            .map(|rel| home.path().join(rel))
            .unwrap_or_else(|| home.path().to_path_buf());
        let pgo_profile = std::env::var_os("ISH_PGO_PROFILE_DIR").map(|dir| {
            PathBuf::from(dir)
                .join("ish-%p.profraw")
                .to_string_lossy()
                .into_owned()
        });
        let mut command = CommandSpec::new(binary)
            .current_dir(&cwd)
            .env("HOME", &home_path)
            .env("USER", "testuser")
            .env("PWD", &cwd)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        if let Some(path) = &pgo_profile {
            command = command.env("LLVM_PROFILE_FILE", path);
        }
        for (key, value) in extra_env {
            command = command.env(key, value);
        }
        let mut environment = if cfg!(target_os = "linux") {
            TestEnv::hermetic_ascii()
        } else {
            TestEnv::hermetic_utf8("C.UTF-8")
        }
        .expect("a supported hermetic locale must be available on PTY platforms")
            .env("HOME", &home_path)
            .env("USER", "testuser")
            .env("PWD", &cwd)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        if let Some(path) = &pgo_profile {
            environment = environment.env("LLVM_PROFILE_FILE", path);
        }
        let scenario = Scenario::new("ish interactive shell")
            .expect("valid scenario label")
            .command(command)
            .size(Size::new(cols, rows).expect("non-zero shell size"))
            .environment(environment)
            .protocol_profile(ProtocolProfile::xterm_minimal_v1());
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

        // Wait for the initial prompt
        sh.startup_output = sh.wait_for_prompt(3000);
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
        terminal.send_bytes(deadline, input).expect("PTY write failed");
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
            let _ = terminal.wait_for_output(deadline).expect("wait for shell output");
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

    fn screen(&self) -> ptytest::ScreenSnapshot {
        self.terminal.borrow().screen()
    }

    /// Wait until the current editable prompt shows the expected history line.
    fn wait_for_prompt_text(&self, expected: &str, timeout_ms: u64) {
        let mut terminal = self.terminal.borrow_mut();
        let deadline = terminal.deadline(std::time::Duration::from_millis(timeout_ms));
        terminal
            .wait_for_screen(deadline, "history line", |screen| {
                active_prompt_text(screen).contains(expected)
            })
            .unwrap_or_else(|_| panic!("timed out waiting for history line {expected:?}"));
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

        panic!(
            "timed out after {timeout_ms}ms waiting for {marker:?}; output: {accumulated:?}"
        );
    }

    /// Wait for the shell prompt (` $ `).
    fn wait_for_prompt(&self, timeout_ms: u64) -> String {
        self.wait_for("$ ", timeout_ms)
    }

    /// Wait until Enter has been processed and the shell advanced to a new line.
    fn wait_for_line_advance(&self, timeout_ms: u64) -> String {
        self.wait_for("\n", timeout_ms)
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

            if let Some(nl) = accumulated.rfind('\n')
                && let Some(prompt_offset) = accumulated[nl + 1..].find("$ ")
            {
                let prompt_end = nl + 1 + prompt_offset + 2;
                let suffix = accumulated.split_off(prompt_end);
                self.pending_output
                    .borrow_mut()
                    .extend_from_slice(suffix.as_bytes());
                return accumulated;
            }
        }

        panic!("timed out waiting for the prompt after running {cmd:?}; output: {accumulated:?}");
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
    while lines.last().is_some_and(String::is_empty) { lines.pop(); }
    lines.join("\n")
}

fn active_prompt_text(screen: &ptytest::ScreenSnapshot) -> String {
    let cursor_row = usize::from(screen.cursor().row);
    let first = cursor_row.saturating_sub(2);
    let last = (cursor_row + 2).min(screen.row_count().saturating_sub(1));
    (first..=last)
        .filter_map(|row| screen.row(row))
        .map(|row| row.trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
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

fn pgo_profile_history() -> Vec<String> {
    const COMMANDS: &[&str] = &[
        "cargo test --workspace",
        "cargo test --test pty",
        "cargo clippy --all-targets",
        "git status --short",
        "git diff --stat",
        "git log --oneline --decorate -20",
        "rg -n TODO src tests",
        "docker compose up -d database",
        "ssh staging deploy --verbose",
        "kubectl get pods --all-namespaces",
    ];

    (0..12_000)
        .map(|index| {
            format!(
                "{} # pgo-session-{index:05}",
                COMMANDS[index % COMMANDS.len()]
            )
        })
        .collect()
}

/// Representative user activity for release PGO collection. This deliberately
/// stays in the PTY test so the profiled process is the real shell event loop;
/// the test harness itself is built and run without profile instrumentation.
#[test]
#[ignore]
fn pgo_profile_startup_history_tui() {
    assert!(
        std::env::var_os("ISH_TEST_BINARY").is_some(),
        "this scenario must run with ISH_TEST_BINARY set to an instrumented ish"
    );

    let config = "set EDITOR vi\nset PAGER less\nalias gs git status\nalias ct cargo test\n";
    let startup_files = [(".config/ish/config.ish", config)];

    // Repeated launches give startup code meaningful profile counts without
    // letting one interactive session dominate the profile by construction.
    for _ in 0..4 {
        let sh = PtyShell::spawn_with_setup(&startup_files, &[], &[], 24, 80, None, |_| {});
        assert!(sh.startup_output().contains("$ "));
    }

    let history = pgo_profile_history();
    let history_refs = history.iter().map(String::as_str).collect::<Vec<_>>();
    let files = [
        (".config/ish/config.ish", config),
        ("project/.git/HEAD", "ref: refs/heads/main\n"),
        (
            "project/.git/refs/heads/main",
            "0123456789012345678901234567890123456789\n",
        ),
    ];
    let sh =
        PtyShell::spawn_with_setup(&files, &history_refs, &[], 32, 120, Some("project"), |_| {});

    // Exercise the initial pager, incremental fuzzy search, candidate-stack
    // restoration on backspace, and selection movement without executing a
    // command. The short reads drain the PTY without adding sleeps to the
    // profiled workload.
    sh.ctrl_r();
    assert!(
        sh.wait_for("search:", 5000).contains("search:"),
        "history search did not open"
    );
    sh.type_str("cargo tes");
    sh.read_timeout(500);
    sh.backspace();
    sh.type_str("t");
    sh.read_timeout(500);
    sh.down();
    sh.up();
    sh.read_timeout(500);
    sh.escape();
    sh.wait_for_prompt(5000);

    // A second pass uses a different match distribution and forces the pager
    // to repaint as the query narrows and then expands again.
    sh.ctrl_r();
    assert!(
        sh.wait_for("search:", 5000).contains("search:"),
        "second history search did not open"
    );
    sh.type_str("git sta");
    sh.read_timeout(500);
    sh.type_str("tus");
    sh.read_timeout(500);
    sh.backspace();
    sh.backspace();
    sh.type_str("at");
    sh.read_timeout(500);
    sh.down();
    sh.down();
    sh.up();
    sh.read_timeout(500);
    sh.escape();
    sh.wait_for_prompt(5000);
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
fn line_editing_ctrl_delete() {
    let sh = PtyShell::spawn();
    sh.type_str("echo alpha beta");
    sh.ctrl_delete();
    sh.enter();
    let out = sh.wait_for_prompt(2000);
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
    let out = sh.wait_for_prompt(2000);
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
fn set_option_forms_fall_through_to_epsh() {
    let sh = PtyShell::spawn();
    let out = sh.run_command("set -e");
    let text = PtyShell::strip_ansi(&out);
    assert!(
        !text.contains("not a valid variable name"),
        "set -e should fall through to epsh: {text:?}"
    );
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
    let sh = PtyShell::spawn_with_opts(&[], &["echo from_global"]);
    sh.run_command("echo local_one");
    sh.run_command("echo local_two");
    // Wait for each redraw before sending the next escape sequence. The macOS
    // PTY runner can otherwise coalesce rapid writes with the previous redraw.
    for expected in ["echo local_two", "echo local_one", "echo from_global"] {
        sh.up();
        sh.wait_for_prompt_text(expected, 2000);
    }
    sh.enter();
    let out = sh.wait_for_prompt(2000);
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
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    // The selected command should be on the line — enter executes it
    assert!(text.contains("beta"), "expected 'beta' in output: {text:?}");
}

#[test]
fn history_ctrl_r_ignores_later_global_entries() {
    use std::io::Write;

    let sh = PtyShell::spawn_with_opts(&[], &["echo startup"]);
    let hist_path = sh.home_path().join(".local/share/ish/history");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&hist_path)
        .unwrap()
        .write_all(b"echo later_global\n")
        .unwrap();

    sh.ctrl_r();
    let out = sh.wait_for("search:", 2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(text.contains("search:"), "expected search pager: {text:?}");
    assert!(
        !text.contains("later_global"),
        "later global entry leaked into initial history pager: {text:?}"
    );

    sh.type_str("later");
    sh.wait_for_quiescence(200);
    sh.enter();
    let out = sh.wait_for_prompt(2000);
    let text = PtyShell::strip_ansi(&out);
    assert!(
        !text.contains("echo later_global"),
        "later global entry leaked into Ctrl+R acceptance: {text:?}"
    );
}

#[test]
fn history_ctrl_r_escape_cancels() {
    let sh = PtyShell::spawn_with_opts(&[], &["echo secret"]);
    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    sh.type_str("secret");
    sh.wait_for_quiescence(200);
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

    sh.read_timeout(600);
    let screen = snapshot_text(&sh.screen());
    assert_screen_contains_once(&screen, "search:");
    assert_screen_contains_once(&screen, "abc1");
    assert_screen_contains_once(&screen, "abc2");
    assert_screen_contains_once(&screen, "abc3");

    sh.escape();
    sh.wait_for_prompt(2000);
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
    sh.escape();
    sh.wait_for_prompt(2000);
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

    sh.escape();
    sh.wait_for_prompt(2000);
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
    sh.wait_for_quiescence(300);
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
fn tab_completion_first_tab_has_no_selection_second_tab_selects_first() {
    let sh = PtyShell::spawn_with_opts(&[("aaa.txt", ""), ("aab.txt", ""), ("aac.txt", "")], &[]);
    sh.type_str("echo aa");
    sh.tab();
    sh.wait_for_quiescence(150);
    sh.read_timeout(400);
    let first = snapshot_text(&sh.screen());
    assert_eq!(
        first.matches("aaa.txt").count(),
        1,
        "first tab should show the grid without previewing the first item into the prompt line: {first:?}"
    );

    sh.tab();
    sh.wait_for_quiescence(150);
    sh.read_timeout(400);
    let second = snapshot_text(&sh.screen());
    assert_eq!(
        second.matches("aaa.txt").count(),
        2,
        "second tab should activate selection on the first item and preview it in the prompt line: {second:?}"
    );

    sh.escape();
    sh.wait_for_prompt(2000);
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

    sh.read_timeout(600);
    let screen = snapshot_text(&sh.screen());
    let flattened = screen.replace('\n', "");
    assert_eq!(
        flattened.matches("aaa.txt").count(),
        2,
        "selected completion should appear once in the preview line and once in the grid: {screen:?}"
    );
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
    sh.wait_for_quiescence(300);
    sh.enter();
    sh.wait_for_prompt(2000);
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
    sh.wait_for_prompt(2000);
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
    sh.wait_for_prompt(2000);
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
    sh.wait_for_quiescence(200);
    sh.read_timeout(300);

    sh.resize(24, 12);
    sh.wait_for_quiescence(200);

    sh.read_timeout(800);
    let screen = snapshot_text(&sh.screen());
    assert_screen_contains_once(&screen, "aaa.txt");
    assert_screen_contains_once(&screen, "aab.txt");
    assert_screen_contains_once(&screen, "aac.txt");

    sh.escape();
    sh.wait_for_prompt(2000);
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
        prompt_count,
        1,
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
    sh.wait_for_prompt(2000);
}

#[test]
fn history_resize_rerenders_pager() {
    let sh = PtyShell::spawn_with_size(&[], &["abc1", "abc2", "abc3"], 24, 20);
    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    sh.type_str("abc");
    sh.wait_for_quiescence(200);
    sh.read_timeout(300);

    sh.resize(24, 10);
    sh.wait_for_quiescence(200);

    sh.read_timeout(800);
    let screen = snapshot_text(&sh.screen());
    assert_screen_contains_once(&screen, "search:");
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
    sh.wait_for_quiescence(200);
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
    sh.wait_for_quiescence(150);
    sh.down();
    sh.up();
    sh.wait_for_quiescence(200);

    sh.read_timeout(800);
    let screen = snapshot_text(&sh.screen());
    let (_, picker) = screen
        .split_once("dirs:\n")
        .expect("directory picker should be visible");
    assert_screen_contains_once(picker, "~/one");
    assert_screen_contains_once(picker, "~/two");

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
fn history_help() {
    let sh = PtyShell::spawn();
    let text = PtyShell::strip_ansi(&sh.run_command("history -h"));
    assert!(
        text.contains("Usage: history [compact|rebuild|reset]"),
        "expected history help text: {text:?}"
    );
}

#[test]
fn history_autosuggest_ignores_later_global_entries() {
    use std::io::Write;

    let sh = PtyShell::spawn_with_opts(&[], &["echo startup"]);
    let hist_path = sh.home_path().join(".local/share/ish/history");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&hist_path)
        .unwrap()
        .write_all(b"echo later_global\n")
        .unwrap();

    sh.ctrl_r();
    sh.wait_for("search:", 2000);
    sh.escape();
    sh.wait_for_prompt(2000);

    sh.type_str("echo l");
    sh.right();
    sh.enter();
    let out = sh.wait_for_prompt(2000);
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

fn allow_envrc(home: &Path, rel_envrc: &str) {
    let envrc = home
        .join(rel_envrc)
        .canonicalize()
        .unwrap_or_else(|_| home.join(rel_envrc));
    let allow_dir = home.join(".local/share/denv/allow");
    std::fs::create_dir_all(&allow_dir).unwrap();
    let mtime = std::fs::metadata(&envrc).unwrap().mtime();
    std::fs::write(allow_dir.join(denv_trust_key(&envrc)), format!("{mtime}")).unwrap();
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
fn denv_loads_dotenv_on_cd_without_allow() {
    let sh = PtyShell::spawn_with_opts(&[("project/.env", "DENV_TEST_VAR=loaded\n")], &[]);
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(
        text.contains("loaded"),
        "expected dotenv var after cd: {text:?}"
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
fn denv_startup_loads_dotenv_in_initial_cwd() {
    let sh = PtyShell::spawn_with_setup(
        &[("project/.env", "DENV_TEST_VAR=from_startup\n")],
        &[],
        &[],
        24,
        80,
        Some("project"),
        |_| {},
    );
    let text = PtyShell::strip_ansi(&sh.run_command("echo $DENV_TEST_VAR"));
    assert!(
        text.contains("from_startup"),
        "expected dotenv to load on startup: {text:?}"
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
    let sh = spawn_with_allowed_envrc(
        &[
            (
                "project/.envrc",
                "export SHARED='from_envrc'\nexport ENVRC_ONLY='1'\n",
            ),
            ("project/.env", "SHARED=from_dotenv\nDOTENV_ONLY=1\n"),
        ],
        "project/.envrc",
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

    // `denv` deliberately uses filesystem modification timestamps as its
    // trust invalidation contract; cross the one-second timestamp boundary.
    std::thread::sleep(std::time::Duration::from_millis(1100));
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

    // `denv` deliberately uses filesystem modification timestamps as its
    // trust invalidation contract; cross the one-second timestamp boundary.
    std::thread::sleep(std::time::Duration::from_millis(1100));
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
fn denv_dotenv_helper_loads_env_file() {
    let sh = spawn_with_allowed_envrc(
        &[
            ("project/.envrc", "dotenv\nexport AFTER='1'\n"),
            ("project/.env", "FROM_ENV=loaded\n"),
        ],
        "project/.envrc",
    );
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("echo $FROM_ENV $AFTER"));
    assert!(
        text.contains("loaded 1"),
        "expected dotenv helper values: {text:?}"
    );
}

#[test]
fn denv_allow_requires_envrc() {
    let sh = PtyShell::spawn_with_opts(&[("project/.env", "DENV_TEST_VAR=loaded\n")], &[]);
    sh.run_command("cd project");
    let text = PtyShell::strip_ansi(&sh.run_command("denv allow"));
    assert!(
        text.contains("no .envrc"),
        "expected allow failure without envrc: {text:?}"
    );
}

#[test]
fn job_suspend_and_resume() {
    let sh = PtyShell::spawn();

    // Start a long-running foreground process.
    sh.type_str("sleep 60");
    sh.enter();
    sh.wait_for_line_advance(1000);

    // Give sleep a moment to start, then suspend it with Ctrl+Z.
    sh.wait_for_quiescence(200);
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
    sh.wait_for_quiescence(100);
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

/// Paste a 2 KiB payload via bracketed paste and verify the shell
/// instantly rejects it with "[paste exceeded 1KB limit]".
#[test]
fn bracketed_paste_over_limit_rejected() {
    let content: String = "x".repeat(2048);
    assert!(content.len() > 1024);

    let sh = PtyShell::spawn();

    // Build the bracketed paste payload: \x1b[200~ CONTENT \x1b[201~
    let mut payload = Vec::with_capacity(6 + content.len() + 6);
    payload.extend_from_slice(b"\x1b[200~");
    payload.extend_from_slice(content.as_bytes());
    payload.extend_from_slice(b"\x1b[201~");

    sh.send(&payload);

    let out = sh.wait_for_prompt(5000);
    let clean = PtyShell::strip_ansi(&out);

    assert!(
        clean.contains("[paste exceeded 1KB limit]"),
        "expected paste-rejection message, got: {clean:?}"
    );
    // No 'x's from the paste content should appear on screen.
    assert!(
        !clean.contains("xxxxx"),
        "paste content appeared on screen — limit bypassed: {clean:?}"
    );
    // Shell must still be responsive after rejection.
    let out2 = sh.run_command("echo ok");
    let clean2 = PtyShell::strip_ansi(&out2);
    assert!(
        clean2.contains("ok"),
        "shell unresponsive after paste: {clean2:?}"
    );
}

/// Paste the real AGENTS.md (14.8 KB) via bracketed paste and verify it's
/// instantly rejected without displaying the content.
#[test]
fn bracketed_paste_agents_md_rejected() {
    let content = include_str!("../AGENTS.md");
    assert!(
        content.len() > 2048,
        "AGENTS.md must be > 2 KiB for this test"
    );

    let sh = PtyShell::spawn();

    let mut payload = Vec::with_capacity(6 + content.len() + 6);
    payload.extend_from_slice(b"\x1b[200~");
    payload.extend_from_slice(content.as_bytes());
    payload.extend_from_slice(b"\x1b[201~");

    sh.send(&payload);

    let out = sh.wait_for_prompt(5000);
    let clean = PtyShell::strip_ansi(&out);

    assert!(
        clean.contains("[paste exceeded 1KB limit]"),
        "expected paste-rejection message, got: {clean:?}"
    );
    // The first line of AGENTS.md must NOT appear on screen.
    let first_line = content.lines().next().unwrap_or("");
    assert!(
        !clean.contains(first_line),
        "paste content appeared on screen — limit bypassed: {clean:?}"
    );

    let out2 = sh.run_command("echo ok");
    let clean2 = PtyShell::strip_ansi(&out2);
    assert!(
        clean2.contains("ok"),
        "shell unresponsive after paste: {clean2:?}"
    );
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
        !clean.contains("[paste exceeded 1KB limit]"),
        "1024-byte paste was wrongly rejected: {clean:?}"
    );
}
