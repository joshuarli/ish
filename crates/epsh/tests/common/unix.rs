use std::path::Path;

#[cfg(target_vendor = "apple")]
use std::ffi::CString;
#[cfg(target_vendor = "apple")]
use std::os::unix::ffi::OsStrExt;

pub(super) fn create_fifo(path: &Path) -> std::io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: rustix excludes mkfifoat on Apple; libc receives a NUL-terminated path and does not retain it.
        if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            path,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map_err(Into::into)
    }
}
