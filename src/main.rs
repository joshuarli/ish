use ish::alias::AliasMap;
use ish::complete::CompletionState;
use ish::history::FuzzyMatch;
use ish::input::{InputEvent, InputReader, Key, KeyEvent};
use ish::job::Job;
use ish::line::LineBuffer;
use ish::term::TermWriter;
use ish::{builtin, complete, config, denv, exec, history, parse, prompt, render, signal, term};
use std::os::fd::RawFd;

struct Shell {
    aliases: AliasMap,
    last_status: i32,
    prev_dir: Option<String>,
    rows: u16,
    cols: u16,
    history: history::History,
    prompt: prompt::Prompt,
    /// Reusable buffer for rendered prompt string — avoids allocation per render.
    prompt_buf: String,
    /// Reusable completion arena — avoids allocation per tab press.
    comp_buf: complete::Completions,
    /// Reusable fuzzy match buffer — avoids allocation per Ctrl+R keystroke.
    match_buf: Vec<history::FuzzyMatch>,
    job: Option<Job>,
    exit_warned: bool,
    denv_active: Option<bool>, // None = unchecked, defers scan_path to first use
    orig_termios: libc::termios,
    signal_fd: RawFd,
    home: String,
    session_log: String,
}

enum ReadResult {
    Line(String),
    Exit,
    Empty,
}

enum Mode {
    Normal,
    Completion(CompletionState),
    HistorySearch {
        query: String,
        matches: Vec<FuzzyMatch>,
        selected: usize,
        saved_line: String,
    },
}

struct Args {
    config: Option<String>, // -c <path>: custom config file
    no_config: bool,        // --no-config: skip config loading
}

fn parse_args() -> Args {
    let mut args = Args {
        config: None,
        no_config: false,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("ish {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-c" => match argv.next() {
                Some(path) => args.config = Some(path),
                None => {
                    eprintln!("ish: -c requires a config file path");
                    std::process::exit(1);
                }
            },
            "--no-config" => args.no_config = true,
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

fn main() {
    let cli = parse_args();

    // Unset $SHELL — ish is not POSIX, don't confuse child processes
    // SAFETY: single-threaded shell, no other threads reading env
    unsafe { std::env::remove_var("SHELL") };

    // Save original termios
    let orig_termios = match term::save_termios() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ish: cannot get terminal settings: {e}");
            std::process::exit(1);
        }
    };

    // SAFETY: Set shell as its own process group leader and take foreground
    // control of the terminal. Called once at startup, single-threaded.
    unsafe {
        let pid = libc::getpid();
        let _ = libc::setpgid(pid, pid);
        // pgid == pid after setpgid above, no need for getpgrp() syscall
        let _ = libc::tcsetpgrp(0, pid);
    }

    // Initialize signals
    let signal_fd = signal::init();

    let (rows, cols) = term::term_size();
    let home = std::env::var("HOME").unwrap_or_default();

    let mut shell = Shell {
        aliases: AliasMap::new(),
        last_status: 0,
        prev_dir: None,
        rows,
        cols,
        history: history::History::load(),
        prompt: prompt::Prompt::new(),
        prompt_buf: String::with_capacity(128),
        comp_buf: complete::Completions::with_capacity(2048, 64),
        match_buf: Vec::with_capacity(200),
        job: None,
        exit_warned: false,
        denv_active: None,
        orig_termios,
        signal_fd,
        home: home.clone(),
        session_log: String::new(),
    };

    // Load config
    if !cli.no_config {
        config::load(&mut shell.aliases, cli.config.as_deref());
    }

    // denv init is deferred — scan_path("denv") runs on first cd, not startup

    // Main loop
    loop {
        match read_line(&mut shell) {
            ReadResult::Line(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                shell.exit_warned = false;

                // Add to history unless it's a bare alias or builtin
                let first_word = line.split_whitespace().next().unwrap_or("");
                if !builtin::is_builtin(first_word) && shell.aliases.get(first_word).is_none() {
                    shell.history.add(&line);
                }

                // Log for session transcript
                shell.session_log.push_str(&line);
                shell.session_log.push('\n');

                match parse::parse(&line) {
                    Ok(cmdline) => {
                        // Handle alias builtin specially
                        if cmdline.segments.len() == 1 && cmdline.segments[0].0.commands.len() == 1
                        {
                            let cmd = &cmdline.segments[0].0.commands[0].cmd;
                            let first =
                                parse::unescape(cmd.argv.first().map(|s| s.as_str()).unwrap_or(""));
                            if first == "source" || first == "." {
                                eprintln!("ish: {first}: sourcing is not supported");
                                shell.last_status = 1;
                                continue;
                            }

                            if first == "alias" {
                                let args: Vec<String> =
                                    cmd.argv[1..].iter().map(|s| parse::unescape(s)).collect();
                                if args.len() >= 2 {
                                    let name = args[0].clone();
                                    let expansion = args[1..].to_vec();
                                    shell.aliases.set(name, expansion);
                                    shell.last_status = 0;
                                } else if args.len() == 1 {
                                    // Show alias
                                    if let Some(exp) = shell.aliases.get(&args[0]) {
                                        println!("alias {} {}", args[0], exp.join(" "));
                                    } else {
                                        eprintln!("ish: alias: not found: {}", args[0]);
                                        shell.last_status = 1;
                                    }
                                } else {
                                    // List all aliases
                                    for (name, exp) in shell.aliases.iter() {
                                        println!("alias {name} {}", exp.join(" "));
                                    }
                                    shell.last_status = 0;
                                }
                                continue;
                            }

                            // Handle `exit` with exit_warned logic
                            if first == "exit" {
                                if shell.job.is_some() {
                                    if shell.exit_warned {
                                        if let Some(job) = shell.job.take() {
                                            // SAFETY: Send SIGTERM to job's pgid before exit.
                                            unsafe {
                                                libc::killpg(job.pgid, libc::SIGTERM);
                                            }
                                        }
                                        break;
                                    } else {
                                        eprintln!(
                                            "ish: there is a suspended job. Exit again to force quit."
                                        );
                                        shell.exit_warned = true;
                                        shell.last_status = 1;
                                        continue;
                                    }
                                }
                                let code: i32 = cmd
                                    .argv
                                    .get(1)
                                    .map(|s| parse::unescape(s))
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(0);
                                shell.history.save_cache();
                                std::process::exit(code);
                            }

                            // Handle `history` builtin
                            if first == "history" {
                                let sub = cmd.argv.get(1).map(|s| parse::unescape(s));
                                match sub.as_deref() {
                                    None => {
                                        for i in 0..shell.history.len() {
                                            println!("{}", shell.history.get(i));
                                        }
                                        shell.last_status = 0;
                                    }
                                    Some("compact") => {
                                        shell.history.compact();
                                        shell.last_status = 0;
                                    }
                                    Some(other) => {
                                        eprintln!("ish: history: unknown subcommand: {other}");
                                        shell.last_status = 1;
                                    }
                                }
                                continue;
                            }

                            // Handle `copy-scrollback` — OSC 52 clipboard
                            if first == "copy-scrollback" {
                                use std::io::Write;
                                let encoded = base64_encode(shell.session_log.as_bytes());
                                let osc = format!("\x1b]52;c;{encoded}\x07");
                                let _ = std::io::stdout().write_all(osc.as_bytes());
                                let _ = std::io::stdout().flush();
                                shell.last_status = 0;
                                continue;
                            }

                            // Handle `denv allow|deny|reload` — apply output to env
                            if first == "denv" && *shell.denv_active.get_or_insert_with(denv::init)
                            {
                                let args: Vec<String> =
                                    cmd.argv[1..].iter().map(|s| parse::unescape(s)).collect();
                                if let Some(_path_modified) = denv::command(&args) {
                                    shell.last_status = 0;
                                    continue;
                                }
                                // Not allow/deny/reload — fall through to normal exec
                            }

                            // Handle `fg` with continuation support
                            if first == "fg" {
                                let (status, cont) = exec::resume_job(&mut shell.job);
                                shell.last_status = status;
                                // Execute remaining segments from compound command
                                run_continuation(&mut shell, cont);
                                continue;
                            }

                            // Handle `w`/`which`/`type` with alias awareness
                            if first == "w" || first == "which" || first == "type" {
                                let args: Vec<String> =
                                    cmd.argv[1..].iter().map(|s| parse::unescape(s)).collect();
                                if let Some(name) = args.first()
                                    && let Some(exp) = shell.aliases.get(name.as_str())
                                {
                                    println!("alias: {} {}", name, exp.join(" "));
                                    shell.last_status = 0;
                                    continue;
                                }
                                // Fall through to normal exec for builtin/PATH check
                            }
                        }

                        // Handle cd specially to invalidate prompt git cache
                        let is_cd = cmdline.segments.len() == 1
                            && cmdline.segments[0].0.commands.len() == 1
                            && parse::unescape(
                                cmdline.segments[0].0.commands[0]
                                    .cmd
                                    .argv
                                    .first()
                                    .map(|s| s.as_str())
                                    .unwrap_or(""),
                            ) == "cd";

                        shell.last_status = exec::execute(
                            &cmdline,
                            None,
                            &shell.aliases,
                            &mut shell.job,
                            &shell.orig_termios,
                            &shell.home,
                            &mut shell.prev_dir,
                            &mut shell.session_log,
                        );

                        if is_cd {
                            shell.prompt.invalidate_git();
                            if *shell.denv_active.get_or_insert_with(denv::init) {
                                denv::on_cd();
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("ish: {e}");
                        shell.last_status = 1;
                    }
                }
            }
            ReadResult::Exit => {
                shell.history.compact();
                break;
            }
            ReadResult::Empty => {}
        }
    }
}

/// Read an env var via libc::getenv — zero allocation (returns &str into env block).
/// SAFETY: Only safe in a single-threaded context (which ish is).
fn getenv_str(name: &std::ffi::CStr) -> &'static str {
    unsafe {
        let ptr = libc::getenv(name.as_ptr());
        if ptr.is_null() {
            return "";
        }
        let cstr = std::ffi::CStr::from_ptr(ptr);
        // Env vars set by the shell are always valid UTF-8; fallback to empty on exotic values.
        std::str::from_utf8(cstr.to_bytes()).unwrap_or("")
    }
}

fn read_line(shell: &mut Shell) -> ReadResult {
    let _raw = match term::RawMode::enable() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ish: raw mode: {e}");
            return ReadResult::Exit;
        }
    };

    let mut tw = TermWriter::new();
    let mut reader = InputReader::new(shell.signal_fd);
    let mut full_input = String::new();
    let mut line = LineBuffer::new();
    let mut mode = Mode::Normal;
    let mut history_idx: Option<usize> = None;
    let mut saved_line = String::new();
    // Render prompt — zero-alloc: reuse shell.prompt_buf, read env via getenv.
    // Take ownership so prompt_str can be borrowed independently of shell.
    let pwd = getenv_str(c"PWD");
    let denv_dirty = getenv_str(c"__DENV_DIRTY") == "1";
    shell
        .prompt
        .render_into(&mut shell.prompt_buf, shell.last_status, pwd, denv_dirty);
    let prompt_str = std::mem::take(&mut shell.prompt_buf);
    let prompt_display_len = shell.prompt.display_len(&prompt_str);

    let info = render::render_line(
        &mut tw,
        &prompt_str,
        prompt_display_len,
        &line,
        shell.cols,
        0,
    );
    let mut cursor_row = info.cursor_row;
    let _ = tw.flush_to_stdout();

    loop {
        let event = reader.read_event();
        match event {
            InputEvent::Signal(sig) => {
                if sig == libc::SIGWINCH {
                    let (rows, cols) = term::term_size();
                    shell.rows = rows;
                    shell.cols = cols;
                }
                // Re-render
                if let Mode::Normal = &mode {
                    let (p, pdl) = active_prompt(&prompt_str, prompt_display_len, &full_input);
                    let info = render::render_line(&mut tw, p, pdl, &line, shell.cols, cursor_row);
                    cursor_row = info.cursor_row;
                }
                let _ = tw.flush_to_stdout();
                continue;
            }
            InputEvent::Key(key) => {
                match &mut mode {
                    Mode::Normal => {
                        match handle_normal_key(
                            key,
                            &mut line,
                            &mut history_idx,
                            &mut saved_line,
                            shell,
                            &full_input,
                        ) {
                            KeyAction::Continue => {}
                            KeyAction::Execute(text) => {
                                tw.write_str("\r\n");
                                let _ = tw.flush_to_stdout();
                                shell.prompt_buf = prompt_str;
                                if full_input.is_empty() {
                                    return if text.is_empty() {
                                        ReadResult::Empty
                                    } else {
                                        ReadResult::Line(text)
                                    };
                                } else {
                                    full_input.push(' ');
                                    full_input.push_str(&text);
                                    return ReadResult::Line(full_input);
                                }
                            }
                            KeyAction::Continuation(text) => {
                                if full_input.is_empty() {
                                    full_input = text;
                                } else {
                                    full_input.push(' ');
                                    full_input.push_str(&text);
                                }
                                line = LineBuffer::new();
                                history_idx = None;
                                tw.write_str("\r\n");
                                let info =
                                    render::render_line(&mut tw, "  ", 2, &line, shell.cols, 0);
                                cursor_row = info.cursor_row;
                                let _ = tw.flush_to_stdout();
                                continue;
                            }
                            KeyAction::Cancel => {
                                tw.write_str("^C\r\n");
                                let _ = tw.flush_to_stdout();
                                shell.prompt_buf = prompt_str;
                                return ReadResult::Empty;
                            }
                            KeyAction::Exit => {
                                tw.write_str("\r\n");
                                let _ = tw.flush_to_stdout();
                                shell.prompt_buf = prompt_str;
                                return handle_exit(shell);
                            }
                            KeyAction::ClearScreen => {
                                tw.clear_screen();
                                cursor_row = 0;
                            }
                            KeyAction::StartHistorySearch => {
                                saved_line = line.text().to_string();
                                let mut matches = std::mem::take(&mut shell.match_buf);
                                shell.history.fuzzy_search_into("", &mut matches, 200);
                                mode = Mode::HistorySearch {
                                    query: String::new(),
                                    matches,
                                    selected: 0,
                                    saved_line: saved_line.clone(),
                                };
                                render_history_mode(&mut tw, &mode, shell);
                                let _ = tw.flush_to_stdout();
                                continue;
                            }
                            KeyAction::StartCompletion => {
                                let comp = std::mem::take(&mut shell.comp_buf);
                                let cs = start_completion(&line, shell.cols, &shell.home, comp);
                                if cs.comp.len() == 1 {
                                    accept_completion(&mut line, &cs);
                                    shell.comp_buf = cs.comp;
                                } else if !cs.comp.is_empty() {
                                    mode = Mode::Completion(cs);
                                } else {
                                    shell.comp_buf = cs.comp;
                                }
                            }
                        }

                        match &mode {
                            Mode::Normal => {
                                let (p, pdl) =
                                    active_prompt(&prompt_str, prompt_display_len, &full_input);
                                let info = render::render_line(
                                    &mut tw, p, pdl, &line, shell.cols, cursor_row,
                                );
                                cursor_row = info.cursor_row;
                            }
                            Mode::Completion(state) => {
                                let (p, pdl) =
                                    active_prompt(&prompt_str, prompt_display_len, &full_input);
                                let info = render::render_line(
                                    &mut tw, p, pdl, &line, shell.cols, cursor_row,
                                );
                                cursor_row = info.cursor_row;
                                render::render_completions(&mut tw, state, &info, true);
                            }
                            Mode::HistorySearch { .. } => {}
                        }
                        let _ = tw.flush_to_stdout();
                    }

                    Mode::Completion(state) => {
                        let (p, pdl) = active_prompt(&prompt_str, prompt_display_len, &full_input);
                        match handle_completion_key(key, state, &mut line) {
                            CompAction::Navigate => {
                                // Cursor is on prompt line — repaint grid in-place
                                let info = render::render_line(
                                    &mut tw, p, pdl, &line, shell.cols, cursor_row,
                                );
                                cursor_row = info.cursor_row;
                                render::render_completions(&mut tw, state, &info, false);
                                let _ = tw.flush_to_stdout();
                                continue;
                            }
                            CompAction::Refilter => {
                                // Reclaim buffer from current state, re-run completion
                                let comp = std::mem::take(&mut state.comp);
                                let cs = start_completion(&line, shell.cols, &shell.home, comp);
                                if cs.comp.len() == 1 {
                                    accept_completion(&mut line, &cs);
                                    shell.comp_buf = cs.comp;
                                    mode = Mode::Normal;
                                } else if !cs.comp.is_empty() {
                                    let info = render::render_line(
                                        &mut tw, p, pdl, &line, shell.cols, cursor_row,
                                    );
                                    cursor_row = info.cursor_row;
                                    render::render_completions(&mut tw, &cs, &info, true);
                                    mode = Mode::Completion(cs);
                                    let _ = tw.flush_to_stdout();
                                    continue;
                                } else {
                                    shell.comp_buf = cs.comp;
                                    mode = Mode::Normal;
                                }
                            }
                            CompAction::Accept => {
                                accept_completion(&mut line, state);
                                shell.comp_buf = std::mem::take(&mut state.comp);
                                mode = Mode::Normal;
                            }
                            CompAction::Cancel => {
                                shell.comp_buf = std::mem::take(&mut state.comp);
                                mode = Mode::Normal;
                            }
                        }

                        // Cursor is on prompt line — render_line's \r + clear_to_end_of_screen
                        // naturally clears the grid below
                        let info =
                            render::render_line(&mut tw, p, pdl, &line, shell.cols, cursor_row);
                        cursor_row = info.cursor_row;
                        let _ = tw.flush_to_stdout();
                    }

                    Mode::HistorySearch {
                        query,
                        matches,
                        selected,
                        saved_line,
                    } => match handle_history_search_key(key, query, matches, selected, shell) {
                        HistAction::Continue => {
                            render_history_mode(&mut tw, &mode, shell);
                            let _ = tw.flush_to_stdout();
                        }
                        HistAction::Accept(text) => {
                            line.set(&text);
                            shell.match_buf = std::mem::take(matches);
                            mode = Mode::Normal;
                            tw.carriage_return();
                            tw.clear_to_end_of_screen();
                            let info = render::render_line(
                                &mut tw,
                                &prompt_str,
                                prompt_display_len,
                                &line,
                                shell.cols,
                                0,
                            );
                            cursor_row = info.cursor_row;
                            let _ = tw.flush_to_stdout();
                        }
                        HistAction::Cancel => {
                            line.set(saved_line);
                            shell.match_buf = std::mem::take(matches);
                            mode = Mode::Normal;
                            tw.carriage_return();
                            tw.clear_to_end_of_screen();
                            let info = render::render_line(
                                &mut tw,
                                &prompt_str,
                                prompt_display_len,
                                &line,
                                shell.cols,
                                0,
                            );
                            cursor_row = info.cursor_row;
                            let _ = tw.flush_to_stdout();
                        }
                    },
                }
            }
        }
    }
}

fn active_prompt<'a>(
    prompt_str: &'a str,
    prompt_display_len: usize,
    full_input: &str,
) -> (&'a str, usize) {
    if full_input.is_empty() {
        (prompt_str, prompt_display_len)
    } else {
        ("  ", 2)
    }
}

enum KeyAction {
    Continue,
    Execute(String),
    Continuation(String),
    Cancel,
    Exit,
    ClearScreen,
    StartHistorySearch,
    StartCompletion,
}

fn handle_normal_key(
    key: KeyEvent,
    line: &mut LineBuffer,
    history_idx: &mut Option<usize>,
    saved_line: &mut String,
    shell: &Shell,
    full_input: &str,
) -> KeyAction {
    match (key.key, key.mods.ctrl, key.mods.alt) {
        (Key::Char('c'), true, _) => return KeyAction::Cancel,
        (Key::Char('d'), true, _) => {
            if line.is_empty() && full_input.is_empty() {
                return KeyAction::Exit;
            }
            line.delete_forward();
        }
        (Key::Char('a'), true, _) => line.move_home(),
        (Key::Char('e'), true, _) => line.move_end(),
        (Key::Char('k'), true, _) => line.kill_to_end(),
        (Key::Char('u'), true, _) => line.kill_to_start(),
        (Key::Char('w'), true, _) => line.kill_word_back(),
        (Key::Char('y'), true, _) => line.yank(),
        (Key::Char('l'), true, _) => return KeyAction::ClearScreen,
        (Key::Char('r'), true, _) => return KeyAction::StartHistorySearch,

        (Key::Char('b'), false, true) => line.move_word_left(),
        (Key::Char('f'), false, true) => line.move_word_right(),

        (Key::Up, _, _) if !key.mods.ctrl => {
            navigate_history(line, history_idx, saved_line, shell, true);
        }
        (Key::Down, _, _) if !key.mods.ctrl => {
            navigate_history(line, history_idx, saved_line, shell, false);
        }
        (Key::Left, _, _) if key.mods.ctrl || key.mods.alt => line.move_word_left(),
        (Key::Right, _, _) if key.mods.ctrl || key.mods.alt => line.move_word_right(),
        (Key::Left, _, _) => {
            line.move_left();
        }
        (Key::Right, _, _) => {
            line.move_right();
        }
        (Key::Home, _, _) => line.move_home(),
        (Key::End, _, _) => line.move_end(),

        (Key::Backspace, _, _) => {
            line.delete_back();
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
            let text = line.text().to_string();
            let combined = if full_input.is_empty() {
                text.clone()
            } else {
                format!("{full_input} {text}")
            };
            if parse::needs_continuation(&combined) {
                return KeyAction::Continuation(text);
            }
            return KeyAction::Execute(text);
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
            hist.len().checked_sub(1 + skip).map(|i| hist.get(i))
        } else {
            hist.prefix_search(saved_line, skip)
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
                    hist.len().checked_sub(1 + skip).map(|i| hist.get(i))
                } else {
                    hist.prefix_search(saved_line, skip)
                };
                if let Some(e) = entry {
                    line.set(e);
                }
            }
        }
    }
}

fn try_alias_expand(line: &mut LineBuffer, aliases: &AliasMap) {
    let text = line.text().to_string();
    let first_word = match text.split_whitespace().next() {
        Some(w) => w.to_string(),
        None => return,
    };

    if let Some(expansion) = aliases.get(&first_word) {
        let rest = &text[first_word.len()..];
        let expanded_str = expansion.join(" ");
        let new_text = format!("{expanded_str}{rest}");
        line.set(&new_text);
    }
}

// -- Completion --

enum CompAction {
    Navigate,
    Accept,
    Cancel,
    Refilter,
}

fn start_completion(
    line: &LineBuffer,
    term_cols: u16,
    home: &str,
    mut comp: complete::Completions,
) -> CompletionState {
    comp.clear();

    let text = line.text();
    let before_cursor = &text[..line.cursor()];
    let word_start = before_cursor.rfind(' ').map(|i| i + 1).unwrap_or(0);
    let partial = &before_cursor[word_start..];

    // Detect if first word is cd → complete only directories
    let first_word = text.split_whitespace().next().unwrap_or("");
    let dirs_only = first_word == "cd" && word_start > 0;

    // Expand tilde for filesystem lookup
    let expanded = if partial == "~" {
        format!("{home}/")
    } else if let Some(rest) = partial.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        partial.to_string()
    };

    // dir_prefix keeps the original (unexpanded) form for accept_completion
    let dir_prefix = if let Some(slash_pos) = partial.rfind('/') {
        partial[..=slash_pos].to_string()
    } else {
        String::new()
    };

    complete::complete_path_into(&expanded, dirs_only, &mut comp);
    if !comp.is_empty() {
        let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
        return CompletionState {
            comp,
            selected: 0,
            cols,
            rows,
            scroll: 0,
            dir_prefix,
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
                comp.finish_entry(mark, entry.is_dir, entry.is_link, entry.is_exec);
            }
        }

        let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
        return CompletionState {
            comp,
            selected: 0,
            cols,
            rows,
            scroll: 0,
            dir_prefix: user_root,
        };
    }

    // Nothing found — return empty state, preserving the buffer for reuse.
    CompletionState {
        comp,
        selected: 0,
        cols: 0,
        rows: 0,
        scroll: 0,
        dir_prefix: String::new(),
    }
}

fn handle_completion_key(
    key: KeyEvent,
    state: &mut CompletionState,
    line: &mut LineBuffer,
) -> CompAction {
    match key.key {
        Key::Up => {
            state.move_up();
            CompAction::Navigate
        }
        Key::Down | Key::Tab => {
            state.move_down();
            CompAction::Navigate
        }
        Key::Left => {
            state.move_left();
            CompAction::Navigate
        }
        Key::Right => {
            state.move_right();
            CompAction::Navigate
        }
        Key::Enter => CompAction::Accept,
        Key::Escape => CompAction::Cancel,
        Key::Char('c') if key.mods.ctrl => CompAction::Cancel,
        Key::Char(c) if !key.mods.ctrl && !key.mods.alt => {
            line.insert_char(c);
            CompAction::Refilter
        }
        Key::Backspace => {
            if line.cursor() > 0 {
                line.delete_back();
                CompAction::Refilter
            } else {
                CompAction::Cancel
            }
        }
        _ => CompAction::Cancel,
    }
}

fn accept_completion(line: &mut LineBuffer, state: &CompletionState) {
    let entry = match state.selected_entry() {
        Some(e) => e,
        None => return,
    };
    let name = match state.selected_name() {
        Some(n) => n,
        None => return,
    };

    let text = line.text().to_string();
    let before_cursor = &text[..line.cursor()];
    let word_start = before_cursor.rfind(' ').map(|i| i + 1).unwrap_or(0);
    let after_cursor = &text[line.cursor()..];

    let mut completion = state.dir_prefix.clone();
    completion.push_str(name);
    if entry.is_dir {
        completion.push('/');
    }

    let new_text = format!("{}{}{}", &text[..word_start], completion, after_cursor);
    let new_cursor_chars = text[..word_start].chars().count() + completion.chars().count();
    line.set(&new_text);
    // Position cursor after the completion
    line.move_home();
    for _ in 0..new_cursor_chars {
        line.move_right();
    }
}

// -- History Search --

enum HistAction {
    Continue,
    Accept(String),
    Cancel,
}

fn handle_history_search_key(
    key: KeyEvent,
    query: &mut String,
    matches: &mut Vec<FuzzyMatch>,
    selected: &mut usize,
    shell: &Shell,
) -> HistAction {
    match key.key {
        Key::Escape => HistAction::Cancel,
        Key::Char('c') if key.mods.ctrl => HistAction::Cancel,
        Key::Enter => {
            if let Some(m) = matches.get(*selected) {
                HistAction::Accept(shell.history.get(m.entry_idx).to_string())
            } else {
                HistAction::Cancel
            }
        }
        Key::Up | Key::Char('p') if key.key == Key::Up || key.mods.ctrl => {
            if *selected > 0 {
                *selected -= 1;
            }
            HistAction::Continue
        }
        Key::Down | Key::Char('n') if key.key == Key::Down || key.mods.ctrl => {
            if *selected + 1 < matches.len() {
                *selected += 1;
            }
            HistAction::Continue
        }
        Key::Backspace => {
            query.pop();
            shell.history.fuzzy_search_into(query, matches, 200);
            *selected = 0;
            HistAction::Continue
        }
        Key::Char(c) if !key.mods.ctrl => {
            query.push(c);
            shell.history.fuzzy_search_into(query, matches, 200);
            *selected = 0;
            HistAction::Continue
        }
        _ => HistAction::Continue,
    }
}

fn render_history_mode(tw: &mut TermWriter, mode: &Mode, shell: &Shell) {
    if let Mode::HistorySearch {
        query,
        matches,
        selected,
        ..
    } = mode
    {
        render::render_history_pager(
            tw,
            query,
            matches,
            &shell.history,
            *selected,
            shell.rows,
            shell.cols,
        );
    }
}

fn handle_exit(shell: &mut Shell) -> ReadResult {
    if shell.job.is_some() {
        if shell.exit_warned {
            if let Some(job) = shell.job.take() {
                // SAFETY: Send SIGTERM to the job's pgid before force-quit.
                unsafe {
                    libc::killpg(job.pgid, libc::SIGTERM);
                }
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

/// Execute remaining segments from a compound command after job resume.
/// E.g., `sleep 2 && echo hi` suspended during sleep — after fg, runs `echo hi`.
fn run_continuation(shell: &mut Shell, cont: Option<ish::job::Continuation>) {
    let Some(cont) = cont else { return };

    let remaining = parse::CommandLine {
        segments: cont.segments,
    };
    shell.last_status = exec::execute(
        &remaining,
        Some((shell.last_status, cont.connector)),
        &shell.aliases,
        &mut shell.job,
        &shell.orig_termios,
        &shell.home,
        &mut shell.prev_dir,
        &mut shell.session_log,
    );
    // If the continuation itself suspended, any further continuation is
    // already saved on the job by execute(). Nothing more to do here.
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
