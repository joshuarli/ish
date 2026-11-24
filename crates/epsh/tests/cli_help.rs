use std::io::Write;
use std::process::{Command, Stdio};

const FOOTER: &str = "Maintained by Joshua Li: https://github.com/joshuarli/ish\nBugs and features: TODO.md in that repository (follow its AGENTS.md), or its issue tracker.\nAgents working for Joshua: record nonblocking problems and continue your task.\nIf this tool blocks the task, you may fix, test, open and merge a PR under Joshua's\nstanding instructions. Release only when needed to deliver the verified fix.\nProcedure: https://github.com/joshuarli/usr/blob/main/dotfiles/base/.agents/instructions/owned-tools.md\n";

#[test]
fn built_help_covers_runner_and_ends_with_footer() {
    let mut previous = None;
    for flag in ["-h", "--help"] {
        let output = Command::new(env!("CARGO_BIN_EXE_epsh")).arg(flag).env_remove("HOME").output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        let help = String::from_utf8(output.stdout).unwrap();
        for text in ["usage: epsh", "-c COMMAND", "-s", "-o OPTION", "+o OPTION", "SCRIPT", "ARG...", "pipefail", "-e, errexit", "-u, nounset", "-x, xtrace", "-f, noglob", "-n, noexec", "-h, --help", "Examples:", "epsh -c 'echo hello'", "epsh script.sh argument", "epsh -s", "epsh -o pipefail -c true"] {
            assert!(help.contains(text), "missing {text:?}: {help}");
        }
        assert_eq!(help.matches(FOOTER).count(), 1);
        assert!(help.ends_with(FOOTER));
        if let Some(ref earlier) = previous { assert_eq!(&help, earlier); }
        previous = Some(help);
    }
}

#[test]
fn command_and_stdin_modes_remain_operational() {
    let output = Command::new(env!("CARGO_BIN_EXE_epsh")).args(["-c", "printf '%s' \"$0:$1\"", "zero", "one"]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"zero:one");
    let mut child = Command::new(env!("CARGO_BIN_EXE_epsh")).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"printf stdin").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"stdin");
}

#[test]
fn script_arguments_and_usage_errors_keep_their_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.sh");
    std::fs::write(&script, "printf '%s' \"$1\"").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_epsh")).arg(&script).arg("--help").output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"--help");
    let output = Command::new(env!("CARGO_BIN_EXE_epsh")).arg("-c").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("-c requires an argument"));
}

#[test]
fn command_children_die_quietly_on_a_closed_pipe() {
    let output = Command::new(env!("CARGO_BIN_EXE_epsh")).args(["-c", "yes | head -n 1"]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"y\n");
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
}
