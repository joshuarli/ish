use ish::alias::AliasMap;
use ish::complete::CompletionState;
use ish::history::FuzzyMatch;
use ish::input::{InputEvent, InputReader, Key, KeyEvent};
use ish::job::Job;
use ish::line::LineBuffer;
use ish::term::TermWriter;
use ish::{
    builtin, complete, config, denv, finder, frecency, history, path, prompt, render, signal, term,
};
use std::cell::RefCell;
use std::ffi::OsString;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;

// Thread-local storage for a stopped job's info, written by the external handler
// and consumed by the main loop. Avoids needing to thread state through epsh's
// callback boundary.
thread_local! {
    static STOPPED_JOB: RefCell<Option<(libc::pid_t, String, libc::termios)>> =
        const { RefCell::new(None) };
}

struct Shell {
    aliases: AliasMap,
    last_status: i32,
    prev_dir: Option<String>,
    dir_stack: Vec<String>,
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
    path_cache: path::PathCache,
    exit_warned: bool,
    signal_fd: RawFd,
    home: String,
    session_log: String,
    epsh: epsh::eval::Shell,
    shell_pid: i32,
}

enum ReadResult {
    Line(String),
    Exit,
    Empty,
}

enum Mode {
    Normal,
    Completion {
        state: CompletionState,
        base_line: LineBuffer,
    },
    HistorySearch {
        query: LineBuffer,
        matches: Vec<FuzzyMatch>,
        candidates: Vec<usize>,
        scratch: Vec<usize>,
        candidate_stack: Vec<(usize, Vec<usize>)>,
        selected: usize,
        saved_line: String,
    },
    FilePicker {
        query: LineBuffer,
        /// All entries from the background walk (depth, rel_path).
        all_entries: Vec<(usize, String)>,
        /// Filtered + sorted results: indices into `all_entries`.
        filtered: Vec<usize>,
        selected: usize,
        saved_line: String,
        saved_cursor: usize,
        hidden: bool,
        handle: finder::FinderHandle,
    },
    DirPicker {
        entries: Vec<String>,
        selected: usize,
    },
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
    let home = std::env::var_os("HOME")
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let shell_pid = unsafe { libc::getpid() };

    let cwd = std::env::current_dir().unwrap_or_default();
    let epsh = epsh::eval::Shell::builder()
        .cwd(cwd)
        .interactive(true)
        .build();

    let mut shell = Shell {
        aliases: AliasMap::new(),
        last_status: 0,
        prev_dir: None,
        dir_stack: Vec::with_capacity(32),
        rows,
        cols,
        history: history::History::load(),
        prompt: prompt::Prompt::new(),
        prompt_buf: String::with_capacity(128),
        comp_buf: complete::Completions::with_capacity(2048, 64),
        match_buf: Vec::with_capacity(200),
        job: None,
        path_cache: path::PathCache::new(),
        exit_warned: false,
        signal_fd,
        home: home.clone(),
        session_log: String::new(),
        epsh,
        shell_pid,
    };

    // Load config
    if !cli.no_config {
        config::load(&mut shell.aliases, &mut shell.epsh, cli.config.as_deref());
    }

    denv::init();
    let changes = denv::on_startup();
    apply_denv_changes(&changes, &mut shell.epsh);

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
                // Resolve relative cd paths to absolute so `z` can use them.
                let history_line =
                    resolve_cd_for_history(&expand_aliases_for_history(&line, &shell.aliases));

                // Log for session transcript
                shell.session_log.push_str(&line);
                shell.session_log.push('\n');

                // Handle ish-level commands that must be intercepted before epsh
                let first_word = line.split_whitespace().next().unwrap_or("");
                let simple_words = parse_simple_command_words(&line);
                let handled = match first_word {
                    "alias" => {
                        handle_alias(&line, &mut shell);
                        true
                    }
                    "exit" => {
                        if handle_exit_command(&line, &mut shell) {
                            break;
                        }
                        true
                    }
                    "cd" => {
                        if let Some(words) = simple_words.as_deref() {
                            match expand_builtin_args(&words[1..], &mut shell, "cd") {
                                Some(args) if args.len() > 1 => {
                                    eprintln!("ish: cd: too many arguments");
                                    shell.last_status = 1;
                                    true
                                }
                                Some(args) if args.first().map(String::as_str) == Some("-") => {
                                    if let Some(prev) =
                                        std::env::var_os("OLDPWD").filter(|s| !s.is_empty())
                                    {
                                        eprintln!("{}", Path::new(&prev).display());
                                        shell.last_status =
                                            do_cd_path(Path::new(&prev), &mut shell);
                                    } else {
                                        eprintln!("ish: cd: no previous directory");
                                        shell.last_status = 1;
                                    }
                                    true
                                }
                                Some(args) => {
                                    let target =
                                        args.first().cloned().unwrap_or_else(|| shell.home.clone());
                                    shell.last_status = do_cd(&target, &mut shell);
                                    true
                                }
                                None => true,
                            }
                        } else {
                            false
                        }
                    }
                    "history" => handle_history(&line, &mut shell),
                    "copy-scrollback" => {
                        use std::io::Write;
                        let encoded = base64_encode(shell.session_log.as_bytes());
                        let osc = format!("\x1b]52;c;{encoded}\x07");
                        let _ = std::io::stdout().write_all(osc.as_bytes());
                        let _ = std::io::stdout().flush();
                        shell.last_status = 0;
                        true
                    }
                    "denv" => {
                        let args = simple_words
                            .as_deref()
                            .and_then(|words| expand_builtin_args(&words[1..], &mut shell, "denv"))
                            .unwrap_or_else(|| {
                                line.split_whitespace().skip(1).map(String::from).collect()
                            });
                        let outcome = denv::command(&args);
                        apply_denv_changes(&outcome.changes, &mut shell.epsh);
                        shell.last_status = outcome.status;
                        true
                    }
                    "fg" => {
                        shell.last_status = ish::job::resume_job(&mut shell.job);
                        true
                    }
                    "z" => {
                        let args = simple_words
                            .as_deref()
                            .and_then(|words| expand_builtin_args(&words[1..], &mut shell, "z"))
                            .unwrap_or_else(|| {
                                line.split_whitespace().skip(1).map(String::from).collect()
                            });
                        shell.last_status = frecency::builtin_z(&args, &shell.history, &shell.home);
                        if shell.last_status == 0 {
                            // z already did chdir — run post-cd hooks
                            let old_cwd = shell.epsh.cwd().to_path_buf();
                            let new_cwd = std::env::current_dir().unwrap_or_default();
                            sync_cwd_change(&mut shell, &old_cwd, new_cwd);
                        }
                        true
                    }
                    "w" | "which" | "type" => {
                        let args = simple_words
                            .as_deref()
                            .and_then(|words| {
                                expand_builtin_args(&words[1..], &mut shell, first_word)
                            })
                            .unwrap_or_else(|| {
                                line.split_whitespace().skip(1).map(String::from).collect()
                            });
                        shell.last_status =
                            builtin::builtin_w(&args, &shell.aliases, shell.epsh.functions());
                        true
                    }
                    "l" => {
                        if let Some(words) = simple_words.as_deref() {
                            match expand_builtin_args(&words[1..], &mut shell, "l") {
                                Some(args) => {
                                    shell.last_status = builtin::builtin_l(&args);
                                    true
                                }
                                None => true,
                            }
                        } else {
                            false
                        }
                    }
                    "c" => {
                        print!("\x1b[H\x1b[2J");
                        shell.last_status = 0;
                        true
                    }
                    _ => false,
                };

                if handled {
                    if history_line.trim() != "l" {
                        shell.history.add(&history_line);
                    }
                    continue;
                }

                // Expand aliases and run through epsh
                let expanded = shell.aliases.expand_line(&line);

                // If any command in the line is `history`, flush entries
                // to the text file so forked children can read them.
                if expanded.contains("history") {
                    shell.history.flush_for_read();
                }

                // Save cwd before execution to detect cd
                let prev_cwd = shell.epsh.cwd().to_path_buf();

                // Set up external handler for ish-specific builtins
                let shell_pid = shell.shell_pid;
                let handler = make_external_handler(shell_pid);
                shell.epsh.set_external_handler(handler);

                shell.last_status = shell.epsh.run_script(&expanded);

                // Detect cwd change from epsh (compound commands with cd, pushd, etc.)
                if shell.epsh.cwd() != prev_cwd {
                    let new_cwd = shell.epsh.cwd().to_path_buf();
                    let _ = std::env::set_current_dir(&new_cwd);
                    sync_cwd_change(&mut shell, &prev_cwd, new_cwd);
                }

                // Detect job suspension (status 148 = 128 + SIGTSTP)
                if shell.last_status == 148
                    && let Some((pgid, cmd, termios)) =
                        STOPPED_JOB.with(|cell| cell.borrow_mut().take())
                {
                    eprintln!("ish: stopped: {} (pgid={})", cmd, pgid);
                    shell.job = Some(Job { pgid, cmd, termios });
                }

                if history_line.trim() != "l" {
                    shell.history.add(&history_line);
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

    let info = render_prompt_region(
        &mut tw,
        shell,
        &line,
        &prompt_str,
        prompt_display_len,
        render::RenderedRegion::default(),
    );
    let mut region = info;
    let mut history_cache = render::HistoryPagerCache::default();
    let _ = tw.flush_to_stdout();

    loop {
        // In file picker mode, use a short timeout so we periodically drain
        // the background walk channel and re-render as results stream in.
        let event = if matches!(mode, Mode::FilePicker { .. }) {
            match reader.read_event_timeout(50) {
                Some(ev) => ev,
                None => {
                    // Timeout: drain channel, re-filter, re-render
                    if let Mode::FilePicker {
                        query,
                        all_entries,
                        filtered,
                        selected,
                        handle,
                        ..
                    } = &mut mode
                    {
                        let before = all_entries.len();
                        handle.drain_into(all_entries);
                        if all_entries.len() != before {
                            filter_entries(query.text(), all_entries, filtered, selected);
                            region = render_file_picker_mode(&mut tw, &mode, shell, region);
                            let _ = tw.flush_to_stdout();
                        }
                    }
                    continue;
                }
            }
        } else {
            reader.read_event()
        };
        match event {
            InputEvent::Signal(sig) => {
                if sig == libc::SIGWINCH {
                    let (rows, cols) = term::term_size();
                    shell.rows = rows;
                    shell.cols = cols;
                    update_mode_layout_for_resize(&mut mode, shell);
                }
                region = render_active_mode(
                    &mut tw,
                    &mode,
                    shell,
                    &line,
                    &prompt_str,
                    prompt_display_len,
                    region,
                    &mut history_cache,
                );
                let _ = tw.flush_to_stdout();
                continue;
            }
            InputEvent::Key(key) => {
                match &mut mode {
                    Mode::Normal => {
                        // Bracketed paste: insert newlines into the buffer
                        // rather than executing or splitting into continuations.
                        if key.key == Key::Enter && reader.in_paste() {
                            line.insert_char('\n');
                        } else {
                            match handle_normal_key(
                                key,
                                &mut line,
                                &mut history_idx,
                                &mut saved_line,
                                shell,
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
                                    let joined = join_continuation_lines(&text);
                                    return if joined.is_empty() {
                                        ReadResult::Empty
                                    } else {
                                        ReadResult::Line(joined)
                                    };
                                }
                                KeyAction::Continuation => {
                                    line.insert_char('\n');
                                    history_idx = None;
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
                                    region = render::RenderedRegion::default();
                                }
                                KeyAction::StartHistorySearch => {
                                    shell.history.sync();
                                    saved_line = line.text().to_string();
                                    let mut candidates = Vec::new();
                                    shell.history.visible_entry_indices_into(&mut candidates);
                                    let mut scratch = Vec::new();
                                    let mut matches = std::mem::take(&mut shell.match_buf);
                                    shell.history.fuzzy_search_subset_into(
                                        "",
                                        &candidates,
                                        &mut scratch,
                                        &mut matches,
                                        200,
                                    );
                                    std::mem::swap(&mut candidates, &mut scratch);
                                    region.clear(&mut tw);
                                    mode = Mode::HistorySearch {
                                        query: LineBuffer::new(),
                                        matches,
                                        candidates,
                                        scratch,
                                        candidate_stack: Vec::new(),
                                        selected: 0,
                                        saved_line: saved_line.clone(),
                                    };
                                    history_cache.clear();
                                    region = render_history_mode(
                                        &mut tw,
                                        &mode,
                                        shell,
                                        render::RenderedRegion::default(),
                                        &mut history_cache,
                                    );
                                    let _ = tw.flush_to_stdout();
                                    continue;
                                }
                                KeyAction::StartFilePicker => {
                                    saved_line = line.text().to_string();
                                    let handle = finder::find_async(".", false);
                                    region.clear(&mut tw);
                                    mode = Mode::FilePicker {
                                        query: LineBuffer::new(),
                                        all_entries: Vec::new(),
                                        filtered: Vec::new(),
                                        selected: 0,
                                        saved_line: saved_line.clone(),
                                        saved_cursor: line.cursor(),
                                        hidden: false,
                                        handle,
                                    };
                                    region = render_file_picker_mode(
                                        &mut tw,
                                        &mode,
                                        shell,
                                        render::RenderedRegion::default(),
                                    );
                                    let _ = tw.flush_to_stdout();
                                    continue;
                                }
                                KeyAction::StartDirPicker => {
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
                                        // No history yet — stay in normal mode
                                    } else {
                                        region.clear(&mut tw);
                                        mode = Mode::DirPicker {
                                            entries,
                                            selected: 0,
                                        };
                                        region =
                                            render_dir_picker_mode(&mut tw, &mode, shell, region);
                                        let _ = tw.flush_to_stdout();
                                        continue;
                                    }
                                }
                                KeyAction::StartCompletion => {
                                    let comp = std::mem::take(&mut shell.comp_buf);
                                    let base_line = line.clone();
                                    let mut cs = start_completion(
                                        &base_line,
                                        shell.cols,
                                        &shell.home,
                                        &shell.aliases,
                                        comp,
                                    );
                                    if cs.comp.len() == 1 {
                                        preview_completion(&mut line, &cs, &base_line);
                                        shell.comp_buf = cs.comp;
                                    } else if !cs.comp.is_empty() {
                                        cs.selected = usize::MAX;
                                        preview_completion(&mut line, &cs, &base_line);
                                        mode = Mode::Completion {
                                            state: cs,
                                            base_line,
                                        };
                                    } else {
                                        shell.comp_buf = cs.comp;
                                    }
                                }
                            }
                        }

                        region = render_active_mode(
                            &mut tw,
                            &mode,
                            shell,
                            &line,
                            &prompt_str,
                            prompt_display_len,
                            region,
                            &mut history_cache,
                        );
                        let _ = tw.flush_to_stdout();
                    }

                    Mode::Completion { state, base_line } => {
                        let (p, pdl) = active_prompt(&prompt_str, prompt_display_len);
                        match handle_completion_key(key, state, base_line) {
                            CompAction::Navigate => {
                                preview_completion(&mut line, state, base_line);
                                // Cursor is on prompt line — repaint grid in-place
                                let info = render::render_line(
                                    &mut tw,
                                    p,
                                    pdl,
                                    &line,
                                    shell.cols,
                                    region,
                                    &render::RenderOpts::default(),
                                );
                                region = info;
                                render::render_completions(&mut tw, state, info, false);
                                let _ = tw.flush_to_stdout();
                                continue;
                            }
                            CompAction::Refilter => {
                                // Reclaim buffer from current state, re-run completion
                                let selected = state.selected;
                                let comp = std::mem::take(&mut state.comp);
                                let mut cs = start_completion(
                                    base_line,
                                    shell.cols,
                                    &shell.home,
                                    &shell.aliases,
                                    comp,
                                );
                                if !cs.comp.is_empty() {
                                    cs.selected = if selected == usize::MAX {
                                        usize::MAX
                                    } else {
                                        selected.min(cs.comp.len() - 1)
                                    };
                                    preview_completion(&mut line, &cs, base_line);
                                    let info = render::render_line(
                                        &mut tw,
                                        p,
                                        pdl,
                                        &line,
                                        shell.cols,
                                        region,
                                        &render::RenderOpts::default(),
                                    );
                                    region = info;
                                    render::render_completions(&mut tw, &cs, info, true);
                                    mode = Mode::Completion {
                                        state: cs,
                                        base_line: base_line.clone(),
                                    };
                                    let _ = tw.flush_to_stdout();
                                    continue;
                                } else {
                                    line = base_line.clone();
                                    shell.comp_buf = cs.comp;
                                    mode = Mode::Normal;
                                }
                            }
                            CompAction::Accept => {
                                preview_completion(&mut line, state, base_line);
                                shell.comp_buf = std::mem::take(&mut state.comp);
                                mode = Mode::Normal;
                            }
                            CompAction::Cancel => {
                                line = base_line.clone();
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
                            region,
                            &render::RenderOpts::default(),
                        );
                        region = info;
                        let _ = tw.flush_to_stdout();
                    }

                    Mode::HistorySearch {
                        query,
                        matches,
                        candidates,
                        scratch,
                        candidate_stack,
                        selected,
                        saved_line,
                    } => match handle_history_search_key(
                        key,
                        query,
                        matches,
                        candidates,
                        scratch,
                        candidate_stack,
                        selected,
                        shell,
                    ) {
                        HistAction::Continue => {
                            region = render_history_mode(
                                &mut tw,
                                &mode,
                                shell,
                                region,
                                &mut history_cache,
                            );
                            let _ = tw.flush_to_stdout();
                        }
                        HistAction::Accept(text) => {
                            line.set(&text);
                            shell.match_buf = std::mem::take(matches);
                            history_cache.clear();
                            mode = Mode::Normal;
                            region.clear(&mut tw);
                            let info = render::render_line(
                                &mut tw,
                                &prompt_str,
                                prompt_display_len,
                                &line,
                                shell.cols,
                                render::RenderedRegion::default(),
                                &render::RenderOpts::default(),
                            );
                            region = info;
                            let _ = tw.flush_to_stdout();
                        }
                        HistAction::Cancel => {
                            line.set(saved_line);
                            shell.match_buf = std::mem::take(matches);
                            history_cache.clear();
                            mode = Mode::Normal;
                            region.clear(&mut tw);
                            let info = render::render_line(
                                &mut tw,
                                &prompt_str,
                                prompt_display_len,
                                &line,
                                shell.cols,
                                render::RenderedRegion::default(),
                                &render::RenderOpts::default(),
                            );
                            region = info;
                            let _ = tw.flush_to_stdout();
                        }
                    },

                    Mode::FilePicker {
                        query,
                        all_entries,
                        filtered,
                        selected,
                        saved_line,
                        saved_cursor,
                        hidden,
                        handle,
                    } => {
                        // Drain new entries from the background walk
                        handle.drain_into(all_entries);
                        let action = handle_file_picker_key(
                            key,
                            query,
                            all_entries,
                            filtered,
                            selected,
                            hidden,
                            handle,
                        );
                        match action {
                            FilePickerAction::Continue => {
                                region = render_file_picker_mode(&mut tw, &mode, shell, region);
                                let _ = tw.flush_to_stdout();
                            }
                            FilePickerAction::Accept(path) => {
                                // Insert path at the saved cursor position
                                let mut text = saved_line.clone();
                                text.insert_str(*saved_cursor, &path);
                                let new_cursor = *saved_cursor + path.len();
                                line.set_with_cursor(&text, new_cursor);
                                mode = Mode::Normal;
                                region.clear(&mut tw);
                                let info = render::render_line(
                                    &mut tw,
                                    &prompt_str,
                                    prompt_display_len,
                                    &line,
                                    shell.cols,
                                    render::RenderedRegion::default(),
                                    &render::RenderOpts::default(),
                                );
                                region = info;
                                let _ = tw.flush_to_stdout();
                            }
                            FilePickerAction::Cancel => {
                                line.set(saved_line);
                                mode = Mode::Normal;
                                region.clear(&mut tw);
                                let info = render::render_line(
                                    &mut tw,
                                    &prompt_str,
                                    prompt_display_len,
                                    &line,
                                    shell.cols,
                                    render::RenderedRegion::default(),
                                    &render::RenderOpts::default(),
                                );
                                region = info;
                                let _ = tw.flush_to_stdout();
                            }
                        }
                    }

                    Mode::DirPicker { entries, selected } => match (key.key, key.mods.ctrl) {
                        (Key::Escape, _) | (Key::Char('c'), true) => {
                            mode = Mode::Normal;
                            region.clear(&mut tw);
                            let info = render::render_line(
                                &mut tw,
                                &prompt_str,
                                prompt_display_len,
                                &line,
                                shell.cols,
                                render::RenderedRegion::default(),
                                &render::RenderOpts::default(),
                            );
                            region = info;
                            let _ = tw.flush_to_stdout();
                        }
                        (Key::Enter, _) => {
                            if let Some(dir) = entries.get(*selected) {
                                let dir = dir.clone();
                                eprintln!("{dir}");
                                do_cd(&dir, shell);
                            }
                            mode = Mode::Normal;
                            region.clear(&mut tw);
                            let info = render::render_line(
                                &mut tw,
                                &prompt_str,
                                prompt_display_len,
                                &line,
                                shell.cols,
                                render::RenderedRegion::default(),
                                &render::RenderOpts::default(),
                            );
                            region = info;
                            let _ = tw.flush_to_stdout();
                        }
                        (Key::Up, _) if *selected > 0 => {
                            *selected -= 1;
                            region = render_dir_picker_mode(&mut tw, &mode, shell, region);
                            let _ = tw.flush_to_stdout();
                        }
                        (Key::Down, _) if *selected + 1 < entries.len() => {
                            *selected += 1;
                            region = render_dir_picker_mode(&mut tw, &mode, shell, region);
                            let _ = tw.flush_to_stdout();
                        }
                        _ => {}
                    },
                }
            }
        }
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
            Some(shell.path_cache.contains(first))
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
        Mode::Completion { state, .. } => {
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
        Mode::HistorySearch { .. } => render_history_mode(tw, mode, shell, region, history_cache),
        Mode::FilePicker { .. } => render_file_picker_mode(tw, mode, shell, region),
        Mode::DirPicker { .. } => render_dir_picker_mode(tw, mode, shell, region),
    }
}

fn update_mode_layout_for_resize(mode: &mut Mode, shell: &Shell) {
    if let Mode::Completion { state, .. } = mode {
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
    StartFilePicker,
    StartDirPicker,
    StartCompletion,
}

/// Handle a newline during bracketed paste: merge any continuation and
/// insert a space instead of executing.
/// Expand the first word of each ;/&&/||-separated segment through aliases.
/// Used to record expanded commands in history (e.g., "g status" → "git status").
/// Get the first non-assignment command word from a single-command cmdline.
fn expand_aliases_for_history(line: &str, aliases: &AliasMap) -> String {
    aliases.expand_line(line).into_owned()
}

/// Resolve relative cd/z paths to absolute for history, so `z` can match them.
/// "cd src" → "cd /Users/josh/d/ish/src", "cd ~/d" stays as-is.
fn resolve_cd_for_history(line: &str) -> String {
    let trimmed = line.trim();
    let (prefix, rest) = if let Some(r) = trimmed.strip_prefix("cd ") {
        ("cd ", r.trim_start())
    } else if let Some(r) = trimmed.strip_prefix("z ") {
        ("z ", r.trim_start())
    } else {
        return line.to_string();
    };

    // Already absolute or tilde-prefixed — no resolution needed
    if rest.starts_with('/') || rest.starts_with('~') || rest == "-" || rest.is_empty() {
        return line.to_string();
    }

    // Split at first whitespace/operator to get just the path argument
    let (path_arg, suffix) =
        match rest.find(|c: char| c.is_whitespace() || c == '&' || c == '|' || c == ';') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };

    // Resolve relative to PWD
    if let Ok(pwd) = std::env::current_dir() {
        let resolved = pwd.join(path_arg);
        if let Ok(canonical) = resolved.canonicalize() {
            return format!("{prefix}{}{suffix}", canonical.display());
        }
        // canonicalize failed (dir doesn't exist yet?) — use joined path
        return format!("{prefix}{}{suffix}", resolved.display());
    }

    line.to_string()
}

/// Join continuation lines for execution: strip `\<newline>` sequences,
/// replace remaining newlines with spaces.
fn join_continuation_lines(input: &str) -> String {
    if !input.contains('\n') {
        return input.to_string();
    }
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            // Line continuation: skip \ and \n
            i += 2;
        } else if bytes[i] == b'\n' {
            result.push(' ');
            i += 1;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

fn handle_normal_key(
    key: KeyEvent,
    line: &mut LineBuffer,
    history_idx: &mut Option<usize>,
    saved_line: &mut String,
    shell: &Shell,
) -> KeyAction {
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
        (Key::Char('f'), true, _) => return KeyAction::StartFilePicker,

        (Key::Char('b'), false, true) => line.move_word_left(),
        (Key::Char('f'), false, true) => line.move_word_right(),

        (Key::Up, _, _) if !key.mods.ctrl => {
            if line.has_newlines() && !line.on_first_line() {
                line.move_line_up();
            } else {
                navigate_history(line, history_idx, saved_line, shell, true);
            }
        }
        (Key::Down, _, _) if !key.mods.ctrl => {
            if line.has_newlines() && !line.on_last_line() {
                line.move_line_down();
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
            let text = join_continuation_lines(line.text());
            if needs_continuation(&text) {
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
    // Only expand on the first space — if the trimmed text already contains
    // a space, the alias was already expanded or the user typed arguments.
    let trimmed = text.trim_end();
    if trimmed.contains(' ') {
        return;
    }

    let expanded = aliases.expand_line(text);
    if let std::borrow::Cow::Owned(new_text) = expanded {
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
        path::complete_commands(&partial, &mut comp);
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
                    term_cols,
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
                    term_cols,
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

enum HistAction {
    Continue,
    Accept(String),
    Cancel,
}

fn handle_history_search_key(
    key: KeyEvent,
    query: &mut LineBuffer,
    matches: &mut Vec<FuzzyMatch>,
    candidates: &mut Vec<usize>,
    scratch: &mut Vec<usize>,
    candidate_stack: &mut Vec<(usize, Vec<usize>)>,
    selected: &mut usize,
    shell: &Shell,
) -> HistAction {
    let mut re_search = false;
    let prev_text = query.text().to_string();
    let prev_cursor = query.cursor();
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
        let new_text = query.text();
        let append_at_end = prev_cursor == prev_text.len()
            && query.cursor() == new_text.len()
            && new_text.len() > prev_text.len()
            && new_text.starts_with(&prev_text);
        let delete_at_end = prev_cursor == prev_text.len()
            && query.cursor() == new_text.len()
            && new_text.len() < prev_text.len()
            && prev_text.starts_with(new_text);
        if append_at_end {
            candidate_stack.push((prev_text.len(), std::mem::take(candidates)));
            shell.history.fuzzy_search_subset_into(
                new_text,
                &candidate_stack.last().unwrap().1,
                scratch,
                matches,
                200,
            );
            std::mem::swap(candidates, scratch);
        } else if delete_at_end {
            while candidate_stack
                .last()
                .is_some_and(|(len, _)| *len > new_text.len())
            {
                if let Some((_, old_candidates)) = candidate_stack.pop() {
                    *scratch = old_candidates;
                }
            }
            if let Some((len, _)) = candidate_stack.last()
                && *len == new_text.len()
            {
                let (_, old_candidates) = candidate_stack.pop().unwrap();
                *candidates = old_candidates;
                shell
                    .history
                    .fuzzy_search_subset_into(new_text, candidates, scratch, matches, 200);
            } else {
                candidate_stack.clear();
                shell.history.visible_entry_indices_into(scratch);
                shell
                    .history
                    .fuzzy_search_subset_into(new_text, scratch, candidates, matches, 200);
                std::mem::swap(candidates, scratch);
            }
        } else {
            candidate_stack.clear();
            shell.history.visible_entry_indices_into(scratch);
            shell
                .history
                .fuzzy_search_subset_into(new_text, scratch, candidates, matches, 200);
            std::mem::swap(candidates, scratch);
        }
        *selected = 0;
    }
    HistAction::Continue
}

fn render_history_mode(
    tw: &mut TermWriter,
    mode: &Mode,
    shell: &Shell,
    prev: render::RenderedRegion,
    cache: &mut render::HistoryPagerCache,
) -> render::RenderedRegion {
    if let Mode::HistorySearch {
        query,
        matches,
        selected,
        ..
    } = mode
    {
        render::render_history_pager_cached(
            tw,
            query.text(),
            matches,
            &shell.history,
            *selected,
            shell.rows,
            shell.cols,
            query.display_cursor_pos(),
            prev,
            cache,
        )
    } else {
        render::RenderedRegion::default()
    }
}

// ---------------------------------------------------------------------------
// File picker (Ctrl+F)
// ---------------------------------------------------------------------------

enum FilePickerAction {
    Continue,
    Accept(String),
    Cancel,
}

fn handle_file_picker_key(
    key: KeyEvent,
    query: &mut LineBuffer,
    all_entries: &mut Vec<(usize, String)>,
    filtered: &mut Vec<usize>,
    selected: &mut usize,
    hidden: &mut bool,
    handle: &mut finder::FinderHandle,
) -> FilePickerAction {
    let mut refilter = false;

    match (key.key, key.mods.ctrl, key.mods.alt) {
        (Key::Escape, _, _) | (Key::Char('c'), true, _) => return FilePickerAction::Cancel,

        // Accept selected result
        (Key::Enter, _, _) => {
            return if let Some(&entry_idx) = filtered.get(*selected) {
                FilePickerAction::Accept(all_entries[entry_idx].1.clone())
            } else {
                FilePickerAction::Cancel
            };
        }

        // Navigate results
        (Key::Up, _, _) | (Key::Char('p'), true, _) if *selected > 0 => {
            *selected -= 1;
        }
        (Key::Down, _, _) | (Key::Char('n'), true, _)
            if !filtered.is_empty() && *selected + 1 < filtered.len() =>
        {
            *selected += 1;
        }

        // Hidden mode toggle (Ctrl+F again)
        (Key::Char('f'), true, _) => {
            *hidden = !*hidden;
            // Restart the walk with new hidden setting
            handle.stop();
            all_entries.clear();
            filtered.clear();
            *selected = 0;
            *handle = finder::find_async(".", *hidden);
        }

        // Query editing
        (Key::Backspace, _, _) => {
            query.delete_back();
            refilter = true;
        }
        (Key::Delete, true, _) | (Key::Char('d'), false, true) => {
            query.kill_word_back();
            refilter = true;
        }
        (Key::Delete, _, _) | (Key::Char('d'), true, _) => {
            query.delete_forward();
            refilter = true;
        }
        (Key::Left, _, false) if key.mods.ctrl => query.move_word_left(),
        (Key::Right, _, false) if key.mods.ctrl => query.move_word_right(),
        (Key::Left, _, _) => {
            query.move_left();
        }
        (Key::Right, _, _) => {
            query.move_right();
        }
        (Key::Home, _, _) | (Key::Char('a'), true, _) => query.move_home(),
        (Key::End, _, _) | (Key::Char('e'), true, _) => query.move_end(),
        (Key::Char('u'), true, _) => {
            query.kill_to_start();
            refilter = true;
        }
        (Key::Char('k'), true, _) => {
            query.kill_to_end();
            refilter = true;
        }
        (Key::Char('w'), true, _) => {
            query.kill_word_back();
            refilter = true;
        }
        (Key::Char('y'), true, _) => {
            query.yank();
            refilter = true;
        }
        (Key::Char(c), false, false) => {
            query.insert_char(c);
            refilter = true;
        }
        _ => {}
    }

    if refilter {
        filter_entries(query.text(), all_entries, filtered, selected);
    }

    FilePickerAction::Continue
}

fn filter_entries(
    query: &str,
    all_entries: &[(usize, String)],
    filtered: &mut Vec<usize>,
    selected: &mut usize,
) {
    finder::filter_entries_pub(query, all_entries, filtered, selected);
}

fn render_file_picker_mode(
    tw: &mut TermWriter,
    mode: &Mode,
    shell: &Shell,
    prev: render::RenderedRegion,
) -> render::RenderedRegion {
    if let Mode::FilePicker {
        query,
        all_entries,
        filtered,
        selected,
        hidden,
        ..
    } = mode
    {
        let query_text = query.text();
        let in_query = query_text.len() < 3;
        render::render_file_picker(
            tw,
            query_text,
            all_entries,
            filtered,
            *selected,
            shell.rows,
            shell.cols,
            query.display_cursor_pos(),
            in_query,
            *hidden,
            prev,
        )
    } else {
        render::RenderedRegion::default()
    }
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
}

fn sync_cwd_change(shell: &mut Shell, old_cwd: &Path, new_cwd: std::path::PathBuf) {
    sync_path_var(&mut shell.epsh, "OLDPWD", old_cwd);
    sync_path_var(&mut shell.epsh, "PWD", &new_cwd);
    shell.prev_dir = Some(old_cwd.to_string_lossy().into_owned());
    shell.epsh.set_cwd(new_cwd);
    push_dir_stack(&mut shell.dir_stack);
    shell.prompt.invalidate_git();
    let changes = denv::on_cd();
    apply_denv_changes(&changes, &mut shell.epsh);
}

/// Parse a command line into shell words only.
/// Returns `None` if the line contains operators, redirections, or lexer errors.
fn parse_simple_command_words(line: &str) -> Option<Vec<epsh::ast::Word>> {
    let mut lex = epsh::lexer::Lexer::new(line);
    lex.recognize_reserved = false;

    let mut words = Vec::new();
    loop {
        match lex.next_token() {
            Ok((epsh::lexer::Token::Word(parts, _), span)) => {
                words.push(epsh::ast::Word { parts, span });
            }
            Ok((epsh::lexer::Token::Eof, _)) => break,
            Ok(_) | Err(_) => return None,
        }
    }

    (!words.is_empty()).then_some(words)
}

/// Expand intercepted builtin arguments with epsh so top-level builtins honor
/// quoting, variables, command substitution, and globbing like normal commands.
fn expand_builtin_args(
    words: &[epsh::ast::Word],
    shell: &mut Shell,
    name: &str,
) -> Option<Vec<String>> {
    let mut args = Vec::new();
    for word in words {
        match epsh::expand::expand_word_to_fields(word, &mut shell.epsh) {
            Ok(fields) => args.extend(fields),
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
fn do_cd(target: &str, shell: &mut Shell) -> i32 {
    // Resolve ~ prefix
    let resolved = if target == "~" || target.is_empty() {
        shell.home.clone()
    } else if let Some(rest) = target.strip_prefix("~/") {
        format!("{}/{rest}", shell.home)
    } else {
        target.to_string()
    };

    do_cd_path(Path::new(&resolved), shell)
}

fn do_cd_path(target: &Path, shell: &mut Shell) -> i32 {
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
    mode: &Mode,
    shell: &Shell,
    prev: render::RenderedRegion,
) -> render::RenderedRegion {
    if let Mode::DirPicker { entries, selected } = mode {
        render::render_dir_picker(
            tw,
            entries,
            *selected,
            &shell.home,
            shell.rows,
            shell.cols,
            prev,
        )
    } else {
        render::RenderedRegion::default()
    }
}

/// Handle the "exit" command typed at the prompt. Returns true if the shell should break.
fn handle_exit_command(line: &str, shell: &mut Shell) -> bool {
    if shell.job.is_some() {
        if shell.exit_warned {
            if let Some(job) = shell.job.take() {
                unsafe {
                    libc::killpg(job.pgid, libc::SIGTERM);
                }
            }
            return true; // break
        } else {
            eprintln!("ish: there is a suspended job. Exit again to force quit.");
            shell.exit_warned = true;
            shell.last_status = 1;
            return false;
        }
    }
    let code: i32 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    shell.history.compact();
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
            for i in 0..shell.history.len() {
                println!("{}", shell.history.get(i));
            }
            shell.last_status = 0;
            true
        }
        Some("compact") => {
            shell.history.compact();
            shell.last_status = 0;
            true
        }
        Some("rebuild") => {
            shell.history.rebuild();
            shell.last_status = 0;
            true
        }
        Some(other) => {
            eprintln!("ish: history: unknown subcommand: {other}");
            shell.last_status = 1;
            true
        }
    }
}

/// Build the external handler for epsh. Handles ish-specific builtins
/// and fork/exec with job control for external commands.
fn make_external_handler(shell_pid: i32) -> epsh::eval::ExternalHandler {
    Box::new(
        move |args: &[epsh::shell_bytes::ShellBytes],
              env_pairs: &[(String, epsh::shell_bytes::ShellBytes)]| {
            let name = args[0].to_shell_string();

            // ish interactive builtins
            match name.as_str() {
                "l" => {
                    let l_args: Vec<String> =
                        args[1..].iter().map(|arg| arg.to_shell_string()).collect();
                    let status = builtin::builtin_l(&l_args);
                    return Ok(epsh::error::ExitStatus::from(status));
                }
                "c" => {
                    print!("\x1b[H\x1b[2J");
                    return Ok(epsh::error::ExitStatus::SUCCESS);
                }
                "history" => {
                    // In a pipeline context — read from text file
                    let path = if let Some(home) = std::env::var_os("HOME") {
                        std::path::PathBuf::from(home).join(".local/share/ish/history")
                    } else {
                        std::path::PathBuf::from("/tmp/ish_history")
                    };
                    match history::render_history_file(&path) {
                        Ok(content) => {
                            print!("{content}");
                            return Ok(epsh::error::ExitStatus::SUCCESS);
                        }
                        Err(e) => {
                            eprintln!("ish: history: {e}");
                            return Ok(epsh::error::ExitStatus::FAILURE);
                        }
                    }
                }
                _ => {}
            }

            // External command: fork/exec with job control
            let is_main = unsafe { libc::getpid() } == shell_pid;

            let mut cmd = std::process::Command::new(args[0].to_os_string());
            cmd.args(args[1..].iter().map(|arg| arg.to_os_string()));

            // Apply prefix env assignments
            for (k, v) in env_pairs {
                cmd.env(k, v.to_os_string());
            }

            if is_main {
                // In the child: become its own process group leader and restore
                // signal dispositions that the shell overrode (especially SIGTSTP,
                // which the shell ignores — children must inherit SIG_DFL or
                // Ctrl+Z will never stop them).
                //
                // setpgid(0, 0) here (in the child, before exec) is race-free;
                // the parent's setpgid(child_id, child_id) after spawn() is a
                // belt-and-suspenders duplicate for the parent-side view.
                unsafe {
                    cmd.pre_exec(|| {
                        libc::setpgid(0, 0);
                        ish::signal::restore_defaults();
                        Ok(())
                    });
                }
            }

            match cmd.spawn() {
                Ok(mut child) => {
                    let child_id = child.id() as i32;

                    if is_main {
                        // Parent: duplicate setpgid (child did it too — no race)
                        // and hand the terminal to the new process group.
                        unsafe {
                            libc::setpgid(child_id, child_id);
                            libc::tcsetpgrp(0, child_id);
                        }

                        // Wait with WUNTRACED for job control
                        let mut status = 0i32;
                        unsafe {
                            libc::waitpid(child_id, &mut status, libc::WUNTRACED);
                        }

                        // Reclaim terminal
                        unsafe {
                            libc::tcsetpgrp(0, libc::getpgrp());
                        }

                        if libc::WIFSTOPPED(status) {
                            // Capture the terminal state the stopped process left behind,
                            // then save it in the thread-local so the main loop can build a Job.
                            let mut saved_termios: libc::termios = unsafe { std::mem::zeroed() };
                            unsafe { libc::tcgetattr(0, &mut saved_termios) };
                            let cmd = args[0].to_shell_string();
                            STOPPED_JOB.with(|cell| {
                                *cell.borrow_mut() = Some((child_id, cmd, saved_termios));
                            });
                            Err(epsh::error::ShellError::Stopped {
                                pid: child_id,
                                pgid: child_id,
                            })
                        } else if libc::WIFEXITED(status) {
                            Ok(epsh::error::ExitStatus::from(libc::WEXITSTATUS(status)))
                        } else if libc::WIFSIGNALED(status) {
                            Ok(epsh::error::ExitStatus::from(128 + libc::WTERMSIG(status)))
                        } else {
                            Ok(epsh::error::ExitStatus::FAILURE)
                        }
                    } else {
                        // Pipeline child: just wait normally
                        match child.wait() {
                            Ok(s) => Ok(epsh::error::ExitStatus::from(s.code().unwrap_or(128))),
                            Err(e) => Err(epsh::error::ShellError::Io(e)),
                        }
                    }
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        eprintln!("{name}: not found");
                        Ok(epsh::error::ExitStatus::NOT_FOUND)
                    } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                        eprintln!("{name}: permission denied");
                        Ok(epsh::error::ExitStatus::NOT_EXECUTABLE)
                    } else {
                        Err(epsh::error::ShellError::Io(e))
                    }
                }
            }
        },
    )
}

/// Check if input needs a continuation line (open quotes, trailing operator, etc.)
fn needs_continuation(input: &str) -> bool {
    let trimmed = input.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    let (in_single, in_double, escape) = scan_quote_state(trimmed.as_bytes());
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

/// Rewrite shorthand cd forms:
///   ".." → "cd ..", "..." → "cd ../..", "...." → "cd ../../..", etc.
///   single-arg that is a directory (and not an alias/builtin/executable) → "cd <arg>"
fn maybe_rewrite_cd(line: &str, aliases: &AliasMap) -> String {
    let trimmed = line.trim();
    let words = match parse_simple_command_words(trimmed) {
        Some(words) => words,
        None => return line.to_string(),
    };
    let first = match words.first() {
        Some(word) => word,
        None => return line.to_string(),
    };
    let first_text = epsh::lexer::parts_to_text(&first.parts);
    let first_quoted = epsh::lexer::parts_have_quoting(&first.parts);

    // Plain cd commands pass through — intercepted in main loop
    if !first_quoted && first_text == "cd" {
        return line.to_string();
    }

    // Dot-dot shorthand: ".." is already valid, "..." → "../..", etc.
    if words.len() == 1
        && !first_quoted
        && first_text.len() >= 2
        && first_text.bytes().all(|b| b == b'.')
    {
        let levels = first_text.len() - 1; // ".." = 1 level, "..." = 2, etc.
        let path = (0..levels).map(|_| "..").collect::<Vec<_>>().join("/");
        return format!("cd {path}");
    }

    // Implicit cd: single word that is a directory (and not a builtin/alias/executable).
    if words.len() != 1 || builtin::is_builtin(&first_text) || aliases.get(&first_text).is_some() {
        return line.to_string();
    }

    let expanded = if let Some(rest) = first_text.strip_prefix('~') {
        let home = std::env::var_os("HOME")
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{home}{rest}")
    } else {
        first_text
    };
    let path = std::path::Path::new(&expanded);
    if path.is_dir() && !is_executable(path) {
        return format!("cd {trimmed}");
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
