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
    path_cache: exec::PathCache,
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
        query: LineBuffer,
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

    // Set $SHELL to ish binary path
    if let Ok(exe) = std::env::current_exe() {
        ish::shell_setenv("SHELL", &exe.to_string_lossy());
    }

    // Set $PWD — parent process (terminal emulator) may not provide it
    if let Ok(cwd) = std::env::current_dir() {
        ish::shell_setenv("PWD", &cwd.to_string_lossy());
    }

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
        path_cache: exec::PathCache::new(),
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
        shell.history.sync();
        match read_line(&mut shell) {
            ReadResult::Line(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                shell.exit_warned = false;

                // Rewrite shorthand cd forms before parsing:
                //   ".." → "cd ..", "..." → "cd ../..", etc.
                //   single-arg directory that isn't an executable → "cd <dir>"
                let line = maybe_rewrite_cd(&line, &shell.aliases);

                // Expand aliases before recording in history so "g status"
                // becomes "git status" — matches what actually runs.
                let history_line = expand_aliases_for_history(&line, &shell.aliases);

                // Log for session transcript
                shell.session_log.push_str(&line);
                shell.session_log.push('\n');

                match parse::parse(&line) {
                    Ok(cmdline) => {
                        // Single simple command: handle shell-level builtins
                        let handled = if let Some(first) = first_command_word(&cmdline) {
                            let cmd = &cmdline.segments[0].0.commands[0].cmd;
                            match first.as_str() {
                                "source" | "." => {
                                    eprintln!("ish: {first}: sourcing is not supported");
                                    shell.last_status = 1;
                                    true
                                }
                                "alias" => {
                                    let args: Vec<String> =
                                        cmd.argv[1..].iter().map(|s| parse::unescape(s)).collect();
                                    if args.len() >= 2 {
                                        let name = args[0].clone();
                                        let expansion = args[1..].to_vec();
                                        shell.aliases.set(name, expansion);
                                        shell.last_status = 0;
                                    } else if args.len() == 1 {
                                        if let Some(exp) = shell.aliases.get(&args[0]) {
                                            println!("alias {} {}", args[0], exp.join(" "));
                                        } else {
                                            eprintln!("ish: alias: not found: {}", args[0]);
                                            shell.last_status = 1;
                                        }
                                    } else {
                                        for (name, exp) in shell.aliases.iter() {
                                            println!("alias {name} {}", exp.join(" "));
                                        }
                                        shell.last_status = 0;
                                    }
                                    true
                                }
                                "exit" => {
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
                                            true
                                        }
                                    } else {
                                        let code: i32 = cmd
                                            .argv
                                            .get(1)
                                            .map(|s| parse::unescape(s))
                                            .and_then(|s| s.parse().ok())
                                            .unwrap_or(0);
                                        shell.history.compact();
                                        std::process::exit(code);
                                    }
                                }
                                "history" => {
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
                                        Some("rebuild") => {
                                            shell.history.rebuild();
                                            shell.last_status = 0;
                                        }
                                        Some(other) => {
                                            eprintln!("ish: history: unknown subcommand: {other}");
                                            shell.last_status = 1;
                                        }
                                    }
                                    true
                                }
                                "copy-scrollback" => {
                                    use std::io::Write;
                                    let encoded = base64_encode(shell.session_log.as_bytes());
                                    let osc = format!("\x1b]52;c;{encoded}\x07");
                                    let _ = std::io::stdout().write_all(osc.as_bytes());
                                    let _ = std::io::stdout().flush();
                                    shell.last_status = 0;
                                    true
                                }
                                "denv" if *shell.denv_active.get_or_insert_with(denv::init) => {
                                    let args: Vec<String> =
                                        cmd.argv[1..].iter().map(|s| parse::unescape(s)).collect();
                                    if let Some(_path_modified) = denv::command(&args) {
                                        shell.last_status = 0;
                                        true
                                    } else {
                                        false // not allow/deny/reload — fall through to exec
                                    }
                                }
                                "fg" => {
                                    let (status, cont) = exec::resume_job(&mut shell.job);
                                    shell.last_status = status;
                                    run_continuation(&mut shell, cont);
                                    true
                                }
                                "w" | "which" | "type" => {
                                    let args: Vec<String> =
                                        cmd.argv[1..].iter().map(|s| parse::unescape(s)).collect();
                                    if let Some(name) = args.first()
                                        && let Some(exp) = shell.aliases.get(name.as_str())
                                    {
                                        println!("alias: {} {}", name, exp.join(" "));
                                        shell.last_status = 0;
                                        true
                                    } else {
                                        false // fall through to exec for builtin/PATH check
                                    }
                                }
                                _ => false,
                            }
                        } else {
                            false
                        };

                        if handled {
                            if history_line.trim() != "l" {
                                shell.history.add(&history_line);
                            }
                            continue;
                        }

                        let is_cd = first_command_word(&cmdline).as_deref() == Some("cd");

                        shell.last_status = exec::execute(
                            &cmdline,
                            None,
                            &shell.aliases,
                            &mut shell.job,
                            &shell.orig_termios,
                            &shell.home,
                            &mut shell.prev_dir,
                            &mut shell.session_log,
                            shell.last_status,
                        );

                        // Don't record commands that don't exist (127) or
                        // aren't executable (126) — they're just typos.
                        if !matches!(shell.last_status, 126 | 127) && history_line.trim() != "l" {
                            shell.history.add(&history_line);
                        }

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
    let mut cont_stack: Vec<(String, u16, String)> = Vec::new(); // (line_text, rows_above, full_input_before)
    let mut rows_above: u16 = 0;
    let mut line = LineBuffer::new();
    let mut mode = Mode::Normal;
    let mut history_idx: Option<usize> = None;
    let mut saved_line = String::new();
    // Emit OSC 7 so terminal emulators track the working directory
    {
        let pwd = getenv_str(c"PWD");
        if !pwd.is_empty() {
            let hostname = getenv_str(c"HOSTNAME");
            // percent-encode spaces in the path
            let mut osc = format!("\x1b]7;file://{hostname}");
            for b in pwd.bytes() {
                if b == b' ' {
                    osc.push_str("%20");
                } else {
                    osc.push(b as char);
                }
            }
            osc.push('\x07');
            let _ = std::io::Write::write_all(&mut std::io::stdout(), osc.as_bytes());
        }
    }

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
        &render::RenderOpts::default(),
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
                    let info = render::render_line(
                        &mut tw,
                        p,
                        pdl,
                        &line,
                        shell.cols,
                        cursor_row,
                        &render::RenderOpts::default(),
                    );
                    cursor_row = info.cursor_row;
                }
                let _ = tw.flush_to_stdout();
                continue;
            }
            InputEvent::Key(key) => {
                match &mut mode {
                    Mode::Normal => {
                        // Bracketed paste: convert bare newlines to spaces
                        // instead of executing immediately. Continuations
                        // (trailing \, unclosed quotes, trailing operators)
                        // still go through the normal path.
                        let paste_sep = key.key == Key::Enter && reader.in_paste() && {
                            let text = line.text();
                            let combined = if full_input.is_empty() {
                                text.to_string()
                            } else {
                                format!("{full_input} {text}")
                            };
                            !parse::needs_continuation(&combined)
                        };

                        if paste_sep {
                            handle_paste_newline(&mut line, &mut full_input);
                        } else {
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
                                    // Clear autosuggestion ghost text before freezing the line
                                    if line.cursor() == line.text().len() {
                                        tw.write_str("\x1b[J");
                                    }
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
                                    cont_stack.push((text.clone(), rows_above, full_input.clone()));
                                    rows_above += cursor_row + 1;
                                    if full_input.is_empty() {
                                        full_input = text;
                                    } else {
                                        full_input.push(' ');
                                        full_input.push_str(&text);
                                    }
                                    if parse::ends_with_line_continuation(&full_input) {
                                        let end = full_input.trim_end().len();
                                        full_input.truncate(end - 1);
                                    }
                                    line = LineBuffer::new();
                                    history_idx = None;
                                    tw.write_str("\r\n");
                                    let info = render::render_line(
                                        &mut tw,
                                        "  ",
                                        2,
                                        &line,
                                        shell.cols,
                                        0,
                                        &render::RenderOpts::default(),
                                    );
                                    cursor_row = info.cursor_row;
                                    let _ = tw.flush_to_stdout();
                                    continue;
                                }
                                KeyAction::Unwind => {
                                    if let Some((prev_text, prev_rows_above, prev_full_input)) =
                                        cont_stack.pop()
                                    {
                                        let current = line.text().to_string();
                                        let mut prev = prev_text;
                                        // Strip trailing `\` continuation marker
                                        if parse::ends_with_line_continuation(&prev) {
                                            let end = prev.trim_end().len();
                                            prev.truncate(end - 1);
                                        }
                                        let join_pos = prev.len();
                                        if !current.is_empty() {
                                            prev.push(' ');
                                            prev.push_str(&current);
                                        }
                                        line.set_with_cursor(&prev, join_pos);
                                        full_input = prev_full_input;
                                        cursor_row = rows_above + cursor_row - prev_rows_above;
                                        rows_above = prev_rows_above;
                                        history_idx = None;
                                    }
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
                                    rows_above = 0;
                                    for entry in &mut cont_stack {
                                        entry.1 = 0;
                                    }
                                }
                                KeyAction::StartHistorySearch => {
                                    shell.history.sync();
                                    saved_line = line.text().to_string();
                                    let mut matches = std::mem::take(&mut shell.match_buf);
                                    shell.history.fuzzy_search_into("", &mut matches, 200);
                                    mode = Mode::HistorySearch {
                                        query: LineBuffer::new(),
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
                                    let cs = start_completion(
                                        &line,
                                        shell.cols,
                                        &shell.home,
                                        &shell.aliases,
                                        comp,
                                    );
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
                        }

                        match &mode {
                            Mode::Normal => {
                                let (p, pdl) =
                                    active_prompt(&prompt_str, prompt_display_len, &full_input);

                                // Command word coloring: green if valid, red if not
                                let cmd_color = if full_input.is_empty() {
                                    let first = line.text().split_whitespace().next().unwrap_or("");
                                    if first.is_empty() {
                                        None
                                    } else if builtin::is_builtin(first)
                                        || shell.aliases.get(first).is_some()
                                        || first.contains('/')
                                    {
                                        Some(true)
                                    } else {
                                        Some(shell.path_cache.contains(first))
                                    }
                                } else {
                                    None // continuation line — no coloring
                                };

                                // Autosuggestion: gray ghost from history
                                let text = line.text();
                                let suggestion = if text.len() >= 3
                                    && full_input.is_empty()
                                    && line.cursor() == text.len()
                                {
                                    shell
                                        .history
                                        .prefix_search(text, 0)
                                        .and_then(|entry| entry.strip_prefix(text))
                                        .unwrap_or("")
                                } else {
                                    ""
                                };

                                let opts = render::RenderOpts {
                                    cmd_color,
                                    suggestion,
                                };
                                let info = render::render_line(
                                    &mut tw, p, pdl, &line, shell.cols, cursor_row, &opts,
                                );
                                cursor_row = info.cursor_row;
                            }
                            Mode::Completion(state) => {
                                let (p, pdl) =
                                    active_prompt(&prompt_str, prompt_display_len, &full_input);
                                let info = render::render_line(
                                    &mut tw,
                                    p,
                                    pdl,
                                    &line,
                                    shell.cols,
                                    cursor_row,
                                    &render::RenderOpts::default(),
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
                                    &mut tw,
                                    p,
                                    pdl,
                                    &line,
                                    shell.cols,
                                    cursor_row,
                                    &render::RenderOpts::default(),
                                );
                                cursor_row = info.cursor_row;
                                render::render_completions(&mut tw, state, &info, false);
                                let _ = tw.flush_to_stdout();
                                continue;
                            }
                            CompAction::Refilter => {
                                // Reclaim buffer from current state, re-run completion
                                let comp = std::mem::take(&mut state.comp);
                                let cs = start_completion(
                                    &line,
                                    shell.cols,
                                    &shell.home,
                                    &shell.aliases,
                                    comp,
                                );
                                if cs.comp.len() == 1 {
                                    accept_completion(&mut line, &cs);
                                    shell.comp_buf = cs.comp;
                                    mode = Mode::Normal;
                                } else if !cs.comp.is_empty() {
                                    let info = render::render_line(
                                        &mut tw,
                                        p,
                                        pdl,
                                        &line,
                                        shell.cols,
                                        cursor_row,
                                        &render::RenderOpts::default(),
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
                        let info = render::render_line(
                            &mut tw,
                            p,
                            pdl,
                            &line,
                            shell.cols,
                            cursor_row,
                            &render::RenderOpts::default(),
                        );
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
                                &render::RenderOpts::default(),
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
                                &render::RenderOpts::default(),
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
    Unwind,
    Cancel,
    Exit,
    ClearScreen,
    StartHistorySearch,
    StartCompletion,
}

/// Handle a newline during bracketed paste: merge any continuation and
/// insert a space instead of executing.
/// Expand the first word of each ;/&&/||-separated segment through aliases.
/// Used to record expanded commands in history (e.g., "g status" → "git status").
/// Get the first non-assignment command word from a single-command cmdline.
fn first_command_word(cmdline: &parse::CommandLine) -> Option<String> {
    if cmdline.segments.len() != 1 || cmdline.segments[0].0.commands.len() != 1 {
        return None;
    }
    cmdline.segments[0].0.commands[0]
        .cmd
        .argv
        .iter()
        .map(|s| parse::unescape(s))
        .find(|s| exec::var_assignment_pos(s).is_none())
}

/// Join alias expansion words, quoting any that contain whitespace so the
/// result re-parses into the same tokens.
fn shell_quote_join(words: &[String]) -> String {
    let mut result = String::new();
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        if word.contains([' ', '\t', '\n']) {
            result.push('"');
            for c in word.chars() {
                if c == '"' || c == '\\' {
                    result.push('\\');
                }
                result.push(c);
            }
            result.push('"');
        } else {
            result.push_str(word);
        }
    }
    result
}

fn expand_aliases_for_history(line: &str, aliases: &AliasMap) -> String {
    let trimmed = line.trim();
    let first_word = trimmed.split_whitespace().next().unwrap_or("");
    if let Some(expansion) = aliases.get(first_word) {
        let expanded_str = shell_quote_join(expansion);
        if trimmed.starts_with(&expanded_str) {
            return line.to_string();
        }
        let rest = &trimmed[first_word.len()..];
        format!("{expanded_str}{rest}")
    } else {
        line.to_string()
    }
}

fn handle_paste_newline(line: &mut LineBuffer, full_input: &mut String) {
    if !full_input.is_empty() {
        let text = line.text().to_string();
        full_input.push(' ');
        full_input.push_str(&text);
        line.set(full_input);
        *full_input = String::new();
    }
    if !line.text().is_empty() {
        line.insert_char(' ');
    }
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
            if !full_input.is_empty() {
                return KeyAction::Unwind;
            }
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
            if line.cursor() >= line.text().len() && full_input.is_empty() {
                // At end of line — accept autosuggestion from history
                if let Some(entry) = shell.history.prefix_search(line.text(), 0) {
                    let owned = entry.to_string();
                    line.set(&owned);
                }
            } else {
                line.move_right();
            }
        }
        (Key::Home, _, _) => line.move_home(),
        (Key::End, _, _) => line.move_end(),

        (Key::Backspace, true, _) => {
            line.kill_word_back();
            *history_idx = None;
        }
        (Key::Backspace, _, _) => {
            if line.cursor() == 0 && !full_input.is_empty() {
                return KeyAction::Unwind;
            }
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
            hist.local_get(skip)
        } else {
            hist.local_prefix_search(saved_line, skip)
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
                    hist.local_get(skip)
                } else {
                    hist.local_prefix_search(saved_line, skip)
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
    // Only expand on the first space — if the trimmed text already contains
    // a space, the alias was already expanded or the user typed arguments.
    let trimmed = text.trim_end();
    if trimmed.contains(' ') {
        return;
    }

    if let Some(expansion) = aliases.get(trimmed) {
        let rest = &text[trimmed.len()..];
        let expanded_str = shell_quote_join(expansion);
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
            b' ' | b'\t' if !in_single && !in_double => {
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

/// Whether a completion string needs quoting for the shell.
fn needs_quoting(s: &str) -> bool {
    s.bytes().any(|b| {
        matches!(
            b,
            b' ' | b'\t'
                | b'('
                | b')'
                | b'$'
                | b'*'
                | b'?'
                | b'['
                | b']'
                | b'|'
                | b'&'
                | b'>'
                | b'<'
                | b';'
                | b'#'
                | b'\\'
                | b'"'
                | b'`'
        )
    })
}

fn start_completion(
    line: &LineBuffer,
    term_cols: u16,
    home: &str,
    aliases: &AliasMap,
    mut comp: complete::Completions,
) -> CompletionState {
    comp.clear();

    let text = line.text();
    let before_cursor = &text[..line.cursor()];
    let (word_start, in_single) = find_comp_word_start(before_cursor);
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

    // Command position: first word or after |, &&, ;
    if !partial.is_empty()
        && !partial.contains('/')
        && is_command_position(before_cursor, word_start)
    {
        for &b in builtin::ALL_BUILTINS {
            if b.starts_with(partial.as_str()) {
                comp.push(b, false, false, false);
            }
        }
        for (name, _) in aliases.iter() {
            if name.starts_with(partial.as_str()) {
                comp.push(name, false, false, false);
            }
        }
        exec::complete_commands(&partial, &mut comp);
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
            dir_prefix: String::new(),
            in_quote: false,
        };
    }

    // Detect if first word is cd → complete only directories
    let first_word = text.split_whitespace().next().unwrap_or("");
    let dirs_only = first_word == "cd" && word_start > 0;

    // SSH-aware completion: hostname and remote path
    const SSH_CMDS: &[&str] = &["ssh", "scp", "rsync", "sftp", "mosh"];
    if word_start > 0 && SSH_CMDS.contains(&first_word) {
        if let Some(colon_pos) = partial.find(':') {
            // Remote path: host:/path/prefix → complete via SSH
            let host = &partial[..colon_pos];
            let remote_path = &partial[colon_pos + 1..];
            complete::complete_remote_path(host, remote_path, &mut comp);
            if !comp.is_empty() {
                let dir_prefix = if let Some(slash) = remote_path.rfind('/') {
                    format!("{}:{}", host, &remote_path[..=slash])
                } else {
                    format!("{host}:")
                };
                let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
                return CompletionState {
                    comp,
                    selected: 0,
                    cols,
                    rows,
                    scroll: 0,
                    dir_prefix,
                    in_quote,
                };
            }
        } else if !partial.contains('/') {
            // No colon, no slash → hostname completion + local files
            complete::complete_hostnames(&partial, home, &mut comp);
            complete::complete_path_into(&partial, false, &mut comp);
            comp.sort_entries();
            comp.dedup_sorted();
            if !comp.is_empty() {
                let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
                return CompletionState {
                    comp,
                    selected: 0,
                    cols,
                    rows,
                    scroll: 0,
                    dir_prefix: String::new(),
                    in_quote,
                };
            }
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
    if !comp.is_empty() {
        let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
        return CompletionState {
            comp,
            selected: 0,
            cols,
            rows,
            scroll: 0,
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

        let (cols, rows) = complete::compute_grid(&comp.entries, term_cols);
        return CompletionState {
            comp,
            selected: 0,
            cols,
            rows,
            scroll: 0,
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
        dir_prefix: String::new(),
        in_quote,
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
    let (word_start, _) = find_comp_word_start(before_cursor);
    let after_cursor = &text[line.cursor()..];

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
    let new_cursor_chars = text[..word_start].chars().count() + replacement.chars().count();
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
    query: &mut LineBuffer,
    matches: &mut Vec<FuzzyMatch>,
    selected: &mut usize,
    shell: &Shell,
) -> HistAction {
    let mut re_search = false;
    match (key.key, key.mods.ctrl, key.mods.alt) {
        (Key::Escape, _, _) => return HistAction::Cancel,
        (Key::Char('c'), true, _) => return HistAction::Cancel,
        (Key::Enter, _, _) => {
            return if let Some(m) = matches.get(*selected) {
                HistAction::Accept(shell.history.get(m.entry_idx).to_string())
            } else {
                HistAction::Cancel
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
        shell.history.fuzzy_search_into(query.text(), matches, 200);
        *selected = 0;
    }
    HistAction::Continue
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
            query.text(),
            matches,
            &shell.history,
            *selected,
            shell.rows,
            shell.cols,
            query.display_cursor_pos(),
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
        shell.last_status,
    );
    // If the continuation itself suspended, any further continuation is
    // already saved on the job by execute(). Nothing more to do here.
}

/// Rewrite shorthand cd forms:
///   ".." → "cd ..", "..." → "cd ../..", "...." → "cd ../../..", etc.
///   single-arg that is a directory (and not an alias/builtin/executable) → "cd <arg>"
fn maybe_rewrite_cd(line: &str, aliases: &AliasMap) -> String {
    let trimmed = line.trim();
    let mut words = trimmed.split_whitespace();
    let first = match words.next() {
        Some(w) => w,
        None => return line.to_string(),
    };

    // Dot-dot shorthand: ".." is already valid, "..." → "../..", etc.
    if first.len() >= 2 && first.bytes().all(|b| b == b'.') {
        let levels = first.len() - 1; // ".." = 1 level, "..." = 2, etc.
        let path = (0..levels).map(|_| "..").collect::<Vec<_>>().join("/");
        let rest: String = words.collect::<Vec<_>>().join(" ");
        return if rest.is_empty() {
            format!("cd {path}")
        } else {
            format!("cd {path} {rest}")
        };
    }

    // Implicit cd: single arg, not an alias/builtin, is a directory, not an executable
    if words.next().is_none()
        && !builtin::is_builtin(first)
        && aliases.get(first).is_none()
        && !first.contains('|')
        && !first.contains('&')
        && !first.contains(';')
    {
        // Resolve ~ prefix
        let expanded = if let Some(rest) = first.strip_prefix('~') {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}{rest}")
        } else {
            first.to_string()
        };
        let path = std::path::Path::new(&expanded);
        if path.is_dir() && !is_executable(path) {
            return format!("cd {first}");
        }
    }

    line.to_string()
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0 && m.is_file())
        .unwrap_or(false)
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
