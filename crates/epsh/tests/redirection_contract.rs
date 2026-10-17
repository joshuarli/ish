use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::Command;
use std::os::unix::process::CommandExt;

const CHILD_TARGET: &str = "EPSH_RESTORE_FAILURE_TARGET";
const CHILD_PRIMARY: &str = "EPSH_RESTORE_PRIMARY_TARGET";

#[test]
fn saved_copies_do_not_make_closed_target_descriptors_look_open() {
    let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
        .arg("-c")
        .arg("printf hidden >&10")
        .output()
        .unwrap();

    assert_ne!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
}

fn is_open(fd: i32) -> bool {
    // SAFETY: fcntl only reads the status flags of the specified descriptor.
    (unsafe { libc::fcntl(fd, libc::F_GETFD) }) != -1
}

fn open_descriptors() -> HashSet<i32> {
    (3..4096).filter(|fd| is_open(*fd)).collect()
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn child_failure(message: &str) -> ! {
    // SAFETY: message points to a byte slice valid for the duration of the write.
    unsafe {
        libc::write(2, message.as_ptr().cast(), message.len());
    }
    std::process::exit(1)
}

#[test]
fn reports_failure_to_restore_a_closed_saved_descriptor_in_child() {
    let Some(target) = std::env::var_os(CHILD_TARGET) else {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("redirected");
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("reports_failure_to_restore_a_closed_saved_descriptor_in_child")
            .env(CHILD_TARGET, &target)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(target.exists());
        return;
    };

    let target = PathBuf::from(target);
    let existing_fds = open_descriptors();
    let open_target = target.clone();
    let handler = Box::new(move |_request: &epsh::eval::RedirectOpen<'_>| {
        let saved_fd = (3..4096)
            .find(|fd| !existing_fds.contains(fd) && is_open(*fd))
            .expect("redirection saved a new descriptor");
        // SAFETY: saved_fd is the temporary backup descriptor selected above.
        if unsafe { libc::close(saved_fd) } != 0 {
            return Some(Err(std::io::Error::last_os_error()));
        }
        Some(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&open_target)
                .map(OwnedFd::from),
        )
    });
    let mut shell = epsh::eval::Shell::builder()
        .redirect_open_handler(handler)
        .build();
    let script = format!(": > {}", quote(target.to_str().unwrap()));
    let program = epsh::parser::Parser::new(&script).parse().unwrap();

    match shell.eval_command(&program.commands[0]) {
        Err(epsh::error::ShellError::Io(error)) => {
            let restore_failed_with_bad_fd = error.to_string().contains("restore")
                && error
                    .get_ref()
                    .and_then(|failure| std::error::Error::source(failure))
                    .and_then(|source| source.downcast_ref::<std::io::Error>())
                    .is_some_and(|source| source.raw_os_error() == Some(libc::EBADF));
            if restore_failed_with_bad_fd {
                std::process::exit(0)
            }
            child_failure(&format!("unexpected restore error: {error}\n"))
        }
        result => child_failure(&format!("unexpected redirection result: {result:?}\n")),
    }
}

#[test]
fn primary_redirection_error_keeps_restore_context_in_child() {
    let Some(target) = std::env::var_os(CHILD_PRIMARY) else {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("redirected");
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("primary_redirection_error_keeps_restore_context_in_child")
            .env(CHILD_PRIMARY, &target)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        return;
    };

    let target = PathBuf::from(target);
    let existing_fds = open_descriptors();
    let handler = Box::new(move |_request: &epsh::eval::RedirectOpen<'_>| {
        let saved_fd = (3..4096)
            .find(|fd| !existing_fds.contains(fd) && is_open(*fd))
            .expect("redirection saved a new descriptor");
        // SAFETY: saved_fd is the temporary backup descriptor selected above.
        if unsafe { libc::close(saved_fd) } != 0 {
            return Some(Err(std::io::Error::last_os_error()));
        }
        Some(Err(std::io::Error::other("primary open failure")))
    });
    let mut shell = epsh::eval::Shell::builder()
        .redirect_open_handler(handler)
        .build();
    let script = format!(": > {}", quote(target.to_str().unwrap()));
    let program = epsh::parser::Parser::new(&script).parse().unwrap();

    match shell.eval_command(&program.commands[0]) {
        Err(epsh::error::ShellError::Io(error)) => {
            let message = error.to_string();
            if message.matches("primary open failure").count() == 1
                && message.matches("restore").count() == 1
            {
                std::process::exit(0)
            }
            child_failure(&format!("unexpected combined I/O error: {message}\n"))
        }
        result => {
            child_failure(&format!("unexpected redirection result: {result:?}\n"))
        }
    }
}

fn descriptor_three_command(script: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_epsh"));
    command.arg("-c").arg(script);
    // SAFETY: the child has completed stdio setup; closing descriptor 3 makes the redirection allocate it.
    unsafe {
        command.pre_exec(|| {
            libc::close(3);
            Ok(())
        });
    }
    command
}

#[test]
fn external_output_inherits_a_newly_opened_target_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("output");
    let script = format!("/bin/sh -c 'printf inherited >&3' 3>{}", quote(target.to_str().unwrap()));
    let output = descriptor_three_command(&script).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read(target).unwrap(), b"inherited");
}

#[test]
fn external_input_inherits_a_newly_opened_target_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("input");
    std::fs::write(&target, b"inherited").unwrap();
    let script = format!("/bin/sh -c 'cat <&3' 3<{}", quote(target.to_str().unwrap()));
    let output = descriptor_three_command(&script).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"inherited");
}

#[test]
fn external_heredoc_inherits_a_newly_opened_target_descriptor() {
    let output = descriptor_three_command("/bin/sh -c 'cat <&3' 3<<EOF\ninherited\nEOF\n").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"inherited\n");
}

#[test]
fn nested_redirections_cannot_read_an_outer_saved_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("output");
    let script = format!("{{ printf hidden >&10; }} >{}", quote(target.to_str().unwrap()));
    let output = descriptor_three_command(&script).output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(std::fs::read(target).unwrap().is_empty());
}

#[test]
fn nested_closes_cannot_destroy_an_outer_saved_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("output");
    let script = format!("{{ exec 10>&-; printf redirected; }} >{}; printf restored", quote(target.to_str().unwrap()));
    let output = descriptor_three_command(&script).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"restored");
    assert_eq!(std::fs::read(target).unwrap(), b"redirected");
}

#[test]
fn cleanup_failure_preserves_control_flow_in_child() {
    const CHILD_CONTROL: &str = "EPSH_RESTORE_CONTROL";
    let Some(control) = std::env::var_os(CHILD_CONTROL) else {
        for control in ["exit", "return", "break", "continue", "cancel", "timeout", "stopped"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .arg("--exact").arg("cleanup_failure_preserves_control_flow_in_child")
                .env(CHILD_CONTROL, control).output().unwrap();
            assert!(output.status.success(), "{control}: {output:?}");
        }
        return;
    };
    let existing_fds = open_descriptors();
    let handler = Box::new(move |_request: &epsh::eval::RedirectOpen<'_>| {
        let saved_fd = (3..4096).find(|fd| !existing_fds.contains(fd) && is_open(*fd)).unwrap();
        // SAFETY: the descriptor is the backup selected above; this injects a restoration failure.
        unsafe { libc::close(saved_fd); }
        Some(std::fs::File::create("/dev/null").map(OwnedFd::from))
    });
    let stderr = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let expected = control.to_str().unwrap().to_owned();
    let produced = expected.clone();
    let mut shell = epsh::eval::Shell::builder()
        .redirect_open_handler(handler).stderr_sink(stderr.clone())
        .external_handler(Box::new(move |_, _| {
            use epsh::error::{ExitStatus, ShellError};
            Err(match produced.as_str() {
                "exit" => ShellError::Exit(ExitStatus::from(7)),
                "return" => ShellError::Return(ExitStatus::from(8)),
                "break" => ShellError::Break(2),
                "continue" => ShellError::Continue(2),
                "cancel" => ShellError::Cancelled,
                "timeout" => ShellError::TimedOut,
                "stopped" => ShellError::Stopped { pid: 123, pgid: 123 },
                _ => unreachable!(),
            })
        })).build();
    let program = epsh::parser::Parser::new("external >/dev/null").parse().unwrap();
    let result = shell.eval_command(&program.commands[0]);
    use epsh::error::ShellError;
    let preserved = match (expected.as_str(), result) {
        ("exit", Err(ShellError::Exit(status))) => status.code() == 7,
        ("return", Err(ShellError::Return(status))) => status.code() == 8,
        ("break", Err(ShellError::Break(2))) | ("continue", Err(ShellError::Continue(2)))
        | ("cancel", Err(ShellError::Cancelled)) | ("timeout", Err(ShellError::TimedOut))
        | ("stopped", Err(ShellError::Stopped { pid: 123, pgid: 123 })) => true,
        _ => false,
    };
    if !preserved { child_failure("cleanup replaced control flow\n"); }
    if !String::from_utf8_lossy(&stderr.lock().unwrap()).contains("restore") {
        child_failure("missing cleanup diagnostic\n");
    }
    std::process::exit(0)
}

#[test]
fn heredoc_pipe_exhaustion_returns_an_error_without_aborting() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_epsh"));
    command.arg("-c").arg(": 3<<EOF\ninput\nEOF\n");
    // SAFETY: after fork these syscalls fill child descriptors and limit only the child process.
    unsafe {
        command.pre_exec(|| {
            let source = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
            if source < 0 { return Err(std::io::Error::last_os_error()); }
            for fd in 3..10 {
                if libc::dup2(source, fd) < 0 { return Err(std::io::Error::last_os_error()); }
            }
            if source >= 10 { libc::close(source); }
            let limit = libc::rlimit { rlim_cur: 11, rlim_max: 11 };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            libc::setrlimit(libc::RLIMIT_CORE, &core);
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("24"), "{output:?}");
}
