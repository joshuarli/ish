use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epsh::error::{ExitStatus, ShellError};
use epsh::eval::Shell;
use epsh::parser::Parser;

#[test]
fn terminal_status_explicit_exit_reaches_trap_and_survives_it() {
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().stdout_sink(output.clone()).build();
    let program = Parser::new("trap 'printf \"%s\" \"$?\"; false' EXIT; exit 7").parse().unwrap();

    assert_eq!(shell.run_program(&program).code(), 7);
    assert_eq!(*output.lock().unwrap(), b"7");
    assert_eq!(shell.exit_status().code(), 7);
    assert_eq!(shell.run_script("printf '%s' \"$?\""), 0);
    assert_eq!(*output.lock().unwrap(), b"77");
}

#[test]
fn terminal_status_normal_completion_survives_exit_trap() {
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().stdout_sink(output.clone()).build();

    assert_eq!(shell.run_script("trap 'printf \"%s\" \"$?\"; true' EXIT; false"), 1);
    assert_eq!(*output.lock().unwrap(), b"1");
    assert_eq!(shell.exit_status().code(), 1);
}

#[test]
fn terminal_status_runtime_failure_reaches_trap_and_survives_it() {
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let errors = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().stdout_sink(output.clone()).stderr_sink(errors)
        .external_handler(Box::new(|_, _| Err(ShellError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)))))
        .build();

    assert_eq!(shell.run_script("trap 'printf \"%s\" \"$?\"; true' EXIT; denied"), 2);
    assert_eq!(*output.lock().unwrap(), b"2");
    assert_eq!(shell.exit_status(), ExitStatus::MISUSE);
}

#[test]
fn terminal_status_cancellation_is_stored_for_the_next_call() {
    let cancelled = Arc::new(AtomicBool::new(true));
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().cancel_flag(cancelled.clone()).stdout_sink(output.clone()).build();

    assert_eq!(shell.run_script("true"), 130);
    assert_eq!(shell.exit_status().code(), 130);
    cancelled.store(false, Ordering::Relaxed);
    assert_eq!(shell.run_script("printf '%s' \"$?\""), 0);
    assert_eq!(*output.lock().unwrap(), b"130");
}

#[test]
fn terminal_status_timeout_is_stored_for_the_next_call() {
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().timeout(Duration::ZERO).stdout_sink(output.clone()).build();

    assert_eq!(shell.run_script("true"), 130);
    assert_eq!(shell.exit_status().code(), 130);
    shell.set_timeout(Duration::from_secs(60));
    assert_eq!(shell.run_script("printf '%s' \"$?\""), 0);
    assert_eq!(*output.lock().unwrap(), b"130");
}

#[test]
fn terminal_status_stopped_command_is_stored_for_the_next_call() {
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().stdout_sink(output.clone())
        .external_handler(Box::new(|_, _| Err(ShellError::Stopped { pid: 123, pgid: 123 })))
        .build();

    assert_eq!(shell.run_script("stopped"), 148);
    assert_eq!(shell.exit_status().code(), 148);
    assert_eq!(shell.run_script("printf '%s' \"$?\""), 0);
    assert_eq!(*output.lock().unwrap(), b"148");
}

#[test]
fn terminal_status_parse_failure_is_stored_for_the_next_call() {
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let errors = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().stdout_sink(output.clone()).stderr_sink(errors).build();

    assert_eq!(shell.run_script("if"), 2);
    assert_eq!(shell.exit_status(), ExitStatus::MISUSE);
    assert_eq!(shell.run_script("printf '%s' \"$?\""), 0);
    assert_eq!(*output.lock().unwrap(), b"2");
}
