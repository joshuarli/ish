use std::os::unix::fs::PermissionsExt;

pub struct CompEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub is_exec: bool,
}

impl CompEntry {
    pub fn display_name(&self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name.clone()
        }
    }

    pub fn display_width(&self) -> usize {
        self.name.len() + if self.is_dir { 1 } else { 0 }
    }
}

pub struct CompletionState {
    pub entries: Vec<CompEntry>,
    pub selected: usize,
    pub cols: usize,
    pub rows: usize,
    pub scroll: usize,
    /// The prefix that was used to generate completions (directory portion).
    pub dir_prefix: String,
}

impl CompletionState {
    pub fn selected_entry(&self) -> Option<&CompEntry> {
        self.entries.get(self.selected)
    }

    pub fn move_up(&mut self) {
        if self.rows == 0 {
            return;
        }
        let row = self.selected % self.rows;
        let col = self.selected / self.rows;
        if row == 0 {
            // Wrap to previous column, last row
            if col > 0 {
                let prev_col = col - 1;
                let idx = prev_col * self.rows + self.rows - 1;
                self.selected = idx.min(self.entries.len() - 1);
            } else {
                // Wrap to last column
                let last_col = (self.entries.len().saturating_sub(1)) / self.rows;
                let idx = last_col * self.rows + self.rows - 1;
                self.selected = idx.min(self.entries.len() - 1);
            }
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.rows == 0 {
            return;
        }
        let row = self.selected % self.rows;
        let col = self.selected / self.rows;
        if row + 1 >= self.rows || self.selected + 1 >= self.entries.len() {
            // Wrap to next column, first row
            let next_col = col + 1;
            let idx = next_col * self.rows;
            if idx < self.entries.len() {
                self.selected = idx;
            } else {
                self.selected = 0;
            }
        } else {
            self.selected += 1;
        }
    }

    pub fn move_left(&mut self) {
        if self.rows == 0 {
            return;
        }
        let col = self.selected / self.rows;
        let row = self.selected % self.rows;
        if col == 0 {
            // Wrap to last column
            let last_col = (self.entries.len().saturating_sub(1)) / self.rows;
            let idx = last_col * self.rows + row;
            self.selected = idx.min(self.entries.len() - 1);
        } else {
            self.selected -= self.rows;
        }
    }

    pub fn move_right(&mut self) {
        if self.rows == 0 {
            return;
        }
        let col = self.selected / self.rows;
        let row = self.selected % self.rows;
        let next = (col + 1) * self.rows + row;
        if next < self.entries.len() {
            self.selected = next;
        } else {
            // Wrap to first column
            self.selected = row.min(self.entries.len() - 1);
        }
    }
}

/// Generate file completions for the given partial word.
pub fn complete_path(partial: &str) -> Vec<CompEntry> {
    let (dir, prefix) = split_path(partial);

    let read_dir = match std::fs::read_dir(if dir.is_empty() { "." } else { &dir }) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let mut entries: Vec<CompEntry> = read_dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // Skip . and ..
            if name == "." || name == ".." {
                return None;
            }
            // Skip hidden files unless prefix starts with .
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            // Filter by prefix
            if !name.starts_with(&prefix) {
                return None;
            }
            let meta = e.metadata().ok()?;
            let is_link = e
                .path()
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            Some(CompEntry {
                is_dir: meta.is_dir(),
                is_exec: !meta.is_dir() && meta.permissions().mode() & 0o111 != 0,
                is_link,
                name,
            })
        })
        .collect();

    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    entries
}

/// Split "path/to/pref" into ("path/to/", "pref").
fn split_path(partial: &str) -> (String, String) {
    match partial.rfind('/') {
        Some(i) => (partial[..=i].to_string(), partial[i + 1..].to_string()),
        None => (String::new(), partial.to_string()),
    }
}

/// Compute grid layout: (cols, rows) for column-major display.
pub fn compute_grid(entries: &[CompEntry], term_cols: u16) -> (usize, usize) {
    let n = entries.len();
    if n == 0 {
        return (0, 0);
    }

    let max_cols = 6.min(n);
    let term_w = term_cols as usize;

    for cols in (1..=max_cols).rev() {
        let rows = n.div_ceil(cols);
        // Compute column widths (column-major layout: entries[col*rows + row])
        let mut col_widths = vec![0usize; cols];
        for (i, entry) in entries.iter().enumerate() {
            let col = i / rows;
            if col < cols {
                col_widths[col] = col_widths[col].max(entry.display_width());
            }
        }
        // Total width with 2-char gaps between columns
        let total: usize = col_widths.iter().sum::<usize>() + cols.saturating_sub(1) * 2;
        if total <= term_w {
            return (cols, rows);
        }
    }

    (1, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_path_no_slash() {
        assert_eq!(split_path("foo"), ("".to_string(), "foo".to_string()));
    }

    #[test]
    fn split_path_with_dir() {
        assert_eq!(split_path("src/ma"), ("src/".to_string(), "ma".to_string()));
    }

    #[test]
    fn grid_computation() {
        let entries: Vec<CompEntry> = (0..7)
            .map(|i| CompEntry {
                name: format!("file{i}.rs"),
                is_dir: false,
                is_link: false,
                is_exec: false,
            })
            .collect();
        let (cols, rows) = compute_grid(&entries, 80);
        assert!(cols >= 1);
        assert!(rows >= 1);
        assert!(cols * rows >= 7);
    }
}
