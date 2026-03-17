pub struct Job {
    pub pgid: libc::pid_t,
    pub cmd: String,
}
