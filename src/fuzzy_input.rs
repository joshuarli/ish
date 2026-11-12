use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::fd::OwnedFd;
use std::path::Path;

/// Input already read by the editor, shared with fresh fz workers. Workers read
/// one byte at a time; unread bytes return in front of the next editor input.
pub struct FuzzyInput { source: File }
impl FuzzyInput {
    pub fn new(bytes: &[u8], directory: &Path) -> io::Result<(Self, OwnedFd)> {
        let mut source = pending_file(directory)?;
        source.write_all(bytes)?;
        source.rewind()?;
        let input = source.try_clone()?.into();
        Ok((Self { source }, input))
    }
    pub fn finish(mut self) -> io::Result<Vec<u8>> {
        let mut remaining = Vec::new();
        self.source.read_to_end(&mut remaining)?;
        Ok(remaining)
    }
}

#[cfg(target_os = "linux")]
fn pending_file(_: &Path) -> io::Result<File> {
    rustix::fs::memfd_create(c"ish-fz-input", rustix::fs::MemfdFlags::CLOEXEC).map(File::from).map_err(Into::into)
}

#[cfg(target_os = "macos")]
fn pending_file(directory: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    for _ in 0..8 {
        let mut bytes = [0u8; 16];
        crate::secure_random::fill_bytes(&mut bytes)?;
        let name = format!(".ish-fz-{:032x}", u128::from_ne_bytes(bytes));
        let path = directory.join(name);
        let source = match std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        std::fs::remove_file(path)?;
        return Ok(source);
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "cannot allocate picker input file"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_queue_and_unread_suffix_share_one_file_offset() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
        std::fs::create_dir_all(&directory).unwrap();
        let bytes = (0..32768).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let (session, descriptor) = FuzzyInput::new(&bytes, &directory).unwrap();
        let mut worker = File::from(descriptor);
        let mut consumed = vec![0u8; 12345];
        worker.read_exact(&mut consumed).unwrap();
        assert_eq!(consumed, bytes[..12345]);
        drop(worker);
        assert_eq!(session.finish().unwrap(), bytes[12345..]);
    }
}
