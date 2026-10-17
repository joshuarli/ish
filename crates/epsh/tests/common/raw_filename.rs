use std::io;
use std::path::Path;

pub(crate) fn create_raw_filename(
    path: &Path,
    create: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<bool> {
    match create(path) {
        Ok(()) => Ok(true),
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => Ok(false),
        Err(error) => Err(error),
    }
}
