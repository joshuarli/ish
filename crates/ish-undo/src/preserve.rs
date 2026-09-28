//! Preserving versions, restoring them, and checking recorded evidence.
//!
//! Preservation strength is explicit:
//!
//! - **clone**: `fclonefileat`/`FICLONE` into a store on the same filesystem;
//!   frozen, and no data moves.
//! - **linked**: a hard link that retains the inode when a name is about to
//!   be unlinked. Other links or open writers can still change it, so it is
//!   only offered for unlink/replacement and is labeled as weaker.
//! - **copy**: an independent byte copy, bounded by the byte-copy limit and
//!   the free-space reserve.
//!
//! Capability errors (no clone support, cross-device) select the next
//! strategy; permission, I/O, and space errors fail the preservation.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use crate::journal::{Content, Evidence, Ident, Meta, ObjRef, Saved, Strength};
use crate::store::{StoreRef, Stores, next_object_name};
use crate::sys::{self, CopyStats, Kind, Stat};

/// Why a version is being preserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// The name is about to be unlinked or replaced; a retained inode is an
    /// acceptable (weaker) fallback.
    Unlink,
    /// The object will be written in place or must stay frozen; only a
    /// clone or an independent copy will do.
    Frozen,
}

/// Limits and accounting for one operation.
pub struct Budget<'a> {
    /// Remaining bytes the byte-copy fallback may copy.
    pub copy_remaining: u64,
    /// Free space that must remain on a store after a copy.
    pub min_free: u64,
    pub cancel: &'a AtomicBool,
    pub stats: CopyStats,
    /// Fidelity notices to attach to the transaction.
    pub notes: Vec<String>,
    /// Whether the in-transaction copy/clone counts changed.
    pub clones: u64,
    pub links: u64,
    pub copies: u64,
}

impl<'a> Budget<'a> {
    pub fn new(copy_limit: u64, min_free: u64, cancel: &'a AtomicBool) -> Budget<'a> {
        Budget {
            copy_remaining: copy_limit,
            min_free,
            cancel,
            stats: CopyStats::default(),
            notes: Vec::new(),
            clones: 0,
            links: 0,
            copies: 0,
        }
    }
}

/// Filesystems (by device) where cloning failed with a capability error in
/// this process, so later preservations skip straight to the fallback.
static NO_CLONE: Mutex<Vec<u64>> = Mutex::new(Vec::new());

fn clone_known_unsupported(dev: u64) -> bool {
    NO_CLONE.lock().unwrap().contains(&dev)
}

fn mark_clone_unsupported(dev: u64) {
    let mut v = NO_CLONE.lock().unwrap();
    if !v.contains(&dev) {
        v.push(dev);
    }
}

fn describe(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn err(kind: io::ErrorKind, msg: String) -> io::Error {
    io::Error::new(kind, msg)
}

/// Create a fresh object name in `dir`, retrying on the rare collision.
fn with_new_name<T>(
    dir: BorrowedFd<'_>,
    mut f: impl FnMut(BorrowedFd<'_>, &CStr) -> io::Result<T>,
) -> io::Result<(CString, T)> {
    for _ in 0..8 {
        let name = next_object_name();
        match f(dir, &name) {
            Ok(v) => return Ok((name, v)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(err(
        io::ErrorKind::AlreadyExists,
        "could not allocate an object name".into(),
    ))
}

fn obj_ref(store: &StoreRef, name: &CStr) -> ObjRef {
    let mut rel = b"obj/".to_vec();
    rel.extend_from_slice(name.to_bytes());
    ObjRef {
        store: store.id,
        name: rel,
    }
}

fn owner_note(st: &Stat, path: &[u8]) -> Option<String> {
    let euid = rustix::process::geteuid().as_raw();
    (st.uid != euid).then(|| {
        format!(
            "{}: owned by uid {}; restored copies will be owned by uid {euid}",
            describe(path),
            st.uid
        )
    })
}

/// Preserve the entry `name` in `dir`, which `st` describes (from lstat).
/// `path` is used for messages only.
pub fn preserve(
    stores: &mut Stores,
    budget: &mut Budget<'_>,
    dir: BorrowedFd<'_>,
    name: &CStr,
    path: &[u8],
    st: &Stat,
    need: Need,
) -> io::Result<Saved> {
    let meta = Meta::of(st);
    let ident = Ident::of(st);
    match st.kind {
        Kind::Symlink => {
            let target = sys::read_link_at(dir, name)?;
            Ok(Saved {
                content: Content::Symlink { target },
                ident,
                meta,
                nlink: st.nlink,
                copied: 0,
            })
        }
        Kind::File => preserve_file(stores, budget, dir, name, path, st, need, meta, ident),
        Kind::Dir => Err(err(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: directories are recorded, not preserved",
                describe(path)
            ),
        )),
        _ => {
            // FIFOs, sockets, and devices have no contents to copy; the
            // inode itself can be retained on the same filesystem.
            if need == Need::Unlink
                && let Some(store) = stores.for_dev(st.dev)
            {
                let obj_dir = stores.obj_dir(&store)?;
                let (obj_name, ()) = with_new_name(obj_dir, |d, n| sys::link_at(dir, name, d, n))?;
                budget.links += 1;
                return Ok(Saved {
                    content: Content::Special {
                        obj: obj_ref(&store, &obj_name),
                    },
                    ident,
                    meta,
                    nlink: st.nlink,
                    copied: 0,
                });
            }
            Err(err(
                io::ErrorKind::Unsupported,
                format!(
                    "{}: cannot preserve a {} without a store on its filesystem",
                    describe(path),
                    st.kind.name()
                ),
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn preserve_file(
    stores: &mut Stores,
    budget: &mut Budget<'_>,
    dir: BorrowedFd<'_>,
    name: &CStr,
    path: &[u8],
    st: &Stat,
    need: Need,
    meta: Meta,
    ident: Ident,
) -> io::Result<Saved> {
    let same_fs_store = stores.for_dev(st.dev);
    let file = match sys::open_read_at(dir, name) {
        Ok(fd) => Some(fd),
        // An unreadable file can still be retained by link.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && need == Need::Unlink => None,
        Err(e) => {
            return Err(err(
                e.kind(),
                format!(
                    "{}: cannot open for preservation: {}",
                    describe(path),
                    sys::describe_error(&e)
                ),
            ));
        }
    };
    if let Some(fd) = &file {
        let now = sys::fstat(fd.as_fd())?;
        if !now.same_object(st) || now.kind != Kind::File {
            return Err(err(
                io::ErrorKind::Other,
                format!("{}: changed while being preserved", describe(path)),
            ));
        }
    }
    let saved = |content, copied| Saved {
        content,
        ident,
        meta: meta.clone(),
        nlink: st.nlink,
        copied,
    };

    if let (Some(fd), Some(store)) = (&file, &same_fs_store)
        && !clone_known_unsupported(st.dev)
    {
        let obj_dir = stores.obj_dir(store)?;
        match with_new_name(obj_dir, |d, n| sys::clone_file(fd.as_fd(), d, n)) {
            Ok((obj_name, ())) => {
                budget.clones += 1;
                if let Some(note) = owner_note(st, path) {
                    budget.notes.push(note);
                }
                // Clones do not reliably carry ACLs (APFS) or extended
                // attributes (FICLONE copies data only); copy them onto the
                // stored version explicitly.
                let obj = open_for_meta(stores.obj_dir(store)?, &obj_name)?;
                // The clone inherits the source's mode; Linux refuses xattr
                // writes to a read-only file even for its owner. The stored
                // version's own mode is irrelevant (the journal records it),
                // and this is a private clone, never a linked original.
                sys::set_mode(obj.as_fd(), 0o600)?;
                if let Some(note) = sys::copy_file_metadata(fd.as_fd(), obj.as_fd()) {
                    budget.notes.push(format!("{}: {note}", describe(path)));
                }
                return Ok(saved(
                    Content::File {
                        obj: obj_ref(store, &obj_name),
                        strength: Strength::Clone,
                    },
                    0,
                ));
            }
            Err(e) if sys::is_capability_error(&e) => mark_clone_unsupported(st.dev),
            Err(e) => {
                return Err(err(
                    e.kind(),
                    format!(
                        "{}: clone failed: {}",
                        describe(path),
                        sys::describe_error(&e)
                    ),
                ));
            }
        }
    }

    if need == Need::Unlink
        && let Some(store) = &same_fs_store
    {
        let obj_dir = stores.obj_dir(store)?;
        let (obj_name, ()) =
            with_new_name(obj_dir, |d, n| sys::link_at(dir, name, d, n)).map_err(|e| {
                err(
                    e.kind(),
                    format!(
                        "{}: link failed: {}",
                        describe(path),
                        sys::describe_error(&e)
                    ),
                )
            })?;
        // Verify the retained inode is the one we examined.
        let linked = sys::lstat_at(stores.obj_dir(store)?, &obj_name)?;
        if !linked.same_object(st) {
            let _ = sys::unlink_at(stores.obj_dir(store)?, &obj_name);
            return Err(err(
                io::ErrorKind::Other,
                format!("{}: changed while being preserved", describe(path)),
            ));
        }
        budget.links += 1;
        return Ok(saved(
            Content::File {
                obj: obj_ref(store, &obj_name),
                strength: Strength::Link,
            },
            0,
        ));
    }

    let Some(fd) = file else {
        return Err(err(
            io::ErrorKind::PermissionDenied,
            format!("{}: not readable, so it cannot be copied", describe(path)),
        ));
    };
    copy_into_store(stores, budget, fd.as_fd(), path, st, same_fs_store).map(|(obj, copied)| {
        saved(
            Content::File {
                obj,
                strength: Strength::Copy,
            },
            copied,
        )
    })
}

/// Make an independent frozen copy of an open regular file.
fn copy_into_store(
    stores: &mut Stores,
    budget: &mut Budget<'_>,
    src: BorrowedFd<'_>,
    path: &[u8],
    st: &Stat,
    preferred: Option<StoreRef>,
) -> io::Result<(ObjRef, u64)> {
    if st.size > budget.copy_remaining {
        return Err(err(
            io::ErrorKind::FileTooLarge,
            format!(
                "{}: {} would need a byte copy of {}, over the remaining copy limit of {} (clone unavailable)",
                describe(path),
                st.kind.name(),
                crate::human_bytes(st.size),
                crate::human_bytes(budget.copy_remaining)
            ),
        ));
    }
    let store = preferred.unwrap_or_else(|| stores.home_ref());
    let obj_dir = stores.obj_dir(&store)?;
    let free = sys::free_bytes(obj_dir)?;
    if free < st.size.saturating_add(budget.min_free) {
        return Err(err(
            io::ErrorKind::StorageFull,
            format!(
                "{}: copying {} would leave less than the {} free-space reserve",
                describe(path),
                crate::human_bytes(st.size),
                crate::human_bytes(budget.min_free)
            ),
        ));
    }
    let (obj_name, dst) = with_new_name(obj_dir, |d, n| sys::create_excl_at(d, n, 0o600))?;
    let before = budget.stats.bytes;
    let result = (|| {
        sys::copy_data(src, dst.as_fd(), st.size, budget.cancel, &mut budget.stats)?;
        // A byte copy is not atomic: a write during it would leave a mix of
        // old and new contents, which is not a version of the file at all.
        if !sys::fstat(src)?.unchanged_since(st) {
            return Err(io::Error::other("changed while being copied"));
        }
        if let Some(note) = sys::copy_file_metadata(src, dst.as_fd()) {
            budget.notes.push(format!("{}: {note}", describe(path)));
        }
        sys::sync_file(dst.as_fd())
    })();
    if let Err(e) = result {
        drop(dst);
        let _ = sys::unlink_at(stores.obj_dir(&store)?, &obj_name);
        return Err(err(
            e.kind(),
            format!(
                "{}: copy failed: {}",
                describe(path),
                sys::describe_error(&e)
            ),
        ));
    }
    let copied = budget.stats.bytes - before;
    budget.copy_remaining = budget.copy_remaining.saturating_sub(st.size);
    budget.copies += 1;
    Ok((obj_ref(&store, &obj_name), copied))
}

/// Frozen post-image of a file for later content comparison. Only clones are
/// taken: without cloning, stat evidence is recorded instead of copying.
pub fn capture_evidence(
    stores: &mut Stores,
    budget: &mut Budget<'_>,
    dir: BorrowedFd<'_>,
    name: &CStr,
) -> io::Result<Evidence> {
    let st = match sys::lstat_at(dir, name) {
        Ok(st) => st,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Evidence::Absent),
        Err(e) => return Err(e),
    };
    Ok(match st.kind {
        Kind::Dir => Evidence::Dir {
            dev: st.dev,
            ino: st.ino,
        },
        Kind::Symlink => Evidence::Symlink {
            target: sys::read_link_at(dir, name)?,
        },
        Kind::File => {
            let ident = Ident::of(&st);
            let Some(store) = stores.for_dev(st.dev) else {
                return Ok(Evidence::Stat(ident));
            };
            if clone_known_unsupported(st.dev) {
                return Ok(Evidence::Stat(ident));
            }
            let Ok(fd) = sys::open_read_at(dir, name) else {
                return Ok(Evidence::Stat(ident));
            };
            let obj_dir = stores.obj_dir(&store)?;
            match with_new_name(obj_dir, |d, n| sys::clone_file(fd.as_fd(), d, n)) {
                Ok((obj_name, ())) => {
                    budget.clones += 1;
                    // The clone reflects the file at clone time; record the
                    // stat taken after it so a write in between is caught.
                    let after = sys::fstat(fd.as_fd())?;
                    if after.unchanged_since(&st) {
                        Evidence::Frozen {
                            ident,
                            obj: obj_ref(&store, &obj_name),
                        }
                    } else {
                        let _ = sys::unlink_at(stores.obj_dir(&store)?, &obj_name);
                        Evidence::Stat(Ident::of(&after))
                    }
                }
                Err(e) if sys::is_capability_error(&e) => {
                    mark_clone_unsupported(st.dev);
                    Evidence::Stat(ident)
                }
                Err(_) => Evidence::Stat(ident),
            }
        }
        _ => Evidence::Stat(Ident::of(&st)),
    })
}

/// Result of comparing a path with recorded evidence.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Match,
    Mismatch(String),
}

/// Compare the entry `name` in `dir` with `evidence`. Content comparison
/// against a frozen object happens here, deferred from capture time.
pub fn check_evidence(
    stores: &mut Stores,
    cancel: &AtomicBool,
    dir: BorrowedFd<'_>,
    name: &CStr,
    evidence: &Evidence,
) -> io::Result<Check> {
    let st = match sys::lstat_at(dir, name) {
        Ok(st) => Some(st),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mismatch = |why: &str| Ok(Check::Mismatch(why.to_owned()));
    match (evidence, st) {
        (Evidence::Absent, None) => Ok(Check::Match),
        (Evidence::Absent, Some(_)) => mismatch("something now exists there"),
        (_, None) => mismatch("it no longer exists"),
        (Evidence::Unknown, Some(_)) => mismatch("no reliable record of its expected state"),
        (Evidence::Stat(ident), Some(st)) => {
            if ident.unchanged(&st) {
                Ok(Check::Match)
            } else if !ident.same_object(&st) {
                mismatch("it was replaced by a different object")
            } else {
                mismatch("it was modified afterward")
            }
        }
        (Evidence::Frozen { ident, obj }, Some(st)) => {
            if st.kind != Kind::File {
                return mismatch("it is no longer a regular file");
            }
            if ident.unchanged(&st) {
                return Ok(Check::Match);
            }
            if st.size != ident.size {
                return mismatch("its contents changed afterward");
            }
            let expected = match stores.open_object(obj) {
                Ok(fd) => fd,
                Err(_) => return mismatch("the recorded contents are unavailable"),
            };
            let current = sys::open_read_at(dir, name)?;
            if sys::same_contents(current.as_fd(), expected.as_fd(), cancel)? {
                Ok(Check::Match)
            } else {
                mismatch("its contents changed afterward")
            }
        }
        (Evidence::Symlink { target }, Some(st)) => {
            if st.kind == Kind::Symlink && sys::read_link_at(dir, name)? == *target {
                Ok(Check::Match)
            } else {
                mismatch("the symlink changed afterward")
            }
        }
        (Evidence::Dir { dev, ino }, Some(st)) => {
            if st.kind == Kind::Dir && st.dev == *dev && st.ino == *ino {
                Ok(Check::Match)
            } else {
                mismatch("it is not the directory that was recorded")
            }
        }
        (Evidence::Object { dev, ino, kind }, Some(st)) => {
            if st.kind == *kind && st.dev == *dev && st.ino == *ino {
                Ok(Check::Match)
            } else {
                mismatch("it is not the object that was recorded")
            }
        }
    }
}

/// Restore `saved` as a new entry named `tmp` in `dir`; the caller then
/// publishes it with a no-replace or replacing rename.
///
/// Creation is exclusive in every branch. An entry that already has the name
/// is somebody else's: it is never touched and the error is `AlreadyExists`,
/// so the caller can pick another name. Once created the entry is ours.
/// `created` is called with its identity before any data goes into it, so the
/// journal can name it, and the entry is removed again if that call or any
/// later step fails. Frozen versions are restored through a new clone or
/// copy, never by exposing the stored object itself.
pub fn materialize(
    stores: &mut Stores,
    budget: &mut Budget<'_>,
    saved: &Saved,
    dir: BorrowedFd<'_>,
    tmp: &CStr,
    created: &mut dyn FnMut(&Stat) -> io::Result<()>,
) -> io::Result<()> {
    enum Made {
        /// Complete on creation: a symlink or a relinked retained inode.
        Whole,
        /// A regular file to fill from `src`. A clone already has the data.
        File {
            src: OwnedFd,
            size: u64,
            fd: OwnedFd,
            cloned: bool,
        },
    }
    let made = match &saved.content {
        Content::Symlink { target } => {
            sys::symlink_at(target, dir, tmp)?;
            let _ = sys::set_symlink_times(dir, tmp, saved.meta.atime, saved.meta.mtime);
            Made::Whole
        }
        Content::Special { obj }
        | Content::File {
            obj,
            strength: Strength::Link,
        } => {
            // A retained inode is restored by linking it back: the same
            // object, still subject to the weaker linked-version contract.
            let (obj_dir, obj_name) = stores.object_entry(obj)?;
            sys::link_at(obj_dir.as_fd(), &obj_name, dir, tmp)?;
            Made::Whole
        }
        Content::File { obj, .. } => {
            let src = stores.open_object(obj)?;
            let src_st = sys::fstat(src.as_fd())?;
            let size = src_st.size;
            let dir_dev = sys::fstat(dir)?.dev;
            let cloned = src_st.dev == dir_dev
                && !clone_known_unsupported(dir_dev)
                && match sys::clone_file(src.as_fd(), dir, tmp) {
                    Ok(()) => true,
                    Err(e) if sys::is_capability_error(&e) => {
                        mark_clone_unsupported(dir_dev);
                        false
                    }
                    Err(e) => return Err(e),
                };
            let fd = if cloned {
                match open_for_meta(dir, tmp) {
                    Ok(fd) => fd,
                    Err(e) => {
                        let _ = sys::unlink_at(dir, tmp);
                        return Err(e);
                    }
                }
            } else {
                sys::create_excl_at(dir, tmp, 0o600)?
            };
            Made::File {
                src,
                size,
                fd,
                cloned,
            }
        }
    };
    let result = (|| {
        let st = match &made {
            Made::Whole => sys::lstat_at(dir, tmp)?,
            Made::File { fd, .. } => sys::fstat(fd.as_fd())?,
        };
        created(&st)?;
        if let Made::File {
            src,
            size,
            fd,
            cloned,
        } = &made
        {
            if *cloned {
                budget.clones += 1;
                // Writable while attributes are copied; the recorded mode is
                // applied below.
                sys::set_mode(fd.as_fd(), 0o600)?;
            } else {
                sys::copy_data(
                    src.as_fd(),
                    fd.as_fd(),
                    *size,
                    budget.cancel,
                    &mut budget.stats,
                )?;
                budget.copies += 1;
            }
            if let Some(note) = sys::copy_file_metadata(src.as_fd(), fd.as_fd()) {
                budget.notes.push(note);
            }
            apply_file_meta(fd.as_fd(), &saved.meta, &mut budget.notes);
            sys::sync_file(fd.as_fd())?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = sys::unlink_at(dir, tmp);
    }
    result
}

fn open_for_meta(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(
        dir,
        name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?)
}

/// Apply ownership, mode, flags, and timestamps. Ownership first, because
/// changing it can clear set-id bits; flags last, because immutable flags
/// block later changes.
pub fn apply_file_meta(fd: BorrowedFd<'_>, meta: &Meta, notes: &mut Vec<String>) {
    if let Err(e) = sys::set_owner(fd, meta.uid, meta.gid) {
        notes.push(format!(
            "ownership {}:{} not restored: {e}",
            meta.uid, meta.gid
        ));
    }
    if let Err(e) = sys::set_mode(fd, meta.mode) {
        notes.push(format!("mode {:o} not restored: {e}", meta.mode));
    }
    if let Err(e) = sys::set_times(fd, meta.atime, meta.mtime) {
        notes.push(format!("timestamps not restored: {e}"));
    }
    if meta.flags != 0
        && let Err(e) = sys::set_flags(fd, meta.flags)
    {
        notes.push(format!("file flags {:x} not restored: {e}", meta.flags));
    }
}

/// Record a directory's metadata, including extended attributes up to a
/// bound.
pub fn dir_meta(fd: BorrowedFd<'_>, st: &Stat) -> Meta {
    let mut meta = Meta::of(st);
    match sys::read_xattrs(fd, 256 * 1024) {
        Ok(Some(x)) => meta.xattrs = x,
        Ok(None) | Err(_) => meta.xattrs_complete = false,
    }
    meta
}

/// Apply directory metadata after its children were restored.
pub fn apply_dir_meta(fd: BorrowedFd<'_>, meta: &Meta, notes: &mut Vec<String>) {
    let failed = sys::write_xattrs(fd, &meta.xattrs);
    if !failed.is_empty() {
        notes.push(format!(
            "{} directory extended attributes not restored",
            failed.len()
        ));
    }
    if !meta.xattrs_complete {
        notes.push("directory extended attributes were too large to record".into());
    }
    apply_file_meta(fd, meta, notes);
}
