//! Scoped checkpoints: `undo run --scope <dir> -- <program> ...`.
//!
//! Before the program starts, every entry under the scope is cataloged and
//! each regular file gets a frozen before-image (a clone, or an independent
//! copy within the byte-copy limit; never a hard link, which would change
//! along with the file). After the program completes, the tree is walked
//! again and the differences become ordinary journal actions, so undo and
//! redo use the same conditional replay as native commands.
//!
//! This records observed changes within one tree over one interval. It is
//! not process attribution, a sandbox, or an atomic snapshot: concurrent
//! writers and detached descendants can make recovery conflict or remain
//! uncertain, and the checkpoint is kept either way.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Config;
use crate::journal::{
    self, Action, Begin, CatEntry, Content, Evidence, Ident, Meta, Record, Saved,
};
use crate::preserve::{self, Budget, Need};
use crate::replay::Model;
use crate::store::Home;
use crate::sys::{self, Kind, Stat};
use crate::txn::Recorder;

/// Timestamps this close to the checkpoint start may hide a same-tick
/// change on filesystems with coarse timestamps, so such files are compared
/// by content instead of trusted by stat.
const RACY_WINDOW_NS: i128 = 2_000_000_000;

/// A before-checkpoint waiting for its program to finish.
pub struct Checkpoint {
    pub id: u64,
    pub root: PathBuf,
    home: Home,
    started: (i64, u32),
    entries: Vec<CatEntry>,
    config: Config,
}

pub struct Summary {
    pub id: u64,
    pub created: usize,
    pub modified: usize,
    pub removed: usize,
    /// Directories whose metadata (including times) changed.
    pub metadata: usize,
    pub notes: Vec<String>,
}

fn join_rel(rel: &[u8], name: &[u8]) -> Vec<u8> {
    if rel.is_empty() {
        name.to_vec()
    } else {
        let mut p = rel.to_vec();
        p.push(b'/');
        p.extend_from_slice(name);
        p
    }
}

fn abs(root: &Path, rel: &[u8]) -> Vec<u8> {
    let mut p = root.as_os_str().as_bytes().to_vec();
    if !rel.is_empty() {
        if !p.ends_with(b"/") {
            p.push(b'/');
        }
        p.extend_from_slice(rel);
    }
    p
}

fn show(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

/// Check the scope before anything is recorded.
pub fn validate_scope(home_root: &Path, scope: &Path) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(scope).map_err(|e| format!("{}: {e}", scope.display()))?;
    let st = sys::lstat(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    if st.kind != Kind::Dir {
        return Err(format!("{}: not a directory", root.display()));
    }
    if root == Path::new("/") {
        return Err("the scope must not be the filesystem root".into());
    }
    let mut stores = vec![home_root.to_path_buf()];
    if let Ok(Some(home)) = Home::open_existing(home_root) {
        stores.extend(
            home.registry()
                .unwrap_or_default()
                .into_iter()
                .map(|e| e.path),
        );
    }
    for store in stores {
        let store = canonical_prefix(&store);
        if store.starts_with(&root) || root.starts_with(&store) {
            return Err(format!(
                "{}: overlaps the undo store at {}",
                root.display(),
                store.display()
            ));
        }
    }
    Ok(root)
}

/// Canonicalize the longest existing prefix of `path` and append the rest,
/// so a store that does not exist yet still compares through symlinks such
/// as `/var` → `/private/var`.
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&cur) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name().map(|n| n.to_owned()), cur.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                cur = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Walk a tree in pre-order without following symlinks, calling `visit`
/// with each entry's parent descriptor, name, relative path, and stat.
fn walk(
    root: &Path,
    cancel: &AtomicBool,
    mut visit: impl FnMut(BorrowedFd<'_>, &CString, &[u8], &Stat) -> Result<(), String>,
) -> Result<(), String> {
    let (parent, name) = sys::split_parent(root).map_err(|e| e.to_string())?;
    let parent_fd = sys::open_dir(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let name = sys::os_cstr(name).map_err(|e| e.to_string())?;
    let st =
        sys::lstat_at(parent_fd.as_fd(), &name).map_err(|e| format!("{}: {e}", root.display()))?;
    let root_dev = st.dev;
    visit(parent_fd.as_fd(), &name, b"", &st)?;
    // Pending directories hold their parent's descriptor, shared, and are
    // opened only when visited, so open descriptors stay bounded by depth
    // rather than by how many directories are waiting.
    let mut stack: Vec<(Rc<OwnedFd>, CString, Vec<u8>, Stat)> =
        vec![(Rc::new(parent_fd), name, Vec::new(), st)];
    while let Some((parent, name, rel, st)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err("interrupted".into());
        }
        let fd = sys::open_dir_at(parent.as_fd(), &name)
            .map_err(|e| format!("{}: {e}", show(&abs(root, &rel))))?;
        drop(parent);
        match sys::fstat(fd.as_fd()) {
            Ok(now) if now.same_object(&st) => {}
            _ => {
                return Err(format!(
                    "{}: changed while being checkpointed",
                    show(&abs(root, &rel))
                ));
            }
        }
        let fd = Rc::new(fd);
        let mut names = crate::ops::list_dir(fd.as_fd())
            .map_err(|e| format!("{}: cannot read directory: {e}", show(&abs(root, &rel))))?;
        names.sort();
        let mut subdirs = Vec::new();
        for child in names {
            let child_rel = join_rel(&rel, child.to_bytes());
            let st = match sys::lstat_at(fd.as_fd(), &child) {
                Ok(st) => st,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("{}: {e}", show(&abs(root, &child_rel)))),
            };
            if st.kind == Kind::Dir && st.dev != root_dev {
                return Err(format!(
                    "{}: a different filesystem is mounted here; scoped checkpoints do not cross mounts",
                    show(&abs(root, &child_rel))
                ));
            }
            visit(fd.as_fd(), &child, &child_rel, &st)?;
            if st.kind == Kind::Dir {
                subdirs.push((fd.clone(), child, child_rel, st));
            }
        }
        // Reverse so the stack visits subdirectories in sorted order.
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(())
}

/// Take the before-checkpoint. Nothing is launched if this fails, and a
/// failed checkpoint leaves no transaction behind.
#[allow(clippy::too_many_arguments)]
pub fn begin(
    home_root: &Path,
    config: &Config,
    session: u64,
    shell_pid: u32,
    cwd: &Path,
    command: &str,
    scope: &Path,
    cancel: &AtomicBool,
) -> Result<Checkpoint, String> {
    let root = validate_scope(home_root, scope)?;
    let home = Home::open_or_create(home_root).map_err(|e| format!("undo store: {e}"))?;
    let id = {
        let lock = home.lock().map_err(|e| e.to_string())?;
        home.allocate(
            Begin {
                id: 0,
                session,
                shell_pid,
                started_ns: sys::now_ns(),
                cwd: cwd.as_os_str().as_bytes().to_vec(),
                command: command.as_bytes().to_vec(),
                scope: Some(root.as_os_str().as_bytes().to_vec()),
                detached: false,
            },
            &lock,
        )
        .map_err(|e| format!("undo store: {e}"))?
    };
    let started = sys::now();
    let result = (|| {
        let mut rec = Recorder::open(&home, id).map_err(|e| e.to_string())?;
        let mut budget = Budget::new(config.copy_limit, config.min_free, cancel);
        let mut entries = Vec::new();
        walk(&root, cancel, |dir, name, rel, st| {
            if entries.len() as u64 >= config.scope_max_entries {
                return Err(format!(
                    "{}: more than {} entries (ISH_UNDO_SCOPE_LIMIT)",
                    root.display(),
                    config.scope_max_entries
                ));
            }
            let path = abs(&root, rel);
            let mut entry = CatEntry {
                rel: rel.to_vec(),
                ident: Ident::of(st),
                meta: Meta::of(st),
                nlink: st.nlink,
                obj: None,
                target: None,
            };
            match st.kind {
                Kind::Dir => {
                    let fd =
                        sys::open_dir_at(dir, name).map_err(|e| format!("{}: {e}", show(&path)))?;
                    entry.meta = preserve::dir_meta(fd.as_fd(), st);
                }
                Kind::File => {
                    let saved = preserve::preserve(
                        &mut rec.stores,
                        &mut budget,
                        dir,
                        name,
                        &path,
                        st,
                        Need::Frozen,
                    )
                    .map_err(|e| format!("{e}"))?;
                    if let Content::File { obj, strength } = saved.content {
                        entry.obj = Some((obj, strength));
                    }
                }
                Kind::Symlink => {
                    entry.target = Some(
                        sys::read_link_at(dir, name)
                            .map_err(|e| format!("{}: {e}", show(&path)))?,
                    );
                }
                _ => {}
            }
            entries.push(entry);
            Ok(())
        })?;
        rec.stores.sync_objects().map_err(|e| e.to_string())?;
        journal::write_catalog(&home.txn_dir(id).join("before.cat"), &entries)
            .map_err(|e| e.to_string())?;
        let mut records: Vec<Record> = budget
            .notes
            .iter()
            .map(|t| Record::Note { text: t.clone() })
            .collect();
        records.push(Record::Checkpoint {
            entries: entries.len() as u64,
            cloned: budget.clones,
            copied_bytes: budget.stats.bytes,
        });
        rec.append(&records, true).map_err(|e| e.to_string())?;
        Ok(entries)
    })();
    match result {
        Ok(entries) => Ok(Checkpoint {
            id,
            root,
            home,
            started,
            entries,
            config: config.clone(),
        }),
        Err(e) => {
            // Nothing ran: drop the partial checkpoint entirely.
            let _ = crate::retention::delete_txn(&home, id);
            Err(e)
        }
    }
}

impl Checkpoint {
    /// Take the after-checkpoint and record the observed changes.
    pub fn finish(self, status: i32, cancel: &AtomicBool) -> io::Result<Summary> {
        let started_ns = self.started.0 as i128 * 1_000_000_000 + self.started.1 as i128;
        record_changes(
            &self.home,
            self.id,
            &self.root,
            self.entries,
            started_ns,
            status,
            false,
            &self.config,
            cancel,
        )
    }
}

/// Record changes for a scoped transaction whose shell died before the
/// after-checkpoint.
pub fn finalize_interrupted(home: &Home, id: u64, cancel: &AtomicBool) -> io::Result<()> {
    let model = Model::load(home, id)?;
    if model.end.is_some() {
        return Ok(());
    }
    let Some(root) = model.begin.scope.clone() else {
        return Ok(());
    };
    let entries = journal::read_catalog(&home.txn_dir(id).join("before.cat"))?;
    record_changes(
        home,
        id,
        Path::new(OsStr::from_bytes(&root)),
        entries,
        model.begin.started_ns as i128,
        -1,
        true,
        &Config::default(),
        cancel,
    )?;
    Ok(())
}

struct After {
    st: Stat,
    target: Option<Vec<u8>>,
    meta: Meta,
}

fn racy(ident: &Ident, started_ns: i128) -> bool {
    let ns = |t: (i64, u32)| t.0 as i128 * 1_000_000_000 + t.1 as i128;
    ns(ident.mtime) >= started_ns - RACY_WINDOW_NS || ns(ident.ctime) >= started_ns - RACY_WINDOW_NS
}

#[allow(clippy::too_many_arguments)]
fn record_changes(
    home: &Home,
    id: u64,
    root: &Path,
    before: Vec<CatEntry>,
    started_ns: i128,
    status: i32,
    interrupted: bool,
    config: &Config,
    cancel: &AtomicBool,
) -> io::Result<Summary> {
    let mut rec = Recorder::open(home, id)?;
    let mut notes = Vec::new();
    let mut after: HashMap<Vec<u8>, After> = HashMap::new();
    let mut order: Vec<Vec<u8>> = Vec::new();
    let never = AtomicBool::new(false);
    let walked = walk(root, &never, |dir, name, rel, st| {
        let target = (st.kind == Kind::Symlink)
            .then(|| sys::read_link_at(dir, name).ok())
            .flatten();
        let meta = if st.kind == Kind::Dir {
            sys::open_dir_at(dir, name)
                .map(|fd| preserve::dir_meta(fd.as_fd(), st))
                .unwrap_or_else(|_| Meta::of(st))
        } else {
            Meta::of(st)
        };
        order.push(rel.to_vec());
        after.insert(
            rel.to_vec(),
            After {
                st: *st,
                target,
                meta,
            },
        );
        Ok(())
    });
    if let Err(e) = walked {
        notes.push(format!("after-checkpoint incomplete: {e}"));
    }

    let mut removed: Vec<Action> = Vec::new();
    let mut created: Vec<(Vec<u8>, Action)> = Vec::new();
    let mut modified: Vec<Action> = Vec::new();
    let mut referenced: Vec<journal::ObjRef> = Vec::new();
    let before_map: HashMap<Vec<u8>, &CatEntry> =
        before.iter().map(|e| (e.rel.clone(), e)).collect();

    let saved_of = |entry: &CatEntry| -> Option<Saved> {
        let content = match (&entry.obj, &entry.target) {
            (Some((obj, strength)), _) => Content::File {
                obj: obj.clone(),
                strength: *strength,
            },
            (None, Some(target)) => Content::Symlink {
                target: target.clone(),
            },
            _ => return None,
        };
        Some(Saved {
            content,
            ident: entry.ident,
            meta: entry.meta.clone(),
            nlink: entry.nlink,
            copied: 0,
        })
    };

    for entry in &before {
        let path = abs(root, &entry.rel);
        let now = after.get(&entry.rel);
        let kind_changed = now.is_some_and(|a| a.st.kind != entry.ident.kind);
        if now.is_none() || kind_changed {
            match entry.ident.kind {
                Kind::Dir => removed.push(Action::Rmdir {
                    path,
                    ident: entry.ident,
                    meta: entry.meta.clone(),
                }),
                _ => match saved_of(entry) {
                    Some(saved) => {
                        if let Some(obj) = saved.obj() {
                            referenced.push(obj.clone());
                        }
                        removed.push(Action::Unlink { path, saved });
                    }
                    None => notes.push(format!(
                        "{}: removed {} cannot be restored",
                        show(&path),
                        entry.ident.kind.name()
                    )),
                },
            }
            continue;
        }
        let now = now.unwrap();
        match entry.ident.kind {
            Kind::File => {
                let unchanged = entry.ident.unchanged(&now.st) && entry.meta.mode == now.st.mode;
                let changed = if !unchanged {
                    true
                } else if racy(&entry.ident, started_ns) {
                    // Stat cannot rule out a same-tick write; compare.
                    !same_as_object(&mut rec, root, &entry.rel, entry, cancel)
                } else {
                    false
                };
                if changed && let Some(saved) = saved_of(entry) {
                    if let Some(obj) = saved.obj() {
                        referenced.push(obj.clone());
                    }
                    modified.push(Action::Write {
                        path,
                        ident: entry.ident,
                        saved: Some(saved),
                    });
                }
            }
            Kind::Symlink => {
                if now.target != entry.target {
                    if let Some(saved) = saved_of(entry) {
                        removed.push(Action::Unlink {
                            path: path.clone(),
                            saved,
                        });
                    }
                    created.push((
                        entry.rel.clone(),
                        Action::Create {
                            path,
                            ident: Ident::of(&now.st),
                        },
                    ));
                }
            }
            Kind::Dir => {
                let m = &entry.meta;
                let a = &now.meta;
                if m.mode != a.mode
                    || m.uid != a.uid
                    || m.gid != a.gid
                    || m.flags != a.flags
                    || m.xattrs != a.xattrs
                    || m.mtime != a.mtime
                {
                    modified.push(Action::Meta {
                        path,
                        kind: Kind::Dir,
                        before: m.clone(),
                        after: a.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    for rel in &order {
        let now = &after[rel];
        let prior = before_map.get(rel);
        if prior.is_some_and(|p| p.ident.kind == now.st.kind) {
            continue;
        }
        let path = abs(root, rel);
        let action = if now.st.kind == Kind::Dir {
            Action::Mkdir {
                path,
                ident: Ident::of(&now.st),
            }
        } else {
            Action::Create {
                path,
                ident: Ident::of(&now.st),
            }
        };
        created.push((rel.clone(), action));
    }

    // Removals children-first, creations parents-first, then content and
    // metadata changes. Undo runs this in reverse.
    let depth = |a: &Action| a.paths()[0].iter().filter(|&&b| b == b'/').count();
    removed.sort_by_key(|a| std::cmp::Reverse(depth(a)));
    created.sort_by_key(|(rel, _)| rel.iter().filter(|&&b| b == b'/').count());
    let metadata = modified
        .iter()
        .filter(|a| matches!(a, Action::Meta { .. }))
        .count();
    let summary_counts = (created.len(), modified.len() - metadata, removed.len());
    let mut actions = removed;
    actions.extend(created.into_iter().map(|(_, a)| a));
    actions.extend(modified);

    let mut budget = Budget::new(0, config.min_free, cancel);
    let mut records = Vec::new();
    if !actions.is_empty() {
        let finals: Vec<Vec<u8>> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Create { path, .. } | Action::Write { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        let n = actions.len() as u32;
        let op = rec.prepare(actions)?;
        rec.commit(op, n, None)?;
        for path in finals {
            let p = Path::new(OsStr::from_bytes(&path));
            let evidence = sys::split_parent(p)
                .and_then(|(parent, name)| Ok((sys::open_dir(parent)?, sys::os_cstr(name)?)))
                .and_then(|(dir, name)| {
                    preserve::capture_evidence(&mut rec.stores, &mut budget, dir.as_fd(), &name)
                })
                .unwrap_or(Evidence::Unknown);
            records.push(Record::Final { path, evidence });
        }
    }
    records.extend(notes.iter().map(|t| Record::Note { text: t.clone() }));
    records.push(Record::End {
        status,
        finished_ns: sys::now_ns(),
        interrupted,
    });
    rec.stores.sync_objects()?;
    rec.append(&records, true)?;

    // Before-images of unchanged files are not referenced by any action.
    for entry in &before {
        if let Some((obj, _)) = &entry.obj
            && !referenced.contains(obj)
            && let Some(path) = rec.stores.object_path(obj)
        {
            let _ = std::fs::remove_file(path);
        }
    }
    let after_entries: Vec<CatEntry> = order
        .iter()
        .map(|rel| {
            let a = &after[rel];
            CatEntry {
                rel: rel.clone(),
                ident: Ident::of(&a.st),
                meta: a.meta.clone(),
                nlink: a.st.nlink,
                obj: None,
                target: a.target.clone(),
            }
        })
        .collect();
    let _ = journal::write_catalog(&home.txn_dir(id).join("after.cat"), &after_entries);
    Ok(Summary {
        id,
        created: summary_counts.0,
        modified: summary_counts.1,
        removed: summary_counts.2,
        metadata,
        notes,
    })
}

fn same_as_object(
    rec: &mut Recorder,
    root: &Path,
    rel: &[u8],
    entry: &CatEntry,
    cancel: &AtomicBool,
) -> bool {
    let Some((obj, _)) = &entry.obj else {
        return false;
    };
    let path = PathBuf::from(OsStr::from_bytes(&abs(root, rel)));
    let (Ok(expected), Ok(current)) = (
        rec.stores.open_object(obj),
        rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ),
    ) else {
        return false;
    };
    sys::same_contents(current.as_fd(), expected.as_fd(), cancel).unwrap_or(false)
}
