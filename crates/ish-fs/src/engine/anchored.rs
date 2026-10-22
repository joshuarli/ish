use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, Mode};

use super::records::Records;
use crate::roots::{self, Identity, Observation};

pub(super) use fdu_posix::identity::{DIRECTORY_FLAGS as DIRECTORY, READ_FILE_FLAGS as READ_FILE};

pub(super) fn unchanged(expected: Observation, actual: Observation) -> bool {
    let mut expected = expected;
    expected.atime = actual.atime;
    expected == actual
}

pub(super) fn require_unchanged(parent: BorrowedFd<'_>, name: &OsStr, expected: Observation) -> io::Result<()> {
    if unchanged(expected, roots::observe(parent, name)?) { Ok(()) } else { Err(io::Error::other("source identity or version changed")) }
}

pub(super) fn identity(parent: BorrowedFd<'_>, name: &OsStr, expected: Identity) -> io::Result<()> {
    if roots::observe(parent, name)?.identity == expected { Ok(()) } else { Err(io::Error::other("directory entry identity changed")) }
}

pub(super) fn parent(root: BorrowedFd<'_>, path: &Path, directories: &Mutex<Records>, destination: bool) -> io::Result<(OwnedFd, OsString)> {
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no final name"))?.to_os_string();
    let directory = path.parent().unwrap_or_else(|| Path::new(""));
    Ok((open_directory(root, directory, directories, destination)?, name))
}

pub(super) fn open_directory(root: BorrowedFd<'_>, path: &Path, directories: &Mutex<Records>, destination: bool) -> io::Result<OwnedFd> {
    open_directory_with_recovery(root, path, directories, destination, false)
}

pub(super) fn checked_parent(root: BorrowedFd<'_>, path: &Path, directories: &Mutex<Records>, destination: bool) -> io::Result<(OwnedFd, OsString)> {
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no final name"))?.to_os_string();
    Ok((open_checked_directory(root, path.parent().unwrap_or_else(|| Path::new("")), directories, destination)?, name))
}

pub(super) fn open_checked_directory(root: BorrowedFd<'_>, path: &Path, directories: &Mutex<Records>, destination: bool) -> io::Result<OwnedFd> {
    open_directory_with_recovery(root, path, directories, destination, true)
}

fn open_directory_with_recovery(root: BorrowedFd<'_>, path: &Path, directories: &Mutex<Records>, destination: bool, recover: bool) -> io::Result<OwnedFd> {
    let mut fd = fs::openat(root, ".", DIRECTORY, Mode::empty())?;
    if recover { fdu_posix::quarantine::Session::check_recovery(fd.as_fd())?; }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        let Component::Normal(name) = component else { return Err(io::Error::new(io::ErrorKind::InvalidData, "non-relative traversal record")); };
        prefix.push(name);
        let expected = directories.lock().map_err(|_| io::Error::other("directory records poisoned"))?.find(&prefix)?.ok_or_else(|| io::Error::other("missing directory identity record"))?;
        let next = fs::openat(fd.as_fd(), name, DIRECTORY, Mode::empty())?;
        let actual = roots::observe_fd(next.as_fd())?.identity;
        let expected = if destination { expected.published } else { expected.observed.identity };
        if actual != expected { return Err(io::Error::other("traversal directory identity changed")); }
        if recover { fdu_posix::quarantine::Session::check_recovery(next.as_fd())?; }
        fd = next;
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(super::super::unique_name("anchor-record-test"));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { std::fs::remove_dir_all(&self.0).unwrap(); }
    }

    fn entry_record(root: BorrowedFd<'_>, path: PathBuf) -> io::Result<crate::engine::records::Record> {
        let (parent, name) = if path.parent().is_some_and(|parent| !parent.as_os_str().is_empty()) {
            let mut current = rustix::fs::openat(root, ".", DIRECTORY, Mode::empty())?;
            for component in path.parent().unwrap().components() {
                let Component::Normal(component) = component else { return Err(io::Error::other("invalid test path")); };
                current = rustix::fs::openat(current.as_fd(), component, DIRECTORY, Mode::empty())?;
            }
            (current, path.file_name().unwrap().to_os_string())
        } else {
            (rustix::fs::openat(root, ".", DIRECTORY, Mode::empty())?, path.file_name().unwrap().to_os_string())
        };
        let observed = roots::observe(parent.as_fd(), &name)?;
        Ok(crate::engine::records::Record { source: path, destination: PathBuf::new(), observed, published: observed.identity, output: None, parent: None, anchor: None, ancestry: Vec::new(), final_flags: 0 })
    }

    #[test]
    fn wide_deep_skipped_merge_anchor_lookup_work_is_bounded() {
        let fixture = Fixture::new();
        let root_path = fixture.0.join("root");
        std::fs::create_dir(&root_path).unwrap();
        let merge_path = root_path.join("merge");
        std::fs::create_dir(&merge_path).unwrap();
        let root = rustix::fs::open(&root_path, DIRECTORY, Mode::empty()).unwrap();
        let mut directories = Records::new(&fixture.0).unwrap();
        let merge = PathBuf::from("merge");
        directories.push(&entry_record(root.as_fd(), merge.clone()).unwrap()).unwrap();
        let mut paths = Vec::new();
        for index in 0..128 {
            let name = format!("wide-{index:04}");
            std::fs::create_dir(merge_path.join(&name)).unwrap();
            let path = merge.join(&name);
            directories.push(&entry_record(root.as_fd(), path.clone()).unwrap()).unwrap();
            paths.push(path);
        }
        let mut skipped = Records::new(&fixture.0).unwrap();
        let mut deep_path = merge_path.join("deep");
        std::fs::create_dir(&deep_path).unwrap();
        let mut deep = merge.join("deep");
        directories.push(&entry_record(root.as_fd(), deep.clone()).unwrap()).unwrap();
        for index in 0..32 {
            let name = format!("level-{index:02}");
            deep_path.push(&name);
            std::fs::create_dir(&deep_path).unwrap();
            deep.push(&name);
            directories.push(&entry_record(root.as_fd(), deep.clone()).unwrap()).unwrap();
        }
        for path in &paths {
            let skipped_path = path.join("retained");
            let wide_path = merge_path.join(path.file_name().unwrap()).join("retained");
            std::fs::write(wide_path, b"retained").unwrap();
            let record = entry_record(root.as_fd(), skipped_path).unwrap();
            skipped.push(&record).unwrap();
        }
        let directory_records = Mutex::new(directories);
        for path in &paths { open_directory(root.as_fd(), path, &directory_records, true).unwrap(); }
        open_directory(root.as_fd(), &deep, &directory_records, false).unwrap();
        for path in &paths {
            let skipped_path = path.join("retained");
            assert!(directory_records.lock().unwrap().find(&skipped_path).unwrap().is_none());
            assert_eq!(skipped.find(&skipped_path).unwrap().unwrap().source, skipped_path);
        }
        let work = directory_records.lock().unwrap().lookup_work() + skipped.lookup_work();
        let records = 1 + paths.len() + 33;
        let lookups = paths.len() * 2 + 34 + paths.len() * 2;
        let logarithm = usize::BITS - records.leading_zeros();
        assert!(work <= ((records + lookups) * logarithm as usize * 5) as u64);
        eprintln!("wide_deep_anchor records={records} lookups={lookups} record_visits={work}");
    }
}
