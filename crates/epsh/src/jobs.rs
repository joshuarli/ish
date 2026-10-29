use super::*;

/// Program completion or transfer of an owned stopped foreground group.
#[must_use]
pub enum ProgramOutcome {
    Completed(ExitStatus),
    Stopped(ForegroundJob),
}

#[derive(Clone, Copy)]
enum MemberState {
    Running,
    Stopped,
    Exited(ExitStatus),
    Reaped(ExitStatus),
    Relinquished,
}

impl MemberState {
    fn status(self) -> Option<ExitStatus> {
        match self {
            Self::Exited(status) | Self::Reaped(status) => Some(status),
            Self::Running | Self::Stopped | Self::Relinquished => None,
        }
    }

    fn is_owned(self) -> bool { matches!(self, Self::Running | Self::Stopped | Self::Exited(_)) }
    fn is_live(self) -> bool { matches!(self, Self::Running | Self::Stopped) }
}

struct Member {
    pid: i32,
    state: MemberState,
    control: Option<HeldDescriptor>,
}

struct JobRelay {
    handles: Vec<std::thread::JoinHandle<std::io::Result<()>>>,
    descriptors: Vec<HeldDescriptor>,
}

/// An owned, non-cloneable foreground group, its member statuses, and output relays.
/// Dropping the handle terminates and reaps children still owned by its process.
#[must_use]
pub struct ForegroundJob {
    owner_pid: u32,
    shell: Arc<()>,
    pgid: i32,
    members: Vec<Member>,
    relays: Vec<JobRelay>,
    pipefail: bool,
    inverted: bool,
    ownership_lost: bool,
    /// The members own the terminal descriptor as their controlling terminal
    /// in a session of their own. Terminal handoff and reclamation across
    /// that boundary would fail, so the parent skips both.
    pub(crate) converged: bool,
}

impl ForegroundJob {
    pub(super) fn invert(&mut self) { self.inverted = !self.inverted; }

    /// The process group for diagnostics. This does not transfer wait ownership.
    pub fn process_group(&self) -> i32 { self.pgid }

    /// Whether every direct child has exited or been relinquished.
    pub fn is_complete(&self) -> bool { self.members.iter().all(|member| !member.state.is_owned()) }

    fn terminate(&mut self) -> crate::error::Result<()> {
        let mut owned = Vec::new();
        let mut failure = None;
        for member in &self.members {
            if !member.state.is_owned() { continue; }
            let Some(pid) = rustix::process::Pid::from_raw(member.pid) else { continue; };
            let observed = loop {
                match rustix::process::waitid(rustix::process::WaitId::Pid(pid), rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT | rustix::process::WaitIdOptions::NOHANG) {
                    Err(rustix::io::Errno::INTR) => continue,
                    result => break result,
                }
            };
            match observed {
                Ok(_) => owned.push(pid),
                Err(rustix::io::Errno::CHILD) => {}
                Err(error) => failure = Some(ShellError::Io(error.into())),
            }
        }
        if !owned.is_empty() {
            if self.pgid != rustix::process::getpgrp().as_raw_pid() {
                if let Some(group) = rustix::process::Pid::from_raw(self.pgid)
                    && let Err(error) = rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
                    && error != rustix::io::Errno::SRCH
                { failure = Some(ShellError::Io(error.into())); }
            } else {
                for &pid in &owned {
                    if let Err(error) = rustix::process::kill_process(pid, rustix::process::Signal::KILL)
                        && error != rustix::io::Errno::SRCH
                    { failure = Some(ShellError::Io(error.into())); }
                }
            }
        }
        for pid in owned {
            let waited = loop {
                match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()) {
                    Err(rustix::io::Errno::INTR) => continue,
                    result => break result,
                }
            };
            if let Err(error) = waited { failure = Some(ShellError::Io(error.into())); }
        }
        for relay in &self.relays {
            for descriptor in &relay.descriptors {
                drop(descriptor.lock().unwrap().take());
            }
        }
        self.members.clear();
        if let Err(error) = self.finish_relays() { failure = Some(error); }
        failure.map_or(Ok(()), Err)
    }

    fn result(&self) -> ExitStatus {
        let last = self.members.last().and_then(|member| member.state.status()).unwrap_or(ExitStatus::FAILURE);
        let status = if self.pipefail {
            self.members.iter().rev().filter_map(|member| member.state.status()).find(|status| !status.success()).unwrap_or(last)
        } else { last };
        if self.inverted { status.inverted() } else { status }
    }

    fn finish_relays(&mut self) -> crate::error::Result<()> {
        let mut failure = None;
        for relay in self.relays.drain(..) {
            for handle in relay.handles {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => failure = Some(ShellError::Io(error)),
                    Err(_) => failure = Some(ShellError::Io(std::io::Error::other("output relay panicked"))),
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for ForegroundJob {
    fn drop(&mut self) {
        if std::process::id() != self.owner_pid {
            for relay in self.relays.drain(..) { std::mem::forget(relay); }
            return;
        }
        if let Err(error) = self.terminate() {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "epsh: cannot clean up foreground job: {error}");
        }
    }
}

impl Shell {
    pub(super) fn wait_foreground(&mut self, children: &[i32], pgid: i32) -> crate::error::Result<ExitStatus> {
        let mut job = ForegroundJob {
            owner_pid: std::process::id(), shell: self.job_owner.clone(), pgid,
            members: children.iter().map(|&pid| Member { pid, state: MemberState::Running, control: self.child_controls.remove(&pid) }).collect(),
            relays: Vec::new(), pipefail: self.opts.pipefail, inverted: false, ownership_lost: false,
            converged: self.opts.foreground_ctty && !self.in_forked_child,
        };
        for &pid in children {
            if let Some(relay) = self.child_relays.remove(&pid) {
                let descriptors: Vec<_> = relay.descriptors.into_iter().filter_map(|index| self.redirection_fds[index].take()).collect();
                self.private_descriptors.extend(descriptors.iter().map(Arc::downgrade));
                job.relays.push(JobRelay { handles: relay.handles, descriptors });
            }
            self.forget_child(pid);
        }
        self.trim_redirection_fds();
        let result = self.wait_foreground_job(&mut job);
        self.private_descriptors.retain(|descriptor| descriptor.strong_count() > 0);
        match result {
            Ok(Some(status)) => Ok(status),
            Ok(None) => {
                self.pending_job = Some(job);
                Err(ShellError::Stopped { pid: *children.last().unwrap(), pgid })
            }
            Err(error) => Err(error),
        }
    }

    fn wait_foreground_job(&mut self, job: &mut ForegroundJob) -> crate::error::Result<Option<ExitStatus>> {
        let handoff = self.opts.interactive && !self.in_forked_child && !job.converged;
        if handoff { self.foreground_terminal(job.pgid)?; }
        let result = self.observe_foreground_job(job);
        if let Err(error) = &result {
            if error.is_interrupted() {
                if let Err(error) = job.terminate() { self.err_msg(&format!("epsh: foreground cleanup failed: {error}")); }
            } else if !job.is_complete() && job.pgid != rustix::process::getpgrp().as_raw_pid()
                && let Some(group) = rustix::process::Pid::from_raw(job.pgid)
                && let Err(stop_error) = rustix::process::kill_process_group(group, rustix::process::Signal::STOP)
                && stop_error != rustix::io::Errno::SRCH
            {
                self.err_msg(&format!("epsh: cannot stop foreground group after wait failure: {stop_error}"));
            }
        }
        if self.opts.interactive && !self.in_forked_child && !job.converged {
            let reclaimed = self.foreground_terminal(rustix::process::getpgrp().as_raw_pid());
            match (&result, reclaimed) {
                (Ok(_), Err(error)) => return Err(error),
                (Err(_), Err(error)) => self.err_msg(&format!("epsh: cannot reclaim terminal: {error}")),
                _ => {}
            }
        }
        result
    }

    fn observe_foreground_job(&mut self, job: &mut ForegroundJob) -> crate::error::Result<Option<ExitStatus>> {
        let mut stopping = false;
        loop {
            self.check_cancel()?;
            let mut all_stopped = true;
            let mut live = 0;
            for member in &mut job.members {
                if !member.state.is_live() { continue; }
                let pid = rustix::process::Pid::from_raw(member.pid).unwrap();
                let options = rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT | rustix::process::WaitIdOptions::NOHANG
                    | if self.opts.interactive { rustix::process::WaitIdOptions::STOPPED } else { rustix::process::WaitIdOptions::empty() };
                match rustix::process::waitid(rustix::process::WaitId::Pid(pid), options) {
                    Ok(Some(status)) if status.stopped() => {
                        let consumed = loop {
                            match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::UNTRACED | rustix::process::WaitOptions::NOHANG) {
                                Err(rustix::io::Errno::INTR) => continue,
                                result => break result.map_err(std::io::Error::from)?,
                            }
                        };
                        if let Some((_, status)) = consumed {
                            if status.stopped() {
                                member.state = MemberState::Stopped;
                                if !stopping {
                                    if job.pgid == rustix::process::getpgrp().as_raw_pid() {
                                        rustix::process::kill_process(pid, rustix::process::Signal::STOP).map_err(std::io::Error::from)?;
                                    } else {
                                        rustix::process::kill_process_group(rustix::process::Pid::from_raw(job.pgid).unwrap(), rustix::process::Signal::STOP).map_err(std::io::Error::from)?;
                                    }
                                    stopping = true;
                                }
                            } else {
                                member.state = MemberState::Reaped(ExitStatus::from_wait(status.as_raw()));
                                if let Some(reader) = member.control.take() { Shell::read_child_refusal(&reader)?; }
                            }
                        }
                    }
                    Ok(Some(status)) => {
                        let status = if let Some(code) = status.exit_status() { ExitStatus::from(code) }
                            else { ExitStatus::from_signal(status.terminating_signal().unwrap_or(0)) };
                        member.state = MemberState::Exited(status);
                        if let Some(reader) = member.control.take() { Shell::read_child_refusal(&reader)?; }
                    }
                    Ok(None) | Err(rustix::io::Errno::INTR) => {}
                    Err(rustix::io::Errno::CHILD) => {
                        member.state = MemberState::Relinquished;
                        member.control.take();
                        if !job.ownership_lost { self.err_msg("epsh: lost ownership of a foreground child"); }
                        job.ownership_lost = true;
                    }
                    Err(error) => return Err(ShellError::Io(error.into())),
                }
                if member.state.is_live() {
                    live += 1;
                    all_stopped &= matches!(member.state, MemberState::Stopped);
                }
            }
            if live == 0 {
                while job.relays.iter().flat_map(|relay| &relay.handles).any(|handle| !handle.is_finished()) {
                    self.check_cancel()?;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                for member in &mut job.members {
                    if !member.state.is_owned() { continue; }
                    let pid = rustix::process::Pid::from_raw(member.pid).unwrap();
                    let waited = loop {
                        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()) {
                            Err(rustix::io::Errno::INTR) => continue,
                            result => break result,
                        }
                    };
                    match waited {
                        Ok(Some((_, status))) => member.state = MemberState::Reaped(ExitStatus::from_wait(status.as_raw())),
                        Err(rustix::io::Errno::CHILD) => {
                            member.state = MemberState::Relinquished;
                            if !job.ownership_lost { self.err_msg("epsh: lost ownership of a foreground child"); }
                            job.ownership_lost = true;
                        }
                        Err(error) => return Err(ShellError::Io(error.into())),
                        Ok(None) => return Err(ShellError::Io(std::io::Error::other("foreground child exited without a wait status"))),
                    }
                }
                job.finish_relays()?;
                if job.ownership_lost { return Err(ShellError::Io(std::io::Error::from_raw_os_error(libc::ECHILD))); }
                return Ok(Some(job.result()));
            }
            if stopping && all_stopped { return Ok(None); }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub(super) fn foreground_terminal(&self, pgid: i32) -> crate::error::Result<()> {
        let pid = rustix::process::Pid::from_raw(pgid).ok_or_else(|| ShellError::Io(std::io::Error::other("invalid foreground process group")))?;
        sys::set_foreground_group(self.terminal_raw_fd(), pid.as_raw_pid()).map_err(ShellError::Io)
    }

    /// Resume this shell's stopped group and wait for completion or another stop.
    /// `None` retains a stopped group. A completed group returns `Some(status)`.
    /// Recoverable errors retain the handle. Cancellation terminates its children.
    pub fn resume_foreground(&mut self, job: &mut ForegroundJob) -> crate::error::Result<Option<ExitStatus>> {
        if job.owner_pid != std::process::id() || !Arc::ptr_eq(&job.shell, &self.job_owner) {
            return Err(ShellError::Io(std::io::Error::other("foreground job belongs to a different shell or process")));
        }
        for member in &mut job.members {
            if !member.state.is_owned() { continue; }
            let pid = rustix::process::Pid::from_raw(member.pid).unwrap();
            loop {
                match rustix::process::waitid(rustix::process::WaitId::Pid(pid), rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT | rustix::process::WaitIdOptions::NOHANG) {
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(rustix::io::Errno::CHILD) => {
                        member.state = MemberState::Relinquished;
                        member.control.take();
                        if !job.ownership_lost { self.err_msg("epsh: lost ownership of a foreground child"); }
                        job.ownership_lost = true;
                        break;
                    }
                    Err(error) => return Err(ShellError::Io(error.into())),
                    Ok(_) => break,
                }
            }
        }
        if job.is_complete() {
            job.finish_relays()?;
            return Err(ShellError::Io(std::io::Error::from_raw_os_error(libc::ECHILD)));
        }
        if self.opts.interactive && !self.in_forked_child && !job.converged { self.foreground_terminal(job.pgid)?; }
        if let Err(error) = rustix::process::kill_process_group(rustix::process::Pid::from_raw(job.pgid).unwrap(), rustix::process::Signal::CONT) {
            if self.opts.interactive && !self.in_forked_child && !job.converged { self.foreground_terminal(rustix::process::getpgrp().as_raw_pid())?; }
            return Err(ShellError::Io(error.into()));
        }
        for member in &mut job.members { if matches!(member.state, MemberState::Stopped) { member.state = MemberState::Running; } }
        let result = self.wait_foreground_job(job);
        if let Ok(Some(status)) = result { self.exit_status = status; }
        result
    }

    /// Terminate and reap this shell's owned group, reporting cleanup failures.
    pub fn terminate_foreground(&mut self, job: &mut ForegroundJob) -> crate::error::Result<()> {
        if job.owner_pid != std::process::id() || !Arc::ptr_eq(&job.shell, &self.job_owner) {
            return Err(ShellError::Io(std::io::Error::other("foreground job belongs to a different shell or process")));
        }
        job.terminate()
    }
}
