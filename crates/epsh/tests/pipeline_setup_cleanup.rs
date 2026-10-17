#![cfg(target_os = "linux")]

#[path = "common/unix.rs"]
mod unix_support;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use epsh::error::ShellError;
use epsh::eval::Shell;
use epsh::parser::Parser;

#[test]
fn pipeline_flush_failure_preserves_earlier_background_child() {
    const PROBE: &str = "EPSH_PIPELINE_SETUP_PROBE";
    if std::env::var_os(PROBE).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pipeline_flush_failure_preserves_earlier_background_child", "--nocapture"])
            .env(PROBE, "1")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
            .output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let gate = directory.path().join("gate");
    unix_support::create_fifo(&ready).unwrap();
    unix_support::create_fifo(&gate).unwrap();
    let mut release = OpenOptions::new().read(true).write(true).open(&gate).unwrap();
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().cwd(directory.path().to_path_buf()).stdout_sink(output.clone()).build();
    assert_eq!(shell.run_script("/bin/sh -c 'printf ready >ready; read line <gate; printf completed >output; printf captured; exit 7' & background=$!"), 0);
    let background = shell.get_var("background").unwrap().to_owned();
    let mut started = [0; 5];
    File::open(ready).unwrap().read_exact(&mut started).unwrap();
    assert_eq!(&started, b"ready");

    let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
    // SAFETY: this isolated probe owns stdout; rustix's standard-descriptor helpers are not enabled.
    assert_eq!(unsafe { libc::dup2(full.as_raw_fd(), 1) }, 1);
    std::io::stdout().write_all(b"buffered parent output").unwrap();
    let program = Parser::new("printf pipeline | /bin/cat").parse().unwrap();
    let result = shell.eval_command(&program.commands[0]);
    let null = OpenOptions::new().write(true).open("/dev/null").unwrap();
    // SAFETY: this isolated probe owns stdout; rustix's standard-descriptor helpers are not enabled.
    assert_eq!(unsafe { libc::dup2(null.as_raw_fd(), 1) }, 1);
    std::io::stdout().flush().unwrap();

    release.write_all(b"go\n").unwrap();
    let status = shell.run_script(&format!("wait {background}"));
    drop(shell);
    assert!(matches!(result, Err(ShellError::Io(error)) if error.raw_os_error() == Some(libc::ENOSPC)));
    assert_eq!(status, 7, "the earlier background child must remain waitable");
    assert_eq!(std::fs::read(directory.path().join("output")).unwrap(), b"completed");
    assert_eq!(*output.lock().unwrap(), b"captured");
    assert!(matches!(rustix::process::waitpid(None, rustix::process::WaitOptions::NOHANG), Err(rustix::io::Errno::CHILD)));
}
