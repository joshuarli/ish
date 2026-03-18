use std::os::unix::fs::PermissionsExt;

pub struct CompEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub is_exec: bool,
}

impl CompEntry {
    pub fn display_width(&self) -> usize {
        self.name.len() + if self.is_dir { 1 } else { 0 }
    }

    /// Display name: name for files, name + "/" for dirs.
    /// Returns Cow — borrows for files (zero alloc), allocates only for dirs.
    pub fn display_name(&self) -> std::borrow::Cow<'_, str> {
        if self.is_dir {
            let mut s = String::with_capacity(self.name.len() + 1);
            s.push_str(&self.name);
            s.push('/');
            std::borrow::Cow::Owned(s)
        } else {
            std::borrow::Cow::Borrowed(&self.name)
        }
    }

    /// Write the display name directly into a TermWriter — zero allocation.
    /// Prefer this on the rendering hot path over display_name().
    pub fn write_display_name(&self, tw: &mut crate::term::TermWriter) {
        tw.write_str(&self.name);
        if self.is_dir {
            tw.write_str("/");
        }
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
/// If `dirs_only` is true, only return directories (and symlinks to directories).
pub fn complete_path(partial: &str, dirs_only: bool) -> Vec<CompEntry> {
    let (dir, prefix) = split_path(partial);
    complete_in_dir(dir, prefix, dirs_only)
}

/// Complete entries in `dir` whose names start with `prefix`.
fn complete_in_dir(dir: &str, prefix: &str, dirs_only: bool) -> Vec<CompEntry> {
    let read_dir = match std::fs::read_dir(if dir.is_empty() { "." } else { dir }) {
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
            // Skip filenames with control characters (newlines, etc.)
            if name.bytes().any(|b| b < b' ' || b == 0x7f) {
                return None;
            }
            // Skip hidden files unless prefix starts with .
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            // Filter by prefix
            if !name.starts_with(prefix) {
                return None;
            }
            // e.metadata() follows symlinks, so symlinks to dirs count as dirs
            let meta = e.metadata().ok()?;
            let is_dir = meta.is_dir();
            if dirs_only && !is_dir {
                return None;
            }
            let is_link = e
                .path()
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            Some(CompEntry {
                is_dir,
                is_exec: !is_dir && meta.permissions().mode() & 0o111 != 0,
                is_link,
                name,
            })
        })
        .collect();

    entries.sort_by_key(|a| a.name.to_lowercase());
    entries
}

/// Fish-style partial path completion: each intermediate directory component
/// is treated as a prefix. e.g., "/home/user/de/s" finds entries starting
/// with "s" in /home/user/dev/, /home/user/Desktop/, etc.
/// Returns (resolved_dir_with_slash, entries) pairs.
pub fn complete_partial_path(partial: &str, dirs_only: bool) -> Vec<(String, Vec<CompEntry>)> {
    let (dir, prefix) = split_path(partial);
    if dir.is_empty() {
        return Vec::new();
    }

    let dir_trimmed = dir.trim_end_matches('/');

    // If dir already exists, complete_path handles it
    if std::fs::metadata(dir_trimmed)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Vec::new();
    }

    let resolved_dirs = resolve_partial_dir(dir_trimmed);
    let mut results = Vec::new();
    for rdir in resolved_dirs {
        let entries = complete_in_dir(&rdir, prefix, dirs_only);
        if !entries.is_empty() {
            results.push((format!("{rdir}/"), entries));
        }
    }
    results
}

/// Recursively resolve a directory path where each component is a prefix.
/// e.g., "/home/user/de" → ["/home/user/dev", "/home/user/Desktop", ...]
fn resolve_partial_dir(dir: &str) -> Vec<String> {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return Vec::new();
    }

    // Base: if it exists as a directory, return it
    if std::fs::metadata(dir).map(|m| m.is_dir()).unwrap_or(false) {
        return vec![dir.to_string()];
    }

    // Split parent / component
    let (parent, component) = match dir.rfind('/') {
        Some(0) => ("/", &dir[1..]),
        Some(i) => (&dir[..i], &dir[i + 1..]),
        None => (".", dir),
    };

    if component.is_empty() {
        return Vec::new();
    }

    // Recursively resolve parent
    let parents = resolve_partial_dir(parent);

    let mut results = Vec::new();
    for p in &parents {
        let rd = match std::fs::read_dir(p.as_str()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(component) {
                continue;
            }
            if name.starts_with('.') && !component.starts_with('.') {
                continue;
            }
            if !entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                continue;
            }
            if p == "/" {
                results.push(format!("/{name}"));
            } else if p == "." {
                results.push(name);
            } else {
                results.push(format!("{p}/{name}"));
            }
        }
        // Cap expansion to avoid combinatorial explosion
        if results.len() > 64 {
            results.truncate(64);
            break;
        }
    }
    results
}

/// Generate environment variable completions for a `$` prefix.
/// `partial` should include the `$` (e.g., `$PA`).
pub fn complete_env(partial: &str) -> Vec<CompEntry> {
    let prefix = partial.strip_prefix('$').unwrap_or(partial);
    let mut entries: Vec<CompEntry> = std::env::vars()
        .filter(|(key, _)| key.starts_with(prefix))
        .map(|(key, _)| CompEntry {
            name: key,
            is_dir: false,
            is_link: false,
            is_exec: false,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Split "path/to/pref" into ("path/to/", "pref").
/// Returns slices — no heap allocation.
fn split_path(partial: &str) -> (&str, &str) {
    match partial.rfind('/') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
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
        // Stack array for col widths — max 6 columns, no heap allocation.
        let mut col_widths = [0usize; 6];
        for (i, entry) in entries.iter().enumerate() {
            let col = i / rows;
            if col < cols {
                col_widths[col] = col_widths[col].max(entry.display_width());
            }
        }
        // Total width with 2-char gaps between columns
        let total: usize = col_widths[..cols].iter().sum::<usize>() + cols.saturating_sub(1) * 2;
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
        assert_eq!(split_path("foo"), ("", "foo"));
    }

    #[test]
    fn split_path_with_dir() {
        assert_eq!(split_path("src/ma"), ("src/", "ma"));
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

    #[test]
    fn partial_path_resolves_this_repo() {
        // "src" exists under ".", so "sr" should resolve to "src"
        let resolved = resolve_partial_dir("sr");
        assert!(
            resolved.iter().any(|d| d == "src"),
            "expected 'src' in {resolved:?}"
        );
    }

    #[test]
    fn partial_path_two_levels() {
        // "sr/m" should resolve to "src/main.rs" parent, i.e., "src"
        // but "m" as a directory — only if there's a dir starting with "m" in src/
        // There isn't, so this tests the resolution still works (returns src at least)
        let resolved = resolve_partial_dir("sr");
        assert!(!resolved.is_empty());
    }

    #[test]
    fn partial_path_complete_finds_entries() {
        // "./sr/m" → should find main.rs in src/
        let results = complete_partial_path("./sr/m", false);
        let names: Vec<&str> = results
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|e| e.name.as_str()))
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with("main")),
            "expected main.rs in {names:?}"
        );
    }

    #[test]
    fn partial_path_existing_dir_returns_empty() {
        // If the directory exists, complete_partial_path defers to complete_path
        let results = complete_partial_path("./src/m", false);
        assert!(results.is_empty());
    }

    #[test]
    fn partial_path_nonexistent_returns_empty() {
        let results = complete_partial_path("./zzzzz/m", false);
        assert!(results.is_empty());
    }

    #[test]
    fn partial_path_absolute_tilde_expanded() {
        // Simulate "~/de/s" with tilde already expanded
        let home = std::env::var("HOME").unwrap();
        let expanded = format!("{home}/de/s");
        let results = complete_partial_path(&expanded, true);
        let all_names: Vec<&str> = results
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|e| e.name.as_str()))
            .collect();
        assert!(
            all_names.contains(&"sentry"),
            "expected 'sentry' in {all_names:?}"
        );
    }
}
