use std::fmt;

/// Source position for error reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub offset: usize,
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

/// Shell errors and control flow signals.
///
/// This enum serves two purposes:
/// - **Errors**: `Syntax`, `CommandNotFound`, `Io`, `Runtime`, `Cancelled`, `TimedOut`
/// - **Control flow**: `Exit`, `Return`, `Break`, `Continue`
///
/// Control flow variants propagate up the call stack via `Result`, replacing
/// the setjmp/longjmp mechanism used by dash and posh.
///
/// # For embedders
///
/// After calling [`Shell::run_program`](crate::eval::Shell::run_program), errors are
/// already handled internally — you get an [`ExitStatus`] back. `ShellError` is only
/// relevant if you call lower-level methods like [`Shell::eval_command`](crate::eval::Shell::eval_command)
/// directly, or if you need to distinguish cancellation from timeout in custom control flow.
#[derive(Debug)]
pub enum ShellError {
    /// `exit [n]` — terminate the shell.
    Exit(ExitStatus),
    /// `return [n]` — return from function or dot-script.
    Return(ExitStatus),
    /// `break [n]` — break from n enclosing loops.
    Break(usize),
    /// `continue [n]` — continue nth enclosing loop.
    Continue(usize),
    /// Syntax error during parsing.
    Syntax { msg: String, span: Span },
    /// Command not found in PATH or builtins.
    CommandNotFound(String),
    /// I/O error (redirections, pipes, file operations).
    Io(std::io::Error),
    /// Runtime error with source position.
    Runtime { msg: String, span: Span },
    /// Execution was cancelled via the cancel flag.
    Cancelled,
    /// Execution exceeded the configured timeout.
    TimedOut,
    /// A foreground process was stopped (SIGTSTP/SIGSTOP).
    /// Only produced in interactive mode. The embedder should save the job
    /// and resume it later with `kill(pgid, SIGCONT)` + `tcsetpgrp`.
    Stopped {
        /// PID of the stopped process.
        pid: i32,
        /// Process group ID (for resuming the entire pipeline).
        pgid: i32,
    },
}

impl ShellError {
    /// True if execution was interrupted (cancelled or timed out).
    pub fn is_interrupted(&self) -> bool {
        matches!(self, ShellError::Cancelled | ShellError::TimedOut)
    }

    /// True if this is a cancellation.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, ShellError::Cancelled)
    }

    /// True if this is a timeout.
    pub fn is_timed_out(&self) -> bool {
        matches!(self, ShellError::TimedOut)
    }

    /// True if a foreground process was stopped (interactive mode).
    pub fn is_stopped(&self) -> bool {
        matches!(self, ShellError::Stopped { .. })
    }

    /// If this is an Exit or Return, return the exit code.
    pub fn exit_code(&self) -> Option<ExitStatus> {
        match self {
            ShellError::Exit(s) | ShellError::Return(s) => Some(*s),
            _ => None,
        }
    }
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellError::Exit(n) => write!(f, "exit {n}"),
            ShellError::Return(n) => write!(f, "return {n}"),
            ShellError::Break(n) => write!(f, "break {n}"),
            ShellError::Continue(n) => write!(f, "continue {n}"),
            ShellError::Syntax { msg, span } => write!(f, "{span}: syntax error: {msg}"),
            ShellError::CommandNotFound(name) => write!(f, "{name}: not found"),
            ShellError::Io(e) => write!(f, "{e}"),
            ShellError::Runtime { msg, span } => write!(f, "{span}: {msg}"),
            ShellError::Cancelled => write!(f, "cancelled"),
            ShellError::TimedOut => write!(f, "timed out"),
            ShellError::Stopped { pid, pgid } => write!(f, "stopped (pid={pid}, pgid={pgid})"),
        }
    }
}

impl std::error::Error for ShellError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShellError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ShellError {
    fn from(e: std::io::Error) -> Self {
        ShellError::Io(e)
    }
}

/// Alias for `std::result::Result<T, ShellError>`.
pub type Result<T> = std::result::Result<T, ShellError>;

/// Shell exit status (0-255).
///
/// Wraps an `i32` with type-safe constructors and accessors. Named constants
/// for common statuses: `SUCCESS` (0), `FAILURE` (1), `MISUSE` (2),
/// `NOT_FOUND` (127), `NOT_EXECUTABLE` (126).
///
/// The inner field is private — use `.code()` to extract, `From<i32>` to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExitStatus(i32);

impl ExitStatus {
    pub const SUCCESS: ExitStatus = ExitStatus(0);
    pub const FAILURE: ExitStatus = ExitStatus(1);
    pub const MISUSE: ExitStatus = ExitStatus(2);
    pub const NOT_FOUND: ExitStatus = ExitStatus(127);
    pub const NOT_EXECUTABLE: ExitStatus = ExitStatus(126);

    /// The raw exit code as an integer.
    pub fn code(self) -> i32 {
        self.0
    }

    /// True if the exit code is 0 (success).
    pub fn success(self) -> bool {
        self.0 == 0
    }

    /// Return `SUCCESS` if true, `FAILURE` if false.
    pub fn from_bool(ok: bool) -> Self {
        if ok { Self::SUCCESS } else { Self::FAILURE }
    }

    /// Logical negation: `SUCCESS` becomes `FAILURE` and vice versa.
    pub fn inverted(self) -> Self {
        Self::from_bool(!self.success())
    }

    /// Create from a raw waitpid status (decodes WIFEXITED/WEXITSTATUS/WTERMSIG).
    pub fn from_wait(status: i32) -> Self {
        if crate::sys::wifexited(status) {
            ExitStatus(crate::sys::wexitstatus(status))
        } else {
            ExitStatus(128 + crate::sys::wtermsig(status))
        }
    }

    /// Create from a signal number (128 + sig).
    pub fn from_signal(sig: i32) -> Self {
        ExitStatus(128 + sig)
    }
}

impl From<i32> for ExitStatus {
    fn from(n: i32) -> Self {
        ExitStatus(n)
    }
}

impl From<ExitStatus> for i32 {
    fn from(s: ExitStatus) -> i32 {
        s.0
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
