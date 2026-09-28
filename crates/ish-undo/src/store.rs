//! Recovery storage layout, locks, and volume-local stores.
//!
//! The catalog lives in the home store, `~/.local/share/ish/undo`:
//!
//! ```text
//! format                 "ish-undo-store 1" and the store id
//! lock                   flock for id allocation, GC, and the registry
//! next                   next transaction id
//! volumes                registered volume stores (id and hex path per line)
//! sessions/<id>.lock     held by each running shell that recorded something
//! txn/<id>/journal       transaction journal
//! txn/<id>/obj/<name>    saved versions on the home filesystem
//! txn/<id>/before.cat    scoped checkpoint catalog
//! ```
//!
//! A volume store (`<dir>/.ish-undo`) holds saved versions for another
//! filesystem, because clones and hard links only work within one. Each
//! transaction directory there carries an `owner` file naming the catalog
//! transaction, so associations can be rebuilt without the central catalog.
//! The central journal and a volume store never commit atomically together;
//! objects without a journal reference are orphan candidates, not garbage.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::journal::{self, Begin, Record};
use crate::sys;

pub const FORMAT: &str = "ish-undo-store 1";
pub const VOLUME_FORMAT: &str = "ish-undo-volume 1";
pub const VOLUME_DIR: &str = ".ish-undo";

/// Home store root for a HOME value, following ish's `~/.local/share/ish`.
pub fn root_for_home(home: &OsStr) -> PathBuf {
    Path::new(home).join(".local/share/ish/undo")
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn private_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Verify a store directory is a real directory owned by us and private.
fn validate_private(path: &Path) -> io::Result<sys::Stat> {
    let st = sys::lstat(path)?;
    if st.kind != sys::Kind::Dir {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    if st.uid != rustix::process::geteuid().as_raw() {
        return Err(io::Error::other(format!(
            "{} is owned by another user",
            path.display()
        )));
    }
    if st.mode & 0o077 != 0 {
        // Our own directory: tighten it rather than storing data readable
        // by others.
        let fd = sys::open_dir(path)?;
        sys::set_mode(fd.as_fd(), st.mode & 0o700)?;
    }
    Ok(st)
}

fn read_format(path: &Path, expect: &str) -> io::Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if lines.first().map(String::as_str) != Some(expect) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: unrecognized store format", path.display()),
        ));
    }
    Ok(lines)
}

fn write_new_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents)?;
    sys::sync_file(f.as_fd())
}

fn replace_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    write_new_file(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        sys::sync_dir(sys::open_dir(parent)?.as_fd())?;
    }
    Ok(())
}

/// An exclusive `flock` released on drop.
pub struct Lock {
    fd: OwnedFd,
}

impl Lock {
    pub fn acquire(path: &Path) -> io::Result<Lock> {
        let fd: OwnedFd = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?
            .into();
        rustix::fs::flock(&fd, rustix::fs::FlockOperation::LockExclusive)?;
        Ok(Lock { fd })
    }

    pub fn try_acquire(path: &Path) -> io::Result<Option<Lock>> {
        let fd: OwnedFd = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?
            .into();
        match rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Lock { fd })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.fd, rustix::fs::FlockOperation::Unlock);
    }
}

/// The home store: catalog, locks, registry, and home-filesystem objects.
#[derive(Clone, Debug)]
pub struct Home {
    pub root: PathBuf,
    pub id: u64,
}

impl Home {
    /// Open an existing home store without creating anything.
    pub fn open_existing(root: &Path) -> io::Result<Option<Home>> {
        match read_format(&root.join("format"), FORMAT) {
            Ok(lines) => {
                let id = lines
                    .get(1)
                    .and_then(|s| u64::from_str_radix(s, 16).ok())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "store id missing")
                    })?;
                validate_private(root)?;
                Ok(Some(Home {
                    root: root.to_path_buf(),
                    id,
                }))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Open the home store, creating it on first use.
    pub fn open_or_create(root: &Path) -> io::Result<Home> {
        if let Some(home) = Home::open_existing(root)? {
            return Ok(home);
        }
        if let Some(parent) = root.parent() {
            fs::create_dir_all(parent)?;
        }
        private_dir(root)?;
        validate_private(root)?;
        let _lock = Lock::acquire(&root.join("lock"))?;
        if let Some(home) = Home::open_existing(root)? {
            return Ok(home);
        }
        for sub in ["txn", "sessions"] {
            private_dir(&root.join(sub))?;
        }
        let id = sys::random_u64() | 1;
        write_new_file(
            &root.join("format"),
            format!("{FORMAT}\n{id:016x}\n").as_bytes(),
        )?;
        sys::sync_dir(sys::open_dir(root)?.as_fd())?;
        Ok(Home {
            root: root.to_path_buf(),
            id,
        })
    }

    pub fn lock(&self) -> io::Result<Lock> {
        Lock::acquire(&self.root.join("lock"))
    }

    pub fn txn_dir(&self, id: u64) -> PathBuf {
        self.root.join("txn").join(id.to_string())
    }

    pub fn journal_path(&self, id: u64) -> PathBuf {
        self.txn_dir(id).join("journal")
    }

    pub fn session_lock_path(&self, session: u64) -> PathBuf {
        self.root
            .join("sessions")
            .join(format!("{session:016x}.lock"))
    }

    /// Allocate a transaction id and create its journal with `begin`.
    /// Called with the store lock held.
    pub fn allocate(&self, mut begin: Begin, _lock: &Lock) -> io::Result<u64> {
        let next_path = self.root.join("next");
        let mut id = match fs::read_to_string(&next_path) {
            Ok(s) => s.trim().parse::<u64>().unwrap_or(1),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 1,
            Err(e) => return Err(e),
        };
        // Never reuse an id whose directory still exists.
        while self.txn_dir(id).exists() {
            id += 1;
        }
        replace_file(&next_path, format!("{}\n", id + 1).as_bytes())?;
        let dir = self.txn_dir(id);
        private_dir(&self.root.join("txn"))?;
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        begin.id = id;
        journal::create(&dir.join("journal"), &[Record::Begin(begin)])?;
        sys::sync_dir(sys::open_dir(&self.root.join("txn"))?.as_fd())?;
        Ok(id)
    }

    /// Transaction ids present in the catalog, ascending.
    pub fn txn_ids(&self) -> io::Result<Vec<u64>> {
        let mut ids = Vec::new();
        match fs::read_dir(self.root.join("txn")) {
            Ok(entries) => {
                for entry in entries {
                    if let Some(id) = entry?.file_name().to_str().and_then(|s| s.parse().ok()) {
                        ids.push(id);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        ids.sort_unstable();
        Ok(ids)
    }

    pub fn registry(&self) -> io::Result<Vec<VolumeEntry>> {
        let text = match fs::read_to_string(self.root.join("volumes")) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(text
            .lines()
            .filter_map(|line| {
                let (id, path) = line.split_once('\t')?;
                Some(VolumeEntry {
                    id: u64::from_str_radix(id, 16).ok()?,
                    path: PathBuf::from(OsString::from_vec(unhex(path)?)),
                })
            })
            .collect())
    }

    pub fn write_registry(&self, entries: &[VolumeEntry], _lock: &Lock) -> io::Result<()> {
        let mut text = String::new();
        for e in entries {
            text.push_str(&format!(
                "{:016x}\t{}\n",
                e.id,
                hex(e.path.as_os_str().as_bytes())
            ));
        }
        replace_file(&self.root.join("volumes"), text.as_bytes())
    }
}

/// A registered volume store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeEntry {
    pub id: u64,
    /// The `.ish-undo` directory itself.
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeState {
    Available {
        dev: u64,
    },
    /// The path is missing: the volume is disconnected or unmounted. Its
    /// objects are unavailable, not deleted.
    Missing,
    /// Something is at the path but it is not this store.
    Mismatch(String),
}

impl VolumeEntry {
    pub fn state(&self, home_id: u64) -> VolumeState {
        let lines = match read_format(&self.path.join("format"), VOLUME_FORMAT) {
            Ok(lines) => lines,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return VolumeState::Missing,
            Err(e) => return VolumeState::Mismatch(e.to_string()),
        };
        let id = lines.get(1).and_then(|s| u64::from_str_radix(s, 16).ok());
        let owner = lines.get(2).and_then(|s| u64::from_str_radix(s, 16).ok());
        if id != Some(self.id) || owner != Some(home_id) {
            return VolumeState::Mismatch("store identity does not match the registry".into());
        }
        match validate_private(&self.path) {
            Ok(st) => VolumeState::Available { dev: st.dev },
            Err(e) => VolumeState::Mismatch(e.to_string()),
        }
    }
}

/// Create a volume store in `dir` for the home store `home`.
pub fn create_volume(home: &Home, dir: &Path) -> io::Result<VolumeEntry> {
    let path = dir.join(VOLUME_DIR);
    private_dir(&path)?;
    validate_private(&path)?;
    let id = sys::random_u64() | 1;
    match write_new_file(
        &path.join("format"),
        format!("{VOLUME_FORMAT}\n{id:016x}\n{:016x}\n", home.id).as_bytes(),
    ) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Reuse a store left by an earlier registration of this home.
            let lines = read_format(&path.join("format"), VOLUME_FORMAT)?;
            let existing = lines.get(1).and_then(|s| u64::from_str_radix(s, 16).ok());
            let owner = lines.get(2).and_then(|s| u64::from_str_radix(s, 16).ok());
            if owner != Some(home.id) {
                return Err(io::Error::other(format!(
                    "{} belongs to another ish undo store",
                    path.display()
                )));
            }
            return Ok(VolumeEntry {
                id: existing.unwrap_or(id),
                path,
            });
        }
        Err(e) => return Err(e),
    }
    private_dir(&path.join("txn"))?;
    sys::sync_dir(sys::open_dir(&path)?.as_fd())?;
    Ok(VolumeEntry { id, path })
}

/// A store usable for saving objects in one transaction.
#[derive(Clone, Debug)]
pub struct StoreRef {
    pub id: u64,
    pub root: PathBuf,
    pub dev: u64,
    pub home: bool,
}

/// Object-name source unique across processes: pid plus a per-process
/// counter. Forked children get a different pid, so inherited counter values
/// cannot collide; an unlikely collision is retried by the O_EXCL creation.
static OBJECT_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn next_object_name() -> CString {
    let n = OBJECT_COUNTER.fetch_add(1, Ordering::Relaxed);
    CString::new(format!("{:x}-{n:x}", std::process::id())).unwrap()
}

/// The stores known to a transaction and its per-store object directories.
pub struct Stores {
    pub home: Home,
    home_dev: u64,
    volumes: Option<Vec<(VolumeEntry, VolumeState)>>,
    /// Open object directories keyed by store id.
    obj_dirs: Vec<(u64, OwnedFd)>,
    pub txn: u64,
}

impl Stores {
    pub fn new(home: Home, txn: u64) -> io::Result<Stores> {
        let home_dev = sys::lstat(&home.root)?.dev;
        Ok(Stores {
            home,
            home_dev,
            volumes: None,
            obj_dirs: Vec::new(),
            txn,
        })
    }

    fn volumes(&mut self) -> &[(VolumeEntry, VolumeState)] {
        if self.volumes.is_none() {
            let entries = self.home.registry().unwrap_or_default();
            let home_id = self.home.id;
            self.volumes = Some(
                entries
                    .into_iter()
                    .map(|e| {
                        let state = e.state(home_id);
                        (e, state)
                    })
                    .collect(),
            );
        }
        self.volumes.as_deref().unwrap()
    }

    pub fn home_ref(&self) -> StoreRef {
        StoreRef {
            id: self.home.id,
            root: self.home.root.clone(),
            dev: self.home_dev,
            home: true,
        }
    }

    /// The store on filesystem `dev`, if one is registered and available.
    pub fn for_dev(&mut self, dev: u64) -> Option<StoreRef> {
        if dev == self.home_dev {
            return Some(self.home_ref());
        }
        self.volumes().iter().find_map(|(e, state)| match state {
            VolumeState::Available { dev: d } if *d == dev => Some(StoreRef {
                id: e.id,
                root: e.path.clone(),
                dev,
                home: false,
            }),
            _ => None,
        })
    }

    /// Resolve a store id to its root, or `None` when unavailable.
    pub fn by_id(&mut self, id: u64) -> Option<StoreRef> {
        if id == self.home.id {
            return Some(self.home_ref());
        }
        self.volumes().iter().find_map(|(e, state)| match state {
            VolumeState::Available { dev } if e.id == id => Some(StoreRef {
                id,
                root: e.path.clone(),
                dev: *dev,
                home: false,
            }),
            _ => None,
        })
    }

    pub fn txn_dir_in(&self, store: &StoreRef) -> PathBuf {
        store.root.join("txn").join(self.txn.to_string())
    }

    /// Object directory for this transaction in `store`, created on demand.
    pub fn obj_dir(&mut self, store: &StoreRef) -> io::Result<BorrowedFd<'_>> {
        if let Some(i) = self.obj_dirs.iter().position(|(id, _)| *id == store.id) {
            return Ok(self.obj_dirs[i].1.as_fd());
        }
        let txn_dir = self.txn_dir_in(store);
        if !store.home {
            private_dir(&store.root.join("txn"))?;
            private_dir(&txn_dir)?;
            let owner = txn_dir.join("owner");
            if !owner.exists() {
                // Enough to reassociate these objects with the catalog.
                let _ = write_new_file(
                    &owner,
                    format!("{}\t{:016x}\n", self.txn, self.home.id).as_bytes(),
                );
            }
        }
        let obj = txn_dir.join("obj");
        private_dir(&obj)?;
        let fd = sys::open_dir(&obj)?;
        self.obj_dirs.push((store.id, fd));
        Ok(self.obj_dirs.last().unwrap().1.as_fd())
    }

    /// Flush every object directory touched so far.
    pub fn sync_objects(&self) -> io::Result<()> {
        for (_, fd) in &self.obj_dirs {
            sys::sync_dir(fd.as_fd())?;
        }
        Ok(())
    }

    pub fn object_path(&mut self, obj: &journal::ObjRef) -> Option<PathBuf> {
        let store = self.by_id(obj.store)?;
        Some(self.txn_dir_in(&store).join(OsStr::from_bytes(&obj.name)))
    }

    /// Open an object for reading.
    pub fn open_object(&mut self, obj: &journal::ObjRef) -> io::Result<OwnedFd> {
        let path = self.object_path(obj).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the store holding this version is unavailable",
            )
        })?;
        Ok(rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?)
    }

    /// Directory fd and entry name of an object, for linking it back.
    pub fn object_entry(&mut self, obj: &journal::ObjRef) -> io::Result<(OwnedFd, CString)> {
        let path = self.object_path(obj).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the store holding this version is unavailable",
            )
        })?;
        let (parent, name) = sys::split_parent(&path)?;
        Ok((sys::open_dir(parent)?, sys::os_cstr(name)?))
    }

    pub fn volume_list(&mut self) -> Vec<(VolumeEntry, VolumeState)> {
        self.volumes().to_vec()
    }

    pub fn store_ids(&mut self) -> Vec<u64> {
        let mut ids = vec![self.home.id];
        ids.extend(self.volumes().iter().map(|(e, _)| e.id));
        ids
    }
}

/// Liveness of the shell session that owns an unsealed transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
}

/// A shell holds its session lock for its lifetime once it records anything.
/// If the lock can be taken, the shell is gone. A missing lock file means the
/// shell has not reached the point of locking yet; its pid decides.
pub fn session_liveness(home: &Home, session: u64, shell_pid: u32) -> Liveness {
    let path = home.session_lock_path(session);
    if path.exists() {
        return match Lock::try_acquire(&path) {
            Ok(Some(_lock)) => Liveness::Dead,
            Ok(None) => Liveness::Alive,
            Err(_) => Liveness::Alive,
        };
    }
    match rustix::process::Pid::from_raw(shell_pid as i32) {
        Some(pid) if rustix::process::test_kill_process(pid).is_ok() => Liveness::Alive,
        _ => Liveness::Dead,
    }
}

/// Hold the session lock for the life of the process that calls this. The
/// descriptor is close-on-exec so external commands never inherit it.
pub fn hold_session_lock(home: &Home, session: u64) -> io::Result<OwnedFd> {
    let path = home.session_lock_path(session);
    private_dir(&home.root.join("sessions"))?;
    let fd: OwnedFd = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?
        .into();
    rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
    Ok(fd)
}

pub fn cstr_name(name: &CStr) -> &OsStr {
    OsStr::from_bytes(name.to_bytes())
}
