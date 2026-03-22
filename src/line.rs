/// Line editing buffer with cursor and kill ring.
pub struct LineBuffer {
    buf: String,
    cursor: usize, // byte offset into buf
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

    /// Set content and move cursor to end.
    pub fn set(&mut self, s: &str) {
        self.buf.clear();
        self.buf.push_str(s);
        self.cursor = self.buf.len();
    }

    /// Set content and place cursor at the given byte offset.
    pub fn set_with_cursor(&mut self, s: &str, cursor: usize) {
        self.buf.clear();
        self.buf.push_str(s);
        self.cursor = cursor.min(self.buf.len());
        while self.cursor > 0 && !self.buf.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    /// Number of display columns from start of line to cursor.
    pub fn display_cursor_pos(&self) -> usize {
        if self.buf.is_ascii() {
            self.cursor
        } else {
            str_width(&self.buf[..self.cursor])
        }
    }

    /// Number of display columns for the full line.
    pub fn display_len(&self) -> usize {
        if self.buf.is_ascii() {
            self.buf.len()
        } else {
            str_width(&self.buf)
        }
    }

    // -- Insertion / Deletion --

    pub fn insert_char(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Delete char before cursor (Backspace).
    pub fn delete_back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = self.prev_char_boundary();
        self.buf.drain(prev..self.cursor);
        self.cursor = prev;
        true
    }

    /// Delete char at cursor (Delete / Ctrl+D).
    pub fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        let next = self.next_char_boundary();
        self.buf.drain(self.cursor..next);
        true
    }

    // -- Cursor Movement --

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

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    pub fn move_word_left(&mut self) {
        // Skip whitespace, then skip non-whitespace
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
        // Skip non-whitespace, then skip whitespace
        while self.cursor < len && self.char_at_cursor().is_some_and(|c| !c.is_whitespace()) {
            self.cursor = self.next_char_boundary();
        }
        while self.cursor < len && self.char_at_cursor().is_some_and(|c| c.is_whitespace()) {
            self.cursor = self.next_char_boundary();
        }
    }

    // -- Multiline Navigation --

    /// Whether the buffer contains newline characters.
    pub fn has_newlines(&self) -> bool {
        self.buf.contains('\n')
    }

    /// True if cursor is on the first line (no `\n` before it).
    pub fn on_first_line(&self) -> bool {
        !self.buf[..self.cursor].contains('\n')
    }

    /// True if cursor is on the last line (no `\n` after it).
    pub fn on_last_line(&self) -> bool {
        !self.buf[self.cursor..].contains('\n')
    }

    /// Move cursor up one line, preserving column position.
    /// Returns false if already on the first line.
    pub fn move_line_up(&mut self) -> bool {
        // Find start of current line
        let cur_line_start = match self.buf[..self.cursor].rfind('\n') {
            Some(i) => i + 1,
            None => return false, // already on first line
        };
        let col = self.cursor - cur_line_start;

        // Find start of previous line
        let prev_line_start = self.buf[..cur_line_start - 1]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prev_line_len = (cur_line_start - 1) - prev_line_start;

        self.cursor = prev_line_start + col.min(prev_line_len);
        true
    }

    /// Move cursor down one line, preserving column position.
    /// Returns false if already on the last line.
    pub fn move_line_down(&mut self) -> bool {
        // Find start of current line
        let cur_line_start = self.buf[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let col = self.cursor - cur_line_start;

        // Find end of current line (\n position)
        let cur_line_end = match self.buf[self.cursor..].find('\n') {
            Some(i) => self.cursor + i,
            None => return false, // already on last line
        };

        let next_line_start = cur_line_end + 1;
        let next_line_end = self.buf[next_line_start..]
            .find('\n')
            .map(|i| next_line_start + i)
            .unwrap_or(self.buf.len());
        let next_line_len = next_line_end - next_line_start;

        self.cursor = next_line_start + col.min(next_line_len);
        true
    }

    // -- Kill Ring Operations --

    /// Kill from cursor to end of line (Ctrl+K).
    pub fn kill_to_end(&mut self) {
        if self.cursor < self.buf.len() {
            self.kill_ring = self.buf[self.cursor..].to_string();
            self.buf.truncate(self.cursor);
        }
    }

    /// Kill from start to cursor (Ctrl+U).
    pub fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.kill_ring = self.buf[..self.cursor].to_string();
            self.buf.drain(..self.cursor);
            self.cursor = 0;
        }
    }

    /// Kill word backward (Ctrl+W).
    pub fn kill_word_back(&mut self) {
        let end = self.cursor;
        // Skip whitespace, then skip non-whitespace
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

    /// Yank (paste) from kill ring (Ctrl+Y).
    pub fn yank(&mut self) {
        if !self.kill_ring.is_empty() {
            // Take the kill ring to avoid borrow conflict with insert_str,
            // then restore it. mem::take replaces with empty String (no alloc).
            let text = std::mem::take(&mut self.kill_ring);
            self.insert_str(&text);
            self.kill_ring = text;
        }
    }

    // -- Internal helpers --

    fn prev_char_boundary(&self) -> usize {
        if self.cursor == 0 {
            return 0;
        }
        // ASCII fast path: previous byte is always a char boundary
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
        // ASCII fast path: next byte is always a char boundary
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
        // ASCII fast path
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
        // ASCII fast path
        let b = self.buf.as_bytes()[self.cursor];
        if b < 0x80 {
            return Some(b as char);
        }
        self.buf[self.cursor..].chars().next()
    }
}

/// Display width of a string in terminal columns.
pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Display width of a character in terminal columns.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x7F {
        return if cp >= 0x20 { 1 } else { 0 };
    }
    // Control chars, soft hyphen
    if cp < 0xA0 || cp == 0xAD {
        return 0;
    }
    // Zero-width characters
    if matches!(cp, 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0xFEFF) {
        return 0;
    }
    // Combining marks
    if matches!(cp,
        0x0300..=0x036F   // Combining Diacritical Marks
        | 0x1AB0..=0x1AFF // Combining Diacritical Marks Extended
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x20D0..=0x20FF // Combining Marks for Symbols
        | 0xFE20..=0xFE2F // Combining Half Marks
    ) {
        return 0;
    }
    if is_wide(cp) { 2 } else { 1 }
}

fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F   // Hangul Jamo
        | 0x231A..=0x231B // Watch, Hourglass
        | 0x2329..=0x232A // Angle brackets
        | 0x23E9..=0x23F3 // Various symbols
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
        | 0x2E80..=0x303E  // CJK Radicals, Kangxi, Ideographic, CJK Symbols
        | 0x3040..=0x33BF  // Hiragana, Katakana, Bopomofo, Hangul Compat, Kanbun
        | 0x3400..=0x4DBF  // CJK Extension A
        | 0x4E00..=0xA4CF  // CJK Unified, Yi
        | 0xA960..=0xA97C  // Hangul Jamo Extended-A
        | 0xAC00..=0xD7A3  // Hangul Syllables
        | 0xF900..=0xFAFF  // CJK Compatibility Ideographs
        | 0xFE10..=0xFE19  // Vertical Forms
        | 0xFE30..=0xFE6B  // CJK Compatibility Forms
        | 0xFF01..=0xFF60  // Fullwidth Forms
        | 0xFFE0..=0xFFE6  // Fullwidth Signs
        | 0x1F004
        | 0x1F0CF
        | 0x1F170..=0x1F171
        | 0x1F17E..=0x1F17F
        | 0x1F18E
        | 0x1F191..=0x1F19A
        | 0x1F1E0..=0x1F1FF // Regional Indicators (flags)
        | 0x1F200..=0x1F202
        | 0x1F210..=0x1F23B
        | 0x1F240..=0x1F248
        | 0x1F250..=0x1F251
        | 0x1F260..=0x1F265
        | 0x1F300..=0x1F9FF // Misc Symbols, Emoticons, Transport, etc.
        | 0x1FA00..=0x1FA6F
        | 0x1FA70..=0x1FAFF
        | 0x20000..=0x2FA1F // CJK Extension B-F + Compatibility
        | 0x30000..=0x3134F // CJK Extension G
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_widths() {
        // ASCII
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width(' '), 1);
        // Control
        assert_eq!(char_width('\0'), 0);
        assert_eq!(char_width('\x1b'), 0);
        // Fullwidth
        assert_eq!(char_width('｜'), 2); // U+FF5C fullwidth vertical bar
        assert_eq!(char_width('Ａ'), 2); // U+FF21 fullwidth A
        // CJK
        assert_eq!(char_width('漢'), 2);
        assert_eq!(char_width('ア'), 2); // katakana
        // Regular non-ASCII (Latin extended, etc.)
        assert_eq!(char_width('é'), 1);
        assert_eq!(char_width('ñ'), 1);
        // Combining marks
        assert_eq!(char_width('\u{0301}'), 0); // combining acute accent
    }

    #[test]
    fn str_widths() {
        assert_eq!(str_width("hello"), 5);
        assert_eq!(str_width("a｜b"), 4); // 1 + 2 + 1
        assert_eq!(str_width("漢字"), 4);
        assert_eq!(str_width("café"), 4); // all width-1
    }

    #[test]
    fn display_with_fullwidth() {
        let mut lb = LineBuffer::new();
        lb.set("a｜b"); // 'a'(1) + '｜'(2) + 'b'(1) = 4 columns
        assert_eq!(lb.display_len(), 4);
        assert_eq!(lb.display_cursor_pos(), 4); // cursor at end
        lb.move_left(); // before 'b'
        assert_eq!(lb.display_cursor_pos(), 3); // 1 + 2 = 3
        lb.move_left(); // before '｜'
        assert_eq!(lb.display_cursor_pos(), 1); // just 'a' = 1
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
        // Clamp to end
        lb.set_with_cursor("hi", 100);
        assert_eq!(lb.cursor(), 2);
        // Position 0
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
    fn kill_to_start_and_yank() {
        let mut lb = LineBuffer::new();
        lb.set("hello world");
        lb.move_home();
        lb.move_word_right();
        // move_word_right skips non-ws then ws, cursor is at 'w' (pos 6)
        lb.kill_to_start();
        assert_eq!(lb.text(), "world");
        lb.move_end();
        lb.yank();
        assert_eq!(lb.text(), "worldhello ");
    }
}
