use crate::complete::{CompEntry, CompletionState};
use crate::history::FuzzyMatch;
use crate::line::LineBuffer;
use crate::term::TermWriter;

/// Cursor position info returned by render_line, needed by completion rendering.
pub struct PromptInfo {
    pub total_rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Render the prompt + line buffer. Positions cursor correctly.
/// Returns prompt geometry so completion rendering can restore cursor.
pub fn render_line(
    tw: &mut TermWriter,
    prompt: &str,
    prompt_display_len: usize,
    line: &LineBuffer,
    term_cols: u16,
) -> PromptInfo {
    tw.hide_cursor();
    tw.carriage_return();
    tw.clear_to_end_of_screen();

    tw.write_str(prompt);
    tw.write_str(line.text());

    // Calculate cursor position
    let total_before_cursor = prompt_display_len + line.display_cursor_pos();
    let total_full = prompt_display_len + line.display_len();
    let cols = term_cols as usize;

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

/// Render the completion grid below the current line (initial render).
/// Cursor should be on the prompt line (as left by render_line).
/// Leaves cursor back on the prompt line at cursor position.
pub fn render_completions(tw: &mut TermWriter, state: &CompletionState, info: &PromptInfo) {
    let visible_rows = grid_visible_rows(state);
    if visible_rows == 0 {
        return;
    }
    tw.hide_cursor();

    // Move from cursor to bottom of prompt area
    let rows_below = info.total_rows - 1 - info.cursor_row;
    if rows_below > 0 {
        tw.move_cursor_down(rows_below);
    }

    // Create new line for grid
    tw.write_str("\n");
    draw_grid(tw, state, visible_rows);

    // Move back to cursor position
    let up = info.total_rows + visible_rows as u16 - 1 - info.cursor_row;
    tw.move_cursor_up(up);
    tw.carriage_return();
    if info.cursor_col > 0 {
        tw.move_cursor_right(info.cursor_col);
    }
    tw.show_cursor();
}

/// Repaint the completion grid in-place (navigation/refilter).
/// Cursor should be on the prompt line. Leaves cursor on the prompt line.
pub fn repaint_completions(tw: &mut TermWriter, state: &CompletionState, info: &PromptInfo) {
    let visible_rows = grid_visible_rows(state);
    if visible_rows == 0 {
        return;
    }
    tw.hide_cursor();

    // Move from cursor to first grid row (one row below last prompt row)
    let down = info.total_rows - info.cursor_row;
    tw.move_cursor_down(down);

    draw_grid(tw, state, visible_rows);

    // Move back to cursor position
    let up = info.total_rows + visible_rows as u16 - 1 - info.cursor_row;
    tw.move_cursor_up(up);
    tw.carriage_return();
    if info.cursor_col > 0 {
        tw.move_cursor_right(info.cursor_col);
    }
    tw.show_cursor();
}

pub fn grid_visible_rows(state: &CompletionState) -> usize {
    if state.entries.is_empty() || state.rows == 0 {
        return 0;
    }
    state.rows.min(10)
}

fn draw_grid(tw: &mut TermWriter, state: &CompletionState, visible_rows: usize) {
    let mut col_widths = vec![0usize; state.cols];
    for (i, entry) in state.entries.iter().enumerate() {
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

        for (col, &col_w) in col_widths.iter().enumerate().take(state.cols) {
            let idx = col * state.rows + row;
            if idx >= state.entries.len() {
                break;
            }
            let entry = &state.entries[idx];
            let is_selected = idx == state.selected;

            if is_selected {
                tw.write_str("\x1b[7m"); // reverse video
            }

            write_colored_entry(tw, entry);

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
pub fn render_history_pager(
    tw: &mut TermWriter,
    query: &str,
    matches: &[FuzzyMatch],
    selected: usize,
    term_rows: u16,
    term_cols: u16,
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

        // Write entry with matching chars highlighted
        let entry_chars: Vec<char> = m.text.chars().collect();
        let mut match_set = std::collections::HashSet::new();
        for &pos in &m.match_positions {
            match_set.insert(pos);
        }

        let max_width = term_cols as usize - 2;
        for (ci, &ch) in entry_chars.iter().enumerate() {
            if ci >= max_width {
                break;
            }
            if match_set.contains(&ci) && !is_selected {
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

    // Position cursor at end of search field
    let up = matches.len().min(max_results) + 1;
    tw.move_cursor_up(up as u16);
    tw.carriage_return();
    tw.move_cursor_right((8 + query.len()) as u16); // "search: " = 8 chars
    tw.show_cursor();
}

fn write_colored_entry(tw: &mut TermWriter, entry: &CompEntry) {
    if entry.is_link {
        tw.write_str("\x1b[36m"); // cyan
    } else if entry.is_dir {
        tw.write_str("\x1b[34m"); // blue
    } else if entry.is_exec {
        tw.write_str("\x1b[32m"); // green
    }

    tw.write_str(&entry.display_name());

    if entry.is_link || entry.is_dir || entry.is_exec {
        tw.write_str("\x1b[0m");
    }
}
