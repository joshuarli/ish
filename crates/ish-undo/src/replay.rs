//! Conditional undo and redo.
//!
//! A transaction's journal is folded into a [`Model`]: its actions, whether
//! each happened, and which replay runs moved each one back and forth. A run
//! turns the transaction's actions into steps (undo walks actions in reverse,
//! redo forward), checks every step against the current filesystem right
//! before mutating, and records a per-action outcome, so an interrupted or
//! partially conflicting run can simply be run again.
//!
//! Replay never discards data it cannot account for: a step whose expected
//! state no longer matches is a conflict, left untouched, and the user
//! resolves it and retries the transaction. Every step that removes or
//! replaces something preserves it first, which is what makes redo, and undo
//! after redo, possible.
//!
//! Steps are write-ahead. Before a step's destructive mutation, a
//! `StepPrepare` record names the version the step displaces (already
//! preserved and durable) and the staging entry it created, with that entry's
//! identity. Only then does the step mutate, and only afterwards is
//! `StepDone` appended. A run that died anywhere in between leaves a
//! prepared, unfinished step; the next run removes the staging entry it
//! recorded (while it is still that object) and, if the step turns out to
//! have already happened, takes the displaced version from the prepare record
//! rather than recording none.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::journal::{
    self, Action, Begin, Content, Displaced, Evidence, Ident, Meta, Record, Saved, Stage, Strength,
};
use crate::preserve::{self, Budget, Check, Need};
use crate::store::{self, Home, Liveness};
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

/// A replay step that wrote its prepare record and has no completion record.
#[derive(Clone, Debug)]
pub struct Pending {
    pub stage: Option<Stage>,
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
    /// The latest unfinished replay step for this action. `StepDone` clears
    /// it, and so does a conflict the step itself settled after preparing.
    /// A conflict found without a prepare of its own does not: it says
    /// nothing about whether an interrupted earlier attempt's mutation
    /// happened.
    pub pending: Option<Pending>,
}

#[derive(Clone, Debug)]
pub struct RunInfo {
    pub run: u64,
    pub redo: bool,
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
    pub runs: Vec<RunInfo>,
    pub torn_bytes: u64,
    /// Logical bytes of the stored objects the journal references.
    pub retained: u64,
}

impl Model {
    pub fn load(home: &Home, id: u64) -> io::Result<Model> {
        let scan = journal::read(&home.journal_path(id))?;
        Model::from_records(id, scan.records, scan.torn_bytes)
    }

    pub fn from_records(id: u64, records: Vec<Record>, torn_bytes: u64) -> io::Result<Model> {
        let retained = journal::retained_logical(&records);
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
            runs: Vec::new(),
            torn_bytes,
            retained,
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
                            pending: None,
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
                Record::End {
                    status,
                    finished_ns,
                    interrupted,
                } => model.end = Some((status, finished_ns, interrupted)),
                Record::RunBegin {
                    run,
                    redo,
                    started_ns,
                } => model.runs.push(RunInfo {
                    run,
                    redo,
                    started_ns,
                    done: 0,
                    conflicts: 0,
                    finished: false,
                    interrupted: false,
                }),
                Record::StepPrepare {
                    action,
                    stage,
                    displaced,
                    ..
                } => {
                    if let Some(state) = model.actions.get_mut(action as usize) {
                        state.pending = Some(Pending { stage, displaced });
                    }
                }
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
                        state.pending = None;
                        state.last = Some(LastStep {
                            run,
                            redo,
                            placed,
                            displaced,
                        });
                    }
                }
                Record::StepConflict {
                    action,
                    reason,
                    settled,
                    ..
                } => {
                    if let Some(state) = model.actions.get_mut(action as usize) {
                        state.conflict = Some(reason);
                        if settled {
                            state.pending = None;
                        }
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

    /// Latest replay run's start time, if any.
    pub fn last_run(&self) -> Option<&RunInfo> {
        self.runs.last()
    }

    /// The saved versions this transaction references, including versions a
    /// replay step displaced or prepared to displace.
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
            if let Some(Pending {
                displaced: Displaced::Saved(d),
                ..
            }) = &s.pending
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
                Action::Create { path: p, .. } if p == path => {
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
                _ => {}
            }
        }
        if let Some(evidence) = self.finals.get(path) {
            return evidence.clone();
        }
        match &self.actions[i].action {
            Action::Create { ident, .. } => Evidence::Stat(*ident),
            Action::Rename { to, ident, .. } if to == path => Evidence::Object {
                dev: ident.dev,
                ino: ident.ino,
                kind: ident.kind,
            },
            Action::Write { .. } => Evidence::Unknown,
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
    }))
}

/// Plan the steps of an undo (`redo == false`) or redo run. Redo steps whose
/// recorded state is unavailable are returned separately.
pub fn plan(model: &Model, redo: bool) -> (Vec<Step>, Vec<(u32, String)>) {
    let mut steps = Vec::new();
    let mut unavailable = Vec::new();
    let order: Vec<usize> = if redo {
        (0..model.actions.len()).collect()
    } else {
        (0..model.actions.len()).rev().collect()
    };
    for i in order {
        if !model.relevant(i) {
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
    /// The goal state was already in place. `displaced` is what an
    /// interrupted earlier attempt of this step recorded before it mutated,
    /// if anything.
    Already {
        placed: Evidence,
        displaced: Displaced,
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
    /// The steps a dry run would perform, in order.
    pub planned: Vec<String>,
    pub notes: Vec<String>,
    pub interrupted: bool,
}

pub struct Options<'a> {
    pub redo: bool,
    /// Report the plan without touching the filesystem or the journal.
    pub dry_run: bool,
    pub copy_limit: u64,
    pub min_free: u64,
    pub cancel: &'a AtomicBool,
}

fn open_parent(path: &[u8]) -> io::Result<(OwnedFd, CString)> {
    let p = Path::new(OsStr::from_bytes(path));
    let (parent, name) = sys::split_parent(p)?;
    Ok((sys::open_dir(parent)?, sys::os_cstr(name)?))
}

fn parent_path_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => b"/",
        Some(i) => &path[..i],
        None => b".",
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = dir.to_vec();
    if !p.ends_with(b"/") {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

/// Fresh staging names tried per step before giving up.
const STAGE_ATTEMPTS: usize = 4;

/// A hard-link group restored during a run.
struct LinkGroup {
    /// The first name restored.
    first: Vec<u8>,
    /// Names restored so far.
    restored: u64,
    /// Original link count.
    nlink: u64,
}

/// Replay state shared by the steps of a run.
struct Runner<'b> {
    rec: Recorder,
    run: u64,
    budget: Budget<'b>,
    /// Hard-link groups restored in this run, by original device and inode.
    groups: HashMap<(u64, u64), LinkGroup>,
    /// Directories placed in this run, whose metadata is applied after
    /// their children.
    deferred: Vec<(Vec<u8>, Meta)>,
    /// What earlier steps of this run left at each path. A later step's
    /// expectation for that path comes from here: replay itself changes
    /// object identities (a restored file is a new clone), and the recorded
    /// evidence predates those changes.
    placed_at: HashMap<Vec<u8>, Evidence>,
    /// Versions that interrupted earlier steps recorded before mutating,
    /// by action. Used only if the step's goal state is found in place.
    interrupted: HashMap<u32, Displaced>,
    /// Whether the current step has written a prepare record. If it then
    /// ends without changing anything, that record is settled.
    prepared: bool,
}

impl Runner<'_> {
    fn check(
        &mut self,
        dir: BorrowedFd<'_>,
        name: &CString,
        evidence: &Evidence,
    ) -> io::Result<Check> {
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

    fn exists(&self, dir: BorrowedFd<'_>, name: &CString) -> io::Result<Option<sys::Stat>> {
        match sys::lstat_at(dir, name) {
            Ok(st) => Ok(Some(st)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn parent(&self, path: &[u8]) -> Result<(OwnedFd, CString), Box<Outcome>> {
        open_parent(path).map_err(|e| Box::new(parent_error(e)))
    }

    /// The goal state was found in place. If an interrupted earlier attempt
    /// of this step recorded what it displaced before mutating, that is what
    /// was displaced.
    fn already(&mut self, action: u32, placed: Evidence) -> Outcome {
        Outcome::Already {
            placed,
            displaced: self.interrupted.remove(&action).unwrap_or(Displaced::None),
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
        if st.kind == Kind::Dir {
            // An empty directory holds nothing to preserve; the action that
            // removed it recorded its metadata.
            return Ok(Displaced::None);
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

    /// Write the prepare record for a step that removes or replaces
    /// something. The displaced version's objects are made durable first, and
    /// the record itself is durable on return: nothing destructive may run
    /// before this succeeds.
    fn prepare_removal(&mut self, action: u32, displaced: &Displaced) -> io::Result<()> {
        let durable = !matches!(displaced, Displaced::None);
        if durable {
            self.rec.stores.sync_objects()?;
        }
        self.rec.append(
            &[Record::StepPrepare {
                run: self.run,
                action,
                stage: None,
                displaced: displaced.clone(),
            }],
            durable,
        )?;
        self.prepared = true;
        crate::fault::check("replay-prepared")
    }

    /// Create the restored form of `saved` under a fresh, exclusively created
    /// staging name in `dir` (whose path is `dir_path`), rejoining hard links
    /// restored earlier in this run. The prepare record naming the staging
    /// entry and its identity is appended as soon as the entry exists and
    /// before any data goes into it, so a crash at any later point leaves an
    /// entry the next run can prove is ours. The caller has already
    /// preserved and synced `displaced`.
    fn stage(
        &mut self,
        action: u32,
        saved: &Saved,
        dir: BorrowedFd<'_>,
        dir_path: &[u8],
        displaced: &Displaced,
    ) -> Result<CString, String> {
        let durable = !matches!(displaced, Displaced::None);
        let relink = if saved.nlink > 1 {
            self.groups
                .get(&(saved.ident.dev, saved.ident.ino))
                .map(|g| g.first.clone())
        } else {
            None
        };
        for _ in 0..STAGE_ATTEMPTS {
            let name = CString::new(format!(
                ".ish-undo-{}-{}-{action}-{:016x}",
                self.rec.id,
                self.run,
                sys::random_u64()
            ))
            .unwrap();
            let path = join(dir_path, name.to_bytes());
            let run = self.run;
            let Recorder { stores, writer, .. } = &mut self.rec;
            let prepared = &mut self.prepared;
            let mut created = |st: &sys::Stat| -> io::Result<()> {
                writer.append(
                    &[Record::StepPrepare {
                        run,
                        action,
                        stage: Some(Stage {
                            path: path.clone(),
                            ident: Ident::of(st),
                        }),
                        displaced: displaced.clone(),
                    }],
                    durable,
                )?;
                *prepared = true;
                crate::fault::check("replay-prepared")
            };
            let relinked = match &relink {
                Some(first) => match open_parent(first)
                    .and_then(|(fdir, fname)| sys::link_at(fdir.as_fd(), &fname, dir, &name))
                {
                    Ok(()) => Some(
                        sys::lstat_at(dir, &name)
                            .and_then(|st| created(&st))
                            .inspect_err(|_| {
                                let _ = sys::unlink_at(dir, &name);
                            }),
                    ),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(_) => None,
                },
                None => None,
            };
            let result = match relinked {
                Some(r) => r,
                None => {
                    preserve::materialize(stores, &mut self.budget, saved, dir, &name, &mut created)
                }
            };
            match result {
                Ok(()) => return Ok(name),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot restore the saved version: {e}")),
            }
        }
        Err("could not create a staging entry".into())
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
        self.prepared = false;
        let action = step.action;
        let expect = |placed_at: &HashMap<Vec<u8>, Evidence>, path: &[u8], recorded: &Evidence| {
            placed_at
                .get(path)
                .cloned()
                .unwrap_or_else(|| recorded.clone())
        };
        let outcome = match &step.kind {
            StepKind::Place { path, source } => self.place(action, path, source),
            StepKind::Remove {
                path,
                expect: e,
                dir,
            } => {
                let e = expect(&self.placed_at, path, e);
                self.remove(action, path, &e, *dir)
            }
            StepKind::Replace {
                path,
                expect: e,
                source,
            } => {
                let e = expect(&self.placed_at, path, e);
                self.replace(action, path, &e, source)
            }
            StepKind::Move {
                from,
                to,
                expect: e,
            } => {
                let e = expect(&self.placed_at, from, e);
                self.move_back(from, to, &e)
            }
        };
        if let Outcome::Done { placed, .. } | Outcome::Already { placed, .. } = &outcome {
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
            }
        }
        outcome
    }

    fn place(&mut self, action: u32, path: &[u8], source: &Source) -> Outcome {
        let (dir, name) = match self.parent(path) {
            Ok(v) => v,
            Err(o) => return *o,
        };
        let current = match self.exists(dir.as_fd(), &name) {
            Ok(c) => c,
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        if let Some(st) = current {
            // Already in the goal state (a restarted run, or restored by
            // hand): accept it.
            let goal = match source {
                Source::Dir(_) => {
                    if st.kind == Kind::Dir {
                        self.deferred_meta(path, source);
                        return self.already(
                            action,
                            Evidence::Dir {
                                dev: st.dev,
                                ino: st.ino,
                            },
                        );
                    }
                    None
                }
                Source::Saved(saved) => Some(saved.evidence()),
            };
            if let Some(goal) = goal
                && matches!(self.check(dir.as_fd(), &name, &goal), Ok(Check::Match))
            {
                let placed = self.placed_evidence(dir.as_fd(), &name, source_saved(source));
                return self.already(action, placed);
            }
            return Outcome::Conflict("something newer is in the way".into());
        }
        match source {
            Source::Dir(meta) => {
                if let Err(e) = sys::mkdir_at(dir.as_fd(), &name, 0o700) {
                    return Outcome::Failed(sys::describe_error(&e));
                }
                self.deferred.push((path.to_vec(), meta.clone()));
            }
            Source::Saved(saved) => {
                let tmp = match self.stage(
                    action,
                    saved,
                    dir.as_fd(),
                    parent_path_of(path),
                    &Displaced::None,
                ) {
                    Ok(tmp) => tmp,
                    Err(e) => return Outcome::Failed(e),
                };
                if let Err(e) = sys::rename_noreplace(dir.as_fd(), &tmp, dir.as_fd(), &name) {
                    let _ = sys::unlink_at(dir.as_fd(), &tmp);
                    return if e.kind() == io::ErrorKind::AlreadyExists {
                        Outcome::Conflict("something newer appeared there".into())
                    } else {
                        Outcome::Failed(sys::describe_error(&e))
                    };
                }
                if saved.nlink > 1 {
                    let key = (saved.ident.dev, saved.ident.ino);
                    self.groups
                        .entry(key)
                        .and_modify(|g| g.restored += 1)
                        .or_insert_with(|| LinkGroup {
                            first: path.to_vec(),
                            restored: 1,
                            nlink: saved.nlink,
                        });
                }
            }
        }
        Outcome::Done {
            placed: self.placed_evidence(dir.as_fd(), &name, source_saved(source)),
            displaced: Displaced::None,
        }
    }

    fn deferred_meta(&mut self, path: &[u8], source: &Source) {
        if let Source::Dir(meta) = source {
            self.deferred.push((path.to_vec(), meta.clone()));
        }
    }

    fn remove(&mut self, action: u32, path: &[u8], expect: &Evidence, dir_kind: bool) -> Outcome {
        let (dir, name) = match self.parent(path) {
            Ok(v) => v,
            // A missing parent means the path is already gone; any other
            // error (permissions, I/O) says nothing about it.
            Err(o) if matches!(*o, Outcome::Conflict(_)) => {
                return self.already(action, Evidence::Absent);
            }
            Err(o) => return *o,
        };
        let current = match self.exists(dir.as_fd(), &name) {
            Ok(Some(st)) => st,
            Ok(None) => return self.already(action, Evidence::Absent),
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        match self.check(dir.as_fd(), &name, expect) {
            Ok(Check::Match) => {}
            Ok(Check::Mismatch(why)) => return Outcome::Conflict(why),
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        }
        let displaced = match self.displace(dir.as_fd(), &name, path, &current, expect) {
            Ok(d) => d,
            Err(e) => return Outcome::Conflict(e),
        };
        if let Err(e) = self.prepare_removal(action, &displaced) {
            return Outcome::Failed(format!(
                "cannot record the step: {}",
                sys::describe_error(&e)
            ));
        }
        // Remove only the object that was preserved.
        match sys::lstat_at(dir.as_fd(), &name) {
            Ok(now) if now.same_object(&current) => {}
            _ => return Outcome::Conflict("it changed while being removed".into()),
        }
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

    fn replace(&mut self, action: u32, path: &[u8], expect: &Evidence, source: &Saved) -> Outcome {
        let (dir, name) = match self.parent(path) {
            Ok(v) => v,
            Err(o) => return *o,
        };
        let current = match self.exists(dir.as_fd(), &name) {
            Ok(Some(st)) => st,
            Ok(None) => return Outcome::Conflict("it no longer exists".into()),
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        if current.kind == Kind::Dir {
            return Outcome::Conflict("it is now a directory".into());
        }
        if matches!(
            self.check(dir.as_fd(), &name, &source.evidence()),
            Ok(Check::Match)
        ) {
            let placed = self.placed_evidence(dir.as_fd(), &name, Some(source));
            return self.already(action, placed);
        }
        match self.check(dir.as_fd(), &name, expect) {
            Ok(Check::Match) => {}
            Ok(Check::Mismatch(why)) => return Outcome::Conflict(why),
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        }
        let displaced = match self.displace(dir.as_fd(), &name, path, &current, expect) {
            Ok(d) => d,
            Err(e) => return Outcome::Conflict(e),
        };
        if let Err(e) = self.rec.stores.sync_objects() {
            return Outcome::Failed(format!(
                "cannot record the step: {}",
                sys::describe_error(&e)
            ));
        }
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
        // Staging journals the prepare record, displaced version included,
        // before the destructive rename below.
        let tmp = match self.stage(
            action,
            source,
            dir.as_fd(),
            parent_path_of(path),
            &displaced,
        ) {
            Ok(tmp) => tmp,
            Err(e) => return Outcome::Failed(e),
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
        let source = match self.exists(fdir.as_fd(), &fname) {
            Ok(s) => s,
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        };
        let Some(source) = source else {
            // Already moved (a restarted run)?
            if let Ok(Check::Match) = preserve::check_evidence(
                &mut self.rec.stores,
                self.budget.cancel,
                tdir.as_fd(),
                &tname,
                expect,
            ) {
                return Outcome::Already {
                    placed: expect.clone(),
                    displaced: Displaced::None,
                };
            }
            return Outcome::Conflict("it is no longer there".into());
        };
        match self.check(fdir.as_fd(), &fname, expect) {
            Ok(Check::Match) => {}
            Ok(Check::Mismatch(why)) => return Outcome::Conflict(why),
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        }
        match self.exists(tdir.as_fd(), &tname) {
            Ok(None) => {}
            Ok(Some(_)) => {
                return Outcome::Conflict(format!(
                    "{} is occupied by newer data",
                    String::from_utf8_lossy(to)
                ));
            }
            Err(e) => return Outcome::Failed(sys::describe_error(&e)),
        }
        match sys::rename_noreplace(fdir.as_fd(), &fname, tdir.as_fd(), &tname) {
            Ok(()) => Outcome::Done {
                placed: Evidence::Object {
                    dev: source.dev,
                    ino: source.ino,
                    kind: source.kind,
                },
                displaced: Displaced::None,
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

    /// Reconcile steps that an earlier run prepared and never finished:
    /// remove the staging entries they recorded, but only while each is still
    /// the object that was recorded, and remember what they displaced.
    fn reconcile(&mut self, model: &Model) {
        for (i, state) in model.actions.iter().enumerate() {
            let Some(pending) = &state.pending else {
                continue;
            };
            if !matches!(pending.displaced, Displaced::None) {
                self.interrupted.insert(i as u32, pending.displaced.clone());
            }
            if let Some(stage) = &pending.stage {
                self.remove_stage(stage);
            }
        }
    }

    fn remove_stage(&mut self, stage: &Stage) {
        let shown = String::from_utf8_lossy(&stage.path).into_owned();
        let result = open_parent(&stage.path).and_then(|(dir, name)| {
            match sys::lstat_at(dir.as_fd(), &name) {
                // Already gone: published, or removed by hand.
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(e) => Err(e),
                Ok(st) if stage.ident.same_object(&st) && st.kind != Kind::Dir => {
                    sys::unlink_at(dir.as_fd(), &name).map(|()| true)
                }
                Ok(_) => Ok(false),
            }
        });
        match result {
            Ok(true) => {}
            Ok(false) => self.budget.notes.push(format!(
                "{shown}: left in place; it is not the staging entry an interrupted restore recorded"
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => self.budget.notes.push(format!(
                "{shown}: interrupted restore's staging entry not removed: {}",
                sys::describe_error(&e)
            )),
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
    /// Sealed, and another process is undoing or redoing it right now.
    Replaying,
}

/// State from the journal and the recording shell alone. Callers that hold
/// the transaction's replay lock use this: probing the lock would find their
/// own.
pub fn recorded_lifecycle(home: &Home, model: &Model) -> Lifecycle {
    if model.end.is_some() {
        return Lifecycle::Completed;
    }
    match store::session_liveness(home, model.begin.session, model.begin.shell_pid) {
        Liveness::Alive => Lifecycle::Active,
        Liveness::Dead => Lifecycle::Interrupted,
    }
}

pub fn lifecycle(home: &Home, model: &Model) -> Lifecycle {
    let recorded = recorded_lifecycle(home, model);
    // A run that began and did not end is either in progress or died. Only
    // then is the lock probed, so a probe never contends with a replay that
    // is merely about to start.
    if recorded == Lifecycle::Completed
        && model.runs.last().is_some_and(|r| !r.finished)
        && matches!(home.lock_replay(model.id), Ok(None))
    {
        return Lifecycle::Replaying;
    }
    recorded
}

/// Run an undo or redo of transaction `id`.
pub fn run(home: &Home, id: u64, opts: &Options<'_>) -> io::Result<Report> {
    if opts.dry_run {
        let model = Model::load(home, id)?;
        return Ok(dry_run(&model, opts.redo));
    }
    let _lock = home.lock_replay(id)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("transaction {id} is being replayed by another shell"),
        )
    })?;
    let model = Model::load(home, id)?;
    let (steps, unavailable) = plan(&model, opts.redo);
    let mut report = Report::default();
    for (action, reason) in &unavailable {
        report.conflicts.push((
            describe_action(&model.actions[*action as usize].action),
            reason.clone(),
        ));
    }
    let run_id = model.runs.iter().map(|r| r.run).max().unwrap_or(0) + 1;
    report.run = run_id;
    let mut runner = Runner {
        rec: Recorder::open(home, id)?,
        run: run_id,
        budget: Budget::new(opts.copy_limit, opts.min_free, opts.cancel),
        groups: HashMap::new(),
        deferred: Vec::new(),
        placed_at: HashMap::new(),
        interrupted: HashMap::new(),
        prepared: false,
    };
    // The cached size describes the journal as it was before this run.
    crate::retention::forget_size(home, id);
    runner.rec.append(
        &[Record::RunBegin {
            run: run_id,
            redo: opts.redo,
            started_ns: sys::now_ns(),
        }],
        true,
    )?;
    runner.reconcile(&model);
    for step in &steps {
        if opts.cancel.load(Ordering::Relaxed) {
            report.interrupted = true;
            break;
        }
        let label = step.describe();
        let outcome = runner.step(step);
        if matches!(outcome, Outcome::Done { .. } | Outcome::Already { .. }) {
            // The mutation happened; its completion record has not been
            // written yet. Tests interrupt here.
            crate::fault::check("replay-mutated")?;
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
            Outcome::Already { placed, displaced } => {
                report.already += 1;
                Record::StepDone {
                    run: run_id,
                    action: step.action,
                    redo: opts.redo,
                    placed,
                    displaced,
                }
            }
            Outcome::Conflict(why) => {
                report.conflicts.push((label, why.clone()));
                Record::StepConflict {
                    run: run_id,
                    action: step.action,
                    reason: why,
                    settled: runner.prepared,
                }
            }
            Outcome::Failed(why) => {
                report.failures.push((label, why.clone()));
                Record::StepConflict {
                    run: run_id,
                    action: step.action,
                    reason: why,
                    settled: runner.prepared,
                }
            }
        };
        runner.rec.append(&[record], false)?;
        crate::fault::check("replay-step")?;
    }
    for LinkGroup {
        restored,
        nlink,
        first: path,
    } in runner.groups.values()
    {
        if restored < nlink {
            runner.budget.notes.push(format!(
                "{}: had {nlink} hard links; {restored} restored together, other links (outside this change) were not rejoined",
                String::from_utf8_lossy(path)
            ));
        }
    }
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
    report.notes = std::mem::take(&mut runner.budget.notes);
    Ok(report)
}

/// The steps a run would perform, from the same plan a real run executes.
/// Conflicts are not predicted: they depend on the filesystem at the moment
/// each step runs, and a real run leaves them untouched.
fn dry_run(model: &Model, redo: bool) -> Report {
    let (steps, unavailable) = plan(model, redo);
    let mut report = Report::default();
    for (action, reason) in &unavailable {
        report.conflicts.push((
            describe_action(&model.actions[*action as usize].action),
            reason.clone(),
        ));
    }
    report.planned = steps.iter().map(Step::describe).collect();
    report
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
        Action::Create { path, .. } => format!("created {}", s(path)),
        Action::Write { path, .. } => format!("wrote {}", s(path)),
        Action::Rename { from, to, .. } => format!("moved {} to {}", s(from), s(to)),
    }
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

    fn begin() -> Record {
        Record::Begin(Begin {
            id: 1,
            session: 1,
            shell_pid: 1,
            started_ns: 0,
            cwd: b"/w".to_vec(),
            command: Vec::new(),
            detached: false,
        })
    }

    fn model(actions: Vec<Action>) -> Model {
        let n = actions.len() as u32;
        let records = vec![
            begin(),
            Record::Prepare { op: 1, actions },
            Record::Commit {
                op: 1,
                done: n,
                error: None,
            },
        ];
        Model::from_records(1, records, 0).unwrap()
    }

    fn saved_symlink(target: &[u8]) -> Saved {
        Saved {
            content: Content::Symlink {
                target: target.to_vec(),
            },
            ident: ident(9),
            meta: Meta::default(),
            nlink: 1,
            copied: 0,
        }
    }

    #[test]
    fn expected_state_comes_from_the_next_action_on_the_path() {
        let m = model(vec![
            Action::Create {
                path: b"/w/f".to_vec(),
                ident: ident(1),
            },
            Action::Unlink {
                path: b"/w/f".to_vec(),
                saved: saved_symlink(b"t"),
            },
            Action::Create {
                path: b"/w/f".to_vec(),
                ident: ident(2),
            },
        ]);
        let (steps, _) = plan(&m, false);
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

    #[test]
    fn only_a_completed_step_settles_a_prepared_one() {
        let displaced = Displaced::Saved(saved_symlink(b"d"));
        let stage = Stage {
            path: b"/w/.ish-undo-1-1-0-00".to_vec(),
            ident: ident(5),
        };
        let mut records = vec![
            begin(),
            Record::Prepare {
                op: 1,
                actions: vec![Action::Create {
                    path: b"/w/f".to_vec(),
                    ident: ident(1),
                }],
            },
            Record::Commit {
                op: 1,
                done: 1,
                error: None,
            },
            Record::RunBegin {
                run: 1,
                redo: false,
                started_ns: 0,
            },
            Record::StepPrepare {
                run: 1,
                action: 0,
                stage: Some(stage.clone()),
                displaced: displaced.clone(),
            },
        ];
        let pending = |records: &[Record]| {
            let m = Model::from_records(1, records.to_vec(), 0).unwrap();
            m.actions[0].pending.clone()
        };
        let p = pending(&records).expect("prepared and unfinished");
        assert_eq!(p.stage, Some(stage));
        assert_eq!(p.displaced, displaced);

        // A conflict does not say whether the mutation happened.
        records.push(Record::StepConflict {
            run: 1,
            action: 0,
            reason: "changed".into(),
            settled: false,
        });
        assert!(pending(&records).is_some());

        // A conflict the step settled after preparing (and undoing) does.
        let mut settled = records.clone();
        settled.push(Record::StepConflict {
            run: 2,
            action: 0,
            reason: "changed while being removed".into(),
            settled: true,
        });
        assert!(pending(&settled).is_none());

        records.push(Record::StepDone {
            run: 2,
            action: 0,
            redo: false,
            placed: Evidence::Absent,
            displaced,
        });
        assert!(pending(&records).is_none());
    }

    #[test]
    fn prepared_displaced_versions_count_as_referenced() {
        let mut saved = saved_symlink(b"d");
        saved.content = Content::File {
            obj: journal::ObjRef {
                store: 4,
                name: b"obj/x".to_vec(),
            },
            strength: Strength::Clone,
        };
        saved.ident.size = 100;
        let records = vec![
            begin(),
            Record::Prepare {
                op: 1,
                actions: vec![Action::Create {
                    path: b"/w/f".to_vec(),
                    ident: ident(1),
                }],
            },
            Record::Commit {
                op: 1,
                done: 1,
                error: None,
            },
            Record::StepPrepare {
                run: 1,
                action: 0,
                stage: None,
                displaced: Displaced::Saved(saved),
            },
        ];
        let m = Model::from_records(1, records, 0).unwrap();
        assert_eq!(m.saved_versions().count(), 1);
        assert_eq!(m.retained, 100);
    }
}
