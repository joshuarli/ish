use std::process::Command;

#[test]
fn malformed_command_substitution_fails_before_later_commands_run() {
    let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
        .args(["-c", "echo \"$(if)\"; printf after"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("1:11: syntax error"), "{stderr}");
    assert!(stderr.contains("unexpected token: Eof"), "{stderr}");
}

#[test]
fn valid_nested_command_substitution_still_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
        .args(["-c", "printf '<%s>' \"$(printf nested)\""])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"<nested>");
}
