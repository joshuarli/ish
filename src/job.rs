pub struct Job {
    pub pgid: libc::pid_t,
    pub cmd: String,
    /// Saved terminal attributes — restored on fg resume.
    pub termios: libc::termios,
}
