use epsh::error::ExitStatus;
use epsh::eval::{ProgramOutcome, Shell};
use epsh::parser::Parser;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn isolated(name: &str, probe: impl FnOnce()) {
    if std::env::var("EPSH_JOB_PROBE").as_deref() == Ok(name) {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptor outputs; no name or terminal settings are requested.
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, 0);
        // SAFETY: openpty returned two owned descriptors and they are wrapped once.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        rustix::process::ioctl_tiocsctty(&slave).unwrap();
        // SAFETY: the isolated session owns these terminal and standard-input descriptors and signal dispositions.
        unsafe {
            assert_eq!(libc::dup2(slave.as_raw_fd(), 0), 0);
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
        probe();
        drop((master, slave));
        return;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", name, "--nocapture"]).env("EPSH_JOB_PROBE", name)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
    // SAFETY: setsid is the only operation before exec and establishes the probe's private session.
    unsafe { command.pre_exec(|| if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }); }
    let mut child = command.spawn().unwrap();
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || sender.send(child.wait()).unwrap());
    let result = receiver.recv_timeout(Duration::from_secs(5));
    if result.is_err() {
        let _ = rustix::process::kill_process_group(rustix::process::Pid::from_raw(pid as i32).unwrap(), rustix::process::Signal::KILL);
    }
    waiter.join().unwrap();
    assert!(result.expect("foreground probe hung").unwrap().success(), "probe failed: {name}");
}

#[test]
fn foreground_jobs_retain_pipeline_status_negation_and_repeated_stops() {
    isolated("foreground_jobs_retain_pipeline_status_negation_and_repeated_stops", || {
        for pipefail in [false, true] {
            for inverted in [false, true] {
                let output = Arc::new(Mutex::new(Vec::<u8>::new()));
                let mut shell = Shell::builder().interactive(true).pipefail(pipefail).stdout_sink(output.clone()).build();
                let prefix = if inverted { "! " } else { "" };
                let source = format!("{prefix}/bin/sh -c 'printf one; kill -STOP $$; printf two; kill -STOP $$; exit 7' | /bin/cat; printf forbidden");
                let program = Parser::new(&source).parse().unwrap();
                let ProgramOutcome::Stopped(mut job) = shell.run_program_outcome(&program) else { panic!("pipeline did not stop"); };
                assert_eq!(shell.resume_foreground(&mut job).unwrap(), None);
                let actual = shell.resume_foreground(&mut job).unwrap().unwrap();
                let expected = ExitStatus::from(if pipefail { 7 } else { 0 });
                assert_eq!(actual, if inverted { expected.inverted() } else { expected });
                assert_eq!(*output.lock().unwrap(), b"onetwo");
                assert_eq!(rustix::termios::tcgetpgrp(std::io::stdin()).unwrap(), rustix::process::getpgrp());
            }
        }
    });
}

#[test]
fn foreground_jobs_keep_completed_final_stage_and_reject_foreign_shells() {
    isolated("foreground_jobs_keep_completed_final_stage_and_reject_foreign_shells", || {
        let mut shell = Shell::builder().interactive(true).build();
        let program = Parser::new("/bin/sh -c 'kill -STOP $$; exit 7' | true").parse().unwrap();
        let ProgramOutcome::Stopped(mut job) = shell.run_program_outcome(&program) else { panic!("pipeline did not stop"); };
        let mut other = Shell::builder().interactive(true).build();
        assert!(other.resume_foreground(&mut job).is_err());
        assert!(other.terminate_foreground(&mut job).is_err());
        assert!(!job.is_complete());
        assert_eq!(shell.resume_foreground(&mut job).unwrap(), Some(ExitStatus::SUCCESS));
    });
}

#[test]
fn foreground_job_drop_reaps_owned_children_and_leaves_unrelated_children_waitable() {
    isolated("foreground_job_drop_reaps_owned_children_and_leaves_unrelated_children_waitable", || {
        let mut unrelated = Command::new("/bin/sh").args(["-c", "exit 19"]).spawn().unwrap();
        let mut shell = Shell::builder().interactive(true).build();
        let program = Parser::new("/bin/sh -c 'kill -STOP $$; exec /bin/sleep 60' | /bin/cat").parse().unwrap();
        let ProgramOutcome::Stopped(job) = shell.run_program_outcome(&program) else { panic!("pipeline did not stop"); };
        let group = rustix::process::Pid::from_raw(job.process_group()).unwrap();
        drop(job);
        assert!(matches!(rustix::process::waitpgid(group, rustix::process::WaitOptions::NOHANG), Err(rustix::io::Errno::CHILD)));
        assert_eq!(unrelated.wait().unwrap().code(), Some(19));
    });
}

#[test]
fn foreground_numeric_adapter_terminates_stopped_job_without_transferring_ownership() {
    isolated("foreground_numeric_adapter_terminates_stopped_job_without_transferring_ownership", || {
        let mut shell = Shell::builder().interactive(true).build();
        assert_eq!(shell.run_script("/bin/sh -c 'kill -STOP $$; exec /bin/sleep 60' | /bin/cat"), 148);
        assert!(matches!(shell.run_program_outcome(&Parser::new("true").parse().unwrap()), ProgramOutcome::Completed(ExitStatus::SUCCESS)));
    });
}

#[test]
fn foreground_cancellation_keeps_an_earlier_stopped_job_owned() {
    isolated("foreground_cancellation_keeps_an_earlier_stopped_job_owned", || {
        struct CancelOnOutput(Arc<std::sync::atomic::AtomicBool>);
        impl std::io::Write for CancelOnOutput {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sink = Arc::new(Mutex::new(CancelOnOutput(cancelled.clone())));
        let mut shell = Shell::builder().interactive(true).cancel_flag(cancelled.clone()).stdout_sink(sink).build();
        let program = Parser::new("/bin/sh -c 'kill -STOP $$; exit 7'").parse().unwrap();
        let ProgramOutcome::Stopped(mut earlier) = shell.run_program_outcome(&program) else { panic!("command did not stop"); };
        assert_eq!(shell.run_script("/bin/sh -c 'printf cancel; exec /bin/sleep 60'"), 130);
        cancelled.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(shell.resume_foreground(&mut earlier).unwrap(), Some(ExitStatus::from(7)));
    });
}

#[test]
fn foreground_resume_relinquishes_a_child_reaped_by_the_embedder() {
    isolated("foreground_resume_relinquishes_a_child_reaped_by_the_embedder", || {
        let mut shell = Shell::builder().interactive(true).build();
        let program = Parser::new("/bin/sh -c 'kill -STOP $$; exit 7'").parse().unwrap();
        let ProgramOutcome::Stopped(mut job) = shell.run_program_outcome(&program) else { panic!("command did not stop"); };
        let child = rustix::process::Pid::from_raw(job.process_group()).unwrap();
        rustix::process::kill_process(child, rustix::process::Signal::KILL).unwrap();
        rustix::process::waitpid(Some(child), rustix::process::WaitOptions::empty()).unwrap().unwrap();
        assert!(shell.resume_foreground(&mut job).is_err());
        assert!(job.is_complete());
        drop(job);
        assert!(matches!(shell.run_program_outcome(&Parser::new("true").parse().unwrap()), ProgramOutcome::Completed(ExitStatus::SUCCESS)));
    });
}

#[test]
fn foreground_partial_ownership_loss_is_reported_after_remaining_members_finish() {
    isolated("foreground_partial_ownership_loss_is_reported_after_remaining_members_finish", || {
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().interactive(true).stdout_sink(output.clone()).build();
        let program = Parser::new("/bin/sh -c 'kill -STOP $$; exit 7' | /bin/sh -c '/bin/cat >/dev/null; printf remaining'").parse().unwrap();
        let ProgramOutcome::Stopped(mut job) = shell.run_program_outcome(&program) else { panic!("pipeline did not stop"); };
        let child = rustix::process::Pid::from_raw(job.process_group()).unwrap();
        rustix::process::kill_process(child, rustix::process::Signal::KILL).unwrap();
        rustix::process::waitpid(Some(child), rustix::process::WaitOptions::empty()).unwrap().unwrap();
        assert!(shell.resume_foreground(&mut job).is_err());
        assert!(job.is_complete());
        assert_eq!(*output.lock().unwrap(), b"remaining");
    });
}
