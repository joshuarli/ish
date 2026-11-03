use crate::ls;
use crate::path;
use crate::report;

/// Where an ish-owned builtin may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exec {
    /// Reads or changes shell state, or performs an ish-owned operation that
    /// must stay in the shell process. It runs only as the whole input line.
    InShell,
    /// As the whole input line it runs in the shell with shell state; inside a
    /// list epsh's own builtin or the parent-process external-command handler
    /// supplies a stateless form of the same command.
    Hybrid,
    /// Runs through epsh in lists and redirections, then the parent-process
    /// external handler.
    Foreground,
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
    ListHidden,
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
}

#[derive(Clone, Copy)]
enum Help {
    Text(&'static str),
    Copy,
    Move,
}

impl Help {
    fn render(self, name: &str) -> String {
        match self {
            Help::Text(text) => text.replace("{name}", name),
            Help::Copy => crate::fs_command::help(ish_fs::Operation::Copy),
            Help::Move => crate::fs_command::help(ish_fs::Operation::Move),
        }
    }
}

/// One ish-owned builtin. This table is the command-name, execution-class,
/// handler, and help contract used by dispatch, completion, lookup, and the
/// shell and external handlers. Only the constructors build entries, so `exec`
/// always matches the handler.
pub struct Builtin {
    pub name: &'static str,
    pub exec: Exec,
    handler: Handler,
    help: Help,
}

impl Builtin {
    const fn in_shell(
        name: &'static str,
        command: ShellCommand,
        help: Help,
    ) -> Self {
        Self { name, exec: Exec::InShell, handler: Handler::InShell(command), help }
    }

    const fn hybrid(
        name: &'static str,
        shell: ShellCommand,
        stateless: Option<ExternalCommand>,
        help: Help,
    ) -> Self {
        Self { name, exec: Exec::Hybrid, handler: Handler::Hybrid { shell, stateless }, help }
    }

    const fn foreground(
        name: &'static str,
        command: ExternalCommand,
        help: Help,
    ) -> Self {
        Self { name, exec: Exec::Foreground, handler: Handler::Foreground(command), help }
    }

    /// The handler for running as the whole input line, for classes that have one.
    pub fn shell_command(&self) -> Option<ShellCommand> {
        match self.handler {
            Handler::InShell(command) | Handler::Hybrid { shell: command, .. } => Some(command),
            Handler::Foreground(_) => None,
        }
    }

    /// The handler the external-command handler runs, for classes that have one.
    pub fn external_command(&self) -> Option<ExternalCommand> {
        match self.handler {
            Handler::Hybrid { stateless, .. } => stateless,
            Handler::Foreground(command) => Some(command),
            Handler::InShell(_) => None,
        }
    }
}

const COPY_LAST_OUTPUT_HELP: &str =
    "Usage: {name}\nCopy the previous command's output through OSC 52.";
const ALIAS_HELP: &str =
    "Usage: {name} [NAME [COMMAND...]]\nDefine an alias, or list aliases when NAME is omitted.";
const CHANGE_DIRECTORY_HELP: &str =
    "Usage: {name} [DIRECTORY]\nChange the current directory. No argument goes to HOME; `-` returns to the previous directory.";
const COPY_SCROLLBACK_HELP: &str =
    "Usage: {name}\nCopy the terminal scrollback through OSC 52.";
const DENV_HELP: &str =
    "Usage: {name} <allow|deny|reload>\nTrust, revoke, or reload environment files for this directory.";
const EXIT_HELP: &str =
    "Usage: {name} [STATUS]\nExit the shell. Leaving a suspended foreground job produces a warning.";
const RESUME_JOB_HELP: &str =
    "Usage: {name}\nResume the suspended foreground job.";
const HISTORY_HELP: &str =
    "Usage: {name} [reset]\nShow saved history, or clear it with `reset`.";
const DUMP_LAYOUT_HELP: &str =
    "Usage: {name}\nWrite the current layout dump to the ish data directory.";
const LIST_HELP: &str =
    "Usage: {name} [PATH...]\nList visible files with sizes and relative ages.";
const LIST_HIDDEN_HELP: &str =
    "Usage: {name} [PATH...]\nList all files, including hidden ones, with sizes and relative ages.";
const CLEAR_SCREEN_HELP: &str =
    "Usage: {name}\nClear the screen.";
const TRASH_HELP: &str =
    "Usage: {name} [--yes] [--] PATH\nScan PATH before permanently deleting its entries. PATH stays in place.\nType 'yes' to confirm; --yes skips confirmation.";
const LOCATE_HELP: &str =
    "Usage: {name} NAME\nLocate a command through aliases, builtins, functions, or PATH.";
const JUMP_HELP: &str =
    "Usage: {name} [QUERY]\nJump to the most frequently used directory matching QUERY. No query goes home.";

const ISH_BUILTINS: &[Builtin] = &[
    Builtin::in_shell("?", ShellCommand::CopyLastOutput, Help::Text(COPY_LAST_OUTPUT_HELP)),
    Builtin::in_shell("alias", ShellCommand::Alias, Help::Text(ALIAS_HELP)),
    Builtin::hybrid("cd", ShellCommand::ChangeDirectory, None, Help::Text(CHANGE_DIRECTORY_HELP)),
    Builtin::in_shell("copy-scrollback", ShellCommand::CopyScrollback, Help::Text(COPY_SCROLLBACK_HELP)),
    Builtin::foreground("cp", ExternalCommand::Copy, Help::Copy),
    Builtin::foreground("c", ExternalCommand::ClearScreen, Help::Text(CLEAR_SCREEN_HELP)),
    Builtin::in_shell("denv", ShellCommand::Denv, Help::Text(DENV_HELP)),
    Builtin::in_shell("exit", ShellCommand::Exit, Help::Text(EXIT_HELP)),
    Builtin::in_shell("fg", ShellCommand::ResumeJob, Help::Text(RESUME_JOB_HELP)),
    Builtin::hybrid("history", ShellCommand::History, Some(ExternalCommand::History), Help::Text(HISTORY_HELP)),
    Builtin::in_shell("ish-dump", ShellCommand::DumpLayout, Help::Text(DUMP_LAYOUT_HELP)),
    Builtin::foreground("l", ExternalCommand::List, Help::Text(LIST_HELP)),
    Builtin::foreground("ll", ExternalCommand::ListHidden, Help::Text(LIST_HIDDEN_HELP)),
    Builtin::foreground("mv", ExternalCommand::Move, Help::Move),
    Builtin::in_shell("t", ShellCommand::Trash, Help::Text(TRASH_HELP)),
    Builtin::in_shell("trash", ShellCommand::Trash, Help::Text(TRASH_HELP)),
    Builtin::in_shell("type", ShellCommand::Locate, Help::Text(LOCATE_HELP)),
    Builtin::in_shell("w", ShellCommand::Locate, Help::Text(LOCATE_HELP)),
    Builtin::in_shell("which", ShellCommand::Locate, Help::Text(LOCATE_HELP)),
    Builtin::in_shell("z", ShellCommand::Jump, Help::Text(JUMP_HELP)),
];

struct EpshBuiltin {
    name: &'static str,
    arguments: Option<epsh::eval::CommandRule>,
    help: &'static str,
}

impl EpshBuiltin {
    const fn any_arguments(name: &'static str, help: &'static str) -> Self {
        Self { name, arguments: None, help }
    }
}

const ECHO_HELP: &str =
    "Usage: {name} [-n] [-e|-E] [ARG]...\nWrite arguments separated by spaces. `-n` omits the trailing newline; `-e` interprets escapes.";
const PRINTF_HELP: &str =
    "Usage: {name} FORMAT [ARG]...\nWrite formatted output from a C-style format string.";
const PWD_HELP: &str =
    "Usage: {name}\nPrint the current directory.";
const TRUE_HELP: &str =
    "Usage: {name}\nReturn a successful status.";
const FALSE_HELP: &str =
    "Usage: {name}\nReturn a failure status.";
const COLON_HELP: &str =
    "Usage: {name}\nReturn a successful status.";
const TEST_HELP: &str =
    "Usage: {name} EXPRESSION\nEvaluate a conditional expression.";
const BRACKET_HELP: &str =
    "Usage: {name} EXPRESSION ]\nEvaluate a conditional expression; the closing `]` is required.";
const KILL_HELP: &str =
    "Usage: {name} [-l] [-s SIGNAL | -SIGNAL] PID...\nSend a signal, or list signals with `-l`.";
const EXPORT_HELP: &str =
    "Usage: {name} [NAME[=VALUE]]...\nMark variables for the environment, or list exported variables with no arguments.";
const UNSET_HELP: &str =
    "Usage: {name} [-v|-f] NAME...\nRemove variables, or functions with `-f`.";
const SET_HELP: &str =
    "Usage: {name} [NAME [VALUE...]]\nSet and export a shell variable, or list the environment with no arguments.";
const COMMAND_HELP: &str =
    "Usage: {name} [-v|-V] NAME...\n       {name} [-p] COMMAND [ARG...]\nRun a command without function lookup, or describe how NAME resolves.";

const EPSH_BUILTINS: &[EpshBuiltin] = &[
    EpshBuiltin::any_arguments("echo", ECHO_HELP),
    EpshBuiltin::any_arguments("printf", PRINTF_HELP),
    EpshBuiltin::any_arguments("pwd", PWD_HELP),
    EpshBuiltin::any_arguments("true", TRUE_HELP),
    EpshBuiltin::any_arguments("false", FALSE_HELP),
    EpshBuiltin::any_arguments(":", COLON_HELP),
    EpshBuiltin::any_arguments("test", TEST_HELP),
    EpshBuiltin::any_arguments("[", BRACKET_HELP),
    EpshBuiltin::any_arguments("kill", KILL_HELP),
    EpshBuiltin::any_arguments("export", EXPORT_HELP),
    EpshBuiltin::any_arguments("unset", UNSET_HELP),
    EpshBuiltin { name: "set", arguments: Some(epsh::eval::CommandRule::VariableAssignment), help: SET_HELP },
    EpshBuiltin::any_arguments("command", COMMAND_HELP),
];

/// Epsh implementation names exposed by the interactive policy, including hybrid fallbacks.
pub fn epsh_builtin_names() -> impl Iterator<Item = &'static str> {
    EPSH_BUILTINS.iter().map(|builtin| builtin.name).chain(
        ISH_BUILTINS.iter().filter(|builtin| builtin.exec == Exec::Hybrid && builtin.external_command().is_none()).map(|builtin| builtin.name)
    )
}

/// The owned argument restriction for an exposed epsh command, if any.
pub fn epsh_argument_rule(name: &str) -> Option<&'static epsh::eval::CommandRule> {
    EPSH_BUILTINS.iter().find(|builtin| builtin.name == name).and_then(|builtin| builtin.arguments.as_ref())
}

/// Whether a reserved epsh builtin name is unavailable in ish, including PATH lookup.
pub fn is_refused_epsh_builtin(name: &str) -> bool {
    epsh::builtins::is_builtin(name) && !is_builtin(name)
}

/// Install the interactive execution policy without changing standalone epsh defaults.
pub fn install_epsh_policy(shell: &mut epsh::eval::Shell) {
    shell.set_builtin_allowlist(epsh_builtin_names().map(epsh::shell_bytes::ShellBytes::from).collect());
    for builtin in ISH_BUILTINS.iter().filter(|builtin| builtin.exec == Exec::InShell) {
        shell.set_command_rule(epsh::shell_bytes::ShellBytes::from(builtin.name), epsh::eval::CommandRule::Deny(format!("{} changes shell state or needs the ish process, so it requires a statically named whole-line command", builtin.name)));
    }
    for builtin in EPSH_BUILTINS {
        if let Some(rule) = &builtin.arguments {
            shell.set_command_rule(epsh::shell_bytes::ShellBytes::from(builtin.name), rule.clone());
        }
    }
}

/// The ish-owned builtin called `name`, if any.
pub fn lookup(name: &str) -> Option<&'static Builtin> {
    ISH_BUILTINS.iter().find(|b| b.name == name)
}

/// Help text for a builtin exposed by ish, including epsh builtins.
pub fn help(name: &str) -> Option<String> {
    if let Some(builtin) = lookup(name) {
        return Some(builtin.help.render(name));
    }
    EPSH_BUILTINS
        .iter()
        .find(|builtin| builtin.name == name)
        .map(|builtin| builtin.help.replace("{name}", name))
}

/// Whether running the whole-line builtin `name` replaces the captured output
/// of the previous command. `?` reads it, `fg` continues the suspended job's
/// capture, and `exit` leaves the shell.
pub fn replaces_last_output(name: &str) -> bool {
    !matches!(name, "?" | "fg" | "exit")
}

/// Whether ish exposes this command as a builtin.
pub fn is_builtin(name: &str) -> bool {
    lookup(name).is_some() || EPSH_BUILTINS.iter().any(|builtin| builtin.name == name)
}

/// Every exposed builtin name once, for lookup and completion.
pub fn all_builtin_names() -> impl Iterator<Item = &'static str> {
    ISH_BUILTINS.iter().map(|builtin| builtin.name).chain(
        EPSH_BUILTINS.iter().map(|builtin| builtin.name).filter(|name| lookup(name).is_none())
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
        report!("ish: w: expected command name");
        return 1;
    }

    let name = &args[0];

    let mut has_same_name_alias = false;
    if let Some(exp) = aliases.get(name) {
        println!("alias: {} {}", name, exp.join(" "));
        let same_name = exp.first().is_some_and(|first| strip_quotes(first) == name);
        if !same_name {
            return 0;
        }
        has_same_name_alias = true;
    }

    if is_builtin(name) {
        println!("{name}: builtin");
        if let Some(help) = help(name) {
            print!("{help}");
            if !help.ends_with('\n') {
                println!();
            }
        }
        return 0;
    }

    if is_refused_epsh_builtin(name) {
        if has_same_name_alias {
            return 0;
        }
        report!("ish: w: not found: {name}");
        return 1;
    }

    if function_exists(name) {
        println!("function");
        return 0;
    }

    if let Some(p) = path::scan_path(name, path_env) {
        println!("{}", p.display());
        return 0;
    }

    if has_same_name_alias {
        return 0;
    }
    report!("ish: w: not found: {name}");
    1
}

fn strip_quotes(word: &str) -> &str {
    if word.len() >= 2 {
        let bytes = word.as_bytes();
        if (bytes[0] == b'\'' && bytes[word.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[word.len() - 1] == b'"')
        {
            return &word[1..word.len() - 1];
        }
    }
    word
}

/// List directories using ish's native directory listing.
/// `show_hidden` selects `ll` (hidden files) over `l` (visible only).
pub fn list_directory(args: &[impl AsRef<std::path::Path>], show_hidden: bool) -> i32 {
    if args.is_empty() {
        ls::list_dir(".", show_hidden)
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
            let s = ls::list_dir(arg, show_hidden);
            if s != 0 {
                status = s;
            }
        }
        status
    }
}
