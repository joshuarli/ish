use ish::alias::AliasMap;
use ish::cd_rewrite::{maybe_rewrite_cd, resolve_cd_for_history};
use ish::complete::needs_quoting;
use ish::diagnostics::{self, base64_encode};
use ish::mode::{CompletionMode, DirPicker, HistorySearch, Mode};
use ish::complete::CompletionState;
use ish::history::FuzzyMatch;
use ish::input::{InputEvent, InputReader, Key, KeyEvent};
use ish::job::{self, Job};
use ish::external;
use ish::line::LineBuffer;
use ish::term::TermWriter;
use ish::{builtin, capture, complete, config, denv, dispatch, frecency, history, path, prompt, render, signal, term};
use std::ffi::OsString;
use std::path::Path;
use ish::builtin::ShellCommand;
mod trash;

struct Shell {
    aliases: AliasMap,
    last_status: i32,
    dir_stack: Vec<String>,
    rows: u16,
    cols: u16,
    history: history::History,
    prompt: prompt::Prompt,
    /// Reusable buffer for rendered prompt string — avoids allocation per render.
    prompt_buf: String,
    /// Reusable completion arena — avoids allocation per tab press.
    completion_buffer: complete::Completions,
    /// Reusable fuzzy match buffer — avoids allocation per Ctrl+R keystroke.
    history_match_buffer: Vec<history::FuzzyMatch>,
    /// The previous evaluated command may have left the terminal mid-line.
    /// The next prompt must begin at a fresh row so its layout starts from a
    /// known column, even when command output omitted its final newline.
    prompt_needs_line_start: bool,
    job: Option<Job>,
    path_cache: path::PathCache,
    exit_warned: bool,
    input_reader: Option<InputReader>,
    home: Option<String>,
    home_display: String,
    session_log: String,
    last_output: capture::OutputBuffer,
    layout_dump: String,
    epsh: epsh::eval::Shell,
    denv: denv::Session,
    shell_pid: i32,
}

enum ReadResult {
    Line(String),
    Exit,
    /// SIGTERM or SIGHUP arrived through the self-pipe.
    Terminated(i32),
    Empty,
}

struct Args {
    config: Option<OsString>, // -c <path>: custom config file
    no_config: bool,          // --no-config: skip config loading
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

fn ish_command_policy(target: &epsh::shell_bytes::ShellBytes) -> Option<String> {
    dispatch::check_expanded_command(target)
        .err()
        .map(|rejection| rejection.to_string())
}

fn main() {
    let cli = parse_args();

    // Set $SHELL to ish binary path
    if let Ok(exe) = std::env::current_exe() {
        ish::shell_setenv_os("SHELL", exe.as_os_str());
    }

    // Set $PWD — parent process (terminal emulator) may not provide it
    if let Ok(cwd) = std::env::current_dir() {
        ish::shell_setenv_os("PWD", cwd.as_os_str());
    }

    // Initialize signals before taking terminal control. When launched through
    // macOS login(1), the shell starts in a background process group behind the
    // login process; tcsetpgrp from that state raises SIGTTOU unless ignored.
    let signal_fd = signal::init();
    term::install_panic_hook();

    let pid = rustix::process::getpid();
    if let Err(error) = rustix::process::setpgid(None, None)
        && error != rustix::io::Errno::PERM
    {
        eprintln!("ish: cannot become a process group leader: {error}");
    }
    let stdin = term::stdin();
    if let Err(error) = rustix::termios::tcsetpgrp(stdin, pid) {
        eprintln!("ish: cannot take the terminal: {error}");
    }

    let (rows, cols) = term::term_size();
    let shell_pid = rustix::process::getpid().as_raw_pid();

    let cwd = std::env::current_dir().unwrap_or_default();
    let mut epsh = epsh::eval::Shell::builder()
        .cwd(cwd)
        .interactive(true)
        .build();
    for name in builtin::all_builtin_names() {
        let Some(spec) = builtin::lookup(name) else {
            continue;
        };
        epsh.set_command_identity(
            epsh::shell_bytes::ShellBytes::from(name),
            epsh::eval::CommandIdentity::Builtin,
        );
        if spec.exec == builtin::Exec::Foreground {
            epsh.set_command_placement(
                epsh::shell_bytes::ShellBytes::from(name),
                epsh::eval::CommandPlacement::ParentProcess,
            );
        }
    }
    let fs_default_scratch = store_env_os(&epsh, "TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    let initial_home = store_env_os(&epsh, "HOME");
    let mut aliases = AliasMap::new();
    if !cli.no_config {
        config::load_with_home(
            &mut aliases,
            &mut epsh,
            cli.config.as_deref(),
            initial_home.as_deref(),
        );
    }
    let user = store_env_display(&epsh, "USER");
    let home = store_home_text(&epsh);
    let home_display = store_env_display(&epsh, "HOME");
    let home_os = store_env_os(&epsh, "HOME");

    let denv = denv::init(&mut epsh);
    let mut shell = Shell {
        aliases,
        last_status: 0,
        dir_stack: Vec::with_capacity(32),
        rows,
        cols,
        history: history::History::load_from_home(home_os.as_deref()).unwrap_or_else(|error| {
            eprintln!("ish: cannot open history: {error}");
            std::process::exit(1);
        }),
        prompt: prompt::Prompt::with_identity(&user, &home_display),
        prompt_buf: String::with_capacity(128),
        completion_buffer: complete::Completions::with_capacity(2048, 64),
        history_match_buffer: Vec::with_capacity(200),
        prompt_needs_line_start: false,
        job: None,
        path_cache: path::PathCache::new(),
        exit_warned: false,
        input_reader: Some(InputReader::new(signal_fd)),
        home,
        home_display,
        session_log: String::new(),
        last_output: capture::OutputBuffer::default(),
        layout_dump: String::new(),
        epsh,
        denv,
        shell_pid,
    };

    let changes = denv::on_startup(&shell.denv, &shell.epsh);
    apply_denv_changes(&changes, &mut shell.epsh);
    sync_shell_store_derived(&mut shell);
    shell
        .epsh
        .set_command_policy(Box::new(ish_command_policy));

    // Main loop
    let mut exit_code = 0;
    loop {
        if let Some(sig) = signal::take_pending_termination() {
            if let Some(job) = shell.job.take() {
                job::abandon(job);
            }
            exit_code = 128 + sig;
            break;
        }
        shell
            .epsh
            .set_exit_status(epsh::error::ExitStatus::from(shell.last_status));
        if let Err(error) = shell.history.sync() {
            eprintln!("ish: cannot sync history: {error}");
        }
        match read_line(&mut shell) {
            ReadResult::Line(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                // A second `exit` confirms the warning the first one printed.
                if line.split_whitespace().next() != Some("exit") {
                    shell.exit_warned = false;
                }

                let line = maybe_rewrite_cd(
                    &line,
                    &shell.aliases,
                    store_env_os(&shell.epsh, "HOME").as_deref().unwrap_or(std::ffi::OsStr::new("")),
                    store_path_env(&shell.epsh),
                );

                // Expand aliases before recording in history so "g status"
                // becomes "git status" — matches what actually runs.
                // Resolve relative cd paths to absolute so `z` can use them.
                let history_line =
                    resolve_cd_for_history(&expand_aliases_for_history(&line, &shell.aliases));
                let history_cwd = shell.epsh.cwd().to_path_buf();

                // Persist accepted input before execution so killing the shell
                // or a long-running command cannot lose the command itself.
                if history_line.trim() != "l"
                    && let Err(error) = shell.history.add_in_dir(&history_line, Some(&history_cwd))
                {
                    eprintln!("ish: command was not saved to history: {error}");
                }

                // Log for session transcript
                shell.session_log.push_str(&line);
                shell.session_log.push('\n');

                let expanded = expand_aliases_for_line(&line, &shell.aliases);

                let plan = match dispatch::plan(&expanded) {
                    Ok(plan) => plan,
                    Err(rejection) => {
                        eprintln!("ish: {rejection}");
                        shell.last_status = 2;
                        continue;
                    }
                };

                let program = match plan {
                    dispatch::Plan::Builtin { name, args } => {
                        let quit = if builtin::replaces_last_output(name) {
                            let buffer = std::mem::take(&mut shell.last_output);
                            let (quit, session) = capture_output(buffer, || {
                                run_shell_builtin(name, &args, &expanded, &mut shell)
                            });
                            if let Some(session) = session {
                                session.finish_into(&mut shell.last_output);
                            }
                            quit
                        } else {
                            run_shell_builtin(name, &args, &expanded, &mut shell)
                        };
                        if quit {
                            exit_code = shell.last_status;
                            break;
                        }
                        continue;
                    }
                    dispatch::Plan::Epsh(program) => program,
                };

                // Save cwd before execution to detect cd
                let prev_cwd = shell.epsh.cwd().to_path_buf();

                // Set up external handler for ish-specific builtins
                let shell_pid = shell.shell_pid;
                let history_log = shell
                    .history
                    .storage_path()
                    .expect("interactive history has a storage path")
                    .to_path_buf();
                let handler = external::make_external_handler(shell_pid, history_log, fs_default_scratch.clone());
                shell.epsh.set_external_handler(handler);
                external::begin_command(shell.job.is_some());

                let buffer = std::mem::take(&mut shell.last_output);
                let (status, mut session) =
                    capture_output(buffer, || shell.epsh.run_program(&program).code());
                shell.last_status = status;
                sync_shell_store_derived(&mut shell);

                // Detect cwd change from epsh (compound commands with cd, pushd, etc.)
                if shell.epsh.cwd() != prev_cwd {
                    let new_cwd = shell.epsh.cwd().to_path_buf();
                    let _ = std::env::set_current_dir(&new_cwd);
                    sync_cwd_change(&mut shell, &prev_cwd, new_cwd);
                }

                // Detect job suspension (status 148 = 128 + SIGTSTP).
                if shell.last_status == 148
                    && let Some(external::StoppedJob { pgid, cmd, termios }) =
                        external::take_stopped_job()
                {
                    eprintln!("ish: stopped: {} (pgid={})", cmd, pgid);
                    shell.job = Some(Job { pgid, cmd, termios, capture: session.take() });
                }
                if let Some(session) = session {
                    session.finish_into(&mut shell.last_output);
                }

                shell.prompt_needs_line_start = true;
            }
            ReadResult::Exit => {
                break;
            }
            ReadResult::Terminated(sig) => {
                // The terminal is already restored: read_line's raw-mode guard
                // dropped on its way out.
                if let Some(job) = shell.job.take() {
                    job::abandon(job);
                }
                exit_code = 128 + sig;
                break;
            }
            ReadResult::Empty => {}
        }
    }

    drop(shell);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

/// Run `run` with the shell's terminal output captured. The session is `None`
/// when capture is unavailable, which is reported unless stdout is not a
/// terminal. Otherwise the caller stores the output with
/// `Session::finish_into`, or hands the session to a suspended job.
fn capture_output<T>(
    mut buffer: capture::OutputBuffer,
    run: impl FnOnce() -> T,
) -> (T, Option<capture::Session>) {
    buffer.clear();
    match capture::Capture::begin(buffer) {
        Ok(Some(capture)) => {
            let result = run();
            (result, Some(capture.end()))
        }
        Ok(None) => (run(), None),
        Err(error) => {
            eprintln!("ish: output capture unavailable: {error}");
            (run(), None)
        }
    }
}

fn write_clipboard(bytes: &[u8]) {
    use std::io::Write;
    let encoded = base64_encode(bytes);
    let osc = format!("\x1b]52;c;{encoded}\x07");
    let _ = std::io::stdout().write_all(osc.as_bytes());
    let _ = std::io::stdout().flush();
}

fn copy_last_output(args: &[epsh::ast::Word], output: &capture::OutputBuffer) -> i32 {
    if !args.is_empty() {
        eprintln!("ish: ?: takes no arguments");
        return 2;
    }
    let text = output.plain_text();
    if text.is_empty() {
        eprintln!("ish: ?: the last command wrote no text to the terminal");
        return 1;
    }
    if output.is_truncated() {
        eprintln!(
            "ish: ?: output exceeded {} KiB; copied only its end",
            capture::OUTPUT_LIMIT / 1024
        );
    }
    write_clipboard(&text);
    0
}

/// Run a shell-owned builtin that is the whole input line. Returns true when
/// the shell should quit. `line` is the alias-expanded text; `args` are the
/// unexpanded shell words after the command name.
fn run_shell_builtin(name: &str, args: &[epsh::ast::Word], line: &str, shell: &mut Shell) -> bool {
    // Builtins that take arguments expand them first, so quoting, variables,
    // command substitution, and globbing behave as for any command. A failed
    // expansion has already reported itself and set the status.
    macro_rules! expanded_args {
        () => {
            match expand_builtin_args(args, shell, name) {
                Some(args) => args,
                None => return false,
            }
        };
    }

    let Some(command) = builtin::lookup(name).and_then(builtin::Builtin::shell_command) else {
        eprintln!("ish: internal error: no shell handler for builtin {name}");
        shell.last_status = 1;
        return false;
    };

    match command {
        ShellCommand::Alias => handle_alias(line, shell),
        ShellCommand::Exit => {
            return handle_exit_command(line, shell);
        }
        ShellCommand::ChangeDirectory => {
            let args = match expand_builtin_args_bytes(args, shell, name) {
                Some(args) => args,
                None => return false,
            };
            if args.len() > 1 {
                eprintln!("ish: cd: too many arguments");
                shell.last_status = 1;
            } else if args.first().map(|arg| arg.as_bytes()) == Some(b"-".as_slice()) {
                if let Some(prev) = store_env_os(&shell.epsh, "OLDPWD").filter(|s| !s.is_empty()) {
                    eprintln!("{}", Path::new(&prev).display());
                    shell.last_status = change_directory_path(Path::new(&prev), shell);
                } else {
                    eprintln!("ish: cd: no previous directory");
                    shell.last_status = 1;
                }
            } else {
                let home = store_env_os(&shell.epsh, "HOME").unwrap_or_default();
                let target = match args.first().map(|arg| arg.as_bytes()) {
                    None | Some(b"") | Some(b"~") => std::path::PathBuf::from(home),
                    Some(bytes) => {
                        if let Some(relative) = bytes.strip_prefix(b"~/") {
                            use std::os::unix::ffi::OsStrExt;
                            Path::new(&home).join(std::ffi::OsStr::from_bytes(relative))
                        } else {
                            args[0].to_path_buf()
                        }
                    }
                };
                shell.last_status = change_directory_path(&target, shell);
            }
        }
        ShellCommand::History => {
            handle_history(line, shell);
        }
        ShellCommand::CopyScrollback => {
            write_clipboard(shell.session_log.as_bytes());
            shell.last_status = 0;
        }
        ShellCommand::CopyLastOutput => shell.last_status = copy_last_output(args, &shell.last_output),
        ShellCommand::DumpLayout => shell.last_status = write_layout_dump(shell),
        ShellCommand::Trash => {
            let args = match expand_builtin_args_bytes(args, shell, name) {
                Some(args) => args
                    .iter()
                    .map(epsh::shell_bytes::ShellBytes::to_os_string)
                    .collect::<Vec<_>>(),
                None => return false,
            };
            match trash::parse_args(&args) {
                Ok(trash::Command::Help) => {
                    println!("Usage: {name} [--yes] [--] PATH");
                    println!("Scan PATH before permanently deleting its entries. PATH stays in place.");
                    println!("Type 'yes' to confirm. --yes skips confirmation.");
                    shell.last_status = 0;
                }
                Ok(trash::Command::Run { path, assume_yes }) => {
                    match trash::run(path, assume_yes, store_env_os(&shell.epsh, "HOME")) {
                        Ok(()) => shell.last_status = 0,
                        Err(error) => {
                            eprintln!("ish: {name}: {error}");
                            shell.last_status = if error.kind() == std::io::ErrorKind::Interrupted { 130 } else { 1 };
                        }
                    }
                }
                Err(error) => {
                    eprintln!("ish: {name}: {error}");
                    shell.last_status = 2;
                }
            }
        }
        ShellCommand::Denv => {
            let args = expanded_args!();
            let outcome = denv::command(&shell.denv, &args, &shell.epsh);
            apply_denv_changes(&outcome.changes, &mut shell.epsh);
            sync_shell_store_derived(shell);
            shell.last_status = outcome.status;
        }
        ShellCommand::ResumeJob => {
            shell.last_status = ish::job::resume_job(&mut shell.job, &mut shell.last_output);
        }
        ShellCommand::Jump => {
            let args = expanded_args!();
            let Some(home) = shell.home.as_deref() else {
                eprintln!("ish: z: HOME is not UTF-8");
                shell.last_status = 1;
                return false;
            };
            shell.last_status = frecency::builtin_z(&args, &shell.history, home);
            if shell.last_status == 0 {
                // z already did chdir — run post-cd hooks
                let old_cwd = shell.epsh.cwd().to_path_buf();
                let new_cwd = std::env::current_dir().unwrap_or_default();
                sync_cwd_change(shell, &old_cwd, new_cwd);
            }
        }
        ShellCommand::Locate => {
            let args = expanded_args!();
            shell.last_status = builtin::locate_command(
                &args,
                &shell.aliases,
                |name| shell.epsh.functions().contains_key(name),
                store_path_env(&shell.epsh),
            );
        }
    }
    false
}

fn capture_layout_dump(
    shell: &mut Shell,
    mode: &Mode,
    line: &LineBuffer,
    prompt_display_len: usize,
    region: render::RenderedRegion,
) {
    let suggestion =
        if line.text().len() >= 3 && !line.has_newlines() && line.cursor() == line.text().len() {
            shell
                .history
                .session_prefix_search(line.text(), 0)
                .and_then(|entry| entry.strip_prefix(line.text()))
        } else {
            None
        };
    let snapshot = diagnostics::LayoutSnapshot {
        rows: shell.rows,
        cols: shell.cols,
        prompt_display_len,
        line,
        suggestion_display_len: suggestion.map(ish::line::str_width),
        region,
        mode,
    };
    diagnostics::render_layout_dump(&mut shell.layout_dump, snapshot);
}

/// Write the captured layout dump to a fresh file in `~/.cache/ish/dump-<hex>`.
/// Returns the written path on success, or an error message. Deliberately does
/// not write to the terminal, so Ctrl+P can capture a corrupted screen without
/// changing its contents. The dump holds geometry and lengths only, never typed
/// text, and the directory and files are private to the user.
fn write_layout_dump_to_file(shell: &Shell) -> Result<String, String> {
    let home = store_env_os(&shell.epsh, "HOME").unwrap_or_default();
    diagnostics::write_layout_dump_file(&home, &shell.layout_dump)
}

fn write_layout_dump(shell: &Shell) -> i32 {
    match write_layout_dump_to_file(shell) {
        Ok(path) => {
            println!("ish-dump: wrote {path}");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

fn store_env_os(epsh: &epsh::eval::Shell, name: &str) -> Option<OsString> {
    epsh.vars()
        .get_bytes(name)
        .map(|value| value.to_os_string())
}

fn store_env_display(epsh: &epsh::eval::Shell, name: &str) -> String {
    epsh.vars().get_bytes(name)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `$PATH` as raw bytes from epsh's variable store — the single source of
/// truth for the environment. Empty when unset. ish's in-process PATH lookups
/// (command coloring, `which`/`type`, completion) use this instead of the OS
/// environment, which is only a denv mirror and can be stale after
/// `set`/`export`/`unset`.
fn store_path_env(epsh: &epsh::eval::Shell) -> &[u8] {
    epsh.vars().get_bytes("PATH").map_or(&[], |v| v.as_bytes())
}

fn store_home_text(epsh: &epsh::eval::Shell) -> Option<String> {
    match epsh.vars().get_bytes("HOME") {
        Some(value) => value.as_utf8_str().map(str::to_owned),
        None => Some(String::new()),
    }
}

fn sync_shell_store_derived(shell: &mut Shell) {
    shell.home = store_home_text(&shell.epsh);
    let user = store_env_display(&shell.epsh, "USER");
    shell.home_display = store_env_display(&shell.epsh, "HOME");
    shell.prompt.set_identity(&user, &shell.home_display);
}

fn read_line(shell: &mut Shell) -> ReadResult {
    let mut reader = shell
        .input_reader
        .take()
        .expect("input reader is present between prompts");
    let result = read_line_with_reader(shell, &mut reader);
    shell.input_reader = Some(reader);
    result
}

struct EditView {
    tw: TermWriter,
    line: LineBuffer,
    history_idx: Option<usize>,
    saved_line: String,
    grid_column: Option<usize>,
    prompt_str: String,
    prompt_display_len: usize,
    region: render::RenderedRegion,
    history_cache: render::HistoryPagerCache,
}

enum Next {
    Stay,
    Normal,
}

fn emit_working_directory(shell: &Shell) {
    let pwd = shell.epsh.vars().get("PWD").unwrap_or("");
    if !pwd.is_empty() {
        let hostname = shell.epsh.vars().get("HOSTNAME").unwrap_or("");
        // Percent-encode every byte outside the URI unreserved set so
        // control and non-ASCII bytes cannot end or inject sequences.
        let mut osc = String::from("\x1b]7;file://");
        prompt::push_percent_encoded(&mut osc, hostname.as_bytes(), false);
        prompt::push_percent_encoded(&mut osc, pwd.as_bytes(), true);
        osc.push('\x07');
        let _ = std::io::Write::write_all(&mut std::io::stdout(), osc.as_bytes());
    }
}

fn read_line_with_reader(shell: &mut Shell, reader: &mut InputReader) -> ReadResult {
    let _raw = match term::RawMode::enable() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ish: raw mode: {e}");
            return ReadResult::Exit;
        }
    };

    let mut tw = TermWriter::new();
    let line = LineBuffer::new();
    emit_working_directory(shell);

    if shell.prompt_needs_line_start {
        // Commands write directly to the terminal, so ish cannot observe
        // whether their output ended with a newline. Start the prompt on a
        // fresh row to keep prompt geometry independent of child output.
        tw.write_str("\r\n");
        shell.prompt_needs_line_start = false;
    }

    // Render prompt — zero-alloc: reuse shell.prompt_buf, read state from epsh.
    // Take ownership so prompt_str can be borrowed independently of shell.
    let pwd = shell.epsh.vars().get("PWD").unwrap_or("");
    let denv_dirty = shell.epsh.vars().get("__DENV_DIRTY") == Some("1");
    shell
        .prompt
        .render_into(&mut shell.prompt_buf, shell.last_status, pwd, denv_dirty);
    let prompt_str = std::mem::take(&mut shell.prompt_buf);
    let prompt_display_len = shell.prompt.display_len(&prompt_str);

    let region = render_prompt_region(
        &mut tw,
        shell,
        &line,
        &prompt_str,
        prompt_display_len,
        render::RenderedRegion::default(),
    );
    let _ = tw.flush_to_stdout();

    let mut view = EditView {
        tw,
        line,
        history_idx: None,
        saved_line: String::new(),
        grid_column: None,
        prompt_str,
        prompt_display_len,
        region,
        history_cache: render::HistoryPagerCache::default(),
    };
    let mut mode = Mode::Normal;

    loop {
        let finished = match reader.read_event() {
            InputEvent::Signal(sig) => handle_signal(&mut view, &mut mode, shell, sig),
            InputEvent::Paste(text) => {
                handle_paste(&mut view, &mut mode, shell, &text);
                None
            }
            InputEvent::PasteRejected => {
                view.grid_column = None;
                // The line stays as typed (a message inserted into it could be
                // run); ring the bell instead of repainting.
                view.tw.write_str("\x07");
                let _ = view.tw.flush_to_stdout();
                None
            }
            InputEvent::Key(key) => handle_key(&mut view, &mut mode, shell, key),
        };
        if let Some(result) = finished {
            return result;
        }
    }
}

fn repaint(view: &mut EditView, mode: &Mode, shell: &mut Shell) {
    view.region = render_active_mode(
        &mut view.tw,
        mode,
        shell,
        &view.line,
        &view.prompt_str,
        view.prompt_display_len,
        view.region,
        &mut view.history_cache,
    );
    let _ = view.tw.flush_to_stdout();
}

fn redraw_prompt_line(view: &mut EditView, shell: &Shell) {
    view.region = render::render_line(
        &mut view.tw,
        &view.prompt_str,
        view.prompt_display_len,
        &view.line,
        shell.cols,
        render::RenderedRegion::default(),
        &render::RenderOpts::default(),
    );
    let _ = view.tw.flush_to_stdout();
}

fn handle_signal(
    view: &mut EditView,
    mode: &mut Mode,
    shell: &mut Shell,
    sig: i32,
) -> Option<ReadResult> {
    if sig == rustix::process::Signal::TERM.as_raw() || sig == rustix::process::Signal::HUP.as_raw()
    {
        return Some(ReadResult::Terminated(sig));
    }
    if sig == rustix::process::Signal::WINCH.as_raw() {
        let (rows, cols) = term::term_size();
        shell.rows = rows;
        shell.cols = cols;
        view.grid_column = None;
        update_mode_layout_for_resize(mode, shell);

        // Terminal emulators reflow the live cursor and their
        // DEC saved cursor differently on resize. The saved
        // RenderedRegion anchor is therefore no longer a safe
        // basis for an incremental repaint. Start from a known
        // screen position and rebuild the active region.
        view.tw.clear_screen();
        view.region = render::RenderedRegion::default();
        view.history_cache.clear();
    }
    repaint(view, mode, shell);
    None
}

fn handle_paste(view: &mut EditView, mode: &mut Mode, shell: &mut Shell, text: &str) {
    view.grid_column = None;
    if let Mode::HistorySearch(search) = mode {
        let HistorySearch {
            query,
            matches,
            candidates,
            scratch,
            candidate_stack,
            spare_candidates,
            previous_query,
            selected,
            ..
        } = search;
        previous_query.clear();
        previous_query.push_str(query.text());
        let previous_cursor = query.cursor();
        query.insert_str(text);
        update_history_search(
            query,
            (previous_query, previous_cursor),
            matches,
            HistoryCandidates {
                candidates,
                scratch,
                candidate_stack,
                spare_candidates,
            },
            selected,
            shell,
        );
    } else {
        view.line.insert_str(text);
    }
    repaint(view, mode, shell);
}

fn handle_key(
    view: &mut EditView,
    mode: &mut Mode,
    shell: &mut Shell,
    key: KeyEvent,
) -> Option<ReadResult> {
    // Ctrl+P writes a layout dump in any mode and does not repaint,
    // so it captures the screen exactly as it is (including the
    // status line) without disturbing it. Intercept before mode
    // dispatch so every mode is covered uniformly.
    if key.key == Key::Char('p') && key.mods.ctrl {
        capture_layout_dump(shell, mode, &view.line, view.prompt_display_len, view.region);
        if let Err(message) = write_layout_dump_to_file(shell) {
            eprintln!("{message}");
        }
        return None;
    }
    let next = match &mut *mode {
        Mode::Normal => return handle_normal_key_event(view, mode, shell, key),
        Mode::Completion(completion) => handle_completion_key_event(view, shell, key, completion),
        Mode::HistorySearch(search) => handle_history_key_event(view, shell, key, search),
        Mode::DirPicker(picker) => handle_dir_picker_key_event(view, shell, key, picker),
    };
    if let Next::Normal = next {
        *mode = Mode::Normal;
    }
    None
}

fn handle_normal_key_event(
    view: &mut EditView,
    mode: &mut Mode,
    shell: &mut Shell,
    key: KeyEvent,
) -> Option<ReadResult> {
    match handle_normal_key(
        key,
        &mut view.line,
        &mut view.history_idx,
        &mut view.saved_line,
        &mut view.grid_column,
        view.prompt_display_len,
        shell,
    ) {
        KeyAction::Continue => {}
        KeyAction::Execute(text) => {
            capture_layout_dump(shell, mode, &view.line, view.prompt_display_len, view.region);
            // Clear autosuggestion ghost text before freezing the line
            if view.line.cursor() == view.line.text().len() {
                view.tw.write_str("\x1b[J");
            }
            view.tw.write_str("\r\n");
            let _ = view.tw.flush_to_stdout();
            shell.prompt_buf = std::mem::take(&mut view.prompt_str);
            return Some(if text.is_empty() {
                ReadResult::Empty
            } else {
                ReadResult::Line(text)
            });
        }
        KeyAction::Continuation => {
            view.line.insert_char('\n');
            view.history_idx = None;
        }
        KeyAction::Cancel => {
            view.tw.write_str("^C\r\n");
            let _ = view.tw.flush_to_stdout();
            shell.prompt_buf = std::mem::take(&mut view.prompt_str);
            return Some(ReadResult::Empty);
        }
        KeyAction::Exit => {
            view.tw.write_str("\r\n");
            let _ = view.tw.flush_to_stdout();
            shell.prompt_buf = std::mem::take(&mut view.prompt_str);
            return Some(handle_exit(shell));
        }
        KeyAction::ClearScreen => {
            view.tw.clear_screen();
            view.region = render::RenderedRegion::default();
        }
        KeyAction::StartHistorySearch => {
            start_history_search(view, mode, shell);
            return None;
        }
        KeyAction::StartDirPicker => {
            if start_dir_picker(view, mode, shell) {
                return None;
            }
        }
        KeyAction::StartCompletion => start_completion_mode(view, mode, shell),
    }

    repaint(view, mode, shell);
    None
}

fn start_history_search(view: &mut EditView, mode: &mut Mode, shell: &mut Shell) {
    if let Err(error) = shell.history.sync() {
        eprintln!("ish: cannot sync history: {error}");
    }
    view.saved_line = view.line.text().to_string();
    let mut candidates = Vec::new();
    shell.history.search_entry_indices_into(&mut candidates);
    let mut scratch = Vec::new();
    let mut matches = std::mem::take(&mut shell.history_match_buffer);
    shell.history.fuzzy_search_subset_into_in_dir(
        "",
        &candidates,
        &mut scratch,
        &mut matches,
        200,
        shell.epsh.cwd(),
    );
    std::mem::swap(&mut candidates, &mut scratch);
    view.region.clear(&mut view.tw);
    let search = HistorySearch {
        query: LineBuffer::new(),
        matches,
        candidates,
        scratch,
        candidate_stack: Vec::new(),
        spare_candidates: Vec::new(),
        previous_query: String::new(),
        selected: 0,
        saved_line: view.saved_line.clone(),
    };
    view.history_cache.clear();
    view.region = render_history_mode(
        &mut view.tw,
        &search,
        shell,
        render::RenderedRegion::default(),
        &mut view.history_cache,
    );
    *mode = Mode::HistorySearch(search);
    let _ = view.tw.flush_to_stdout();
}

fn start_dir_picker(view: &mut EditView, mode: &mut Mode, shell: &Shell) -> bool {
    // Show dir stack in reverse (most recent first),
    // excluding the current directory
    let pwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let entries: Vec<String> = shell
        .dir_stack
        .iter()
        .rev()
        .filter(|d| *d != &pwd)
        .cloned()
        .collect();
    if entries.is_empty() {
        return false;
    }
    view.region.clear(&mut view.tw);
    let picker = DirPicker {
        entries,
        selected: 0,
    };
    view.region = render_dir_picker_mode(&mut view.tw, &picker, shell, view.region);
    *mode = Mode::DirPicker(picker);
    let _ = view.tw.flush_to_stdout();
    true
}

fn start_completion_mode(view: &mut EditView, mode: &mut Mode, shell: &mut Shell) {
    let comp = std::mem::take(&mut shell.completion_buffer);
    let base_line = view.line.clone();
    let mut cs = start_completion(
        &base_line,
        shell.cols,
        shell.home.as_deref(),
        &shell.aliases,
        &shell.epsh,
        comp,
    );
    if cs.comp.len() == 1 {
        preview_completion(&mut view.line, &cs, &base_line);
        shell.completion_buffer = cs.comp;
    } else if !cs.comp.is_empty() {
        cs.selected = usize::MAX;
        preview_completion(&mut view.line, &cs, &base_line);
        *mode = Mode::Completion(CompletionMode {
            state: cs,
            base_line,
        });
    } else {
        shell.completion_buffer = cs.comp;
    }
}

fn handle_completion_key_event(
    view: &mut EditView,
    shell: &mut Shell,
    key: KeyEvent,
    completion: &mut CompletionMode,
) -> Next {
    let CompletionMode { state, base_line } = completion;
    let (p, pdl) = active_prompt(&view.prompt_str, view.prompt_display_len);
    let next = match handle_completion_key(key, state, base_line) {
        CompletionAction::Navigate => {
            preview_completion(&mut view.line, state, base_line);
            // Cursor is on prompt line — repaint grid in-place
            let info = render::render_line(
                &mut view.tw,
                p,
                pdl,
                &view.line,
                shell.cols,
                view.region,
                &render::RenderOpts::default(),
            );
            view.region = info;
            render::render_completions(&mut view.tw, state, info, false);
            let _ = view.tw.flush_to_stdout();
            return Next::Stay;
        }
        CompletionAction::Refilter => {
            // Reclaim buffer from current state, re-run completion
            let selected = state.selected;
            let comp = std::mem::take(&mut state.comp);
            let mut cs = start_completion(
                base_line,
                shell.cols,
                shell.home.as_deref(),
                &shell.aliases,
                &shell.epsh,
                comp,
            );
            if !cs.comp.is_empty() {
                cs.selected = if selected == usize::MAX {
                    usize::MAX
                } else {
                    selected.min(cs.comp.len() - 1)
                };
                preview_completion(&mut view.line, &cs, base_line);
                let info = render::render_line(
                    &mut view.tw,
                    p,
                    pdl,
                    &view.line,
                    shell.cols,
                    view.region,
                    &render::RenderOpts::default(),
                );
                view.region = info;
                render::render_completions(&mut view.tw, &cs, info, true);
                *completion = CompletionMode {
                    state: cs,
                    base_line: base_line.clone(),
                };
                let _ = view.tw.flush_to_stdout();
                return Next::Stay;
            } else {
                view.line = base_line.clone();
                shell.completion_buffer = cs.comp;
                Next::Normal
            }
        }
        CompletionAction::Accept => {
            preview_completion(&mut view.line, state, base_line);
            shell.completion_buffer = std::mem::take(&mut state.comp);
            Next::Normal
        }
        CompletionAction::Cancel => {
            view.line = base_line.clone();
            shell.completion_buffer = std::mem::take(&mut state.comp);
            Next::Normal
        }
    };

    // Cursor is on prompt line — render_line's \r + clear_to_end_of_screen
    // naturally clears the grid below
    let (p, pdl) = active_prompt(&view.prompt_str, view.prompt_display_len);
    let info = render::render_line(
        &mut view.tw,
        p,
        pdl,
        &view.line,
        shell.cols,
        view.region,
        &render::RenderOpts::default(),
    );
    view.region = info;
    let _ = view.tw.flush_to_stdout();
    next
}

fn handle_history_key_event(
    view: &mut EditView,
    shell: &mut Shell,
    key: KeyEvent,
    search: &mut HistorySearch,
) -> Next {
    let action = {
        let HistorySearch {
            query,
            matches,
            candidates,
            scratch,
            candidate_stack,
            spare_candidates,
            previous_query,
            selected,
            ..
        } = &mut *search;
        handle_history_search_key(
            key,
            query,
            previous_query,
            matches,
            HistoryCandidates {
                candidates,
                scratch,
                candidate_stack,
                spare_candidates,
            },
            selected,
            shell,
        )
    };
    match action {
        HistoryAction::Continue => {
            view.region = render_history_mode(
                &mut view.tw,
                search,
                shell,
                view.region,
                &mut view.history_cache,
            );
            let _ = view.tw.flush_to_stdout();
            Next::Stay
        }
        HistoryAction::Accept(text) => {
            view.line.set(&text);
            leave_history_search(view, shell, search);
            Next::Normal
        }
        HistoryAction::Cancel => {
            view.line.set(&search.saved_line);
            leave_history_search(view, shell, search);
            Next::Normal
        }
    }
}

fn leave_history_search(view: &mut EditView, shell: &mut Shell, search: &mut HistorySearch) {
    shell.history_match_buffer = std::mem::take(&mut search.matches);
    view.history_cache.clear();
    view.region.clear_from_cursor(&mut view.tw);
    redraw_prompt_line(view, shell);
}

fn handle_dir_picker_key_event(
    view: &mut EditView,
    shell: &mut Shell,
    key: KeyEvent,
    picker: &mut DirPicker,
) -> Next {
    match (key.key, key.mods.ctrl) {
        (Key::Escape, _) | (Key::Char('c'), true) => {
            view.region.clear(&mut view.tw);
            redraw_prompt_line(view, shell);
            Next::Normal
        }
        (Key::Enter, _) => {
            if let Some(dir) = picker.entries.get(picker.selected) {
                let dir = dir.clone();
                eprintln!("{dir}");
                change_directory(&dir, shell);
            }
            view.region.clear(&mut view.tw);
            redraw_prompt_line(view, shell);
            Next::Normal
        }
        (Key::Up, _) if picker.selected > 0 => {
            picker.selected -= 1;
            view.region = render_dir_picker_mode(&mut view.tw, picker, shell, view.region);
            let _ = view.tw.flush_to_stdout();
            Next::Stay
        }
        (Key::Down, _) if picker.selected + 1 < picker.entries.len() => {
            picker.selected += 1;
            view.region = render_dir_picker_mode(&mut view.tw, picker, shell, view.region);
            let _ = view.tw.flush_to_stdout();
            Next::Stay
        }
        _ => Next::Stay,
    }
}

fn active_prompt(prompt_str: &str, prompt_display_len: usize) -> (&str, usize) {
    (prompt_str, prompt_display_len)
}

fn render_prompt_region(
    tw: &mut TermWriter,
    shell: &mut Shell,
    line: &LineBuffer,
    prompt_str: &str,
    prompt_display_len: usize,
    region: render::RenderedRegion,
) -> render::RenderedRegion {
    let (p, pdl) = active_prompt(prompt_str, prompt_display_len);

    let cmd_color = if !line.has_newlines() {
        let first = line.text().split_whitespace().next().unwrap_or("");
        if first.is_empty() {
            None
        } else if builtin::is_builtin(first)
            || shell.aliases.get(first).is_some()
            || first.contains('/')
        {
            Some(true)
        } else {
            Some(
                shell
                    .path_cache
                    .contains(first, store_path_env(&shell.epsh)),
            )
        }
    } else {
        None
    };

    let text = line.text();
    let suggestion = if text.len() >= 3 && !line.has_newlines() && line.cursor() == text.len() {
        shell
            .history
            .session_prefix_search(text, 0)
            .and_then(|entry| entry.strip_prefix(text))
            .unwrap_or("")
    } else {
        ""
    };

    let opts = render::RenderOpts {
        cmd_color,
        suggestion,
    };
    render::render_line(tw, p, pdl, line, shell.cols, region, &opts)
}

#[allow(clippy::too_many_arguments)]
fn render_active_mode(
    tw: &mut TermWriter,
    mode: &Mode,
    shell: &mut Shell,
    line: &LineBuffer,
    prompt_str: &str,
    prompt_display_len: usize,
    region: render::RenderedRegion,
    history_cache: &mut render::HistoryPagerCache,
) -> render::RenderedRegion {
    match mode {
        Mode::Normal => {
            render_prompt_region(tw, shell, line, prompt_str, prompt_display_len, region)
        }
        Mode::Completion(CompletionMode { state, .. }) => {
            let (p, pdl) = active_prompt(prompt_str, prompt_display_len);
            let info = render::render_line(
                tw,
                p,
                pdl,
                line,
                shell.cols,
                region,
                &render::RenderOpts::default(),
            );
            render::render_completions(tw, state, info, true);
            info
        }
        Mode::HistorySearch(search) => render_history_mode(tw, search, shell, region, history_cache),
        Mode::DirPicker(picker) => render_dir_picker_mode(tw, picker, shell, region),
    }
}

fn update_mode_layout_for_resize(mode: &mut Mode, shell: &Shell) {
    if let Mode::Completion(CompletionMode { state, .. }) = mode {
        let (cols, rows) = complete::compute_grid(&state.comp.entries, shell.cols);
        state.cols = cols;
        state.rows = rows;
        state.scroll = state.scroll.min(rows.saturating_sub(1));
    }
}

enum KeyAction {
    Continue,
    Execute(String),
    Continuation,
    Cancel,
    Exit,
    ClearScreen,
    StartHistorySearch,
    StartDirPicker,
    StartCompletion,
}

fn expand_aliases_for_history(line: &str, aliases: &AliasMap) -> String {
    expand_aliases_for_line(line, aliases).into_owned()
}

fn expand_aliases_for_line<'a>(line: &'a str, aliases: &AliasMap) -> std::borrow::Cow<'a, str> {
    let trimmed = line.trim_start();
    let leading = &line[..line.len() - trimmed.len()];
    if let Some(after_bang) = trimmed.strip_prefix('!')
        && after_bang.chars().next().is_some_and(char::is_whitespace)
    {
        return match aliases.expand_line(&trimmed[1..]) {
            std::borrow::Cow::Borrowed(_) => std::borrow::Cow::Borrowed(line),
            std::borrow::Cow::Owned(expanded) => {
                std::borrow::Cow::Owned(format!("{leading}!{expanded}"))
            }
        };
    }
    aliases.expand_line(line)
}

fn handle_normal_key(
    key: KeyEvent,
    line: &mut LineBuffer,
    history_idx: &mut Option<usize>,
    saved_line: &mut String,
    grid_column: &mut Option<usize>,
    prompt_display_len: usize,
    shell: &Shell,
) -> KeyAction {
    if !matches!(key.key, Key::Up | Key::Down) || key.mods.ctrl {
        *grid_column = None;
    }

    match (key.key, key.mods.ctrl, key.mods.alt) {
        (Key::Char('c'), true, _) => return KeyAction::Cancel,
        (Key::Char('d'), true, _) => {
            if line.is_empty() {
                return KeyAction::Exit;
            }
            line.delete_forward();
        }
        (Key::Char('a'), true, _) => line.move_home(),
        (Key::Char('e'), true, _) => line.move_end(),
        (Key::Char('k'), true, _) => line.kill_to_end(),
        (Key::Char('u'), true, _) => line.kill_to_start(),
        (Key::Char('w'), true, _) => line.kill_word_back(),
        (Key::Char('d'), false, true) => line.kill_word_forward(),
        (Key::Char('y'), true, _) => line.yank(),
        (Key::Char('l'), true, _) => return KeyAction::ClearScreen,
        (Key::Char('r'), true, _) => return KeyAction::StartHistorySearch,

        (Key::Char('b'), false, true) => line.move_word_left(),
        (Key::Char('f'), false, true) => line.move_word_right(),

        (Key::Up, _, _) if !key.mods.ctrl => {
            if line.has_newlines() && !line.on_first_line() {
                line.move_line_up();
                *grid_column = None;
            } else if !line.has_newlines() {
                let col = *grid_column.get_or_insert_with(|| {
                    line.grid_cursor_position(prompt_display_len, shell.cols).1
                });
                if !line.move_grid_up(prompt_display_len, shell.cols, col) {
                    *grid_column = None;
                    navigate_history(line, history_idx, saved_line, shell, true);
                }
            } else {
                navigate_history(line, history_idx, saved_line, shell, true);
            }
        }
        (Key::Down, _, _) if !key.mods.ctrl => {
            if line.has_newlines() && !line.on_last_line() {
                line.move_line_down();
                *grid_column = None;
            } else if !line.has_newlines() {
                let col = *grid_column.get_or_insert_with(|| {
                    line.grid_cursor_position(prompt_display_len, shell.cols).1
                });
                if !line.move_grid_down(prompt_display_len, shell.cols, col) {
                    *grid_column = None;
                    navigate_history(line, history_idx, saved_line, shell, false);
                }
            } else {
                navigate_history(line, history_idx, saved_line, shell, false);
            }
        }
        (Key::Left, _, _) if key.mods.ctrl || key.mods.alt => line.move_word_left(),
        (Key::Right, _, _) if key.mods.ctrl || key.mods.alt => line.move_word_right(),
        (Key::Left, _, _) => {
            line.move_left();
        }
        (Key::Right, _, _) => {
            if line.cursor() >= line.text().len() && !line.has_newlines() {
                // At end of line — accept autosuggestion from history
                if let Some(entry) = shell.history.session_prefix_search(line.text(), 0) {
                    let owned = entry.to_string();
                    line.set(&owned);
                }
            } else {
                line.move_right();
            }
        }
        (Key::Home, _, _) => line.move_home(),
        (Key::End, _, _) => line.move_end(),

        (Key::Backspace, true, _) => return KeyAction::StartDirPicker,
        (Key::Backspace, _, _) => {
            line.delete_back();
            *history_idx = None;
        }
        (Key::Delete, true, _) => {
            line.kill_word_back();
            *history_idx = None;
        }
        (Key::Delete, _, _) => {
            line.delete_forward();
        }
        (Key::Tab, _, _) => {
            if line.is_empty() {
                line.insert_str("cd ");
            }
            return KeyAction::StartCompletion;
        }
        (Key::Enter, _, _) => {
            if needs_continuation(line.text()) {
                return KeyAction::Continuation;
            }
            return KeyAction::Execute(line.text().to_string());
        }

        (Key::Char(c), false, false) => {
            line.insert_char(c);
            if c == ' ' {
                try_alias_expand(line, &shell.aliases);
            }
            *history_idx = None;
        }

        _ => {}
    }

    KeyAction::Continue
}

fn navigate_history(
    line: &mut LineBuffer,
    history_idx: &mut Option<usize>,
    saved_line: &mut String,
    shell: &Shell,
    up: bool,
) {
    let hist = &shell.history;
    if hist.is_empty() {
        return;
    }

    if history_idx.is_none() {
        *saved_line = line.text().to_string();
    }

    if up {
        let skip = history_idx.map(|i| i + 1).unwrap_or(0);
        let entry = if saved_line.is_empty() {
            hist.session_get(skip)
        } else {
            hist.session_prefix_search(saved_line, skip)
        };
        if let Some(e) = entry {
            *history_idx = Some(skip);
            line.set(e);
        }
    } else {
        match history_idx {
            Some(0) | None => {
                *history_idx = None;
                line.set(saved_line);
            }
            Some(idx) => {
                *idx -= 1;
                let skip = *idx;
                let entry = if saved_line.is_empty() {
                    hist.session_get(skip)
                } else {
                    hist.session_prefix_search(saved_line, skip)
                };
                if let Some(e) = entry {
                    line.set(e);
                }
            }
        }
    }
}

fn try_alias_expand(line: &mut LineBuffer, aliases: &AliasMap) {
    let text = line.text();
    let trimmed = text.trim_end();
    let mut words = trimmed.split_whitespace();
    let first = words.next();
    let command_name = if first == Some("!") {
        words.next()
    } else {
        first
    };
    if command_name.is_none() || words.next().is_some() {
        return;
    }

    let expanded = expand_aliases_for_line(text, aliases);
    if let std::borrow::Cow::Owned(new_text) = expanded {
        line.set(&new_text);
    }
}

// -- Completion --

enum CompletionAction {
    Navigate,
    Accept,
    Cancel,
    Refilter,
}

/// Returns the byte offset of the start of the current shell command segment
/// (the text after the last unquoted |, ;, or && in `s`).
fn current_cmd_start(s: &str) -> usize {
    let mut in_single = false;
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                in_single = !in_single;
                i += 1;
            }
            b'|' | b';' if !in_single => {
                i += 1;
                start = i;
            }
            b'&' if !in_single && i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                i += 2;
                start = i;
            }
            _ => i += 1,
        }
    }
    start
}

/// Split a shell command segment into tokens, stripping single-quote delimiters.
fn shell_tokens(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    for c in s.chars() {
        match c {
            '\'' => in_single = !in_single,
            ' ' | '\t' if !in_single => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Collect the arguments already present in the current command before `word_start`,
/// skipping the command name itself. Used to filter duplicates from completions.
fn existing_cmd_args(before_cursor: &str, word_start: usize) -> Vec<String> {
    let prefix = &before_cursor[..word_start];
    let cmd_start = current_cmd_start(prefix);
    let tokens = shell_tokens(&prefix[cmd_start..]);
    if tokens.len() > 1 {
        tokens[1..].to_vec()
    } else {
        Vec::new()
    }
}

/// Remove completion entries whose full form (dir_prefix + name) is already
/// present in `existing`. Leaves the names arena intact; orphaned bytes are harmless.
fn filter_existing_args(comp: &mut complete::Completions, existing: &[String], dir_prefix: &str) {
    if existing.is_empty() {
        return;
    }
    let to_remove: Vec<bool> = comp
        .entries
        .iter()
        .map(|e| {
            let full = format!("{}{}", dir_prefix, comp.entry_name(e));
            // Directories are inserted with a trailing '/', so check both forms.
            existing
                .iter()
                .any(|a| *a == full || (e.is_dir() && a.strip_suffix('/') == Some(full.as_str())))
        })
        .collect();
    let mut idx = 0;
    comp.entries.retain(|_| {
        let remove = to_remove[idx];
        idx += 1;
        !remove
    });
}

/// Whether the cursor is at a command position (first word, after | && ;,
/// or immediately after sudo/doas/su).
fn is_command_position(before_cursor: &str, word_start: usize) -> bool {
    if word_start == 0 {
        return true;
    }
    let before_word = before_cursor[..word_start].trim_end();
    if before_word.ends_with('|') || before_word.ends_with(';') || before_word.ends_with("&&") {
        return true;
    }
    // The argument after sudo/doas/su is also a command
    let prev_word = before_word
        .rsplit_once(|c: char| c.is_whitespace() || c == '|' || c == ';')
        .map(|(_, w)| w)
        .unwrap_or(before_word);
    matches!(prev_word, "sudo" | "doas" | "su")
}

/// Find the start of the completion word, respecting quotes.
/// Returns (byte_offset_of_word_start, currently_inside_single_quote).
fn find_comp_word_start(s: &str) -> (usize, bool) {
    let mut in_single = false;
    let mut in_double = false;
    let mut word_start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => {
                in_single = !in_single;
                i += 1;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                i += 1;
            }
            b'\\' if !in_single && i + 1 < bytes.len() => {
                i += 2;
            }
            b' ' | b'\t' | b'\n' if !in_single && !in_double => {
                i += 1;
                word_start = i;
            }
            _ => {
                i += 1;
            }
        }
    }
    (word_start, in_single)
}

fn start_completion(
    line: &LineBuffer,
    term_cols: u16,
    home: Option<&str>,
    aliases: &AliasMap,
    epsh: &epsh::eval::Shell,
    mut comp: complete::Completions,
) -> CompletionState {
    comp.clear();

    let text = line.text();
    let before_cursor = &text[..line.cursor()];
    let (word_start, in_single) = find_comp_word_start(before_cursor);
    let existing = existing_cmd_args(before_cursor, word_start);
    let raw_word = &before_cursor[word_start..];

    // Strip quotes for filesystem lookup; track whether the word was quoted.
    // Handles: open quote ('path), balanced quote ('path'), and tilde (~/'path').
    let (partial, in_quote): (String, bool) = if in_single {
        (raw_word.strip_prefix('\'').unwrap_or(raw_word).into(), true)
    } else if let Some(rest) = raw_word.strip_prefix("~/'") {
        let unquoted = rest.strip_suffix('\'').unwrap_or(rest);
        (format!("~/{unquoted}"), true)
    } else if let Some(inner) = raw_word.strip_prefix('\'') {
        (inner.strip_suffix('\'').unwrap_or(inner).into(), true)
    } else {
        (raw_word.into(), false)
    };

    let first_command_word = text.split_whitespace().next().unwrap_or("");
    const SSH_CMDS: &[&str] = &["ssh", "scp", "rsync", "sftp", "mosh"];
    let needs_home = partial.starts_with('~')
        || (word_start > 0 && SSH_CMDS.contains(&first_command_word) && !partial.contains([':', '/']));
    let home = match home {
        Some(home) => home,
        None if needs_home => {
            eprintln!("ish: completion: HOME is not UTF-8");
            return CompletionState {
                comp,
                selected: 0,
                cols: 1,
                rows: 0,
                scroll: 0,
                term_cols,
                dir_prefix: String::new(),
                in_quote,
            };
        }
        None => "",
    };

    // Command position: first word or after |, &&, ;
    if !partial.is_empty()
        && !partial.contains('/')
        && is_command_position(before_cursor, word_start)
    {
        for b in builtin::all_builtin_names() {
            if b.starts_with(partial.as_str()) {
                comp.push(b, false, false, false);
            }
        }
        for (name, _) in aliases.iter() {
            if name.starts_with(partial.as_str()) {
                comp.push(name, false, false, false);
            }
        }
        path::complete_commands(&partial, &mut comp, store_path_env(epsh));
        // Directories are valid commands (implicit cd)
        complete::complete_path_into(&partial, true, &mut comp);
        comp.sort_entries();
        comp.dedup_sorted();

        let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
        return CompletionState {
            comp,
            selected: 0,
            cols,
            rows,
            scroll: 0,
            term_cols,
            dir_prefix: String::new(),
            in_quote: false,
        };
    }

    // Detect if first word is cd → complete only directories
    let dirs_only = first_command_word == "cd" && word_start > 0;

    // After ssh-like commands, offer hostnames beside local files. There is no
    // remote path completion: it would run ssh from the completion path.
    if word_start > 0 && SSH_CMDS.contains(&first_command_word) && !partial.contains([':', '/']) {
        complete::complete_hostnames(&partial, home, &mut comp);
        complete::complete_path_into(&partial, false, &mut comp);
        comp.sort_entries();
        comp.dedup_sorted();
        filter_existing_args(&mut comp, &existing, "");
        if !comp.is_empty() {
            let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
            return CompletionState {
                comp,
                selected: 0,
                cols,
                rows,
                scroll: 0,
                term_cols,
                dir_prefix: String::new(),
                in_quote,
            };
        }
    }

    // Expand tilde for filesystem lookup
    let expanded = if partial == "~" {
        format!("{home}/")
    } else if let Some(rest) = partial.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        partial.clone()
    };

    // dir_prefix keeps the original (unexpanded) form for accept_completion
    let dir_prefix = if let Some(slash_pos) = partial.rfind('/') {
        partial[..=slash_pos].to_string()
    } else {
        String::new()
    };

    complete::complete_path_into(&expanded, dirs_only, &mut comp);
    filter_existing_args(&mut comp, &existing, &dir_prefix);
    if !comp.is_empty() {
        let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
        return CompletionState {
            comp,
            selected: 0,
            cols,
            rows,
            scroll: 0,
            term_cols,
            dir_prefix,
            in_quote,
        };
    }

    // Fish-style partial path completion: each intermediate component is a prefix.
    // e.g., "~/de/s" → resolve "de" to "dev", complete "s" in ~/dev/
    let (partial_comp, groups) = complete::complete_partial_path(&expanded, dirs_only);
    if !groups.is_empty() {
        // Determine the user-facing root prefix (for tilde re-contraction)
        let (user_root, expanded_root) = if partial.starts_with("~/") || partial == "~" {
            ("~/".to_string(), format!("{home}/"))
        } else if partial.starts_with('/') {
            ("/".to_string(), "/".to_string())
        } else {
            (String::new(), String::new())
        };

        // Build entries with resolved intermediate components in the name.
        // comp is already empty — reuse its arena for combined "rel_dir/name" strings.
        for (resolved_dir, start, count) in &groups {
            let rel_dir = if expanded_root.is_empty() {
                resolved_dir.as_str()
            } else {
                resolved_dir
                    .strip_prefix(&expanded_root)
                    .unwrap_or(resolved_dir)
            };
            for i in *start..*start + *count {
                let entry = &partial_comp.entries[i];
                let orig_name = partial_comp.entry_name(entry);
                let mark = comp.begin_entry();
                comp.names.push_str(rel_dir);
                comp.names.push_str(orig_name);
                comp.finish_entry(mark, entry.is_dir(), entry.is_link(), entry.is_exec());
            }
        }

        filter_existing_args(&mut comp, &existing, &user_root);
        let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
        return CompletionState {
            comp,
            selected: 0,
            cols,
            rows,
            scroll: 0,
            term_cols,
            dir_prefix: user_root,
            in_quote,
        };
    }

    // Nothing found — return empty state, preserving the buffer for reuse.
    CompletionState {
        comp,
        selected: 0,
        cols: 0,
        rows: 0,
        scroll: 0,
        term_cols,
        dir_prefix: String::new(),
        in_quote,
    }
}

fn handle_completion_key(
    key: KeyEvent,
    state: &mut CompletionState,
    line: &mut LineBuffer,
) -> CompletionAction {
    match key.key {
        Key::Up => {
            state.move_up();
            CompletionAction::Navigate
        }
        Key::Down | Key::Tab => {
            state.move_down();
            CompletionAction::Navigate
        }
        Key::Left => {
            state.move_left();
            CompletionAction::Navigate
        }
        Key::Right => {
            state.move_right();
            CompletionAction::Navigate
        }
        Key::Enter => CompletionAction::Accept,
        Key::Escape => CompletionAction::Cancel,
        Key::Char('c') if key.mods.ctrl => CompletionAction::Cancel,
        Key::Char(c) if !key.mods.ctrl && !key.mods.alt => {
            line.insert_char(c);
            CompletionAction::Refilter
        }
        Key::Backspace => {
            if line.cursor() > 0 {
                line.delete_back();
                CompletionAction::Refilter
            } else {
                CompletionAction::Cancel
            }
        }
        _ => CompletionAction::Cancel,
    }
}

fn preview_completion(line: &mut LineBuffer, state: &CompletionState, base_line: &LineBuffer) {
    let entry = match state.selected_entry() {
        Some(e) => e,
        None => {
            *line = base_line.clone();
            return;
        }
    };
    let name = match state.selected_name() {
        Some(n) => n,
        None => {
            *line = base_line.clone();
            return;
        }
    };

    let text = base_line.text().to_string();
    let before_cursor = &text[..base_line.cursor()];
    let (word_start, _) = find_comp_word_start(before_cursor);
    let after_cursor = &text[base_line.cursor()..];

    let mut inner = state.dir_prefix.clone();
    inner.push_str(name);
    if entry.is_dir() {
        inner.push('/');
    } else if entry.is_host() {
        inner.push(':');
    }

    // Single-quote the completion if it contains special characters.
    // Always close the quote so the line is valid for immediate execution.
    // On the next tab press, start_completion strips the outer quotes for lookup.
    // Keep ~/ outside the quotes so tilde expansion still works.
    // Escape embedded single quotes: ' → '\'' (end quote, escaped quote, reopen).
    let replacement = if state.in_quote || needs_quoting(&inner) || inner.contains('\'') {
        let escaped = inner.replace('\'', "'\\''");
        if let Some(rest) = escaped.strip_prefix("~/") {
            format!("~/'{rest}'")
        } else {
            format!("'{escaped}'")
        }
    } else {
        inner
    };

    let new_text = format!("{}{}{}", &text[..word_start], replacement, after_cursor);
    let new_cursor = word_start + replacement.len();
    line.set_with_cursor(&new_text, new_cursor);
}

// -- History Search --

enum HistoryAction {
    Continue,
    Accept(String),
    Cancel,
}

/// The incremental-narrowing state of a history search, borrowed together so
/// the key handler stays within a reasonable argument count.
struct HistoryCandidates<'a> {
    candidates: &'a mut Vec<usize>,
    scratch: &'a mut Vec<usize>,
    candidate_stack: &'a mut Vec<(usize, Vec<usize>)>,
    spare_candidates: &'a mut Vec<Vec<usize>>,
}

fn recycle_candidate_stack(
    candidate_stack: &mut Vec<(usize, Vec<usize>)>,
    spare_candidates: &mut Vec<Vec<usize>>,
) {
    spare_candidates.extend(candidate_stack.drain(..).map(|(_, candidates)| candidates));
}

fn handle_history_search_key(
    key: KeyEvent,
    query: &mut LineBuffer,
    previous_query: &mut String,
    matches: &mut Vec<FuzzyMatch>,
    HistoryCandidates {
        candidates,
        scratch,
        candidate_stack,
        spare_candidates,
    }: HistoryCandidates<'_>,
    selected: &mut usize,
    shell: &Shell,
) -> HistoryAction {
    let mut re_search = false;
    previous_query.clear();
    previous_query.push_str(query.text());
    let prev_cursor = query.cursor();
    match (key.key, key.mods.ctrl, key.mods.alt) {
        (Key::Escape, _, _) => return HistoryAction::Cancel,
        (Key::Char('c'), true, _) => return HistoryAction::Cancel,
        (Key::Enter, _, _) => {
            return if let Some(m) = matches.get(*selected) {
                HistoryAction::Accept(shell.history.get(m.entry_idx).to_string())
            } else {
                HistoryAction::Cancel
            };
        }
        (Key::Up, _, _) | (Key::Char('p'), true, _) if *selected > 0 => {
            *selected -= 1;
        }
        (Key::Down, _, _) | (Key::Char('n'), true, _) if *selected + 1 < matches.len() => {
            *selected += 1;
        }

        // Query editing — cursor movement
        (Key::Left, _, false) if key.mods.ctrl => {
            query.move_word_left();
        }
        (Key::Right, _, false) if key.mods.ctrl => {
            query.move_word_right();
        }
        (Key::Left, _, _) => {
            query.move_left();
        }
        (Key::Right, _, _) => {
            query.move_right();
        }
        (Key::Home, _, _) | (Key::Char('a'), true, _) => query.move_home(),
        (Key::End, _, _) | (Key::Char('e'), true, _) => query.move_end(),
        (Key::Char('b'), _, true) => query.move_word_left(),
        (Key::Char('f'), _, true) => query.move_word_right(),

        // Query editing — text modification
        (Key::Backspace, _, _) => {
            query.delete_back();
            re_search = true;
        }
        (Key::Delete, true, _) | (Key::Char('d'), false, true) => {
            query.kill_word_back();
            re_search = true;
        }
        (Key::Delete, _, _) | (Key::Char('d'), true, _) => {
            query.delete_forward();
            re_search = true;
        }
        (Key::Char('u'), true, _) => {
            query.kill_to_start();
            re_search = true;
        }
        (Key::Char('k'), true, _) => {
            query.kill_to_end();
            re_search = true;
        }
        (Key::Char('w'), true, _) => {
            query.kill_word_back();
            re_search = true;
        }
        (Key::Char('y'), true, _) => {
            query.yank();
            re_search = true;
        }
        (Key::Char(c), false, false) => {
            query.insert_char(c);
            re_search = true;
        }

        _ => {}
    }
    if re_search {
        update_history_search(
            query,
            (previous_query, prev_cursor),
            matches,
            HistoryCandidates {
                candidates,
                scratch,
                candidate_stack,
                spare_candidates,
            },
            selected,
            shell,
        );
    }
    HistoryAction::Continue
}

fn update_history_search(
    query: &LineBuffer,
    (prev_text, prev_cursor): (&str, usize),
    matches: &mut Vec<FuzzyMatch>,
    HistoryCandidates {
        candidates,
        scratch,
        candidate_stack,
        spare_candidates,
    }: HistoryCandidates<'_>,
    selected: &mut usize,
    shell: &Shell,
) {
    let new_text = query.text();
    let append_at_end = prev_cursor == prev_text.len()
        && query.cursor() == new_text.len()
        && new_text.len() > prev_text.len()
        && new_text.starts_with(prev_text);
    let delete_at_end = prev_cursor == prev_text.len()
        && query.cursor() == new_text.len()
        && new_text.len() < prev_text.len()
        && prev_text.starts_with(new_text);
    if append_at_end {
        let narrowed = spare_candidates.pop().unwrap_or_default();
        let wider = std::mem::replace(candidates, narrowed);
        candidate_stack.push((prev_text.len(), wider));
        shell.history.fuzzy_search_subset_into_in_dir(
            new_text,
            &candidate_stack.last().unwrap().1,
            candidates,
            matches,
            200,
            shell.epsh.cwd(),
        );
    } else if delete_at_end {
        while candidate_stack
            .last()
            .is_some_and(|(len, _)| *len > new_text.len())
        {
            if let Some((_, old_candidates)) = candidate_stack.pop() {
                spare_candidates.push(old_candidates);
            }
        }
        if let Some((len, _)) = candidate_stack.last()
            && *len == new_text.len()
        {
            let (_, old_candidates) = candidate_stack.pop().unwrap();
            spare_candidates.push(std::mem::replace(candidates, old_candidates));
            shell.history.fuzzy_search_subset_into_in_dir(
                new_text,
                candidates,
                scratch,
                matches,
                200,
                shell.epsh.cwd(),
            );
        } else {
            recycle_candidate_stack(candidate_stack, spare_candidates);
            shell.history.search_entry_indices_into(scratch);
            shell.history.fuzzy_search_subset_into_in_dir(
                new_text,
                scratch,
                candidates,
                matches,
                200,
                shell.epsh.cwd(),
            );
            std::mem::swap(candidates, scratch);
        }
    } else {
        recycle_candidate_stack(candidate_stack, spare_candidates);
        shell.history.search_entry_indices_into(scratch);
        shell.history.fuzzy_search_subset_into_in_dir(
            new_text,
            scratch,
            candidates,
            matches,
            200,
            shell.epsh.cwd(),
        );
        std::mem::swap(candidates, scratch);
    }
    *selected = 0;
}

fn render_history_mode(
    tw: &mut TermWriter,
    search: &HistorySearch,
    shell: &Shell,
    prev: render::RenderedRegion,
    cache: &mut render::HistoryPagerCache,
) -> render::RenderedRegion {
    render::render_history_pager_cached(
        tw,
        search.query.text(),
        &search.matches,
        &shell.history,
        search.selected,
        shell.rows,
        shell.cols,
        search.query.display_cursor_pos(),
        prev,
        cache,
    )
}

/// Apply denv environment changes to epsh's variable store.
/// The changes are already applied to process env by denv.
fn apply_denv_changes(changes: &[denv::EnvChange], epsh: &mut epsh::eval::Shell) {
    for change in changes {
        match change {
            denv::EnvChange::Set(k, v) => {
                let _ = epsh.vars_mut().set(k, v);
                epsh.vars_mut().export(k);
            }
            denv::EnvChange::Unset(k) => {
                let _ = epsh.vars_mut().unset(k);
            }
        }
    }
}

fn sync_path_var(epsh: &mut epsh::eval::Shell, name: &str, path: &Path) {
    ish::shell_setenv_os(name, path.as_os_str());
    let _ = epsh.vars_mut().set_bytes(
        name,
        epsh::shell_bytes::ShellBytes::from_os_str(path.as_os_str()),
    );
    // PWD/OLDPWD must be exported so they survive denv's process-env mirror
    // sync and reach children (matches bash, which exports both).
    epsh.vars_mut().export(name);
}

fn sync_cwd_change(shell: &mut Shell, old_cwd: &Path, new_cwd: std::path::PathBuf) {
    sync_path_var(&mut shell.epsh, "OLDPWD", old_cwd);
    sync_path_var(&mut shell.epsh, "PWD", &new_cwd);
    shell.epsh.set_cwd(new_cwd);
    push_dir_stack(&mut shell.dir_stack);
    shell.prompt.invalidate_git();
    let changes = denv::on_cd(&shell.denv, &shell.epsh);
    apply_denv_changes(&changes, &mut shell.epsh);
    sync_shell_store_derived(shell);
}

/// Expand intercepted builtin arguments with epsh so top-level builtins honor
/// quoting, variables, command substitution, and globbing like normal commands.
fn expand_builtin_args(
    words: &[epsh::ast::Word],
    shell: &mut Shell,
    name: &str,
) -> Option<Vec<String>> {
    let fields = expand_builtin_args_bytes(words, shell, name)?;
    let mut args = Vec::with_capacity(fields.len());
    for field in fields {
        match String::from_utf8(field.into_vec()) {
            Ok(field) => args.push(field),
            Err(error) => {
                eprintln!("ish: {name}: argument is not UTF-8: {error}");
                shell.last_status = 1;
                return None;
            }
        }
    }
    Some(args)
}

fn expand_builtin_args_bytes(
    words: &[epsh::ast::Word],
    shell: &mut Shell,
    name: &str,
) -> Option<Vec<epsh::shell_bytes::ShellBytes>> {
    let mut args = Vec::new();
    for word in words {
        match epsh::expand::expand_word_to_fields(word, &mut shell.epsh) {
            Ok(fields) => args.extend(fields.iter().map(|field| epsh::shell_bytes::ShellBytes::from_str_lossless(field))),
            Err(e) => {
                eprintln!("ish: {name}: expansion error: {e}");
                shell.last_status = 1;
                return None;
            }
        }
    }
    Some(args)
}

/// Change directory and run all post-cd hooks (OLDPWD, epsh sync, dir stack, denv, prompt).
/// Returns 0 on success, 1 on failure.
fn change_directory(target: &str, shell: &mut Shell) -> i32 {
    let home = store_env_os(&shell.epsh, "HOME").unwrap_or_default();
    let resolved = if target == "~" || target.is_empty() {
        std::path::PathBuf::from(home)
    } else if let Some(rest) = target.strip_prefix("~/") {
        Path::new(&home).join(rest)
    } else {
        std::path::PathBuf::from(target)
    };
    change_directory_path(&resolved, shell)
}

fn change_directory_path(target: &Path, shell: &mut Shell) -> i32 {
    if let Err(e) = std::env::set_current_dir(target) {
        eprintln!("ish: cd: {}: {e}", target.display());
        return 1;
    }

    let old_cwd = shell.epsh.cwd().to_path_buf();
    let new_cwd = std::env::current_dir().unwrap_or_default();
    sync_cwd_change(shell, &old_cwd, new_cwd);
    0
}

fn push_dir_stack(dir_stack: &mut Vec<String>) {
    if let Ok(pwd) = std::env::current_dir() {
        let pwd = pwd.to_string_lossy().into_owned();
        // Don't push duplicates at the top
        if dir_stack.last().map(|s| s.as_str()) != Some(&pwd) {
            dir_stack.push(pwd);
            // Cap at 50 entries
            if dir_stack.len() > 50 {
                dir_stack.remove(0);
            }
        }
    }
}

fn render_dir_picker_mode(
    tw: &mut TermWriter,
    picker: &DirPicker,
    shell: &Shell,
    prev: render::RenderedRegion,
) -> render::RenderedRegion {
    render::render_dir_picker(
        tw,
        &picker.entries,
        picker.selected,
        &shell.home_display,
        shell.rows,
        shell.cols,
        prev,
    )
}

/// Handle the "exit" command typed at the prompt. Returns true if the shell should break.
fn handle_exit_command(line: &str, shell: &mut Shell) -> bool {
    let code: i32 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if shell.job.is_some() {
        if shell.exit_warned {
            if let Some(job) = shell.job.take() {
                job::abandon(job);
            }
            shell.last_status = code;
            return true;
        } else {
            eprintln!("ish: there is a suspended job. Exit again to force quit.");
            shell.exit_warned = true;
            shell.last_status = 1;
            return false;
        }
    }
    denv::on_exit(&shell.denv);
    std::process::exit(code);
}

/// Handle the "alias" command typed at the prompt.
fn handle_alias(line: &str, shell: &mut Shell) {
    let words = parse_alias_words(line);
    if words.len() >= 2 {
        let name = words[0].clone();
        let expansion = words[1..].to_vec();
        shell.aliases.set(name, expansion);
        shell.last_status = 0;
    } else if words.len() == 1 {
        if let Some(exp) = shell.aliases.get(&words[0]) {
            println!("alias {} {}", words[0], exp.join(" "));
            shell.last_status = 0;
        } else {
            eprintln!("ish: alias: not found: {}", words[0]);
            shell.last_status = 1;
        }
    } else {
        for (name, exp) in shell.aliases.iter() {
            println!("alias {name} {}", exp.join(" "));
        }
        shell.last_status = 0;
    }
}

fn parse_alias_words(line: &str) -> Vec<String> {
    let rest = line
        .trim_start()
        .strip_prefix("alias")
        .unwrap_or("")
        .trim_start();
    ish::alias::lex_words(rest)
}

/// Handle the "history" command. Returns true if handled (no fallthrough needed).
fn handle_history(line: &str, shell: &mut Shell) -> bool {
    let sub = line.split_whitespace().nth(1);
    match sub {
        None => {
            let mut indices = Vec::new();
            shell.history.command_indices_into(&mut indices);
            for i in indices {
                println!("{}", shell.history.display(i));
            }
            shell.last_status = 0;
            true
        }
        Some("-h") | Some("--help") => {
            println!("Usage: history [reset]");
            println!();
            println!("Show history or reset it.");
            shell.last_status = 0;
            true
        }
        Some("reset") => {
            match shell.history.reset() {
                Ok(()) => shell.last_status = 0,
                Err(error) => {
                    eprintln!("ish: history reset: {error}");
                    shell.last_status = 1;
                }
            }
            true
        }
        Some(other) => {
            eprintln!("ish: history: unknown subcommand: {other}");
            shell.last_status = 1;
            true
        }
    }
}

/// Check if input needs a continuation line (open quotes, trailing operator, etc.)
fn needs_continuation(input: &str) -> bool {
    let trimmed = input.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    let (in_single, in_double, escape) = scan_quote_state(input.as_bytes());
    if in_single || in_double || escape {
        return true;
    }
    trimmed.ends_with('|') || trimmed.ends_with("&&") || trimmed.ends_with("||")
}

fn scan_quote_state(input: &[u8]) -> (bool, bool, bool) {
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    for &b in input {
        if escape {
            escape = false;
            continue;
        }
        match b {
            b'\\' if !in_single => escape = true,
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            _ => {}
        }
    }
    (in_single, in_double, escape)
}

/// Handle Ctrl+D exit (from read_line).
fn handle_exit(shell: &mut Shell) -> ReadResult {
    if shell.job.is_some() {
        if shell.exit_warned {
            if let Some(job) = shell.job.take() {
                job::abandon(job);
            }
            ReadResult::Exit
        } else {
            eprintln!("ish: there is a suspended job. Press Ctrl+D again to force quit.");
            shell.exit_warned = true;
            ReadResult::Empty
        }
    } else {
        ReadResult::Exit
    }
}
