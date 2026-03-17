use crate::parse::{Connector, Pipeline};

/// Remaining segments from a compound command after a pipeline was suspended.
/// E.g., `sleep 2 && echo hi` suspended during sleep → continuation holds `echo hi`.
pub struct Continuation {
    pub connector: Connector,
    pub segments: Vec<(Pipeline, Option<Connector>)>,
}

pub struct Job {
    pub pgid: libc::pid_t,
    pub cmd: String,
    /// Saved terminal attributes — restored on fg resume.
    pub termios: libc::termios,
    /// If this job was suspended mid-chain (e.g. `a && b`), the remaining segments.
    pub continuation: Option<Continuation>,
}
