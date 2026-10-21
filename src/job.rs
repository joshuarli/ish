pub struct Job {
    pub pgid: i32,
    pub cmd: String,
    /// Saved terminal attributes — restored on fg resume.
    pub termios: crate::term::Termios,
    /// The relay for the job's output; the job still writes to its terminal.
    pub capture: Option<crate::capture::Session>,
}

/// Give a stopped process group the foreground, restore the terminal settings
/// it left behind, and continue it.
///
/// Restoring the settings is critical for programs like vim/less that set raw
/// mode: without it they would resume with the shell's terminal settings.
pub fn continue_in_foreground(pgid: rustix::process::Pid, termios: &crate::term::Termios) {
    let stdin = crate::term::stdin();
    let _ = rustix::termios::tcsetpgrp(stdin, pgid);
    let _ = rustix::termios::tcsetattr(stdin, rustix::termios::OptionalActions::Drain, termios);
    let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::CONT);
}

/// Resume a suspended job in the foreground.
/// Returns exit status (148 if stopped again). A finished job's output
/// becomes `last_output`.
pub fn resume_job(job: &mut Option<Job>, last_output: &mut crate::capture::OutputBuffer) -> i32 {
    let j = match job.take() {
        Some(j) => j,
        None => {
            eprintln!("ish: fg: no suspended job");
            return 1;
        }
    };

    eprintln!("ish: resuming: {}", j.cmd);

    let pgid = match rustix::process::Pid::from_raw(j.pgid) {
        Some(pgid) => pgid,
        None => return 1,
    };
    continue_in_foreground(pgid, &j.termios);

    let stdin = crate::term::stdin();
    // Wait for the job, detecting if it stops again
    let result = rustix::process::waitpgid(pgid, rustix::process::WaitOptions::UNTRACED);

    // Reclaim the terminal
    let _ = rustix::termios::tcsetpgrp(stdin, rustix::process::getpgrp());

    let status = match result {
        Ok(Some((_, status))) if status.stopped() => {
            let new_termios = rustix::termios::tcgetattr(stdin).ok();
            *job = Some(Job {
                pgid: j.pgid,
                cmd: j.cmd,
                termios: new_termios.unwrap_or(j.termios),
                capture: j.capture,
            });
            crate::term::restore_startup_termios();
            return 148;
        }
        Ok(Some((_, status))) if status.exited() => status.exit_status().unwrap_or(1),
        Ok(Some((_, status))) if status.signaled() => {
            128 + status.terminating_signal().unwrap_or(0)
        }
        _ => 1,
    };
    crate::term::restore_startup_termios();
    if let Some(capture) = j.capture {
        capture.finish_into(last_output);
    }
    status
}
