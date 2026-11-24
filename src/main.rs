use ish::report;
use epsh::shell_bytes::ShellBytes;
use std::ffi::{OsStr, OsString};
mod shell;
mod rmrf;

#[derive(Debug, Default)]
struct Args {
    config: Option<OsString>,
    no_config: bool,
    command: Option<OsString>,
}

#[derive(Clone, Copy)]
enum OptionKind { Command, Config, NoConfig, Version, Help }

struct CliOption {
    names: &'static [&'static str],
    value: Option<&'static str>,
    description: &'static str,
    kind: OptionKind,
}

const OPTIONS: &[CliOption] = &[
    CliOption { names: &["-c"], value: Some("COMMAND"), description: "Run COMMAND with the built-in epsh engine and exit (login-shell contract)", kind: OptionKind::Command },
    CliOption { names: &["--config"], value: Some("PATH"), description: "Use PATH instead of the default config file", kind: OptionKind::Config },
    CliOption { names: &["--no-config"], value: None, description: "Skip loading the config file", kind: OptionKind::NoConfig },
    CliOption { names: &["-V", "--version"], value: None, description: "Show version and exit", kind: OptionKind::Version },
    CliOption { names: &["-h", "--help"], value: None, description: "Show this help and exit", kind: OptionKind::Help },
];

const EXAMPLES: &[&[&str]] = &[
    &[],
    &["-c", "true"],
    &["--config", "config.ish"],
    &["--no-config"],
];

const FOOTER: &str = "Maintained by Joshua Li: https://github.com/joshuarli/ish
Bugs and features: TODO.md in that repository (follow its AGENTS.md), or its issue tracker.
Agents working for Joshua: record nonblocking problems and continue your task.
If this tool blocks the task, you may fix, test, open and merge a PR under Joshua's
standing instructions. Release only when needed to deliver the verified fix.
Procedure: https://github.com/joshuarli/usr/blob/main/dotfiles/base/.agents/instructions/owned-tools.md";

enum Parsed { Run(Args), Help, Version }

fn parse<I: Iterator<Item = OsString>>(mut argv: I) -> Result<Parsed, &'static str> {
    let mut args = Args::default();
    while let Some(arg) = argv.next() {
        let option = OPTIONS.iter().find(|option| option.names.iter().any(|name| arg == OsStr::new(name)));
        match option.map(|option| option.kind) {
            Some(OptionKind::Version) => return Ok(Parsed::Version),
            Some(OptionKind::Help) => return Ok(Parsed::Help),
            Some(OptionKind::Command) => args.command = Some(argv.next().ok_or("ish: -c requires a command")?),
            Some(OptionKind::Config) => args.config = Some(argv.next().ok_or("ish: --config requires a config file path")?),
            Some(OptionKind::NoConfig) => args.no_config = true,
            None => return Err("ish: this shell is interactive-only and does not run scripts"),
        }
    }
    Ok(Parsed::Run(args))
}

fn parse_args() -> Args {
    match parse(std::env::args_os().skip(1)) {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help) => { print_help(); std::process::exit(0); }
        Ok(Parsed::Version) => { println!("ish {}", env!("CARGO_PKG_VERSION")); std::process::exit(0); }
        Err(error) => {
            report!("{error}");
            if error == "ish: this shell is interactive-only and does not run scripts" {
                report!("usage: ish [-c command] [--config config] [--no-config] [-h|--help]");
            }
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!("ish — minimal interactive shell\n");
    print!("usage: ish");
    for option in OPTIONS {
        print!(" [{}", option.names.join("|"));
        if let Some(value) = option.value { print!(" {value}"); }
        print!("]");
    }
    println!("\n");
    println!("Without options, start an interactive shell. Scripts are not accepted.\n");
    println!("Options:");
    for option in OPTIONS {
        let mut names = option.names.join(", ");
        if let Some(value) = option.value { names.push(' '); names.push_str(value); }
        println!("  {names:<20} {}", option.description);
    }
    println!("\nConfig defaults to ~/.config/ish/config.ish, or /etc/ish/config.ish if HOME is unset.");
    println!("\nExamples:");
    for argv in EXAMPLES {
        println!("  ish{}", if argv.is_empty() { String::new() } else { format!(" {}", argv.join(" ")) });
    }
    println!("\n{FOOTER}");
}


fn run_command(command: &OsStr) -> ! {
    if let Err(error) = ish::signal::restore_pipe_default() {
        report!("ish: command signal setup: {error}");
        std::process::exit(2);
    }
    let program = match epsh::parser::Parser::new_bytes(ShellBytes::from_os_str(command).as_bytes()).parse() {
        Ok(program) => program,
        Err(error) => {
            report!("ish: {error}");
            std::process::exit(2);
        }
    };
    let mut shell = epsh::eval::Shell::new();
    shell.set_args_bytes(&[ShellBytes::from("ish")]);
    std::process::exit(shell.run_program(&program).code());
}

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new("--ish-fz-worker")) {
        use std::os::unix::ffi::OsStringExt;
        if let Err(error) = ish::signal::restore_defaults() { report!("ish: worker signal setup: {error}"); std::process::exit(2); }
        use std::os::fd::FromRawFd;
        let descriptor = std::env::args_os().nth(2).and_then(|arg| arg.to_str().and_then(|arg| arg.parse::<i32>().ok())).unwrap_or(-1);
        let input = if descriptor >= 3 {
            // SAFETY: the private worker entry receives this owned descriptor
            // from epsh's explicit close-on-exec exception for fz only.
            Some(unsafe { std::fs::File::from_raw_fd(descriptor) })
        } else { None };
        let status = fz::cli::run_in_foreground(std::env::args_os().skip(3).map(OsString::into_vec), input);
        if matches!(status - 128, libc::SIGHUP | libc::SIGTERM | libc::SIGQUIT) {
            // SAFETY: fz has restored terminal and signal state. Re-raise in
            // the fresh executable so the parent observes signal death.
            unsafe { libc::signal(status - 128, libc::SIG_DFL); libc::raise(status - 128); }
        }
        std::process::exit(status);
    }
    let args = parse_args();
    if let Some(command) = args.command.as_deref() {
        run_command(command);
    }
    shell::run(args);
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    #[test]
    fn examples_parse_and_option_names_are_unique() {
        for argv in EXAMPLES {
            assert!(matches!(parse(argv.iter().map(OsString::from)), Ok(Parsed::Run(_))), "{argv:?}");
        }
        let mut names = std::collections::HashSet::new();
        for option in OPTIONS {
            for name in option.names { assert!(names.insert(name), "duplicate option {name}"); }
        }
    }
}
