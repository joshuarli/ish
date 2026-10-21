//! Decide how one accepted input line runs.
//!
//! The line is parsed once with epsh. The parsed program is then checked
//! against the syntax ish documents: pipelines, `&&`/`||` lists, `;` lists,
//! redirections, quoting, tilde and parameter expansion, command substitution,
//! and globbing. epsh's AST can express much more (background jobs, compound
//! commands, functions, `source`, `eval`); those are refused here instead of
//! being run. The check is a flat walk over epsh's own AST.
//!
//! The same parse decides whether a shell-owned builtin is the whole line, so
//! `z nothing; echo X` is a refusal rather than `z` receiving `nothing;`.

use crate::builtin::{self, Exec};
use epsh::ast::{Command, HereDocBody, Program, RedirKind, Word, WordPart};
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use std::fmt;

/// How an accepted input line will run.
#[derive(Debug)]
pub enum Plan {
    /// Run the parsed program through epsh.
    Epsh(Program),
    /// A shell-owned builtin that is the entire line. `args` are the
    /// unexpanded shell words after the command name.
    Builtin {
        name: &'static str,
        args: Vec<Word>,
    },
}

/// Why a line was refused before running anything.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejection {
    Syntax(String),
    Background,
    Compound(&'static str),
    FunctionDefinition,
    Source(String),
    ParameterOperator,
    ShellBuiltinOutsideLine(&'static str),
    ExpandedShellBuiltin(&'static str),
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::Syntax(message) => write!(f, "{message}"),
            Rejection::Background => {
                write!(f, "background jobs (`&`) are not supported")
            }
            Rejection::Compound(kind) => {
                write!(f, "{kind} commands are not supported")
            }
            Rejection::FunctionDefinition => {
                write!(f, "function definitions are not supported")
            }
            Rejection::Source(name) => write!(f, "`{name}` is not supported"),
            Rejection::ParameterOperator => write!(
                f,
                "`${{...}}` parameter operators are not supported; use `$NAME`"
            ),
            Rejection::ShellBuiltinOutsideLine(name) => write!(
                f,
                "{name} changes shell state or needs the ish process, so it must be the only command on its line"
            ),
            Rejection::ExpandedShellBuiltin(name) => write!(
                f,
                "{name} changes shell state or needs the ish process, so it requires a statically named whole-line command"
            ),
        }
    }
}

/// Refuse an expanded command name that ish cannot route safely.
///
/// `eval`, `source`, `.`, and InShell builtins are rejected. Other names,
/// including non-UTF-8 names, are allowed.
pub fn check_expanded_command(target: &ShellBytes) -> Result<(), Rejection> {
    let Some(name) = target.as_utf8_str() else {
        return Ok(());
    };
    if matches!(name, "eval" | "source" | ".") {
        return Err(Rejection::Source(name.to_owned()));
    }

    if let Some(spec) = builtin::lookup(name)
        && spec.exec == Exec::InShell
    {
        return Err(Rejection::ExpandedShellBuiltin(spec.name));
    }

    Ok(())
}

/// Parse `line` and decide how it runs, or refuse it.
///
/// `${NAME}` without an operator parses to the same tree as `$NAME`, so it is
/// accepted; operators such as `${NAME:-x}` and `${#NAME}` are refused.
pub fn plan(line: &str) -> Result<Plan, Rejection> {
    let program = Parser::new(line)
        .parse()
        .map_err(|error| Rejection::Syntax(error.to_string()))?;

    if let [
        Command::Simple {
            assigns,
            args,
            redirs,
            ..
        },
    ] = program.commands.as_slice()
        && assigns.is_empty()
        && redirs.is_empty()
        && let Some(first) = args.first()
        && let Some(spec) = static_text(first).as_deref().and_then(builtin::lookup)
        && matches!(spec.exec, Exec::InShell | Exec::Hybrid)
    {
        for word in &args[1..] {
            check_word(word)?;
        }
        return Ok(Plan::Builtin {
            name: spec.name,
            args: args[1..].to_vec(),
        });
    }

    for command in &program.commands {
        check_command(command)?;
    }
    Ok(Plan::Epsh(program))
}

fn check_command(command: &Command) -> Result<(), Rejection> {
    match command {
        Command::Simple {
            assigns,
            args,
            redirs,
            ..
        } => {
            if let Some(name) = args.first().and_then(static_text) {
                if matches!(name.as_str(), "eval" | "source" | ".") {
                    return Err(Rejection::Source(name));
                }
                if let Some(spec) = builtin::lookup(&name)
                    && spec.exec == Exec::InShell
                {
                    return Err(Rejection::ShellBuiltinOutsideLine(spec.name));
                }
            }
            for assign in assigns {
                check_word(&assign.value)?;
            }
            for word in args {
                check_word(word)?;
            }
            for redir in redirs {
                match &redir.kind {
                    RedirKind::Input(word)
                    | RedirKind::Output(word)
                    | RedirKind::Clobber(word)
                    | RedirKind::Append(word)
                    | RedirKind::ReadWrite(word)
                    | RedirKind::DupInput(word)
                    | RedirKind::DupOutput(word) => check_word(word)?,
                    RedirKind::HereDoc(HereDocBody::Parsed(parts))
                    | RedirKind::HereDocStrip(HereDocBody::Parsed(parts)) => {
                        check_parts(parts)?
                    }
                    RedirKind::HereDoc(HereDocBody::Literal(_))
                    | RedirKind::HereDocStrip(HereDocBody::Literal(_)) => {}
                }
            }
            Ok(())
        }
        Command::Pipeline { commands, .. } => commands.iter().try_for_each(check_command),
        Command::And(left, right) | Command::Or(left, right) | Command::Sequence(left, right) => {
            check_command(left)?;
            check_command(right)
        }
        Command::Not(inner) => check_command(inner),
        Command::Background { .. } => Err(Rejection::Background),
        Command::FuncDef { .. } => Err(Rejection::FunctionDefinition),
        Command::Subshell { .. } => Err(Rejection::Compound("subshell")),
        Command::BraceGroup { .. } => Err(Rejection::Compound("brace group")),
        Command::If { .. } => Err(Rejection::Compound("if")),
        Command::While { .. } => Err(Rejection::Compound("while")),
        Command::Until { .. } => Err(Rejection::Compound("until")),
        Command::For { .. } => Err(Rejection::Compound("for")),
        Command::Case { .. } => Err(Rejection::Compound("case")),
    }
}

fn check_word(word: &Word) -> Result<(), Rejection> {
    check_parts(&word.parts)
}

fn check_parts(parts: &[WordPart]) -> Result<(), Rejection> {
    for part in parts {
        match part {
            WordPart::Literal(_) | WordPart::SingleQuoted(_) | WordPart::Tilde(_) => {}
            WordPart::DoubleQuoted(inner) | WordPart::Arith(inner) => check_parts(inner)?,
            // A bad substitution is reported by epsh when it expands.
            WordPart::Param(param) => {
                if !matches!(
                    param.op,
                    epsh::ast::ParamOp::Normal | epsh::ast::ParamOp::BadSubst
                ) {
                    return Err(Rejection::ParameterOperator);
                }
            }
            WordPart::CmdSubst(command) | WordPart::Backtick(command) => check_command(command)?,
        }
    }
    Ok(())
}

/// The text of a word made only of literal and quoted-literal parts, or
/// `None` when any part expands.
fn static_text(word: &Word) -> Option<String> {
    fn push(parts: &[WordPart], out: &mut String) -> Option<()> {
        for part in parts {
            match part {
                WordPart::Literal(bytes) | WordPart::SingleQuoted(bytes) => {
                    out.push_str(&bytes.to_shell_string())
                }
                WordPart::DoubleQuoted(inner) => push(inner, out)?,
                _ => return None,
            }
        }
        Some(())
    }
    let mut text = String::new();
    push(&word.parts, &mut text)?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expanded_command_policy_allows_non_utf8_names() {
        let command = ShellBytes::from_vec(vec![b'c', 0xff]);
        assert_eq!(check_expanded_command(&command), Ok(()));
    }

    fn builtin_name(line: &str) -> Option<&'static str> {
        match plan(line) {
            Ok(Plan::Builtin { name, .. }) => Some(name),
            _ => None,
        }
    }

    fn rejection(line: &str) -> Rejection {
        match plan(line) {
            Err(rejection) => rejection,
            Ok(plan) => panic!("{line:?} should be refused, got {plan:?}"),
        }
    }

    #[test]
    fn shell_builtin_alone_runs_in_the_shell_with_its_words() {
        match plan("z  foo  'a b'").unwrap() {
            Plan::Builtin { name, args } => {
                assert_eq!(name, "z");
                assert_eq!(args.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        for name in ["cd", "history", "fg", "alias gs git status", "exit 3", "?"] {
            assert!(builtin_name(name).is_some(), "{name}");
        }
    }

    #[test]
    fn shell_builtin_inside_a_list_or_pipeline_is_refused() {
        for line in [
            "z nothing; echo X",
            "copy-scrollback && echo X",
            "? && echo X",
            "fg && echo X",
            "alias gg echo && echo X",
            "echo x | w",
            "echo $(z foo)",
            "FOO=1 z foo",
            "z foo > out",
            "echo x\nz y",
        ] {
            assert!(
                matches!(rejection(line), Rejection::ShellBuiltinOutsideLine(_)),
                "{line:?}"
            );
        }
    }

    #[test]
    fn hybrid_and_native_builtins_stay_usable_in_lists() {
        for line in [
            "cd /tmp && ls",
            "history | grep x",
            "l | cat",
        ] {
            assert!(matches!(plan(line), Ok(Plan::Epsh(_))), "{line:?}");
        }
        // Native builtins are not intercepted when alone: they run through
        // the external handler like any command.
        assert!(matches!(plan("l -l"), Ok(Plan::Epsh(_))));
    }

    #[test]
    fn foreground_file_commands_use_epsh_dispatch_in_lists_and_redirections() {
        for name in ["cp", "mv"] {
            assert_eq!(builtin::lookup(name).unwrap().exec, Exec::Foreground);
        }
        for line in [
            "cp source destination && echo done",
            "cp source destination > output.log 2> errors.log",
            "command cp source destination",
            "$copy source destination",
        ] {
            assert!(matches!(plan(line), Ok(Plan::Epsh(_))), "{line:?}");
        }

        let mut aliases = crate::alias::AliasMap::new();
        aliases.set("copy".to_owned(), vec!["cp".to_owned()]);
        let expanded = aliases.expand_line("copy source destination");
        assert!(matches!(plan(&expanded), Ok(Plan::Epsh(_))));
    }

    #[test]
    fn documented_syntax_is_accepted() {
        for line in [
            "echo hi | grep h && echo ok || echo no; echo done",
            "echo 'a b' \"c $HOME\" ~/x $(echo y) `echo z` *.rs > out 2>&1",
            "FOO=1 env",
            "! false",
            "echo ${HOME}",
            "cat < in >> out",
            "echo $((1 + 2))",
        ] {
            assert!(matches!(plan(line), Ok(Plan::Epsh(_))), "{line:?}");
        }
    }

    #[test]
    fn undocumented_syntax_is_refused() {
        assert_eq!(rejection("sleep 3 & echo X"), Rejection::Background);
        assert_eq!(rejection("if true; then echo x; fi"), Rejection::Compound("if"));
        assert_eq!(rejection("for i in a; do echo $i; done"), Rejection::Compound("for"));
        assert_eq!(rejection("while false; do :; done"), Rejection::Compound("while"));
        assert_eq!(rejection("case x in x) :;; esac"), Rejection::Compound("case"));
        assert_eq!(rejection("(echo x)"), Rejection::Compound("subshell"));
        assert_eq!(rejection("{ echo x; }"), Rejection::Compound("brace group"));
        assert_eq!(rejection("f() { echo x; }; f"), Rejection::FunctionDefinition);
        assert_eq!(rejection("source /dev/null"), Rejection::Source("source".into()));
        assert_eq!(rejection("echo a; . ./x"), Rejection::Source(".".into()));
        assert_eq!(rejection("eval echo x"), Rejection::Source("eval".into()));
        assert_eq!(rejection("'eval' x"), Rejection::Source("eval".into()));
        assert_eq!(rejection("echo ${HOME:-x}"), Rejection::ParameterOperator);
        assert_eq!(rejection("echo \"${#HOME}\""), Rejection::ParameterOperator);
    }

    #[test]
    fn refusals_inside_command_substitution_are_found() {
        assert_eq!(rejection("echo $(sleep 1 &)"), Rejection::Background);
        assert_eq!(rejection("echo `eval x`"), Rejection::Source("eval".into()));
    }

    #[test]
    fn syntax_errors_are_reported_not_run() {
        assert!(matches!(rejection("echo 'unterminated"), Rejection::Syntax(_)));
        assert!(matches!(rejection("echo |"), Rejection::Syntax(_)));
    }
}
