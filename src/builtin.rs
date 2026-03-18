use crate::exec;
use crate::job::Job;
use crate::ls;
use crate::parse::Redirect;

/// Builtins that modify shell state — must run in the main process.
const SPECIAL_BUILTINS: &[&str] = &[
    "cd",
    "exit",
    "fg",
    "set",
    "unset",
    "alias",
    "copy-scrollback",
    "history",
];

/// All builtins.
const ALL_BUILTINS: &[&str] = &[
    "cd",
    "exit",
    "fg",
    "set",
    "unset",
    "alias",
    "l",
    "c",
    "w",
    "which",
    "type",
    "echo",
    "pwd",
    "true",
    "false",
    "copy-scrollback",
    "history",
];

pub fn is_builtin(name: &str) -> bool {
    ALL_BUILTINS.contains(&name)
}

pub fn is_special_builtin(name: &str) -> bool {
    SPECIAL_BUILTINS.contains(&name)
}

/// Run a state-modifying builtin in the main process. Returns exit status.
pub fn run_special(
    name: &str,
    args: &[String],
    _redirects: &[Redirect],
    prev_dir: &mut Option<String>,
    home: &str,
    job: &mut Option<Job>,
    _session_log: &mut String,
) -> i32 {
    match name {
        "cd" => builtin_cd(args, prev_dir, home),
        "exit" => builtin_exit(args, job),
        "fg" => exec::resume_job(job).0, // continuation handled in main.rs
        "set" => builtin_set(args),
        "unset" => builtin_unset(args),
        "alias" => {
            eprintln!("ish: alias: use at the prompt level");
            1
        }
        "copy-scrollback" => {
            // Handled in main.rs where session_log is accessible
            0
        }
        _ => {
            eprintln!("ish: {name}: unknown builtin");
            1
        }
    }
}

/// Run an output-only builtin (can be forked in pipelines). Returns exit status.
pub fn run_output(name: &str, args: &[String], _redirects: &[Redirect]) -> i32 {
    match name {
        "l" => {
            let path = args.first().map(|s| s.as_str()).unwrap_or(".");
            ls::list_dir(path)
        }
        "c" => {
            // Clear screen
            print!("\x1b[H\x1b[2J");
            0
        }
        "w" | "which" | "type" => builtin_w(args),
        "echo" => {
            println!("{}", args.join(" "));
            0
        }
        "pwd" => match std::env::current_dir() {
            Ok(dir) => {
                println!("{}", dir.display());
                0
            }
            Err(e) => {
                eprintln!("ish: pwd: {e}");
                1
            }
        },
        "true" => 0,
        "false" => 1,
        "copy-scrollback" => {
            // OSC 52 clipboard — we'd need the scrollback content.
            // For now, this is a placeholder that copies a message.
            eprintln!("ish: copy-scrollback: not yet implemented");
            1
        }
        // Special builtins in a pipeline context (forked) — these shouldn't modify state
        "cd" | "exit" | "fg" | "set" | "unset" | "alias" => {
            eprintln!("ish: {name}: cannot use in a pipeline");
            1
        }
        _ => {
            eprintln!("ish: {name}: unknown builtin");
            1
        }
    }
}

fn builtin_cd(args: &[String], prev_dir: &mut Option<String>, home: &str) -> i32 {
    let target = if args.is_empty() {
        home.to_string()
    } else if args[0] == "-" {
        match prev_dir {
            Some(d) => {
                let d = d.clone();
                eprintln!("{d}");
                d
            }
            None => {
                eprintln!("ish: cd: no previous directory");
                return 1;
            }
        }
    } else {
        args[0].clone()
    };

    let old_pwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());

    if let Err(e) = std::env::set_current_dir(&target) {
        eprintln!("ish: cd: {target}: {e}");
        return 1;
    }

    // Update PWD
    if let Ok(new_pwd) = std::env::current_dir() {
        let new_pwd_str = new_pwd.to_string_lossy().into_owned();
        // SAFETY: single-threaded shell, no other threads reading env
        unsafe { std::env::set_var("PWD", &new_pwd_str) };
    }

    *prev_dir = old_pwd;
    0
}

fn builtin_exit(_args: &[String], _job: &mut Option<Job>) -> i32 {
    // exit is handled directly in main.rs for exit_warned logic
    // This is only reached if exit somehow gets here in a pipeline
    eprintln!("ish: exit: cannot use in a pipeline");
    1
}

fn builtin_set(args: &[String]) -> i32 {
    if args.is_empty() {
        // Print all env vars
        for (key, val) in std::env::vars() {
            println!("{key}={val}");
        }
        return 0;
    }

    let name = &args[0];
    let value = if args.len() > 1 {
        args[1..].join(" ")
    } else {
        String::new() // set VAR with no value → empty string
    };

    // SAFETY: single-threaded shell
    unsafe { std::env::set_var(name, &value) };
    0
}

fn builtin_unset(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("ish: unset: expected variable name");
        return 1;
    }

    for name in args {
        // SAFETY: single-threaded shell
        unsafe { std::env::remove_var(name) };
    }
    0
}

fn builtin_w(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("ish: w: expected command name");
        return 1;
    }

    let name = &args[0];

    // Note: we can't access the AliasMap from here when forked.
    // For single-command invocation, the caller should check aliases first.
    // When forked in a pipeline, we just check builtins + PATH.

    if is_builtin(name) {
        println!("builtin");
        return 0;
    }

    if let Some(path) = exec::scan_path(name) {
        println!("{}", path.display());
        return 0;
    }

    eprintln!("ish: w: not found: {name}");
    1
}
