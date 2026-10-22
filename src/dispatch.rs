
use crate::builtin::{self, Exec};
use epsh::ast::{Command, HereDocBody, Program, RedirKind, Word, WordPart};
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use std::fmt;

#[derive(Debug)]
pub enum Plan {
    Epsh(Program),
    Builtin {
        name: &'static str,
        args: Vec<Word>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rejection {
    Syntax(String),
    Background,
    Compound(&'static str),
    FunctionDefinition,
    UnsupportedBuiltin(String),
    Arguments(String),
    ParameterOperator,
    ShellBuiltinOutsideLine(&'static str),
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
            Rejection::UnsupportedBuiltin(name) => write!(f, "`{name}` is not supported"),
            Rejection::Arguments(diagnostic) => write!(f, "{diagnostic}"),
            Rejection::ParameterOperator => write!(
                f,
                "`${{...}}` parameter operators are not supported; use `$NAME`"
            ),
            Rejection::ShellBuiltinOutsideLine(name) => write!(
                f,
                "{name} changes shell state or needs the ish process, so it must be the only command on its line"
            ),
        }
    }
}

fn refused_command_name(name: &str) -> Option<Rejection> {
    if let Some(spec) = builtin::lookup(name).filter(|spec| spec.exec == Exec::InShell) {
        return Some(Rejection::ShellBuiltinOutsideLine(spec.name));
    }
    (epsh::builtins::is_builtin(name) && !builtin::epsh_builtin_names().any(|allowed| allowed == name))
        .then(|| Rejection::UnsupportedBuiltin(name.to_owned()))
}

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
            check_static_arguments(args)?;
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

fn check_static_arguments(words: &[Word]) -> Result<(), Rejection> {
    let prefix: Vec<_> = words.iter().map_while(static_text).collect();
    let mut arguments = prefix.as_slice();
    loop {
        let Some(name) = arguments.first() else { return Ok(()); };
        if let Some(rejection) = refused_command_name(name) { return Err(rejection); }
        if let Some(rule) = builtin::epsh_argument_rule(name) {
            let argv: Vec<_> = arguments.iter().map(|argument| ShellBytes::from_str_lossless(argument)).collect();
            rule.validate_arguments(&argv).map_err(Rejection::Arguments)?;
        }
        if name != "command" { return Ok(()); }
        match epsh::builtins::command_action(arguments) {
            epsh::builtins::CommandAction::Execute(nested) => arguments = nested,
            epsh::builtins::CommandAction::Empty | epsh::builtins::CommandAction::Describe { .. } => return Ok(()),
        }
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
    fn finite_policy_refuses_static_targets_and_set_options_before_the_line_runs() {
        for line in [
            "echo earlier; exec echo replaced >target",
            "'trap' ignored TERM >target",
            "echo ok | command -p command -- exec echo replaced >target",
            "echo $(command command trap ignored TERM >target)",
            "set -e >target", "set +e", "set -o pipefail", "set -o noexec",
            "set -", "set --", "set 7 value", "set -e NAME value",
            "command command set '-e' >target",
        ] {
            assert!(plan(line).is_err(), "unsupported command was planned: {line}");
        }
        for line in ["command -v exit", "command -v exec", "command -V trap", "set NAME -e +e", "set NAME", "set"] {
            assert!(plan(line).is_ok(), "supported command was refused: {line}");
        }
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
    fn every_epsh_builtin_is_allowed_or_refused() {
        const ALLOWED: &[&str] = &[":", "true", "false", "echo", "printf", "cd", "pwd", "export", "unset", "set", "test", "[", "command", "kill"];
        for name in epsh::builtins::BUILTIN_NAMES {
            let refused = refused_command_name(name);
            assert_eq!(
                refused.is_none(),
                ALLOWED.contains(name),
                "classify epsh builtin {name:?} as allowed or refused"
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
        assert_eq!(rejection("source /dev/null"), Rejection::UnsupportedBuiltin("source".into()));
        assert_eq!(rejection("echo a; . ./x"), Rejection::UnsupportedBuiltin(".".into()));
        assert_eq!(rejection("eval echo x"), Rejection::UnsupportedBuiltin("eval".into()));
        assert_eq!(rejection("'eval' x"), Rejection::UnsupportedBuiltin("eval".into()));
        assert_eq!(rejection("echo ${HOME:-x}"), Rejection::ParameterOperator);
        assert_eq!(rejection("echo \"${#HOME}\""), Rejection::ParameterOperator);
    }

    #[test]
    fn refusals_inside_command_substitution_are_found() {
        assert_eq!(rejection("echo $(sleep 1 &)"), Rejection::Background);
        assert_eq!(rejection("echo `eval x`"), Rejection::UnsupportedBuiltin("eval".into()));
    }

    #[test]
    fn syntax_errors_are_reported_not_run() {
        assert!(matches!(rejection("echo 'unterminated"), Rejection::Syntax(_)));
        assert!(matches!(rejection("echo |"), Rejection::Syntax(_)));
    }

    #[test]
    fn deeply_nested_substitutions_are_refused_not_crashed() {
        let command_substitutions = format!("echo {}x{}", "$(echo ".repeat(1000), ")".repeat(1000));
        let arithmetic = format!("echo {}1{}", "$(( ".repeat(1000), " ))".repeat(1000));
        assert!(matches!(rejection(&command_substitutions), Rejection::Syntax(_)));
        assert!(matches!(rejection(&arithmetic), Rejection::Syntax(_)));
        assert!(matches!(rejection("echo $(( ) )"), Rejection::Syntax(_)));
    }
}
