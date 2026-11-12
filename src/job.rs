use crate::report;
pub struct Job {
    pub handle: epsh::eval::ForegroundJob,
    pub cmd: String,
    pub termios: crate::term::Termios,
    pub capture: Option<crate::capture::Session>,
    pub fuzzy_input: Option<crate::fuzzy_input::FuzzyInput>,
}

pub const WAIT_FAILURE_STATUS: i32 = 1;

pub fn resume_job(epsh: &mut epsh::eval::Shell, slot: &mut Option<Job>, last_output: &mut crate::capture::OutputBuffer) -> (i32, Vec<u8>) {
    let Some(mut job) = slot.take() else {
        report!("ish: fg: no suspended job");
        return (1, Vec::new());
    };
    match epsh.reap_completed_foreground(&mut job.handle) {
        Ok(Some(status)) => return (status.code(), finish_job(&mut job, last_output)),
        Ok(None) => {}
        Err(error) => {
            report!("ish: fg: cannot inspect job: {error}");
            if !job.handle.is_complete() { *slot = Some(job); }
            return (WAIT_FAILURE_STATUS, Vec::new());
        }
    }
    report!("ish: resuming: {}", job.cmd);
    let converged = job.capture.as_ref().is_some_and(|capture| capture.is_converged());
    if converged {
        let Some(capture) = job.capture.as_ref() else {
            report!("ish: fg: no suspended job");
            return (1, Vec::new());
        };
        if let Some(raw) = capture.real_raw_termios()
            && let Err(error) = rustix::termios::tcsetattr(crate::term::stdin(), rustix::termios::OptionalActions::Drain, &raw)
        {
            report!("ish: fg: cannot restore terminal attributes: {error}");
            *slot = Some(job);
            return (1, Vec::new());
        }
        if let Err(error) = capture.restore_slave_termios(&job.termios) {
            report!("ish: fg: cannot restore terminal attributes: {error}");
            *slot = Some(job);
            return (1, Vec::new());
        }
        capture.set_paused(false);
    } else if let Err(error) = rustix::termios::tcsetattr(crate::term::stdin(), rustix::termios::OptionalActions::Drain, &job.termios) {
        report!("ish: fg: cannot restore terminal attributes: {error}");
        *slot = Some(job);
        return (1, Vec::new());
    }
    let result = epsh.resume_foreground(&mut job.handle);
    match result {
        Ok(None) => {
            let saved = match job.capture.as_ref() {
                Some(capture) if capture.is_converged() => capture.slave_termios(),
                _ => crate::term::save_termios(),
            };
            match saved {
                Ok(termios) => job.termios = termios,
                Err(error) => {
                    report!("ish: fg: cannot save terminal attributes: {error}");
                    drop(job);
                    crate::term::restore_startup_termios();
                    return (WAIT_FAILURE_STATUS, Vec::new());
                }
            }
            if let Some(capture) = job.capture.as_ref() {
                capture.set_paused(true);
            }
            *slot = Some(job);
            crate::term::restore_startup_termios();
            (148, Vec::new())
        }
        Ok(Some(status)) => {
            crate::term::restore_startup_termios();
            let tail = finish_job(&mut job, last_output);
            (status.code(), tail)
        }
        Err(error) => {
            report!("ish: fg: cannot resume {}: {error}", job.cmd);
            crate::term::restore_startup_termios();
            if job.handle.is_complete() {
                let tail = finish_job(&mut job, last_output);
                (WAIT_FAILURE_STATUS, tail)
            } else {
                *slot = Some(job);
                (WAIT_FAILURE_STATUS, Vec::new())
            }
        }
    }
}

fn finish_job(job: &mut Job, last_output: &mut crate::capture::OutputBuffer) -> Vec<u8> {
    let mut tail = match job.fuzzy_input.take().map(crate::fuzzy_input::FuzzyInput::finish).transpose() {
        Ok(tail) => tail.unwrap_or_default(),
        Err(error) => { report!("ish: cannot return picker input: {error}"); Vec::new() }
    };
    tail.extend(finish_capture(job.capture.take(), last_output));
    tail
}

fn finish_capture(capture: Option<crate::capture::Session>, last_output: &mut crate::capture::OutputBuffer) -> Vec<u8> {
    match capture {
        Some(capture) => capture.finish_into(last_output),
        None => Vec::new(),
    }
}

pub fn abandon(job: Job) { drop(job); }
