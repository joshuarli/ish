use crate::ls;
use crate::path;

/// Where an ish-owned builtin may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exec {
    /// Reads or changes shell state, or performs an ish-owned operation that
    /// must stay in the shell process. It runs only as the whole input line.
    InShell,
    /// As the whole input line it runs in the shell with shell state; inside a
    /// list or pipeline epsh's own builtin or the external-command handler
    /// supplies a stateless form of the same command.
    Hybrid,
    /// Runs through epsh in lists and redirections, then the parent-process
    /// external handler.
    Foreground,
    /// Self-contained: runs through the external-command handler wherever it
    /// appears, including pipeline stages.
    Native,
}

/// A builtin that runs in the shell process as the whole input line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellCommand {
    CopyLastOutput,
    Alias,
    ChangeDirectory,
    CopyScrollback,
    Denv,
    Exit,
    ResumeJob,
    History,
    DumpLayout,
    Trash,
    Locate,
    Jump,
}

/// A builtin the external-command handler runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalCommand {
    Copy,
    Move,
    List,
    ClearScreen,
    History,
}

#[derive(Clone, Copy)]
enum Handler {
    InShell(ShellCommand),
    Hybrid {
        shell: ShellCommand,
        stateless: Option<ExternalCommand>,
    },
    Foreground(ExternalCommand),
    Native(ExternalCommand),
}

/// One ish-owned builtin. This table is the command-name, execution-class, and
/// handler contract used by dispatch, completion, lookup, and the shell and
/// external handlers. Only the constructors build entries, so `exec` always
/// matches the handler.
pub struct Builtin {
    pub name: &'static str,
    pub exec: Exec,
    handler: Handler,
}

impl Builtin {
    const fn in_shell(name: &'static str, command: ShellCommand) -> Self {
        Self { name, exec: Exec::InShell, handler: Handler::InShell(command) }
    }

    const fn hybrid(
        name: &'static str,
        shell: ShellCommand,
        stateless: Option<ExternalCommand>,
    ) -> Self {
        Self { name, exec: Exec::Hybrid, handler: Handler::Hybrid { shell, stateless } }
    }

    const fn foreground(name: &'static str, command: ExternalCommand) -> Self {
        Self { name, exec: Exec::Foreground, handler: Handler::Foreground(command) }
    }

    const fn native(name: &'static str, command: ExternalCommand) -> Self {
        Self { name, exec: Exec::Native, handler: Handler::Native(command) }
    }

    /// The handler for running as the whole input line, for classes that have one.
    pub fn shell_command(&self) -> Option<ShellCommand> {
        match self.handler {
            Handler::InShell(command) | Handler::Hybrid { shell: command, .. } => Some(command),
            Handler::Foreground(_) | Handler::Native(_) => None,
        }
    }

    /// The handler the external-command handler runs, for classes that have one.
    pub fn external_command(&self) -> Option<ExternalCommand> {
        match self.handler {
            Handler::Hybrid { stateless, .. } => stateless,
            Handler::Foreground(command) | Handler::Native(command) => Some(command),
            Handler::InShell(_) => None,
        }
    }
}

const ISH_BUILTINS: &[Builtin] = &[
    Builtin::in_shell("?", ShellCommand::CopyLastOutput),
    Builtin::in_shell("alias", ShellCommand::Alias),
    Builtin::hybrid("cd", ShellCommand::ChangeDirectory, None),
    Builtin::in_shell("copy-scrollback", ShellCommand::CopyScrollback),
    Builtin::foreground("cp", ExternalCommand::Copy),
    Builtin::native("c", ExternalCommand::ClearScreen),
    Builtin::in_shell("denv", ShellCommand::Denv),
    Builtin::in_shell("exit", ShellCommand::Exit),
    Builtin::in_shell("fg", ShellCommand::ResumeJob),
    Builtin::hybrid("history", ShellCommand::History, Some(ExternalCommand::History)),
    Builtin::in_shell("ish-dump", ShellCommand::DumpLayout),
    Builtin::native("l", ExternalCommand::List),
    Builtin::foreground("mv", ExternalCommand::Move),
    Builtin::in_shell("t", ShellCommand::Trash),
    Builtin::in_shell("trash", ShellCommand::Trash),
    Builtin::in_shell("type", ShellCommand::Locate),
    Builtin::in_shell("w", ShellCommand::Locate),
    Builtin::in_shell("which", ShellCommand::Locate),
    Builtin::in_shell("z", ShellCommand::Jump),
];

/// The ish-owned builtin called `name`, if any.
pub fn lookup(name: &str) -> Option<&'static Builtin> {
    ISH_BUILTINS.iter().find(|b| b.name == name)
}

/// Whether running the whole-line builtin `name` replaces the captured output
/// of the previous command. `?` reads it, `fg` continues the suspended job's
/// capture, and `exit` leaves the shell.
pub fn replaces_last_output(name: &str) -> bool {
    !matches!(name, "?" | "fg" | "exit")
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
pub fn list_directory(args: &[impl AsRef<std::path::Path>]) -> i32 {
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
                println!("{}:", arg.as_ref().display());
            }
            let s = ls::list_dir(arg);
            if s != 0 {
                status = s;
            }
        }
        status
    }
}
