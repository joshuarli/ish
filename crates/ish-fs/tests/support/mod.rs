use std::io;
use std::path::Path;

pub fn make_fifo(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        ).map_err(Into::into)
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `name` is NUL-terminated and remains live; rustix does not expose mkfifo on Darwin.
        let result = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        if result == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
}
