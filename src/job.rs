pub struct Job {
    pub handle: epsh::eval::ForegroundJob,
    pub cmd: String,
    pub termios: crate::term::Termios,
    pub capture: Option<crate::capture::Session>,
}

pub const WAIT_FAILURE_STATUS: i32 = 1;

pub fn resume_job(epsh: &mut epsh::eval::Shell, slot: &mut Option<Job>, last_output: &mut crate::capture::OutputBuffer) -> i32 {
    let Some(mut job) = slot.take() else {
        eprintln!("ish: fg: no suspended job");
        return 1;
    };
    eprintln!("ish: resuming: {}", job.cmd);
    if let Err(error) = rustix::termios::tcsetattr(crate::term::stdin(), rustix::termios::OptionalActions::Drain, &job.termios) {
        eprintln!("ish: fg: cannot restore terminal attributes: {error}");
        *slot = Some(job);
        return 1;
    }
    let result = epsh.resume_foreground(&mut job.handle);
    match result {
        Ok(None) => {
            match crate::term::save_termios() {
                Ok(termios) => job.termios = termios,
                Err(error) => {
                    eprintln!("ish: fg: cannot save terminal attributes: {error}");
                    drop(job);
                    crate::term::restore_startup_termios();
                    return WAIT_FAILURE_STATUS;
                }
            }
            *slot = Some(job);
            crate::term::restore_startup_termios();
            148
        }
        Ok(Some(status)) => {
            crate::term::restore_startup_termios();
            if let Some(capture) = job.capture.take() { capture.finish_into(last_output); }
            status.code()
        }
        Err(error) => {
            eprintln!("ish: fg: cannot resume {}: {error}", job.cmd);
            crate::term::restore_startup_termios();
            if job.handle.is_complete() {
                if let Some(capture) = job.capture.take() { capture.finish_into(last_output); }
            } else { *slot = Some(job); }
            WAIT_FAILURE_STATUS
        }
    }
}

pub fn abandon(job: Job) { drop(job); }
