use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

static WRITER: AtomicI32 = AtomicI32::new(-1);
static PENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn notify(signal: i32) {
    // SAFETY: errno is thread-local. The handler uses only lock-free atomics
    // and write on a nonblocking socket kept alive until dispositions restore.
    unsafe {
        #[cfg(target_os = "macos")]
        let errno = libc::__error();
        #[cfg(target_os = "linux")]
        let errno = libc::__errno_location();
        let saved = *errno;
        PENDING.store(signal, Ordering::Relaxed);
        let byte = signal as u8;
        libc::write(WRITER.load(Ordering::Relaxed), (&byte as *const u8).cast(), 1);
        *errno = saved;
    }
}

pub(crate) struct Termination {
    reader: UnixStream,
    _writer: UnixStream,
    previous: Vec<(i32, libc::sigaction)>,
    original_mask: Option<libc::sigset_t>,
}
impl Termination {
    pub(crate) fn install() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        if WRITER.compare_exchange(-1, writer.as_raw_fd(), Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return Err(io::Error::other("a standalone terminal session is already active"));
        }
        PENDING.store(0, Ordering::Relaxed);
        let mut guard = Self { reader, _writer: writer, previous: Vec::new(), original_mask: None };
        // SAFETY: these are initialized ABI signal records; handlers only
        // notify the owning event loop. The full original mask is retained.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            let mut original: libc::sigset_t = std::mem::zeroed();
            if libc::sigemptyset(&mut set) == -1 { return Err(io::Error::last_os_error()); }
            for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGQUIT] {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut previous: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, std::ptr::null(), &mut previous) == -1 { return Err(io::Error::last_os_error()); }
                if signal == libc::SIGHUP && previous.sa_sigaction == libc::SIG_IGN { continue; }
                action.sa_sigaction = notify as *const () as libc::sighandler_t;
                if libc::sigemptyset(&mut action.sa_mask) == -1 || libc::sigaction(signal, &action, &mut previous) == -1 {
                    return Err(io::Error::last_os_error());
                }
                guard.previous.push((signal, previous));
                if libc::sigaddset(&mut set, signal) == -1 { return Err(io::Error::last_os_error()); }
            }
            let result = libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, &mut original);
            if result != 0 { return Err(io::Error::from_raw_os_error(result)); }
            guard.original_mask = Some(original);
        }
        Ok(guard)
    }
    pub(crate) fn fd(&self) -> RawFd { self.reader.as_raw_fd() }
    pub(crate) fn pending(&self) -> Option<i32> {
        match PENDING.load(Ordering::Relaxed) { 0 => None, signal => Some(signal) }
    }
    pub(crate) fn restore(&mut self) -> io::Result<()> {
        while let Some((signal, action)) = self.previous.last() {
            // SAFETY: action was captured in this process before installation.
            if unsafe { libc::sigaction(*signal, action, std::ptr::null_mut()) } == -1 { return Err(io::Error::last_os_error()); }
            self.previous.pop();
        }
        if let Some(mask) = self.original_mask {
            // SAFETY: the complete mask was captured on this same thread.
            let result = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) };
            if result != 0 { return Err(io::Error::from_raw_os_error(result)); }
            self.original_mask = None;
        }
        WRITER.store(-1, Ordering::SeqCst);
        Ok(())
    }
}
impl Drop for Termination { fn drop(&mut self) { let _ = self.restore(); } }

#[derive(Debug)]
pub(crate) struct Interrupted(pub(crate) i32);
impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "interrupted by signal {}", self.0) }
}
impl std::error::Error for Interrupted {}
