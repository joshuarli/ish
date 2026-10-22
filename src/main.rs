use std::ffi::OsString;
mod shell;
mod trash;

struct Args {
    config: Option<OsString>,
    no_config: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        config: None,
        no_config: false,
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
                Some(path) => args.config = Some(path),
                None => {
                    eprintln!("ish: -c requires a config file path");
                    std::process::exit(1);
                }
            },
            Some("--no-config") => args.no_config = true,
            _ => {
                eprintln!("ish: this shell is interactive-only and does not run scripts");
                eprintln!("usage: ish [-c config] [--no-config] [-h|--help]");
                std::process::exit(1);
            }
        }
    }
    args
}

fn print_help() {
    println!("ish — minimal interactive shell");
    println!();
    println!("usage: ish [-c config] [--no-config] [-V|--version] [-h|--help]");
    println!();
    println!("options:");
    println!("  -c <path>      Use a custom config file instead of the default");
    println!("  --no-config    Skip loading the config file");
    println!("  -V, --version  Show version");
    println!("  -h, --help     Show this help message");
}



fn main() { shell::run(parse_args()); }
