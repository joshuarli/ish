//! Conditional undo and redo.
//!
//! A transaction's journal is folded into a [`Model`]: its actions, whether
//! each happened, and which replay runs moved each one back and forth. A run
//! turns the selected actions into steps (undo walks actions in reverse,
//! redo forward), checks every step against the current filesystem right
//! before mutating, and records a per-action outcome, so an interrupted or
//! partially conflicting run can simply be run again.
//!
//! Default replay never discards data it cannot account for: a step whose
//! expected state no longer matches is a conflict. `--force` first preserves
//! the conflicting current state and then proceeds. Every step that removes
//! or replaces something preserves it first, which is what makes redo, and
//! undo after redo, possible.

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::journal::{
    self, Action, Begin, Content, Displaced, Evidence, Ident, Meta, Record, Saved, Strength,
};
use crate::preserve::{self, Budget, Check, LinkMap, Need};
use crate::store::{self, Home, Liveness, Lock};
use crate::sys::{self, Kind};
use crate::txn::Recorder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Committed {
    Done,
    NotDone,
    /// Prepared, but no outcome was recorded (the writer was interrupted).
    Ambiguous,
}

#[derive(Clone, Debug)]
pub struct LastStep {
    pub run: u64,
    pub redo: bool,
    pub placed: Evidence,
    pub displaced: Displaced,
}

#[derive(Clone, Debug)]
pub struct ActionState {
    pub action: Action,
    pub committed: Committed,
    pub error: Option<String>,
    /// Whether the action's effect is currently in place, as far as replay
    /// records say.
    pub applied: bool,
    pub last: Option<LastStep>,
    pub conflict: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RunInfo {
    pub run: u64,
    pub redo: bool,
    pub force: bool,
    pub started_ns: u64,
    pub done: u32,
    pub conflicts: u32,
    pub finished: bool,
    pub interrupted: bool,
}

/// A transaction as recorded.
#[derive(Clone, Debug)]
pub struct Model {
    pub id: u64,
    pub begin: Begin,
    pub actions: Vec<ActionState>,
    pub finals: HashMap<Vec<u8>, Evidence>,
    pub end: Option<(i32, u64, bool)>,
    pub opaque: u32,
    pub notes: Vec<String>,
    pub checkpoint: Option<(u64, u64, u64)>,
    pub runs: Vec<RunInfo>,
    pub torn_bytes: u64,
}

impl Model {
    pub fn load(home: &Home, id: u64) -> io::Result<Model> {
        let scan = journal::read(&home.journal_path(id))?;
        Model::from_records(id, scan.records, scan.torn_bytes)
    }

    pub fn from_records(id: u64, records: Vec<Record>, torn_bytes: u64) -> io::Result<Model> {
        let mut iter = records.into_iter();
        let begin = match iter.next() {
            Some(Record::Begin(b)) => b,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "journal does not start with a begin record",
                ));
            }
        };
        let mut model = Model {
            id,
            begin,
            actions: Vec::new(),
            finals: HashMap::new(),
            end: None,
            opaque: 0,
            notes: Vec::new(),
            checkpoint: None,
            runs: Vec::new(),
            torn_bytes,
        };
        let mut ops: HashMap<u64, (usize, usize)> = HashMap::new();
        for record in iter {
            match record {
                Record::Begin(_) => {}
                Record::Prepare { op, actions } => {
                    let start = model.actions.len();
                    for action in actions {
                        model.actions.push(ActionState {
                            action,
                            committed: Committed::Ambiguous,
                            error: None,
                            applied: true,
                            last: None,
                            conflict: None,
                        });
                    }
                    ops.insert(op, (start, model.actions.len()));
                }
                Record::Commit { op, done, error } => {
                    if let Some(&(start, end)) = ops.get(&op) {
                        for (k, state) in model.actions[start..end].iter_mut().enumerate() {
                            if (k as u32) < done {
                                state.committed = Committed::Done;
                            } else {
                                state.committed = Committed::NotDone;
                                state.applied = false;
                                state.error = error.clone();
                            }
                        }
                    }
                }
                Record::Final { path, evidence } => {
                    model.finals.insert(path, evidence);
                }
                Record::Opaque { count } => model.opaque += count,
                Record::Note { text } => model.notes.push(text),
                Record::Checkpoint {
                    entries,
                    cloned,
                    copied_bytes,
                } => model.checkpoint = Some((entries, cloned, copied_bytes)),
                Record::End {
                    status,
                    finished_ns,
                    interrupted,
                } => model.end = Some((status, finished_ns, interrupted)),
                Record::RunBegin {
                    run,
                    redo,
                    force,
                    started_ns,
                    ..
                } => model.runs.push(RunInfo {
                    run,
                    redo,
                    force,
                    started_ns,
                    done: 0,
                    conflicts: 0,
                    finished: false,
                    interrupted: false,
                }),
                Record::StepPrepare { .. } => {}
                Record::StepDone {
                    run,
                    action,
                    redo,
                    placed,
                    displaced,
                } => {
                    if let Some(state) = model.actions.get_mut(action as usize) {
                        state.applied = redo;
                        state.conflict = None;
                        state.last = Some(LastStep {
                            run,
                            redo,
                            placed,
                            displaced,
                        });
                    }
                }
                Record::StepConflict { action, reason, .. } => {
                    if let Some(state) = model.actions.get_mut(action as usize) {
                        state.conflict = Some(reason);
                    }
                }
                Record::RunEnd {
                    run,
                    done,
                    conflicts,
                    interrupted,
                } => {
                    if let Some(info) = model.runs.iter_mut().find(|r| r.run == run) {
                        info.done = done;
                        info.conflicts = conflicts;
                        info.finished = true;
                        info.interrupted = interrupted;
                    }
                }
            }
        }
        Ok(model)
    }

    /// Actions that replay can act on (happened, or might have).
    fn relevant(&self, i: usize) -> bool {
        let s = &self.actions[i];
        s.committed != Committed::NotDone && !matches!(s.action, Action::Write { saved: None, .. })
    }

    pub fn undoable(&self) -> usize {
        (0..self.actions.len())
            .filter(|&i| self.relevant(i) && self.actions[i].applied)
            .count()
    }

    pub fn redoable(&self) -> usize {
        (0..self.actions.len())
            .filter(|&i| {
                self.relevant(i) && !self.actions[i].applied && self.actions[i].last.is_some()
            })
            .count()
    }

    pub fn is_scoped(&self) -> bool {
        self.begin.scope.is_some()
    }

    /// Latest replay run's start time, if any.
    pub fn last_run(&self) -> Option<&RunInfo> {
        self.runs.last()
    }

    /// The saved versions this transaction references.
    pub fn saved_versions(&self) -> impl Iterator<Item = &Saved> {
        self.actions.iter().flat_map(|s| {
            let mut v: Vec<&Saved> = s.action.saved().into_iter().collect();
            if let Some(LastStep {
                displaced: Displaced::Saved(d),
                ..
            }) = &s.last
            {
                v.push(d);
            }
            v
        })
    }

    pub fn has_linked(&self) -> bool {
        self.saved_versions()
            .any(|s| s.strength() == Some(Strength::Link))
    }

    /// Recorded state at `path` right after action `i`, before any replay.
    fn recorded_after(&self, i: usize, path: &[u8]) -> Evidence {
        for s in &self.actions[i + 1..] {
            if s.committed == Committed::NotDone {
                continue;
            }
            match &s.action {
                Action::Write { saved: None, .. } => continue,
                Action::Unlink { path: p, saved } if p == path => return saved.evidence(),
                Action::Rmdir { path: p, ident, .. } if p == path => {
                    return Evidence::Dir {
                        dev: ident.dev,
                        ino: ident.ino,
                    };
                }
                Action::Mkdir { path: p, .. } | Action::Create { path: p, .. } if p == path => {
                    return Evidence::Absent;
                }
                Action::Write {
                    path: p,
                    saved: Some(saved),
                    ..
                } if p == path => return saved.evidence(),
                Action::Rename { from, ident, .. } if from == path => {
                    return Evidence::Object {
                        dev: ident.dev,
                        ino: ident.ino,
                        kind: ident.kind,
                    };
                }
                Action::Rename { to, .. } if to == path => return Evidence::Absent,
                Action::Meta { path: p, .. } if p == path => return Evidence::Unknown,
                _ => {}
            }
        }
        if let Some(evidence) = self.finals.get(path) {
            return evidence.clone();
        }
        match &self.actions[i].action {
            Action::Mkdir { ident, .. } => Evidence::Dir {
                dev: ident.dev,
                ino: ident.ino,
            },
            Action::Create { ident, .. } => Evidence::Stat(*ident),
            Action::Rename { to, ident, .. } if to == path => Evidence::Object {
                dev: ident.dev,
                ino: ident.ino,
                kind: ident.kind,
            },
            Action::Meta { .. } | Action::Write { .. } => Evidence::Unknown,
            _ => Evidence::Absent,
        }
    }

    /// Expected current state at `path` for undoing action `i`.
    fn expect_for_undo(&self, i: usize, path: &[u8]) -> Evidence {
        match &self.actions[i].last {
            Some(last) if last.redo => last.placed.clone(),
            _ => self.recorded_after(i, path),
        }
    }
}

/// One mutation of a replay run.
#[derive(Clone, Debug)]
pub enum StepKind {
    /// Create `path` from a saved version or as a directory; it must be
    /// absent.
    Place { path: Vec<u8>, source: Source },
    /// Remove `path`, which must match `expect`.
    Remove {
        path: Vec<u8>,
        expect: Evidence,
        dir: bool,
    },
    /// Replace `path`, which must match `expect`, with `source`.
    Replace {
        path: Vec<u8>,
        expect: Evidence,
        source: Saved,
    },
    /// Move the object at `from` (matching `expect`) to the absent `to`.
    Move {
        from: Vec<u8>,
        to: Vec<u8>,
        expect: Evidence,
    },
    /// Apply metadata to the existing `path`.
    SetMeta { path: Vec<u8>, meta: Meta },
}

#[derive(Clone, Debug)]
pub enum Source {
    Saved(Saved),
    Dir(Meta),
}

#[derive(Clone, Debug)]
pub struct Step {
    pub action: u32,
    pub kind: StepKind,
}

impl Step {
    pub fn describe(&self) -> String {
        let s = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
        match &self.kind {
            StepKind::Place {
                path,
                source: Source::Dir(_),
            } => format!("recreate directory {}", s(path)),
            StepKind::Place { path, .. } => format!("restore {}", s(path)),
            StepKind::Remove {
                path, dir: true, ..
            } => format!("remove directory {}", s(path)),
            StepKind::Remove { path, .. } => format!("remove {}", s(path)),
            StepKind::Replace { path, .. } => format!("restore contents of {}", s(path)),
            StepKind::Move { from, to, .. } => format!("move {} back to {}", s(from), s(to)),
            StepKind::SetMeta { path, .. } => format!("restore metadata of {}", s(path)),
        }
    }
}

fn undo_step(model: &Model, i: usize) -> Option<StepKind> {
    let state = &model.actions[i];
    Some(match &state.action {
        Action::Unlink { path, saved } => StepKind::Place {
            path: path.clone(),
            source: Source::Saved(saved.clone()),
        },
        Action::Rmdir { path, meta, .. } => StepKind::Place {
            path: path.clone(),
            source: Source::Dir(meta.clone()),
        },
        Action::Mkdir { path, .. } => StepKind::Remove {
            path: path.clone(),
            expect: model.expect_for_undo(i, path),
            dir: true,
        },
        Action::Create { path, .. } => StepKind::Remove {
            path: path.clone(),
            expect: model.expect_for_undo(i, path),
            dir: false,
        },
        Action::Write {
            path,
            saved: Some(saved),
            ..
        } => StepKind::Replace {
            path: path.clone(),
            expect: model.expect_for_undo(i, path),
            source: saved.clone(),
        },
        Action::Write { saved: None, .. } => return None,
        Action::Rename { from, to, .. } => StepKind::Move {
            from: to.clone(),
            to: from.clone(),
            expect: model.expect_for_undo(i, to),
        },
        Action::Meta { path, before, .. } => StepKind::SetMeta {
            path: path.clone(),
            meta: before.clone(),
        },
    })
}

fn redo_step(model: &Model, i: usize) -> Result<Option<StepKind>, String> {
    let state = &model.actions[i];
    let Some(last) = &state.last else {
        return Ok(None);
    };
    let displaced_saved = || match &last.displaced {
        Displaced::Saved(s) => Ok(s.clone()),
        _ => Err("the state to redo was not preserved".to_string()),
    };
    Ok(Some(match &state.action {
        Action::Unlink { path, .. } => StepKind::Remove {
            path: path.clone(),
            expect: last.placed.clone(),
            dir: false,
        },
        Action::Rmdir { path, .. } => StepKind::Remove {
            path: path.clone(),
            expect: last.placed.clone(),
            dir: true,
        },
        Action::Mkdir { path, .. } => StepKind::Place {
            path: path.clone(),
            source: Source::Dir(match &last.displaced {
                Displaced::Dir(m) => m.clone(),
                _ => Meta {
                    mode: 0o755,
                    ..Meta::default()
                },
            }),
        },
        Action::Create { path, .. } => StepKind::Place {
            path: path.clone(),
            source: Source::Saved(displaced_saved()?),
        },
        Action::Write { path, .. } => StepKind::Replace {
            path: path.clone(),
            expect: last.placed.clone(),
            source: displaced_saved()?,
        },
        Action::Rename { from, to, .. } => StepKind::Move {
            from: from.clone(),
            to: to.clone(),
            expect: last.placed.clone(),
        },
        Action::Meta { path, after, .. } => StepKind::SetMeta {
            path: path.clone(),
            meta: match &last.displaced {
                Displaced::Meta(m) => m.clone(),
                _ => after.clone(),
            },
        },
    }))
}

fn under(path: &[u8], root: &[u8]) -> bool {
    path == root
        || (path.starts_with(root) && (root.ends_with(b"/") || path.get(root.len()) == Some(&b'/')))
}

/// Actions selected by `--only`, plus the directory and rename actions they
/// depend on: ancestors that must be recreated, and moves of an ancestor.
fn select(model: &Model, only: &[Vec<u8>]) -> Vec<bool> {
    let n = model.actions.len();
    if only.is_empty() {
        return vec![true; n];
    }
    let mut selected: Vec<bool> = (0..n)
        .map(|i| {
            model.actions[i]
                .action
                .paths()
                .iter()
                .any(|p| only.iter().any(|o| under(p, o)))
        })
        .collect();
    loop {
        let needed: Vec<Vec<u8>> = (0..n)
            .filter(|&i| selected[i])
            .flat_map(|i| {
                model.actions[i]
                    .action
                    .paths()
                    .into_iter()
                    .map(<[u8]>::to_vec)
            })
            .collect();
        let mut changed = false;
        for j in 0..n {
            if selected[j] {
                continue;
            }
            let dependency = match &model.actions[j].action {
                Action::Rmdir { path, .. } | Action::Mkdir { path, .. } => {
                    needed.iter().any(|p| p != path && under(p, path))
                }
                Action::Rename { from, to, .. } => needed
                    .iter()
                    .any(|p| (p != from && under(p, from)) || (p != to && under(p, to))),
                _ => false,
            };
            if dependency {
                selected[j] = true;
                changed = true;
            }
        }
        if !changed {
            return selected;
        }
    }
}

/// Plan the steps of an undo (`redo == false`) or redo run.
pub fn plan(model: &Model, redo: bool, only: &[Vec<u8>]) -> (Vec<Step>, Vec<(u32, String)>) {
    let selected = select(model, only);
    let mut steps = Vec::new();
    let mut unavailable = Vec::new();
    let order: Vec<usize> = if redo {
        (0..model.actions.len()).collect()
    } else {
        (0..model.actions.len()).rev().collect()
    };
    for i in order {
        if !selected[i] || !model.relevant(i) {
            continue;
        }
        let state = &model.actions[i];
        if redo {
            if state.applied {
                continue;
            }
            match redo_step(model, i) {
                Ok(Some(kind)) => steps.push(Step {
                    action: i as u32,
                    kind,
                }),
                Ok(None) => {}
                Err(reason) => unavailable.push((i as u32, reason)),
            }
        } else if state.applied
            && let Some(kind) = undo_step(model, i)
        {
            steps.push(Step {
                action: i as u32,
                kind,
            });
        }
    }
    (steps, unavailable)
}

/// Outcome of one step.
#[derive(Clone, Debug)]
pub enum Outcome {
    Done {
        placed: Evidence,
        displaced: Displaced,
    },
    /// The goal state was already in place.
    Already {
        placed: Evidence,
    },
    Conflict(String),
    Failed(String),
}

#[derive(Default, Debug)]
pub struct Report {
    pub run: u64,
    pub done: u32,
    pub already: u32,
    pub conflicts: Vec<(String, String)>,
    pub failures: Vec<(String, String)>,
    pub planned: Vec<String>,
    pub notes: Vec<String>,
    pub interrupted: bool,
}

pub struct Options<'a> {
    pub redo: bool,
    pub force: bool,
    pub dry_run: bool,
    pub only: Vec<Vec<u8>>,
    pub copy_limit: u64,
    pub min_free: u64,
    pub cancel: &'a AtomicBool,
}

fn open_parent(path: &[u8]) -> io::Result<(OwnedFd, CString)> {
    let p = Path::new(OsStr::from_bytes(path));
    let (parent, name) = sys::split_parent(p)?;
    Ok((sys::open_dir(parent)?, sys::os_cstr(name)?))
}

/// A hard-link group restored during a run.
struct LinkGroup {
    /// Original device and inode.
    key: (u64, u64),
    /// Names restored so far.
    restored: u64,
    /// Original link count.
    nlink: u64,
    first: Vec<u8>,
}

/// Replay state shared by the steps of a run.
struct Runner<'b> {
    rec: Recorder,
    budget: Budget<'b>,
    force: bool,
    dry: bool,
    links: LinkMap,
    /// Directories placed in this run, whose metadata is applied after
    /// their children.
    deferred: Vec<(Vec<u8>, Meta)>,
    /// What earlier steps of this run left at each path. A later step's
    /// expectation for that path comes from here: replay itself changes
    /// object identities (a restored file is a new clone), and the recorded
    /// evidence predates those changes.
    placed_at: HashMap<Vec<u8>, Evidence>,
    /// Hard-link groups restored in this run.
    groups: Vec<LinkGroup>,
    /// Dry-run view of paths created and removed by earlier steps.
    sim_present: HashSet<Vec<u8>>,
    sim_absent: HashSet<Vec<u8>>,
}

impl Runner<'_> {
    fn sim_state(&self, path: &[u8]) -> Option<bool> {
        if self.sim_present.contains(path) {
            return Some(true);
        }
        if self.sim_absent.iter().any(|a| under(path, a)) {
            return Some(false);
        }
        None
    }

    fn check(
        &mut self,
        dir: BorrowedFd<'_>,
        name: &CString,
        path: &[u8],
        evidence: &Evidence,
    ) -> io::Result<Check> {
        if self.dry
            && let Some(present) = self.sim_state(path)
        {
            // Earlier simulated steps put exactly the recorded state here.
            return Ok(match (present, evidence) {
                (false, Evidence::Absent) | (true, _) => Check::Match,
                (false, _) => Check::Mismatch("it no longer exists".into()),
            });
        }
        let result = preserve::check_evidence(
            &mut self.rec.stores,
            self.budget.cancel,
            dir,
            name,
            evidence,
        )?;
        if let (Check::Mismatch(_), Evidence::Stat(ident)) = (&result, evidence)
            && ident.kind == Kind::File
            && self.same_as_frozen_version(dir, name, ident)
        {
            return Ok(Check::Match);
        }
        Ok(result)
    }

    /// Stat evidence names an exact object state. Without clones it cannot
    /// be compared by contents, and replay of another transaction may have
    /// put the same contents back as a new object. If any transaction kept
    /// a frozen version captured from exactly that state, compare the
    /// current contents with it instead.
    fn same_as_frozen_version(
        &mut self,
        dir: BorrowedFd<'_>,
        name: &CString,
        ident: &Ident,
    ) -> bool {
        let home = self.rec.home.clone();
        let Ok(ids) = home.txn_ids() else {
            return false;
        };
        for id in ids.into_iter().rev() {
            let Ok(model) = Model::load(&home, id) else {
                continue;
            };
            let frozen = model
                .saved_versions()
                .filter(|s| s.ident == *ident && s.strength().is_some_and(Strength::frozen))
                .find_map(|s| s.obj().cloned())
                .or_else(|| {
                    model.finals.values().find_map(|e| match e {
                        Evidence::Frozen { ident: i, obj } if i == ident => Some(obj.clone()),
                        _ => None,
                    })
                });
            let Some(obj) = frozen else {
                continue;
            };
            let Ok(mut stores) = crate::store::Stores::new(home.clone(), id) else {
                return false;
            };
            let (Ok(expected), Ok(current)) =
                (stores.open_object(&obj), sys::open_read_at(dir, name))
            else {
                return false;
            };
            return sys::same_contents(current.as_fd(), expected.as_fd(), self.budget.cancel)
                .unwrap_or(false);
        }
        false
    }

    fn exists(
        &self,
        dir: BorrowedFd<'_>,
        name: &CString,
        path: &[u8],
    ) -> io::Result<Option<sys::Stat>> {
        if self.dry
            && let Some(present) = self.sim_state(path)
        {
            return Ok(present.then_some(sys::Stat {
                dev: 0,
                ino: 0,
                kind: Kind::Unknown,
                mode: 0,
                uid: 0,
                gid: 0,
                nlink: 1,
                size: 0,
                blocks: 0,
                atime: (0, 0),
                mtime: (0, 0),
                ctime: (0, 0),
                flags: 0,
            }));
        }
        match sys::lstat_at(dir, name) {
            Ok(st) => Ok(Some(st)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn parent(&self, path: &[u8]) -> Result<(OwnedFd, CString), Box<Outcome>> {
        match open_parent(path) {
            Ok(v) => Ok(v),
            Err(e) if self.dry => {
                let parent = &path[..path.iter().rposition(|&b| b == b'/').unwrap_or(0)];
                if self.sim_present.contains(parent) {
                    // The parent is created by an earlier simulated step;
                    // stand in with the root directory.
                    let root = sys::open_dir(Path::new("/"))
                        .map_err(|e| Box::new(Outcome::Failed(sys::describe_error(&e))))?;
                    let name = sys::os_cstr(
                        Path::new(OsStr::from_bytes(path))
                            .file_name()
                            .unwrap_or_default(),
                    )
                    .map_err(|e| Box::new(Outcome::Failed(sys::describe_error(&e))))?;
                    return Ok((root, name));
                }
                Err(Box::new(parent_error(e)))
            }
            Err(e) => Err(Box::new(parent_error(e))),
        }
    }

    /// Preserve whatever is at `name` before it is removed or replaced.
    fn displace(
        &mut self,
        dir: BorrowedFd<'_>,
        name: &CString,
        path: &[u8],
        st: &sys::Stat,
        matched: &Evidence,
    ) -> Result<Displaced, String> {
        if self.dry {
            return Ok(Displaced::None);
        }
        if st.kind == Kind::Dir {
            let fd = sys::open_dir_at(dir, name).map_err(|e| e.to_string())?;
            return Ok(Displaced::Dir(preserve::dir_meta(fd.as_fd(), st)));
        }
        match preserve::preserve(
            &mut self.rec.stores,
            &mut self.budget,
            dir,
            name,
            path,
            st,
            Need::Unlink,
        ) {
            Ok(saved) => Ok(Displaced::Saved(saved)),
            // Contents verified equal to a frozen object can be referenced
            // instead of preserved again.
            Err(e) => match matched {
                Evidence::Frozen { obj, .. } => Ok(Displaced::Saved(Saved {
                    content: Content::File {
                        obj: obj.clone(),
                        strength: Strength::Clone,
                    },
                    ident: Ident::of(st),
                    meta: Meta::of(st),
                    nlink: st.nlink,
                    copied: 0,
                })),
                _ => Err(format!("cannot preserve the current version first: {e}")),
            },
        }
    }

    /// Materialize `saved` into this step's temporary name in `dir`,
    /// rejoining hard links restored earlier in this run. The name depends
    /// only on the transaction and action, so a restarted run replaces a
    /// temporary left by an interrupted one.
    fn materialize(
        &mut self,
        saved: &Saved,
        dir: BorrowedFd<'_>,
        action: u32,
    ) -> io::Result<CString> {
        let tmp = CString::new(format!(".ish-undo-{}-{action}", self.rec.id)).unwrap();
        if saved.nlink > 1
            && let Some(first) = self
                .links
                .get((saved.ident.dev, saved.ident.ino))
                .map(<[u8]>::to_vec)
            && let Ok((fdir, fname)) = open_parent(&first)
        {
            let _ = sys::unlink_at(dir, &tmp);
            if sys::link_at(fdir.as_fd(), &fname, dir, &tmp).is_ok() {
                return Ok(tmp);
            }
        }
        preserve::materialize(&mut self.rec.stores, &mut self.budget, saved, dir, &tmp)?;
        Ok(tmp)
    }

    fn placed_evidence(
        &self,
        dir: BorrowedFd<'_>,
        name: &CString,
        source: Option<&Saved>,
    ) -> Evidence {
        let Ok(st) = sys::lstat_at(dir, name) else {
            return Evidence::Unknown;
        };
        match (st.kind, source) {
            (Kind::Dir, _) => Evidence::Dir {
                dev: st.dev,
                ino: st.ino,
            },
            (
                Kind::Symlink,
                Some(Saved {
                    content: Content::Symlink { target },
                    ..
                }),
            ) => Evidence::Symlink {
                target: target.clone(),
            },
            (
                Kind::File,
                Some(Saved {
                    content: Content::File { obj, strength },
                    ..
                }),
            ) if strength.frozen() => Evidence::Frozen {
                ident: Ident::of(&st),
                obj: obj.clone(),
            },
            // A relinked retained inode: record its full state as placed, so
            // later edits through this name are detected.
            _ => Evidence::Stat(Ident::of(&st)),
        }
    }

    fn step(&mut self, step: &Step) -> Outcome {
        let action = step.action;
        let expect = |path: &[u8], recorded: &Evidence| {
            self.placed_at
                .get(path)
                .cloned()
                .unwrap_or_else(|| recorded.clone())
        };
        let outcome = match &step.kind {
            StepKind::Place { path, source } => self.place(path, source, action),
            StepKind::Remove {
                path,
                expect: e,
                dir,
            } => {
                let e = expect(path, e);
                self.remove(path, &e, *dir)
            }
            StepKind::Replace {
                path,
                expect: e,
                source,
            } => {
                let e = expect(path, e);
                self.replace(path, &e, source, action)
            }
            StepKind::Move {
                from,
                to,
                expect: e,
            } => {
                let e = expect(from, e);
                self.move_back(from, to, &e)
            }
            StepKind::SetMeta { path, meta } => self.set_meta(path, meta),
        };
        if let Outcome::Done { placed, .. } | Outcome::Already { placed } = &outcome {
            match &step.kind {
                StepKind::Place { path, .. }
                | StepKind::Replace { path, .. }
                | StepKind::Remove { path, .. } => {
                    self.placed_at.insert(path.clone(), placed.clone());
                }
                StepKind::Move { from, to, .. } => {
                    self.placed_at.insert(from.clone(), Evidence::Absent);
                    self.placed_at.insert(to.clone(), placed.clone());
                }
                StepKind::SetMeta { .. } => {}
            }
        }
        outcome
    }

    fn place(&mut self, path: &[u8], source: &Source, action: u32) -> Outcome {
        let (dir, name) = match self.parent(path) {
            Ok(v) => v,
            Err(o) => return *o,
        };
        let current = match self.exists(dir.as_fd(), &name, path) {
            Ok(c) => c,
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        let mut displaced = Displaced::None;
        let mut replace = false;
        if let Some(st) = current {
            // Already in the goal state (a restarted run, or restored by
            // hand): accept it.
            let goal = match source {
                Source::Dir(_) => {
                    if st.kind == Kind::Dir {
                        self.deferred_meta(path, source);
                        return Outcome::Already {
                            placed: Evidence::Dir {
                                dev: st.dev,
                                ino: st.ino,
                            },
                        };
                    }
                    None
                }
                Source::Saved(saved) => Some(saved.evidence()),
            };
            if let Some(goal) = goal
                && !self.dry
                && matches!(
                    self.check(dir.as_fd(), &name, path, &goal),
                    Ok(Check::Match)
                )
            {
                return Outcome::Already {
                    placed: self.placed_evidence(dir.as_fd(), &name, source_saved(source)),
                };
            }
            if !self.force {
                return Outcome::Conflict("something newer is in the way".into());
            }
            if st.kind == Kind::Dir {
                return Outcome::Conflict(
                    "a directory is in the way; --force does not replace directories".into(),
                );
            }
            displaced = match self.displace(dir.as_fd(), &name, path, &st, &Evidence::Unknown) {
                Ok(d) => d,
                Err(e) => return Outcome::Conflict(e),
            };
            replace = true;
        }
        if self.dry {
            self.sim_present.insert(path.to_vec());
            self.sim_absent.remove(path);
            return Outcome::Done {
                placed: Evidence::Unknown,
                displaced,
            };
        }
        match source {
            Source::Dir(meta) => {
                if let Err(e) = sys::mkdir_at(dir.as_fd(), &name, 0o700) {
                    return Outcome::Failed(sys::describe_error(&e));
                }
                self.deferred.push((path.to_vec(), meta.clone()));
            }
            Source::Saved(saved) => {
                let tmp = match self.materialize(saved, dir.as_fd(), action) {
                    Ok(tmp) => tmp,
                    Err(e) => {
                        return Outcome::Failed(format!("cannot restore the saved version: {e}"));
                    }
                };
                let publish = if replace {
                    sys::rename_replace(dir.as_fd(), &tmp, dir.as_fd(), &name)
                } else {
                    sys::rename_noreplace(dir.as_fd(), &tmp, dir.as_fd(), &name)
                };
                if let Err(e) = publish {
                    let _ = sys::unlink_at(dir.as_fd(), &tmp);
                    return if e.kind() == io::ErrorKind::AlreadyExists {
                        Outcome::Conflict("something newer appeared there".into())
                    } else {
                        Outcome::Failed(sys::describe_error(&e))
                    };
                }
                if saved.nlink > 1 {
                    let key = (saved.ident.dev, saved.ident.ino);
                    if self.links.get(key).is_none() {
                        self.links.insert(key, path.to_vec());
                    }
                    match self.groups.iter_mut().find(|g| g.key == key) {
                        Some(group) => group.restored += 1,
                        None => self.groups.push(LinkGroup {
                            key,
                            restored: 1,
                            nlink: saved.nlink,
                            first: path.to_vec(),
                        }),
                    }
                }
            }
        }
        Outcome::Done {
            placed: self.placed_evidence(dir.as_fd(), &name, source_saved(source)),
            displaced,
        }
    }

    fn deferred_meta(&mut self, path: &[u8], source: &Source) {
        if let Source::Dir(meta) = source
            && !self.dry
        {
            self.deferred.push((path.to_vec(), meta.clone()));
        }
    }

    fn remove(&mut self, path: &[u8], expect: &Evidence, dir_kind: bool) -> Outcome {
        let (dir, name) = match self.parent(path) {
            Ok(v) => v,
            // A missing parent means the path is already gone; any other
            // error (permissions, I/O) says nothing about it.
            Err(o) if matches!(*o, Outcome::Conflict(_)) => {
                return Outcome::Already {
                    placed: Evidence::Absent,
                };
            }
            Err(o) => return *o,
        };
        let current = match self.exists(dir.as_fd(), &name, path) {
            Ok(Some(st)) => st,
            Ok(None) => {
                return Outcome::Already {
                    placed: Evidence::Absent,
                };
            }
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        match self.check(dir.as_fd(), &name, path, expect) {
            Ok(Check::Match) => {}
            Ok(Check::Mismatch(why)) if !self.force => return Outcome::Conflict(why),
            Ok(Check::Mismatch(_)) => {}
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        }
        if self.dry {
            self.sim_absent.insert(path.to_vec());
            self.sim_present.remove(path);
            return Outcome::Done {
                placed: Evidence::Absent,
                displaced: Displaced::None,
            };
        }
        let displaced = match self.displace(dir.as_fd(), &name, path, &current, expect) {
            Ok(d) => d,
            Err(e) => return Outcome::Conflict(e),
        };
        let result = if current.kind == Kind::Dir || dir_kind {
            sys::rmdir_at(dir.as_fd(), &name)
        } else {
            sys::unlink_at(dir.as_fd(), &name)
        };
        match result {
            Ok(()) => Outcome::Done {
                placed: Evidence::Absent,
                displaced,
            },
            Err(e)
                if e.raw_os_error() == Some(libc::ENOTEMPTY)
                    || e.raw_os_error() == Some(libc::EEXIST) =>
            {
                Outcome::Conflict("the directory contains newer data".into())
            }
            Err(e) => Outcome::Failed(sys::describe_error(&e)),
        }
    }

    fn replace(&mut self, path: &[u8], expect: &Evidence, source: &Saved, action: u32) -> Outcome {
        let (dir, name) = match self.parent(path) {
            Ok(v) => v,
            Err(o) => return *o,
        };
        let current = match self.exists(dir.as_fd(), &name, path) {
            Ok(c) => c,
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        let Some(current) = current else {
            if self.force {
                return self.place(path, &Source::Saved(source.clone()), action);
            }
            return Outcome::Conflict("it no longer exists".into());
        };
        if current.kind == Kind::Dir {
            return Outcome::Conflict("it is now a directory".into());
        }
        if !self.dry
            && matches!(
                self.check(dir.as_fd(), &name, path, &source.evidence()),
                Ok(Check::Match)
            )
        {
            return Outcome::Already {
                placed: self.placed_evidence(dir.as_fd(), &name, Some(source)),
            };
        }
        match self.check(dir.as_fd(), &name, path, expect) {
            Ok(Check::Match) => {}
            Ok(Check::Mismatch(why)) if !self.force => return Outcome::Conflict(why),
            Ok(Check::Mismatch(_)) => {}
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        }
        if self.dry {
            return Outcome::Done {
                placed: Evidence::Unknown,
                displaced: Displaced::None,
            };
        }
        let displaced = match self.displace(dir.as_fd(), &name, path, &current, expect) {
            Ok(d) => d,
            Err(e) => return Outcome::Conflict(e),
        };
        // Preserving by hard link changes the ctime of the object itself, so
        // the baseline for revalidation is taken after preservation.
        let current = match sys::lstat_at(dir.as_fd(), &name) {
            Ok(now)
                if now.same_object(&current)
                    && now.size == current.size
                    && now.mtime == current.mtime =>
            {
                now
            }
            _ => return Outcome::Conflict("it changed while being replaced".into()),
        };
        let tmp = match self.materialize(source, dir.as_fd(), action) {
            Ok(tmp) => tmp,
            Err(e) => return Outcome::Failed(format!("cannot restore the saved version: {e}")),
        };
        // Revalidate right before the atomic replacement.
        match sys::lstat_at(dir.as_fd(), &name) {
            Ok(now) if now.unchanged_since(&current) => {}
            _ => {
                let _ = sys::unlink_at(dir.as_fd(), &tmp);
                return Outcome::Conflict("it changed while being replaced".into());
            }
        }
        if let Err(e) = sys::rename_replace(dir.as_fd(), &tmp, dir.as_fd(), &name) {
            let _ = sys::unlink_at(dir.as_fd(), &tmp);
            return Outcome::Failed(sys::describe_error(&e));
        }
        if current.nlink > 1 {
            self.budget.notes.push(format!(
                "{}: restored as a new file; its other hard links still point to the newer version",
                String::from_utf8_lossy(path)
            ));
        }
        Outcome::Done {
            placed: self.placed_evidence(dir.as_fd(), &name, Some(source)),
            displaced,
        }
    }

    fn move_back(&mut self, from: &[u8], to: &[u8], expect: &Evidence) -> Outcome {
        let (fdir, fname) = match self.parent(from) {
            Ok(v) => v,
            Err(o) => return *o,
        };
        let (tdir, tname) = match self.parent(to) {
            Ok(v) => v,
            Err(o) => return *o,
        };
        let source = match self.exists(fdir.as_fd(), &fname, from) {
            Ok(s) => s,
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        let Some(source) = source else {
            // Already moved (a restarted run)?
            if !self.dry
                && let Ok(Check::Match) = preserve::check_evidence(
                    &mut self.rec.stores,
                    self.budget.cancel,
                    tdir.as_fd(),
                    &tname,
                    expect,
                )
            {
                return Outcome::Already {
                    placed: expect.clone(),
                };
            }
            return Outcome::Conflict("it is no longer there".into());
        };
        match self.check(fdir.as_fd(), &fname, from, expect) {
            Ok(Check::Match) => {}
            Ok(Check::Mismatch(why)) if !self.force => return Outcome::Conflict(why),
            Ok(Check::Mismatch(_)) => {}
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        }
        let occupant = match self.exists(tdir.as_fd(), &tname, to) {
            Ok(o) => o,
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        let mut displaced = Displaced::None;
        if let Some(occ) = &occupant {
            if !self.force {
                return Outcome::Conflict(format!(
                    "{} is occupied by newer data",
                    String::from_utf8_lossy(to)
                ));
            }
            if occ.kind == Kind::Dir {
                return Outcome::Conflict(
                    "a directory is in the way; --force does not replace directories".into(),
                );
            }
            displaced = match self.displace(tdir.as_fd(), &tname, to, occ, &Evidence::Unknown) {
                Ok(d) => d,
                Err(e) => return Outcome::Conflict(e),
            };
        }
        if self.dry {
            self.sim_absent.insert(from.to_vec());
            self.sim_present.insert(to.to_vec());
            return Outcome::Done {
                placed: Evidence::Unknown,
                displaced,
            };
        }
        let result = if occupant.is_some() {
            sys::rename_replace(fdir.as_fd(), &fname, tdir.as_fd(), &tname)
        } else {
            sys::rename_noreplace(fdir.as_fd(), &fname, tdir.as_fd(), &tname)
        };
        match result {
            Ok(()) => Outcome::Done {
                placed: Evidence::Object {
                    dev: source.dev,
                    ino: source.ino,
                    kind: source.kind,
                },
                displaced,
            },
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Outcome::Conflict("something newer appeared at the destination".into())
            }
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                Outcome::Conflict("the paths are now on different filesystems".into())
            }
            Err(e) => Outcome::Failed(sys::describe_error(&e)),
        }
    }

    fn set_meta(&mut self, path: &[u8], meta: &Meta) -> Outcome {
        let (dir, name) = match self.parent(path) {
            Ok(v) => v,
            Err(o) => return *o,
        };
        let st = match self.exists(dir.as_fd(), &name, path) {
            Ok(Some(st)) => st,
            Ok(None) => return Outcome::Conflict("it no longer exists".into()),
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        if self.dry {
            return Outcome::Done {
                placed: Evidence::Unknown,
                displaced: Displaced::None,
            };
        }
        let placed = Evidence::Object {
            dev: st.dev,
            ino: st.ino,
            kind: st.kind,
        };
        match st.kind {
            Kind::Dir => {
                let fd = match sys::open_dir_at(dir.as_fd(), &name) {
                    Ok(fd) => fd,
                    Err(e) => return Outcome::Failed(sys::describe_error(&e)),
                };
                let before = preserve::dir_meta(fd.as_fd(), &st);
                preserve::apply_dir_meta(fd.as_fd(), meta, &mut self.budget.notes);
                Outcome::Done {
                    placed,
                    displaced: Displaced::Meta(before),
                }
            }
            Kind::Symlink => {
                let _ = sys::set_symlink_times(dir.as_fd(), &name, meta.atime, meta.mtime);
                Outcome::Done {
                    placed,
                    displaced: Displaced::Meta(Meta::of(&st)),
                }
            }
            _ => match sys::open_read_at(dir.as_fd(), &name) {
                Ok(fd) => {
                    preserve::apply_file_meta(fd.as_fd(), meta, &mut self.budget.notes);
                    Outcome::Done {
                        placed,
                        displaced: Displaced::Meta(Meta::of(&st)),
                    }
                }
                Err(e) => Outcome::Failed(sys::describe_error(&e)),
            },
        }
    }

    /// Apply metadata of directories placed in this run, deepest first.
    fn finish_dirs(&mut self) {
        let mut deferred = std::mem::take(&mut self.deferred);
        deferred.sort_by_key(|(p, _)| std::cmp::Reverse(p.iter().filter(|&&b| b == b'/').count()));
        for (path, meta) in deferred {
            let result =
                open_parent(&path).and_then(|(dir, name)| sys::open_dir_at(dir.as_fd(), &name));
            match result {
                Ok(fd) => preserve::apply_dir_meta(fd.as_fd(), &meta, &mut self.budget.notes),
                Err(e) => self.budget.notes.push(format!(
                    "{}: directory metadata not restored: {e}",
                    String::from_utf8_lossy(&path)
                )),
            }
        }
    }
}

fn source_saved(source: &Source) -> Option<&Saved> {
    match source {
        Source::Saved(s) => Some(s),
        Source::Dir(_) => None,
    }
}

fn parent_error(e: io::Error) -> Outcome {
    if e.kind() == io::ErrorKind::NotFound {
        Outcome::Conflict("its parent directory is missing".into())
    } else {
        Outcome::Failed(sys::describe_error(&e))
    }
}

/// State of a transaction for selection and display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Active,
    Completed,
    Interrupted,
}

pub fn lifecycle(home: &Home, model: &Model) -> Lifecycle {
    if model.end.is_some() {
        return Lifecycle::Completed;
    }
    match store::session_liveness(home, model.begin.session, model.begin.shell_pid) {
        Liveness::Alive => Lifecycle::Active,
        Liveness::Dead => Lifecycle::Interrupted,
    }
}

/// Run an undo or redo of transaction `id`.
pub fn run(home: &Home, id: u64, opts: &Options<'_>) -> io::Result<Report> {
    let dir = home.txn_dir(id);
    let _lock = match Lock::try_acquire(&dir.join("replay.lock"))? {
        Some(lock) => lock,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("transaction {id} is being replayed by another shell"),
            ));
        }
    };
    let mut model = Model::load(home, id)?;
    if model.is_scoped() && model.end.is_none() && lifecycle(home, &model) == Lifecycle::Interrupted
    {
        // The program's shell died before the after-checkpoint; take it now.
        crate::scope::finalize_interrupted(home, id, opts.cancel)?;
        model = Model::load(home, id)?;
    }
    let (steps, unavailable) = plan(&model, opts.redo, &opts.only);
    let mut report = Report::default();
    for (action, reason) in &unavailable {
        report.conflicts.push((
            describe_action(&model.actions[*action as usize].action),
            reason.clone(),
        ));
    }
    let mut runner = Runner {
        rec: Recorder::open(home, id)?,
        budget: Budget::new(opts.copy_limit, opts.min_free, opts.cancel),
        force: opts.force,
        dry: opts.dry_run,
        links: LinkMap::default(),
        deferred: Vec::new(),
        placed_at: HashMap::new(),
        groups: Vec::new(),
        sim_present: HashSet::new(),
        sim_absent: HashSet::new(),
    };
    let run_id = model.runs.iter().map(|r| r.run).max().unwrap_or(0) + 1;
    report.run = run_id;
    if !opts.dry_run {
        runner.rec.append(
            &[Record::RunBegin {
                run: run_id,
                redo: opts.redo,
                force: opts.force,
                only: opts.only.clone(),
                started_ns: sys::now_ns(),
            }],
            true,
        )?;
    }
    for step in &steps {
        if opts.cancel.load(Ordering::Relaxed) {
            report.interrupted = true;
            break;
        }
        let label = step.describe();
        let outcome = runner.step(step);
        if opts.dry_run {
            match outcome {
                Outcome::Done { .. } => report.planned.push(label),
                Outcome::Already { .. } => report.already += 1,
                Outcome::Conflict(why) => report.conflicts.push((label, why)),
                Outcome::Failed(why) => report.failures.push((label, why)),
            }
            continue;
        }
        let record = match outcome {
            Outcome::Done { placed, displaced } => {
                report.done += 1;
                Record::StepDone {
                    run: run_id,
                    action: step.action,
                    redo: opts.redo,
                    placed,
                    displaced,
                }
            }
            Outcome::Already { placed } => {
                report.already += 1;
                Record::StepDone {
                    run: run_id,
                    action: step.action,
                    redo: opts.redo,
                    placed,
                    displaced: Displaced::None,
                }
            }
            Outcome::Conflict(why) => {
                report.conflicts.push((label, why.clone()));
                Record::StepConflict {
                    run: run_id,
                    action: step.action,
                    reason: why,
                }
            }
            Outcome::Failed(why) => {
                report.failures.push((label, why.clone()));
                Record::StepConflict {
                    run: run_id,
                    action: step.action,
                    reason: why,
                }
            }
        };
        runner.rec.stores.sync_objects()?;
        runner.rec.append(&[record], false)?;
        crate::fault::check("replay-step")?;
    }
    for LinkGroup {
        restored,
        nlink,
        first: path,
        ..
    } in &runner.groups
    {
        if restored < nlink {
            runner.budget.notes.push(format!(
                "{}: had {nlink} hard links; {restored} restored together, other links (outside this change) were not rejoined",
                String::from_utf8_lossy(path)
            ));
        }
    }
    if !opts.dry_run {
        runner.finish_dirs();
        runner.rec.stores.sync_objects()?;
        let mut tail: Vec<Record> = runner
            .budget
            .notes
            .iter()
            .map(|t| Record::Note { text: t.clone() })
            .collect();
        tail.push(Record::RunEnd {
            run: run_id,
            done: report.done + report.already,
            conflicts: (report.conflicts.len() + report.failures.len()) as u32,
            interrupted: report.interrupted,
        });
        runner.rec.append(&tail, true)?;
    }
    report.notes = std::mem::take(&mut runner.budget.notes);
    Ok(report)
}

pub fn describe_action(action: &Action) -> String {
    let s = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
    match action {
        Action::Unlink { path, saved } => match &saved.content {
            Content::Symlink { .. } => format!("removed symlink {}", s(path)),
            Content::Special { .. } => format!("removed special file {}", s(path)),
            Content::File { .. } => format!("removed {}", s(path)),
        },
        Action::Rmdir { path, .. } => format!("removed directory {}", s(path)),
        Action::Mkdir { path, .. } => format!("created directory {}", s(path)),
        Action::Create { path, .. } => format!("created {}", s(path)),
        Action::Write { path, .. } => format!("wrote {}", s(path)),
        Action::Rename { from, to, .. } => format!("moved {} to {}", s(from), s(to)),
        Action::Meta { path, .. } => format!("changed metadata of {}", s(path)),
    }
}

/// Resolve `--only` arguments against the transaction's recorded cwd.
pub fn resolve_only(model: &Model, only: &[std::ffi::OsString]) -> Vec<Vec<u8>> {
    let cwd = PathBuf::from(OsStr::from_bytes(&model.begin.cwd));
    only.iter()
        .map(|o| {
            let p = crate::ops::resolve(&cwd, o);
            let mut b = p.as_os_str().as_bytes().to_vec();
            while b.len() > 1 && b.ends_with(b"/") {
                b.pop();
            }
            b
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(ino: u64) -> Ident {
        Ident {
            dev: 1,
            ino,
            kind: Kind::File,
            size: 0,
            mtime: (0, 0),
            ctime: (0, 0),
        }
    }

    fn model(actions: Vec<Action>) -> Model {
        let mut records = vec![Record::Begin(Begin {
            id: 1,
            session: 1,
            shell_pid: 1,
            started_ns: 0,
            cwd: b"/w".to_vec(),
            command: Vec::new(),
            scope: None,
            detached: false,
        })];
        let n = actions.len() as u32;
        records.push(Record::Prepare { op: 1, actions });
        records.push(Record::Commit {
            op: 1,
            done: n,
            error: None,
        });
        Model::from_records(1, records, 0).unwrap()
    }

    #[test]
    fn only_includes_parent_and_rename_dependencies() {
        let m = model(vec![
            Action::Rmdir {
                path: b"/w/d/sub".to_vec(),
                ident: ident(2),
                meta: Meta::default(),
            },
            Action::Rmdir {
                path: b"/w/d".to_vec(),
                ident: ident(3),
                meta: Meta::default(),
            },
            Action::Mkdir {
                path: b"/w/other".to_vec(),
                ident: ident(4),
            },
            Action::Rename {
                from: b"/w/a".to_vec(),
                to: b"/w/d/sub/a".to_vec(),
                ident: ident(5),
            },
            Action::Create {
                path: b"/w/d/sub/a/x".to_vec(),
                ident: ident(6),
            },
        ]);
        let selected = select(&m, &[b"/w/d/sub/a/x".to_vec()]);
        assert_eq!(selected, vec![true, true, false, true, true]);
    }

    #[test]
    fn expected_state_comes_from_the_next_action_on_the_path() {
        let saved = Saved {
            content: Content::Symlink {
                target: b"t".to_vec(),
            },
            ident: ident(9),
            meta: Meta::default(),
            nlink: 1,
            copied: 0,
        };
        let m = model(vec![
            Action::Create {
                path: b"/w/f".to_vec(),
                ident: ident(1),
            },
            Action::Unlink {
                path: b"/w/f".to_vec(),
                saved,
            },
            Action::Create {
                path: b"/w/f".to_vec(),
                ident: ident(2),
            },
        ]);
        let (steps, _) = plan(&m, false, &[]);
        assert_eq!(steps.len(), 3);
        // Undo order is reverse: the last create, then the restore, then the
        // first create, whose expected state is the unlinked version.
        match &steps[2].kind {
            StepKind::Remove { expect, .. } => {
                assert_eq!(
                    *expect,
                    Evidence::Symlink {
                        target: b"t".to_vec()
                    }
                )
            }
            other => panic!("unexpected step {other:?}"),
        }
        match &steps[0].kind {
            StepKind::Remove { expect, .. } => assert_eq!(*expect, Evidence::Stat(ident(2))),
            other => panic!("unexpected step {other:?}"),
        }
    }
}
