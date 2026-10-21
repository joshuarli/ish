use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs;
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStringExt;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ish_fs::{Cancellation, Context, Event, Jobs, Operands, Operation, Options, Request, execute};

mod support;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    cwd: PathBuf,
    scratch: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap();
        let parent = workspace.join(".work/tmp");
        fs::create_dir_all(&parent).unwrap();
        let root = loop {
            let index = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("preservation-{}-{index}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        };
        let cwd = root.join("cwd");
        let scratch = root.join("scratch");
        fs::create_dir(&cwd).unwrap();
        fs::create_dir(&scratch).unwrap();
        Self { root, cwd, scratch }
    }

    fn request(&self, operands: Operands, options: Options) -> Request {
        Request {
            operation: Operation::Copy,
            operands,
            options,
            context: Context { cwd: self.cwd.clone(), home: None, scratch: self.scratch.clone() },
        }
    }

    fn run(&self, request: &Request) -> ish_fs::Outcome {
        execute(request, &Cancellation::default(), &|_: Event| Ok(()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[cfg(target_os = "linux")]
fn set_times(path: &Path, seconds: i64, nanoseconds: i64) {
    let file = if path.is_dir() {
        File::open(path).unwrap()
    } else {
        OpenOptions::new().read(true).write(true).open(path).unwrap()
    };
    rustix::fs::futimens(
        &file,
        &rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec { tv_sec: seconds, tv_nsec: nanoseconds },
            last_modification: rustix::fs::Timespec { tv_sec: seconds, tv_nsec: nanoseconds },
        },
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
fn set_xattr(path: &Path, name: &[u8], value: &[u8]) {
    let file = if path.is_dir() {
        File::open(path).unwrap()
    } else {
        OpenOptions::new().read(true).write(true).open(path).unwrap()
    };
    rustix::fs::fsetxattr(&file, OsStr::from_bytes(name), value, rustix::fs::XattrFlags::empty()).unwrap();
}

#[cfg(target_os = "linux")]
fn get_xattr(path: &Path, name: &[u8]) -> Vec<u8> {
    let file = File::open(path).unwrap();
    let mut value = vec![0; 4096];
    let length = rustix::fs::fgetxattr(&file, OsStr::from_bytes(name), &mut value).unwrap();
    value.truncate(length);
    value
}

#[cfg(target_os = "linux")]
fn access_acl(owner: u16, named_user: u16, group: u16, mask: u16, other: u16) -> Vec<u8> {
    let entries = [
        (0x01_u16, owner, u32::MAX),
        (0x02, named_user, 1234),
        (0x04, group, u32::MAX),
        (0x10, mask, u32::MAX),
        (0x20, other, u32::MAX),
    ];
    let mut bytes = Vec::with_capacity(4 + entries.len() * 8);
    bytes.extend_from_slice(&2_u32.to_le_bytes());
    for (tag, permission, id) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&permission.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}

#[test]
fn tree_entry_names_retain_printable_control_and_non_utf8_bytes() {
    let fixture = Fixture::new();
    let source_name = OsString::from_vec(b"source-\xff".to_vec());
    let destination_name = OsString::from_vec(b"destination-\xfe".to_vec());
    let source = fixture.cwd.join(&source_name);
    let destination = fixture.cwd.join(&destination_name);
    match fs::create_dir(&source) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!("raw-byte filename fixture unavailable: filesystem rejected a malformed UTF-8 root name");
            return;
        }
        Err(error) => panic!("create raw-byte source directory: {error}"),
    }
    let printable = ["space name", "tab\tname", "line\nname", "-leading-dash", "back\\slash", "unicøde"];
    for (index, name) in printable.iter().enumerate() {
        fs::write(source.join(name), format!("payload-{index}")).unwrap();
    }
    let raw_child = OsString::from_vec(b"child\n\t-\x80".to_vec());
    match fs::write(source.join(&raw_child), b"raw payload") {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!("raw-byte filename fixture unavailable: filesystem rejected a malformed UTF-8 child name");
            return;
        }
        Err(error) => panic!("create raw-byte source child: {error}"),
    }

    let request = fixture.request(
        Operands::Exact { source: source_name.into(), destination: destination_name.into() },
        Options::default(),
    );
    assert_eq!(fixture.run(&request).status(), 0);
    for (index, name) in printable.iter().enumerate() {
        assert_eq!(fs::read(destination.join(name)).unwrap(), format!("payload-{index}").as_bytes());
    }
    assert_eq!(fs::read(destination.join(raw_child)).unwrap(), b"raw payload");
}

#[test]
fn hardlink_groups_span_operands_in_separate_source_directories() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.cwd.join("left")).unwrap();
    fs::create_dir(fixture.cwd.join("right")).unwrap();
    fs::create_dir(fixture.cwd.join("out")).unwrap();
    fs::write(fixture.cwd.join("left/one"), b"shared payload").unwrap();
    fs::hard_link(fixture.cwd.join("left/one"), fixture.cwd.join("right/two")).unwrap();
    let request = fixture.request(
        Operands::Into {
            directory: "out".into(),
            first: "left/one".into(),
            rest: vec!["right/two".into()],
        },
        Options::default(),
    );

    assert_eq!(fixture.run(&request).status(), 0);
    assert_eq!(fs::read(fixture.cwd.join("out/one")).unwrap(), b"shared payload");
    assert_eq!(fs::read(fixture.cwd.join("out/two")).unwrap(), b"shared payload");
    assert_eq!(fs::metadata(fixture.cwd.join("out/one")).unwrap().ino(), fs::metadata(fixture.cwd.join("out/two")).unwrap().ino());
    assert_eq!(fs::metadata(fixture.cwd.join("out/one")).unwrap().nlink(), 2);
}

#[test]
fn zero_length_and_partial_buffer_payloads_keep_exact_bytes() {
    let fixture = Fixture::new();
    fs::write(fixture.cwd.join("empty"), []).unwrap();
    let payload = (0..256 * 1024 + 37).map(|index| ((index * 37 + 11) % 251) as u8).collect::<Vec<_>>();
    fs::write(fixture.cwd.join("partial"), &payload).unwrap();

    let empty = fixture.request(
        Operands::Exact { source: "empty".into(), destination: "empty-copy".into() },
        Options::default(),
    );
    assert_eq!(fixture.run(&empty).status(), 0);
    assert_eq!(fs::metadata(fixture.cwd.join("empty-copy")).unwrap().len(), 0);
    let partial = fixture.request(
        Operands::Exact { source: "partial".into(), destination: "partial-copy".into() },
        Options::default(),
    );
    assert_eq!(fixture.run(&partial).status(), 0);
    assert_eq!(fs::read(fixture.cwd.join("partial-copy")).unwrap(), payload);
}

#[cfg(target_os = "linux")]
#[test]
fn public_copy_preserves_file_metadata_and_caller_ownership() {
    let fixture = Fixture::new();
    let source = fixture.cwd.join("source");
    let destination = fixture.cwd.join("destination");
    fs::write(&source, b"metadata payload").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
    let acl = access_acl(6, 4, 4, 4, 0);
    set_xattr(&source, b"user.ish-fs-preservation", b"source value");
    let source_file = OpenOptions::new().read(true).write(true).open(&source).unwrap();
    rustix::fs::fsetxattr(&source_file, OsStr::from_bytes(b"system.posix_acl_access"), &acl, rustix::fs::XattrFlags::empty()).unwrap();
    set_times(&source, 1_700_000_100, 123_000_000);
    let source_metadata = fs::metadata(&source).unwrap();

    let request = fixture.request(
        Operands::Exact { source: "source".into(), destination: "destination".into() },
        Options::default(),
    );
    assert_eq!(fixture.run(&request).status(), 0);
    let copied = fs::metadata(&destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"metadata payload");
    assert_eq!(copied.uid(), source_metadata.uid());
    assert_eq!(copied.gid(), source_metadata.gid());
    assert_eq!(copied.permissions().mode() & 0o7777, source_metadata.permissions().mode() & 0o7777);
    assert_eq!((copied.mtime(), copied.mtime_nsec()), (1_700_000_100, 123_000_000));
    assert_eq!(get_xattr(&destination, b"user.ish-fs-preservation"), b"source value");
    assert_eq!(get_xattr(&destination, b"system.posix_acl_access"), acl);
}

#[cfg(target_os = "linux")]
#[test]
fn merge_retains_existing_directory_metadata_and_finalizes_new_directory_metadata() {
    let fixture = Fixture::new();
    let source = fixture.cwd.join("source");
    let destination = fixture.cwd.join("destination");
    fs::create_dir_all(source.join("new/nested")).unwrap();
    fs::write(source.join("new/nested/file"), b"new subtree").unwrap();
    fs::create_dir(&destination).unwrap();
    let source_acl = access_acl(7, 4, 5, 5, 0);
    let destination_acl = access_acl(7, 1, 0, 1, 1);
    set_xattr(&source, b"user.ish-fs-container", b"source container");
    let source_fd = File::open(&source).unwrap();
    rustix::fs::fsetxattr(&source_fd, OsStr::from_bytes(b"system.posix_acl_access"), &source_acl, rustix::fs::XattrFlags::empty()).unwrap();
    set_xattr(&destination, b"user.ish-fs-container", b"destination container");
    let destination_fd = File::open(&destination).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o711)).unwrap();
    rustix::fs::fsetxattr(&destination_fd, OsStr::from_bytes(b"system.posix_acl_access"), &destination_acl, rustix::fs::XattrFlags::empty()).unwrap();
    fs::set_permissions(source.join("new"), fs::Permissions::from_mode(0o710)).unwrap();
    set_xattr(&source.join("new"), b"user.ish-fs-new-directory", b"new directory");
    set_times(&source.join("new"), 1_700_000_200, 456_000_000);
    set_times(&source, 1_700_000_300, 789_000_000);
    set_times(&destination, 1_700_000_400, 987_000_000);

    let request = fixture.request(
        Operands::Exact { source: "source".into(), destination: "destination".into() },
        Options { merge: true, ..Options::default() },
    );
    assert_eq!(fixture.run(&request).status(), 0);
    let existing = fs::metadata(&destination).unwrap();
    assert_eq!(existing.permissions().mode() & 0o7777, 0o711);
    assert_eq!(get_xattr(&destination, b"user.ish-fs-container"), b"destination container");
    assert_eq!(get_xattr(&destination, b"system.posix_acl_access"), destination_acl);
    let created = destination.join("new");
    let created_metadata = fs::metadata(&created).unwrap();
    assert_eq!(created_metadata.permissions().mode() & 0o7777, 0o710);
    assert_eq!((created_metadata.mtime(), created_metadata.mtime_nsec()), (1_700_000_200, 456_000_000));
    assert_eq!(get_xattr(&created, b"user.ish-fs-new-directory"), b"new directory");
    assert_eq!(fs::read(created.join("nested/file")).unwrap(), b"new subtree");
}

#[cfg(target_os = "linux")]
#[test]
fn copy_normalizes_privilege_bits_for_a_supported_supplementary_group() {
    let fixture = Fixture::new();
    let parent_gid = fs::metadata(&fixture.cwd).unwrap().gid();
    let source_group = rustix::process::getgroups()
        .unwrap()
        .into_iter()
        .map(|group| group.as_raw())
        .find(|group| *group != parent_gid);
    let Some(source_group) = source_group else {
        eprintln!("privilege normalization fixture unavailable: process has no supplementary group distinct from its working directory");
        return;
    };
    let source = fixture.cwd.join("source");
    fs::create_dir(&source).unwrap();
    std::os::unix::fs::chown(&source, None, Some(source_group)).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o2755)).unwrap();
    fs::write(source.join("file"), b"grouped source").unwrap();
    fs::set_permissions(source.join("file"), fs::Permissions::from_mode(0o6750)).unwrap();
    let source_directory_metadata = fs::metadata(&source).unwrap();
    let source_file_metadata = fs::metadata(source.join("file")).unwrap();
    assert_ne!(source_directory_metadata.gid(), parent_gid);
    assert_eq!(source_directory_metadata.permissions().mode() & 0o7777, 0o2755);
    assert_eq!(source_file_metadata.gid(), source_group);
    assert_eq!(source_file_metadata.permissions().mode() & 0o7777, 0o6750);

    let request = fixture.request(
        Operands::Exact { source: "source".into(), destination: "destination".into() },
        Options::default(),
    );
    assert_eq!(fixture.run(&request).status(), 0);
    let source_metadata = fs::metadata(source.join("file")).unwrap();
    let copied_metadata = fs::metadata(fixture.cwd.join("destination/file")).unwrap();
    assert_eq!(copied_metadata.uid(), source_metadata.uid());
    assert_eq!(copied_metadata.uid(), fs::metadata(&fixture.cwd).unwrap().uid());
    assert_eq!(copied_metadata.gid(), parent_gid);
    assert_eq!(copied_metadata.permissions().mode() & 0o7777, 0o0750);
    assert_eq!(fs::metadata(fixture.cwd.join("destination")).unwrap().permissions().mode() & 0o7777, 0o0755);
}

#[cfg(unix)]
#[test]
fn unsupported_fifo_fails_without_publishing_a_partial_tree_with_readonly_files() {
    let fixture = Fixture::new();
    let source = fixture.cwd.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("readonly"), b"read-only bytes").unwrap();
    fs::set_permissions(source.join("readonly"), fs::Permissions::from_mode(0o444)).unwrap();
    support::make_fifo(&source.join("pipe")).unwrap();
    let request = fixture.request(
        Operands::Exact { source: "source".into(), destination: "destination".into() },
        Options { jobs: Some(Jobs::new(1).unwrap()), ..Options::default() },
    );

    assert_eq!(fixture.run(&request).status(), 1);
    assert!(!fixture.cwd.join("destination").exists());
    assert_eq!(fs::read(source.join("readonly")).unwrap(), b"read-only bytes");
    assert!(fs::symlink_metadata(source.join("pipe")).unwrap().file_type().is_fifo());
    assert_eq!(fs::read_dir(&fixture.cwd).unwrap().count(), 1);
}
