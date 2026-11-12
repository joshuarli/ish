use ish::report;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::process::Command;
mod shell;
mod rmrf;

struct Args {
    config: Option<OsString>,
    no_config: bool,
    command: Option<OsString>,
}

fn parse_args() -> Args {
    let mut args = Args {
        config: None,
        no_config: false,
        command: None,
    };
    let mut argv = std::env::args_os().skip(1);
    while let Some(arg) = argv.next() {
        match arg.to_str() {
            Some("-V") | Some("--version") => {
                println!("ish {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Some("-h") | Some("--help") => {
                print_help();
                std::process::exit(0);
            }
            Some("-c") => match argv.next() {
                Some(command) => args.command = Some(command),
                None => {
                    report!("ish: -c requires a command");
                    std::process::exit(1);
                }
            },
            Some("--config") => match argv.next() {
                Some(path) => args.config = Some(path),
                None => {
                    report!("ish: --config requires a config file path");
                    std::process::exit(1);
                }
            },
            Some("--no-config") => args.no_config = true,
            _ => {
                report!("ish: this shell is interactive-only and does not run scripts");
                report!("usage: ish [-c command] [--config config] [--no-config] [-h|--help]");
                std::process::exit(1);
            }
        }
    }
    args
}

fn print_help() {
    println!("ish — minimal interactive shell");
    println!();
    println!("usage: ish [-c command] [--config config] [--no-config] [-V|--version] [-h|--help]");
    println!();
    println!("options:");
    println!("  -c <command>     Run a command with /bin/sh and exit (login-shell contract)");
    println!("  --config <path>  Use a custom config file instead of the default");
    println!("  --no-config      Skip loading the config file");
    println!("  -V, --version    Show version");
    println!("  -h, --help       Show this help message");
}

fn run_command(command: &OsStr) -> ! {
    let error = Command::new("/bin/sh").arg("-c").arg(command).exec();
    report!("ish: cannot run /bin/sh: {error}");
    std::process::exit(127);
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
