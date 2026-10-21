#[derive(Clone)]
pub struct LineBuffer {
    buf: String,
    cursor: usize,
    kill_ring: String,
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LineBuffer {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
            kill_ring: String::new(),
        }
    }

    pub fn text(&self) -> &str {
        &self.buf
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn set(&mut self, s: &str) {
        self.buf.clear();
        self.buf.push_str(s);
        self.cursor = self.buf.len();
    }

    pub fn set_with_cursor(&mut self, s: &str, cursor: usize) {
        self.buf.clear();
        self.buf.push_str(s);
        self.cursor = cursor.min(self.buf.len());
        while self.cursor > 0 && !self.buf.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    pub fn display_cursor_pos(&self) -> usize {
        if self.buf.is_ascii() {
            self.cursor
        } else {
            str_width(&self.buf[..self.cursor])
        }
    }

    pub fn display_len(&self) -> usize {
        if self.buf.is_ascii() {
            self.buf.len()
        } else {
            str_width(&self.buf)
        }
    }


    pub fn insert_char(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn delete_back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = self.prev_char_boundary();
        self.buf.drain(prev..self.cursor);
        self.cursor = prev;
        true
    }

    pub fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        let next = self.next_char_boundary();
        self.buf.drain(self.cursor..next);
        true
    }


    pub fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = self.prev_char_boundary();
        true
    }

    pub fn move_right(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        self.cursor = self.next_char_boundary();
        true
    }

    pub fn grid_cursor_position(
        &self,
        prompt_display_len: usize,
        term_cols: u16,
    ) -> (usize, usize) {
        let cols = usize::from(term_cols).max(1);
        let total = prompt_display_len + self.display_cursor_pos();
        (total / cols, total % cols)
    }

    pub fn move_grid_up(
        &mut self,
        prompt_display_len: usize,
        term_cols: u16,
        target_col: usize,
    ) -> bool {
        self.move_grid(-1, prompt_display_len, term_cols, target_col)
    }

    pub fn move_grid_down(
        &mut self,
        prompt_display_len: usize,
        term_cols: u16,
        target_col: usize,
    ) -> bool {
        self.move_grid(1, prompt_display_len, term_cols, target_col)
    }

    fn move_grid(
        &mut self,
        direction: isize,
        prompt_display_len: usize,
        term_cols: u16,
        target_col: usize,
    ) -> bool {
        debug_assert!(!self.has_newlines());
        if self.buf.is_empty() {
            return false;
        }
        let cols = usize::from(term_cols).max(1);
        let (row, _) = self.grid_cursor_position(prompt_display_len, term_cols);
        let last_row = (prompt_display_len + self.display_len()) / cols;
        let target_row = if direction < 0 {
            let first_input_row = prompt_display_len / cols;
            if row > first_input_row {
                Some(row - 1)
            } else {
                None
            }
        } else if row < last_row {
            Some(row + 1)
        } else {
            None
        };
        let Some(target_row) = target_row else {
            return false;
        };

        let target_total = target_row * cols + target_col.min(cols - 1);
        let target_display = target_total.saturating_sub(prompt_display_len);
        self.cursor = self.byte_offset_for_display_pos(target_display);
        true
    }

    fn byte_offset_for_display_pos(&self, target: usize) -> usize {
        if target == 0 {
            return 0;
        }
        let mut display = 0;
        for (byte, ch) in self.buf.char_indices() {
            let next = display + char_width(ch);
            if target < next {
                return byte;
            }
            display = next;
            if target == display {
                return byte + ch.len_utf8();
            }
        }
        self.buf.len()
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    pub fn move_word_left(&mut self) {
        while self.cursor > 0 && self.char_before_cursor().is_some_and(|c| c.is_whitespace()) {
            self.cursor = self.prev_char_boundary();
        }
        while self.cursor > 0
            && self
                .char_before_cursor()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.cursor = self.prev_char_boundary();
        }
    }

    pub fn move_word_right(&mut self) {
        let len = self.buf.len();
        while self.cursor < len && self.char_at_cursor().is_some_and(|c| !c.is_whitespace()) {
            self.cursor = self.next_char_boundary();
        }
        while self.cursor < len && self.char_at_cursor().is_some_and(|c| c.is_whitespace()) {
            self.cursor = self.next_char_boundary();
        }
    }


    pub fn has_newlines(&self) -> bool {
        self.buf.contains('\n')
    }

    pub fn on_first_line(&self) -> bool {
        !self.buf[..self.cursor].contains('\n')
    }

    pub fn on_last_line(&self) -> bool {
        !self.buf[self.cursor..].contains('\n')
    }

    pub fn move_line_up(&mut self) -> bool {
        let cur_line_start = match self.buf[..self.cursor].rfind('\n') {
            Some(i) => i + 1,
            None => return false,
        };
        let col = str_width(&self.buf[cur_line_start..self.cursor]);

        let prev_line_start = self.buf[..cur_line_start - 1]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor =
            prev_line_start + offset_at_column(&self.buf[prev_line_start..cur_line_start - 1], col);
        true
    }

    pub fn move_line_down(&mut self) -> bool {
        let cur_line_start = self.buf[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let col = str_width(&self.buf[cur_line_start..self.cursor]);

        let cur_line_end = match self.buf[self.cursor..].find('\n') {
            Some(i) => self.cursor + i,
            None => return false,
        };

        let next_line_start = cur_line_end + 1;
        let next_line_end = self.buf[next_line_start..]
            .find('\n')
            .map(|i| next_line_start + i)
            .unwrap_or(self.buf.len());
        self.cursor =
            next_line_start + offset_at_column(&self.buf[next_line_start..next_line_end], col);
        true
    }


    pub fn kill_to_end(&mut self) {
        if self.cursor < self.buf.len() {
            self.kill_ring = self.buf[self.cursor..].to_string();
            self.buf.truncate(self.cursor);
        }
    }

    pub fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.kill_ring = self.buf[..self.cursor].to_string();
            self.buf.drain(..self.cursor);
            self.cursor = 0;
        }
    }

    pub fn kill_word_back(&mut self) {
        let end = self.cursor;
        while self.cursor > 0 && self.char_before_cursor().is_some_and(|c| c.is_whitespace()) {
            self.cursor = self.prev_char_boundary();
        }
        while self.cursor > 0
            && self
                .char_before_cursor()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.cursor = self.prev_char_boundary();
        }
        if self.cursor < end {
            self.kill_ring = self.buf[self.cursor..end].to_string();
            self.buf.drain(self.cursor..end);
        }
    }

    pub fn kill_word_forward(&mut self) {
        let start = self.cursor;
        let len = self.buf.len();
        while self.cursor < len && self.char_at_cursor().is_some_and(|c| c.is_whitespace()) {
            self.cursor = self.next_char_boundary();
        }
        while self.cursor < len && self.char_at_cursor().is_some_and(|c| !c.is_whitespace()) {
            self.cursor = self.next_char_boundary();
        }
        if start < self.cursor {
            self.kill_ring = self.buf[start..self.cursor].to_string();
            self.buf.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    pub fn yank(&mut self) {
        if !self.kill_ring.is_empty() {
            let text = std::mem::take(&mut self.kill_ring);
            self.insert_str(&text);
            self.kill_ring = text;
        }
    }


    fn prev_char_boundary(&self) -> usize {
        if self.cursor == 0 {
            return 0;
        }
        let prev = self.cursor - 1;
        if self.buf.as_bytes()[prev] < 0x80 {
            return prev;
        }
        let mut pos = prev;
        while pos > 0 && !self.buf.is_char_boundary(pos) {
            pos -= 1;
        }
        pos
    }

    fn next_char_boundary(&self) -> usize {
        let next = self.cursor + 1;
        if next >= self.buf.len() {
            return self.buf.len();
        }
        if self.buf.as_bytes()[self.cursor] < 0x80 {
            return next;
        }
        let mut pos = next;
        while pos < self.buf.len() && !self.buf.is_char_boundary(pos) {
            pos += 1;
        }
        pos.min(self.buf.len())
    }

    fn char_before_cursor(&self) -> Option<char> {
        if self.cursor == 0 {
            return None;
        }
        let b = self.buf.as_bytes()[self.cursor - 1];
        if b < 0x80 {
            return Some(b as char);
        }
        self.buf[..self.cursor].chars().next_back()
    }

    fn char_at_cursor(&self) -> Option<char> {
        if self.cursor >= self.buf.len() {
            return None;
        }
        let b = self.buf.as_bytes()[self.cursor];
        if b < 0x80 {
            return Some(b as char);
        }
        self.buf[self.cursor..].chars().next()
    }
}

fn offset_at_column(line: &str, col: usize) -> usize {
    let mut width = 0;
    for (i, c) in line.char_indices() {
        if width >= col {
            return i;
        }
        width += char_width(c);
    }
    line.len()
}

pub fn display_char(c: char) -> char {
    if c.is_control() { '\u{FFFD}' } else { c }
}

pub fn str_width(s: &str) -> usize {
    let mut width = 0;
    let mut after_joiner = false;
    for c in s.chars() {
        if !after_joiner {
            width += char_width(c);
        }
        after_joiner = c == '\u{200D}';
    }
    width
}

pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x7F {
        return 1;
    }
    if c.is_control() {
        return 1;
    }
    if cp == 0xAD {
        return 0;
    }
    if matches!(cp, 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0xFEFF) {
        return 0;
    }
    if matches!(cp, 0xFE00..=0xFE0F | 0x1F3FB..=0x1F3FF) {
        return 0;
    }
    if matches!(cp,
        0x0300..=0x036F
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20FF
        | 0xFE20..=0xFE2F
    ) {
        return 0;
    }
    if is_wide(cp) { 2 } else { 1 }
}

fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F
        | 0x231A..=0x231B
        | 0x2329..=0x232A
        | 0x23E9..=0x23F3
        | 0x23F8..=0x23FA
        | 0x25FD..=0x25FE
        | 0x2614..=0x2615
        | 0x2648..=0x2653
        | 0x267F
        | 0x2693
        | 0x26A1
        | 0x26AA..=0x26AB
        | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5
        | 0x26CE
        | 0x26D4
        | 0x26EA
        | 0x26F2..=0x26F3
        | 0x26F5
        | 0x26FA
        | 0x26FD
        | 0x2702
        | 0x2705
        | 0x2708..=0x270D
        | 0x270F
        | 0x2712
        | 0x2714
        | 0x2716
        | 0x271D
        | 0x2721
        | 0x2728
        | 0x2733..=0x2734
        | 0x2744
        | 0x2747
        | 0x274C
        | 0x274E
        | 0x2753..=0x2755
        | 0x2757
        | 0x2763..=0x2764
        | 0x2795..=0x2797
        | 0x27A1
        | 0x27B0
        | 0x27BF
        | 0x2934..=0x2935
        | 0x2B05..=0x2B07
        | 0x2B1B..=0x2B1C
        | 0x2B50
        | 0x2B55
        | 0x2E80..=0x303E
        | 0x3040..=0x33BF
        | 0x3400..=0x4DBF
        | 0x4E00..=0xA4CF
        | 0xA960..=0xA97C
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6B
        | 0xFF01..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F004
        | 0x1F0CF
        | 0x1F170..=0x1F171
        | 0x1F17E..=0x1F17F
        | 0x1F18E
        | 0x1F191..=0x1F19A
        | 0x1F1E0..=0x1F1FF
        | 0x1F200..=0x1F202
        | 0x1F210..=0x1F23B
        | 0x1F240..=0x1F248
        | 0x1F250..=0x1F251
        | 0x1F260..=0x1F265
        | 0x1F300..=0x1F9FF
        | 0x1FA00..=0x1FA6F
        | 0x1FA70..=0x1FAFF
        | 0x20000..=0x2FA1F
        | 0x30000..=0x3134F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_sequences_and_controls_have_terminal_widths() {
        assert_eq!(str_width("\u{2764}\u{FE0F}"), 2);
        assert_eq!(str_width("\u{1F44D}\u{1F3FD}"), 2);
        assert_eq!(str_width("\u{1F468}\u{200D}\u{1F469}"), 2);
        assert_eq!(str_width("\t"), 1);
        assert_eq!(display_char('\t'), '\u{FFFD}');
    }

    #[test]
    fn vertical_motion_never_lands_inside_a_char() {
        let mut line = LineBuffer::new();
        line.set("\u{e9}a\nxyz");
        line.set_with_cursor("\u{e9}a\nxyz", "\u{e9}a\nx".len());
        assert!(line.move_line_up());
        assert!(line.text().is_char_boundary(line.cursor()));
        line.move_line_down();
        assert!(line.text().is_char_boundary(line.cursor()));
    }

    #[test]
    fn char_widths() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width(' '), 1);
        assert_eq!(char_width('\0'), 1);
        assert_eq!(char_width('\x1b'), 1);
        assert_eq!(char_width('｜'), 2);
        assert_eq!(char_width('Ａ'), 2);
        assert_eq!(char_width('漢'), 2);
        assert_eq!(char_width('ア'), 2);
        assert_eq!(char_width('é'), 1);
        assert_eq!(char_width('ñ'), 1);
        assert_eq!(char_width('\u{0301}'), 0);
    }

    #[test]
    fn str_widths() {
        assert_eq!(str_width("hello"), 5);
        assert_eq!(str_width("a｜b"), 4);
        assert_eq!(str_width("漢字"), 4);
        assert_eq!(str_width("café"), 4);
    }

    #[test]
    fn display_with_fullwidth() {
        let mut lb = LineBuffer::new();
        lb.set("a｜b");
        assert_eq!(lb.display_len(), 4);
        assert_eq!(lb.display_cursor_pos(), 4);
        lb.move_left();
        assert_eq!(lb.display_cursor_pos(), 3);
        lb.move_left();
        assert_eq!(lb.display_cursor_pos(), 1);
    }

    #[test]
    fn insert_and_cursor() {
        let mut lb = LineBuffer::new();
        lb.insert_char('h');
        lb.insert_char('i');
        assert_eq!(lb.text(), "hi");
        assert_eq!(lb.cursor(), 2);
        assert_eq!(lb.display_cursor_pos(), 2);
    }

    #[test]
    fn set_with_cursor_position() {
        let mut lb = LineBuffer::new();
        lb.set_with_cursor("hello world", 5);
        assert_eq!(lb.text(), "hello world");
        assert_eq!(lb.cursor(), 5);
        lb.set_with_cursor("hi", 100);
        assert_eq!(lb.cursor(), 2);
        lb.set_with_cursor("test", 0);
        assert_eq!(lb.cursor(), 0);
    }

    #[test]
    fn delete_back() {
        let mut lb = LineBuffer::new();
        lb.insert_str("hello");
        assert!(lb.delete_back());
        assert_eq!(lb.text(), "hell");
    }

    #[test]
    fn move_word() {
        let mut lb = LineBuffer::new();
        lb.set("hello world");
        lb.move_word_left();
        assert_eq!(lb.display_cursor_pos(), 6);
        lb.move_word_left();
        assert_eq!(lb.display_cursor_pos(), 0);
        lb.move_word_right();
        assert_eq!(lb.display_cursor_pos(), 6);
    }

    #[test]
    fn kill_word_back() {
        let mut lb = LineBuffer::new();
        lb.set("hello world");
        lb.kill_word_back();
        assert_eq!(lb.text(), "hello ");
    }

    #[test]
    fn kill_word_forward() {
        let mut lb = LineBuffer::new();
        lb.set("hello world");
        lb.move_home();
        lb.kill_word_forward();
        assert_eq!(lb.text(), " world");
        assert_eq!(lb.cursor(), 0);
    }

    #[test]
    fn kill_to_start_and_yank() {
        let mut lb = LineBuffer::new();
        lb.set("hello world");
        lb.move_home();
        lb.move_word_right();
        lb.kill_to_start();
        assert_eq!(lb.text(), "world");
        lb.move_end();
        lb.yank();
        assert_eq!(lb.text(), "worldhello ");
    }
}
