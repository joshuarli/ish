pub struct Job {
    pub pgid: i32,
    pub cmd: String,
    /// Saved terminal attributes — restored on fg resume.
    pub termios: crate::term::Termios,
}

/// Resume a suspended job in the foreground.
/// Returns exit status (148 if stopped again).
pub fn resume_job(job: &mut Option<Job>) -> i32 {
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
    // Give the job the foreground and restore its terminal settings.
    // This is critical for programs like vim/less that set raw mode —
    // without this, they'd resume with the shell's terminal settings.
    let stdin = unsafe { std::os::fd::BorrowedFd::borrow_raw(0) };
    let _ = rustix::termios::tcsetpgrp(stdin, pgid);
    let _ = rustix::termios::tcsetattr(stdin, rustix::termios::OptionalActions::Drain, &j.termios);
    let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::CONT);

    // Wait for the job, detecting if it stops again
    let result = rustix::process::waitpgid(pgid, rustix::process::WaitOptions::UNTRACED);

    // Reclaim the terminal
    let _ = rustix::termios::tcsetpgrp(stdin, rustix::process::getpgrp());

    match result {
        Ok(Some((_, status))) if status.stopped() => {
            let new_termios = rustix::termios::tcgetattr(stdin).ok();
            *job = Some(Job {
                pgid: j.pgid,
                cmd: j.cmd,
                termios: new_termios.unwrap_or(j.termios),
            });
            148
        }
        Ok(Some((_, status))) if status.exited() => status.exit_status().unwrap_or(1),
        Ok(Some((_, status))) if status.signaled() => {
            128 + status.terminating_signal().unwrap_or(0)
        }
        _ => 1,
    }
}
