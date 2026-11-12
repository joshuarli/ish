use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
mod common;

fn worker(args: &[&str], input: &[u8]) -> std::process::Output {
    let mut command = Command::new(common::ish_binary());
    command.args(["--ish-fz-worker", "-1"]).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // SAFETY: setsid is the only child pre-exec operation and takes no pointers or locks.
    unsafe { command.pre_exec(|| { if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) } }); }
    let mut child = command.spawn().unwrap();
    match child.stdin.take().unwrap().write_all(input) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
        result => result.unwrap(),
    }
    child.wait_with_output().unwrap()
}

#[test]
fn fresh_worker_filter_preserves_framing_raw_bytes_and_ties_without_tty() {
    for (args, input, expected) in [
        (vec!["-e", ""], b"\xff\nalpha\nalpha\n\n".as_slice(), b"\xff\nalpha\nalpha\n".as_slice()),
        (vec!["-0e", ""], b"alpha\nline\0beta\0", b"alpha\nline\nbeta\n"),
        (vec!["--show-matches=A"], b"alpha\nAlpha\nALPHA\n", b"Alpha\nALPHA\n"),
        (vec!["-e", "x"], b"alpha\nbeta\n", b""),
    ] {
        let result = worker(&args, input);
        assert_eq!(result.status.code(), Some(0), "{:?}", result.stderr);
        assert_eq!(result.stdout, expected);
    }
}

#[test]
fn fresh_worker_absent_terminal_and_cli_errors_produce_no_selected_bytes() {
    for (args, status) in [(vec![], 1), (vec!["--multi"], 2), (vec!["--lines=2"], 1)] {
        let result = worker(&args, b"alpha\n");
        assert_eq!(result.status.code(), Some(status), "{:?}", result.stderr);
        assert!(result.stdout.is_empty());
        assert!(result.status.signal().is_none());
    }
}

#[test]
fn fresh_worker_large_redirected_input_preserves_all_records() {
    let directory = common::scratch_dir("fz-input");
    let path = directory.join("candidates");
    let input = (0..40000).map(|index| format!("alpha-{index:05}\n")).collect::<String>().into_bytes();
    std::fs::write(&path, &input).unwrap();
    let result = Command::new(common::ish_binary()).args(["--ish-fz-worker", "-1", "-e", ""])
        .stdin(std::fs::File::open(path).unwrap()).output().unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, input);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn fresh_worker_restores_paste_mode_exactly_once() {
    use ptytest::{CommandSpec, ExitStatus, PtyTest, Scenario, Size, TestEnv};
    use std::time::Duration;
    let scenario = Scenario::new("fresh fz worker restores once").unwrap()
        .command(CommandSpec::new(common::ish_binary()).arg("--ish-fz-worker").arg("-1"))
        .size(Size::new(80, 24).unwrap()).environment(TestEnv::hermetic_ascii().unwrap());
    let mut terminal = PtyTest::spawn(scenario).unwrap();
    let baseline = terminal.terminal_baseline();
    let deadline = terminal.deadline(Duration::from_secs(5));
    terminal.send_bytes(deadline, b"alpha\n\x04").unwrap();
    terminal.wait_for_screen(deadline, "picker prompt", |screen| {
        (0..screen.row_count()).any(|row| screen.row(row).is_some_and(|line| line.trim() == ">"))
    }).unwrap();
    terminal.send_bytes(deadline, b"\r").unwrap();
    assert_eq!(terminal.wait_for_exit(deadline).unwrap(), ExitStatus::Code(0));
    let disable = b"\x1b[?2004l";
    assert_eq!(terminal.raw_output().windows(disable.len()).filter(|bytes| *bytes == disable).count(), 1);
    terminal.assert_terminal_restored(&baseline).unwrap();
}
