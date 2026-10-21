pub struct Job {
    pub pgid: i32,
    pub cmd: String,
    /// Saved terminal attributes — restored on fg resume.
    pub termios: crate::term::Termios,
    /// The relay for the job's output; the job still writes to its terminal.
    pub capture: Option<crate::capture::Session>,
}

pub const WAIT_FAILURE_STATUS: i32 = 1;

pub fn retry_interrupted<T>(
    mut attempt: impl FnMut() -> rustix::io::Result<T>,
) -> rustix::io::Result<T> {
    loop {
        match attempt() {
            Err(rustix::io::Errno::INTR) => {}
            result => return result,
        }
    }
}

pub fn hand_terminal_to(pgid: rustix::process::Pid) -> rustix::io::Result<()> {
    rustix::termios::tcsetpgrp(crate::term::stdin(), pgid)
}

pub fn reclaim_terminal() {
    if let Err(error) = hand_terminal_to(rustix::process::getpgrp()) {
        eprintln!("ish: cannot reclaim the terminal: {error}");
    }
}

pub fn kill_group(pgid: rustix::process::Pid) {
    if let Err(error) = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL) {
        eprintln!("ish: cannot kill process group {}: {error}", pgid.as_raw_pid());
    }
}

/// Give a stopped process group the foreground, restore the terminal settings
/// it left behind, and continue it.
///
/// Restoring the settings is critical for programs like vim/less that set raw
/// mode: without it they would resume with the shell's terminal settings.
pub fn continue_in_foreground(
    pgid: rustix::process::Pid,
    termios: &crate::term::Termios,
) -> rustix::io::Result<()> {
    hand_terminal_to(pgid)?;
    rustix::termios::tcsetattr(
        crate::term::stdin(),
        rustix::termios::OptionalActions::Drain,
        termios,
    )?;
    rustix::process::kill_process_group(pgid, rustix::process::Signal::CONT)
}

/// Resume a suspended job in the foreground.
/// Returns exit status (148 if stopped again). A finished job's output
/// becomes `last_output`. A job that could not be resumed or waited for
/// stays in its slot, except when the kernel reports it is not our child.
pub fn resume_job(job: &mut Option<Job>, last_output: &mut crate::capture::OutputBuffer) -> i32 {
    let j = match job.take() {
        Some(j) => j,
        None => {
            eprintln!("ish: fg: no suspended job");
            return 1;
        }
    };

    let Some(pgid) = rustix::process::Pid::from_raw(j.pgid) else {
        eprintln!("ish: fg: invalid process group {}", j.pgid);
        return 1;
    };

    eprintln!("ish: resuming: {}", j.cmd);

    if let Err(error) = continue_in_foreground(pgid, &j.termios) {
        eprintln!("ish: fg: cannot resume {}: {error}", j.cmd);
        reclaim_terminal();
        crate::term::restore_startup_termios();
        *job = Some(j);
        return 1;
    }

    let stdin = crate::term::stdin();
    let result = retry_interrupted(|| {
        rustix::process::waitpgid(pgid, rustix::process::WaitOptions::UNTRACED)
    });

    reclaim_terminal();

    let status = match result {
        Ok(Some((_, status))) if status.stopped() => {
            let new_termios = match rustix::termios::tcgetattr(stdin) {
                Ok(termios) => termios,
                Err(error) => {
                    eprintln!("ish: fg: cannot save terminal attributes: {error}");
                    kill_group(pgid);
                    let _ = retry_interrupted(|| {
                        rustix::process::waitpgid(pgid, rustix::process::WaitOptions::empty())
                    });
                    crate::term::restore_startup_termios();
                    return WAIT_FAILURE_STATUS;
                }
            };
            *job = Some(Job {
                pgid: j.pgid,
                cmd: j.cmd,
                termios: new_termios,
                capture: j.capture,
            });
            crate::term::restore_startup_termios();
            return 148;
        }
        Ok(Some((_, status))) if status.exited() => status.exit_status().unwrap_or(1),
        Ok(Some((_, status))) if status.signaled() => {
            128 + status.terminating_signal().unwrap_or(0)
        }
        Ok(_) => WAIT_FAILURE_STATUS,
        Err(error) => {
            eprintln!("ish: fg: cannot wait for {}: {error}", j.cmd);
            crate::term::restore_startup_termios();
            if error != rustix::io::Errno::CHILD {
                *job = Some(j);
            }
            return WAIT_FAILURE_STATUS;
        }
    };
    crate::term::restore_startup_termios();
    if let Some(capture) = j.capture {
        capture.finish_into(last_output);
    }
    status
}
