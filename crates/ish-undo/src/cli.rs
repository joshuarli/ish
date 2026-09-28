//! The `undo` builtin.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::journal::{Content, Saved, Strength};
use crate::replay::{self, Committed, Lifecycle, Model};
use crate::retention;
use crate::store::{self, Home, Stores, VolumeState};
use crate::{Config, Io, human_bytes, sys};

pub const USAGE: &str = "\
usage: undo [id] [--dry-run]
       undo redo [id] [--dry-run]
       undo list [-a]
       undo show [id]
       undo gc
       undo purge <id>
       undo doctor
       undo volume add <directory> | list | remove <directory|id>";

/// Subcommands, for completion.
pub const SUBCOMMANDS: &[&str] = &["doctor", "gc", "list", "purge", "redo", "show", "volume"];

/// What the shell tells the builtin about where it runs.
pub struct Context {
    pub home_root: PathBuf,
    pub config: Config,
    pub session: u64,
    pub shell_pid: u32,
    pub cwd: PathBuf,
    /// False inside pipeline stages and command substitutions, where
    /// commands that change recovery state are refused.
    pub may_mutate: bool,
}

fn is_readonly(sub: &str) -> bool {
    matches!(
        sub,
        "list" | "show" | "doctor" | "help" | "-h" | "--help" | "volume-list"
    )
}

/// Run the builtin and return its exit status.
pub fn main(ctx: &Context, args: &[OsString], io: &mut Io<'_>) -> i32 {
    let sub = args.first().and_then(|a| a.to_str()).unwrap_or("");
    let sub_key = if sub == "volume" && args.get(1).and_then(|a| a.to_str()) == Some("list") {
        "volume-list"
    } else {
        sub
    };
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    let is_sub = matches!(
        sub,
        "redo" | "list" | "show" | "gc" | "purge" | "doctor" | "volume" | "help" | "-h" | "--help"
    );
    if !ctx.may_mutate && !is_readonly(sub_key) {
        let name = if is_sub { sub } else { "undo" };
        let _ = writeln!(
            io.err,
            "undo: {name} changes recovery state and must run in the shell itself, not in a pipeline or command substitution"
        );
        return 2;
    }
    match sub {
        "help" | "-h" | "--help" => {
            let _ = writeln!(io.out, "{USAGE}");
            0
        }
        "redo" => replay_command(ctx, rest, true, io),
        "list" => list(ctx, rest, io),
        "show" => show(ctx, rest, io),
        "gc" => gc(ctx, io),
        "purge" => purge(ctx, rest, io),
        "doctor" => doctor(ctx, io),
        "volume" => volume(ctx, rest, io),
        _ => replay_command(ctx, args, false, io),
    }
}

fn open_home(ctx: &Context, io: &mut Io<'_>) -> Option<Home> {
    match Home::open_existing(&ctx.home_root) {
        Ok(Some(home)) => Some(home),
        Ok(None) => {
            let _ = writeln!(io.err, "undo: nothing has been recorded yet");
            None
        }
        Err(e) => {
            let _ = writeln!(io.err, "undo: {}: {e}", ctx.home_root.display());
            None
        }
    }
}

fn parse_id(s: &OsStr) -> Option<u64> {
    s.to_str()?.parse().ok()
}

fn models_desc(home: &Home) -> Vec<Model> {
    let mut ids = home.txn_ids().unwrap_or_default();
    ids.reverse();
    ids.into_iter()
        .filter_map(|id| Model::load(home, id).ok())
        .collect()
}

fn select_undo(ctx: &Context, home: &Home) -> Option<u64> {
    models_desc(home)
        .into_iter()
        .find(|m| m.begin.session == ctx.session && m.end.is_some() && m.undoable() > 0)
        .map(|m| m.id)
}

fn select_redo(ctx: &Context, home: &Home) -> Option<u64> {
    models_desc(home)
        .into_iter()
        .filter(|m| m.begin.session == ctx.session && m.redoable() > 0)
        .max_by_key(|m| m.last_run().map(|r| r.started_ns).unwrap_or(0))
        .map(|m| m.id)
}

fn command_text(model: &Model) -> String {
    String::from_utf8_lossy(&model.begin.command)
        .trim()
        .to_string()
}

/// Dry-run output is a plan of one line per step; a large removal has
/// thousands, so only the first are shown.
const DRY_RUN_LINES: usize = 200;

fn replay_command(ctx: &Context, args: &[OsString], redo: bool, io: &mut Io<'_>) -> i32 {
    let name = if redo { "undo redo" } else { "undo" };
    let mut id = None;
    let mut dry_run = false;
    for arg in args {
        match arg.as_bytes() {
            b"--dry-run" | b"-n" => dry_run = true,
            _ if id.is_none() && parse_id(arg).is_some() => id = parse_id(arg),
            _ => {
                let _ = writeln!(
                    io.err,
                    "{name}: unexpected argument {}\n{USAGE}",
                    arg.to_string_lossy()
                );
                return 2;
            }
        }
    }
    let Some(home) = open_home(ctx, io) else {
        return 1;
    };
    let id = match id.or_else(|| {
        if redo {
            select_redo(ctx, &home)
        } else {
            select_undo(ctx, &home)
        }
    }) {
        Some(id) => id,
        None => {
            let _ = writeln!(
                io.err,
                "{name}: nothing to {} in this session (see `undo list` for other sessions)",
                if redo { "redo" } else { "undo" }
            );
            return 1;
        }
    };
    let model = match Model::load(&home, id) {
        Ok(m) => m,
        Err(e) => {
            let _ = writeln!(
                io.err,
                "{name}: transaction {id}: {}",
                sys::describe_error(&e)
            );
            return 1;
        }
    };
    match replay::lifecycle(&home, &model) {
        Lifecycle::Active => {
            let _ = writeln!(io.err, "{name}: transaction {id} is still active");
            return 1;
        }
        Lifecycle::Replaying => {
            let _ = writeln!(
                io.err,
                "{name}: transaction {id} is being replayed by another shell"
            );
            return 1;
        }
        Lifecycle::Interrupted => {
            let _ = writeln!(
                io.err,
                "{name}: transaction {id} was interrupted; recovering from what was recorded"
            );
        }
        Lifecycle::Completed => {}
    }
    let opts = replay::Options {
        redo,
        dry_run,
        copy_limit: ctx.config.copy_limit,
        min_free: ctx.config.min_free,
        cancel: io.cancel,
    };
    let report = match replay::run(&home, id, &opts) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(
                io.err,
                "{name}: transaction {id}: {}",
                sys::describe_error(&e)
            );
            return 1;
        }
    };
    let cmd = command_text(&model);
    if dry_run {
        let _ = writeln!(io.out, "{name}: dry run for transaction {id} ({cmd}):");
        for step in report.planned.iter().take(DRY_RUN_LINES) {
            let _ = writeln!(io.out, "  {step}");
        }
        if report.planned.len() > DRY_RUN_LINES {
            let _ = writeln!(
                io.out,
                "  ... and {} more",
                report.planned.len() - DRY_RUN_LINES
            );
        }
        if report.planned.is_empty() {
            let _ = writeln!(io.out, "  nothing to do");
        }
    } else {
        let total = report.done + report.already;
        let verb = if redo { "reapplied" } else { "reverted" };
        if total > 0 || (report.conflicts.is_empty() && report.failures.is_empty()) {
            let _ = writeln!(
                io.err,
                "{name}: {verb} {total} change{} from transaction {id} ({cmd})",
                if total == 1 { "" } else { "s" }
            );
        }
    }
    for note in &report.notes {
        let _ = writeln!(io.err, "{name}: note: {note}");
    }
    if !report.conflicts.is_empty() {
        let _ = writeln!(
            io.err,
            "{name}: {} conflict{} (left untouched):",
            report.conflicts.len(),
            if report.conflicts.len() == 1 { "" } else { "s" }
        );
        for (step, why) in &report.conflicts {
            let _ = writeln!(io.err, "  {step}: {why}");
        }
        if !dry_run {
            let _ = writeln!(
                io.err,
                "  nothing newer was overwritten; resolve the conflicts, then run `{name} {id}` again"
            );
        }
    }
    for (step, why) in &report.failures {
        let _ = writeln!(io.err, "{name}: {step}: {why}");
    }
    if report.interrupted {
        let _ = writeln!(io.err, "{name}: interrupted; run it again to continue");
        return 130;
    }
    (!report.conflicts.is_empty() || !report.failures.is_empty()) as i32
}

/// Display labels for a transaction.
struct Labels {
    state: String,
    flags: Vec<String>,
}

fn labels(home: &Home, stores: &mut Stores, model: &Model) -> Labels {
    let life = replay::lifecycle(home, model);
    let undoable = model.undoable();
    let redoable = model.redoable();
    let state = match life {
        Lifecycle::Active => "active".into(),
        Lifecycle::Interrupted => "interrupted".into(),
        Lifecycle::Replaying => "replaying".into(),
        Lifecycle::Completed => match model.last_run() {
            Some(run) if run.redo && undoable > 0 => "redone".into(),
            _ if undoable == 0 && redoable > 0 => "undone".into(),
            _ if undoable > 0 && redoable > 0 => "partly undone".into(),
            _ => "completed".into(),
        },
    };
    let mut flags = Vec::new();
    let partial = model.actions.iter().any(|a| a.committed != Committed::Done)
        || model.end.is_some_and(|(_, _, interrupted)| interrupted)
        || model.torn_bytes > 0;
    if partial {
        flags.push("partial".to_string());
    }
    if model.actions.iter().any(|a| a.conflict.is_some()) {
        flags.push("conflict".into());
    }
    if model.actions.iter().any(|a| a.pending.is_some()) {
        flags.push("replay interrupted".into());
    }
    if model.has_linked() {
        flags.push("linked".into());
    }
    if retention::unavailable(stores, model) {
        flags.push("unavailable".into());
    }
    if model.opaque > 0 {
        flags.push(format!("{} uncaptured", model.opaque));
    }
    Labels { state, flags }
}

fn age(ns: u64) -> String {
    let secs = sys::now_ns().saturating_sub(ns) / 1_000_000_000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// UTC calendar time for display, from nanoseconds since the epoch.
fn utc(ns: u64) -> String {
    let secs = (ns / 1_000_000_000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn list(ctx: &Context, args: &[OsString], io: &mut Io<'_>) -> i32 {
    let all = args.iter().any(|a| a == "-a" || a == "--all");
    if let Some(bad) = args.iter().find(|a| *a != "-a" && *a != "--all") {
        let _ = writeln!(
            io.err,
            "undo list: unexpected argument {}",
            bad.to_string_lossy()
        );
        return 2;
    }
    let home = match Home::open_existing(&ctx.home_root) {
        Ok(Some(home)) => home,
        Ok(None) => return 0,
        Err(e) => {
            let _ = writeln!(io.err, "undo list: {e}");
            return 1;
        }
    };
    let mut stores = match Stores::new(home.clone(), 0) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(io.err, "undo list: {e}");
            return 1;
        }
    };
    let mut ids = home.txn_ids().unwrap_or_default();
    if !all && ids.len() > 30 {
        ids.drain(..ids.len() - 30);
    }
    let _ = writeln!(
        io.out,
        "  {:>5}  {:>4}  {:<13} {:>7} {:>6}  COMMAND",
        "ID", "AGE", "STATE", "CHANGES", "SIZE"
    );
    for id in ids {
        let model = match Model::load(&home, id) {
            Ok(m) => m,
            Err(e) => {
                let _ = writeln!(
                    io.out,
                    "  {id:>5}  unreadable: {}; `undo purge {id}` removes it",
                    sys::describe_error(&e)
                );
                continue;
            }
        };
        let l = labels(&home, &mut stores, &model);
        let mine = if model.begin.session == ctx.session {
            '*'
        } else {
            ' '
        };
        let mut command = command_text(&model);
        if !l.flags.is_empty() {
            command = format!("{command}  [{}]", l.flags.join(", "));
        }
        let _ = writeln!(
            io.out,
            "{mine} {id:>5}  {:>4}  {:<13} {:>7} {:>6}  {command}",
            age(model.begin.started_ns),
            l.state,
            model
                .actions
                .iter()
                .filter(|a| a.committed != Committed::NotDone)
                .count(),
            human_bytes(model.retained),
        );
    }
    0
}

fn pick_id(
    ctx: &Context,
    home: &Home,
    args: &[OsString],
    name: &str,
    io: &mut Io<'_>,
) -> Option<u64> {
    match args {
        [] => {
            let found = models_desc(home)
                .into_iter()
                .find(|m| m.begin.session == ctx.session && m.end.is_some())
                .map(|m| m.id);
            if found.is_none() {
                let _ = writeln!(
                    io.err,
                    "undo {name}: no completed transaction in this session"
                );
            }
            found
        }
        [id] => match parse_id(id) {
            Some(id) if home.txn_dir(id).exists() => Some(id),
            _ => {
                let _ = writeln!(
                    io.err,
                    "undo {name}: no transaction {}",
                    id.to_string_lossy()
                );
                None
            }
        },
        _ => {
            let _ = writeln!(io.err, "undo {name}: expected at most one id\n{USAGE}");
            None
        }
    }
}

fn saved_label(saved: &Saved) -> String {
    match &saved.content {
        Content::File { strength, .. } => match strength {
            Strength::Link => "linked version: retained inode, not frozen".into(),
            s => format!("{} version, {}", s.name(), human_bytes(saved.ident.size)),
        },
        Content::Symlink { target } => format!("symlink to {}", String::from_utf8_lossy(target)),
        Content::Special { .. } => "special file, linked".into(),
    }
}

fn show(ctx: &Context, args: &[OsString], io: &mut Io<'_>) -> i32 {
    let Some(home) = open_home(ctx, io) else {
        return 1;
    };
    let Some(id) = pick_id(ctx, &home, args, "show", io) else {
        return 1;
    };
    let model = match Model::load(&home, id) {
        Ok(m) => m,
        Err(e) => {
            let _ = writeln!(io.err, "undo show: {e}");
            return 1;
        }
    };
    let mut stores = match Stores::new(home.clone(), id) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(io.err, "undo show: {e}");
            return 1;
        }
    };
    let l = labels(&home, &mut stores, &model);
    let usage = retention::usage(&mut stores, id);
    let out = &mut *io.out;
    let _ = writeln!(
        out,
        "transaction {id}{}",
        if model.begin.session == ctx.session {
            " (this session)"
        } else {
            ""
        }
    );
    let _ = writeln!(out, "command:  {}", command_text(&model));
    let _ = writeln!(
        out,
        "cwd:      {}",
        String::from_utf8_lossy(&model.begin.cwd)
    );
    let _ = writeln!(out, "started:  {}", utc(model.begin.started_ns));
    match model.end {
        Some((status, finished, _)) => {
            let _ = writeln!(out, "finished: {} (status {status})", utc(finished));
        }
        None => {
            let _ = writeln!(out, "finished: not recorded");
        }
    }
    let coverage = if model.opaque > 0 {
        format!(
            "ish's rm, mv, and redirections only; {} other command{} ran without capture",
            model.opaque,
            if model.opaque == 1 { "" } else { "s" }
        )
    } else {
        "ish's rm, mv, and redirections only".to_string()
    };
    let _ = writeln!(out, "capture:  {coverage}");
    let _ = writeln!(
        out,
        "state:    {}{}",
        l.state,
        if l.flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", l.flags.join(", "))
        }
    );
    let copied: u64 = model.saved_versions().map(|s| s.copied).sum();
    let _ = writeln!(
        out,
        "stored:   {} logical, {} allocated, {} copied",
        human_bytes(model.retained),
        human_bytes(usage.allocated),
        human_bytes(copied)
    );
    let mut counts = [0usize; 3];
    for s in model.saved_versions() {
        match s.strength() {
            Some(Strength::Clone) => counts[0] += 1,
            Some(Strength::Copy) => counts[1] += 1,
            Some(Strength::Link) => counts[2] += 1,
            None => {}
        }
    }
    let _ = writeln!(
        out,
        "versions: {} clone, {} copy, {} linked",
        counts[0], counts[1], counts[2]
    );
    if counts[2] > 0 {
        let _ = writeln!(
            out,
            "          linked versions retain the original inode; another hard link or an open writer can still change them"
        );
    }
    let _ = writeln!(out, "changes:");
    for state in &model.actions {
        let status = match (state.committed, state.applied, &state.conflict) {
            (Committed::NotDone, _, _) => "not done".to_string(),
            (_, _, Some(why)) => format!("conflict: {why}"),
            (Committed::Ambiguous, true, _) => "uncertain".to_string(),
            (_, true, _) => "applied".to_string(),
            (_, false, _) => "undone".to_string(),
        };
        let status = if state.pending.is_some() && state.committed != Committed::NotDone {
            format!("{status}; a replay step was interrupted")
        } else {
            status
        };
        let mut line = replay::describe_action(&state.action);
        if let Some(saved) = state.action.saved() {
            line.push_str(&format!(" ({})", saved_label(saved)));
        }
        let _ = writeln!(out, "  [{status}] {line}");
    }
    if model.actions.is_empty() {
        let _ = writeln!(out, "  none");
    }
    if !model.notes.is_empty() {
        let _ = writeln!(out, "notes:");
        for note in &model.notes {
            let _ = writeln!(out, "  {note}");
        }
    }
    if model.torn_bytes > 0 {
        let _ = writeln!(
            out,
            "journal:  {} trailing bytes were incomplete and ignored",
            model.torn_bytes
        );
    }
    0
}

fn gc(ctx: &Context, io: &mut Io<'_>) -> i32 {
    let Ok(Some(home)) = Home::open_existing(&ctx.home_root) else {
        let _ = writeln!(io.out, "undo gc: nothing stored");
        return 0;
    };
    match retention::collect(&home, &ctx.config, None) {
        Ok(report) => {
            let _ = writeln!(
                io.out,
                "undo gc: deleted {} transaction{} ({}), kept {} protected, removed {} orphaned volume director{}",
                report.deleted.len(),
                if report.deleted.len() == 1 { "" } else { "s" },
                human_bytes(report.freed_logical),
                report.kept_protected,
                report.orphans_removed,
                if report.orphans_removed == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            0
        }
        Err(e) => {
            let _ = writeln!(io.err, "undo gc: {e}");
            1
        }
    }
}

fn purge(ctx: &Context, args: &[OsString], io: &mut Io<'_>) -> i32 {
    let [arg] = args else {
        let _ = writeln!(io.err, "undo purge: expected one transaction id\n{USAGE}");
        return 2;
    };
    let Some(home) = open_home(ctx, io) else {
        return 1;
    };
    let Some(id) = parse_id(arg).filter(|id| home.txn_dir(*id).exists()) else {
        let _ = writeln!(
            io.err,
            "undo purge: no transaction {}",
            arg.to_string_lossy()
        );
        return 1;
    };
    if let Ok(model) = Model::load(&home, id)
        && replay::lifecycle(&home, &model) == Lifecycle::Active
    {
        let _ = writeln!(io.err, "undo purge: transaction {id} is still active");
        return 1;
    }
    let _lock = match home.lock() {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(io.err, "undo purge: {e}");
            return 1;
        }
    };
    match retention::purge(&home, id) {
        Ok(true) => {
            let _ = writeln!(
                io.out,
                "undo purge: deleted transaction {id} and its saved versions"
            );
            0
        }
        Ok(false) => {
            let _ = writeln!(
                io.err,
                "undo purge: transaction {id} is being replayed by another shell"
            );
            1
        }
        Err(e) => {
            let _ = writeln!(io.err, "undo purge: {e}");
            1
        }
    }
}

/// Capability probes in a scratch directory inside a store.
fn probe(dir: &Path, out: &mut dyn Write) {
    let scratch = dir.join(format!("probe-{:016x}", sys::random_u64()));
    if let Err(e) = std::fs::create_dir(&scratch) {
        let _ = writeln!(out, "    probe: cannot create scratch directory: {e}");
        return;
    }
    let result = (|| -> io::Result<Vec<(String, String)>> {
        let fd = sys::open_dir(&scratch)?;
        let mut results = Vec::new();
        let a = sys::create_excl_at(fd.as_fd(), c"a", 0o600)?;
        rustix::io::write(&a, b"ish-undo probe")?;
        let mut record = |name: &str, r: io::Result<()>| {
            results.push((
                name.to_string(),
                match r {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("failed ({e})"),
                },
            ));
        };
        let src = sys::open_read_at(fd.as_fd(), c"a")?;
        let cloned = sys::clone_file(src.as_fd(), fd.as_fd(), c"b");
        let isolated = cloned.as_ref().ok().map(|()| {
            // Writing the source must not change the clone.
            let w = rustix::fs::openat(
                fd.as_fd(),
                c"a",
                rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            );
            let _ = w.and_then(|w| rustix::io::pwrite(&w, b"X", 0).map(|_| ()));
            let b = sys::open_read_at(fd.as_fd(), c"b").ok();
            let mut buf = [0u8; 1];
            b.and_then(|b| sys::read_full_at(b.as_fd(), &mut buf, 0).ok())
                .map(|_| buf[0] == b'i')
                .unwrap_or(false)
        });
        record(
            "clone",
            match (cloned, isolated) {
                (Ok(()), Some(true)) => Ok(()),
                (Ok(()), _) => Err(io::Error::other("clone is not isolated")),
                (Err(e), _) => Err(e),
            },
        );
        record(
            "hard link",
            sys::link_at(fd.as_fd(), c"a", fd.as_fd(), c"c"),
        );
        let _ = sys::create_excl_at(fd.as_fd(), c"d", 0o600);
        record(
            "no-replace rename",
            match sys::rename_noreplace(fd.as_fd(), c"d", fd.as_fd(), c"a") {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(e),
                Ok(()) => Err(io::Error::other("replaced an existing entry")),
            },
        );
        let xname = if cfg!(target_os = "linux") {
            c"user.ish-undo-probe"
        } else {
            c"ish-undo-probe"
        };
        record(
            "xattr",
            rustix::fs::fsetxattr(&a, xname, b"1", rustix::fs::XattrFlags::empty())
                .map_err(Into::into),
        );
        record(
            "fsync",
            sys::sync_file(a.as_fd()).and_then(|()| sys::sync_dir(fd.as_fd())),
        );
        Ok(results)
    })();
    let _ = sys::open_dir(dir)
        .and_then(|d| sys::remove_tree(d.as_fd(), &sys::os_cstr(scratch.file_name().unwrap())?));
    match result {
        Ok(results) => {
            let line: Vec<String> = results.iter().map(|(k, v)| format!("{k} {v}")).collect();
            let _ = writeln!(out, "    probes: {}", line.join(", "));
        }
        Err(e) => {
            let _ = writeln!(out, "    probe failed: {e}");
        }
    }
}

fn describe_store(path: &Path, min_free: u64, out: &mut dyn Write) {
    match sys::open_dir(path) {
        Ok(fd) => {
            let st = sys::fstat(fd.as_fd());
            let free = sys::free_bytes(fd.as_fd()).unwrap_or(0);
            let _ = writeln!(
                out,
                "    filesystem {} (device {}), mode {:o}, {} free (reserve {}){}",
                sys::fs_type_name(fd.as_fd()),
                st.as_ref().map(|s| s.dev).unwrap_or(0),
                st.as_ref().map(|s| s.mode).unwrap_or(0),
                human_bytes(free),
                human_bytes(min_free),
                if free < min_free {
                    " — below the reserve"
                } else {
                    ""
                }
            );
            probe(path, out);
        }
        Err(e) => {
            let _ = writeln!(out, "    cannot open: {e}");
        }
    }
}

fn doctor(ctx: &Context, io: &mut Io<'_>) -> i32 {
    let out = &mut *io.out;
    let _ = writeln!(out, "home store: {}", ctx.home_root.display());
    let home = match Home::open_existing(&ctx.home_root) {
        Ok(Some(home)) => home,
        Ok(None) => {
            let _ = writeln!(
                out,
                "    not created yet; it is created on the first protected change"
            );
            if let Some(parent) = ctx.home_root.ancestors().find(|p| p.exists()) {
                let _ = writeln!(out, "    probing {} instead:", parent.display());
                describe_store(parent, ctx.config.min_free, out);
            }
            return 0;
        }
        Err(e) => {
            let _ = writeln!(out, "    unusable: {e}");
            return 1;
        }
    };
    let _ = writeln!(out, "    format ok, store id {:016x}", home.id);
    describe_store(&home.root, ctx.config.min_free, out);
    let mut stores = match Stores::new(home.clone(), 0) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "    {e}");
            return 1;
        }
    };
    let volumes = stores.volume_list();
    if volumes.is_empty() {
        let _ = writeln!(out, "volume stores: none registered");
    } else {
        let _ = writeln!(out, "volume stores:");
        for (entry, state) in &volumes {
            let _ = writeln!(out, "  {:016x} {}", entry.id, entry.path.display());
            match state {
                VolumeState::Available { .. } => {
                    describe_store(&entry.path, ctx.config.min_free, out);
                    let catalog = home.txn_ids().unwrap_or_default();
                    let orphans = std::fs::read_dir(entry.path.join("txn"))
                        .map(|d| {
                            d.flatten()
                                .filter_map(|e| e.file_name().to_str()?.parse::<u64>().ok())
                                .filter(|id| catalog.binary_search(id).is_err())
                                .count()
                        })
                        .unwrap_or(0);
                    if orphans > 0 {
                        let _ = writeln!(
                            out,
                            "    {orphans} transaction director{} without a catalog entry (left while disconnected); `undo gc` removes them",
                            if orphans == 1 { "y" } else { "ies" }
                        );
                    }
                }
                VolumeState::Missing => {
                    let _ = writeln!(
                        out,
                        "    unavailable: volume not mounted (its versions are kept, not deleted)"
                    );
                }
                VolumeState::Mismatch(why) => {
                    let _ = writeln!(out, "    not usable: {why}");
                }
            }
        }
    }
    let ids = home.txn_ids().unwrap_or_default();
    let (mut active, mut interrupted, mut unreadable, mut torn) = (0, 0, 0, 0);
    let mut logical = 0;
    for &id in &ids {
        match Model::load(&home, id) {
            Ok(model) => {
                match replay::lifecycle(&home, &model) {
                    Lifecycle::Active => active += 1,
                    Lifecycle::Interrupted => interrupted += 1,
                    Lifecycle::Completed | Lifecycle::Replaying => {}
                }
                if model.torn_bytes > 0 {
                    torn += 1;
                }
                logical += model.retained;
            }
            Err(_) => unreadable += 1,
        }
    }
    let _ = writeln!(
        out,
        "transactions: {} ({active} active, {interrupted} interrupted, {unreadable} unreadable, {torn} with incomplete tails), {} retained",
        ids.len(),
        human_bytes(logical)
    );
    let c = &ctx.config;
    let _ = writeln!(
        out,
        "limits: copy {} per operation, {} or {} entries or {} days retained, {} free-space reserve",
        human_bytes(c.copy_limit),
        human_bytes(c.max_bytes),
        c.max_entries,
        c.max_age_days,
        human_bytes(c.min_free)
    );
    0
}

fn volume(ctx: &Context, args: &[OsString], io: &mut Io<'_>) -> i32 {
    let sub = args.first().and_then(|a| a.to_str()).unwrap_or("list");
    match (sub, args.get(1..).unwrap_or(&[])) {
        ("list", []) => {
            let Ok(Some(home)) = Home::open_existing(&ctx.home_root) else {
                return 0;
            };
            let Ok(mut stores) = Stores::new(home, 0) else {
                return 1;
            };
            for (entry, state) in stores.volume_list() {
                let state = match state {
                    VolumeState::Available { dev } => format!("available (device {dev})"),
                    VolumeState::Missing => "unavailable (not mounted)".into(),
                    VolumeState::Mismatch(why) => format!("not usable: {why}"),
                };
                let _ = writeln!(
                    io.out,
                    "{:016x}  {}  {state}",
                    entry.id,
                    entry.path.display()
                );
            }
            0
        }
        ("add", [dir]) => {
            let dir = crate::ops::resolve(&ctx.cwd, dir);
            let dir = match std::fs::canonicalize(&dir) {
                Ok(d) => d,
                Err(e) => {
                    let _ = writeln!(io.err, "undo volume add: {}: {e}", dir.display());
                    return 1;
                }
            };
            let home = match Home::open_or_create(&ctx.home_root) {
                Ok(h) => h,
                Err(e) => {
                    let _ = writeln!(io.err, "undo volume add: {e}");
                    return 1;
                }
            };
            let home_dev = sys::lstat(&home.root).map(|s| s.dev).unwrap_or(0);
            match sys::stat(&dir) {
                Ok(st) if st.kind != sys::Kind::Dir => {
                    let _ = writeln!(
                        io.err,
                        "undo volume add: {}: not a directory",
                        dir.display()
                    );
                    return 1;
                }
                Ok(st) if st.dev == home_dev => {
                    let _ = writeln!(
                        io.err,
                        "undo volume add: {} is on the same filesystem as the home store, which already covers it",
                        dir.display()
                    );
                    return 1;
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = writeln!(io.err, "undo volume add: {}: {e}", dir.display());
                    return 1;
                }
            }
            let lock = match home.lock() {
                Ok(l) => l,
                Err(e) => {
                    let _ = writeln!(io.err, "undo volume add: {e}");
                    return 1;
                }
            };
            let mut entries = home.registry().unwrap_or_default();
            let entry = match store::create_volume(&home, &dir) {
                Ok(e) => e,
                Err(e) => {
                    let _ = writeln!(io.err, "undo volume add: {e}");
                    return 1;
                }
            };
            if !entries.iter().any(|e| e.id == entry.id) {
                entries.push(entry.clone());
                if let Err(e) = home.write_registry(&entries, &lock) {
                    let _ = writeln!(io.err, "undo volume add: {e}");
                    return 1;
                }
            }
            let _ = writeln!(
                io.out,
                "undo volume add: registered {} ({:016x})",
                entry.path.display(),
                entry.id
            );
            0
        }
        ("remove", [target]) => {
            let Some(home) = open_home(ctx, io) else {
                return 1;
            };
            let lock = match home.lock() {
                Ok(l) => l,
                Err(e) => {
                    let _ = writeln!(io.err, "undo volume remove: {e}");
                    return 1;
                }
            };
            let mut entries = home.registry().unwrap_or_default();
            let t = target.as_bytes();
            let resolved = crate::ops::resolve(&ctx.cwd, target);
            let Some(pos) = entries.iter().position(|e| {
                format!("{:016x}", e.id).as_bytes() == t
                    || e.path == resolved
                    || e.path.parent() == Some(resolved.as_path())
            }) else {
                let _ = writeln!(
                    io.err,
                    "undo volume remove: {} is not registered",
                    target.to_string_lossy()
                );
                return 1;
            };
            let store_id = entries[pos].id;
            let users: Vec<u64> = home
                .txn_ids()
                .unwrap_or_default()
                .into_iter()
                .filter(|&id| {
                    Model::load(&home, id).is_ok_and(|m| {
                        m.saved_versions()
                            .any(|s| s.obj().is_some_and(|o| o.store == store_id))
                    })
                })
                .collect();
            if !users.is_empty() {
                let ids: Vec<String> = users.iter().map(u64::to_string).collect();
                let _ = writeln!(
                    io.err,
                    "undo volume remove: transactions {} keep versions there; purge them first",
                    ids.join(", ")
                );
                return 1;
            }
            let removed = entries.remove(pos);
            if let Err(e) = home.write_registry(&entries, &lock) {
                let _ = writeln!(io.err, "undo volume remove: {e}");
                return 1;
            }
            let _ = writeln!(
                io.out,
                "undo volume remove: unregistered {}; its directory was left in place",
                removed.path.display()
            );
            0
        }
        _ => {
            let _ = writeln!(io.err, "{USAGE}");
            2
        }
    }
}
