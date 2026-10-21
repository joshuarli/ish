use std::ffi::{OsStr, OsString};
use std::io;

use rustix::fd::BorrowedFd;
#[cfg(target_os = "linux")]
use rustix::io::Errno;

mod metadata;

#[cfg(target_os = "linux")]
pub(crate) const METADATA_CAPTURE_BYTES: usize = 16 * 1024 * 1024;
#[cfg(target_os = "macos")]
pub(crate) const METADATA_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

const ZERO_SCAN_WINDOW_BYTES: usize = 4 * 1024;

#[cfg(any(target_os = "macos", test))]
use crate::Cancellation;
#[cfg(target_os = "macos")]
use crate::roots::Timestamp;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CopyMethod {
    Clone,
    Native,
    Buffered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CopyStats {
    pub method: CopyMethod,
    pub logical_bytes: u64,
    pub streamed_bytes: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccelerationError {
    Unsupported,
    Io(Errno),
}

#[derive(Clone, Debug)]
pub(crate) struct Xattr {
    pub name: OsString,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ExtendedMetadata {
    pub xattrs: Vec<Xattr>,
    pub acl: Option<Vec<u8>>,
    pub flags: u32,
    #[cfg(target_os = "macos")]
    pub birthtime: Option<Timestamp>,
    #[cfg(target_os = "macos")]
    pub resource_fork_size: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum MetadataTarget<'a> {
    Fd(BorrowedFd<'a>),
    Symlink { parent: BorrowedFd<'a>, name: &'a OsStr },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Preservation {
    Copy,
    Move,
}

pub(crate) fn requires_final_metadata(metadata: &ExtendedMetadata) -> bool {
    #[cfg(target_os = "linux")]
    let flags = linux::BLOCKING_FLAGS;
    #[cfg(target_os = "macos")]
    let flags = macos::BLOCKING_FLAGS;
    metadata.flags & flags != 0
}

pub(crate) fn rename_noreplace(
    from_parent: BorrowedFd<'_>,
    from_name: &OsStr,
    to_parent: BorrowedFd<'_>,
    to_name: &OsStr,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let flags = rustix::fs::RenameFlags::NOREPLACE;
    #[cfg(target_os = "macos")]
    let flags = rustix::fs::RenameFlags::NOREPLACE;
    rustix::fs::renameat_with(from_parent, from_name, to_parent, to_name, flags).map_err(Into::into)
}

pub(crate) fn rename_exchange(
    from_parent: BorrowedFd<'_>,
    from_name: &OsStr,
    to_parent: BorrowedFd<'_>,
    to_name: &OsStr,
) -> io::Result<()> {
    rustix::fs::renameat_with(from_parent, from_name, to_parent, to_name, rustix::fs::RenameFlags::EXCHANGE).map_err(Into::into)
}

pub(crate) fn sync_file(fd: BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let result = rustix::fs::fcntl_fullfsync(fd);
    #[cfg(target_os = "linux")]
    let result = rustix::fs::fsync(fd);
    result.map_err(Into::into)
}

pub(crate) fn sync_directory(fd: BorrowedFd<'_>) -> io::Result<()> {
    rustix::fs::fsync(fd).map_err(Into::into)
}

pub(crate) fn reports_dense_allocation(
    source: BorrowedFd<'_>,
    destination: BorrowedFd<'_>,
    length: u64,
) -> io::Result<bool> {
    if length == 0 { return Ok(false); }
    let allocated = |fd| -> io::Result<u64> {
        let blocks = rustix::fs::fstat(fd)?.st_blocks;
        let blocks = u64::try_from(blocks).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "filesystem returned a negative allocated block count"))?;
        Ok(blocks.saturating_mul(512))
    };
    Ok(allocated(source)? < length && allocated(destination)? >= length)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn buffered_copy(
    src: BorrowedFd<'_>,
    dst: BorrowedFd<'_>,
    length: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
) -> io::Result<u64> {
    buffered_copy_with(
        length,
        scratch,
        cancel,
        |buffer, offset| rustix::io::pread(src, buffer, offset).map_err(Into::into),
        |buffer, offset| rustix::io::pwrite(dst, buffer, offset).map_err(Into::into),
        |length| rustix::fs::ftruncate(dst, length).map_err(Into::into),
    )
}

#[cfg(any(target_os = "macos", test))]
fn buffered_copy_with(
    length: u64,
    scratch: &mut [u8],
    cancel: &Cancellation,
    mut read_at: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    mut write_at: impl FnMut(&[u8], u64) -> io::Result<usize>,
    mut finalize: impl FnMut(u64) -> io::Result<()>,
) -> io::Result<u64> {
    if scratch.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "copy buffer is empty"));
    }
    let mut copied = 0;
    while copied < length {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "transfer cancelled"));
        }
        let wanted = usize::try_from((length - copied).min(scratch.len() as u64)).expect("bounded by buffer");
        let read = loop {
            if cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "transfer cancelled"));
            }
            match read_at(&mut scratch[..wanted], copied) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "source ended before captured length"));
        }
        if read > wanted {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "source read exceeded the requested length"));
        }
        visit_nonzero_ranges(&scratch[..read], |range| {
            let mut written = 0;
            while written < range.len() {
                if cancel.is_cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "transfer cancelled"));
                }
                let remaining = &scratch[range.start + written..range.end];
                match write_at(remaining, copied + range.start as u64 + written as u64) {
                    Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "destination write made no progress")),
                    Ok(count) if count <= remaining.len() => {
                        written += count;
                    }
                    Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "destination write exceeded the requested length")),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })?;
        copied += read as u64;
    }
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "transfer cancelled"));
        }
        match finalize(length) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => break result?,
        }
    }
    Ok(copied)
}

fn visit_nonzero_ranges(
    bytes: &[u8],
    mut visit: impl FnMut(std::ops::Range<usize>) -> io::Result<()>,
) -> io::Result<()> {
    let mut range_start = None;
    let mut range_end = 0;
    for window_start in (0..bytes.len()).step_by(ZERO_SCAN_WINDOW_BYTES) {
        let window_end = (window_start + ZERO_SCAN_WINDOW_BYTES).min(bytes.len());
        let window = &bytes[window_start..window_end];
        if let Some(first) = window.iter().position(|byte| *byte != 0) {
            if range_start.is_none() {
                range_start = Some(window_start + first);
            }
            let last = window
                .iter()
                .rposition(|byte| *byte != 0)
                .expect("a nonzero window has a final nonzero byte");
            range_end = window_start + last + 1;
        } else if let Some(start) = range_start.take() {
            visit(start..range_end)?;
        }
    }
    if let Some(start) = range_start {
        visit(start..range_end)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::fs::{self, File, OpenOptions};
    #[cfg(target_os = "linux")]
    use std::os::fd::AsFd;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::{FileExt, MetadataExt};
    #[cfg(target_os = "linux")]
    use std::path::{Path, PathBuf};
    #[cfg(target_os = "linux")]
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(target_os = "linux")]
    use super::buffered_copy;
    use super::buffered_copy_with;
    use crate::Cancellation;
    use std::cell::Cell;
    use std::io;

    #[cfg(target_os = "linux")]
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    #[cfg(target_os = "linux")]
    struct Fixture(PathBuf);

    #[cfg(target_os = "linux")]
    impl Fixture {
        fn new() -> Self {
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("workspace root");
            let parent = workspace.join(".work/tmp");
            fs::create_dir_all(&parent).expect("create fixture parent");
            loop {
                let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
                let path = parent.join(format!(
                    "ish-buffered-sparse-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create fixture: {error}"),
                }
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn buffered_copy_skips_zero_chunks_and_preserves_exact_length() {
        let fixture = Fixture::new();
        let source_path = fixture.path("source");
        let destination_path = fixture.path("destination");
        let length = 1024 * 1024_u64;
        let source = File::create(&source_path).expect("create source");
        source.set_len(length).expect("set sparse source length");
        source
            .write_at(b"nonzero data", 64 * 1024 + 3)
            .expect("write source extent");
        drop(source);

        let source = File::open(&source_path).expect("open source");
        let destination = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&destination_path)
            .expect("create destination");
        let mut scratch = vec![0; 4096];
        let streamed = buffered_copy(
            source.as_fd(),
            destination.as_fd(),
            length,
            &mut scratch,
            &Cancellation::default(),
        )
        .expect("copy sparse source");

        assert_eq!(streamed, length);
        assert_eq!(fs::metadata(&destination_path).expect("stat destination").len(), length);
        assert_eq!(fs::read(&source_path).expect("read source"), fs::read(&destination_path).expect("read destination"));
        assert!(fs::metadata(&destination_path).expect("stat destination").blocks() * 512 < length / 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn buffered_copy_trims_edges_with_a_large_buffer() {
        let fixture = Fixture::new();
        let source_path = fixture.path("source");
        let destination_path = fixture.path("destination");
        let length = 1024 * 1024_u64;
        let source = File::create(&source_path).expect("create source");
        source.set_len(length).expect("set sparse source length");
        source
            .write_at(b"nonzero data", 64 * 1024 + 3)
            .expect("write source extent");
        drop(source);

        let source = File::open(&source_path).expect("open source");
        let destination = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&destination_path)
            .expect("create destination");
        let mut scratch = vec![0; length as usize];
        let streamed = buffered_copy(
            source.as_fd(),
            destination.as_fd(),
            length,
            &mut scratch,
            &Cancellation::default(),
        )
        .expect("copy sparse source with a large transfer buffer");

        assert_eq!(streamed, length);
        assert_eq!(fs::metadata(&destination_path).expect("stat destination").len(), length);
        assert_eq!(fs::read(&source_path).expect("read source"), fs::read(&destination_path).expect("read destination"));
        assert!(fs::metadata(&destination_path).expect("stat destination").blocks() * 512 < length / 2);
    }

    #[test]
    fn buffered_copy_retries_short_io_and_interrupted_calls() {
        let source = *b"\0\0\0\0ABCD\0\0\0\0";
        let mut destination = vec![0; source.len()];
        let mut scratch = [0; 4];
        let mut read_offsets = Vec::new();
        let mut read_interrupted = false;
        let mut write_offsets = Vec::new();
        let mut write_interrupted = false;
        let finalized = Cell::new(None);
        let streamed = buffered_copy_with(
            source.len() as u64,
            &mut scratch,
            &Cancellation::default(),
            |buffer, offset| {
                read_offsets.push(offset);
                if !read_interrupted {
                    read_interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let offset = offset as usize;
                let count = buffer.len().min(2).min(source.len() - offset);
                buffer[..count].copy_from_slice(&source[offset..offset + count]);
                Ok(count)
            },
            |buffer, offset| {
                write_offsets.push(offset);
                if !write_interrupted {
                    write_interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let count = buffer.len().min(1);
                let offset = offset as usize;
                destination[offset..offset + count].copy_from_slice(&buffer[..count]);
                Ok(count)
            },
            |length| {
                finalized.set(Some(length));
                Ok(())
            },
        )
        .expect("short IO and interruptions complete");

        assert_eq!(streamed, source.len() as u64);
        assert_eq!(destination, source);
        assert_eq!(read_offsets, [0, 0, 2, 4, 6, 8, 10]);
        assert_eq!(write_offsets, [4, 4, 5, 6, 7]);
        assert_eq!(finalized.get(), Some(source.len() as u64));
    }

    #[test]
    fn buffered_copy_rejects_zero_reads_and_writes() {
        let finalized = Cell::new(false);
        let mut scratch = [0; 2];
        let read_error = buffered_copy_with(
            1,
            &mut scratch,
            &Cancellation::default(),
            |_, _| Ok(0),
            |_, _| Ok(1),
            |_| {
                finalized.set(true);
                Ok(())
            },
        )
        .expect_err("short source must fail");
        assert_eq!(read_error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(!finalized.get());

        let write_error = buffered_copy_with(
            1,
            &mut scratch,
            &Cancellation::default(),
            |buffer, _| {
                buffer[0] = 1;
                Ok(1)
            },
            |_, _| Ok(0),
            |_| {
                finalized.set(true);
                Ok(())
            },
        )
        .expect_err("zero-length destination write must fail");
        assert_eq!(write_error.kind(), io::ErrorKind::WriteZero);
        assert!(!finalized.get());
    }

    #[test]
    fn buffered_copy_cancellation_stops_repeated_interrupted_reads() {
        let cancel = Cancellation::default();
        let read_attempts = Cell::new(0);
        let finalized = Cell::new(false);
        let mut scratch = [0; 1];
        let error = buffered_copy_with(
            1,
            &mut scratch,
            &cancel,
            |_, _| {
                read_attempts.set(read_attempts.get() + 1);
                cancel.cancel();
                Err(io::ErrorKind::Interrupted.into())
            },
            |_, _| Ok(1),
            |_| {
                finalized.set(true);
                Ok(())
            },
        )
        .expect_err("cancellation must stop the read retry loop");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(read_attempts.get(), 1);
        assert!(!finalized.get());
    }

    #[test]
    fn buffered_copy_retries_interrupted_final_length_and_propagates_finalize_errors() {
        let mut destination = Vec::new();
        let mut scratch = [0; 3];
        let finalize_attempts = Cell::new(0);
        let streamed = buffered_copy_with(
            3,
            &mut scratch,
            &Cancellation::default(),
            |buffer, offset| {
                assert_eq!(offset, 0);
                buffer.copy_from_slice(b"xyz");
                Ok(3)
            },
            |buffer, offset| {
                assert_eq!(offset, 0);
                destination.extend_from_slice(buffer);
                Ok(buffer.len())
            },
            |length| {
                assert_eq!(length, 3);
                finalize_attempts.set(finalize_attempts.get() + 1);
                if finalize_attempts.get() == 1 {
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    Ok(())
                }
            },
        )
        .expect("interrupted final-length operation retries");
        assert_eq!(streamed, 3);
        assert_eq!(destination, b"xyz");
        assert_eq!(finalize_attempts.get(), 2);

        let mut destination = Vec::new();
        let mut scratch = [0; 3];
        let finalize_length = Cell::new(None);
        let error = buffered_copy_with(
            3,
            &mut scratch,
            &Cancellation::default(),
            |buffer, _| {
                buffer.copy_from_slice(b"abc");
                Ok(3)
            },
            |buffer, _| {
                destination.extend_from_slice(buffer);
                Ok(buffer.len())
            },
            |length| {
                finalize_length.set(Some(length));
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            },
        )
        .expect_err("finalize storage failure must be returned");
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(destination, b"abc");
        assert_eq!(finalize_length.get(), Some(3));
    }

    #[test]
    fn buffered_copy_propagates_storage_errors_after_partial_progress() {
        for errno in [libc::ENOSPC, libc::EDQUOT] {
            let mut destination = Vec::new();
            let mut write_offsets = Vec::new();
            let finalized = Cell::new(false);
            let mut scratch = [0; 2];
            let error = buffered_copy_with(
                3,
                &mut scratch,
                &Cancellation::default(),
                |buffer, offset| {
                    let data = b"abc";
                    let offset = offset as usize;
                    let count = buffer.len().min(data.len() - offset);
                    buffer[..count].copy_from_slice(&data[offset..offset + count]);
                    Ok(count)
                },
                |buffer, offset| {
                    write_offsets.push(offset);
                    if offset == 0 {
                        destination.push(buffer[0]);
                        Ok(1)
                    } else {
                        Err(io::Error::from_raw_os_error(errno))
                    }
                },
                |_| {
                    finalized.set(true);
                    Ok(())
                },
            )
            .expect_err("storage failure must be returned");

            assert_eq!(error.raw_os_error(), Some(errno));
            assert_eq!(destination, b"a");
            assert_eq!(write_offsets, [0, 1]);
            assert!(!finalized.get());
        }
    }

    #[test]
    fn buffered_copy_stops_after_cancellation_during_partial_write() {
        let cancel = Cancellation::default();
        let mut destination = Vec::new();
        let mut read_offsets = Vec::new();
        let finalized = Cell::new(false);
        let mut scratch = [0; 2];
        let error = buffered_copy_with(
            4,
            &mut scratch,
            &cancel,
            |buffer, offset| {
                read_offsets.push(offset);
                let data = b"abcd";
                let offset = offset as usize;
                buffer[..2].copy_from_slice(&data[offset..offset + 2]);
                Ok(2)
            },
            |buffer, offset| {
                destination.push(buffer[0]);
                cancel.cancel();
                assert_eq!(offset, 0);
                Ok(1)
            },
            |_| {
                finalized.set(true);
                Ok(())
            },
        )
        .expect_err("cancellation must stop remaining writes");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(destination, b"a");
        assert_eq!(read_offsets, [0]);
        assert!(!finalized.get());
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{apply_final_metadata, apply_metadata, capture_metadata, copy_data, mount_id, prepare_directory};

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{apply_final_metadata, apply_metadata, capture_metadata, copy_data, copy_resource_fork, mount_id, prepare_directory, try_clone_to_dir};

#[cfg(target_os = "macos")]
pub(crate) fn address_space_bytes() -> io::Result<u64> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::uninit();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    #[allow(deprecated, reason = "libc exposes the current task port without another dependency")]
    // SAFETY: Darwin initializes this process's task send right before process entry.
    let task = unsafe { libc::mach_task_self_ };
    // SAFETY: Darwin task_info writes this flavor's packed ABI into the sized output buffer.
    let result = unsafe {
        libc::task_info(
            task,
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS {
        return Err(io::Error::other(format!("task_info failed with Mach status {result}")));
    }
    if count != libc::MACH_TASK_BASIC_INFO_COUNT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "task_info returned an unexpected structure size"));
    }
    // SAFETY: A successful complete MACH_TASK_BASIC_INFO query initializes this output.
    Ok(unsafe { info.assume_init() }.virtual_size)
}
