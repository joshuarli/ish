use epsh::eval::Shell;
use epsh::shell_bytes::ShellBytes;
use std::ffi::OsString;
use std::fs::{self, File, FileTimes};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

static TEST_LOCK: Mutex<()> = Mutex::new(());

// Linux provides openpty in libutil; macOS provides it in libc.
#[cfg(target_os = "linux")]
#[link(name = "util")]
unsafe extern "C" {}

#[test]
fn test_file_predicates_use_shell_cwd() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file");
    fs::write(&file, b"contents").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o7777)).unwrap();
    fs::create_dir(directory.path().join("directory")).unwrap();
    symlink("file", directory.path().join("link")).unwrap();
    let fifo = directory.path().join("fifo");
    rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, rustix::fs::Mode::from_raw_mode(0o600)).unwrap();
    let socket = directory.path().join("socket");
    let _listener = UnixListener::bind(&socket).unwrap();
    symlink("/dev/null", directory.path().join("character-device")).unwrap();

    let older = directory.path().join("older");
    let newer = directory.path().join("newer");
    fs::write(&older, b"older").unwrap();
    fs::write(&newer, b"newer").unwrap();
    File::open(&older)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(10)))
        .unwrap();
    File::open(&newer)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(20)))
        .unwrap();

    let mut shell = Shell::builder().cwd(directory.path().to_path_buf()).build();

    for script in [
        "test -e file",
        "test -f file",
        "test -r file",
        "test -w file",
        "test -x file",
        "test -s file",
        "test -u file",
        "test -g file",
        "test -k file",
        "test -d directory",
        "test -L link",
        "test -h link",
        "test -p fifo",
        "test -c character-device",
        "test -S socket",
        "test newer -nt older",
        "test older -ot newer",
        "test file -ef file",
    ] {
        assert_eq!(shell.run_script(script), 0, "{script}");
    }

    assert_eq!(shell.run_script("test -f Cargo.toml"), 1);
}

#[test]
fn test_file_predicates_preserve_raw_path_bytes() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = tempfile::tempdir().unwrap();
    let raw_name = OsString::from_vec(b"raw-\x80-name".to_vec());
    let raw_path = directory.path().join(&raw_name);
    fs::write(&raw_path, b"raw contents").unwrap();
    fs::set_permissions(&raw_path, fs::Permissions::from_mode(0o777)).unwrap();
    let mut shell = Shell::builder().cwd(directory.path().to_path_buf()).build();
    shell
        .set_var_bytes("RAW", ShellBytes::from_os_str(raw_name.as_os_str()))
        .unwrap();

    for script in [
        "test -e \"$RAW\"",
        "test -f \"$RAW\"",
        "test -r \"$RAW\"",
        "test -w \"$RAW\"",
        "test -x \"$RAW\"",
        "test -s \"$RAW\"",
        "test \"$RAW\" -ef \"$RAW\"",
    ] {
        assert_eq!(shell.run_script(script), 0, "{script}");
    }
}

#[test]
fn test_treats_negative_and_closed_descriptors_as_false() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut shell = Shell::new();
    assert_eq!(shell.run_script("test -t -1"), 1);
    assert_eq!(shell.run_script("test -t 2147483647"), 1);
    assert_eq!(shell.run_script("test -t invalid"), 1);
    assert_eq!(shell.run_script("test -t 999999999999999999999"), 1);
}

#[test]
fn test_treats_invalid_descriptors_as_false_when_stdout_is_a_tty() {
    for operand in ["invalid", "2147483648"] {
        let output = run_epsh_with_tty_stdout(&format!("test -t {operand}"));
        assert_eq!(
            output.status.code(),
            Some(1),
            "{operand}: {:?}",
            output.stderr
        );
    }
}

fn run_epsh_with_tty_stdout(script: &str) -> Output {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty writes valid descriptors into the two initialized output slots.
    let result = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: openpty returned ownership of both descriptors to this function.
    let _master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    // SAFETY: openpty returned ownership of both descriptors to this function.
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
    Command::new(env!("CARGO_BIN_EXE_epsh"))
        .args(["-c", script])
        .stdout(Stdio::from(slave))
        .output()
        .unwrap()
}

#[test]
fn wait_returns_the_status_for_each_owned_pid() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut shell = Shell::new();
    let stdout = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
    shell.set_stdout_sink(stdout.clone());

    assert_eq!(shell.run_script("exit 3 &"), 0);
    assert_eq!(shell.run_script("echo $!"), 0);
    let first_pid = String::from_utf8(std::mem::take(&mut *stdout.lock().unwrap()))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_eq!(shell.run_script("exit 7 &"), 0);
    assert_eq!(shell.run_script("echo $!"), 0);
    let second_pid = String::from_utf8(std::mem::take(&mut *stdout.lock().unwrap()))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();

    assert_eq!(shell.run_script(&format!("wait {second_pid}")), 7);
    assert_eq!(shell.run_script(&format!("wait {first_pid}")), 3);
}

#[test]
fn wait_does_not_reap_an_embedder_child() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .spawn()
        .unwrap();
    let mut shell = Shell::new();
    assert_eq!(shell.run_script("wait"), 0);
    assert_eq!(child.wait().unwrap().code(), Some(7));
}

#[test]
fn exec_uses_the_requested_program_as_argv_zero() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
        .args(["-c", "exec /bin/echo ok"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"ok\n");
}

#[cfg(target_os = "linux")]
#[test]
fn successful_exec_does_not_leak_the_saved_cwd_descriptor() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
        .args(["-c", "exec /bin/sh -c 'test ! -e /proc/self/fd/3'"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
}

#[test]
fn failed_exec_restores_the_embedder_current_directory() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let original_cwd = std::env::current_dir().unwrap();
    let shell_cwd = tempfile::tempdir().unwrap();
    let mut shell = Shell::builder()
        .cwd(shell_cwd.path().to_path_buf())
        .build();

    assert_eq!(shell.run_script("exec /definitely/missing/program"), 126);
    assert_eq!(std::env::current_dir().unwrap(), original_cwd);
}

#[test]
fn failed_exec_does_not_run_from_an_unusable_shell_cwd() {
    const CWD_ENV: &str = "EPSH_TEST_UNUSABLE_SHELL_CWD";
    if let Ok(cwd) = std::env::var(CWD_ENV) {
        let mut shell = Shell::builder().cwd(cwd.into()).build();
        assert_eq!(shell.run_script("exec /bin/echo unexpectedly-executed"), 126);
        return;
    }

    let unavailable_cwd = tempfile::tempdir().unwrap();
    let cwd = unavailable_cwd.path().to_path_buf();
    drop(unavailable_cwd);
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "failed_exec_does_not_run_from_an_unusable_shell_cwd",
            "--nocapture",
        ])
        .env(CWD_ENV, cwd)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(!output
        .stdout
        .windows(b"unexpectedly-executed".len())
        .any(|window| window == b"unexpectedly-executed"));
}

#[test]
fn user_signal_traps_run_their_action() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for signal_name in ["USR1", "SIGUSR1"] {
        let script = format!("trap 'echo caught' {signal_name}; kill -USR1 $$; echo after");
        let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
            .args(["-c", script.as_str()])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        let mut lines: Vec<_> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort_unstable();
        assert_eq!(lines, ["after", "caught"]);
    }
}

#[test]
fn trap_reports_uncatchable_signals() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut shell = Shell::new();
    assert_eq!(shell.run_script("trap '' KILL"), 1);
}
