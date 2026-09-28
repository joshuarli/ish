//! Transaction lifecycle for native capture.
//!
//! One accepted input is one transaction. `Session::begin` only fills in
//! memory; the journal is created the first time something is actually
//! recorded, by whichever process gets there first. Pipeline stages and
//! command substitutions are forks of the shell, so they inherit the `Txn`
//! and coordinate through:
//!
//! - an anonymous shared memory page, mapped once per shell before any fork,
//!   whose slots publish each transaction's allocated id and opaque-command
//!   count across processes; and
//! - the store lock (`flock` on a descriptor each process opens itself) that
//!   serializes allocation.
//!
//! The journal is sealed by the shell when the foreground job actually
//! completes; a stopped job keeps its transaction (and its slot) active
//! across `fg`.

use std::cell::{RefCell, RefMut};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::Config;
use crate::journal::{self, Action, Begin, Evidence, Record};
use crate::preserve::{self, Budget};
use crate::store::{self, Home, Stores};
use crate::sys;

const SLOTS: usize = 4;

#[repr(C)]
#[derive(Default)]
struct Slot {
    generation: AtomicU64,
    txn: AtomicU64,
    opaque: AtomicU32,
    _pad: AtomicU32,
}

/// Pointer to the shared slot page. The mapping is never unmapped, so the
/// pointer stays valid for the life of the process and its forks.
#[derive(Clone, Copy)]
struct Page(*const [Slot; SLOTS]);

// SAFETY: the page only contains atomics.
unsafe impl Send for Page {}
unsafe impl Sync for Page {}

impl Page {
    fn new() -> Page {
        let len = std::mem::size_of::<[Slot; SLOTS]>();
        // SAFETY: a fresh anonymous mapping, zero-filled by the kernel, which
        // is a valid all-zero `[Slot; SLOTS]`.
        let mapped = unsafe {
            rustix::mm::mmap_anonymous(
                std::ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED,
            )
        };
        match mapped {
            Ok(ptr) => Page(ptr as *const [Slot; SLOTS]),
            // Without a shared mapping, forks cannot publish their
            // allocations; each would allocate a separate, detached
            // transaction instead.
            Err(_) => Page(Box::leak(Box::default())),
        }
    }

    fn slot(&self, i: usize) -> &Slot {
        // SAFETY: see `Page`.
        unsafe { &(*self.0)[i] }
    }
}

/// Journal writer and stores of one transaction in one process.
pub struct Recorder {
    pub id: u64,
    pub home: Home,
    pub stores: Stores,
    writer: journal::Writer,
    op_counter: u64,
}

impl Recorder {
    pub fn open(home: &Home, id: u64) -> io::Result<Recorder> {
        Ok(Recorder {
            id,
            home: home.clone(),
            stores: Stores::new(home.clone(), id)?,
            writer: journal::Writer::open(&home.journal_path(id))?,
            op_counter: 0,
        })
    }

    fn owned_by_current_process(&self) -> bool {
        self.writer.owned_by_current_process()
    }

    /// Record actions about to happen. Every object they reference is
    /// flushed first, and the record itself is durable on return: nothing
    /// destructive may run before this returns successfully.
    pub fn prepare(&mut self, actions: Vec<Action>) -> io::Result<u64> {
        self.op_counter += 1;
        let op = ((std::process::id() as u64) << 32) | self.op_counter;
        self.stores.sync_objects()?;
        self.writer
            .append(&[Record::Prepare { op, actions }], true)?;
        crate::fault::check("prepared")?;
        Ok(op)
    }

    /// Record how many prepared actions happened. Not synced: an unsynced
    /// commit is reconstructed from the filesystem as an ambiguous
    /// operation, never assumed.
    pub fn commit(&mut self, op: u64, done: u32, error: Option<String>) -> io::Result<()> {
        self.writer
            .append(&[Record::Commit { op, done, error }], false)
    }

    pub fn append(&mut self, records: &[Record], sync: bool) -> io::Result<()> {
        self.writer.append(records, sync)
    }

    pub fn note(&mut self, notes: &[String]) -> io::Result<()> {
        if notes.is_empty() {
            return Ok(());
        }
        let records: Vec<Record> = notes
            .iter()
            .map(|text| Record::Note { text: text.clone() })
            .collect();
        self.writer.append(&records, false)
    }
}

/// The native-capture context for one accepted input.
pub struct Txn {
    pub home_root: PathBuf,
    pub config: Config,
    session: u64,
    shell_pid: u32,
    page: Page,
    slot: usize,
    generation: u64,
    pub cwd: Vec<u8>,
    pub command: Vec<u8>,
    started_ns: u64,
    recorder: RefCell<Option<Recorder>>,
    /// Inodes this transaction created or already preserved, in this
    /// process and the processes it forked from.
    covered: RefCell<Vec<(u64, u64)>>,
}

impl Txn {
    pub fn covers(&self, dev: u64, ino: u64) -> bool {
        self.covered.borrow().contains(&(dev, ino))
    }

    pub fn cover(&self, dev: u64, ino: u64) {
        self.covered.borrow_mut().push((dev, ino));
    }

    /// The recorder for this process, allocating the transaction on first
    /// use.
    pub fn recorder(&self) -> io::Result<RefMut<'_, Recorder>> {
        let mut slot = self.recorder.borrow_mut();
        if slot.as_ref().is_some_and(|r| !r.owned_by_current_process()) {
            // Inherited across fork: this process needs its own descriptors
            // and locks.
            *slot = None;
        }
        if slot.is_none() {
            let home = Home::open_or_create(&self.home_root)?;
            let id = self.allocate(&home)?;
            *slot = Some(Recorder::open(&home, id)?);
        }
        Ok(RefMut::map(slot, |r| r.as_mut().unwrap()))
    }

    fn allocate(&self, home: &Home) -> io::Result<u64> {
        let shared = self.page.slot(self.slot);
        let lock = home.lock()?;
        let current = shared.generation.load(Ordering::Acquire) == self.generation;
        if current {
            let id = shared.txn.load(Ordering::Acquire);
            if id != 0 {
                return Ok(id);
            }
        }
        let begin = Begin {
            id: 0,
            session: self.session,
            shell_pid: self.shell_pid,
            started_ns: self.started_ns,
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            scope: None,
            detached: !current,
        };
        let id = home.allocate(begin, &lock)?;
        if current {
            shared.txn.store(id, Ordering::Release);
        }
        Ok(id)
    }

    /// The transaction id, if anything has been recorded yet.
    pub fn allocated_id(&self) -> Option<u64> {
        let shared = self.page.slot(self.slot);
        (shared.generation.load(Ordering::Acquire) == self.generation)
            .then(|| shared.txn.load(Ordering::Acquire))
            .filter(|&id| id != 0)
    }

    /// Count an external command that ran without capture.
    pub fn note_opaque(&self) {
        let shared = self.page.slot(self.slot);
        if shared.generation.load(Ordering::Acquire) == self.generation {
            shared.opaque.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn opaque_count(&self) -> u32 {
        self.page.slot(self.slot).opaque.load(Ordering::Acquire)
    }

    pub fn session(&self) -> u64 {
        self.session
    }
}

/// A transaction whose foreground job is stopped.
pub struct Suspended {
    txn: Txn,
}

impl Suspended {
    pub fn txn(&self) -> &Txn {
        &self.txn
    }
}

/// Per-shell recovery state.
pub struct Session {
    pub id: u64,
    pub shell_pid: u32,
    page: Page,
    generation: u64,
    reserved: [bool; SLOTS],
    next_slot: usize,
    lock: Option<OwnedFd>,
}

impl Default for Session {
    fn default() -> Self {
        Session::new()
    }
}

impl Session {
    /// Create session state. Maps the shared page; performs no file I/O.
    pub fn new() -> Session {
        Session {
            id: sys::random_u64(),
            shell_pid: std::process::id(),
            page: Page::new(),
            generation: 0,
            reserved: [false; SLOTS],
            next_slot: 0,
            lock: None,
        }
    }

    /// Start the transaction for an accepted input.
    pub fn begin(&mut self, home_root: PathBuf, config: Config, cwd: &Path, command: &str) -> Txn {
        self.generation += 1;
        let mut slot = self.next_slot;
        for _ in 0..SLOTS {
            if !self.reserved[slot] {
                break;
            }
            slot = (slot + 1) % SLOTS;
        }
        self.next_slot = (slot + 1) % SLOTS;
        let shared = self.page.slot(slot);
        shared.txn.store(0, Ordering::Release);
        shared.opaque.store(0, Ordering::Release);
        shared.generation.store(self.generation, Ordering::Release);
        Txn {
            home_root,
            config,
            session: self.id,
            shell_pid: self.shell_pid,
            page: self.page,
            slot,
            generation: self.generation,
            cwd: cwd.as_os_str().as_bytes().to_vec(),
            command: command.as_bytes().to_vec(),
            started_ns: sys::now_ns(),
            recorder: RefCell::new(None),
            covered: RefCell::new(Vec::new()),
        }
    }

    /// Hold the session liveness lock once this shell has recorded
    /// something, so other shells can tell an active transaction from an
    /// interrupted one.
    pub fn hold_lock(&mut self, home_root: &Path) {
        if self.lock.is_none()
            && let Ok(Some(home)) = Home::open_existing(home_root)
        {
            self.lock = store::hold_session_lock(&home, self.id).ok();
        }
    }

    /// The foreground job stopped: keep the transaction active.
    pub fn suspend(&mut self, txn: Txn) -> Suspended {
        self.reserved[txn.slot] = true;
        if txn.allocated_id().is_some() {
            self.hold_lock(&txn.home_root.clone());
        }
        Suspended { txn }
    }

    /// Seal a transaction whose job completed. Returns its id if anything
    /// was recorded.
    pub fn finish(&mut self, txn: Txn, status: i32) -> Option<u64> {
        self.reserved[txn.slot] = false;
        let id = txn.allocated_id()?;
        self.hold_lock(&txn.home_root);
        if let Err(e) = seal(&txn.home_root, id, status, txn.opaque_count(), &txn.config) {
            eprintln!("ish: undo: could not seal transaction {id}: {e}");
        }
        drop(txn);
        Some(id)
    }

    pub fn finish_suspended(&mut self, suspended: Suspended, status: i32) -> Option<u64> {
        self.finish(suspended.txn, status)
    }
}

/// Capture post-state evidence for paths whose last recorded action left
/// content that later replay must compare, then append the end record.
pub fn seal(
    home_root: &Path,
    id: u64,
    status: i32,
    opaque: u32,
    config: &Config,
) -> io::Result<()> {
    let home = Home::open_existing(home_root)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "undo store missing"))?;
    let scan = journal::read(&home.journal_path(id))?;
    if scan.records.iter().any(|r| matches!(r, Record::End { .. })) {
        return Ok(());
    }
    let mut recorder = Recorder::open(&home, id)?;
    let finals = final_paths(&scan.records);
    let never = AtomicBool::new(false);
    let mut budget = Budget::new(0, config.min_free, &never);
    let mut records = Vec::new();
    for path in finals {
        let p = Path::new(std::ffi::OsStr::from_bytes(&path));
        let evidence = match sys::split_parent(p)
            .and_then(|(parent, name)| Ok((sys::open_dir(parent)?, sys::os_cstr(name)?)))
        {
            Ok((dir, name)) => preserve::capture_evidence(
                &mut recorder.stores,
                &mut budget,
                std::os::fd::AsFd::as_fd(&dir),
                &name,
            )
            .unwrap_or(Evidence::Unknown),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Evidence::Absent,
            Err(_) => Evidence::Unknown,
        };
        records.push(Record::Final { path, evidence });
    }
    if opaque > 0 {
        records.push(Record::Opaque { count: opaque });
    }
    records.push(Record::End {
        status,
        finished_ns: sys::now_ns(),
        interrupted: status == 130,
    });
    recorder.stores.sync_objects()?;
    recorder.append(&records, true)
}

/// Paths whose final recorded action created or wrote file content.
fn final_paths(records: &[Record]) -> Vec<Vec<u8>> {
    let mut last: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut set = |path: &[u8], needs: bool| {
        if let Some(entry) = last.iter_mut().find(|(p, _)| p == path) {
            entry.1 = needs;
        } else {
            last.push((path.to_vec(), needs));
        }
    };
    for record in records {
        if let Record::Prepare { actions, .. } = record {
            for action in actions {
                match action {
                    Action::Create { path, .. } | Action::Write { path, .. } => set(path, true),
                    Action::Rename { from, to, .. } => {
                        set(from, false);
                        set(to, false);
                    }
                    other => {
                        for p in other.paths() {
                            set(p, false);
                        }
                    }
                }
            }
        }
    }
    last.into_iter()
        .filter(|(_, needs)| *needs)
        .map(|(p, _)| p)
        .collect()
}
