//! Shell integration for recoverable filesystem operations (`ish-undo`).
//!
//! Each accepted input begins a transaction before evaluation, so command
//! substitutions, pipeline stages, and redirections of that input share it.
//! Native `rm`, `mv`, and `undo` are dispatched from the external handler,
//! after expansion, by exact command name: `/bin/rm` and other explicit
//! paths run the external utility without capture. The transaction is
//! sealed when its foreground job completes; a stopped job keeps it (and any
//! pending scoped run) until it finishes after `fg`.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use epsh::error::{ExitStatus, ShellError};
use epsh::shell_bytes::ShellBytes;
use ish_undo::cli::{self, PendingRun, RunResult};
use ish_undo::{Config, Session, Suspended, Txn};

/// Set by the SIGINT handler while a native builtin runs.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

struct State {
    session: Option<Session>,
    current: Option<Txn>,
    warnings: String,
}

thread_local! {
    static STATE: RefCell<State> = const {
        RefCell::new(State {
            session: None,
            current: None,
            warnings: String::new(),
        })
    };
    /// A scoped run whose program stopped, handed from the external handler
    /// to the main loop like `STOPPED_JOB`.
    static STOPPED_RUN: RefCell<Option<PendingRun>> = const { RefCell::new(None) };
}

/// Recovery work attached to a stopped job.
pub struct JobUndo {
    txn: Option<Suspended>,
    run: Option<PendingRun>,
}

/// Configuration from the shell's variables, reporting invalid values once.
fn config(epsh: &epsh::eval::Shell, warnings: &mut String) -> Config {
    let (config, found) = Config::from_vars(|name| epsh.get_var(name).map(str::to_owned));
    let joined = found.join("\n");
    if joined != *warnings {
        for w in &found {
            eprintln!("ish: {w}");
        }
        *warnings = joined;
    }
    config
}

fn home_root(epsh: &epsh::eval::Shell) -> Option<PathBuf> {
    let home = epsh.get_var_bytes("HOME")?;
    let home = home.to_os_string();
    (!home.is_empty()).then(|| ish_undo::store::root_for_home(&home))
}

/// Start the transaction for an accepted input. No file I/O happens here.
pub fn begin(epsh: &epsh::eval::Shell, command: &str) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let mut warnings = std::mem::take(&mut state.warnings);
        let config = config(epsh, &mut warnings);
        state.warnings = warnings;
        let Some(root) = home_root(epsh) else {
            state.current = None;
            return;
        };
        let session = state.session.get_or_insert_with(Session::new);
        let txn = session.begin(root, config, epsh.cwd(), command);
        state.current = Some(txn);
    });
}

/// Finish the current input. When its job stopped, return the work to keep
/// with the job; otherwise seal the transaction.
pub fn end(status: i32, stopped: bool) -> Option<JobUndo> {
    let run = STOPPED_RUN.with(|r| r.borrow_mut().take());
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let txn = state.current.take();
        let session = state.session.as_mut()?;
        if stopped {
            return Some(JobUndo {
                txn: txn.map(|t| session.suspend(t)),
                run,
            });
        }
        if let Some(run) = run {
            cli::finish_run(run, status, &mut std::io::stderr());
        }
        let txn = txn?;
        let root = txn.home_root.clone();
        let config = txn.config.clone();
        if session.finish(txn, status).is_some() {
            ish_undo::retention::maybe_collect(&root, &config);
        }
        None
    })
}

/// Seal the recovery work of a job that finished after `fg` (or was killed).
pub fn finish_job(work: JobUndo, status: i32) {
    if let Some(run) = work.run {
        cli::finish_run(run, status, &mut std::io::stderr());
    }
    if let Some(txn) = work.txn {
        STATE.with(|state| {
            if let Some(session) = state.borrow_mut().session.as_mut() {
                session.finish_suspended(txn, status);
            }
        });
    }
}

/// Count an external command that runs without capture.
pub fn note_opaque() {
    STATE.with(|state| {
        if let Some(txn) = state.borrow().current.as_ref() {
            txn.note_opaque();
        }
    });
}

fn confirm(question: &str) -> Option<bool> {
    let mut err = std::io::stderr();
    let _ = err.write_all(question.as_bytes());
    let _ = err.flush();
    // SAFETY: fd 0 is the command's standard input for the duration of the
    // read.
    let stdin = unsafe { std::os::fd::BorrowedFd::borrow_raw(0) };
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match rustix::io::read(stdin, &mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(rustix::io::Errno::INTR) => {
                if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = err.write_all(b"\n");
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    if line.is_empty() && byte[0] != b'\n' {
        return None;
    }
    Some(matches!(line.first(), Some(b'y' | b'Y')))
}

fn to_os(args: &[ShellBytes]) -> Vec<OsString> {
    args.iter().map(ShellBytes::to_os_string).collect()
}

/// Run a native builtin if `name` is one and capture is enabled. `spawn`
/// runs a program through the shell's foreground-job machinery. Returns
/// `None` to let the caller run an external command instead.
pub fn dispatch(
    name: &str,
    args: &[ShellBytes],
    is_main: bool,
    spawn: &mut dyn FnMut(&[ShellBytes]) -> epsh::error::Result<ExitStatus>,
) -> Option<epsh::error::Result<ExitStatus>> {
    if !matches!(name, "rm" | "mv" | "undo") {
        return None;
    }
    let cwd = epsh::eval::external_command_cwd()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let argv = to_os(&args[1..]);
    STATE.with(|state| {
        let state = state.borrow();
        let txn = state.current.as_ref();
        if name != "undo" && !txn.is_some_and(|t| t.config.enabled) {
            if txn.is_none() {
                eprintln!("ish: {name}: HOME is not set, so this runs without undo protection");
            }
            return None;
        }
        crate::signal::catch_interrupts(&INTERRUPTED);
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut confirm = confirm;
        let mut io = ish_undo::Io {
            out: &mut out,
            err: &mut err,
            confirm: &mut confirm,
            cancel: &INTERRUPTED,
        };
        let result = match name {
            "rm" => Ok(ExitStatus::from(ish_undo::ops::rm(
                txn.unwrap(),
                &cwd,
                &argv,
                &mut io,
            ))),
            "mv" => Ok(ExitStatus::from(ish_undo::ops::mv(
                txn.unwrap(),
                &cwd,
                &argv,
                &mut io,
            ))),
            _ => {
                let Some(txn) = txn else {
                    let _ = writeln!(io.err, "undo: HOME is not set, so there is no undo store");
                    crate::signal::ignore_interrupts();
                    return Some(Ok(ExitStatus::FAILURE));
                };
                let session = state
                    .session
                    .as_ref()
                    .map(|s| (s.id, s.shell_pid))
                    .unwrap_or((0, 0));
                let ctx = cli::Context {
                    home_root: txn.home_root.clone(),
                    config: txn.config.clone(),
                    session: session.0,
                    shell_pid: session.1,
                    cwd: cwd.clone(),
                    may_mutate: is_main,
                };
                let mut stop: Option<ShellError> = None;
                let mut run = |program: &[OsString]| {
                    // The program runs like any foreground job; SIGINT goes
                    // to it, not to the checkpointing shell.
                    crate::signal::ignore_interrupts();
                    let bytes: Vec<ShellBytes> = program
                        .iter()
                        .map(|a| ShellBytes::from_os_str(a.as_os_str()))
                        .collect();
                    let result = spawn(&bytes);
                    crate::signal::catch_interrupts(&INTERRUPTED);
                    match result {
                        Ok(status) => RunResult::Exited(status.code()),
                        Err(e @ ShellError::Stopped { .. }) => {
                            stop = Some(e);
                            RunResult::Stopped
                        }
                        Err(_) => RunResult::Exited(126),
                    }
                };
                match cli::main(&ctx, &argv, &mut io, &mut run) {
                    cli::Outcome::Status(code) => Ok(ExitStatus::from(code)),
                    cli::Outcome::Suspended(pending) => {
                        STOPPED_RUN.with(|r| *r.borrow_mut() = Some(pending));
                        Err(stop.unwrap_or(ShellError::Stopped { pid: 0, pgid: 0 }))
                    }
                }
            }
        };
        crate::signal::ignore_interrupts();
        Some(result)
    })
}

/// Provider for epsh's writable redirection opens.
pub fn redirect_provider() -> epsh::eval::RedirectOpenHandler {
    Box::new(|request| {
        let mode = match request.mode {
            epsh::eval::RedirectOpenMode::Truncate => ish_undo::ops::RedirMode::Truncate,
            epsh::eval::RedirectOpenMode::Append => ish_undo::ops::RedirMode::Append,
            epsh::eval::RedirectOpenMode::ReadWrite => ish_undo::ops::RedirMode::ReadWrite,
        };
        STATE.with(|state| {
            let state = state.borrow();
            let txn = state.current.as_ref()?;
            ish_undo::ops::open_redirect(txn, request.path, mode, &INTERRUPTED)
        })
    })
}
