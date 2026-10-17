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
