//! Protected native `rm`, `mv`, and redirection opens.
//!
//! Every destructive step follows one ordering: preserve the affected
//! version, make it and a prepare record durable, then mutate, then record
//! the outcome. A failure to preserve refuses the mutation.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crate::journal::{Action, Ident, Meta, Saved};
use crate::preserve::{self, Budget, Need};
use crate::sys::{self, Kind, Stat};
use crate::txn::{Recorder, Txn};
use crate::{Io, store};

/// Entries preserved and recorded per durable batch during recursive
/// removal. One journal sync and one object-directory sync cover a batch.
const BATCH: usize = 256;
/// Directory descriptors kept open during a walk; deeper ancestors are
/// reopened by name and verified by identity.
const MAX_OPEN_DIRS: usize = 48;
/// Deepest tree a recursive walk descends into.
const MAX_DEPTH: usize = 4096;

fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn path_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

fn join(base: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = base.to_vec();
    if !p.ends_with(b"/") {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

/// Whether an operand's last component, ignoring trailing slashes, is `.`
/// or `..`. Checked on the raw operand because lexical resolution would turn
/// `dir/.` into `dir`.
pub fn names_dot_or_dotdot(operand: &[u8]) -> bool {
    let trimmed = match operand.iter().rposition(|&b| b != b'/') {
        Some(end) => &operand[..=end],
        None => return false,
    };
    let last = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    };
    last == b"." || last == b".."
}

/// Resolve an operand against the command's working directory, lexically.
pub fn resolve(cwd: &Path, arg: &OsStr) -> PathBuf {
    let p = Path::new(arg);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    sys::clean_path(&joined)
}

fn errno_msg(e: &io::Error) -> String {
    sys::describe_error(e)
}

/// Why a path may not be removed or replaced.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    DotOrDotDot,
    Root,
    /// The recovery store, one of its ancestors, or something inside it.
    Store(PathBuf),
}

impl Refusal {
    pub fn message(&self, cmd: &str, path: &[u8]) -> String {
        match self {
            Refusal::DotOrDotDot => {
                format!("{cmd}: refusing to remove '.' or '..': {}", show(path))
            }
            Refusal::Root => format!(
                "{cmd}: refusing to operate on the filesystem root: {}",
                show(path)
            ),
            Refusal::Store(store) => format!(
                "{cmd}: refusing to modify {}: it would destroy the undo store at {}",
                show(path),
                store.display()
            ),
        }
    }
}

/// Identities that native operations must never remove or replace: the
/// root, and each recovery store with all of its ancestors. Built without
/// touching the targets, so guards can be tested without executing
/// anything destructive.
pub struct Guard {
    root: (u64, u64),
    stores: Vec<GuardedStore>,
}

/// A store root with its identity and the identities of its ancestors.
struct GuardedStore {
    root: PathBuf,
    ident: (u64, u64),
    chain: Vec<(u64, u64)>,
}

impl Guard {
    pub fn new(store_roots: &[PathBuf]) -> Guard {
        let root = sys::stat(Path::new("/"))
            .map(|s| (s.dev, s.ino))
            .unwrap_or((0, 0));
        let mut stores = Vec::new();
        for store_root in store_roots {
            let Ok(st) = sys::stat(store_root) else {
                continue;
            };
            let mut chain = Vec::new();
            let mut p = store_root.clone();
            while let Some(parent) = p.parent().map(Path::to_path_buf) {
                if let Ok(s) = sys::stat(&p) {
                    chain.push((s.dev, s.ino));
                }
                if parent == p {
                    break;
                }
                p = parent;
            }
            stores.push(GuardedStore {
                root: store_root.clone(),
                ident: (st.dev, st.ino),
                chain,
            });
        }
        Guard { root, stores }
    }

    pub fn for_txn(txn_home: &Path) -> Guard {
        let mut roots = vec![txn_home.to_path_buf()];
        if let Ok(Some(home)) = store::Home::open_existing(txn_home) {
            roots.extend(
                home.registry()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.path),
            );
        }
        Guard::new(&roots)
    }

    /// Check an operand. `name` is its final component, `st` its lstat, and
    /// `parent` an open descriptor of its parent directory, used to walk up
    /// through `..` to detect targets inside a store.
    pub fn check(
        &self,
        name: &OsStr,
        st: &Stat,
        parent: Option<BorrowedFd<'_>>,
    ) -> Result<(), Refusal> {
        if name == "." || name == ".." {
            return Err(Refusal::DotOrDotDot);
        }
        let ident = (st.dev, st.ino);
        if ident == self.root {
            return Err(Refusal::Root);
        }
        for store in &self.stores {
            if store.chain.contains(&ident) {
                return Err(Refusal::Store(store.root.clone()));
            }
            if let Some(parent) = parent
                && inside(parent, store.ident)
            {
                return Err(Refusal::Store(store.root.clone()));
            }
        }
        Ok(())
    }
}

/// Whether the directory `dir` is `target` or lies below it.
fn inside(dir: BorrowedFd<'_>, target: (u64, u64)) -> bool {
    let Ok(mut st) = sys::fstat(dir) else {
        return false;
    };
    let mut current: Option<OwnedFd> = None;
    for _ in 0..MAX_DEPTH {
        if (st.dev, st.ino) == target {
            return true;
        }
        let fd = current.as_ref().map(|f| f.as_fd()).unwrap_or(dir);
        let Ok(up) = sys::open_dir_at(fd, c"..") else {
            return false;
        };
        let Ok(up_st) = sys::fstat(up.as_fd()) else {
            return false;
        };
        if up_st.same_object(&st) {
            return false;
        }
        st = up_st;
        current = Some(up);
    }
    false
}

fn open_parent(path: &Path) -> io::Result<(OwnedFd, CString)> {
    let (parent, name) = sys::split_parent(path)?;
    Ok((sys::open_dir(parent)?, sys::os_cstr(name)?))
}

fn stdin_is_tty() -> bool {
    rustix::termios::isatty(unsafe { BorrowedFd::borrow_raw(0) })
}

fn is_writable(dir: BorrowedFd<'_>, name: &CStr) -> bool {
    rustix::fs::accessat(
        dir,
        name,
        rustix::fs::Access::WRITE_OK,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .is_ok()
}

fn recorder_error(cmd: &str, e: &io::Error, err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "{cmd}: cannot record undo information: {e} (use /bin/{cmd} to proceed without protection)"
    );
}

#[derive(Default)]
struct RmOpts {
    recursive: bool,
    force: bool,
    interactive: bool,
    verbose: bool,
}

pub const RM_USAGE: &str = "usage: rm [-f | -i] [-rRv] [--] file ...";
pub const MV_USAGE: &str = "usage: mv [-f | -i | -n] [-v] [--] source ... target";

/// Protected `rm`. `cwd` is the shell's working directory for the command.
pub fn rm(txn: &Txn, cwd: &Path, args: &[OsString], io: &mut Io<'_>) -> i32 {
    let mut opts = RmOpts::default();
    let mut operands = Vec::new();
    let mut options_done = false;
    for arg in args {
        let bytes = arg.as_bytes();
        if !options_done && bytes == b"--" {
            options_done = true;
        } else if !options_done && bytes.len() > 1 && bytes[0] == b'-' {
            match bytes {
                b"--recursive" => opts.recursive = true,
                b"--force" => {
                    opts.force = true;
                    opts.interactive = false;
                }
                b"--interactive" => {
                    opts.interactive = true;
                    opts.force = false;
                }
                b"--verbose" => opts.verbose = true,
                _ if bytes[1] != b'-' => {
                    for &c in &bytes[1..] {
                        match c {
                            b'r' | b'R' => opts.recursive = true,
                            b'f' => {
                                opts.force = true;
                                opts.interactive = false;
                            }
                            b'i' => {
                                opts.interactive = true;
                                opts.force = false;
                            }
                            b'v' => opts.verbose = true,
                            other => {
                                let _ = writeln!(
                                    io.err,
                                    "rm: unsupported option -{} (native rm is protected; use /bin/rm for other options)\n{RM_USAGE}",
                                    other as char
                                );
                                return 2;
                            }
                        }
                    }
                }
                _ => {
                    let _ = writeln!(
                        io.err,
                        "rm: unsupported option {} (native rm is protected; use /bin/rm for other options)\n{RM_USAGE}",
                        show(bytes)
                    );
                    return 2;
                }
            }
        } else {
            operands.push(arg.clone());
        }
    }
    if operands.is_empty() {
        if opts.force {
            return 0;
        }
        let _ = writeln!(io.err, "{RM_USAGE}");
        return 1;
    }

    let guard = Guard::for_txn(&txn.home_root);
    let mut budget = Budget::new(txn.config.copy_limit, txn.config.min_free, io.cancel);
    let mut status = 0;
    for operand in &operands {
        if io.cancel.load(Ordering::Relaxed) {
            break;
        }
        let path = resolve(cwd, operand);
        let shown = operand.as_bytes();
        if names_dot_or_dotdot(operand.as_bytes()) {
            let _ = writeln!(io.err, "{}", Refusal::DotOrDotDot.message("rm", shown));
            status = 1;
            continue;
        }
        if path == Path::new("/") {
            let _ = writeln!(io.err, "{}", Refusal::Root.message("rm", shown));
            status = 1;
            continue;
        }
        let (parent, name) = match open_parent(&path) {
            Ok(v) => v,
            Err(e) => {
                if !(opts.force && e.kind() == io::ErrorKind::NotFound) {
                    let _ = writeln!(io.err, "rm: {}: {}", show(shown), errno_msg(&e));
                    status = 1;
                }
                continue;
            }
        };
        let st = match sys::lstat_at(parent.as_fd(), &name) {
            Ok(st) => st,
            Err(e) => {
                if !(opts.force && e.kind() == io::ErrorKind::NotFound) {
                    let _ = writeln!(io.err, "rm: {}: {}", show(shown), errno_msg(&e));
                    status = 1;
                }
                continue;
            }
        };
        let base = OsStr::from_bytes(name.to_bytes());
        if let Err(refusal) = guard.check(base, &st, Some(parent.as_fd())) {
            let _ = writeln!(io.err, "{}", refusal.message("rm", shown));
            status = 1;
            continue;
        }
        let mut recorder = match txn.recorder() {
            Ok(r) => r,
            Err(e) => {
                recorder_error("rm", &e, io.err);
                return 1;
            }
        };
        let mut remover = Remover {
            rec: &mut recorder,
            budget: &mut budget,
            io,
            opts: &opts,
            status: 0,
            root_dev: st.dev,
        };
        if st.kind == Kind::Dir {
            if !opts.recursive {
                let _ = writeln!(remover.io.err, "rm: {}: is a directory", show(shown));
                status = 1;
                continue;
            }
            remover.remove_tree(parent.as_fd(), &name, &path_bytes(&path), &st, shown);
        } else {
            if !remover.confirm_file(parent.as_fd(), &name, &st, shown) {
                continue;
            }
            let entry = Pending::Entry {
                name: name.clone(),
                st,
                shown: shown.to_vec(),
            };
            remover.flush(
                parent.as_fd(),
                &path_bytes(sys::split_parent(&path).unwrap().0),
                vec![entry],
            );
        }
        status = status.max(remover.status);
    }
    finish_budget(txn, &mut budget);
    if io.cancel.load(Ordering::Relaxed) {
        let _ = writeln!(io.err, "rm: interrupted");
        return 130;
    }
    status
}

fn finish_budget(txn: &Txn, budget: &mut Budget<'_>) {
    if budget.notes.is_empty() {
        return;
    }
    if let Ok(mut rec) = txn.recorder() {
        let _ = rec.note(&budget.notes);
    }
    budget.notes.clear();
}

enum Pending {
    Entry {
        name: CString,
        st: Stat,
        shown: Vec<u8>,
    },
    Dir {
        name: CString,
        st: Stat,
        meta: Meta,
        shown: Vec<u8>,
    },
}

struct Frame {
    fd: Option<OwnedFd>,
    name: CString,
    st: Stat,
    path: Vec<u8>,
    shown: Vec<u8>,
    listing: Vec<CString>,
    next: usize,
    pending: Vec<Pending>,
    failed: bool,
    meta: Meta,
}

struct Remover<'r, 'b, 'i, 'o> {
    rec: &'r mut Recorder,
    budget: &'r mut Budget<'b>,
    io: &'r mut Io<'i>,
    opts: &'o RmOpts,
    status: i32,
    root_dev: u64,
}

impl Remover<'_, '_, '_, '_> {
    fn fail(&mut self, msg: String) {
        let _ = writeln!(self.io.err, "{msg}");
        self.status = 1;
    }

    fn cancelled(&self) -> bool {
        self.io.cancel.load(Ordering::Relaxed)
    }

    fn ask(&mut self, question: &str) -> bool {
        match (self.io.confirm)(question) {
            Some(answer) => answer,
            None => {
                self.io.cancel.store(true, Ordering::Relaxed);
                false
            }
        }
    }

    fn confirm_file(&mut self, dir: BorrowedFd<'_>, name: &CStr, st: &Stat, shown: &[u8]) -> bool {
        if self.opts.interactive {
            return self.ask(&format!("remove {} '{}'? ", st.kind.name(), show(shown)));
        }
        if !self.opts.force && st.kind != Kind::Symlink && stdin_is_tty() && !is_writable(dir, name)
        {
            return self.ask(&format!(
                "remove write-protected {} '{}'? ",
                st.kind.name(),
                show(shown)
            ));
        }
        true
    }

    /// Preserve, record, and remove a batch of entries of one directory.
    fn flush(&mut self, dir: BorrowedFd<'_>, dir_path: &[u8], batch: Vec<Pending>) -> Vec<bool> {
        if batch.is_empty() {
            return Vec::new();
        }
        let mut actions = Vec::with_capacity(batch.len());
        let mut kept = Vec::with_capacity(batch.len());
        let mut groups: HashMap<(u64, u64), Saved> = HashMap::new();
        for entry in batch {
            match entry {
                Pending::Entry { name, st, shown } => {
                    let path = join(dir_path, name.to_bytes());
                    // A second name of an inode already preserved in this
                    // batch shares the saved version, keeping the link group.
                    let saved = if st.nlink > 1
                        && let Some(saved) = groups.get(&(st.dev, st.ino))
                    {
                        Ok(saved.clone())
                    } else {
                        preserve::preserve(
                            &mut self.rec.stores,
                            self.budget,
                            dir,
                            &name,
                            &path,
                            &st,
                            Need::Unlink,
                        )
                    };
                    match saved {
                        Ok(saved) => {
                            if st.nlink > 1 {
                                groups.insert((st.dev, st.ino), saved.clone());
                            }
                            actions.push(Action::Unlink { path, saved });
                            kept.push((name, st, shown, false));
                        }
                        Err(e) => {
                            self.fail(format!("rm: {}: not removed: {e}", show(&shown)));
                        }
                    }
                }
                Pending::Dir {
                    name,
                    st,
                    meta,
                    shown,
                } => {
                    let path = join(dir_path, name.to_bytes());
                    actions.push(Action::Rmdir {
                        path,
                        ident: Ident::of(&st),
                        meta,
                    });
                    kept.push((name, st, shown, true));
                }
            }
        }
        if actions.is_empty() {
            return Vec::new();
        }
        let op = match self.rec.prepare(actions) {
            Ok(op) => op,
            Err(e) => {
                self.fail(format!("rm: cannot record undo information: {e}"));
                return vec![false; kept.len()];
            }
        };
        let mut done = 0u32;
        let mut results = Vec::with_capacity(kept.len());
        let mut error = None;
        for (name, st, shown, is_dir) in &kept {
            if error.is_some() || self.cancelled() {
                results.push(false);
                continue;
            }
            // Revalidate: remove only the object that was preserved.
            let current = sys::lstat_at(dir, name);
            let unchanged =
                matches!(&current, Ok(now) if now.same_object(st) && now.kind == st.kind);
            let result = if !unchanged {
                Err(io::Error::other(
                    "changed after it was examined; left in place",
                ))
            } else if *is_dir {
                sys::rmdir_at(dir, name)
            } else {
                sys::unlink_at(dir, name)
            };
            match result {
                Ok(()) => {
                    done += 1;
                    results.push(true);
                    if self.opts.verbose {
                        let _ = writeln!(self.io.out, "{}", show(shown));
                    }
                }
                Err(e) => {
                    // The prepared tail did not happen; stop the batch so
                    // the commit count stays a prefix.
                    self.fail(format!("rm: {}: {}", show(shown), errno_msg(&e)));
                    error = Some(e.to_string());
                    results.push(false);
                }
            }
        }
        if let Err(e) = self.rec.commit(op, done, error) {
            self.fail(format!("rm: cannot record undo outcome: {e}"));
        }
        results
    }

    /// Remove a directory tree, children before parents.
    fn remove_tree(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &CStr,
        path: &[u8],
        st: &Stat,
        shown: &[u8],
    ) {
        if self.opts.interactive
            && !self.ask(&format!("descend into directory '{}'? ", show(shown)))
        {
            return;
        }
        let root_fd = match sys::open_dir_at(parent, name) {
            Ok(fd) => fd,
            Err(e) => {
                self.fail(format!("rm: {}: {}", show(shown), errno_msg(&e)));
                return;
            }
        };
        let mut stack = match self.frame(root_fd, name, st, path, shown) {
            Some(frame) => vec![frame],
            None => return,
        };
        while !stack.is_empty() {
            if self.cancelled() {
                // Record what was already removed; the rest stays.
                let depth = stack.len();
                for i in (0..depth).rev() {
                    let pending = std::mem::take(&mut stack[i].pending);
                    let entry_path = stack[i].path.clone();
                    if let Some(fd) = self.frame_fd(&mut stack, i) {
                        self.flush(fd.as_fd(), &entry_path, pending);
                    }
                }
                return;
            }
            let top = stack.len() - 1;
            if stack[top].next < stack[top].listing.len() {
                let child = stack[top].listing[stack[top].next].clone();
                stack[top].next += 1;
                self.visit_child(&mut stack, &child);
                continue;
            }
            // Directory exhausted: flush its entries, then queue its removal
            // in the parent's batch.
            let pending = std::mem::take(&mut stack[top].pending);
            let top_path = stack[top].path.clone();
            if let Some(fd) = self.frame_fd(&mut stack, top) {
                let results = self.flush(fd.as_fd(), &top_path, pending);
                if results.iter().any(|ok| !ok) {
                    stack[top].failed = true;
                }
            } else {
                stack[top].failed = true;
            }
            let frame = stack.pop().unwrap();
            if frame.failed {
                if let Some(parent) = stack.last_mut() {
                    parent.failed = true;
                }
                continue;
            }
            if self.opts.interactive
                && !self.ask(&format!("remove directory '{}'? ", show(&frame.shown)))
            {
                if let Some(parent) = stack.last_mut() {
                    parent.failed = true;
                }
                continue;
            }
            let entry = Pending::Dir {
                name: frame.name.clone(),
                st: frame.st,
                meta: frame.meta.clone(),
                shown: frame.shown.clone(),
            };
            drop(frame);
            match stack.last_mut() {
                Some(parent_frame) => {
                    parent_frame.pending.push(entry);
                    if parent_frame.pending.len() >= BATCH {
                        let i = stack.len() - 1;
                        let pending = std::mem::take(&mut stack[i].pending);
                        let p = stack[i].path.clone();
                        if let Some(fd) = self.frame_fd(&mut stack, i) {
                            let results = self.flush(fd.as_fd(), &p, pending);
                            if results.iter().any(|ok| !ok) {
                                stack[i].failed = true;
                            }
                        }
                    }
                }
                None => {
                    let parent_path = parent_path_of(path);
                    self.flush(parent, &parent_path, vec![entry]);
                }
            }
        }
    }

    fn frame(
        &mut self,
        fd: OwnedFd,
        name: &CStr,
        st: &Stat,
        path: &[u8],
        shown: &[u8],
    ) -> Option<Frame> {
        match sys::fstat(fd.as_fd()) {
            Ok(now) if now.same_object(st) => {}
            _ => {
                self.fail(format!("rm: {}: changed while being examined", show(shown)));
                return None;
            }
        }
        let listing = match list_dir(fd.as_fd()) {
            Ok(l) => l,
            Err(e) => {
                self.fail(format!("rm: {}: {}", show(shown), errno_msg(&e)));
                return None;
            }
        };
        let meta = preserve::dir_meta(fd.as_fd(), st);
        Some(Frame {
            fd: Some(fd),
            name: name.to_owned(),
            st: *st,
            path: path.to_vec(),
            shown: shown.to_vec(),
            listing,
            next: 0,
            pending: Vec::new(),
            failed: false,
            meta,
        })
    }

    fn visit_child(&mut self, stack: &mut Vec<Frame>, child: &CStr) {
        let top = stack.len() - 1;
        let Some(dir) = self.frame_fd(stack, top) else {
            stack[top].failed = true;
            return;
        };
        let child_path = join(&stack[top].path, child.to_bytes());
        let child_shown = join(&stack[top].shown, child.to_bytes());
        let st = match sys::lstat_at(dir.as_fd(), child) {
            Ok(st) => st,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                self.fail(format!("rm: {}: {}", show(&child_shown), errno_msg(&e)));
                stack[top].failed = true;
                return;
            }
        };
        if st.kind == Kind::Dir {
            if st.dev != self.root_dev {
                self.fail(format!(
                    "rm: {}: on a different filesystem; not crossing mount boundaries",
                    show(&child_shown)
                ));
                stack[top].failed = true;
                return;
            }
            if stack.len() >= MAX_DEPTH {
                self.fail(format!(
                    "rm: {}: directory tree too deep",
                    show(&child_shown)
                ));
                stack[top].failed = true;
                return;
            }
            if self.opts.interactive
                && !self.ask(&format!(
                    "descend into directory '{}'? ",
                    show(&child_shown)
                ))
            {
                stack[top].failed = true;
                return;
            }
            let fd = match sys::open_dir_at(dir.as_fd(), child) {
                Ok(fd) => fd,
                Err(e) => {
                    self.fail(format!("rm: {}: {}", show(&child_shown), errno_msg(&e)));
                    stack[top].failed = true;
                    return;
                }
            };
            drop(dir);
            match self.frame(fd, child, &st, &child_path, &child_shown) {
                Some(frame) => {
                    stack.push(frame);
                    // Bound open descriptors: close the oldest one that is
                    // not needed until the walk returns to it.
                    if stack.len() > MAX_OPEN_DIRS {
                        let i = stack.len() - 1 - MAX_OPEN_DIRS;
                        if i > 0 {
                            stack[i].fd = None;
                        }
                    }
                }
                None => stack[top].failed = true,
            }
        } else {
            if !self.confirm_file(dir.as_fd(), child, &st, &child_shown) {
                stack[top].failed = true;
                return;
            }
            stack[top].pending.push(Pending::Entry {
                name: child.to_owned(),
                st,
                shown: child_shown,
            });
            if stack[top].pending.len() >= BATCH {
                let pending = std::mem::take(&mut stack[top].pending);
                let p = stack[top].path.clone();
                let results = self.flush(dir.as_fd(), &p, pending);
                if results.iter().any(|ok| !ok) {
                    stack[top].failed = true;
                }
            }
        }
    }

    /// Descriptor of frame `i`, reopening closed ancestors by name from the
    /// nearest open one and verifying each identity.
    fn frame_fd(&mut self, stack: &mut [Frame], i: usize) -> Option<OwnedFd> {
        if let Some(fd) = &stack[i].fd {
            return fd.try_clone().ok();
        }
        let mut j = i;
        while stack[j].fd.is_none() {
            j -= 1;
        }
        for k in j + 1..=i {
            let parent = stack[k - 1].fd.as_ref()?.try_clone().ok()?;
            let fd = sys::open_dir_at(parent.as_fd(), &stack[k].name).ok()?;
            match sys::fstat(fd.as_fd()) {
                Ok(now) if now.same_object(&stack[k].st) => stack[k].fd = Some(fd),
                _ => {
                    let msg = format!("rm: {}: changed during removal", show(&stack[k].shown));
                    self.fail(msg);
                    return None;
                }
            }
        }
        stack[i].fd.as_ref()?.try_clone().ok()
    }
}

fn parent_path_of(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => b"/".to_vec(),
        Some(i) => path[..i].to_vec(),
        None => b".".to_vec(),
    }
}

/// Names in a directory, excluding `.` and `..`.
pub fn list_dir(fd: BorrowedFd<'_>) -> io::Result<Vec<CString>> {
    let mut dir = rustix::fs::Dir::read_from(fd)?;
    let mut names = Vec::new();
    while let Some(entry) = dir.read() {
        let entry = entry?;
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Overwrite {
    Always,
    Ask,
    Never,
}

/// Protected `mv`.
pub fn mv(txn: &Txn, cwd: &Path, args: &[OsString], io: &mut Io<'_>) -> i32 {
    let mut overwrite = Overwrite::Always;
    let mut verbose = false;
    let mut operands = Vec::new();
    let mut options_done = false;
    for arg in args {
        let bytes = arg.as_bytes();
        if !options_done && bytes == b"--" {
            options_done = true;
        } else if !options_done && bytes.len() > 1 && bytes[0] == b'-' {
            let long = match bytes {
                b"--force" => Some(b'f'),
                b"--interactive" => Some(b'i'),
                b"--no-clobber" => Some(b'n'),
                b"--verbose" => Some(b'v'),
                _ => None,
            };
            let flags: Vec<u8> = match long {
                Some(c) => vec![c],
                None if bytes[1] != b'-' => bytes[1..].to_vec(),
                None => {
                    let _ = writeln!(
                        io.err,
                        "mv: unsupported option {} (native mv is protected; use /bin/mv for other options)\n{MV_USAGE}",
                        show(bytes)
                    );
                    return 2;
                }
            };
            for c in flags {
                match c {
                    b'f' => overwrite = Overwrite::Always,
                    b'i' => overwrite = Overwrite::Ask,
                    b'n' => overwrite = Overwrite::Never,
                    b'v' => verbose = true,
                    other => {
                        let _ = writeln!(
                            io.err,
                            "mv: unsupported option -{} (native mv is protected; use /bin/mv for other options)\n{MV_USAGE}",
                            other as char
                        );
                        return 2;
                    }
                }
            }
        } else {
            operands.push(arg.clone());
        }
    }
    if operands.len() < 2 {
        let _ = writeln!(io.err, "{MV_USAGE}");
        return 1;
    }
    let dest_arg = operands.pop().unwrap();
    let dest = resolve(cwd, &dest_arg);
    let dest_is_dir = sys::stat(&dest).is_ok_and(|st| st.kind == Kind::Dir);
    if operands.len() > 1 && !dest_is_dir {
        let _ = writeln!(io.err, "mv: {}: not a directory", show(dest_arg.as_bytes()));
        return 1;
    }
    let guard = Guard::for_txn(&txn.home_root);
    let mut budget = Budget::new(txn.config.copy_limit, txn.config.min_free, io.cancel);
    let mut status = 0;
    for src_arg in &operands {
        if io.cancel.load(Ordering::Relaxed) {
            break;
        }
        let src = resolve(cwd, src_arg);
        if names_dot_or_dotdot(src_arg.as_bytes()) {
            let _ = writeln!(
                io.err,
                "mv: {}: cannot move '.' or '..'",
                show(src_arg.as_bytes())
            );
            status = 1;
            continue;
        }
        let Some(src_name) = src.file_name().map(OsStr::to_owned) else {
            let _ = writeln!(io.err, "mv: {}: invalid source", show(src_arg.as_bytes()));
            status = 1;
            continue;
        };
        let target = if dest_is_dir {
            dest.join(&src_name)
        } else {
            dest.clone()
        };
        let target_shown = if dest_is_dir {
            join(dest_arg.as_bytes(), src_name.as_bytes())
        } else {
            dest_arg.as_bytes().to_vec()
        };
        let mut mover = Mover {
            txn,
            guard: &guard,
            budget: &mut budget,
            io,
            overwrite,
            verbose,
        };
        if let Err(msg) = mover.move_one(&src, src_arg.as_bytes(), &target, &target_shown) {
            if !msg.is_empty() {
                let _ = writeln!(mover.io.err, "{msg}");
            }
            status = 1;
        }
    }
    finish_budget(txn, &mut budget);
    if io.cancel.load(Ordering::Relaxed) {
        let _ = writeln!(io.err, "mv: interrupted");
        return 130;
    }
    status
}

/// Native `mv` only renames within one filesystem. A move between
/// filesystems is a copy and a removal, which it does not do natively; the
/// caller decides whether to run the unprotected external utility.
fn cross_device(src: &[u8], target: &[u8]) -> String {
    format!(
        "mv: {} -> {}: on different filesystems, and native mv only renames within one filesystem; nothing was moved (use /bin/mv for an unprotected move)",
        show(src),
        show(target)
    )
}

struct Mover<'t, 'g, 'b, 'r, 'i> {
    txn: &'t Txn,
    guard: &'g Guard,
    budget: &'r mut Budget<'b>,
    io: &'r mut Io<'i>,
    overwrite: Overwrite,
    verbose: bool,
}

impl Mover<'_, '_, '_, '_, '_> {
    fn move_one(
        &mut self,
        src: &Path,
        src_shown: &[u8],
        target: &Path,
        target_shown: &[u8],
    ) -> Result<(), String> {
        let fail = |e: &io::Error, shown: &[u8]| format!("mv: {}: {}", show(shown), errno_msg(e));
        if src == Path::new("/") {
            return Err(Refusal::Root.message("mv", src_shown));
        }
        let (sparent, sname) = open_parent(src).map_err(|e| fail(&e, src_shown))?;
        let sst = sys::lstat_at(sparent.as_fd(), &sname).map_err(|e| fail(&e, src_shown))?;
        self.guard
            .check(
                OsStr::from_bytes(sname.to_bytes()),
                &sst,
                Some(sparent.as_fd()),
            )
            .map_err(|r| r.message("mv", src_shown))?;
        let (tparent, tname) = open_parent(target).map_err(|e| fail(&e, target_shown))?;
        let tparent_st = sys::fstat(tparent.as_fd()).map_err(|e| fail(&e, target_shown))?;
        // Decided from the two filesystems alone, before any question about
        // overwriting a target that could never be reached.
        if sst.dev != tparent_st.dev {
            return Err(cross_device(src_shown, target_shown));
        }
        let tst = match sys::lstat_at(tparent.as_fd(), &tname) {
            Ok(st) => Some(st),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(fail(&e, target_shown)),
        };
        if let Some(tst) = &tst {
            if tst.same_object(&sst) {
                return Err(format!(
                    "mv: {} and {} are the same file",
                    show(src_shown),
                    show(target_shown)
                ));
            }
            self.guard
                .check(
                    OsStr::from_bytes(tname.to_bytes()),
                    tst,
                    Some(tparent.as_fd()),
                )
                .map_err(|r| r.message("mv", target_shown))?;
            match self.overwrite {
                Overwrite::Never => return Ok(()),
                Overwrite::Ask => {
                    match (self.io.confirm)(&format!("overwrite '{}'? ", show(target_shown))) {
                        Some(true) => {}
                        Some(false) => return Ok(()),
                        None => {
                            self.io.cancel.store(true, Ordering::Relaxed);
                            return Ok(());
                        }
                    }
                }
                Overwrite::Always => {}
            }
            if sst.kind == Kind::Dir && tst.kind != Kind::Dir {
                return Err(format!(
                    "mv: cannot overwrite non-directory {} with directory {}",
                    show(target_shown),
                    show(src_shown)
                ));
            }
            if sst.kind != Kind::Dir && tst.kind == Kind::Dir {
                return Err(format!(
                    "mv: cannot overwrite directory {} with non-directory {}",
                    show(target_shown),
                    show(src_shown)
                ));
            }
            if tst.kind == Kind::Dir {
                let fd = sys::open_dir_at(tparent.as_fd(), &tname)
                    .map_err(|e| fail(&e, target_shown))?;
                if !list_dir(fd.as_fd())
                    .map_err(|e| fail(&e, target_shown))?
                    .is_empty()
                {
                    return Err(format!("mv: {}: Directory not empty", show(target_shown)));
                }
            }
        }
        if sst.kind == Kind::Dir && inside(tparent.as_fd(), (sst.dev, sst.ino)) {
            return Err(format!(
                "mv: cannot move {} to a subdirectory of itself, {}",
                show(src_shown),
                show(target_shown)
            ));
        }
        let mut rec = self.txn.recorder().map_err(|e| {
            format!("mv: cannot record undo information: {e} (use /bin/mv to proceed without protection)")
        })?;
        let src_path = path_bytes(src);
        let target_path = path_bytes(target);
        self.rename(
            &mut rec,
            &sparent,
            &sname,
            &sst,
            &src_path,
            &tparent,
            &tname,
            tst.as_ref(),
            &target_path,
            target_shown,
        )
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::EXDEV) {
                cross_device(src_shown, target_shown)
            } else {
                format!("mv: {}: {}", show(src_shown), e)
            }
        })?;
        if self.verbose {
            let _ = writeln!(self.io.out, "{} -> {}", show(src_shown), show(target_shown));
        }
        Ok(())
    }

    /// Replaced destination, as an action, preserving it first.
    fn displace_target(
        &mut self,
        rec: &mut Recorder,
        tparent: &OwnedFd,
        tname: &CStr,
        tst: &Stat,
        target_path: &[u8],
    ) -> io::Result<Action> {
        if tst.kind == Kind::Dir {
            let fd = sys::open_dir_at(tparent.as_fd(), tname)?;
            Ok(Action::Rmdir {
                path: target_path.to_vec(),
                ident: Ident::of(tst),
                meta: preserve::dir_meta(fd.as_fd(), tst),
            })
        } else {
            let saved = preserve::preserve(
                &mut rec.stores,
                self.budget,
                tparent.as_fd(),
                tname,
                target_path,
                tst,
                Need::Unlink,
            )?;
            Ok(Action::Unlink {
                path: target_path.to_vec(),
                saved,
            })
        }
    }

    /// Same-filesystem move: a namespace operation. Only a replaced
    /// destination is preserved; the moved object is not copied.
    #[allow(clippy::too_many_arguments)]
    fn rename(
        &mut self,
        rec: &mut Recorder,
        sparent: &OwnedFd,
        sname: &CStr,
        sst: &Stat,
        src_path: &[u8],
        tparent: &OwnedFd,
        tname: &CStr,
        tst: Option<&Stat>,
        target_path: &[u8],
        target_shown: &[u8],
    ) -> io::Result<()> {
        let mut actions = Vec::new();
        if let Some(tst) = tst {
            actions.push(
                self.displace_target(rec, tparent, tname, tst, target_path)
                    .map_err(|e| {
                        io::Error::new(
                            e.kind(),
                            format!("{}: not replaced: {e}", show(target_shown)),
                        )
                    })?,
            );
        }
        actions.push(Action::Rename {
            from: src_path.to_vec(),
            to: target_path.to_vec(),
            ident: Ident::of(sst),
        });
        let count = actions.len() as u32;
        let op = rec.prepare(actions)?;
        let result = (|| {
            let now = sys::lstat_at(sparent.as_fd(), sname)?;
            if !now.same_object(sst) {
                return Err(io::Error::other("source changed after it was examined"));
            }
            if tst.is_some() {
                sys::rename_replace(sparent.as_fd(), sname, tparent.as_fd(), tname)
            } else {
                sys::rename_noreplace(sparent.as_fd(), sname, tparent.as_fd(), tname)
            }
        })();
        match result {
            Ok(()) => rec.commit(op, count, None),
            Err(e) => {
                let _ = rec.commit(op, 0, Some(e.to_string()));
                Err(e)
            }
        }
    }
}

/// How a writable redirection opens its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirMode {
    Truncate,
    Append,
    ReadWrite,
}

fn redirect_flags(mode: RedirMode) -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    let access = match mode {
        RedirMode::Truncate => OFlags::WRONLY,
        RedirMode::Append => OFlags::WRONLY | OFlags::APPEND,
        RedirMode::ReadWrite => OFlags::RDWR,
    };
    access | OFlags::CLOEXEC | OFlags::NOFOLLOW
}

/// Follow symlinks in the final component, so the resolved target is
/// protected without replacing the link. `None` when resolution fails; the
/// default open then reports the error.
fn resolve_final_symlinks(path: &Path) -> Option<(PathBuf, Option<Stat>)> {
    let mut p = path.to_path_buf();
    for _ in 0..40 {
        match sys::lstat(&p) {
            Ok(st) if st.kind == Kind::Symlink => {
                let target = std::fs::read_link(&p).ok()?;
                p = if target.is_absolute() {
                    target
                } else {
                    p.parent()?.join(target)
                };
                p = sys::clean_path(&p);
            }
            Ok(st) => return Some((p, Some(st))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Some((p, None)),
            Err(_) => return None,
        }
    }
    None
}

/// Open a writable redirection target with protection. Returns `None` to
/// let the shell's default open handle targets that are not regular files
/// (for example `/dev/null` or a FIFO), or when capture is disabled.
pub fn open_redirect(
    txn: &Txn,
    path: &Path,
    mode: RedirMode,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<io::Result<OwnedFd>> {
    if !txn.config.enabled {
        return None;
    }
    for _ in 0..8 {
        let (target, st) = resolve_final_symlinks(path)?;
        let (dir, name) = match open_parent(&target) {
            Ok(v) => v,
            Err(_) => return None,
        };
        let flags = redirect_flags(mode);
        match st {
            None => {
                let fd = match rustix::fs::openat(
                    dir.as_fd(),
                    &name,
                    flags | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL,
                    rustix::fs::Mode::from_raw_mode(0o666),
                ) {
                    Ok(fd) => fd,
                    Err(rustix::io::Errno::EXIST) => continue,
                    Err(e) => return Some(Err(e.into())),
                };
                // Creation destroys nothing, so it is recorded afterward.
                if let Ok(st) = sys::fstat(fd.as_fd())
                    && let Err(e) = record_create(txn, &target, &st)
                {
                    eprintln!(
                        "ish: undo: {}: creation not recorded: {e}",
                        target.display()
                    );
                }
                return Some(Ok(fd));
            }
            Some(st) if st.kind != Kind::File => return None,
            Some(st) => {
                let fd = match rustix::fs::openat(
                    dir.as_fd(),
                    &name,
                    flags,
                    rustix::fs::Mode::empty(),
                ) {
                    Ok(fd) => fd,
                    Err(rustix::io::Errno::LOOP) | Err(rustix::io::Errno::NOENT) => continue,
                    Err(e) => return Some(Err(e.into())),
                };
                match sys::fstat(fd.as_fd()) {
                    Ok(now) if now.same_object(&st) && now.kind == Kind::File => {}
                    _ => continue,
                }
                if let Err(e) = protect_write(txn, &dir, &name, &target, &st, &fd, mode, cancel) {
                    return Some(Err(e));
                }
                return Some(Ok(fd));
            }
        }
    }
    Some(Err(io::Error::other(
        "target kept changing while being opened",
    )))
}

fn record_create(txn: &Txn, path: &Path, st: &Stat) -> io::Result<()> {
    let mut rec = txn.recorder()?;
    let op = rec.prepare(vec![Action::Create {
        path: path_bytes(path),
        ident: Ident::of(st),
    }])?;
    rec.commit(op, 1, None)?;
    drop(rec);
    txn.cover(st.dev, st.ino);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn protect_write(
    txn: &Txn,
    dir: &OwnedFd,
    name: &CStr,
    path: &Path,
    st: &Stat,
    fd: &OwnedFd,
    mode: RedirMode,
    cancel: &std::sync::atomic::AtomicBool,
) -> io::Result<()> {
    let mut rec = txn.recorder().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "cannot record undo information: {e} (set ISH_UNDO off to write without protection)"
            ),
        )
    })?;
    let path_b = path_bytes(path);
    let mut budget = Budget::new(txn.config.copy_limit, txn.config.min_free, cancel);
    // A file this transaction already created or preserved does not need
    // another pre-image: undo restores the earliest one.
    let covered = txn.covers(st.dev, st.ino);
    let saved = if covered {
        None
    } else {
        Some(
            preserve::preserve(&mut rec.stores, &mut budget, dir.as_fd(), name, &path_b, st, Need::Frozen)
                .map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("cannot preserve existing contents for undo: {e} (set ISH_UNDO off to write without protection)"),
                    )
                })?,
        )
    };
    let op = rec.prepare(vec![Action::Write {
        path: path_b,
        ident: Ident::of(st),
        saved,
    }])?;
    let result = match mode {
        RedirMode::Truncate => rustix::fs::ftruncate(fd, 0).map_err(io::Error::from),
        RedirMode::Append | RedirMode::ReadWrite => Ok(()),
    };
    // The write itself happens later through the descriptor; the prepared
    // action covers it from here on.
    rec.commit(
        op,
        result.is_ok() as u32,
        result.as_ref().err().map(|e| e.to_string()),
    )?;
    let _ = rec.note(&budget.notes);
    drop(rec);
    txn.cover(st.dev, st.ino);
    result
}
