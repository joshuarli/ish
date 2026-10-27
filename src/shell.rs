use ish::alias::AliasMap;
use ish::cd_rewrite::{maybe_rewrite_cd, resolve_cd_for_history};
use ish::diagnostics::{self, base64_encode};
use ish::input::{InputEvent, InputReader, KeyEvent};
use ish::job::{self, Job};
use ish::external;
use ish::{builtin, capture, config, denv, dispatch, frecency, history, prompt, signal, term};
use std::ffi::OsString;
use std::path::Path;
use ish::builtin::ShellCommand;

use ish::editor::{Editor, EditorAction, EditorContext, EditorEvent};
use crate::trash;
struct Shell {
    editor: Editor,
    aliases: AliasMap,
    last_status: i32,
    dir_stack: Vec<String>,
    history: history::History,
    prompt: prompt::Prompt,
    prompt_buf: String,
    prompt_needs_line_start: bool,
    job: Option<Job>,
    exit_warned: bool,
    input_reader: Option<InputReader>,
    home: Option<String>,
    home_display: String,
    session_log: String,
    last_output: capture::OutputBuffer,
    layout_dump: String,
    epsh: epsh::eval::Shell,
    denv: denv::Session,
}

enum ReadResult {
    Line(String),
    Exit,
    Terminated(i32),
    Empty,
}

pub(super) fn run(cli: crate::Args) {

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

    let cwd = std::env::current_dir().unwrap_or_default();
    let mut epsh = epsh::eval::Shell::builder()
        .cwd(cwd)
        .interactive(true)
        .build();
    if let Ok(executable) = std::env::current_exe() {
        sync_path_var(&mut epsh, "SHELL", &executable);
    }
    let initial_cwd = epsh.cwd().to_path_buf();
    sync_path_var(&mut epsh, "PWD", &initial_cwd);
    for name in builtin::all_builtin_names() {
        let Some(spec) = builtin::lookup(name) else {
            continue;
        };
        epsh.set_command_identity(
            epsh::shell_bytes::ShellBytes::from(name),
            epsh::eval::CommandIdentity::Builtin,
        );
        if spec.external_command().is_some() {
            epsh.set_command_placement(
                epsh::shell_bytes::ShellBytes::from(name),
                epsh::eval::CommandPlacement::ParentProcess,
            );
        }
    }
    let fs_default_scratch = std::path::PathBuf::from("/tmp");

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
        editor: Editor::new(rows, cols),
        aliases,
        last_status: 0,
        dir_stack: Vec::with_capacity(32),
        history: history::History::load_from_home(home_os.as_deref()).unwrap_or_else(|error| {
            eprintln!("ish: cannot open history: {error}");
            std::process::exit(1);
        }),
        prompt: prompt::Prompt::with_identity(&user, &home_display),
        prompt_buf: String::with_capacity(128),
        prompt_needs_line_start: false,
        job: None,
        exit_warned: false,
        input_reader: Some(InputReader::new(signal_fd)),
        home,
        home_display,
        session_log: String::new(),
        last_output: capture::OutputBuffer::default(),
        layout_dump: String::new(),
        epsh,
        denv,
    };

    if let Err(error) = denv::on_startup(&shell.denv, &mut shell.epsh) { eprintln!("denv: {error}"); }
    sync_shell_store_derived(&mut shell);
    builtin::install_epsh_policy(&mut shell.epsh);

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
                if line.split_whitespace().next() != Some("exit") {
                    shell.exit_warned = false;
                }

                let line = maybe_rewrite_cd(
                    &line,
                    &shell.aliases,
                    store_env_os(&shell.epsh, "HOME").as_deref().unwrap_or(std::ffi::OsStr::new("")),
                    store_path_env(&shell.epsh),
                );

                let history_line =
                    resolve_cd_for_history(&expand_aliases_for_history(&line, &shell.aliases));
                let history_cwd = shell.epsh.cwd().to_path_buf();

                if history_line.trim() != "l"
                    && let Err(error) = shell.history.add_in_dir(&history_line, Some(&history_cwd))
                {
                    eprintln!("ish: command was not saved to history: {error}");
                }

                shell.session_log.push_str(&line);
                shell.session_log.push('\n');

                let expanded = ish::alias::expand_command_line(&line, &shell.aliases);

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
                            let (quit, session) = capture_output(buffer, false, || {
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

                let prev_cwd = shell.epsh.cwd().to_path_buf();

                let history_log = shell
                    .history
                    .storage_path()
                    .expect("interactive history has a storage path")
                    .to_path_buf();
                let handler = external::make_external_dispatcher(history_log, fs_default_scratch.clone());
                let names = builtin::all_builtin_names().filter(|name| builtin::lookup(name).is_some_and(|spec| spec.external_command().is_some())).map(epsh::shell_bytes::ShellBytes::from).collect();
                shell.epsh.set_external_dispatcher(names, handler);

                let buffer = std::mem::take(&mut shell.last_output);
                let occupied = shell.job.is_some();
                let converged = dispatch::is_single_simple_command(&program);
                if converged {
                    shell.epsh.set_foreground_ctty(true);
                }
                let (outcome, mut session) = capture_output(buffer, converged, || {
                    let outcome = shell.epsh.run_program_outcome(&program);
                    if occupied {
                        continue_occupied_foreground(&mut shell.epsh, converged, outcome)
                    } else {
                        outcome
                    }
                });
                shell.epsh.set_foreground_ctty(false);
                shell.last_status = match &outcome {
                    epsh::eval::ProgramOutcome::Completed(status) => status.code(),
                    epsh::eval::ProgramOutcome::Stopped(_) => 148,
                };
                sync_shell_store_derived(&mut shell);

                if shell.epsh.cwd() != prev_cwd {
                    let new_cwd = shell.epsh.cwd().to_path_buf();
                    let _ = std::env::set_current_dir(&new_cwd);
                    sync_cwd_change(&mut shell, &prev_cwd, new_cwd);
                }

                if let epsh::eval::ProgramOutcome::Stopped(handle) = outcome {
                    let saved_modes = match session.as_ref() {
                        Some(session) if session.is_converged() => session.slave_termios(),
                        _ => term::save_termios(),
                    };
                    match saved_modes {
                        Ok(termios) => {
                            eprintln!("ish: stopped: {} (pgid={})", expanded, handle.process_group());
                            if let Some(session) = session.as_ref() {
                                session.set_paused(true);
                            }
                            shell.job = Some(Job { handle, cmd: expanded.to_string(), termios, capture: session.take() });
                        }
                        Err(error) => eprintln!("ish: cannot save stopped terminal attributes: {error}"),
                    }
                }
                term::restore_startup_termios();
                if let Some(session) = session {
                    session.finish_into(&mut shell.last_output);
                }

                shell.prompt_needs_line_start = true;
            }
            ReadResult::Exit => {
                break;
            }
            ReadResult::Terminated(sig) => {
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

fn continue_occupied_foreground(epsh: &mut epsh::eval::Shell, converged: bool, outcome: epsh::eval::ProgramOutcome) -> epsh::eval::ProgramOutcome {
    let epsh::eval::ProgramOutcome::Stopped(mut handle) = outcome else { return outcome; };
    eprintln!("ish: a job is already suspended; resume it with fg before suspending another");
    loop {
        if converged {
            match epsh.resume_foreground(&mut handle) {
                Ok(None) => {}
                Ok(Some(status)) => return epsh::eval::ProgramOutcome::Completed(status),
                Err(error) => {
                    eprintln!("ish: cannot continue foreground job: {error}");
                    return epsh::eval::ProgramOutcome::Completed(epsh::error::ExitStatus::FAILURE);
                }
            }
            continue;
        }
        let termios = match term::save_termios() {
            Ok(termios) => termios,
            Err(error) => {
                eprintln!("ish: cannot save foreground terminal attributes: {error}");
                return epsh::eval::ProgramOutcome::Completed(epsh::error::ExitStatus::FAILURE);
            }
        };
        term::restore_startup_termios();
        if let Err(error) = rustix::termios::tcsetattr(term::stdin(), rustix::termios::OptionalActions::Drain, &termios) {
            eprintln!("ish: cannot restore foreground terminal attributes: {error}");
            return epsh::eval::ProgramOutcome::Completed(epsh::error::ExitStatus::FAILURE);
        }
        match epsh.resume_foreground(&mut handle) {
            Ok(None) => {}
            Ok(Some(status)) => return epsh::eval::ProgramOutcome::Completed(status),
            Err(error) => {
                eprintln!("ish: cannot continue foreground job: {error}");
                return epsh::eval::ProgramOutcome::Completed(epsh::error::ExitStatus::FAILURE);
            }
        }
    }
}

fn capture_output<T>(
    mut buffer: capture::OutputBuffer,
    converged: bool,
    run: impl FnOnce() -> T,
) -> (T, Option<capture::Session>) {
    buffer.clear();
    match capture::Capture::begin(buffer, converged) {
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

fn run_shell_builtin(name: &str, args: &[epsh::ast::Word], line: &str, shell: &mut Shell) -> bool {
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
            let status = denv::command(&shell.denv, &args, &mut shell.epsh);
            sync_shell_store_derived(shell);
            shell.last_status = status;
        }
        ShellCommand::ResumeJob => {
            shell.last_status = ish::job::resume_job(&mut shell.epsh, &mut shell.job, &mut shell.last_output);
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

fn expand_aliases_for_history(line: &str, aliases: &AliasMap) -> String {
    ish::alias::expand_command_line(line, aliases).into_owned()
}

fn sync_path_var(epsh: &mut epsh::eval::Shell, name: &str, path: &Path) {
    match epsh.vars_mut().set_bytes(name, epsh::shell_bytes::ShellBytes::from_os_str(path.as_os_str())) {
        Ok(()) => epsh.vars_mut().export(name),
        Err(error) => eprintln!("ish: {error}"),
    }
}

fn sync_cwd_change(shell: &mut Shell, old_cwd: &Path, new_cwd: std::path::PathBuf) {
    sync_path_var(&mut shell.epsh, "OLDPWD", old_cwd);
    sync_path_var(&mut shell.epsh, "PWD", &new_cwd);
    shell.epsh.set_cwd(new_cwd);
    push_dir_stack(&mut shell.dir_stack);
    shell.prompt.invalidate_git();
    if let Err(error) = denv::on_cd(&shell.denv, &mut shell.epsh) { eprintln!("denv: {error}"); }
    sync_shell_store_derived(shell);
}

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
        if dir_stack.last().map(|s| s.as_str()) != Some(&pwd) {
            dir_stack.push(pwd);
            if dir_stack.len() > 50 {
                dir_stack.remove(0);
            }
        }
    }
}

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


fn editor_context<'a>(aliases: &'a AliasMap, history: &'a history::History, epsh: &'a epsh::eval::Shell, home: Option<&'a str>, home_display: &'a str, directories: &'a [String]) -> EditorContext<'a> {
    EditorContext { aliases, history, cwd: epsh.cwd(), home, home_display, path: store_path_env(epsh), directories }
}

fn emit_working_directory(shell: &Shell) {
    let pwd = shell.epsh.vars().get("PWD").unwrap_or("");
    if !pwd.is_empty() {
        let hostname = shell.epsh.vars().get("HOSTNAME").unwrap_or("");
        let mut osc = String::from("\x1b]7;file://");
        prompt::push_percent_encoded(&mut osc, hostname.as_bytes(), false);
        prompt::push_percent_encoded(&mut osc, pwd.as_bytes(), true);
        osc.push('\x07');
        let _ = std::io::Write::write_all(&mut std::io::stdout(), osc.as_bytes());
    }
}

fn flush_editor(editor: &mut Editor) -> std::io::Result<()> {
    use std::io::Write;
    let mut output = std::io::stdout().lock();
    editor.write_output(&mut output)?;
    output.flush()
}

fn read_line(shell: &mut Shell) -> ReadResult {
    let mut reader = shell.input_reader.take().expect("input reader is present between prompts");
    let result = read_line_with_reader(shell, &mut reader);
    shell.input_reader = Some(reader);
    result
}

fn read_line_with_reader(shell: &mut Shell, reader: &mut InputReader) -> ReadResult {
    let _raw = match term::RawMode::enable() {
        Ok(raw) => raw,
        Err(error) => { eprintln!("ish: raw mode: {error}"); return ReadResult::Exit; }
    };
    // Catch SIGINT while editing (the top-level disposition ignores it) so
    // kill -INT cancels the line instead of doing nothing. Restored on drop.
    let _foreground_signals = match signal::ForegroundSignals::install() {
        Ok(guard) => guard,
        Err(error) => { eprintln!("ish: foreground signals: {error}"); return ReadResult::Exit; }
    };
    emit_working_directory(shell);
    if shell.prompt_needs_line_start { print!("\r\n"); shell.prompt_needs_line_start = false; }
    let pwd = shell.epsh.vars().get("PWD").unwrap_or("");
    let dirty = shell.epsh.vars().get("__DENV_DIRTY") == Some("1");
    shell.prompt.render_into(&mut shell.prompt_buf, shell.last_status, pwd, dirty);
    let prompt = std::mem::take(&mut shell.prompt_buf);
    let display_len = shell.prompt.display_len(&prompt);
    let context = editor_context(&shell.aliases, &shell.history, &shell.epsh, shell.home.as_deref(), &shell.home_display, &shell.dir_stack);
    shell.editor.begin(prompt, display_len, &context);
    if let Err(error) = flush_editor(&mut shell.editor) { eprintln!("ish: output: {error}"); return ReadResult::Exit; }
    loop {
        let input = reader.read_event();
        let event = match &input {
            InputEvent::Signal(signal) if *signal == rustix::process::Signal::TERM.as_raw() || *signal == rustix::process::Signal::HUP.as_raw() => return ReadResult::Terminated(*signal),
            InputEvent::Signal(signal) if *signal == rustix::process::Signal::WINCH.as_raw() => {
                let (rows, cols) = term::term_size(); EditorEvent::Resize { rows, cols }
            }
            // A kill -INT during editing cancels the line exactly like Ctrl+C.
            InputEvent::Signal(signal) if *signal == rustix::process::Signal::INT.as_raw() => {
                EditorEvent::Key(KeyEvent::ctrl('c'))
            }
            InputEvent::Signal(_) => continue,
            InputEvent::Key(key) => EditorEvent::Key(*key),
            InputEvent::Paste(text) => EditorEvent::Paste(text),
            InputEvent::PasteRejected => EditorEvent::PasteRejected,
        };
        let context = editor_context(&shell.aliases, &shell.history, &shell.epsh, shell.home.as_deref(), &shell.home_display, &shell.dir_stack);
        let action = shell.editor.handle(event, &context);
        match action {
            EditorAction::Continue => {}
            EditorAction::Complete => shell.editor.complete(&context),
            EditorAction::SearchHistory => {
                if let Err(error) = shell.history.sync() { eprintln!("ish: cannot sync history: {error}"); }
                let context = editor_context(&shell.aliases, &shell.history, &shell.epsh, shell.home.as_deref(), &shell.home_display, &shell.dir_stack);
                shell.editor.search_history(&context);
            }
            EditorAction::Dump => {
                shell.editor.render_layout_dump(&mut shell.layout_dump, &context);
                if let Err(error) = write_layout_dump_to_file(shell) { eprintln!("{error}"); }
            }
            EditorAction::ChangeDirectory(directory) => { eprintln!("{directory}"); change_directory(&directory, shell); }
            EditorAction::Submit(line) => {
                shell.editor.render_layout_dump(&mut shell.layout_dump, &context);
                shell.prompt_buf = shell.editor.take_prompt();
                if let Err(error) = flush_editor(&mut shell.editor) { eprintln!("ish: output: {error}"); return ReadResult::Exit; }
                return if line.is_empty() { ReadResult::Empty } else { ReadResult::Line(line) };
            }
            EditorAction::Cancel | EditorAction::Exit => {
                shell.prompt_buf = shell.editor.take_prompt();
                if let Err(error) = flush_editor(&mut shell.editor) { eprintln!("ish: output: {error}"); return ReadResult::Exit; }
                return match action { EditorAction::Exit => handle_exit(shell), _ => ReadResult::Empty };
            }
        }
        if let Err(error) = flush_editor(&mut shell.editor) { eprintln!("ish: output: {error}"); return ReadResult::Exit; }
    }
}

