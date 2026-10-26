use crate::alias::AliasMap;
use crate::complete::{CompletionState, needs_quoting};
use crate::history::FuzzyMatch;
use crate::input::{Key, KeyEvent};
use crate::line::LineBuffer;
use crate::mode::{CompletionMode, DirPicker, HistorySearch, Mode};
use crate::term::TermWriter;
use crate::{builtin, complete, diagnostics, history, path, render};
use std::path::Path;

/// Borrowed shell data used for editing; runtime ownership stays with the caller.
pub struct EditorContext<'a> {
    pub aliases: &'a AliasMap,
    pub history: &'a history::History,
    pub cwd: &'a Path,
    pub home: Option<&'a str>,
    pub home_display: &'a str,
    pub path: &'a [u8],
    pub directories: &'a [String],
}

/// A measured input event; no terminal polling or measurement occurs in the editor.
pub enum EditorEvent<'a> { Key(KeyEvent), Paste(&'a str), PasteRejected, Resize { rows: u16, cols: u16 } }

/// A requested runtime operation produced without performing its effects.
pub enum EditorAction { Continue, Submit(String), Cancel, Exit, Complete, SearchHistory, ChangeDirectory(String), Dump }

/// Reusable line, mode, layout and completion/search buffers.
pub struct Editor { view: EditView, mode: Mode, state: EditorState }

struct EditorState {
    rows: u16,
    cols: u16,
    completion_buffer: complete::Completions,
    history_match_buffer: Vec<FuzzyMatch>,
    path_cache: path::PathCache,
}

struct Editing<'a> {
    aliases: &'a AliasMap,
    history: &'a history::History,
    cwd: &'a Path,
    home: Option<&'a str>,
    home_display: &'a str,
    path: &'a [u8],
    dir_stack: &'a [String],
    rows: u16,
    cols: u16,
    completion_buffer: &'a mut complete::Completions,
    history_match_buffer: &'a mut Vec<FuzzyMatch>,
    path_cache: &'a mut path::PathCache,
}
impl EditorState {
    fn editing<'a>(&'a mut self, context: &'a EditorContext<'_>) -> Editing<'a> {
        Editing { aliases: context.aliases, history: context.history, cwd: context.cwd, home: context.home, home_display: context.home_display, path: context.path, dir_stack: context.directories,
            rows: self.rows, cols: self.cols, completion_buffer: &mut self.completion_buffer, history_match_buffer: &mut self.history_match_buffer, path_cache: &mut self.path_cache }
    }
}

impl Editor {
    /// Allocate reusable editor buffers for measured terminal dimensions.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            view: EditView { tw: TermWriter::new(), line: LineBuffer::new(), history_idx: None, saved_line: String::new(), grid_column: None, prompt_str: String::new(), prompt_display_len: 0, region: render::RenderedRegion::default(), history_cache: render::HistoryPagerCache::default() },
            mode: Mode::Normal,
            state: EditorState { rows, cols, completion_buffer: complete::Completions::with_capacity(2048, 64), history_match_buffer: Vec::with_capacity(200), path_cache: path::PathCache::new() },
        }
    }
    /// Start an empty input line with an owned prompt while retaining reusable caches.
    pub fn begin(&mut self, prompt: String, display_len: usize, context: &EditorContext<'_>) {
        self.view.line = LineBuffer::new();
        self.view.history_idx = None;
        self.view.saved_line.clear();
        self.view.grid_column = None;
        self.view.prompt_str = prompt;
        self.view.prompt_display_len = display_len;
        self.view.region = render::RenderedRegion::default();
        self.view.history_cache.clear();
        self.mode = Mode::Normal;
        repaint(&mut self.view, &self.mode, &mut self.state.editing(context));
    }
    /// Return the prompt buffer to the runtime after finishing an input line.
    pub fn take_prompt(&mut self) -> String { std::mem::take(&mut self.view.prompt_str) }
    /// Write queued rendering to the supplied sink; the caller controls flushing.
    pub fn write_output(&mut self, output: &mut impl std::io::Write) -> std::io::Result<()> {
        output.write_all(self.view.tw.as_bytes())?;
        self.view.tw.clear_buffer();
        Ok(())
    }
    /// Resolve a completion request in process using the borrowed shell context.
    pub fn complete(&mut self, context: &EditorContext<'_>) {
        let mut editing = self.state.editing(context);
        start_completion_mode(&mut self.view, &mut self.mode, &mut editing);
        repaint(&mut self.view, &self.mode, &mut editing);
    }
    /// Open search over the history snapshot supplied after any runtime synchronization.
    pub fn search_history(&mut self, context: &EditorContext<'_>) {
        start_history_search(&mut self.view, &mut self.mode, &mut self.state.editing(context));
    }
    /// Render the current editor layout into a reusable string without writing a file.
    pub fn render_layout_dump(&self, output: &mut String, context: &EditorContext<'_>) {
        let line = &self.view.line;
        let suggestion = if line.text().len() >= 3 && !line.has_newlines() && line.cursor() == line.text().len() {
            context.history.session_prefix_search(line.text(), 0).and_then(|entry| entry.strip_prefix(line.text()))
        } else { None };
        diagnostics::render_layout_dump(output, diagnostics::LayoutSnapshot { rows: self.state.rows, cols: self.state.cols, prompt_display_len: self.view.prompt_display_len, line, suggestion_display_len: suggestion.map(crate::line::str_width), region: self.view.region, mode: &self.mode });
    }
    /// Update editor state and buffer rendering, returning any requested runtime effect.
    pub fn handle(&mut self, event: EditorEvent<'_>, context: &EditorContext<'_>) -> EditorAction {
        match event {
            EditorEvent::Resize { rows, cols } => {
                self.state.rows = rows; self.state.cols = cols;
                self.view.grid_column = None;
                update_mode_layout_for_resize(&mut self.mode, &self.state.editing(context));
                self.view.tw.clear_screen();
                self.view.region = render::RenderedRegion::default();
                self.view.history_cache.clear();
            }
            EditorEvent::PasteRejected => { self.view.grid_column = None; self.view.tw.write_str("\x07"); return EditorAction::Continue; }
            EditorEvent::Paste(text) => {
                let mut editing = self.state.editing(context);
                match &mut self.mode {
                    Mode::Completion(completion) => {
                        completion.base_line.insert_str(text);
                        if let Next::Normal = apply_completion_action(&mut self.view, &mut editing, completion, CompletionAction::Refilter) { self.mode = Mode::Normal; }
                    }
                    Mode::DirPicker(_) => {}
                    _ => handle_paste(&mut self.view, &mut self.mode, &mut editing, text),
                }
                return EditorAction::Continue;
            }
            EditorEvent::Key(key) => {
                if key.key == Key::Char('p') && key.mods.ctrl { return EditorAction::Dump; }
                let mut editing = self.state.editing(context);
                let next = match &mut self.mode {
                    Mode::Normal => {
                        let view = &mut self.view;
                        match handle_normal_key(key, &mut view.line, &mut view.history_idx, &mut view.saved_line, &mut view.grid_column, view.prompt_display_len, &editing) {
                            KeyAction::Continue => {}
                            KeyAction::Execute(text) => { if view.line.cursor() == view.line.text().len() { view.tw.write_str("\x1b[J"); } view.tw.write_str("\r\n"); return EditorAction::Submit(text); }
                            KeyAction::Continuation => { view.line.insert_char('\n'); view.history_idx = None; }
                            KeyAction::Cancel => { view.tw.write_str("^C\r\n"); return EditorAction::Cancel; }
                            KeyAction::Exit => { view.tw.write_str("\r\n"); return EditorAction::Exit; }
                            KeyAction::ClearScreen => { view.tw.clear_screen(); view.region = render::RenderedRegion::default(); }
                            KeyAction::StartHistorySearch => return EditorAction::SearchHistory,
                            KeyAction::StartCompletion => return EditorAction::Complete,
                            KeyAction::StartDirPicker => { if start_dir_picker(view, &mut self.mode, &editing) { return EditorAction::Continue; } }
                        }
                        repaint(&mut self.view, &self.mode, &mut editing);
                        return EditorAction::Continue;
                    }
                    Mode::Completion(completion) => handle_completion_key_event(&mut self.view, &mut editing, key, completion),
                    Mode::HistorySearch(search) => handle_history_key_event(&mut self.view, &mut editing, key, search),
                    Mode::DirPicker(picker) => {
                        if key.key == Key::Enter {
                            let action = picker.entries.get(picker.selected).cloned().map(EditorAction::ChangeDirectory).unwrap_or(EditorAction::Continue);
                            handle_dir_picker_key_event(&mut self.view, &mut editing, key, picker);
                            self.mode = Mode::Normal;
                            return action;
                        }
                        handle_dir_picker_key_event(&mut self.view, &mut editing, key, picker)
                    }
                };
                if let Next::Normal = next { self.mode = Mode::Normal; }
                return EditorAction::Continue;
            }
        }
        repaint(&mut self.view, &self.mode, &mut self.state.editing(context));
        EditorAction::Continue
    }
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


fn repaint(view: &mut EditView, mode: &Mode, shell: &mut Editing<'_>) {
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

}

fn redraw_prompt_line(view: &mut EditView, shell: &Editing<'_>) {
    view.region = render::render_line(
        &mut view.tw,
        &view.prompt_str,
        view.prompt_display_len,
        &view.line,
        shell.cols,
        render::RenderedRegion::default(),
        &render::RenderOpts::default(),
    );

}


fn handle_paste(view: &mut EditView, mode: &mut Mode, shell: &mut Editing<'_>, text: &str) {
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


fn start_history_search(view: &mut EditView, mode: &mut Mode, shell: &mut Editing<'_>) {
    view.saved_line = view.line.text().to_string();
    let mut candidates = Vec::new();
    shell.history.search_entry_indices_into(&mut candidates);
    let mut scratch = Vec::new();
    let mut matches = std::mem::take(shell.history_match_buffer);
    shell.history.fuzzy_search_subset_into_in_dir(
        "",
        &candidates,
        &mut scratch,
        &mut matches,
        200,
        shell.cwd,
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

}

fn start_dir_picker(view: &mut EditView, mode: &mut Mode, shell: &Editing<'_>) -> bool {
    let pwd = shell.cwd.to_string_lossy();
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

    true
}

fn start_completion_mode(view: &mut EditView, mode: &mut Mode, shell: &mut Editing<'_>) {
    let comp = std::mem::take(shell.completion_buffer);
    let base_line = view.line.clone();
    let mut cs = start_completion(
        &base_line,
        shell.cols,
        shell.rows,
        shell.home,
        shell.aliases,
        shell.path,
        shell.cwd,
        &mut view.tw,
        comp,
    );
    if cs.comp.len() == 1 {
        preview_completion(&mut view.line, &cs, &base_line);
        *shell.completion_buffer = cs.comp;
    } else if !cs.comp.is_empty() {
        cs.selected = usize::MAX;
        preview_completion(&mut view.line, &cs, &base_line);
        *mode = Mode::Completion(CompletionMode {
            state: cs,
            base_line,
        });
    } else {
        *shell.completion_buffer = cs.comp;
    }
}

fn handle_completion_key_event(
    view: &mut EditView,
    shell: &mut Editing<'_>,
    key: KeyEvent,
    completion: &mut CompletionMode,
) -> Next {
    let action = handle_completion_key(key, &mut completion.state, &mut completion.base_line);
    apply_completion_action(view, shell, completion, action)
}

fn apply_completion_action(view: &mut EditView, shell: &mut Editing<'_>, completion: &mut CompletionMode, action: CompletionAction) -> Next {
    let CompletionMode { state, base_line } = completion;
    let (p, pdl) = active_prompt(&view.prompt_str, view.prompt_display_len);
    let next = match action {
        CompletionAction::Navigate => {
            preview_completion(&mut view.line, state, base_line);
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

            return Next::Stay;
        }
        CompletionAction::Refilter => {
            let selected = state.selected;
            let comp = std::mem::take(&mut state.comp);
            let mut cs = start_completion(
                base_line,
                shell.cols,
                shell.rows,
                shell.home,
                shell.aliases,
                shell.path,
                shell.cwd,
                &mut view.tw,
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

                return Next::Stay;
            } else {
                view.line = base_line.clone();
                *shell.completion_buffer = cs.comp;
                Next::Normal
            }
        }
        CompletionAction::Accept => {
            preview_completion(&mut view.line, state, base_line);
            *shell.completion_buffer = std::mem::take(&mut state.comp);
            Next::Normal
        }
        CompletionAction::Cancel => {
            view.line = base_line.clone();
            *shell.completion_buffer = std::mem::take(&mut state.comp);
            Next::Normal
        }
    };

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

    next
}

fn handle_history_key_event(
    view: &mut EditView,
    shell: &mut Editing<'_>,
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

fn leave_history_search(view: &mut EditView, shell: &mut Editing<'_>, search: &mut HistorySearch) {
    *shell.history_match_buffer = std::mem::take(&mut search.matches);
    view.history_cache.clear();
    view.region.clear_from_cursor(&mut view.tw);
    redraw_prompt_line(view, shell);
}

fn handle_dir_picker_key_event(
    view: &mut EditView,
    shell: &mut Editing<'_>,
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
            view.region.clear(&mut view.tw);
            redraw_prompt_line(view, shell);
            Next::Normal
        }
        (Key::Up, _) if picker.selected > 0 => {
            picker.selected -= 1;
            view.region = render_dir_picker_mode(&mut view.tw, picker, shell, view.region);

            Next::Stay
        }
        (Key::Down, _) if picker.selected + 1 < picker.entries.len() => {
            picker.selected += 1;
            view.region = render_dir_picker_mode(&mut view.tw, picker, shell, view.region);

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
    shell: &mut Editing<'_>,
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
        } else if builtin::is_refused_epsh_builtin(first) {
            Some(false)
        } else {
            Some(
                shell
                    .path_cache
                    .contains(first, shell.path),
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
    shell: &mut Editing<'_>,
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

fn update_mode_layout_for_resize(mode: &mut Mode, shell: &Editing<'_>) {
    if let Mode::Completion(CompletionMode { state, .. }) = mode {
        let (cols, rows) = complete::compute_grid(&state.comp.entries, shell.cols);
        state.cols = cols;
        state.rows = rows;
        state.term_cols = shell.cols;
        state.term_rows = shell.rows;
        if state.selected != usize::MAX { state.selected = state.selected.min(state.comp.len().saturating_sub(1)); }
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


fn handle_normal_key(
    key: KeyEvent,
    line: &mut LineBuffer,
    history_idx: &mut Option<usize>,
    saved_line: &mut String,
    grid_column: &mut Option<usize>,
    prompt_display_len: usize,
    shell: &Editing<'_>,
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
                try_alias_expand(line, shell.aliases);
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
    shell: &Editing<'_>,
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

    let expanded = crate::alias::expand_command_line(text, aliases);
    if let std::borrow::Cow::Owned(new_text) = expanded {
        line.set(&new_text);
    }
}


enum CompletionAction {
    Navigate,
    Accept,
    Cancel,
    Refilter,
}

fn current_cmd_start(s: &str) -> usize {
    let mut in_single = false;
    let mut in_double = false;
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !in_single && i + 1 < bytes.len() => {
                i += 2;
            }
            b'\'' if !in_double => {
                in_single = !in_single;
                i += 1;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                i += 1;
            }
            b'|' | b';' if !in_single && !in_double => {
                i += 1;
                start = i;
            }
            b'&' if !in_single && !in_double && i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                i += 2;
                start = i;
            }
            _ => i += 1,
        }
    }
    start
}

fn shell_tokens(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                } else {
                    cur.push('\\');
                }
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
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

fn filter_existing_args(comp: &mut complete::Completions, existing: &[String], dir_prefix: &str) {
    if existing.is_empty() {
        return;
    }
    let to_remove: Vec<bool> = comp
        .entries
        .iter()
        .map(|e| {
            let full = format!("{}{}", dir_prefix, comp.entry_name(e));
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

fn is_command_position(before_cursor: &str, word_start: usize) -> bool {
    if word_start == 0 {
        return true;
    }
    let before_word = before_cursor[..word_start].trim_end();
    if before_word.ends_with('|') || before_word.ends_with(';') || before_word.ends_with("&&") {
        return true;
    }
    let prev_word = before_word
        .rsplit_once(|c: char| c.is_whitespace() || c == '|' || c == ';')
        .map(|(_, w)| w)
        .unwrap_or(before_word);
    matches!(prev_word, "sudo" | "doas" | "su")
}

fn find_comp_word_start(s: &str) -> (usize, bool, bool) {
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
    (word_start, in_single, in_double)
}

fn unescape_outside_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.next()
        {
            out.push(next);
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_completion_word(raw_word: &str, in_single: bool, in_double: bool) -> (String, bool) {
    if let Some(rest) = raw_word.strip_prefix("~/'") {
        let unquoted = rest.strip_suffix('\'').unwrap_or(rest);
        return (format!("~/{unquoted}"), true);
    }
    if let Some(rest) = raw_word.strip_prefix("~/\"") {
        let unquoted = rest.strip_suffix('"').unwrap_or(rest);
        return (format!("~/{unescaped}", unescaped = unescape_outside_single(unquoted)), true);
    }
    if let Some(inner) = raw_word.strip_prefix('\'') {
        let unquoted = inner.strip_suffix('\'').unwrap_or(inner);
        return (unquoted.into(), true);
    }
    if let Some(inner) = raw_word.strip_prefix('"') {
        let unquoted = inner.strip_suffix('"').unwrap_or(inner);
        return (unescape_outside_single(unquoted), true);
    }
    if in_single {
        return (raw_word.replace('\'', ""), true);
    }
    if in_double {
        return (
            unescape_outside_single(&raw_word.replace('"', "")),
            true,
        );
    }
    (unescape_outside_single(raw_word), false)
}

#[allow(clippy::too_many_arguments)]
fn start_completion(
    line: &LineBuffer,
    term_cols: u16,
    term_rows: u16,
    home: Option<&str>,
    aliases: &AliasMap,
    path_env: &[u8],
    cwd: &Path,
    output: &mut TermWriter,
    mut comp: complete::Completions,
) -> CompletionState {
    comp.clear();

    let text = line.text();
    let before_cursor = &text[..line.cursor()];
    let (word_start, in_single, in_double) = find_comp_word_start(before_cursor);
    let existing = existing_cmd_args(before_cursor, word_start);
    let raw_word = &before_cursor[word_start..];

    let (partial, in_quote): (String, bool) = parse_completion_word(raw_word, in_single, in_double);

    let first_command_word = text.split_whitespace().next().unwrap_or("");
    const SSH_CMDS: &[&str] = &["ssh", "scp", "rsync", "sftp", "mosh"];
    let needs_home = partial.starts_with('~')
        || (word_start > 0 && SSH_CMDS.contains(&first_command_word) && !partial.contains([':', '/']));
    let home = match home {
        Some(home) => home,
        None if needs_home => {
            output.write_str("\r\nish: completion: HOME is not UTF-8\r\n");
            return CompletionState {
                comp,
                selected: 0,
                cols: 1,
                rows: 0,
                scroll: 0,
                term_cols,
                term_rows,
                dir_prefix: String::new(),
                in_quote,
            };
        }
        None => "",
    };

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
        path::complete_commands(&partial, &mut comp, path_env);
        complete::complete_path_from(cwd, &partial, true, &mut comp);
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
            term_rows,
            dir_prefix: String::new(),
            in_quote: false,
        };
    }

    let dirs_only = first_command_word == "cd" && word_start > 0;

    if word_start > 0 && SSH_CMDS.contains(&first_command_word) && !partial.contains([':', '/']) {
        complete::complete_hostnames(&partial, home, &mut comp);
        complete::complete_path_from(cwd, &partial, false, &mut comp);
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
                term_rows,
                dir_prefix: String::new(),
                in_quote,
            };
        }
    }

    let expanded = if partial == "~" {
        format!("{home}/")
    } else if let Some(rest) = partial.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        partial.clone()
    };

    let dir_prefix = if let Some(slash_pos) = partial.rfind('/') {
        partial[..=slash_pos].to_string()
    } else {
        String::new()
    };

    complete::complete_path_from(cwd, &expanded, dirs_only, &mut comp);
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
            term_rows,
            dir_prefix,
            in_quote,
        };
    }

    let (partial_comp, groups) = complete::complete_partial_path_from(cwd, &expanded, dirs_only);
    if !groups.is_empty() {
        let (user_root, expanded_root) = if partial.starts_with("~/") || partial == "~" {
            ("~/".to_string(), format!("{home}/"))
        } else if partial.starts_with('/') {
            ("/".to_string(), "/".to_string())
        } else {
            (String::new(), String::new())
        };

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
            term_rows,
            dir_prefix: user_root,
            in_quote,
        };
    }

    CompletionState {
        comp,
        selected: 0,
        cols: 0,
        rows: 0,
        scroll: 0,
        term_cols,
        term_rows,
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
    let (word_start, _, _) = find_comp_word_start(before_cursor);
    let after_cursor = &text[base_line.cursor()..];

    let mut inner = state.dir_prefix.clone();
    inner.push_str(name);
    if entry.is_dir() {
        inner.push('/');
    } else if entry.is_host() {
        inner.push(':');
    }

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


enum HistoryAction {
    Continue,
    Accept(String),
    Cancel,
}

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
    shell: &Editing<'_>,
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
    shell: &Editing<'_>,
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
            shell.cwd,
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
                shell.cwd,
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
                shell.cwd,
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
            shell.cwd,
        );
        std::mem::swap(candidates, scratch);
    }
    *selected = 0;
}

fn render_history_mode(
    tw: &mut TermWriter,
    search: &HistorySearch,
    shell: &Editing<'_>,
    prev: render::RenderedRegion,
    cache: &mut render::HistoryPagerCache,
) -> render::RenderedRegion {
    render::render_history_pager_cached(
        tw,
        search.query.text(),
        &search.matches,
        shell.history,
        search.selected,
        shell.rows,
        shell.cols,
        search.query.display_cursor_pos(),
        prev,
        cache,
    )
}


fn render_dir_picker_mode(
    tw: &mut TermWriter,
    picker: &DirPicker,
    shell: &Editing<'_>,
    prev: render::RenderedRegion,
) -> render::RenderedRegion {
    render::render_dir_picker(
        tw,
        &picker.entries,
        picker.selected,
        shell.home_display,
        shell.rows,
        shell.cols,
        prev,
    )
}


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


#[cfg(test)]
mod tests;
