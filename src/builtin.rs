use crate::ls;
use crate::path;

/// Where an ish-owned builtin may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exec {
    /// Reads or changes shell state (aliases, jobs, history store, directory
    /// stack, environment mirror) that a list or pipeline stage cannot reach.
    /// It runs only when it is the whole input line; anywhere else it is
    /// refused.
    InShell,
    /// As the whole input line it runs in the shell with shell state; inside a
    /// list or pipeline epsh's own builtin or the external-command handler
    /// supplies a stateless form of the same command.
    Hybrid,
    /// Self-contained: runs through the external-command handler wherever it
    /// appears, including pipeline stages.
    Native,
    /// Native, and routed through the undo-recovery dispatcher.
    Undo,
}

/// One ish-owned builtin. The handler for [`Exec::InShell`] and
/// [`Exec::Hybrid`] entries is selected by name in `main.rs`; every other
/// question about builtins (is it one, what completes, how `w` answers, where
/// it may run) is answered from this table.
pub struct Builtin {
    pub name: &'static str,
    pub exec: Exec,
}

const ISH_BUILTINS: &[Builtin] = &[
    Builtin { name: "alias", exec: Exec::InShell },
    Builtin { name: "cd", exec: Exec::Hybrid },
    Builtin { name: "copy-scrollback", exec: Exec::InShell },
    Builtin { name: "c", exec: Exec::Native },
    Builtin { name: "denv", exec: Exec::InShell },
    Builtin { name: "exit", exec: Exec::InShell },
    Builtin { name: "fg", exec: Exec::InShell },
    Builtin { name: "history", exec: Exec::Hybrid },
    Builtin { name: "ish-dump", exec: Exec::InShell },
    Builtin { name: "l", exec: Exec::Native },
    Builtin { name: "mv", exec: Exec::Undo },
    Builtin { name: "rm", exec: Exec::Undo },
    Builtin { name: "type", exec: Exec::InShell },
    Builtin { name: "undo", exec: Exec::Undo },
    Builtin { name: "w", exec: Exec::InShell },
    Builtin { name: "which", exec: Exec::InShell },
    Builtin { name: "z", exec: Exec::InShell },
];

/// The ish-owned builtin called `name`, if any.
pub fn lookup(name: &str) -> Option<&'static Builtin> {
    ISH_BUILTINS.iter().find(|b| b.name == name)
}

/// True for builtins that the undo dispatcher executes (`rm`, `mv`, `undo`).
pub fn is_undo_builtin(name: &str) -> bool {
    lookup(name).is_some_and(|b| b.exec == Exec::Undo)
}

/// Check if a command name is a builtin in either ish or epsh.
pub fn is_builtin(name: &str) -> bool {
    lookup(name).is_some() || epsh::builtins::is_builtin(name)
}

/// All builtin names (ish + epsh), each once, for completion.
pub fn all_builtin_names() -> impl Iterator<Item = &'static str> {
    ISH_BUILTINS.iter().map(|b| b.name).chain(
        epsh::builtins::BUILTIN_NAMES
            .iter()
            .copied()
            .filter(|name| lookup(name).is_none()),
    )
}

/// Locate a command through aliases, builtins, functions, or PATH.
/// `path_env` is `$PATH` from epsh's variable store — the source of truth.
pub fn locate_command(
    args: &[String],
    aliases: &crate::alias::AliasMap,
    function_exists: impl Fn(&str) -> bool,
    path_env: &[u8],
) -> i32 {
    if args.is_empty() {
        eprintln!("ish: w: expected command name");
        return 1;
    }

    let name = &args[0];

    if let Some(exp) = aliases.get(name) {
        println!("alias: {} {}", name, exp.join(" "));
        return 0;
    }

    if is_builtin(name) {
        println!("builtin");
        return 0;
    }

    if function_exists(name) {
        println!("function");
        return 0;
    }

    if let Some(p) = path::scan_path(name, path_env) {
        println!("{}", p.display());
        return 0;
    }

    eprintln!("ish: w: not found: {name}");
    1
}

/// List directories using ish's native directory listing.
pub fn list_directory(args: &[String]) -> i32 {
    if args.is_empty() {
        ls::list_dir(".")
    } else {
        let mut status = 0;
        let label = args.len() > 1;
        for (i, arg) in args.iter().enumerate() {
            if label {
                if i > 0 {
                    println!();
                }
                println!("{arg}:");
            }
            let s = ls::list_dir(arg);
            if s != 0 {
                status = s;
            }
        }
        status
    }
}
