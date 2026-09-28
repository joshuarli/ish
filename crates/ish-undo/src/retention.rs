//! Retention: sizes, garbage collection, and purge.
//!
//! Collection deletes whole transactions, oldest first, and only objects
//! inside that transaction's own directories in stores whose identity was
//! verified. Active and interrupted transactions, and those with versions on
//! a disconnected volume, are never collected automatically; the newest
//! completed transaction is always kept.
//!
//! Sizes are reported three ways: logical bytes of retained versions,
//! allocated blocks (an overestimate for clones, whose blocks may be shared
//! with live files), and bytes actually copied in userspace. Exclusive
//! copy-on-write usage is not knowable without filesystem-specific tools.

use std::io;
use std::os::fd::AsFd;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::Config;
use crate::replay::{Lifecycle, Model, lifecycle};
use crate::store::{Home, Stores, VolumeState};
use crate::sys;

/// How often command-boundary maintenance may run.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);
/// Transactions deleted per maintenance pass, to keep it bounded.
const MAINTENANCE_BATCH: usize = 32;

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub logical: u64,
    pub allocated: u64,
    pub objects: u64,
}

/// Retained usage of one transaction across the available stores.
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

/// Delete a transaction: its objects in every available store, then its
/// catalog directory.
pub fn delete_txn(home: &Home, id: u64) -> io::Result<()> {
    let mut stores = Stores::new(home.clone(), id)?;
    for (entry, state) in stores.volume_list() {
        if let VolumeState::Available { .. } = state {
            let txn = entry.path.join("txn");
            let name = id.to_string();
            if txn.join(&name).join("owner").exists() || txn.join(&name).exists() {
                let owner =
                    std::fs::read_to_string(txn.join(&name).join("owner")).unwrap_or_default();
                let owned = owner
                    .split('\t')
                    .nth(1)
                    .and_then(|s| u64::from_str_radix(s.trim(), 16).ok())
                    == Some(home.id);
                if owned || owner.is_empty() {
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

#[derive(Debug, Default)]
pub struct GcReport {
    pub deleted: Vec<u64>,
    pub freed_logical: u64,
    pub kept_protected: usize,
    pub orphans_removed: usize,
}

/// Collect transactions beyond the configured budgets.
pub fn collect(home: &Home, config: &Config, limit: Option<usize>) -> io::Result<GcReport> {
    let _lock = home.lock()?;
    let mut report = GcReport::default();
    let ids = home.txn_ids()?;
    let mut stores = Stores::new(home.clone(), 0)?;
    struct Candidate {
        id: u64,
        started_ns: u64,
        logical: u64,
        collectable: bool,
    }
    let mut candidates = Vec::new();
    let mut newest_completed = None;
    for &id in &ids {
        let Ok(model) = Model::load(home, id) else {
            // Unreadable journals are kept for inspection.
            report.kept_protected += 1;
            continue;
        };
        let life = lifecycle(home, &model);
        let unavailable = unavailable(&mut stores, &model);
        let collectable = life == Lifecycle::Completed && !unavailable;
        if life == Lifecycle::Completed {
            newest_completed = Some(id);
        }
        if !collectable {
            report.kept_protected += 1;
        }
        candidates.push(Candidate {
            id,
            started_ns: model.begin.started_ns,
            logical: usage(&mut stores, id).logical,
            collectable,
        });
    }
    let now_ns = sys::now_ns();
    let max_age_ns = config.max_age_days.saturating_mul(86_400_000_000_000);
    let mut total: u64 = candidates.iter().map(|c| c.logical).sum();
    let mut count = candidates.len() as u64;
    let free = |home: &Home| {
        sys::open_dir(&home.root)
            .and_then(|fd| sys::free_bytes(fd.as_fd()))
            .unwrap_or(u64::MAX)
    };
    for c in &candidates {
        if limit.is_some_and(|l| report.deleted.len() >= l) {
            break;
        }
        if !c.collectable || Some(c.id) == newest_completed {
            continue;
        }
        let too_old = now_ns.saturating_sub(c.started_ns) > max_age_ns;
        let over =
            count > config.max_entries || total > config.max_bytes || free(home) < config.min_free;
        if !too_old && !over {
            continue;
        }
        delete_txn(home, c.id)?;
        report.deleted.push(c.id);
        report.freed_logical += c.logical;
        total = total.saturating_sub(c.logical);
        count -= 1;
    }
    report.orphans_removed = remove_orphans(home, &mut stores, &ids)?;
    Ok(report)
}

/// Remove transaction directories on available volume stores whose catalog
/// transaction no longer exists (it was purged or collected while the volume
/// was disconnected).
fn remove_orphans(home: &Home, stores: &mut Stores, ids: &[u64]) -> io::Result<usize> {
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
            if ids.contains(&id) || home.txn_dir(id).exists() {
                continue;
            }
            let owner = std::fs::read_to_string(e.path().join("owner")).unwrap_or_default();
            let owned = owner
                .split('\t')
                .nth(1)
                .and_then(|s| u64::from_str_radix(s.trim(), 16).ok())
                == Some(home.id);
            if owned {
                let dir = sys::open_dir(&txn)?;
                sys::remove_tree(dir.as_fd(), &sys::cstr(id.to_string().as_bytes())?)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Bounded collection at a command boundary, at most once per interval.
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
    let _ = std::fs::write(&stamp, b"");
    let _ = collect(&home, config, Some(MAINTENANCE_BATCH));
}
