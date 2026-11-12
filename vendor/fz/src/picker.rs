use crate::{Candidates, Choices};
use std::io::{self, Write};

/// Stable identity in the original candidate sequence, including duplicates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CandidateId(usize);

impl CandidateId {
    pub fn index(self) -> usize { self.0 }
}

/// An acceptance is distinct from cancellation and from fzy's free-query echo.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted(Vec<CandidateId>),
    Query(Vec<u8>),
    NoMatch,
    Cancelled,
}

/// Presentation and search policy for one picker session. No terminal effects.
pub struct PickerConfig {
    pub lines: usize,
    pub prompt: Vec<u8>,
    pub query: Vec<u8>,
    pub show_scores: bool,
    pub show_info: bool,
    pub multi: bool,
    pub workers: usize,
}

/// Owns query editing, ranked source identities, focus and marking order.
/// The host owns events, terminal modes, signals and output framing.
pub struct Picker {
    state: Editor,
    choices: Choices,
    config: PickerConfig,
}

impl Picker {
    pub fn new(candidates: Candidates, config: PickerConfig) -> Self {
        let state = Editor::new(config.query.clone(), config.lines, config.multi);
        let mut choices = Choices::from_candidates(candidates);
        choices.set_workers(config.workers);
        choices.search(&state.query);
        Self { state, choices, config }
    }

    pub fn source(&self, id: CandidateId) -> Option<&[u8]> { self.choices.source(id.0) }
    pub fn ambiguous_key_pending(&self) -> bool { self.state.ambiguous_key_pending }
    pub fn lines(&self) -> usize { self.state.lines }
    pub fn show_info(&self) -> bool { self.config.show_info || self.config.multi }

    /// Set the visible result budget after a resize. Multi-selection requires
    /// both a visible row and enough columns for its mark and candidate.
    pub fn resize(&mut self, width: usize, height: usize) -> io::Result<()> {
        self.state.lines = self.config.lines.min(height.saturating_sub(1 + usize::from(self.show_info())));
        if self.config.multi && (self.state.lines == 0 || width <= self.config.prompt.len().max(4)) {
            return Err(io::Error::other("terminal too small for multi-select results"));
        }
        Ok(())
    }

    pub fn update(&mut self) { self.state.update_choices(&mut self.choices); }

    /// Feed a byte or resolve an ambiguous bare Escape after the host deadline.
    /// Bytes after an outcome belong to the host's next input consumer.
    pub fn feed(&mut self, byte: Option<u8>) -> Option<Outcome> {
        match handle_input(&mut self.state, &mut self.choices, byte, byte.is_none()) {
            InputResult::Continue { search_changed } => { self.state.search_dirty |= search_changed; None }
            InputResult::Cancel => Some(Outcome::Cancelled),
            InputResult::Accept => {
                self.update();
                if self.config.multi && !self.state.marked.is_empty() {
                    Some(Outcome::Accepted(self.state.marked.iter().copied().map(CandidateId).collect()))
                } else if let Some(source) = self.choices.source_index(self.choices.selection()) {
                    Some(Outcome::Accepted(vec![CandidateId(source)]))
                } else if self.config.multi {
                    Some(Outcome::NoMatch)
                } else { Some(Outcome::Query(self.state.query.clone())) }
            }
        }
    }

    /// Paste is query text, including spaces and newlines, never picker actions.
    pub fn paste(&mut self, bytes: &[u8]) {
        self.state.focus_list = false;
        for &byte in bytes { self.state.search_dirty |= self.state.insert(byte); }
    }

    pub fn render(&mut self, output: &mut impl Write, width: usize) -> io::Result<()> {
        self.update();
        Renderer { output, foreground: 9, width }.draw(&mut self.choices, &self.config, &self.state)
    }

    pub fn clear(&self, output: &mut impl Write) -> io::Result<()> {
        Renderer { output, foreground: 9, width: 0 }.clear(self.state.lines, self.show_info())
    }
}

struct Renderer<W> {
    output: W,
    foreground: u8,
    width: usize,
}

impl<W: Write> Renderer<W> {
    fn draw(
        &mut self,
        choices: &mut crate::Choices,
        options: &PickerConfig,
        state: &Editor,
    ) -> io::Result<()> {
        let available = choices.available();
        let selection = choices.selection();
        let mut start = 0usize;
        if state.lines > 0 && selection.saturating_add(1) >= state.lines {
            start = selection.saturating_add(2).saturating_sub(state.lines);
            if start.saturating_add(state.lines) >= available && available > 0 {
                start = available.saturating_sub(state.lines);
            }
        }

        self.set_column(0)?;
        self.write_bytes(&options.prompt)?;
        self.draw_query(state, options)?;
        self.clear_line()?;

        if options.show_info || options.multi {
            self.write_bytes(b"\r\n")?;
            let mut bytes = [0u8; 96];
            let capacity = bytes.len();
            let mut remaining = &mut bytes[..];
            write!(remaining, "[{available}/{}]", choices.len())?;
            if options.multi { write!(remaining, " [{} selected]", state.marked.len())?; }
            let mut length = capacity - remaining.len();
            if options.multi && length >= self.width {
                let mut remaining = &mut bytes[..];
                write!(remaining, "[{} selected]", state.marked.len())?;
                length = (capacity - remaining.len()).min(self.width.saturating_sub(1));
            }
            self.write_bytes(&bytes[..length])?;
            self.clear_line()?;
        }

        let width = self.width;
        let budget = state.lines;
        let prefix_width = usize::from(options.multi) * 4 + usize::from(options.show_scores) * 8;
        let mut end = start.saturating_add(budget).min(available);
        let mut row_counts: Vec<usize> = (start..end)
            .map(|index| choices.get(index).map_or(1, |candidate| wrapped_rows(candidate, prefix_width, width)))
            .collect();
        let mut total_rows: usize = row_counts.iter().sum();
        while total_rows > budget && start < selection && end > start {
            total_rows -= row_counts.remove(0);
            start += 1;
        }
        while total_rows > budget && end > selection.saturating_add(1) && !row_counts.is_empty() {
            total_rows -= row_counts.pop().unwrap_or(0);
            end -= 1;
        }
        let mut rows_used = 0usize;
        for index in start..end {
            self.write_bytes(b"\r\n")?;
            self.clear_line()?;
            let score = options.show_scores.then(|| choices.getscore(index)).flatten();
            let marked = options.multi && choices.source_index(index)
                .is_some_and(|source| state.marked.contains(&source));
            if let Some(candidate) = choices.get(index) {
                let remaining = budget.saturating_sub(rows_used).max(1);
                rows_used += self.draw_candidate(candidate, state, CandidateRow {
                    score, width, max_rows: remaining,
                    selected: index == selection && (!options.multi || state.focus_list),
                    multi: options.multi, marked,
                })?;
            }
        }
        for _ in rows_used..budget {
            self.write_bytes(b"\r\n")?;
            self.clear_line()?;
        }

        let result_rows = state.lines + usize::from(options.show_info || options.multi);
        if result_rows > 0 {
            self.move_up(result_rows)?;
        }
        self.set_column(0)?;
        self.write_bytes(&options.prompt)?;
        let start = self.query_start(state, options);
        self.write_bytes(&state.query[start..state.cursor])?;
        self.flush()
    }

    fn query_start(&self, state: &Editor, options: &PickerConfig) -> usize {
        let budget = self.width.saturating_sub(options.prompt.len() + 1);
        let mut start = state.cursor;
        let mut used = 0;
        while start > 0 {
            let previous = previous_boundary(&state.query, start);
            let (scalar, _) = decode_char(&state.query[previous..]);
            let width = scalar_width(scalar);
            if used + width > budget { break; }
            used += width;
            start = previous;
        }
        start
    }

    fn draw_query(&mut self, state: &Editor, options: &PickerConfig) -> io::Result<()> {
        let mut index = self.query_start(state, options);
        let mut column = options.prompt.len();
        while index < state.query.len() {
            let (scalar, count) = decode_char(&state.query[index..]);
            let width = scalar_width(scalar);
            if column + width >= self.width { break; }
            self.write_bytes(&state.query[index..index + count])?;
            index += count;
            column += width;
        }
        Ok(())
    }

    fn draw_candidate(
        &mut self,
        candidate: &[u8],
        state: &Editor,
        row: CandidateRow,
    ) -> io::Result<usize> {
        let CandidateRow { score, width, max_rows, selected, multi, marked } = row;
        if selected { self.set_invert()?; }
        if multi {
            self.write_bytes(if marked { b"[x] " } else { b"[ ] " })?;
        }
        if let Some(score) = score {
            if score == f64::NEG_INFINITY {
                self.write_bytes(b"(     ) ")?;
            } else {
                write!(self.output, "({score:5.2}) ")?;
            }
        }

        let prefix_width = usize::from(multi) * 4 + usize::from(score.is_some()) * 8;
        if width <= prefix_width {
            self.set_nowrap()?;
            let positions = crate::match_positions(&state.query, candidate).unwrap_or_default();
            let mut position = 0usize;
            for (index, &byte) in candidate.iter().enumerate() {
                if positions.get(position) == Some(&index) {
                    self.set_foreground(3)?;
                    position += 1;
                } else {
                    self.set_foreground(9)?;
                }
                if byte == b'\n' {
                    self.write_bytes(b" ")?;
                } else {
                    self.output.write_all(&candidate[index..=index])?;
                }
            }
            self.set_wrap()?;
            self.set_normal()?;
            return Ok(1);
        }

        let positions = crate::match_positions(&state.query, candidate).unwrap_or_default();
        let mut position = 0usize;
        let mut column = prefix_width;
        let mut rows = 1usize;
        let mut index = 0usize;
        while index < candidate.len() {
            let (scalar, length) = decode_char(&candidate[index..]);
            let char_width = if candidate[index] == b'\n' { 1 } else { scalar_width(scalar) };
            if char_width > 0 && column + char_width > width {
                if rows >= max_rows { break; }
                self.write_bytes(b"\r\n")?;
                self.set_column(0)?;
                self.clear_line()?;
                rows += 1;
                column = 0;
            }
            if positions.get(position) == Some(&index) {
                self.set_foreground(3)?;
                position += 1;
            } else {
                self.set_foreground(9)?;
            }
            if candidate[index] == b'\n' {
                self.write_bytes(b" ")?;
            } else {
                self.output.write_all(&candidate[index..index + length])?;
            }
            column += char_width;
            index += length;
        }

        self.set_normal()?;
        Ok(rows)
    }

    fn clear(&mut self, lines: usize, show_info: bool) -> io::Result<()> {
        self.set_column(0)?;
        for _ in 0..lines + usize::from(show_info) {
            self.newline()?;
        }
        self.clear_line()?;
        if lines > 0 {
            self.move_up(lines + usize::from(show_info))?;
        }
        self.flush()
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }

    fn set_foreground(&mut self, color: u8) -> io::Result<()> {
        if self.foreground != color {
            write!(self.output, "\x1b[{}m", 30 + color)?;
            self.foreground = color;
        }
        Ok(())
    }

    fn set_invert(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[7m")
    }

    fn set_normal(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[0m")?;
        self.foreground = 9;
        Ok(())
    }

    fn set_nowrap(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[?7l")
    }

    fn set_wrap(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[?7h")
    }

    fn clear_line(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[K")
    }

    fn newline(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[K\r\n")
    }

    fn set_column(&mut self, column: usize) -> io::Result<()> {
        write!(self.output, "\x1b[{}G", column + 1)
    }

    fn move_up(&mut self, rows: usize) -> io::Result<()> {
        write!(self.output, "\x1b[{}A", rows)
    }
}

fn handle_input(
        state: &mut Editor,
        choices: &mut crate::Choices,
        byte: Option<u8>,
        resolve_ambiguous_key: bool,
    ) -> InputResult {
        state.ambiguous_key_pending = false;
        if let Some(byte) = byte {
            state.input.push(byte);
        }

        if state.in_paste {
            const END: &[u8] = b"\x1b[201~";
            if state.input == END {
                state.input.clear();
                state.in_paste = false;
                return InputResult::Continue { search_changed: false };
            }
            let mut changed = false;
            while !END.starts_with(&state.input) {
                let byte = state.input.remove(0);
                if is_printable(byte) { changed |= state.insert(byte); }
            }
            return InputResult::Continue { search_changed: changed };
        }

        if state.multi && state.focus_list && state.input == b" " {
            state.input.clear();
            state.update_choices(choices);
            if let Some(source) = choices.source_index(choices.selection()) {
                if let Some(index) = state.marked.iter().position(|&id| id == source) {
                    state.marked.remove(index);
                } else {
                    state.marked.push(source);
                }
            }
            return InputResult::Continue { search_changed: false };
        }

        let mut matching_action = None;
        let mut in_middle = false;
        for &(key, action) in KEY_BINDINGS {
            if state.input == key {
                matching_action = Some(action);
            } else if key.starts_with(&state.input) {
                in_middle = true;
            }
        }

        if let Some(action) = matching_action
            && (!in_middle || resolve_ambiguous_key) {
                state.input.clear();
                return apply_action(action, state, choices);
            }

        if matching_action.is_some() && in_middle {
            state.ambiguous_key_pending = true;
            return InputResult::Continue {
                search_changed: false,
            };
        }

        if in_middle {
            return InputResult::Continue {
                search_changed: false,
            };
        }

        if state.multi && state.focus_list {
            state.focus_list = false;
        }
        let mut search_changed = false;
        for index in 0..state.input.len() {
            let byte = state.input[index];
            if is_printable(byte) {
                search_changed |= state.insert(byte);
            }
        }
        state.input.clear();
        InputResult::Continue { search_changed }
    }

fn apply_action(
        action: Action,
        state: &mut Editor,
        choices: &mut crate::Choices,
    ) -> InputResult {
        if matches!(action, Action::Accept | Action::Complete | Action::Previous | Action::Next | Action::PageUp | Action::PageDown) {
            state.update_choices(choices);
        }
        let changed = match action {
            Action::Cancel => return InputResult::Cancel,
            Action::Accept => return InputResult::Accept,
            Action::DeleteCharacter => { state.focus_list = false; state.delete_character() }
            Action::DeleteWord => { state.focus_list = false; state.delete_word() }
            Action::DeleteToBeginning => { state.focus_list = false; state.delete_to_beginning() }
            Action::Complete => {
                state.focus_list = false;
                choices.selected().is_some_and(|candidate| state.replace(candidate))
            }
            Action::Previous => {
                if state.multi {
                    if state.focus_list {
                        if choices.selection() == 0 { state.focus_list = false; }
                        else { choices.prev(); }
                    }
                } else { choices.prev(); }
                false
            }
            Action::Next => {
                if state.multi && !state.focus_list {
                    state.focus_list = choices.available() > 0;
                } else { choices.next(); }
                false
            }
            Action::Left => { state.focus_list = false; state.left(); false }
            Action::Right => { state.focus_list = false; state.right(); false }
            Action::Beginning => { state.focus_list = false; state.cursor = 0; false }
            Action::End => { state.focus_list = false; state.cursor = state.query.len(); false }
            Action::PageUp => {
                if !state.multi || state.focus_list {
                    for _ in 0..state.lines.min(choices.selection()) { choices.prev(); }
                    if state.multi && choices.selection() == 0 { state.focus_list = false; }
                }
                false
            }
            Action::PageDown => {
                if state.multi && !state.focus_list {
                    state.focus_list = choices.available() > 0;
                } else {
                    let remaining = choices.available().saturating_sub(choices.selection() + 1);
                    for _ in 0..state.lines.min(remaining) { choices.next(); }
                }
                false
            }
            Action::PasteStart => { state.in_paste = true; false }
            Action::Ignore => false,
        };
        InputResult::Continue { search_changed: changed }
    }


#[derive(Clone, Copy)]
enum Action {
    Cancel,
    Accept,
    DeleteCharacter,
    DeleteWord,
    DeleteToBeginning,
    Complete,
    Previous,
    Next,
    Left,
    Right,
    Beginning,
    End,
    PageUp,
    PageDown,
    PasteStart,
    Ignore,
}

const KEY_BINDINGS: &[(&[u8], Action)] = &[
    (b"\x1b", Action::Cancel),
    (b"\x7f", Action::DeleteCharacter),
    (b"\x08", Action::DeleteCharacter),
    (b"\x17", Action::DeleteWord),
    (b"\x15", Action::DeleteToBeginning),
    (b"\x09", Action::Complete),
    (b"\x03", Action::Cancel),
    (b"\x04", Action::Cancel),
    (b"\x07", Action::Cancel),
    (b"\x0d", Action::Accept),
    (b"\x10", Action::Previous),
    (b"\x0e", Action::Next),
    (b"\x0b", Action::Previous),
    (b"\x0a", Action::Next),
    (b"\x01", Action::Beginning),
    (b"\x05", Action::End),
    (b"\x1bOD", Action::Left),
    (b"\x1b[D", Action::Left),
    (b"\x1bOC", Action::Right),
    (b"\x1b[C", Action::Right),
    (b"\x1b[1~", Action::Beginning),
    (b"\x1b[H", Action::Beginning),
    (b"\x1b[4~", Action::End),
    (b"\x1b[F", Action::End),
    (b"\x1b[A", Action::Previous),
    (b"\x1bOA", Action::Previous),
    (b"\x1b[B", Action::Next),
    (b"\x1bOB", Action::Next),
    (b"\x1b[5~", Action::PageUp),
    (b"\x1b[6~", Action::PageDown),
    (b"\x1b[200~", Action::PasteStart),
    (b"\x1b[201~", Action::Ignore),
];

enum InputResult {
    Continue { search_changed: bool },
    Accept,
    Cancel,
}

struct CandidateRow {
    score: Option<f64>,
    width: usize,
    max_rows: usize,
    selected: bool,
    multi: bool,
    marked: bool,
}

struct Editor {
    query: Vec<u8>,
    cursor: usize,
    multi: bool,
    focus_list: bool,
    marked: Vec<usize>,
    input: Vec<u8>,
    in_paste: bool,
    ambiguous_key_pending: bool,
    search_dirty: bool,
    lines: usize,
}

impl Editor {
    fn new(mut query: Vec<u8>, lines: usize, multi: bool) -> Self {
        query.truncate(4096);
        let cursor = query.len();
        Self {
            query,
            cursor,
            multi,
            focus_list: false,
            marked: Vec::new(),
            input: Vec::new(),
            in_paste: false,
            ambiguous_key_pending: false,
            search_dirty: false,
            lines,
        }
    }

    fn update_choices(&mut self, choices: &mut crate::Choices) {
        if self.search_dirty {
            choices.search(&self.query);
            self.search_dirty = false;
        }
    }

    fn insert(&mut self, byte: u8) -> bool {
        if self.query.len() == 4096 {
            return false;
        }
        self.query.insert(self.cursor, byte);
        self.cursor += 1;
        true
    }

    fn replace(&mut self, candidate: &[u8]) -> bool {
        let end = candidate.len().min(4096);
        if self.query == candidate[..end] {
            self.cursor = end;
            return false;
        }
        self.query.clear();
        self.query.extend_from_slice(&candidate[..end]);
        self.cursor = end;
        true
    }

    fn delete_character(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = previous_boundary(&self.query, self.cursor);
        self.query.drain(start..self.cursor);
        self.cursor = start;
        true
    }

    fn delete_word(&mut self) -> bool {
        let original = self.cursor;
        let mut cursor = original;
        while cursor > 0 && is_ascii_space(self.query[cursor - 1]) {
            cursor -= 1;
        }
        while cursor > 0 && !is_ascii_space(self.query[cursor - 1]) {
            cursor -= 1;
        }
        if cursor == original {
            return false;
        }
        self.query.drain(cursor..original);
        self.cursor = cursor;
        true
    }

    fn delete_to_beginning(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.query.drain(..self.cursor);
        self.cursor = 0;
        true
    }

    fn left(&mut self) {
        self.cursor = previous_boundary(&self.query, self.cursor);
    }

    fn right(&mut self) {
        self.cursor = next_boundary(&self.query, self.cursor);
    }
}

fn wrapped_rows(candidate: &[u8], prefix_width: usize, width: usize) -> usize {
    if width <= prefix_width {
        return 1;
    }
    let mut column = prefix_width;
    let mut rows = 1usize;
    let mut index = 0usize;
    while index < candidate.len() {
        let (scalar, length) = decode_char(&candidate[index..]);
        let char_width = if candidate[index] == b'\n' { 1 } else { scalar_width(scalar) };
        if char_width > 0 && column + char_width > width {
            rows += 1;
            column = char_width;
        } else {
            column += char_width;
        }
        index += length;
    }
    rows
}

fn decode_char(bytes: &[u8]) -> (u32, usize) {
    let first = bytes[0];
    if first < 0x80 {
        return (u32::from(first), 1);
    }
    let (mut scalar, length) = match first {
        0xC2..=0xDF => (u32::from(first & 0x1F), 2),
        0xE0..=0xEF => (u32::from(first & 0x0F), 3),
        0xF0..=0xF4 => (u32::from(first & 0x07), 4),
        _ => return (u32::from(first), 1),
    };
    if bytes.len() < length {
        return (u32::from(first), 1);
    }
    for &byte in bytes.iter().take(length).skip(1) {
        if byte & 0b1100_0000 != 0b1000_0000 {
            return (u32::from(first), 1);
        }
        scalar = (scalar << 6) | u32::from(byte & 0x3F);
    }
    let minimum = match length { 2 => 0x80, 3 => 0x800, _ => 0x1_0000 };
    if scalar < minimum || scalar > 0x10_FFFF || (0xD800..0xE000).contains(&scalar) {
        return (u32::from(first), 1);
    }
    (scalar, length)
}

fn scalar_width(scalar: u32) -> usize {
    if scalar < 0x20 || (0x7F..0xA0).contains(&scalar) {
        return 1;
    }
    if matches!(scalar,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF
        | 0x05C1..=0x05C2 | 0x05C4..=0x05C7 | 0x0610..=0x061A | 0x064B..=0x065F
        | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F)
    {
        return 0;
    }
    if matches!(scalar,
        0x1100..=0x115F | 0x2329..=0x232A | 0x2E80..=0x303E | 0x3041..=0x33FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD)
    {
        return 2;
    }
    1
}

fn is_printable(byte: u8) -> bool {
    byte >= 0x80 || (0x20..=0x7e).contains(&byte)
}

fn is_ascii_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

fn previous_boundary(bytes: &[u8], cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut position = cursor - 1;
    while position > 0 && is_continuation(bytes[position]) {
        position -= 1;
    }
    position
}

fn next_boundary(bytes: &[u8], cursor: usize) -> usize {
    if cursor >= bytes.len() {
        return bytes.len();
    }
    let mut position = cursor + 1;
    while position < bytes.len() && is_continuation(bytes[position]) {
        position += 1;
    }
    position
}
