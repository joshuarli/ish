//! Retention: sizes, garbage collection, and purge.
//!
//! Collection deletes whole transactions, oldest first, and only objects
//! inside that transaction's own directories in stores whose identity was
//! verified. Active and interrupted transactions, those with versions on a
//! disconnected volume, and those being undone or redone right now are never
//! collected; the newest completed transaction is always kept.
//!
//! A transaction is deleted only while its replay lock is held, the same lock
//! replay holds for its whole run. Collection and explicit purge therefore
//! cannot remove a journal or objects from under a replay, whichever
//! transaction the replay was asked to act on.
//!
//! Sizes are reported three ways: logical bytes of retained versions,
//! allocated blocks (an overestimate for clones, whose blocks may be shared
//! with live files), and bytes actually copied in userspace. Exclusive
//! copy-on-write usage is not knowable without filesystem-specific tools.
//!
//! Automatic maintenance is bounded by the work it does, not by how much it
//! deletes: it loads at most a fixed number of journals per pass. Totals come
//! from a small per-transaction size file, written when a transaction is
//! sealed and recording whether it was, so a pass reads one tiny file per
//! transaction instead of opening every journal and walking every object
//! directory. A size is cached only while nothing can be replaying the
//! transaction, and replay drops it before appending anything.

use std::io;
use std::os::fd::AsFd;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::Config;
use crate::journal;
use crate::replay::{Lifecycle, Model, recorded_lifecycle};
use crate::store::{Home, Lock, Stores, VolumeState};
use crate::sys;

/// How often command-boundary maintenance may start a new round.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);
/// Journals one maintenance pass may load, and orphaned volume directories
/// it may remove. A pass that runs out picks up after itself next time.
const MAINTENANCE_WORK: usize = 32;

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub logical: u64,
    pub allocated: u64,
    pub objects: u64,
}

/// Retained usage of one transaction across the available stores, by walking
/// its object directories.
pub fn usage(stores: &mut Stores, id: u64) -> Usage {
    let mut usage = Usage::default();
    let ids = stores.store_ids();
    for store_id in ids {
        let Some(store) = stores.by_id(store_id) else {
            continue;
        };
        let dir = store.root.join("txn").join(id.to_string()).join("obj");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(st) = sys::lstat(&entry.path()) {
                usage.logical += st.size;
                usage.allocated += st.blocks * 512;
                usage.objects += 1;
            }
        }
    }
    usage
}

/// Whether the transaction references versions on a store that is not
/// currently available.
pub fn unavailable(stores: &mut Stores, model: &Model) -> bool {
    model
        .saved_versions()
        .filter_map(|s| s.obj())
        .any(|obj| stores.by_id(obj.store).is_none())
}

fn size_path(home: &Home, id: u64) -> std::path::PathBuf {
    home.txn_dir(id).join("size")
}

/// What is remembered about a transaction whose journal can no longer grow.
#[derive(Clone, Copy)]
struct CachedSize {
    bytes: u64,
    /// The journal has an end record.
    sealed: bool,
}

/// Remember a transaction's logical size and whether it was sealed. Best
/// effort: a missing size is recomputed from the journal. The caller holds
/// the transaction's replay lock, so no replay can be growing the journal.
pub fn cache_size(home: &Home, id: u64, bytes: u64, sealed: bool) {
    let state = if sealed { "sealed" } else { "open" };
    let _ = std::fs::write(size_path(home, id), format!("{bytes} {state}\n"));
}

/// Forget the cached size of a transaction whose journal is about to grow.
pub fn forget_size(home: &Home, id: u64) {
    let _ = std::fs::remove_file(size_path(home, id));
}

fn cached_size(home: &Home, id: u64) -> Option<CachedSize> {
    let text = std::fs::read_to_string(size_path(home, id)).ok()?;
    let (bytes, state) = text.trim().split_once(' ')?;
    Some(CachedSize {
        bytes: bytes.parse().ok()?,
        sealed: state == "sealed",
    })
}

/// Delete a transaction: its objects in every available store, then its
/// catalog directory. The caller holds the transaction's replay lock.
pub fn delete_txn(home: &Home, id: u64) -> io::Result<()> {
    let mut stores = Stores::new(home.clone(), id)?;
    for (entry, state) in stores.volume_list() {
        if let VolumeState::Available { .. } = state {
            let txn = entry.path.join("txn");
            let name = id.to_string();
            if txn.join(&name).exists() {
                let owner =
                    std::fs::read_to_string(txn.join(&name).join("owner")).unwrap_or_default();
                if owned_by(&owner, home) || owner.is_empty() {
                    let dir = sys::open_dir(&txn)?;
                    sys::remove_tree(dir.as_fd(), &sys::cstr(name.as_bytes())?)?;
                }
            }
        }
    }
    let txn = home.root.join("txn");
    let dir = sys::open_dir(&txn)?;
    sys::remove_tree(dir.as_fd(), &sys::cstr(id.to_string().as_bytes())?)?;
    sys::sync_dir(dir.as_fd())
}

/// Whether a volume transaction's `owner` file names this home store.
fn owned_by(owner: &str, home: &Home) -> bool {
    owner
        .split('\t')
        .nth(1)
        .and_then(|s| u64::from_str_radix(s.trim(), 16).ok())
        == Some(home.id)
}

/// The replay lock a deletion of `id` needs. `Ok(None)` means another process
/// holds it, and the transaction is being replayed. A transaction with no
/// lock file cannot be replayed at all (only one that is damaged or written by
/// an older build lacks one), so it needs none: `Ok(Some(None))`.
fn lock_for_delete(home: &Home, id: u64) -> io::Result<Option<Option<Lock>>> {
    match home.lock_replay(id) {
        Ok(Some(lock)) => Ok(Some(Some(lock))),
        Ok(None) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Some(None)),
        Err(e) => Err(e),
    }
}

/// Delete a transaction unless another process is replaying it. `Ok(false)`
/// means it was left alone because a replay holds its lock.
pub fn purge(home: &Home, id: u64) -> io::Result<bool> {
    let Some(_replay) = lock_for_delete(home, id)? else {
        return Ok(false);
    };
    delete_txn(home, id)?;
    Ok(true)
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub deleted: Vec<u64>,
    pub freed_logical: u64,
    /// Transactions left alone: unreadable, active, interrupted, on an
    /// unavailable volume, or being replayed.
    pub kept_protected: usize,
    pub orphans_removed: usize,
    /// Journals loaded and orphan directories removed.
    pub work: usize,
    /// The work budget ran out before the pass was done.
    pub more: bool,
}

/// A budget of expensive steps: journal loads and directory removals.
struct Work {
    left: Option<usize>,
    spent: usize,
}

impl Work {
    fn spend(&mut self) -> bool {
        match &mut self.left {
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                self.spent += 1;
                true
            }
            None => {
                self.spent += 1;
                true
            }
        }
    }
}

fn cursor_path(home: &Home) -> std::path::PathBuf {
    home.root.join("gc-cursor")
}

fn read_cursor(home: &Home) -> u64 {
    std::fs::read_to_string(cursor_path(home))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_cursor(home: &Home, id: u64) {
    if id == 0 {
        let _ = std::fs::remove_file(cursor_path(home));
    } else {
        let _ = std::fs::write(cursor_path(home), format!("{id}\n"));
    }
}

/// Collect transactions beyond the configured budgets. `work` bounds the
/// journals loaded and orphans removed (`None` for no bound, as `undo gc`
/// does). A bounded pass resumes after the last transaction it handled, so
/// protected transactions at the old end cannot starve collectable ones
/// behind them.
pub fn collect(home: &Home, config: &Config, work: Option<usize>) -> io::Result<GcReport> {
    let _lock = home.lock()?;
    let bounded = work.is_some();
    let mut work = Work {
        left: work,
        spent: 0,
    };
    let mut report = GcReport::default();
    let ids = home.txn_ids()?;
    let mut stores = Stores::new(home.clone(), 0)?;

    // Sizes come from the cache; a transaction without one is measured from
    // its journal, which costs work.
    let mut sizes = Vec::with_capacity(ids.len());
    let mut unsealed = Vec::with_capacity(ids.len());
    for &id in &ids {
        let cached = cached_size(home, id);
        unsealed.push(cached.is_some_and(|c| !c.sealed));
        let size = match cached {
            Some(cached) => cached.bytes,
            None if work.spend() => measure(home, id),
            None => {
                report.more = true;
                0
            }
        };
        sizes.push(size);
    }
    let mut total: u64 = sizes.iter().sum();
    let mut count = ids.len() as u64;

    let now_ns = sys::now_ns();
    let max_age_ns = config.max_age_days.saturating_mul(86_400_000_000_000);
    let free = |home: &Home| {
        sys::open_dir(&home.root)
            .and_then(|fd| sys::free_bytes(fd.as_fd()))
            .unwrap_or(u64::MAX)
    };
    let cursor = if bounded { read_cursor(home) } else { 0 };
    // Looked up only once something is about to be deleted.
    let mut newest_completed = None;
    let mut resume_after = 0;
    let mut finished = true;
    for (i, &id) in ids.iter().enumerate() {
        if id <= cursor {
            continue;
        }
        let Ok(begin) = journal::read_begin(&home.journal_path(id)) else {
            // Unreadable journals are kept for inspection.
            report.kept_protected += 1;
            resume_after = id;
            continue;
        };
        let too_old = now_ns.saturating_sub(begin.started_ns) > max_age_ns;
        let over =
            count > config.max_entries || total > config.max_bytes || free(home) < config.min_free;
        if !too_old && !over {
            if bounded {
                // Ids are in age order and deleting only tightens the
                // budgets: nothing after this needs collecting.
                break;
            }
            // An explicit collection keeps walking, only to report what it
            // could not have collected anyway.
            if is_protected(home, &mut stores, id) {
                report.kept_protected += 1;
            }
            resume_after = id;
            continue;
        }
        if unsealed[i] {
            // Its cached size says its journal never got an end record:
            // interrupted or unreadable, which nothing collects. There is no
            // journal to load to find that out again.
            report.kept_protected += 1;
            resume_after = id;
            continue;
        }
        let newest =
            *newest_completed.get_or_insert_with(|| find_newest_completed(home, &ids, &mut work));
        if newest == Newest::Unknown {
            report.more = true;
            finished = false;
            break;
        }
        if newest == Newest::Is(id) {
            resume_after = id;
            continue;
        }
        if !work.spend() {
            report.more = true;
            finished = false;
            break;
        }
        match try_delete(home, &mut stores, id)? {
            Fate::Deleted => {
                report.deleted.push(id);
                report.freed_logical += sizes[i];
                total = total.saturating_sub(sizes[i]);
                count -= 1;
            }
            Fate::Protected => report.kept_protected += 1,
        }
        resume_after = id;
    }
    if bounded {
        write_cursor(home, if finished { 0 } else { resume_after });
    }
    report.orphans_removed = remove_orphans(home, &mut stores, &ids, &mut work, &mut report.more)?;
    report.work = work.spent;
    Ok(report)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Newest {
    Is(u64),
    Nobody,
    /// The work budget ran out before the newest completed transaction was
    /// found, so nothing may be deleted yet.
    Unknown,
}

/// The newest transaction whose journal has an end record. It is always kept,
/// so that `undo` has something to act on however small the budgets are.
fn find_newest_completed(home: &Home, ids: &[u64], work: &mut Work) -> Newest {
    for &id in ids.iter().rev() {
        // The size file already says whether the journal was sealed, which
        // spares loading it: transactions that never were cost nothing to
        // pass over, however many of them sit at the new end.
        match cached_size(home, id) {
            Some(cached) if cached.sealed => return Newest::Is(id),
            Some(_) => continue,
            None => {}
        }
        if !work.spend() {
            return Newest::Unknown;
        }
        if Model::load(home, id).is_ok_and(|m| m.end.is_some()) {
            return Newest::Is(id);
        }
    }
    Newest::Nobody
}

/// Size a transaction from its journal, and remember it where that is safe.
/// The size is written only while holding the transaction's replay lock: a
/// replay drops the cached size before it appends to the journal, so a size
/// measured before a replay started must not be written after it.
fn measure(home: &Home, id: u64) -> u64 {
    let lock = lock_for_delete(home, id);
    let idle = matches!(lock, Ok(Some(_)));
    match Model::load(home, id) {
        Ok(model) => {
            // A journal that can no longer grow: sealed, or its shell died.
            // Active ones are still being written.
            if idle && recorded_lifecycle(home, &model) != Lifecycle::Active {
                cache_size(home, id, model.retained, model.end.is_some());
            }
            model.retained
        }
        Err(e) => {
            // A journal in a format this build does not read holds nothing it
            // can total, and re-reading it every pass would only spend work.
            // It is kept, not collected.
            if idle && e.kind() == io::ErrorKind::InvalidData {
                cache_size(home, id, 0, false);
            }
            0
        }
    }
}

/// Whether nothing may delete `id` right now: it is unreadable, unsealed, on
/// an unavailable volume, or being replayed.
fn is_protected(home: &Home, stores: &mut Stores, id: u64) -> bool {
    let Ok(Some(_replay)) = lock_for_delete(home, id) else {
        return true;
    };
    match Model::load(home, id) {
        Ok(model) => {
            recorded_lifecycle(home, &model) != Lifecycle::Completed || unavailable(stores, &model)
        }
        Err(_) => true,
    }
}

enum Fate {
    Deleted,
    Protected,
}

/// Delete `id` if nothing protects it. The replay lock is taken first and held
/// through the deletion, and the journal is read only after that, so the
/// decision is made on what the transaction looks like once no replay can
/// change it.
fn try_delete(home: &Home, stores: &mut Stores, id: u64) -> io::Result<Fate> {
    let Some(_replay) = lock_for_delete(home, id)? else {
        return Ok(Fate::Protected);
    };
    let Ok(model) = Model::load(home, id) else {
        return Ok(Fate::Protected);
    };
    // The lock is ours, so `lifecycle`'s probe would only find it held.
    if recorded_lifecycle(home, &model) != Lifecycle::Completed || unavailable(stores, &model) {
        return Ok(Fate::Protected);
    }
    delete_txn(home, id)?;
    Ok(Fate::Deleted)
}

/// Remove transaction directories on available volume stores whose catalog
/// transaction no longer exists (it was purged or collected while the volume
/// was disconnected).
fn remove_orphans(
    home: &Home,
    stores: &mut Stores,
    ids: &[u64],
    work: &mut Work,
    more: &mut bool,
) -> io::Result<usize> {
    let mut removed = 0;
    for (entry, state) in stores.volume_list() {
        if !matches!(state, VolumeState::Available { .. }) {
            continue;
        }
        let txn = entry.path.join("txn");
        let Ok(entries) = std::fs::read_dir(&txn) else {
            continue;
        };
        for e in entries.flatten() {
            let Some(id) = e.file_name().to_str().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            if ids.binary_search(&id).is_ok() || home.txn_dir(id).exists() {
                continue;
            }
            let owner = std::fs::read_to_string(e.path().join("owner")).unwrap_or_default();
            if owned_by(&owner, home) {
                if !work.spend() {
                    *more = true;
                    return Ok(removed);
                }
                let dir = sys::open_dir(&txn)?;
                sys::remove_tree(dir.as_fd(), &sys::cstr(id.to_string().as_bytes())?)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Bounded collection at a command boundary, at most once per interval. A
/// pass that runs out of work is continued at the next boundary instead of
/// waiting out the interval.
pub fn maybe_collect(home_root: &Path, config: &Config) {
    let Ok(Some(home)) = Home::open_existing(home_root) else {
        return;
    };
    let stamp = home.root.join("gc-stamp");
    let due = match std::fs::metadata(&stamp).and_then(|m| m.modified()) {
        Ok(t) => SystemTime::now()
            .duration_since(t)
            .map(|d| d >= MAINTENANCE_INTERVAL)
            .unwrap_or(true),
        Err(_) => true,
    };
    if !due {
        return;
    }
    if !matches!(collect(&home, config, Some(MAINTENANCE_WORK)), Ok(r) if r.more) {
        let _ = std::fs::write(&stamp, b"");
    }
}
