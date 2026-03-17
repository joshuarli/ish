/// Line editing buffer with cursor and kill ring.
pub struct LineBuffer {
    buf: String,
    cursor: usize, // byte offset into buf
    kill_ring: String,
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

    pub fn into_string(self) -> String {
        self.buf
    }

    /// Set content and move cursor to end.
    pub fn set(&mut self, s: &str) {
        self.buf.clear();
        self.buf.push_str(s);
        self.cursor = self.buf.len();
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    /// Number of display columns from start of line to cursor.
    pub fn display_cursor_pos(&self) -> usize {
        self.buf[..self.cursor].chars().count()
    }

    /// Number of display columns for the full line.
    pub fn display_len(&self) -> usize {
        self.buf.chars().count()
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
        while self.cursor > 0 && self.char_before_cursor().is_some_and(|c| !c.is_whitespace()) {
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
        while self.cursor > 0 && self.char_before_cursor().is_some_and(|c| !c.is_whitespace()) {
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
            let text = self.kill_ring.clone();
            self.insert_str(&text);
        }
    }

    // -- Internal helpers --

    fn prev_char_boundary(&self) -> usize {
        let mut pos = self.cursor.saturating_sub(1);
        while pos > 0 && !self.buf.is_char_boundary(pos) {
            pos -= 1;
        }
        pos
    }

    fn next_char_boundary(&self) -> usize {
        let mut pos = self.cursor + 1;
        while pos < self.buf.len() && !self.buf.is_char_boundary(pos) {
            pos += 1;
        }
        pos.min(self.buf.len())
    }

    fn char_before_cursor(&self) -> Option<char> {
        self.buf[..self.cursor].chars().next_back()
    }

    fn char_at_cursor(&self) -> Option<char> {
        self.buf[self.cursor..].chars().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
