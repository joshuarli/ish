use std::io::Read;

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut shell = epsh::eval::Shell::new();

    if args.len() > 1 {
        // Process options
        let mut i = 1;
        while i < args.len() {
            let arg =
                epsh::shell_bytes::ShellBytes::from_os_str(args[i].as_os_str()).to_shell_string();
            if arg == "-c" {
                // Execute command string
                if i + 1 >= args.len() {
                    eprintln!("epsh: -c requires an argument");
                    std::process::exit(2);
                }
                let script = epsh::shell_bytes::ShellBytes::from_os_str(args[i + 1].as_os_str())
                    .to_shell_string();
                // Remaining args become $0, $1, ...
                if i + 2 < args.len() {
                    let shell_args: Vec<_> = args[i + 2..]
                        .iter()
                        .map(|s| epsh::shell_bytes::ShellBytes::from_os_str(s.as_os_str()))
                        .collect();
                    shell.set_args_bytes(&shell_args);
                }
                let status = shell.run_script(&script);
                std::process::exit(status);
            } else if arg == "-s" {
                // Read from stdin
                break;
            } else if (arg == "-o" || arg == "+o") && i + 1 < args.len() {
                // Long options: -o pipefail, +o pipefail, etc.
                let enable = arg == "-o";
                i += 1;
                let opt = epsh::shell_bytes::ShellBytes::from_os_str(args[i].as_os_str())
                    .to_shell_string();
                match opt.as_str() {
                    "pipefail" => shell.opts_mut().pipefail = enable,
                    "errexit" => shell.opts_mut().errexit = enable,
                    "nounset" => shell.opts_mut().nounset = enable,
                    "xtrace" => shell.opts_mut().xtrace = enable,
                    "noglob" => shell.opts_mut().noglob = enable,
                    "noexec" => shell.opts_mut().noexec = enable,
                    opt => {
                        eprintln!("epsh: unknown option: {opt}");
                        std::process::exit(2);
                    }
                }
                i += 1;
            } else if arg.starts_with('-') && arg.len() > 1 {
                // Shell options like -e, -x, etc.
                for ch in arg[1..].chars() {
                    match ch {
                        'e' => shell.opts_mut().errexit = true,
                        'u' => shell.opts_mut().nounset = true,
                        'x' => shell.opts_mut().xtrace = true,
                        'f' => shell.opts_mut().noglob = true,
                        'n' => shell.opts_mut().noexec = true,
                        _ => {
                            eprintln!("epsh: unknown option: -{ch}");
                            std::process::exit(2);
                        }
                    }
                }
                i += 1;
            } else {
                // Script file
                let filename = &args[i];
                let shell_args: Vec<_> = args[i..]
                    .iter()
                    .map(|s| epsh::shell_bytes::ShellBytes::from_os_str(s.as_os_str()))
                    .collect();
                shell.set_args_bytes(&shell_args);
                let content = match std::fs::read(filename) {
                    Ok(bytes) => epsh::encoding::bytes_to_str(&bytes),
                    Err(e) => {
                        let filename =
                            epsh::shell_bytes::ShellBytes::from_os_str(filename.as_os_str())
                                .to_shell_string();
                        eprintln!("epsh: {filename}: {e}");
                        std::process::exit(127);
                    }
                };
                let status = shell.run_script(&content);
                std::process::exit(status);
            }
        }
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

    // Read from stdin (pipe) — preserve non-UTF-8 bytes via PUA encoding
    let mut bytes = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut bytes) {
        eprintln!("epsh: {e}");
        std::process::exit(1);
    }
    let input = epsh::encoding::bytes_to_str(&bytes);
    let status = shell.run_script(&input);
    std::process::exit(status);
}
