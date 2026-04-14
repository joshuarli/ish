use crate::complete::CompletionState;
use crate::history::{FuzzyMatch, History};
use crate::line::LineBuffer;
use crate::term::TermWriter;

/// Geometry for a rendered region, relative to the region's top row.
#[derive(Clone, Copy, Debug, Default)]
pub struct RenderedRegion {
    pub painted_rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
}
impl RenderedRegion {
    pub fn clear(self, tw: &mut TermWriter) {
        tw.move_cursor_up(self.cursor_row);
        tw.carriage_return();
        tw.clear_to_end_of_screen();
    }
}

struct PromptLayout {
    region: RenderedRegion,
    rows_back: u16,
    needs_forced_wrap: bool,
}

struct PagerLayout {
    region: RenderedRegion,
    rows_back: u16,
    needs_forced_wrap: bool,
    max_results: usize,
    max_width: usize,
    scroll: usize,
}

struct PagerInput {
    prefix_width: usize,
    query_width: usize,
    suffix_width: usize,
    query_cursor: usize,
    total_entries: usize,
    selected: usize,
    term_rows: u16,
    term_cols: u16,
}

struct CompletionGridLayout {
    visible_rows: usize,
    scroll_start: usize,
    col_widths: [usize; 6],
}

/// Optional display hints for render_line.
#[derive(Default)]
pub struct RenderOpts<'a> {
    /// Color the command word: Some(true) = green, Some(false) = red, None = no color.
    pub cmd_color: Option<bool>,
    /// Autosuggestion ghost text to show after the cursor.
    pub suggestion: &'a str,
}

fn layout_single_line_prompt(
    prompt_display_len: usize,
    line: &LineBuffer,
    suggestion_display_len: usize,
    cols: usize,
) -> PromptLayout {
    let total_before_cursor = prompt_display_len + line.display_cursor_pos();
    let total_full = prompt_display_len + line.display_len() + suggestion_display_len;
    let cursor_row = total_before_cursor / cols;
    let cursor_col = total_before_cursor % cols;
    let total_rows = total_full / cols;

    PromptLayout {
        region: RenderedRegion {
            painted_rows: (total_rows + 1) as u16,
            cursor_row: cursor_row as u16,
            cursor_col: cursor_col as u16,
        },
        rows_back: (total_rows - cursor_row) as u16,
        needs_forced_wrap: total_full > 0 && total_full.is_multiple_of(cols),
    }
}

fn layout_multiline_prompt(
    prompt_display_len: usize,
    line: &LineBuffer,
    cols: usize,
) -> PromptLayout {
    let text = line.text();
    let cursor_byte = line.cursor();
    let cont_prompt_len = 2;
    let segment_count = text.split('\n').count();

    let mut row: usize = 0;
    let mut cursor_row: usize = 0;
    let mut cursor_col: usize = 0;
    let mut line_idx = 0;
    let mut last_seg_width: usize = 0;

    for (i, segment) in text.split('\n').enumerate() {
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

        let prefix = if i == 0 {
            prompt_display_len
        } else {
            cont_prompt_len
        };
        let seg_width = prefix + crate::line::str_width(segment);
        if seg_width > 0 {
            row += (seg_width - 1) / cols;
        }
        last_seg_width = seg_width;
        line_idx = seg_end + 1;

        if i + 1 < segment_count {
            row += 1;
        }
    }

    let needs_forced_wrap = last_seg_width > 0 && last_seg_width.is_multiple_of(cols);
    if needs_forced_wrap {
        row += 1;
    }

    PromptLayout {
        region: RenderedRegion {
            painted_rows: (row + 1) as u16,
            cursor_row: cursor_row as u16,
            cursor_col: cursor_col as u16,
        },
        rows_back: (row - cursor_row) as u16,
        needs_forced_wrap,
    }
}

fn layout_pager(input: PagerInput) -> PagerLayout {
    let cols = input.term_cols as usize;
    let header_before_cursor = input.prefix_width + input.query_cursor;
    let header_full = input.prefix_width + input.query_width + input.suffix_width;
    let header_rows = header_full.saturating_sub(1) / cols + 1;
    let cursor_row = header_before_cursor / cols;
    let cursor_col = header_before_cursor % cols;
    let max_results = (input.term_rows as usize).saturating_sub(2).min(20);
    let displayed = input.total_entries.min(max_results);
    let scroll = if input.total_entries <= max_results || input.selected < max_results / 2 {
        0
    } else if input.selected + max_results / 2 >= input.total_entries {
        input.total_entries.saturating_sub(max_results)
    } else {
        input.selected - max_results / 2
    };

    PagerLayout {
        region: RenderedRegion {
            painted_rows: (header_rows + displayed) as u16,
            cursor_row: cursor_row as u16,
            cursor_col: cursor_col as u16,
        },
        rows_back: (header_rows + displayed - cursor_row) as u16,
        needs_forced_wrap: header_full > 0 && header_full.is_multiple_of(cols),
        max_results,
        max_width: input.term_cols as usize - 2,
        scroll,
    }
}

fn layout_completion_grid(state: &CompletionState) -> CompletionGridLayout {
    let visible_rows = grid_visible_rows(state);
    if visible_rows == 0 {
        return CompletionGridLayout {
            visible_rows: 0,
            scroll_start: 0,
            col_widths: [0; 6],
        };
    }
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

    CompletionGridLayout {
        visible_rows,
        scroll_start,
        col_widths,
    }
}

/// Render the prompt + line buffer. Positions cursor correctly.
/// `prev` is the geometry from the previous render of this region. It lets us
/// move back to the top before clearing.
/// Returns prompt geometry so completion rendering can restore cursor.
pub fn render_line(
    tw: &mut TermWriter,
    prompt: &str,
    prompt_display_len: usize,
    line: &LineBuffer,
    term_cols: u16,
    prev: RenderedRegion,
    opts: &RenderOpts,
) -> RenderedRegion {
    tw.hide_cursor();
    prev.clear(tw);

    let text = line.text();
    let cols = term_cols as usize;

    // Multiline path: buffer contains explicit newlines
    if text.contains('\n') {
        return render_line_multiline(tw, prompt, prompt_display_len, line, cols, opts);
    }

    let layout = layout_single_line_prompt(
        prompt_display_len,
        line,
        crate::line::str_width(opts.suggestion),
        cols,
    );

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
    if !opts.suggestion.is_empty() {
        tw.write_str("\x1b[38;5;8m");
        tw.write_str(opts.suggestion);
        tw.write_str("\x1b[0m");
    }

    if layout.needs_forced_wrap {
        tw.write_str(" \r");
    }

    // Move cursor from end of text to correct position
    if layout.rows_back > 0 {
        tw.move_cursor_up(layout.rows_back);
    }
    tw.carriage_return();
    if layout.region.cursor_col > 0 {
        tw.move_cursor_right(layout.region.cursor_col);
    }

    tw.show_cursor();
    layout.region
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
) -> RenderedRegion {
    let text = line.text();
    let cont_prompt = "  ";
    let layout = layout_multiline_prompt(prompt_display_len, line, cols);

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
            tw.write_str(cont_prompt);
            tw.write_str(segment);
        }
    }

    if layout.needs_forced_wrap {
        tw.write_str(" \r");
    }

    // Move cursor from end of text to correct position
    if layout.rows_back > 0 {
        tw.move_cursor_up(layout.rows_back);
    }
    tw.carriage_return();
    if layout.region.cursor_col > 0 {
        tw.move_cursor_right(layout.region.cursor_col);
    }

    tw.show_cursor();
    layout.region
}

/// Render the completion grid below the current line.
/// `initial`: true for first render (adds newline), false for repaint.
/// Cursor should be on the prompt line. Leaves cursor on the prompt line.
pub fn render_completions(
    tw: &mut TermWriter,
    state: &CompletionState,
    info: RenderedRegion,
    initial: bool,
) {
    let layout = layout_completion_grid(state);
    if layout.visible_rows == 0 {
        return;
    }
    tw.hide_cursor();

    if initial {
        // Pre-create grid rows below the prompt. The \n's may scroll
        // the terminal, so we do this before save_cursor.
        let rows_below = info.painted_rows - 1 - info.cursor_row;
        if rows_below > 0 {
            tw.move_cursor_down(rows_below);
        }
        for _ in 0..layout.visible_rows {
            tw.write_str("\n");
        }
        // Return to cursor position — total relative movement was
        // rows_below (down) + visible_rows (down via \n), so reverse it.
        tw.move_cursor_up(rows_below + layout.visible_rows as u16);
        tw.carriage_return();
        if info.cursor_col > 0 {
            tw.move_cursor_right(info.cursor_col);
        }
    }

    // Save cursor, draw grid, restore — works for both initial and
    // repaint because all scrolling is already done above.
    tw.save_cursor();
    tw.move_cursor_down(info.painted_rows - info.cursor_row);
    draw_grid(tw, state, &layout);
    tw.restore_cursor();

    tw.show_cursor();
}

pub fn grid_visible_rows(state: &CompletionState) -> usize {
    if state.comp.is_empty() || state.rows == 0 {
        return 0;
    }
    state.rows.min(10)
}

fn draw_grid(tw: &mut TermWriter, state: &CompletionState, layout: &CompletionGridLayout) {
    for vr in 0..layout.visible_rows {
        let row = layout.scroll_start + vr;
        tw.carriage_return();
        tw.clear_to_end_of_line();

        for (col, &col_w) in layout.col_widths[..state.cols].iter().enumerate() {
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
            let has_next = (col + 1..state.cols)
                .any(|next_col| next_col * state.rows + row < state.comp.entries.len());
            let pad = col_w.saturating_sub(display_w) + if has_next { 2 } else { 0 };
            for _ in 0..pad {
                tw.write_str(" ");
            }
        }

        if vr + 1 < layout.visible_rows {
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
    prev: RenderedRegion,
) -> RenderedRegion {
    tw.hide_cursor();
    prev.clear(tw);

    let prefix = "search: ";
    let layout = layout_pager(PagerInput {
        prefix_width: crate::line::str_width(prefix),
        query_width: crate::line::str_width(query),
        suffix_width: 0,
        query_cursor,
        total_entries: matches.len(),
        selected,
        term_rows,
        term_cols,
    });

    tw.write_str("\x1b[1m"); // bold
    tw.write_str(prefix);
    tw.write_str("\x1b[0m");
    tw.write_str(query);

    if layout.needs_forced_wrap {
        tw.write_str(" \r");
        tw.clear_to_end_of_line();
    } else {
        tw.write_str("\n");
    }

    for (i, m) in matches.iter().take(layout.max_results).enumerate() {
        tw.carriage_return();
        tw.clear_to_end_of_line();

        let is_selected = i == selected;
        if is_selected {
            tw.write_str("\x1b[7m"); // reverse video
        }

        let text = history.get(m.entry_idx);

        // Write entry with matching chars highlighted.
        // Use a sorted position index instead of HashSet to avoid allocation.
        let mut col = 0;
        let mut pi = 0; // index into match_positions
        for (ci, ch) in text.chars().enumerate() {
            let w = crate::line::char_width(ch);
            if col + w > layout.max_width {
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
    tw.move_cursor_up(layout.rows_back);
    tw.carriage_return();
    if layout.region.cursor_col > 0 {
        tw.move_cursor_right(layout.region.cursor_col);
    }
    tw.show_cursor();

    layout.region
}

/// Render the file picker pager (Ctrl+F).
/// In query phase (`query_phase` true), shows "find: " prompt.
/// In results phase, shows the selectable file list.
#[allow(clippy::too_many_arguments)]
pub fn render_file_picker(
    tw: &mut TermWriter,
    query: &str,
    all_entries: &[(usize, String)],
    filtered: &[usize],
    selected: usize,
    term_rows: u16,
    term_cols: u16,
    query_cursor: usize,
    query_phase: bool,
    hidden: bool,
    prev: RenderedRegion,
) -> RenderedRegion {
    tw.hide_cursor();
    prev.clear(tw);

    let prefix = if hidden {
        "find (hidden): "
    } else if query_phase && query.is_empty() {
        "find (ctrl+f toggle hidden): "
    } else {
        "find: "
    };
    let suffix = if !query_phase && filtered.is_empty() && !query.is_empty() {
        "  (no matches)"
    } else {
        ""
    };
    let layout = layout_pager(PagerInput {
        prefix_width: crate::line::str_width(prefix),
        query_width: crate::line::str_width(query),
        suffix_width: crate::line::str_width(suffix),
        query_cursor,
        total_entries: filtered.len(),
        selected,
        term_rows,
        term_cols,
    });

    // Header: "find: " or "find (hidden): "
    tw.write_str("\x1b[1m"); // bold
    if hidden {
        tw.write_str("find \x1b[33m(hidden)\x1b[0;1m: ");
    } else if query_phase && query.is_empty() {
        tw.write_str("find \x1b[2m(ctrl+f toggle hidden)\x1b[0;1m: ");
    } else {
        tw.write_str("find: ");
    }
    tw.write_str("\x1b[0m");
    tw.write_str(query);

    if !query_phase && filtered.is_empty() && !query.is_empty() {
        tw.write_str("  \x1b[2m(no matches)\x1b[0m");
    }

    if layout.needs_forced_wrap {
        tw.write_str(" \r");
        tw.clear_to_end_of_line();
    } else {
        tw.write_str("\n");
    }

    let visible = filtered
        .iter()
        .skip(layout.scroll)
        .take(layout.max_results)
        .enumerate();
    for (i, &entry_idx) in visible {
        let abs_idx = layout.scroll + i;
        let path = &all_entries[entry_idx].1;
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
            if col + w > layout.max_width {
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
    }

    // Clear remaining lines
    tw.clear_to_end_of_screen();

    // Position cursor in the query field
    tw.move_cursor_up(layout.rows_back);
    tw.carriage_return();
    if layout.region.cursor_col > 0 {
        tw.move_cursor_right(layout.region.cursor_col);
    }
    tw.show_cursor();

    layout.region
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_line(s: &str) -> LineBuffer {
        let mut lb = LineBuffer::new();
        for c in s.chars() {
            lb.insert_char(c);
        }
        lb
    }

    fn render_simple(prompt: &str, text: &str, cols: u16) -> (RenderedRegion, Vec<u8>) {
        render_with_suggestion(prompt, text, "", cols)
    }

    fn render_with_suggestion(
        prompt: &str,
        text: &str,
        suggestion: &str,
        cols: u16,
    ) -> (RenderedRegion, Vec<u8>) {
        let mut tw = TermWriter::new();
        let line = make_line(text);
        let pdl = crate::line::str_width(prompt);
        let opts = RenderOpts {
            suggestion,
            ..Default::default()
        };
        let info = render_line(
            &mut tw,
            prompt,
            pdl,
            &line,
            cols,
            RenderedRegion::default(),
            &opts,
        );
        let buf = tw.as_bytes().to_vec();
        (info, buf)
    }

    fn render_history_query(
        query: &str,
        cols: u16,
        query_cursor: usize,
    ) -> (RenderedRegion, Vec<u8>) {
        let mut tw = TermWriter::new();
        let hist = History::load_from(std::path::PathBuf::from("/tmp/ish_render_history_test"));
        let info = render_history_pager(
            &mut tw,
            query,
            &[],
            &hist,
            0,
            24,
            cols,
            query_cursor,
            RenderedRegion::default(),
        );
        (info, tw.as_bytes().to_vec())
    }

    fn render_file_query(
        query: &str,
        cols: u16,
        query_cursor: usize,
        query_phase: bool,
        hidden: bool,
    ) -> (RenderedRegion, Vec<u8>) {
        let mut tw = TermWriter::new();
        let info = render_file_picker(
            &mut tw,
            query,
            &[],
            &[],
            0,
            24,
            cols,
            query_cursor,
            query_phase,
            hidden,
            RenderedRegion::default(),
        );
        (info, tw.as_bytes().to_vec())
    }

    fn completion_state(entries: &[&str], cols: usize, rows: usize) -> CompletionState {
        let mut comp = crate::complete::Completions::new();
        for entry in entries {
            comp.push(entry, false, false, false);
        }
        CompletionState {
            comp,
            selected: usize::MAX,
            cols,
            rows,
            scroll: 0,
            dir_prefix: String::new(),
            in_quote: false,
        }
    }

    // When total display width is NOT a multiple of cols, no force-wrap needed.
    #[test]
    fn prompt_info_no_wrap() {
        // "$ " (2) + "hi" (2) = 4 in 10-col terminal → row 0
        let (info, buf) = render_simple("$ ", "hi", 10);
        assert_eq!(info.painted_rows, 1);
        assert_eq!(info.cursor_row, 0);
        assert_eq!(info.cursor_col, 4);
        assert!(
            !buf.windows(2).any(|w| w == b" \r"),
            "should not force-wrap"
        );
    }

    // When total display width wraps but doesn't land on exact boundary.
    #[test]
    fn prompt_info_partial_wrap() {
        // "$ " (2) + 9 chars = 11 in 10-col terminal → rows 0-1
        let (info, _) = render_simple("$ ", "123456789", 10);
        assert_eq!(info.painted_rows, 2);
        assert_eq!(info.cursor_row, 1);
        assert_eq!(info.cursor_col, 1);
    }

    // Exact multiple of cols → force-wrap triggers, painted_rows accounts for it.
    #[test]
    fn prompt_info_exact_boundary() {
        // "$ " (2) + "12345678" (8) = 10 in 10-col terminal → fills row 0 exactly
        let (info, buf) = render_simple("$ ", "12345678", 10);
        // Force-wrap adds an extra row for the cursor
        assert_eq!(info.painted_rows, 2);
        assert_eq!(info.cursor_row, 1);
        assert_eq!(info.cursor_col, 0);
        assert!(buf.windows(2).any(|w| w == b" \r"), "should force-wrap");
    }

    // Exact multiple with suggestion — cursor in the middle, not at pending-wrap.
    #[test]
    fn prompt_info_exact_boundary_with_suggestion() {
        // "$ " (2) + "ab" (2) = 4 before cursor; + suggestion "123456" (6) → 10 total
        let (info, buf) = render_with_suggestion("$ ", "ab", "123456", 10);
        assert_eq!(info.painted_rows, 2);
        // Cursor at position 4 → row 0, col 4
        assert_eq!(info.cursor_row, 0);
        assert_eq!(info.cursor_col, 4);
        assert!(buf.windows(2).any(|w| w == b" \r"), "should force-wrap");
    }

    // Two full rows (2 * cols) — force-wrap triggers.
    #[test]
    fn prompt_info_two_full_rows() {
        // "$ " (2) + 18 chars = 20, cols = 10 → 2 full rows
        let (info, buf) = render_simple("$ ", "abcdefghijklmnopqr", 10);
        assert_eq!(info.painted_rows, 3); // 2 content rows + 1 cursor row from force-wrap
        assert_eq!(info.cursor_row, 2);
        assert_eq!(info.cursor_col, 0);
        assert!(buf.windows(2).any(|w| w == b" \r"), "should force-wrap");
    }

    // Multiline buffer with last segment filling exact boundary.
    #[test]
    fn multiline_exact_boundary() {
        // First segment: "$ " (2) + "ab" (2) = 4, then newline
        // Second segment: "  " (2) + "12345678" (8) = 10 fills cols exactly
        let mut tw = TermWriter::new();
        let mut line = LineBuffer::new();
        for c in "ab\n12345678".chars() {
            line.insert_char(c);
        }
        let info = render_line(
            &mut tw,
            "$ ",
            2,
            &line,
            10,
            RenderedRegion::default(),
            &RenderOpts::default(),
        );
        let buf = tw.as_bytes();
        // Row 0: "$ ab" (4 cols), row 1: "  12345678" (10 cols, exact boundary)
        // Force-wrap adds row 2 for the cursor
        assert_eq!(info.painted_rows, 3);
        assert!(buf.windows(2).any(|w| w == b" \r"), "should force-wrap");
    }

    // Multiline buffer where last segment does NOT hit exact boundary → no force-wrap.
    #[test]
    fn multiline_no_force_wrap() {
        let mut tw = TermWriter::new();
        let mut line = LineBuffer::new();
        for c in "ab\n1234567".chars() {
            line.insert_char(c);
        }
        let info = render_line(
            &mut tw,
            "$ ",
            2,
            &line,
            10,
            RenderedRegion::default(),
            &RenderOpts::default(),
        );
        let buf = tw.as_bytes();
        // Row 0: "$ ab" (4), row 1: "  1234567" (9) — no exact boundary
        assert_eq!(info.painted_rows, 2);
        assert!(
            !buf.windows(2).any(|w| w == b" \r"),
            "should not force-wrap"
        );
    }

    #[test]
    fn history_pager_tracks_wrapped_query_cursor_row() {
        let (info, _) = render_history_query("abc", 10, 3);
        assert_eq!(info.cursor_row, 1);
        assert_eq!(info.cursor_col, 1);
    }

    #[test]
    fn history_pager_clears_from_top_on_wrapped_rerender() {
        let mut tw = TermWriter::new();
        let hist = History::load_from(std::path::PathBuf::from("/tmp/ish_render_history_test"));
        let info = render_history_pager(
            &mut tw,
            "abc",
            &[],
            &hist,
            0,
            24,
            10,
            3,
            RenderedRegion {
                painted_rows: 2,
                cursor_row: 1,
                cursor_col: 0,
            },
        );
        let buf = tw.as_bytes();
        assert_eq!(info.cursor_row, 1);
        assert!(
            buf.windows(4).any(|w| w == b"\x1b[1A"),
            "expected rerender to move back to the top first"
        );
    }

    #[test]
    fn file_picker_tracks_wrapped_query_cursor_row() {
        let (info, _) = render_file_query("abcdef", 10, 6, false, false);
        assert_eq!(info.cursor_row, 1);
        assert_eq!(info.cursor_col, 2);
    }

    #[test]
    fn file_picker_uses_visible_prefix_widths() {
        let (info, _) = render_file_query("", 80, 0, true, true);
        assert_eq!(info.cursor_row, 0);
        assert_eq!(info.cursor_col, 15);
    }

    #[test]
    fn completion_grid_does_not_pad_last_column() {
        let state = completion_state(&["12345678"], 1, 1);
        let mut tw = TermWriter::new();
        let layout = layout_completion_grid(&state);
        draw_grid(&mut tw, &state, &layout);
        assert!(
            tw.as_bytes().ends_with(b"12345678"),
            "last column should not add trailing padding"
        );
    }
}
