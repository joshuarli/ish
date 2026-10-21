use ptytest::{CommandSpec, ProtocolProfile, PtyTest, Scenario, Size, TestEnv};
use std::path::PathBuf;
use std::time::Duration;

mod common;

struct SearchShell {
    terminal: PtyTest,
    home: PathBuf,
}

impl SearchShell {
    fn spawn(history: &[&str]) -> Self {
        let home = common::scratch_dir("ish_history_paste");
        let data = home.join(".local/share/ish");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("history"), history.join("\n") + "\n").unwrap();
        let environment = if cfg!(target_os = "linux") {
            TestEnv::hermetic_ascii()
        } else {
            TestEnv::hermetic_utf8("C.UTF-8")
        }
        .unwrap()
        .env("HOME", &home)
        .env("USER", "testuser")
        .env("PWD", &home)
        .env("PATH", "/usr/bin:/bin");
        let command = CommandSpec::new(common::ish_binary())
            .current_dir(&home)
            .env("HOME", &home)
            .env("USER", "testuser")
            .env("PWD", &home)
            .env("PATH", "/usr/bin:/bin");
        let scenario = Scenario::new("history search paste")
            .unwrap()
            .command(command)
            .environment(environment)
            .size(Size::new(80, 24).unwrap())
            .protocol_profile(ProtocolProfile::xterm_minimal_v1());
        let mut shell = Self {
            terminal: PtyTest::spawn(scenario).unwrap(),
            home,
        };
        shell.wait("initial prompt", |screen| screen.contains("$ "));
        shell
    }

    fn send(&mut self, bytes: &[u8]) {
        let deadline = self.terminal.deadline(Duration::from_secs(3));
        self.terminal.send_bytes(deadline, bytes).unwrap();
    }

    fn wait(&mut self, description: &str, predicate: impl Fn(&ptytest::ScreenSnapshot) -> bool) {
        let deadline = self.terminal.deadline(Duration::from_secs(3));
        self.terminal.wait_for_screen(deadline, description, predicate).unwrap();
    }
}

impl Drop for SearchShell {
    fn drop(&mut self) {
        let deadline = self.terminal.deadline(Duration::from_secs(2));
        let _ = self.terminal.finish(deadline);
        std::fs::remove_dir_all(&self.home).unwrap();
    }
}

#[test]
fn pasted_history_query_filters_and_executes_selected_command() {
    let mut shell = SearchShell::spawn(&["echo alpha", "echo beta"]);
    shell.send(b"\x12");
    shell.wait("history search", |screen| screen.contains("search:"));
    shell.send(b"\x1b[200~alpha\x1b[201~");
    shell.wait("pasted query filters history", |screen| {
        screen.contains("search: alpha") && !screen.contains("echo beta")
    });
    shell.send(b"\r");
    shell.wait("accept alpha command", |screen| {
        !screen.contains("search:") && screen.contains("$ echo alpha")
    });
    shell.send(b"\r");
    shell.wait("execute alpha command", |screen| {
        (0..screen.row_count()).any(|row| {
            screen.row(row).is_some_and(|line| line.trim() == "alpha")
        })
    });
}

#[test]
fn pasted_history_query_edits_at_cursor_and_restores_candidates_after_delete() {
    let mut shell = SearchShell::spawn(&["echo alpha", "echo beta"]);
    shell.send(b"\x12alha\x01\x1b[C\x1b[C");
    shell.wait("typed query", |screen| screen.contains("search: alha"));
    shell.send(b"\x1b[200~p\x1b[201~");
    shell.wait("paste at query cursor", |screen| {
        screen.contains("search: alpha") && !screen.contains("echo beta")
    });
    shell.send(b"\x05\x15beta");
    shell.wait("deleting pasted query restores candidates", |screen| {
        screen.contains("search: beta") && screen.contains("echo beta")
            && !screen.contains("echo alpha")
    });
    shell.send(b"\r");
    shell.wait("accept beta command", |screen| {
        !screen.contains("search:") && screen.contains("$ echo beta")
    });
}

#[test]
fn pasted_history_query_resets_selection_after_narrowing() {
    let mut shell = SearchShell::spawn(&["echo alpha1", "echo alpha2", "echo beta"]);
    shell.send(b"\x12alpha");
    shell.wait("alpha candidates", |screen| {
        screen.contains("search: alpha") && screen.contains("echo alpha1")
            && screen.contains("echo alpha2")
    });
    shell.send(b"\x1b[B\x1b[200~1\x1b[201~");
    shell.wait("narrow to one command", |screen| {
        screen.contains("search: alpha1") && !screen.contains("echo alpha2")
    });
    shell.send(b"\r");
    shell.wait("accept narrowed command", |screen| {
        !screen.contains("search:") && screen.contains("$ echo alpha1")
    });
}
