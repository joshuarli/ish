use crate::complete::CompletionState;
use crate::history::{FuzzyMatch, History};
use crate::line::LineBuffer;
use crate::term::TermWriter;

/// Cursor position info returned by render_line, needed by completion rendering.
pub struct PromptInfo {
    pub total_rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Optional display hints for render_line.
#[derive(Default)]
pub struct RenderOpts<'a> {
    /// Color the command word: Some(true) = green, Some(false) = red, None = no color.
    pub cmd_color: Option<bool>,
    /// Autosuggestion ghost text to show after the cursor.
    pub suggestion: &'a str,
}

/// Render the prompt + line buffer. Positions cursor correctly.
/// `prev_cursor_row` is the cursor row from the previous render — needed to
/// move back to the top of the prompt area before clearing.
/// Returns prompt geometry so completion rendering can restore cursor.
pub fn render_line(
    tw: &mut TermWriter,
    prompt: &str,
    prompt_display_len: usize,
    line: &LineBuffer,
    term_cols: u16,
    prev_cursor_row: u16,
    opts: &RenderOpts,
) -> PromptInfo {
    tw.hide_cursor();
    tw.move_cursor_up(prev_cursor_row);
    tw.carriage_return();
    tw.clear_to_end_of_screen();

    let text = line.text();
    let cols = term_cols as usize;

    // Multiline path: buffer contains explicit newlines
    if text.contains('\n') {
        return render_line_multiline(tw, prompt, prompt_display_len, line, cols, opts);
    }

    tw.write_str(prompt);

    // Write line text with optional command-word coloring
    match opts.cmd_color {
        Some(valid) => {
            let color = if valid { "\x1b[32m" } else { "\x1b[31m" };
            let first_end = text.find(|c: char| c.is_whitespace()).unwrap_or(text.len());
            tw.write_str(color);
            tw.write_str(&text[..first_end]);
            tw.write_str("\x1b[0m");
            tw.write_str(&text[first_end..]);
        }
        None => tw.write_str(text),
    }

    // Autosuggestion ghost text (dim gray, after line content)
    let suggestion_display_len = if opts.suggestion.is_empty() {
        0
    } else {
        tw.write_str("\x1b[38;5;8m");
        tw.write_str(opts.suggestion);
        tw.write_str("\x1b[0m");
        crate::line::str_width(opts.suggestion)
    };

    // Calculate cursor position
    let total_before_cursor = prompt_display_len + line.display_cursor_pos();
    let total_full = prompt_display_len + line.display_len() + suggestion_display_len;

    let cursor_row = total_before_cursor / cols;
    let cursor_col = total_before_cursor % cols;
    let total_rows = total_full / cols;

    // Move cursor from end of text to correct position
    let rows_back = total_rows - cursor_row;
    if rows_back > 0 {
        tw.move_cursor_up(rows_back as u16);
    }
    tw.carriage_return();
    if cursor_col > 0 {
        tw.move_cursor_right(cursor_col as u16);
    }

    tw.show_cursor();
    PromptInfo {
        total_rows: (total_rows + 1) as u16,
        cursor_row: cursor_row as u16,
        cursor_col: cursor_col as u16,
    }
}

/// Render a multiline buffer (contains `\n`). First line gets the main prompt,
/// subsequent lines get a continuation prompt "  ".
fn render_line_multiline(
    tw: &mut TermWriter,
    prompt: &str,
    prompt_display_len: usize,
    line: &LineBuffer,
    cols: usize,
    opts: &RenderOpts,
) -> PromptInfo {
    let text = line.text();
    let cursor_byte = line.cursor();
    let cont_prompt = "  ";
    let cont_prompt_len = 2;

    let mut row: usize = 0;
    let mut cursor_row: usize = 0;
    let mut cursor_col: usize = 0;
    let mut line_idx = 0;

    for (i, segment) in text.split('\n').enumerate() {
        if i == 0 {
            tw.write_str(prompt);

            // Command-word coloring on first line
            match opts.cmd_color {
                Some(valid) => {
                    let color = if valid { "\x1b[32m" } else { "\x1b[31m" };
                    let first_end = segment
                        .find(|c: char| c.is_whitespace())
                        .unwrap_or(segment.len());
                    tw.write_str(color);
                    tw.write_str(&segment[..first_end]);
                    tw.write_str("\x1b[0m");
                    tw.write_str(&segment[first_end..]);
                }
                None => tw.write_str(segment),
            }
        } else {
            tw.write_str("\r\n");
            row += 1;
            tw.write_str(cont_prompt);
            tw.write_str(segment);
        }

        // Track cursor position within this segment
        let seg_start = line_idx;
        let seg_end = seg_start + segment.len();

        if cursor_byte >= seg_start && cursor_byte <= seg_end {
            let prefix = if i == 0 {
                prompt_display_len
            } else {
                cont_prompt_len
            };
            let cursor_in_seg = cursor_byte - seg_start;
            let display_before = crate::line::str_width(&segment[..cursor_in_seg]);
            let total = prefix + display_before;
            cursor_row = row + total / cols;
            cursor_col = total % cols;
        }

        // Track total display rows for this segment (wrapping)
        let prefix = if i == 0 {
            prompt_display_len
        } else {
            cont_prompt_len
        };
        let seg_width = prefix + crate::line::str_width(segment);
        if seg_width > 0 {
            row += (seg_width - 1) / cols; // additional rows from wrapping
        }

        // Advance line_idx past segment + the \n separator
        line_idx = seg_end + 1; // +1 for the \n
    }

    let total_rows = row;

    // Move cursor from end of text to correct position
    let rows_back = total_rows - cursor_row;
    if rows_back > 0 {
        tw.move_cursor_up(rows_back as u16);
    }
    tw.carriage_return();
    if cursor_col > 0 {
        tw.move_cursor_right(cursor_col as u16);
    }

    tw.show_cursor();
    PromptInfo {
        total_rows: (total_rows + 1) as u16,
        cursor_row: cursor_row as u16,
        cursor_col: cursor_col as u16,
    }
}

/// Render the completion grid below the current line.
/// `initial`: true for first render (adds newline), false for repaint.
/// Cursor should be on the prompt line. Leaves cursor on the prompt line.
pub fn render_completions(
    tw: &mut TermWriter,
    state: &CompletionState,
    info: &PromptInfo,
    initial: bool,
) {
    let visible_rows = grid_visible_rows(state);
    if visible_rows == 0 {
        return;
    }
    tw.hide_cursor();

    if initial {
        // Pre-create grid rows below the prompt. The \n's may scroll
        // the terminal, so we do this before save_cursor.
        let rows_below = info.total_rows - 1 - info.cursor_row;
        if rows_below > 0 {
            tw.move_cursor_down(rows_below);
        }
        for _ in 0..visible_rows {
            tw.write_str("\n");
        }
        // Return to cursor position — total relative movement was
        // rows_below (down) + visible_rows (down via \n), so reverse it.
        tw.move_cursor_up(rows_below + visible_rows as u16);
        tw.carriage_return();
        if info.cursor_col > 0 {
            tw.move_cursor_right(info.cursor_col);
        }
    }

    // Save cursor, draw grid, restore — works for both initial and
    // repaint because all scrolling is already done above.
    tw.save_cursor();
    tw.move_cursor_down(info.total_rows - info.cursor_row);
    draw_grid(tw, state, visible_rows);
    tw.restore_cursor();

    tw.show_cursor();
}

pub fn grid_visible_rows(state: &CompletionState) -> usize {
    if state.comp.is_empty() || state.rows == 0 {
        return 0;
    }
    state.rows.min(10)
}

fn draw_grid(tw: &mut TermWriter, state: &CompletionState, visible_rows: usize) {
    // Stack array — max 6 columns, no heap allocation per grid draw.
    let mut col_widths = [0usize; 6];
    for (i, entry) in state.comp.entries.iter().enumerate() {
        let col = i / state.rows;
        if col < state.cols {
            col_widths[col] = col_widths[col].max(entry.display_width());
        }
    }

    let selected_row = state.selected % state.rows;
    let scroll_start = if selected_row < state.scroll {
        selected_row
    } else if selected_row >= state.scroll + visible_rows {
        selected_row + 1 - visible_rows
    } else {
        state.scroll
    };

    for vr in 0..visible_rows {
        let row = scroll_start + vr;
        tw.carriage_return();
        tw.clear_to_end_of_line();

        for (col, &col_w) in col_widths[..state.cols].iter().enumerate() {
            let idx = col * state.rows + row;
            if idx >= state.comp.entries.len() {
                break;
            }
            let entry = &state.comp.entries[idx];
            let is_selected = idx == state.selected;

            if is_selected {
                tw.write_str("\x1b[7m"); // reverse video
            }

            // Color: symlink=cyan, dir=blue, exec=green, host=magenta
            if entry.is_host() {
                tw.write_str("\x1b[35m");
            } else if entry.is_link() {
                tw.write_str("\x1b[36m");
            } else if entry.is_dir() {
                tw.write_str("\x1b[34m");
            } else if entry.is_exec() {
                tw.write_str("\x1b[32m");
            }

            state.write_display_name(idx, tw);

            if entry.is_host() || entry.is_link() || entry.is_dir() || entry.is_exec() {
                tw.write_str("\x1b[0m");
            }

            if is_selected {
                tw.write_str("\x1b[0m");
            }

            let display_w = entry.display_width();
            let pad = col_w.saturating_sub(display_w) + 2;
            for _ in 0..pad {
                tw.write_str(" ");
            }
        }

        if vr + 1 < visible_rows {
            tw.write_str("\n");
        }
    }
}

/// Render the Ctrl+R history search pager.
#[allow(clippy::too_many_arguments)]
pub fn render_history_pager(
    tw: &mut TermWriter,
    query: &str,
    matches: &[FuzzyMatch],
    history: &History,
    selected: usize,
    term_rows: u16,
    term_cols: u16,
    query_cursor: usize,
) {
    tw.hide_cursor();

    // Search field at top
    tw.carriage_return();
    tw.clear_to_end_of_line();
    tw.write_str("\x1b[1m"); // bold
    tw.write_str("search: ");
    tw.write_str("\x1b[0m");
    tw.write_str(query);
    tw.clear_to_end_of_line();

    // Matches below
    let max_results = (term_rows as usize).saturating_sub(2).min(20);
    tw.write_str("\n");

    for (i, m) in matches.iter().take(max_results).enumerate() {
        tw.carriage_return();
        tw.clear_to_end_of_line();

        let is_selected = i == selected;
        if is_selected {
            tw.write_str("\x1b[7m"); // reverse video
        }

        let text = history.get(m.entry_idx);

        // Write entry with matching chars highlighted.
        // Use a sorted position index instead of HashSet to avoid allocation.
        let max_width = term_cols as usize - 2;
        let mut col = 0;
        let mut pi = 0; // index into match_positions
        for (ci, ch) in text.chars().enumerate() {
            let w = crate::line::char_width(ch);
            if col + w > max_width {
                break;
            }
            col += w;
            let is_match = pi < m.match_count as usize && m.match_positions[pi] == ci as u16;
            if is_match {
                pi += 1;
            }
            if is_match && !is_selected {
                tw.write_str("\x1b[1;33m"); // bold yellow
                let mut buf = [0u8; 4];
                tw.write_str(ch.encode_utf8(&mut buf));
                tw.write_str("\x1b[0m");
            } else {
                let mut buf = [0u8; 4];
                tw.write_str(ch.encode_utf8(&mut buf));
            }
        }

        if is_selected {
            tw.write_str("\x1b[0m");
        }
        tw.clear_to_end_of_line();
        tw.write_str("\n");
    }

    // Clear remaining lines
    tw.clear_to_end_of_screen();

    // Position cursor in search field
    let displayed = matches.len().min(max_results);
    let up = displayed + 1;
    tw.move_cursor_up(up as u16);
    tw.carriage_return();
    tw.move_cursor_right((8 + query_cursor) as u16); // "search: " = 8 chars
    tw.show_cursor();
}

/// Render the file picker pager (Ctrl+F).
/// In query phase (`query_phase` true), shows "find: " prompt.
/// In results phase, shows the selectable file list.
#[allow(clippy::too_many_arguments)]
pub fn render_file_picker(
    tw: &mut TermWriter,
    query: &str,
    entries: &[String],
    selected: usize,
    term_rows: u16,
    term_cols: u16,
    query_cursor: usize,
    query_phase: bool,
    hidden: bool,
) {
    tw.hide_cursor();

    // Header: "find: " or "find (hidden): "
    tw.carriage_return();
    tw.clear_to_end_of_line();
    tw.write_str("\x1b[1m"); // bold
    if hidden {
        tw.write_str("find \x1b[33m(hidden)\x1b[0;1m: ");
    } else if query_phase && query.is_empty() {
        tw.write_str("find \x1b[2m(↓ toggle hidden)\x1b[0;1m: ");
    } else {
        tw.write_str("find: ");
    }
    tw.write_str("\x1b[0m");
    tw.write_str(query);

    if !query_phase && entries.is_empty() && !query.is_empty() {
        tw.write_str("  \x1b[2m(no matches)\x1b[0m");
    }

    tw.clear_to_end_of_line();

    let max_results = (term_rows as usize).saturating_sub(2).min(20);
    let max_width = term_cols as usize - 2;

    // Scroll window: keep selected visible
    let total = entries.len();
    let scroll = if total <= max_results || selected < max_results / 2 {
        0
    } else if selected + max_results / 2 >= total {
        total.saturating_sub(max_results)
    } else {
        selected - max_results / 2
    };

    tw.write_str("\n");

    let visible = entries.iter().skip(scroll).take(max_results).enumerate();
    let mut displayed = 0;
    for (i, path) in visible {
        let abs_idx = scroll + i;
        tw.carriage_return();
        tw.clear_to_end_of_line();

        let is_selected = abs_idx == selected;
        if is_selected {
            tw.write_str("\x1b[7m"); // reverse video
        }

        // Truncate to terminal width
        let mut col = 0;
        for ch in path.chars() {
            let w = crate::line::char_width(ch);
            if col + w > max_width {
                break;
            }
            col += w;
            let mut buf = [0u8; 4];
            tw.write_str(ch.encode_utf8(&mut buf));
        }

        if is_selected {
            tw.write_str("\x1b[0m");
        }
        tw.clear_to_end_of_line();
        tw.write_str("\n");
        displayed += 1;
    }

    // Clear remaining lines
    tw.clear_to_end_of_screen();

    // Position cursor in the query field
    let up = displayed + 1;
    tw.move_cursor_up(up as u16);
    tw.carriage_return();
    // "find (hidden): " = 16, "find (↓ toggle hidden): " = 24, "find: " = 6
    let prefix_len = if hidden {
        16
    } else if query_phase && query.is_empty() {
        24
    } else {
        6
    };
    tw.move_cursor_right((prefix_len + query_cursor) as u16);
    tw.show_cursor();
}
