use std::borrow::Cow;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub use fdu_core::{DirectoryToken, EntryBatch, FileIdentity, ModifiedTime, ScanEvent};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("fdu supports Linux and macOS");

mod fsutil;
mod indexed;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[derive(Clone)]
pub struct RootAnchor {
    fd: Arc<OwnedFd>,
    pub name: Vec<u8>,
    pub identity: FileIdentity,
    pub link_count: u64,
}

impl RootAnchor {
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn try_clone_fd(&self) -> io::Result<OwnedFd> {
        self.fd.try_clone()
    }
}

pub fn open_root_no_follow(path: &Path) -> io::Result<RootAnchor> {
    let open_path = without_trailing_slashes(path);
    let fd = fsutil::open_directory_no_follow(open_path.as_ref())?;
    let (identity, link_count) = fsutil::identity_and_link_count(fd.as_fd())?;
    Ok(RootAnchor {
        fd: Arc::new(fd),
        name: path.as_os_str().as_bytes().to_vec(),
        identity,
        link_count,
    })
}

fn without_trailing_slashes(path: &Path) -> Cow<'_, Path> {
    let bytes = path.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    if end == bytes.len() || end == 0 {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(Path::new(std::ffi::OsStr::from_bytes(&bytes[..end])).to_path_buf())
    }
}

pub fn start_indexed_scan(
    root: RootAnchor,
    sender: SyncSender<ScanEvent>,
    returned_batches: Receiver<EntryBatch>,
    cancelled: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || indexed::scan_indexed(&root, sender, returned_batches, cancelled))
}
