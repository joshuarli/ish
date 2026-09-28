//! Transaction journal: record model, binary encoding, and file access.
//!
//! A journal is `MAGIC` followed by length-framed records:
//! `[u32 payload length][u32 CRC-32 of payload][payload]`, little-endian.
//! Payloads start with a tag byte; integers are LEB128 varints and byte
//! strings are length-prefixed, so paths are stored losslessly.
//!
//! Appends take an exclusive `flock` on a descriptor the appending process
//! opened itself, then write each batch with one `write`. A reader stops at
//! the first record whose length or checksum does not validate; a later
//! appender truncates such a torn tail under the lock, because a live writer
//! always holds the lock for its whole write.

use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::fault;
use crate::sys::{self, Kind, Stat};

pub const MAGIC: &[u8; 8] = b"ISHUNDOJ";
/// Version 2 dropped scoped-checkpoint records and gave replay steps a
/// write-ahead record; version 1 journals are not read.
pub const VERSION: u32 = 2;
const HEADER_LEN: u64 = 12;
/// Upper bound for a single record, to reject garbage lengths early.
const MAX_RECORD: u32 = 64 * 1024 * 1024;

/// Identity and change evidence for a filesystem object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ident {
    pub dev: u64,
    pub ino: u64,
    pub kind: Kind,
    pub size: u64,
    pub mtime: (i64, u32),
    pub ctime: (i64, u32),
}

impl Ident {
    pub fn of(st: &Stat) -> Ident {
        Ident {
            dev: st.dev,
            ino: st.ino,
            kind: st.kind,
            size: st.size,
            mtime: st.mtime,
            ctime: st.ctime,
        }
    }

    pub fn same_object(&self, st: &Stat) -> bool {
        self.dev == st.dev && self.ino == st.ino && self.kind == st.kind
    }

    /// Same object with unchanged size, mtime, and ctime.
    pub fn unchanged(&self, st: &Stat) -> bool {
        self.same_object(st)
            && self.size == st.size
            && self.mtime == st.mtime
            && self.ctime == st.ctime
    }
}

/// Restorable metadata of an object.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: (i64, u32),
    pub mtime: (i64, u32),
    pub flags: u32,
    /// Extended attributes, recorded for directories (regular-file objects
    /// carry their own).
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    /// False when attributes existed but exceeded the recording limit.
    pub xattrs_complete: bool,
}

impl Meta {
    pub fn of(st: &Stat) -> Meta {
        Meta {
            mode: st.mode,
            uid: st.uid,
            gid: st.gid,
            atime: st.atime,
            mtime: st.mtime,
            flags: st.flags,
            xattrs: Vec::new(),
            xattrs_complete: true,
        }
    }
}

/// How a saved version is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strength {
    /// Copy-on-write clone: frozen, independent of later writes.
    Clone,
    /// Independent byte copy: frozen.
    Copy,
    /// Retained inode (hard link). Another link or an open writer can still
    /// change it, so it is not an immutable historical version.
    Link,
}

impl Strength {
    pub fn name(self) -> &'static str {
        match self {
            Strength::Clone => "clone",
            Strength::Copy => "copy",
            Strength::Link => "linked",
        }
    }

    pub fn frozen(self) -> bool {
        !matches!(self, Strength::Link)
    }
}

/// A stored object: `store` is the store id; `name` is relative to the
/// transaction directory in that store (for example `obj/5f3a-12`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjRef {
    pub store: u64,
    pub name: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    File {
        obj: ObjRef,
        strength: Strength,
    },
    Symlink {
        target: Vec<u8>,
    },
    /// FIFO, socket, or device node retained by hard link.
    Special {
        obj: ObjRef,
    },
}

/// A preserved pre-state of a non-directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    pub content: Content,
    pub ident: Ident,
    pub meta: Meta,
    pub nlink: u64,
    /// Bytes copied in userspace to create this version.
    pub copied: u64,
}

impl Saved {
    pub fn obj(&self) -> Option<&ObjRef> {
        match &self.content {
            Content::File { obj, .. } | Content::Special { obj } => Some(obj),
            Content::Symlink { .. } => None,
        }
    }

    pub fn strength(&self) -> Option<Strength> {
        match &self.content {
            Content::File { strength, .. } => Some(*strength),
            Content::Special { .. } => Some(Strength::Link),
            Content::Symlink { .. } => None,
        }
    }

    /// Evidence that a path currently holds exactly this version. A linked
    /// version is the retained inode itself, so only its identity can be
    /// checked; linking and unlinking it changes its ctime.
    pub fn evidence(&self) -> Evidence {
        match &self.content {
            Content::File { obj, strength } if strength.frozen() => Evidence::Frozen {
                ident: self.ident,
                obj: obj.clone(),
            },
            Content::File { .. } | Content::Special { .. } => Evidence::Object {
                dev: self.ident.dev,
                ino: self.ident.ino,
                kind: self.ident.kind,
            },
            Content::Symlink { target } => Evidence::Symlink {
                target: target.clone(),
            },
        }
    }
}

/// Recorded post-state used to decide whether replay may touch a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evidence {
    Absent,
    /// Same object with unchanged size/mtime/ctime. Weaker than content.
    Stat(Ident),
    /// Contents equal to a frozen object (ident allows a fast accept).
    Frozen {
        ident: Ident,
        obj: ObjRef,
    },
    Symlink {
        target: Vec<u8>,
    },
    Dir {
        dev: u64,
        ino: u64,
    },
    /// Same object by identity; contents may have changed. Used for moves,
    /// whose inverse carries later edits along.
    Object {
        dev: u64,
        ino: u64,
        kind: Kind,
    },
    /// Nothing reliable was recorded.
    Unknown,
}

/// One filesystem transition, recorded with the information needed to
/// reverse it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// A non-directory entry was removed; its pre-state is preserved.
    Unlink { path: Vec<u8>, saved: Saved },
    /// An empty directory was removed.
    Rmdir {
        path: Vec<u8>,
        ident: Ident,
        meta: Meta,
    },
    /// A regular file or symlink was created.
    Create { path: Vec<u8>, ident: Ident },
    /// A regular file was modified in place. `saved` is `None` when an
    /// earlier action in the same transaction already preserved it.
    Write {
        path: Vec<u8>,
        ident: Ident,
        saved: Option<Saved>,
    },
    /// A namespace move on one filesystem.
    Rename {
        from: Vec<u8>,
        to: Vec<u8>,
        ident: Ident,
    },
}

impl Action {
    pub fn saved(&self) -> Option<&Saved> {
        match self {
            Action::Unlink { saved, .. } => Some(saved),
            Action::Write { saved, .. } => saved.as_ref(),
            _ => None,
        }
    }
}

/// What a replay step moved out of the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Displaced {
    None,
    Saved(Saved),
}

/// A staging entry a replay step created beside its destination, with the
/// identity that proves it is ours: a restarted run removes it only while the
/// recorded object is still what sits at that path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    pub path: Vec<u8>,
    pub ident: Ident,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Begin {
    pub id: u64,
    pub session: u64,
    pub shell_pid: u32,
    pub started_ns: u64,
    pub cwd: Vec<u8>,
    pub command: Vec<u8>,
    /// Allocated by a process that outlived its transaction.
    pub detached: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Begin(Begin),
    /// Actions about to be performed; every saved version they reference is
    /// already durable.
    Prepare {
        op: u64,
        actions: Vec<Action>,
    },
    /// The first `done` actions of `op` happened; the rest did not.
    Commit {
        op: u64,
        done: u32,
        error: Option<String>,
    },
    /// Post-state of a path, captured when the transaction was sealed.
    Final {
        path: Vec<u8>,
        evidence: Evidence,
    },
    /// Commands that ran without capture.
    Opaque {
        count: u32,
    },
    /// Fidelity or coverage notice shown by `undo show`.
    Note {
        text: String,
    },
    End {
        status: i32,
        finished_ns: u64,
        interrupted: bool,
    },
    RunBegin {
        run: u64,
        redo: bool,
        started_ns: u64,
    },
    /// Written, durably when it holds a displaced version, before a replay
    /// step's destructive mutation: the version the step is about to
    /// displace (already preserved) and the staging entry it created.
    /// Without a later `StepDone`, the step is unfinished and a restarted run
    /// uses this record to reconcile it.
    StepPrepare {
        run: u64,
        action: u32,
        stage: Option<Stage>,
        displaced: Displaced,
    },
    StepDone {
        run: u64,
        action: u32,
        redo: bool,
        placed: Evidence,
        displaced: Displaced,
    },
    /// A step ended without changing anything. `settled` says the step had
    /// written a prepare record in this run and undid everything it prepared
    /// (its staging entry is gone), so that record no longer describes an
    /// unfinished step. A conflict found without a prepare of its own says
    /// nothing about an older interrupted attempt and leaves it pending.
    StepConflict {
        run: u64,
        action: u32,
        reason: String,
        settled: bool,
    },
    RunEnd {
        run: u64,
        done: u32,
        conflicts: u32,
        interrupted: bool,
    },
}

struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn uv(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(byte);
                break;
            }
            self.0.push(byte | 0x80);
        }
    }
    fn iv(&mut self, v: i64) {
        self.uv(((v << 1) ^ (v >> 63)) as u64);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.uv(b.len() as u64);
        self.0.extend_from_slice(b);
    }
    fn bool(&mut self, b: bool) {
        self.u8(b as u8);
    }
    fn time(&mut self, t: (i64, u32)) {
        self.iv(t.0);
        self.uv(t.1 as u64);
    }
    fn ident(&mut self, i: &Ident) {
        self.uv(i.dev);
        self.uv(i.ino);
        self.u8(i.kind.code());
        self.uv(i.size);
        self.time(i.mtime);
        self.time(i.ctime);
    }
    fn meta(&mut self, m: &Meta) {
        self.uv(m.mode as u64);
        self.uv(m.uid as u64);
        self.uv(m.gid as u64);
        self.time(m.atime);
        self.time(m.mtime);
        self.uv(m.flags as u64);
        self.uv(m.xattrs.len() as u64);
        for (k, v) in &m.xattrs {
            self.bytes(k);
            self.bytes(v);
        }
        self.bool(m.xattrs_complete);
    }
    fn obj(&mut self, o: &ObjRef) {
        self.uv(o.store);
        self.bytes(&o.name);
    }
    fn strength(&mut self, s: Strength) {
        self.u8(match s {
            Strength::Clone => 0,
            Strength::Copy => 1,
            Strength::Link => 2,
        });
    }
    fn saved(&mut self, s: &Saved) {
        match &s.content {
            Content::File { obj, strength } => {
                self.u8(0);
                self.obj(obj);
                self.strength(*strength);
            }
            Content::Symlink { target } => {
                self.u8(1);
                self.bytes(target);
            }
            Content::Special { obj } => {
                self.u8(2);
                self.obj(obj);
            }
        }
        self.ident(&s.ident);
        self.meta(&s.meta);
        self.uv(s.nlink);
        self.uv(s.copied);
    }
    fn opt_saved(&mut self, s: &Option<Saved>) {
        match s {
            Some(s) => {
                self.u8(1);
                self.saved(s);
            }
            None => self.u8(0),
        }
    }
    fn evidence(&mut self, e: &Evidence) {
        match e {
            Evidence::Absent => self.u8(0),
            Evidence::Stat(i) => {
                self.u8(1);
                self.ident(i);
            }
            Evidence::Frozen { ident, obj } => {
                self.u8(2);
                self.ident(ident);
                self.obj(obj);
            }
            Evidence::Symlink { target } => {
                self.u8(3);
                self.bytes(target);
            }
            Evidence::Dir { dev, ino } => {
                self.u8(4);
                self.uv(*dev);
                self.uv(*ino);
            }
            Evidence::Unknown => self.u8(5),
            Evidence::Object { dev, ino, kind } => {
                self.u8(6);
                self.uv(*dev);
                self.uv(*ino);
                self.u8(kind.code());
            }
        }
    }
    fn displaced(&mut self, d: &Displaced) {
        match d {
            Displaced::None => self.u8(0),
            Displaced::Saved(s) => {
                self.u8(1);
                self.saved(s);
            }
        }
    }
    fn stage(&mut self, s: &Option<Stage>) {
        match s {
            Some(s) => {
                self.u8(1);
                self.bytes(&s.path);
                self.ident(&s.ident);
            }
            None => self.u8(0),
        }
    }
    fn action(&mut self, a: &Action) {
        match a {
            Action::Unlink { path, saved } => {
                self.u8(0);
                self.bytes(path);
                self.saved(saved);
            }
            Action::Rmdir { path, ident, meta } => {
                self.u8(1);
                self.bytes(path);
                self.ident(ident);
                self.meta(meta);
            }
            Action::Create { path, ident } => {
                self.u8(2);
                self.bytes(path);
                self.ident(ident);
            }
            Action::Write { path, ident, saved } => {
                self.u8(3);
                self.bytes(path);
                self.ident(ident);
                self.opt_saved(saved);
            }
            Action::Rename { from, to, ident } => {
                self.u8(4);
                self.bytes(from);
                self.bytes(to);
                self.ident(ident);
            }
        }
    }
}

pub fn encode(record: &Record) -> Vec<u8> {
    let mut e = Enc(Vec::with_capacity(128));
    match record {
        Record::Begin(b) => {
            e.u8(1);
            e.uv(b.id);
            e.uv(b.session);
            e.uv(b.shell_pid as u64);
            e.uv(b.started_ns);
            e.bytes(&b.cwd);
            e.bytes(&b.command);
            e.bool(b.detached);
        }
        Record::Prepare { op, actions } => {
            e.u8(2);
            e.uv(*op);
            e.uv(actions.len() as u64);
            for a in actions {
                e.action(a);
            }
        }
        Record::Commit { op, done, error } => {
            e.u8(3);
            e.uv(*op);
            e.uv(*done as u64);
            e.bytes(error.as_deref().unwrap_or("").as_bytes());
        }
        Record::Final { path, evidence } => {
            e.u8(4);
            e.bytes(path);
            e.evidence(evidence);
        }
        Record::Opaque { count } => {
            e.u8(5);
            e.uv(*count as u64);
        }
        Record::Note { text } => {
            e.u8(6);
            e.bytes(text.as_bytes());
        }
        Record::End {
            status,
            finished_ns,
            interrupted,
        } => {
            e.u8(7);
            e.iv(*status as i64);
            e.uv(*finished_ns);
            e.bool(*interrupted);
        }
        Record::RunBegin {
            run,
            redo,
            started_ns,
        } => {
            e.u8(8);
            e.uv(*run);
            e.bool(*redo);
            e.uv(*started_ns);
        }
        Record::StepPrepare {
            run,
            action,
            stage,
            displaced,
        } => {
            e.u8(9);
            e.uv(*run);
            e.uv(*action as u64);
            e.stage(stage);
            e.displaced(displaced);
        }
        Record::StepDone {
            run,
            action,
            redo,
            placed,
            displaced,
        } => {
            e.u8(10);
            e.uv(*run);
            e.uv(*action as u64);
            e.bool(*redo);
            e.evidence(placed);
            e.displaced(displaced);
        }
        Record::StepConflict {
            run,
            action,
            reason,
            settled,
        } => {
            e.u8(11);
            e.uv(*run);
            e.uv(*action as u64);
            e.bytes(reason.as_bytes());
            e.bool(*settled);
        }
        Record::RunEnd {
            run,
            done,
            conflicts,
            interrupted,
        } => {
            e.u8(12);
            e.uv(*run);
            e.uv(*done as u64);
            e.uv(*conflicts as u64);
            e.bool(*interrupted);
        }
    }
    e.0
}

struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

type D<T> = Option<T>;

impl<'a> Dec<'a> {
    fn u8(&mut self) -> D<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn uv(&mut self) -> D<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift >= 64 {
                return None;
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
        }
    }
    fn u32(&mut self) -> D<u32> {
        u32::try_from(self.uv()?).ok()
    }
    fn iv(&mut self) -> D<i64> {
        let u = self.uv()?;
        Some(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    fn bytes(&mut self) -> D<Vec<u8>> {
        let len = usize::try_from(self.uv()?).ok()?;
        let end = self.pos.checked_add(len)?;
        let b = self.buf.get(self.pos..end)?.to_vec();
        self.pos = end;
        Some(b)
    }
    fn string(&mut self) -> D<String> {
        Some(String::from_utf8_lossy(&self.bytes()?).into_owned())
    }
    fn bool(&mut self) -> D<bool> {
        Some(self.u8()? != 0)
    }
    fn time(&mut self) -> D<(i64, u32)> {
        Some((self.iv()?, self.u32()?))
    }
    fn ident(&mut self) -> D<Ident> {
        Some(Ident {
            dev: self.uv()?,
            ino: self.uv()?,
            kind: Kind::from_code(self.u8()?),
            size: self.uv()?,
            mtime: self.time()?,
            ctime: self.time()?,
        })
    }
    fn meta(&mut self) -> D<Meta> {
        let mode = self.u32()?;
        let uid = self.u32()?;
        let gid = self.u32()?;
        let atime = self.time()?;
        let mtime = self.time()?;
        let flags = self.u32()?;
        let n = self.uv()?;
        let mut xattrs = Vec::new();
        for _ in 0..n {
            xattrs.push((self.bytes()?, self.bytes()?));
        }
        Some(Meta {
            mode,
            uid,
            gid,
            atime,
            mtime,
            flags,
            xattrs,
            xattrs_complete: self.bool()?,
        })
    }
    fn obj(&mut self) -> D<ObjRef> {
        Some(ObjRef {
            store: self.uv()?,
            name: self.bytes()?,
        })
    }
    fn strength(&mut self) -> D<Strength> {
        Some(match self.u8()? {
            0 => Strength::Clone,
            1 => Strength::Copy,
            2 => Strength::Link,
            _ => return None,
        })
    }
    fn saved(&mut self) -> D<Saved> {
        let content = match self.u8()? {
            0 => Content::File {
                obj: self.obj()?,
                strength: self.strength()?,
            },
            1 => Content::Symlink {
                target: self.bytes()?,
            },
            2 => Content::Special { obj: self.obj()? },
            _ => return None,
        };
        Some(Saved {
            content,
            ident: self.ident()?,
            meta: self.meta()?,
            nlink: self.uv()?,
            copied: self.uv()?,
        })
    }
    fn opt_saved(&mut self) -> D<Option<Saved>> {
        Some(match self.u8()? {
            0 => None,
            _ => Some(self.saved()?),
        })
    }
    fn evidence(&mut self) -> D<Evidence> {
        Some(match self.u8()? {
            0 => Evidence::Absent,
            1 => Evidence::Stat(self.ident()?),
            2 => Evidence::Frozen {
                ident: self.ident()?,
                obj: self.obj()?,
            },
            3 => Evidence::Symlink {
                target: self.bytes()?,
            },
            4 => Evidence::Dir {
                dev: self.uv()?,
                ino: self.uv()?,
            },
            5 => Evidence::Unknown,
            6 => Evidence::Object {
                dev: self.uv()?,
                ino: self.uv()?,
                kind: Kind::from_code(self.u8()?),
            },
            _ => return None,
        })
    }
    fn displaced(&mut self) -> D<Displaced> {
        Some(match self.u8()? {
            0 => Displaced::None,
            1 => Displaced::Saved(self.saved()?),
            _ => return None,
        })
    }
    fn stage(&mut self) -> D<Option<Stage>> {
        Some(match self.u8()? {
            0 => None,
            _ => Some(Stage {
                path: self.bytes()?,
                ident: self.ident()?,
            }),
        })
    }
    fn action(&mut self) -> D<Action> {
        Some(match self.u8()? {
            0 => Action::Unlink {
                path: self.bytes()?,
                saved: self.saved()?,
            },
            1 => Action::Rmdir {
                path: self.bytes()?,
                ident: self.ident()?,
                meta: self.meta()?,
            },
            2 => Action::Create {
                path: self.bytes()?,
                ident: self.ident()?,
            },
            3 => Action::Write {
                path: self.bytes()?,
                ident: self.ident()?,
                saved: self.opt_saved()?,
            },
            4 => Action::Rename {
                from: self.bytes()?,
                to: self.bytes()?,
                ident: self.ident()?,
            },
            _ => return None,
        })
    }
}

pub fn decode(payload: &[u8]) -> Option<Record> {
    let mut d = Dec {
        buf: payload,
        pos: 0,
    };
    let record = match d.u8()? {
        1 => Record::Begin(Begin {
            id: d.uv()?,
            session: d.uv()?,
            shell_pid: d.u32()?,
            started_ns: d.uv()?,
            cwd: d.bytes()?,
            command: d.bytes()?,
            detached: d.bool()?,
        }),
        2 => {
            let op = d.uv()?;
            let n = d.uv()?;
            let mut actions = Vec::new();
            for _ in 0..n {
                actions.push(d.action()?);
            }
            Record::Prepare { op, actions }
        }
        3 => Record::Commit {
            op: d.uv()?,
            done: d.u32()?,
            error: Some(d.string()?).filter(|s| !s.is_empty()),
        },
        4 => Record::Final {
            path: d.bytes()?,
            evidence: d.evidence()?,
        },
        5 => Record::Opaque { count: d.u32()? },
        6 => Record::Note { text: d.string()? },
        7 => Record::End {
            status: i32::try_from(d.iv()?).ok()?,
            finished_ns: d.uv()?,
            interrupted: d.bool()?,
        },
        8 => Record::RunBegin {
            run: d.uv()?,
            redo: d.bool()?,
            started_ns: d.uv()?,
        },
        9 => Record::StepPrepare {
            run: d.uv()?,
            action: d.u32()?,
            stage: d.stage()?,
            displaced: d.displaced()?,
        },
        10 => Record::StepDone {
            run: d.uv()?,
            action: d.u32()?,
            redo: d.bool()?,
            placed: d.evidence()?,
            displaced: d.displaced()?,
        },
        11 => Record::StepConflict {
            run: d.uv()?,
            action: d.u32()?,
            reason: d.string()?,
            settled: d.bool()?,
        },
        12 => Record::RunEnd {
            run: d.uv()?,
            done: d.u32()?,
            conflicts: d.u32()?,
            interrupted: d.bool()?,
        },
        _ => return None,
    };
    (d.pos == payload.len()).then_some(record)
}

/// CRC-32 (IEEE 802.3), table-driven.
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });
    let mut crc = !0u32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

pub fn frame(record: &Record, out: &mut Vec<u8>) {
    let payload = encode(record);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
}

/// Result of scanning a journal image.
pub struct Scan {
    pub records: Vec<Record>,
    /// Byte offset just past the last valid record.
    pub valid_end: u64,
    /// Bytes after `valid_end` that did not form a valid record.
    pub torn_bytes: u64,
}

pub fn scan_bytes(data: &[u8]) -> io::Result<Scan> {
    if data.len() < HEADER_LEN as usize || &data[..8] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an ish-undo journal",
        ));
    }
    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported journal version {version}"),
        ));
    }
    let mut pos = HEADER_LEN as usize;
    let mut records = Vec::new();
    while pos + 8 <= data.len() {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        if len > MAX_RECORD || pos + 8 + len as usize > data.len() {
            break;
        }
        let payload = &data[pos + 8..pos + 8 + len as usize];
        if crc32(payload) != crc {
            break;
        }
        match decode(payload) {
            Some(record) => records.push(record),
            None => break,
        }
        pos += 8 + len as usize;
    }
    Ok(Scan {
        records,
        valid_end: pos as u64,
        torn_bytes: (data.len() - pos) as u64,
    })
}

pub fn read(path: &Path) -> io::Result<Scan> {
    scan_bytes(&std::fs::read(path)?)
}

/// Read only the first record, which is always `Begin`. Maintenance uses it
/// to learn a transaction's age without loading its whole journal.
pub fn read_begin(path: &Path) -> io::Result<Begin> {
    let file = File::open(path)?;
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let mut head = [0u8; HEADER_LEN as usize + 8];
    if sys::read_full_at(file.as_fd(), &mut head, 0)? < head.len() || &head[..8] != MAGIC {
        return Err(bad("not an ish-undo journal"));
    }
    if u32::from_le_bytes(head[8..12].try_into().unwrap()) != VERSION {
        return Err(bad("unsupported journal version"));
    }
    let len = u32::from_le_bytes(head[12..16].try_into().unwrap());
    let crc = u32::from_le_bytes(head[16..20].try_into().unwrap());
    if len > MAX_RECORD {
        return Err(bad("corrupt journal"));
    }
    let mut payload = vec![0u8; len as usize];
    if sys::read_full_at(file.as_fd(), &mut payload, head.len() as u64)? < payload.len()
        || crc32(&payload) != crc
    {
        return Err(bad("corrupt journal"));
    }
    match decode(&payload) {
        Some(Record::Begin(begin)) => Ok(begin),
        _ => Err(bad("journal does not start with a begin record")),
    }
}

/// Create a new journal containing `records`, failing if it exists.
pub fn create(path: &Path, records: &[Record]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    for r in records {
        frame(r, &mut buf);
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    write_all_at(&file, &buf, 0)?;
    sys::sync_file(file.as_fd())?;
    Ok(())
}

fn write_all_at(file: &File, mut buf: &[u8], mut pos: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = rustix::io::pwrite(file, buf, pos)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        pos += n as u64;
    }
    Ok(())
}

/// Appending handle for one process. Each process opens its own descriptor,
/// so `flock` provides mutual exclusion between pipeline and substitution
/// children; a descriptor inherited across `fork` would share one lock.
pub struct Writer {
    file: File,
    pid: u32,
    /// End of the last validated record this process has seen.
    known_end: u64,
}

impl Writer {
    pub fn open(path: &Path) -> io::Result<Writer> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        Ok(Writer {
            file,
            pid: std::process::id(),
            known_end: HEADER_LEN,
        })
    }

    /// Whether this handle was opened by the current process.
    pub fn owned_by_current_process(&self) -> bool {
        self.pid == std::process::id()
    }

    /// Append records under the journal lock. `sync` makes them durable
    /// before returning, which prepare records require.
    pub fn append(&mut self, records: &[Record], sync: bool) -> io::Result<()> {
        let mut buf = Vec::with_capacity(256);
        for r in records {
            frame(r, &mut buf);
        }
        fault::check("journal-append")?;
        rustix::fs::flock(&self.file, rustix::fs::FlockOperation::LockExclusive)?;
        let result = self.append_locked(&buf, sync);
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
        result
    }

    fn append_locked(&mut self, buf: &[u8], sync: bool) -> io::Result<()> {
        let len = rustix::fs::fstat(&self.file)?.st_size as u64;
        let mut end = len;
        if len != self.known_end {
            // Other processes appended since we last looked. Validate only
            // the new bytes. A torn tail can only come from a writer that
            // died mid-write while holding this lock; cut it so new records
            // stay reachable.
            let start = self.known_end.min(len);
            let mut tail = vec![0u8; (len - start) as usize];
            let n = sys::read_full_at(self.file.as_fd(), &mut tail, start)?;
            tail.truncate(n);
            let valid = scan_tail(&tail);
            end = start + valid as u64;
            if end < len {
                rustix::fs::ftruncate(&self.file, end)?;
            }
        }
        write_all_at(&self.file, buf, end)?;
        if sync {
            sys::sync_file(self.file.as_fd())?;
        }
        self.known_end = end + buf.len() as u64;
        fault::check("journal-appended")?;
        Ok(())
    }
}

/// Length of the valid record prefix of `data` (which starts at a record
/// boundary).
fn scan_tail(data: &[u8]) -> usize {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        if len > MAX_RECORD || pos + 8 + len as usize > data.len() {
            break;
        }
        let payload = &data[pos + 8..pos + 8 + len as usize];
        if crc32(payload) != crc || decode(payload).is_none() {
            break;
        }
        pos += 8 + len as usize;
    }
    pos
}

/// Logical bytes of the stored objects `records` reference: saved
/// pre-images, frozen post-images, and versions replay displaced or prepared
/// to displace, each object counted once. Read from the journal, so it costs
/// no filesystem walk.
pub fn retained_logical(records: &[Record]) -> u64 {
    fn displaced_object(d: &Displaced) -> Option<(&ObjRef, u64)> {
        match d {
            Displaced::Saved(s) => s.obj().map(|obj| (obj, s.ident.size)),
            _ => None,
        }
    }
    fn count<'a>(
        seen: &mut std::collections::HashSet<(u64, &'a [u8])>,
        total: &mut u64,
        obj: &'a ObjRef,
        size: u64,
    ) {
        if seen.insert((obj.store, obj.name.as_slice())) {
            *total = total.saturating_add(size);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    for record in records {
        match record {
            Record::Prepare { actions, .. } => {
                for saved in actions.iter().filter_map(Action::saved) {
                    if let Some(obj) = saved.obj() {
                        count(&mut seen, &mut total, obj, saved.ident.size);
                    }
                }
            }
            Record::Final {
                evidence: Evidence::Frozen { ident, obj },
                ..
            } => count(&mut seen, &mut total, obj, ident.size),
            Record::StepPrepare { displaced, .. } | Record::StepDone { displaced, .. } => {
                if let Some((obj, size)) = displaced_object(displaced) {
                    count(&mut seen, &mut total, obj, size);
                }
            }
            _ => {}
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> Ident {
        Ident {
            dev: 1,
            ino: u64::MAX,
            kind: Kind::File,
            size: 42,
            mtime: (-5, 999_999_999),
            ctime: (1_700_000_000, 1),
        }
    }

    fn sample() -> Vec<Record> {
        let saved = Saved {
            content: Content::File {
                obj: ObjRef {
                    store: 7,
                    name: b"obj/1-2".to_vec(),
                },
                strength: Strength::Link,
            },
            ident: ident(),
            meta: Meta {
                mode: 0o4755,
                xattrs: vec![(b"user.k".to_vec(), vec![0, 1, 255])],
                xattrs_complete: true,
                ..Meta::default()
            },
            nlink: 3,
            copied: 0,
        };
        vec![
            Record::Begin(Begin {
                id: 9,
                session: u64::MAX,
                shell_pid: 1,
                started_ns: 2,
                cwd: vec![b'/', 0xff, 0xfe],
                command: b"rm -r x".to_vec(),
                detached: false,
            }),
            Record::Prepare {
                op: 1,
                actions: vec![
                    Action::Unlink {
                        path: vec![b'/', 0x80],
                        saved: saved.clone(),
                    },
                    Action::Rename {
                        from: b"/a".to_vec(),
                        to: b"/b".to_vec(),
                        ident: ident(),
                    },
                    Action::Write {
                        path: b"/c".to_vec(),
                        ident: ident(),
                        saved: None,
                    },
                ],
            },
            Record::Commit {
                op: 1,
                done: 2,
                error: Some("No space left".into()),
            },
            Record::RunBegin {
                run: 3,
                redo: true,
                started_ns: 7,
            },
            Record::StepPrepare {
                run: 3,
                action: 1,
                stage: Some(Stage {
                    path: vec![b'/', 0xff, b'.', b's'],
                    ident: ident(),
                }),
                displaced: Displaced::None,
            },
            Record::StepConflict {
                run: 3,
                action: 2,
                reason: "changed while being removed".into(),
                settled: true,
            },
            Record::StepDone {
                run: 3,
                action: 1,
                redo: true,
                placed: Evidence::Frozen {
                    ident: ident(),
                    obj: ObjRef {
                        store: 0,
                        name: b"obj/x".to_vec(),
                    },
                },
                displaced: Displaced::Saved(saved),
            },
            Record::End {
                status: -1,
                finished_ns: 5,
                interrupted: true,
            },
        ]
    }

    #[test]
    fn records_round_trip_losslessly() {
        for record in sample() {
            assert_eq!(decode(&encode(&record)), Some(record));
        }
    }

    #[test]
    fn scan_stops_at_torn_or_corrupt_tail() {
        let mut data = MAGIC.to_vec();
        data.extend_from_slice(&VERSION.to_le_bytes());
        let records = sample();
        for r in &records {
            frame(r, &mut data);
        }
        let full = scan_bytes(&data).unwrap();
        assert_eq!(full.records, records);
        assert_eq!(full.torn_bytes, 0);

        // Every truncation point yields a valid prefix and never invents a record.
        for cut in HEADER_LEN as usize..data.len() {
            let scan = scan_bytes(&data[..cut]).unwrap();
            assert!(records.starts_with(&scan.records));
            assert_eq!(scan.valid_end + scan.torn_bytes, cut as u64);
        }

        // A flipped payload bit invalidates that record and everything after.
        let mut corrupt = data.clone();
        let second = HEADER_LEN as usize + 8 + encode(&records[0]).len() + 8;
        corrupt[second + 3] ^= 0x10;
        let scan = scan_bytes(&corrupt).unwrap();
        assert_eq!(scan.records, records[..1]);
    }

    #[test]
    fn begin_is_readable_without_the_rest_of_the_journal() {
        let dir = std::env::temp_dir().join(format!(
            "ish-undo-journal-{}-{}",
            std::process::id(),
            sys::now_ns()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("journal");
        let records = sample();
        create(&path, &records).unwrap();
        let Record::Begin(begin) = &records[0] else {
            unreachable!()
        };
        assert_eq!(&read_begin(&path).unwrap(), begin);

        // Garbage, a wrong version, and a truncated first record are errors,
        // never a guess.
        let good = std::fs::read(&path).unwrap();
        for (name, bytes) in [
            ("garbage", b"not a journal at all, really".to_vec()),
            ("version", {
                let mut b = good.clone();
                b[8] = 99;
                b
            }),
            ("truncated", good[..HEADER_LEN as usize + 12].to_vec()),
            ("corrupt", {
                let mut b = good.clone();
                b[HEADER_LEN as usize + 10] ^= 0x40;
                b
            }),
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(read_begin(&path).is_err(), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn retained_size_counts_each_object_once() {
        let obj = |n: &str| ObjRef {
            store: 1,
            name: n.as_bytes().to_vec(),
        };
        let saved = |n: &str, size: u64| Saved {
            content: Content::File {
                obj: obj(n),
                strength: Strength::Clone,
            },
            ident: Ident { size, ..ident() },
            meta: Meta::default(),
            nlink: 1,
            copied: 0,
        };
        let records = vec![
            Record::Prepare {
                op: 1,
                actions: vec![
                    Action::Unlink {
                        path: b"/a".to_vec(),
                        saved: saved("obj/a", 100),
                    },
                    // A second name of the same inode shares its version.
                    Action::Unlink {
                        path: b"/b".to_vec(),
                        saved: saved("obj/a", 100),
                    },
                    Action::Write {
                        path: b"/c".to_vec(),
                        ident: ident(),
                        saved: Some(saved("obj/c", 30)),
                    },
                ],
            },
            Record::Final {
                path: b"/c".to_vec(),
                evidence: Evidence::Frozen {
                    ident: Ident { size: 7, ..ident() },
                    obj: obj("obj/post"),
                },
            },
            Record::StepPrepare {
                run: 1,
                action: 2,
                stage: None,
                displaced: Displaced::Saved(saved("obj/d", 5)),
            },
            Record::StepDone {
                run: 1,
                action: 2,
                redo: false,
                placed: Evidence::Absent,
                displaced: Displaced::Saved(saved("obj/d", 5)),
            },
        ];
        assert_eq!(retained_logical(&records), 100 + 30 + 7 + 5);
    }

    #[test]
    fn crc32_matches_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
