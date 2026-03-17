use std::fmt;

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
}

#[derive(Debug)]
enum ErrorKind {
    Io(std::io::Error),
    Msg(String),
    GlobNoMatch(String),
    CommandNotFound(String),
    BadSubstitution(String),
}

impl Error {
    pub fn msg(s: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Msg(s.into()),
        }
    }

    pub fn glob_no_match(pattern: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::GlobNoMatch(pattern.into()),
        }
    }

    pub fn command_not_found(cmd: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::CommandNotFound(cmd.into()),
        }
    }

    pub fn bad_substitution(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::BadSubstitution(msg.into()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ErrorKind::Io(e) => write!(f, "{e}"),
            ErrorKind::Msg(s) => write!(f, "{s}"),
            ErrorKind::GlobNoMatch(p) => write!(f, "no matches for glob: {p}"),
            ErrorKind::CommandNotFound(c) => write!(f, "command not found: {c}"),
            ErrorKind::BadSubstitution(m) => write!(f, "bad substitution: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self {
            kind: ErrorKind::Io(e),
        }
    }
}
