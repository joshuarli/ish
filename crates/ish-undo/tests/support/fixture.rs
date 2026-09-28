//! Test fixture for ish-undo integration suites.
//!
//! Runs one operation per process so tests can crash it at a named boundary
//! (`--fault point[:skip]=errno|abort`) and recover in a fresh process, hold
//! descriptors and mappings open across a preservation, or act as an
//! external program that changes files through child processes.
//!
//! Every mode takes explicit paths; nothing reads HOME or the process
//! environment for store locations.

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use ish_undo::cli::{self, RunResult};
use ish_undo::{Config, Io, Session, fault, ops};

fn usage() -> ! {
    eprintln!(
        "usage: ish-undo-fixture [--fault spec]... [--config NAME=VALUE]... MODE ...\n\
         modes:\n  shell <store> <cwd> <session> [--hold] <step>...   (steps: rm:ARGS mv:ARGS write:PATH=DATA append:PATH=DATA)\n  \
         undo <store> <cwd> <session> [args...]\n  hold-open <path>\n  mutate <dir> <script>"
    );
    std::process::exit(2);
}

fn args_of(spec: &str) -> Vec<OsString> {
    spec.split('\u{1f}')
        .filter(|s| !s.is_empty())
        .map(OsString::from)
        .collect()
}

/// Decode `\xNN` escapes so tests can pass raw path bytes.
fn raw(s: &str) -> OsString {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1] == b'x'
            && let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    OsString::from_vec(out)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut vars: Vec<(String, String)> = Vec::new();
    while let Some(first) = args.first().cloned() {
        match first.as_str() {
            "--fault" => {
                fault::inject_spec(&args[1]).unwrap_or_else(|e| panic!("bad fault: {e}"));
                args.drain(..2);
            }
            "--nofile" => {
                // Lower the descriptor limit to prove walks stay bounded.
                let n: u64 = args[1].parse().expect("limit");
                rustix::process::setrlimit(
                    rustix::process::Resource::Nofile,
                    rustix::process::Rlimit {
                        current: Some(n),
                        maximum: rustix::process::getrlimit(rustix::process::Resource::Nofile)
                            .maximum,
                    },
                )
                .expect("setrlimit");
                args.drain(..2);
            }
            "--config" => {
                let (k, v) = args[1].split_once('=').expect("NAME=VALUE");
                vars.push((k.to_owned(), v.to_owned()));
                args.drain(..2);
            }
            _ => break,
        }
    }
    let (config, warnings) =
        Config::from_vars(|name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()));
    assert!(warnings.is_empty(), "{warnings:?}");
    let Some(mode) = args.first().cloned() else {
        usage()
    };
    let rest = &args[1..];
    let code = match mode.as_str() {
        "shell" => shell(rest, config),
        "undo" => undo(rest, config),
        "hold-open" => hold_open(Path::new(&rest[0])),
        "mutate" => mutate(Path::new(&rest[0]), &rest[1..]),
        _ => usage(),
    };
    std::process::exit(code);
}

fn confirm_yes(_: &str) -> Option<bool> {
    Some(true)
}

/// Run steps as one transaction of a fresh shell session. `--hold` keeps the
/// transaction open until stdin closes, printing `ready` once recorded.
fn shell(args: &[String], config: Config) -> i32 {
    let store = PathBuf::from(&args[0]);
    let cwd = PathBuf::from(&args[1]);
    let mut steps = &args[3..];
    let hold = steps.first().is_some_and(|s| s == "--hold");
    if hold {
        steps = &steps[1..];
    }
    let mut session = Session::new();
    session.id = args[2].parse().expect("session id");
    let txn = session.begin(store, config, &cwd, &steps.join("; "));
    let cancel = AtomicBool::new(false);
    let mut status = 0;
    for step in steps {
        let (kind, arg) = step.split_once(':').expect("kind:arg");
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut confirm = confirm_yes;
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            confirm: &mut confirm,
            cancel: &cancel,
        };
        status = match kind {
            "rm" => ops::rm(
                &txn,
                &cwd,
                &args_of(arg)
                    .iter()
                    .map(|a| raw(&a.to_string_lossy()))
                    .collect::<Vec<_>>(),
                &mut io,
            ),
            "mv" => ops::mv(
                &txn,
                &cwd,
                &args_of(arg)
                    .iter()
                    .map(|a| raw(&a.to_string_lossy()))
                    .collect::<Vec<_>>(),
                &mut io,
            ),
            "write" | "append" | "readwrite" => {
                let (path, data) = arg.split_once('=').expect("PATH=DATA");
                let mode = match kind {
                    "write" => ops::RedirMode::Truncate,
                    "append" => ops::RedirMode::Append,
                    _ => ops::RedirMode::ReadWrite,
                };
                let path = ops::resolve(&cwd, &raw(path));
                match ops::open_redirect(&txn, &path, mode, &cancel) {
                    Some(Ok(fd)) => {
                        let mut f = std::fs::File::from(fd);
                        f.write_all(data.as_bytes()).unwrap();
                        0
                    }
                    Some(Err(e)) => {
                        eprintln!("redirect: {}: {e}", path.display());
                        1
                    }
                    None => {
                        let mut f = std::fs::OpenOptions::new()
                            .append(true)
                            .create(true)
                            .open(&path)
                            .unwrap();
                        f.write_all(data.as_bytes()).unwrap();
                        3
                    }
                }
            }
            _ => usage(),
        };
    }
    if hold {
        println!("ready {}", txn.allocated_id().unwrap_or(0));
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }
    let id = session.finish(txn, status);
    println!("txn {}", id.unwrap_or(0));
    status
}

fn undo(args: &[String], config: Config) -> i32 {
    let ctx = cli::Context {
        home_root: PathBuf::from(&args[0]),
        config,
        session: args[2].parse().expect("session id"),
        shell_pid: std::process::id(),
        cwd: PathBuf::from(&args[1]),
        may_mutate: true,
    };
    let argv: Vec<OsString> = args[3..].iter().map(|a| raw(a)).collect();
    let cancel = AtomicBool::new(false);
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut confirm = confirm_yes;
    let mut io = Io {
        out: &mut out,
        err: &mut err,
        confirm: &mut confirm,
        cancel: &cancel,
    };
    let mut run = |program: &[OsString]| {
        let status = std::process::Command::new(&program[0])
            .args(&program[1..])
            .current_dir(&ctx.cwd)
            .status()
            .expect("spawn program");
        RunResult::Exited(status.code().unwrap_or(128))
    };
    match cli::main(&ctx, &argv, &mut io, &mut run) {
        cli::Outcome::Status(code) => code,
        cli::Outcome::Suspended(_) => 148,
    }
}

/// Open `path` for writing, report readiness, then on each stdin command
/// write through the descriptor or a shared writable mapping.
fn hold_open(path: &Path) -> i32 {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let len = file.metadata().unwrap().len() as usize;
    println!("ready");
    std::io::stdout().flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        match line.as_str() {
            "write" => {
                rustix::io::pwrite(&file, b"FD", 0).unwrap();
            }
            "map" => {
                // SAFETY: a shared mapping of an open file we own for the
                // duration of this block.
                unsafe {
                    let ptr = rustix::mm::mmap(
                        std::ptr::null_mut(),
                        len,
                        rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                        rustix::mm::MapFlags::SHARED,
                        &file,
                        0,
                    )
                    .unwrap();
                    *(ptr as *mut u8).add(2) = b'M';
                    rustix::mm::msync(ptr, len, rustix::mm::MsyncFlags::SYNC).unwrap();
                    rustix::mm::munmap(ptr, len).unwrap();
                }
            }
            _ => break,
        }
        println!("done");
        std::io::stdout().flush().unwrap();
    }
    0
}

/// An external program that changes a tree through child processes:
/// `create:NAME=DATA`, `rewrite:NAME=DATA` (same size, same mtime restored),
/// `rename:FROM=TO`, `delete:NAME`, `child:STEP` (run STEP in a child
/// process), `fail` (exit 3).
fn mutate(dir: &Path, steps: &[String]) -> i32 {
    for step in steps {
        let (kind, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
        match kind {
            "create" => {
                let (name, data) = arg.split_once('=').unwrap();
                std::fs::write(dir.join(name), data).unwrap();
            }
            "rewrite" => {
                let (name, data) = arg.split_once('=').unwrap();
                let path = dir.join(name);
                let before = std::fs::metadata(&path).unwrap();
                assert_eq!(before.len() as usize, data.len(), "rewrite keeps the size");
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                rustix::io::pwrite(&file, data.as_bytes(), 0).unwrap();
                // Put the old mtime back: only ctime and content betray it.
                file.set_modified(before.modified().unwrap()).unwrap();
            }
            "rename" => {
                let (from, to) = arg.split_once('=').unwrap();
                std::fs::rename(dir.join(from), dir.join(to)).unwrap();
            }
            "delete" => {
                let path = dir.join(arg);
                if path.is_dir() {
                    std::fs::remove_dir_all(path).unwrap();
                } else {
                    std::fs::remove_file(path).unwrap();
                }
            }
            "mkdir" => std::fs::create_dir(dir.join(arg)).unwrap(),
            "child" => {
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("mutate")
                    .arg(dir)
                    .arg(arg.replacen('/', ":", 1))
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            "fail" => return 3,
            other => panic!("unknown mutate step {other}"),
        }
    }
    0
}
