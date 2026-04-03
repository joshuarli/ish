pub struct Job {
    pub pgid: libc::pid_t,
    pub cmd: String,
    /// Saved terminal attributes — restored on fg resume.
    pub termios: libc::termios,
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

    // SAFETY: Give the job the foreground and restore its terminal settings.
    // This is critical for programs like vim/less that set raw mode —
    // without this, they'd resume with the shell's terminal settings.
    unsafe {
        libc::tcsetpgrp(0, j.pgid);
        libc::tcsetattr(0, libc::TCSADRAIN, &j.termios);
        libc::killpg(j.pgid, libc::SIGCONT);
    }

    // Wait for the job, detecting if it stops again
    let mut status = 0i32;
    let result = unsafe { libc::waitpid(-j.pgid, &mut status, libc::WUNTRACED) };

    // Reclaim the terminal
    unsafe {
        libc::tcsetpgrp(0, libc::getpgrp());
    }

    if result > 0 && libc::WIFSTOPPED(status) {
        // Stopped again — re-save the job
        let mut new_termios: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(0, &mut new_termios);
        }
        *job = Some(Job {
            pgid: j.pgid,
            cmd: j.cmd,
            termios: new_termios,
        });
        148
    } else if result > 0 && libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if result > 0 && libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}
