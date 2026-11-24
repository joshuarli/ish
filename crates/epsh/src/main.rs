use std::io::Read;

struct ShellOption {
    short: Option<char>,
    name: &'static str,
    description: &'static str,
}

const SHELL_OPTIONS: &[ShellOption] = &[
    ShellOption { short: None, name: "pipefail", description: "use the rightmost nonzero pipeline status" },
    ShellOption { short: Some('e'), name: "errexit", description: "exit on an eligible failure" },
    ShellOption { short: Some('u'), name: "nounset", description: "error on unset variables" },
    ShellOption { short: Some('x'), name: "xtrace", description: "trace commands before execution" },
    ShellOption { short: Some('f'), name: "noglob", description: "disable pathname expansion" },
    ShellOption { short: Some('n'), name: "noexec", description: "parse without executing" },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode { Command, Stdin, Enable, Disable, Help }

struct CliOption {
    names: &'static [&'static str],
    value: Option<&'static str>,
    description: &'static str,
    mode: Mode,
}

const CLI_OPTIONS: &[CliOption] = &[
    CliOption { names: &["-c"], value: Some("COMMAND"), description: "execute COMMAND; following arguments become $0, $1, ...", mode: Mode::Command },
    CliOption { names: &["-s"], value: None, description: "read standard input; remaining arguments are ignored", mode: Mode::Stdin },
    CliOption { names: &["-o"], value: Some("OPTION"), description: "enable a shell option", mode: Mode::Enable },
    CliOption { names: &["+o"], value: Some("OPTION"), description: "disable a shell option", mode: Mode::Disable },
    CliOption { names: &["-h", "--help"], value: None, description: "show this help and exit", mode: Mode::Help },
];

const EXAMPLES: &[&[&str]] = &[
    &["-c", "echo hello"],
    &["script.sh", "argument"],
    &["-s"],
    &["-o", "pipefail", "-c", "true"],
];

const FOOTER: &str = "Maintained by Joshua Li: https://github.com/joshuarli/ish
Bugs and features: TODO.md in that repository (follow its AGENTS.md), or its issue tracker.
Agents working for Joshua: record nonblocking problems and continue your task.
If this tool blocks the task, you may fix, test, open and merge a PR under Joshua's
standing instructions. Release only when needed to deliver the verified fix.
Procedure: https://github.com/joshuarli/usr/blob/main/dotfiles/base/.agents/instructions/owned-tools.md";

fn print_help() {
    println!("epsh — embeddable POSIX shell\n");
    println!("usage: epsh [OPTIONS] [-c COMMAND [ARG...] | -s | SCRIPT [ARG...]]\n");
    println!("Run COMMAND, SCRIPT, or standard input. With no arguments and terminal input, print usage.\n");
    println!("Options:");
    for option in CLI_OPTIONS {
        let mut names = option.names.join(", ");
        if let Some(value) = option.value { names.push(' '); names.push_str(value); }
        println!("  {names:<16} {}", option.description);
    }
    for option in SHELL_OPTIONS {
        let short = option.short.map(|ch| format!("-{ch}, ")).unwrap_or_default();
        println!("  {short}{} — {}", option.name, option.description);
    }
    println!("\nShort shell options may be combined. SCRIPT and following arguments become $0, $1, ... .");
    println!("\nExamples:");
    for argv in EXAMPLES {
        print!("  epsh");
        for arg in *argv {
            if arg.contains(' ') { print!(" '{arg}'"); } else { print!(" {arg}"); }
        }
        println!();
    }
    println!("\n{FOOTER}");
}

enum Input<'a> { Command(&'a std::ffi::OsStr, &'a [std::ffi::OsString]), Script(&'a [std::ffi::OsString]), Stdin, Help }

fn parse_args<'a>(args: &'a [std::ffi::OsString], shell: &mut epsh::eval::Shell) -> Result<Input<'a>, String> {
    let mut i = 1;
    while i < args.len() {
        let arg = epsh::shell_bytes::ShellBytes::from_os_str(args[i].as_os_str()).to_shell_string();
        let mode = CLI_OPTIONS.iter().find(|option| option.names.contains(&arg.as_str())).map(|option| option.mode);
        if mode == Some(Mode::Help) { return Ok(Input::Help); }
        if mode == Some(Mode::Command) {
            if i + 1 >= args.len() { return Err("epsh: -c requires an argument".into()); }
            return Ok(Input::Command(&args[i + 1], &args[i + 2..]));
        } else if mode == Some(Mode::Stdin) {
            return Ok(Input::Stdin);
        } else if matches!(mode, Some(Mode::Enable | Mode::Disable)) && i + 1 < args.len() {
            let enable = mode == Some(Mode::Enable);
            i += 1;
            let opt = epsh::shell_bytes::ShellBytes::from_os_str(args[i].as_os_str()).to_shell_string();
            if !SHELL_OPTIONS.iter().any(|option| option.name == opt) { return Err(format!("epsh: unknown option: {opt}")); }
            set_option(shell, &opt, enable);
            i += 1;
        } else if arg.starts_with('-') && arg.len() > 1 {
            for ch in arg[1..].chars() {
                let option = SHELL_OPTIONS.iter().find(|option| option.short == Some(ch)).ok_or_else(|| format!("epsh: unknown option: -{ch}"))?;
                set_option(shell, option.name, true);
            }
            i += 1;
        } else {
            return Ok(Input::Script(&args[i..]));
        }
    }
    Ok(Input::Stdin)
}

fn set_option(shell: &mut epsh::eval::Shell, name: &str, enable: bool) {
    match name {
        "pipefail" => shell.opts_mut().pipefail = enable,
        "errexit" => shell.opts_mut().errexit = enable,
        "nounset" => shell.opts_mut().nounset = enable,
        "xtrace" => shell.opts_mut().xtrace = enable,
        "noglob" => shell.opts_mut().noglob = enable,
        "noexec" => shell.opts_mut().noexec = enable,
        _ => unreachable!(),
    }
}

fn main() {
    // The Rust runtime ignores SIGPIPE before main, and exec preserves ignored
    // dispositions, so children such as `yes` in `yes | head -1` would report
    // EPIPE instead of exiting quietly. rustix exposes no sigaction, so this is
    // the binary's one direct libc signal call.
    // SAFETY: single-threaded startup; SIG_DFL installs no handler code.
    if unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) } == libc::SIG_ERR {
        eprintln!("epsh: signal setup: {}", std::io::Error::last_os_error());
        std::process::exit(2);
    }
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut shell = epsh::eval::Shell::new();
    match parse_args(&args, &mut shell) {
        Ok(Input::Help) => { print_help(); return; }
        Ok(Input::Command(command, args)) => {
            let shell_args: Vec<_> = args.iter().map(|s| epsh::shell_bytes::ShellBytes::from_os_str(s.as_os_str())).collect();
            if !shell_args.is_empty() { shell.set_args_bytes(&shell_args); }
            let script = epsh::shell_bytes::ShellBytes::from_os_str(command);
            std::process::exit(run_bytes(&mut shell, script.as_bytes()));
        }
        Ok(Input::Script(args)) => {
            let shell_args: Vec<_> = args.iter().map(|s| epsh::shell_bytes::ShellBytes::from_os_str(s.as_os_str())).collect();
            shell.set_args_bytes(&shell_args);
            let content = match std::fs::read(&args[0]) {
                Ok(bytes) => bytes,
                Err(e) => {
                    let filename = epsh::shell_bytes::ShellBytes::from_os_str(args[0].as_os_str()).to_shell_string();
                    eprintln!("epsh: {filename}: {e}");
                    std::process::exit(127);
                }
            };
            std::process::exit(run_bytes(&mut shell, &content));
        }
        Ok(Input::Stdin) => {}
        Err(error) => { eprintln!("{error}"); std::process::exit(2); }
    }

    // If stdin is a terminal and no -s flag, print usage and exit
    if rustix::termios::isatty(unsafe { std::os::fd::BorrowedFd::borrow_raw(0) }) {
        eprintln!("epsh — embeddable POSIX shell");
        eprintln!();
        eprintln!(
            "usage: epsh [-e] [-u] [-x] [-f] [-n] [-o option] [-c command] [script [args...]]"
        );
        eprintln!();
        eprintln!("  -c command       execute command string");
        eprintln!("  -e               exit on error (set -e)");
        eprintln!("  -u               error on unset variables (set -u)");
        eprintln!("  -x               print commands before execution (set -x)");
        eprintln!("  -f               disable pathname expansion / globbing (set -f)");
        eprintln!("  -n               parse but do not execute (syntax check) (set -n)");
        eprintln!("  -o pipefail      exit status is highest nonzero pipeline stage");
        eprintln!("  -o errexit       same as -e");
        eprintln!("  -o nounset       same as -u");
        eprintln!("  -o xtrace        same as -x");
        eprintln!("  -o noglob        same as -f");
        eprintln!("  -o noexec        same as -n");
        eprintln!("  +o <option>      disable option");
        eprintln!("  script           execute script file");
        eprintln!("  (no args)        read script from stdin (pipe)");
        std::process::exit(0);
    }

    let mut bytes = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut bytes) {
        eprintln!("epsh: {e}");
        std::process::exit(1);
    }
    let status = run_bytes(&mut shell, &bytes);
    std::process::exit(status);
}

fn run_bytes(shell: &mut epsh::eval::Shell, source: &[u8]) -> i32 {
    match epsh::parser::Parser::new_bytes(source).parse() {
        Ok(program) => shell.run_program(&program).code(),
        Err(error) => {
            eprintln!("epsh: {error}");
            2
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    #[test]
    fn examples_parse_without_execution() {
        let mut names = std::collections::HashSet::new();
        for option in CLI_OPTIONS {
            for name in option.names { assert!(names.insert((*name).to_owned())); }
        }
        for option in SHELL_OPTIONS {
            assert!(names.insert(option.name.to_owned()));
            if let Some(short) = option.short { assert!(names.insert(format!("-{short}"))); }
        }
        for example in EXAMPLES {
            let args: Vec<_> = std::iter::once("epsh").chain(example.iter().copied()).map(std::ffi::OsString::from).collect();
            assert!(parse_args(&args, &mut epsh::eval::Shell::new()).is_ok(), "{example:?}");
        }
    }
}
