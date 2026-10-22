use std::fmt;
use std::fmt::Write as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub(super) const REPORT_FD_MIN: RawFd = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetiredHelper {
    SourceEnv,
    SourceEnvIfExists,
    SourceUp,
    SourceUpIfExists,
    Dotenv,
    DotenvIfExists,
}

impl RetiredHelper {
    const ALL: [Self; 6] = [
        Self::SourceEnv, Self::SourceEnvIfExists, Self::SourceUp,
        Self::SourceUpIfExists, Self::Dotenv, Self::DotenvIfExists,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::SourceEnv => "source_env",
            Self::SourceEnvIfExists => "source_env_if_exists",
            Self::SourceUp => "source_up",
            Self::SourceUpIfExists => "source_up_if_exists",
            Self::Dotenv => "dotenv",
            Self::DotenvIfExists => "dotenv_if_exists",
        }
    }
}

impl fmt::Display for RetiredHelper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} was removed; use the automatically discovered .env or put setup in the approved .envrc", self.name())
    }
}

#[derive(Debug)]
pub(super) enum EvaluationFailure {
    RetiredHelper(RetiredHelper),
    MalformedRecord(u8),
    Incomplete,
    Io(std::io::Error),
}

impl fmt::Display for EvaluationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetiredHelper(helper) => write!(f, ".envrc rejected: {helper}"),
            Self::MalformedRecord(byte) => write!(f, "invalid .envrc failure record: {byte}"),
            Self::Incomplete => write!(f, "incomplete .envrc failure channel"),
            Self::Io(error) => write!(f, ".envrc failure channel: {error}"),
        }
    }
}

pub(super) struct FailureChannel {
    pub read: OwnedFd,
    pub write: OwnedFd,
    pub backup: OwnedFd,
}

#[derive(Default)]
pub(super) enum Completion {
    #[default]
    Pending,
    Observed,
}

pub(super) enum ChannelState {
    Open,
    Complete,
}

impl FailureChannel {
    pub fn new() -> Result<Self, EvaluationFailure> {
        let (read, write) = crate::sys::pipe_nonblock_cloexec().map_err(EvaluationFailure::Io)?;
        // SAFETY: pipe_nonblock_cloexec returns uniquely owned descriptors.
        let read = unsafe { OwnedFd::from_raw_fd(read) };
        // SAFETY: pipe_nonblock_cloexec returns uniquely owned descriptors.
        let write = unsafe { OwnedFd::from_raw_fd(write) };
        let reporter = rustix::io::fcntl_dupfd_cloexec(&write, REPORT_FD_MIN)
            .map_err(|error| EvaluationFailure::Io(error.into()))?;
        let backup = rustix::io::fcntl_dupfd_cloexec(&write, REPORT_FD_MIN)
            .map_err(|error| EvaluationFailure::Io(error.into()))?;
        Ok(Self { read, write: reporter, backup })
    }

    pub fn reporters(&self) -> [RawFd; 2] {
        [self.write.as_raw_fd(), self.backup.as_raw_fd()]
    }

    pub fn install_stubs(script: &mut String, reporters: [RawFd; 2]) {
        for (index, helper) in RetiredHelper::ALL.into_iter().enumerate() {
            let record = char::from(b'A' + index as u8);
            writeln!(script, "{}() {{", helper.name()).unwrap();
            for fd in reporters {
                writeln!(script, "  printf '%s' '{record}' >&{fd} 2>/dev/null || :").unwrap();
            }
            writeln!(script, "  printf '%s\\n' 'denv: {helper}' >&2\n  return 2\n}}").unwrap();
        }
    }

    pub fn install(&self, script: &mut String) {
        Self::install_stubs(script, self.reporters());
    }

    pub fn read_state(read: &OwnedFd, completion: &mut Completion) -> Result<ChannelState, EvaluationFailure> {
        let mut record = [0];
        loop {
            match rustix::io::read(read, &mut record) {
                Ok(0) => return match completion {
                    Completion::Observed => Ok(ChannelState::Complete),
                    Completion::Pending => Err(EvaluationFailure::Incomplete),
                },
                Ok(_) if record[0] == b'Z' && matches!(completion, Completion::Pending) => {
                    *completion = Completion::Observed;
                }
                Ok(_) => return Err(match record[0].checked_sub(b'A').and_then(|index| RetiredHelper::ALL.get(usize::from(index))).copied() {
                    Some(helper) => EvaluationFailure::RetiredHelper(helper),
                    None => EvaluationFailure::MalformedRecord(record[0]),
                }),
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::AGAIN) => return Ok(ChannelState::Open),
                Err(error) => return Err(EvaluationFailure::Io(error.into())),
            }
        }
    }

    pub fn finish(self) -> Result<(), EvaluationFailure> {
        match rustix::io::write(&self.write, b"Z") {
            Ok(_) | Err(rustix::io::Errno::AGAIN) => {}
            Err(error) => return Err(EvaluationFailure::Io(error.into())),
        }
        drop(self.write);
        drop(self.backup);
        match Self::read_state(&self.read, &mut Completion::Pending)? {
            ChannelState::Complete => Ok(()),
            ChannelState::Open => Err(EvaluationFailure::Incomplete),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_channel_preserves_typed_failure_without_blocking_reporters() {
        let channel = FailureChannel::new().unwrap();
        let records = [b'F'; 1024];
        loop {
            match rustix::io::write(&channel.write, &records) {
                Ok(_) => {}
                Err(rustix::io::Errno::AGAIN) => break,
                Err(error) => panic!("{error}"),
            }
        }
        assert!(matches!(channel.finish(), Err(EvaluationFailure::RetiredHelper(RetiredHelper::DotenvIfExists))));
    }

    #[test]
    fn malformed_and_incomplete_channels_refuse_the_candidate() {
        let channel = FailureChannel::new().unwrap();
        rustix::io::write(&channel.write, b"?").unwrap();
        assert!(matches!(channel.finish(), Err(EvaluationFailure::MalformedRecord(b'?'))));

        let channel = FailureChannel::new().unwrap();
        let writer = channel.write.try_clone().unwrap();
        assert!(matches!(channel.finish(), Err(EvaluationFailure::Incomplete)));
        drop(writer);
    }
}
