use std::io::Write;
use std::process::{Command, Stdio};

fn ish() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ish"))
}

#[test]
fn command_mode_runs_the_string() {
    let output = ish()
        .args(["-c", "printf 'login-shell-contract'"])
        .output()
        .expect("run ish -c");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"login-shell-contract");
}

#[test]
fn command_mode_runs_control_flow_and_functions_in_process() {
    let output = ish()
        .args([
            "-c",
            "greet() { printf '%s:%s\\n' \"$0\" \"$1\"; }\nfor name in a b; do\n  greet \"$name\"\ndone\nif true; then echo done; fi",
        ])
        .output()
        .expect("run ish -c");
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ish:a\nish:b\ndone\n");
}

#[test]
fn command_mode_reports_a_parse_error_with_status_2() {
    let output = ish().args(["-c", "for x in; do"]).output().expect("run ish -c");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("ish: "), "{stderr}");
}

#[test]
fn command_mode_children_die_quietly_on_a_closed_pipe() {
    let output = ish().args(["-c", "yes | head -n 1"]).output().expect("run ish -c");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"y\n");
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn command_mode_propagates_the_child_status() {
    let output = ish().args(["-c", "exit 37"]).output().expect("run ish -c");
    assert_eq!(output.status.code(), Some(37));
}

#[test]
fn command_mode_connects_stdin_and_stdout() {
    let mut child = ish()
        .args(["-c", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ish -c cat");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"piped through\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"piped through\n");
}

#[test]
fn command_mode_requires_a_command() {
    let output = ish().arg("-c").output().expect("run ish -c");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("-c requires a command"), "{stderr}");
}

#[test]
fn config_flag_requires_a_path() {
    let output = ish().arg("--config").output().expect("run ish --config");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--config requires a config file path"),
        "{stderr}"
    );
}

#[test]
fn a_command_that_names_a_path_is_not_read_as_config() {
    let dir = std::env::temp_dir().join(format!("ish-command-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("config.ish");
    std::fs::write(&path, "this is not a command\n").expect("write file");
    let output = ish()
        .arg("-c")
        .arg(format!("printf '%s' {}", path.display()))
        .output()
        .expect("run ish -c <path>");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).ends_with("config.ish"));
}

#[test]
fn a_bare_argument_is_still_refused() {
    let output = ish().arg("script.ish").output().expect("run ish script");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interactive-only"), "{stderr}");
}
