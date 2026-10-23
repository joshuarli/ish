#[path = "common/unix.rs"]
mod unix_support;

use std::process::{Command, Stdio};
use std::os::unix::process::CommandExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use epsh::eval::Shell;
use unix_support::create_fifo;

fn isolated(name: &str, probe: impl FnOnce()) {
    if std::env::var("EPSH_LIFECYCLE_PROBE").as_deref() == Ok(name) {
        probe();
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("EPSH_LIFECYCLE_PROBE", name)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit())
        .process_group(0).spawn().unwrap();
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || tx.send(child.wait()).unwrap());
    let result = rx.recv_timeout(Duration::from_secs(3));
    if result.is_err() {
        let _ = rustix::process::kill_process_group(rustix::process::Pid::from_raw(pid as i32).unwrap(), rustix::process::Signal::KILL);
    }
    waiter.join().unwrap();
    assert!(result.unwrap_or_else(|_| panic!("probe did not terminate: {name}")).unwrap().success(), "probe failed: {name}");
}

fn captured(script: &str) -> (i32, Vec<u8>) {
    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder().stdout_sink(out.clone()).build();
    let status = shell.run_script(script);
    let bytes = out.lock().unwrap().clone();
    (status, bytes)
}

#[test]
fn captured_output_obeys_pipeline_subshell_and_substitution_destinations() {
    isolated("captured_output_obeys_pipeline_subshell_and_substitution_destinations", || {
        assert_eq!(captured("printf hi | /bin/cat; printf '[%s]' \"$(/bin/echo sub)\"; (echo grouped)"), (0, b"hi[sub]grouped\n".to_vec()));
    });
}

#[test]
fn captured_output_obeys_redirections_and_descriptor_duplication() {
    isolated("captured_output_obeys_redirections_and_descriptor_duplication", || {
        let dir = tempfile::tempdir().unwrap();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let err = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).stdout_sink(out.clone()).stderr_sink(err.clone()).build();
        assert_eq!(shell.run_script("echo builtin >builtin; /bin/echo external >external; echo error >&2; { echo nested; } >group; { echo captured >&3; } 3>&1 >another; /bin/sh -c 'printf alias >&3' 3>&1; /bin/sh -c 'printf out; printf err >&2' 2>&1 >ordered; echo final"), 0);
        assert_eq!(std::fs::read(dir.path().join("builtin")).unwrap(), b"builtin\n");
        assert_eq!(std::fs::read(dir.path().join("external")).unwrap(), b"external\n");
        assert_eq!(std::fs::read(dir.path().join("group")).unwrap(), b"nested\n");
        assert_eq!(std::fs::read(dir.path().join("ordered")).unwrap(), b"out");
        assert_eq!(*out.lock().unwrap(), b"captured\naliaserrfinal\n");
        assert_eq!(*err.lock().unwrap(), b"error\n");
    });
}

#[test]
fn bare_redirections_apply_effects_and_assignment_status_tracks_substitutions() {
    isolated("bare_redirections_apply_effects_and_assignment_status_tracks_substitutions", || {
        let dir = tempfile::tempdir().unwrap();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).stdout_sink(out.clone()).build();
        assert_eq!(shell.run_script(">created; x=1 >assigned; false; x=2; echo bare=$?; x=$(false); echo sub=$?; x=$(false)$(true); echo last=$?"), 0);
        assert_eq!(std::fs::read(dir.path().join("created")).unwrap(), b"");
        assert_eq!(std::fs::read(dir.path().join("assigned")).unwrap(), b"");
        assert_eq!(*out.lock().unwrap(), b"bare=0\nsub=1\nlast=0\n");
    });
}

#[test]
fn nested_command_exec_retains_permanent_redirections() {
    isolated("nested_command_exec_retains_permanent_redirections", || {
        let dir = tempfile::tempdir().unwrap();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).stdout_sink(out.clone()).build();
        assert_eq!(shell.run_script("command command exec >permanent; echo after"), 0);
        assert_eq!(std::fs::read(dir.path().join("permanent")).unwrap(), b"after\n");
        assert!(out.lock().unwrap().is_empty());
    });
}

#[test]
fn captured_pipeline_output_works_when_process_stdout_is_closed() {
    isolated("captured_pipeline_output_works_when_process_stdout_is_closed", || {
        // SAFETY: this probe owns its isolated process standard output and never reads from it afterward.
        assert_eq!(unsafe { libc::close(1) }, 0);
        assert_eq!(captured("printf hi | /bin/cat; printf '[%s]' \"$(/bin/echo sub)\""), (0, b"hi[sub]".to_vec()));
    });
}

#[test]
fn temporary_redirections_preserve_descriptor_flags() {
    isolated("temporary_redirections_preserve_descriptor_flags", || {
        use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
        let file = std::fs::File::open("/dev/null").unwrap();
        let target = file.as_raw_fd();
        let original = rustix::io::fcntl_getfd(&file).unwrap();
        assert!(original.contains(rustix::io::FdFlags::CLOEXEC));
        let raw = file.into_raw_fd();
        let mut shell = Shell::new();
        assert_eq!(shell.run_script(&format!(": {target}>/dev/null")), 0);
        // SAFETY: the shell restored the descriptor whose ownership remains in this probe.
        let file = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        assert_eq!(rustix::io::fcntl_getfd(&file).unwrap(), original);
    });
}

#[test]
fn output_sink_failure_closes_the_reader_and_reaps_the_writer() {
    isolated("output_sink_failure_closes_the_reader_and_reaps_the_writer", || {
        struct FailedSink;
        impl std::io::Write for FailedSink {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> { Err(std::io::Error::other("sink rejected output")) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let sink = Arc::new(Mutex::new(FailedSink));
        let mut shell = Shell::builder().stdout_sink(sink).build();
        assert_eq!(shell.run_script("/bin/cat /dev/zero"), 2);
    });
}

#[test]
fn command_substitutions_isolate_argument_and_command_effects() {
    isolated("command_substitutions_isolate_argument_and_command_effects", || {
        assert_eq!(captured("x=0; echo \"$(echo $((x+=1)))\"; echo x=$x; echo \"$(command export LEAK=changed)\"; echo leak=$LEAK"), (0, b"1\nx=0\n\nleak=\n".to_vec()));
        assert_eq!(captured("x=outer; echo \"$((0))$( (x=inner); echo $x)\""), (0, b"0outer\n".to_vec()));
    });
}

#[test]
fn large_heredocs_do_not_wait_for_a_reader_during_setup() {
    isolated("large_heredocs_do_not_wait_for_a_reader_during_setup", || {
        let body = "a".repeat(128 * 1024);
        let script = format!("/bin/cat <<'EOF'\n{body}\nEOF\n");
        let (status, output) = captured(&script);
        assert_eq!(status, 0);
        assert_eq!(output, format!("{body}\n").as_bytes());
    });
}

#[test]
fn timeout_interrupts_descendant_output_and_command_substitutions() {
    isolated("timeout_interrupts_descendant_output_and_command_substitutions", || {
        for script in ["/bin/sh -c 'sleep 1 & wait'", "/bin/sh -c 'sleep 1 &'", "echo \"$(/bin/sleep 1)\""] {
            let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
            let mut shell = Shell::builder().stdout_sink(sink).timeout(Duration::from_millis(30)).build();
            let started = std::time::Instant::now();
            assert_eq!(shell.run_script(script), 130);
            assert!(started.elapsed() < Duration::from_millis(500));
        }
    });
}

#[test]
fn cancellation_reaches_blocked_substitutions_and_pipelines() {
    isolated("cancellation_reaches_blocked_substitutions_and_pipelines", || {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct CancelOnWrite(Arc<AtomicBool>);
        impl std::io::Write for CancelOnWrite {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.store(true, Ordering::Relaxed);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        for script in ["echo \"$(/bin/sh -c 'echo ready >&2; sleep 1')\"", "/bin/sh -c 'echo ready >&2; sleep 1' | /bin/cat"] {
            let flag = Arc::new(AtomicBool::new(false));
            let sink = Arc::new(Mutex::new(CancelOnWrite(flag.clone())));
            let mut shell = Shell::builder().cancel_flag(flag.clone()).stderr_sink(sink).build();
            assert_eq!(shell.run_script(script), 130);
            assert!(flag.load(Ordering::Relaxed));
        }
    });
}

#[test]
fn background_relay_descriptors_move_with_user_redirections() {
    isolated("background_relay_descriptors_move_with_user_redirections", || {
        let dir = tempfile::tempdir().unwrap();
        create_fifo(&dir.path().join("gate")).unwrap();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).stdout_sink(out.clone()).build();
        let open_descriptors = || (3..1024).filter(|fd| {
            // SAFETY: fcntl accepts raw descriptor numbers and reports closed descriptors as EBADF.
            unsafe { libc::fcntl(*fd, libc::F_GETFD) >= 0 }
        }).collect::<Vec<_>>();
        let original = open_descriptors();
        assert_eq!(shell.run_script("/bin/sh -c 'read line <gate; printf released' &"), 0);
        let reader = open_descriptors().into_iter().find(|fd| !original.contains(fd)).unwrap();
        let moved_status = shell.run_script(&format!(": {reader}>/dev/null"));
        let moved = open_descriptors().into_iter().find(|fd| !original.contains(fd)).unwrap();
        let duplicated_status = shell.run_script(&format!(": 9<&{moved}"));
        std::fs::write(dir.path().join("gate"), b"go\n").unwrap();
        assert_eq!(shell.run_script("wait"), 0);
        drop(shell);
        assert_eq!(moved_status, 0);
        assert_eq!(duplicated_status, 2);
        assert_eq!(*out.lock().unwrap(), b"released");
    });
}

#[test]
fn sequential_subshells_preserve_every_body_and_success_status() {
    let count = 4096;
    let script = format!("i=0; while test $i -lt {count}; do (printf x) || exit 7; i=$((i+1)); done");
    let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
        .args(["-c", &script])
        .output()
        .unwrap();
    assert!(output.status.success(), "subshell failed: {:?}", output);
    assert_eq!(output.stdout, vec![b'x'; count]);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}
