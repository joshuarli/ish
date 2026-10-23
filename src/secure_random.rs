use std::io;

#[cfg(target_os = "linux")]
pub(crate) fn fill_bytes(bytes: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < bytes.len() {
        match rustix::rand::getrandom(&mut bytes[filled..], rustix::rand::GetRandomFlags::empty()) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "random source returned no bytes")),
            Ok(count) => filled += count,
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn fill_bytes(bytes: &mut [u8]) -> io::Result<()> {
    for chunk in bytes.chunks_mut(256) {
        loop {
            // SAFETY: chunk is writable for its length, which does not exceed getentropy's 256-byte limit.
            if unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) } == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ish secure randomness supports Linux and macOS");
