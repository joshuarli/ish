pub struct CompEntry {
    name_start: u32,
    name_len: u16,
    pub is_dir: bool,
    pub is_link: bool,
    pub is_exec: bool,
}

impl CompEntry {
    pub fn display_width(&self) -> usize {
        self.name_len as usize + if self.is_dir { 1 } else { 0 }
    }
}

/// Arena-backed completion results. All entry names are stored contiguously
/// in `names`; each `CompEntry` stores an offset+length into it.
/// Typical completion: 2 heap allocations total (the arena String + entries Vec).
pub struct Completions {
    pub names: String,
    pub entries: Vec<CompEntry>,
}

impl Default for Completions {
    fn default() -> Self {
        Self::new()
    }
}

impl Completions {
    pub fn new() -> Self {
        Self {
            names: String::new(),
            entries: Vec::new(),
        }
    }

    pub fn with_capacity(names_cap: usize, entries_cap: usize) -> Self {
        Self {
            names: String::with_capacity(names_cap),
            entries: Vec::with_capacity(entries_cap),
        }
    }

    pub fn clear(&mut self) {
        self.names.clear();
        self.entries.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Get the name of an entry by index.
    pub fn name(&self, idx: usize) -> &str {
        let e = &self.entries[idx];
        &self.names[e.name_start as usize..][..e.name_len as usize]
    }

    /// Get the name of an entry reference.
    pub fn entry_name(&self, e: &CompEntry) -> &str {
        &self.names[e.name_start as usize..][..e.name_len as usize]
    }

    pub fn push(&mut self, name: &str, is_dir: bool, is_link: bool, is_exec: bool) {
        let start = self.names.len() as u32;
        self.names.push_str(name);
        self.entries.push(CompEntry {
            name_start: start,
            name_len: name.len() as u16,
            is_dir,
            is_link,
            is_exec,
        });
    }

    /// Begin a name by recording the current arena position.
    /// Call `finish_entry` after pushing name parts to `names`.
    pub fn begin_entry(&self) -> u32 {
        self.names.len() as u32
    }

    /// Finish an entry whose name starts at `start` in the arena.
    pub fn finish_entry(&mut self, start: u32, is_dir: bool, is_link: bool, is_exec: bool) {
        let name_len = (self.names.len() - start as usize) as u16;
        self.entries.push(CompEntry {
            name_start: start,
            name_len,
            is_dir,
            is_link,
            is_exec,
        });
    }

    /// Sort entries case-insensitively by name.
    pub fn sort_entries(&mut self) {
        let n = self.entries.len();
        if n <= 1 {
            return;
        }
        let names = self.names.as_bytes();
        if n <= 40 {
            // Insertion sort — O(n²) but minimal overhead for small N.
            for i in 1..n {
                let mut j = i;
                while j > 0
                    && cmp_icase_arena(names, &self.entries[j], &self.entries[j - 1])
                        == std::cmp::Ordering::Less
                {
                    self.entries.swap(j, j - 1);
                    j -= 1;
                }
            }
        } else {
            self.entries
                .sort_unstable_by(|a, b| cmp_icase_arena(names, a, b));
        }
    }
}

/// Case-insensitive byte-level comparator for arena-backed entries.
/// Inlined into sort hot path — no iterator overhead.
#[inline(always)]
fn cmp_icase_arena(names: &[u8], a: &CompEntry, b: &CompEntry) -> std::cmp::Ordering {
    let a_bytes = &names[a.name_start as usize..][..a.name_len as usize];
    let b_bytes = &names[b.name_start as usize..][..b.name_len as usize];
    let len = a_bytes.len().min(b_bytes.len());
    let mut i = 0;
    while i < len {
        let mut ab = a_bytes[i];
        let mut bb = b_bytes[i];
        // ASCII lowercase: branchless for the common case
        ab += (ab.is_ascii_uppercase() as u8) * 32;
        bb += (bb.is_ascii_uppercase() as u8) * 32;
        if ab != bb {
            return if ab < bb {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        i += 1;
    }
    a_bytes.len().cmp(&b_bytes.len())
}

pub struct CompletionState {
    pub comp: Completions,
    pub selected: usize,
    pub cols: usize,
    pub rows: usize,
    pub scroll: usize,
    /// The prefix that was used to generate completions (directory portion).
    pub dir_prefix: String,
}

impl CompletionState {
    pub fn selected_name(&self) -> Option<&str> {
        if self.selected < self.comp.len() {
            Some(self.comp.name(self.selected))
        } else {
            None
        }
    }

    pub fn selected_entry(&self) -> Option<&CompEntry> {
        self.comp.entries.get(self.selected)
    }

    /// Write the display name of entry `idx` into a TermWriter — zero allocation.
    pub fn write_display_name(&self, idx: usize, tw: &mut crate::term::TermWriter) {
        let e = &self.comp.entries[idx];
        let name = &self.comp.names[e.name_start as usize..][..e.name_len as usize];
        tw.write_str(name);
        if e.is_dir {
            tw.write_str("/");
        }
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
                self.selected = idx.min(self.comp.entries.len() - 1);
            } else {
                // Wrap to last column
                let last_col = (self.comp.entries.len().saturating_sub(1)) / self.rows;
                let idx = last_col * self.rows + self.rows - 1;
                self.selected = idx.min(self.comp.entries.len() - 1);
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
        if row + 1 >= self.rows || self.selected + 1 >= self.comp.entries.len() {
            // Wrap to next column, first row
            let next_col = col + 1;
            let idx = next_col * self.rows;
            if idx < self.comp.entries.len() {
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
            let last_col = (self.comp.entries.len().saturating_sub(1)) / self.rows;
            let idx = last_col * self.rows + row;
            self.selected = idx.min(self.comp.entries.len() - 1);
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
        if next < self.comp.entries.len() {
            self.selected = next;
        } else {
            // Wrap to first column
            self.selected = row.min(self.comp.entries.len() - 1);
        }
    }
}

/// Generate file completions for the given partial word.
/// If `dirs_only` is true, only return directories (and symlinks to directories).
pub fn complete_path(partial: &str, dirs_only: bool) -> Completions {
    let (dir, prefix) = split_path(partial);
    let mut comp = Completions::new();
    complete_in_dir(dir, prefix, dirs_only, &mut comp);
    comp
}

/// Like `complete_path` but appends into a caller-owned `Completions` (zero-alloc reuse).
pub fn complete_path_into(partial: &str, dirs_only: bool, comp: &mut Completions) {
    let (dir, prefix) = split_path(partial);
    complete_in_dir(dir, prefix, dirs_only, comp);
}

/// Complete entries in `dir` whose names start with `prefix`.
/// Uses libc opendir/readdir + stat directly. Names are appended to the arena.
fn complete_in_dir(dir: &str, prefix: &str, dirs_only: bool, comp: &mut Completions) {
    let dir_path = if dir.is_empty() { "." } else { dir };

    // Build NUL-terminated dir path on stack
    let dir_bytes = dir_path.as_bytes();
    let mut dir_buf = [0u8; 4096];
    if dir_bytes.len() >= dir_buf.len() {
        return;
    }
    dir_buf[..dir_bytes.len()].copy_from_slice(dir_bytes);
    dir_buf[dir_bytes.len()] = 0;

    // SAFETY: dir_buf is NUL-terminated, opendir is safe for valid paths.
    let dp = unsafe { libc::opendir(dir_buf.as_ptr() as *const libc::c_char) };
    if dp.is_null() {
        return;
    }

    let prefix_bytes = prefix.as_bytes();

    // Stack buffer for "dir/name\0" used by stat/lstat
    let mut path_buf = [0u8; 4096];
    let dir_prefix_len = if dir_path == "." {
        0
    } else {
        let len = dir_bytes.len();
        path_buf[..len].copy_from_slice(dir_bytes);
        if dir_bytes.last() != Some(&b'/') {
            path_buf[len] = b'/';
            len + 1
        } else {
            len
        }
    };

    loop {
        // SAFETY: dp is a valid DIR* from opendir above.
        let ent = unsafe { libc::readdir(dp) };
        if ent.is_null() {
            break;
        }

        // SAFETY: d_name is a NUL-terminated C string within the dirent.
        let name_cstr = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let name_bytes = name_cstr.to_bytes();

        // Skip . and ..
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        // Skip filenames with control characters
        if name_bytes.iter().any(|&b| b < b' ' || b == 0x7f) {
            continue;
        }
        // Skip hidden files unless prefix starts with .
        if name_bytes.first() == Some(&b'.') && !prefix_bytes.starts_with(b".") {
            continue;
        }
        // Filter by prefix
        if !name_bytes.starts_with(prefix_bytes) {
            continue;
        }

        // Build full path for stat: "dir/name\0"
        let total = dir_prefix_len + name_bytes.len();
        if total >= path_buf.len() {
            continue;
        }
        path_buf[dir_prefix_len..total].copy_from_slice(name_bytes);
        path_buf[total] = 0;

        // stat follows symlinks (so symlink-to-dir counts as dir)
        // SAFETY: path_buf is NUL-terminated, stat writes into stack struct.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::stat(path_buf.as_ptr() as *const libc::c_char, &mut st) } != 0 {
            continue;
        }
        let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if dirs_only && !is_dir {
            continue;
        }

        // lstat to detect symlinks
        let mut lst: libc::stat = unsafe { std::mem::zeroed() };
        let is_link = unsafe { libc::lstat(path_buf.as_ptr() as *const libc::c_char, &mut lst) }
            == 0
            && lst.st_mode & libc::S_IFMT == libc::S_IFLNK;

        let is_exec = !is_dir && st.st_mode & 0o111 != 0;

        let name = match std::str::from_utf8(name_bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };

        comp.push(name, is_dir, is_link, is_exec);
    }

    // SAFETY: dp is a valid DIR* from opendir.
    unsafe { libc::closedir(dp) };

    comp.sort_entries();
}

/// Fish-style partial path completion: each intermediate directory component
/// is treated as a prefix. e.g., "/home/user/de/s" finds entries starting
/// with "s" in /home/user/dev/, /home/user/Desktop/, etc.
/// Returns (resolved_dir_with_slash, start_idx, count) tuples indexing into the Completions.
pub fn complete_partial_path(
    partial: &str,
    dirs_only: bool,
) -> (Completions, Vec<(String, usize, usize)>) {
    let (dir, prefix) = split_path(partial);
    if dir.is_empty() {
        return (Completions::new(), Vec::new());
    }

    let dir_trimmed = dir.trim_end_matches('/');

    // If dir already exists, complete_path handles it
    if is_dir(dir_trimmed) {
        return (Completions::new(), Vec::new());
    }

    let resolved_dirs = resolve_partial_dir(dir_trimmed);
    let mut comp = Completions::new();
    let mut groups = Vec::new();

    for rdir in resolved_dirs {
        let start = comp.entries.len();
        complete_in_dir(&rdir, prefix, dirs_only, &mut comp);
        let count = comp.entries.len() - start;
        if count > 0 {
            groups.push((format!("{rdir}/"), start, count));
        }
    }
    (comp, groups)
}

/// Check if path is a directory using libc::stat — zero allocation.
fn is_dir(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut buf = [0u8; 4096];
    if bytes.len() >= buf.len() {
        return false;
    }
    buf[..bytes.len()].copy_from_slice(bytes);
    buf[bytes.len()] = 0;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::stat(buf.as_ptr() as *const libc::c_char, &mut st) };
    rc == 0 && st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

/// Recursively resolve a directory path where each component is a prefix.
/// e.g., "/home/user/de" → ["/home/user/dev", "/home/user/Desktop", ...]
fn resolve_partial_dir(dir: &str) -> Vec<String> {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return Vec::new();
    }

    // Base: if it exists as a directory, return it
    if is_dir(dir) {
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

    let comp_bytes = component.as_bytes();

    // Recursively resolve parent
    let parents = resolve_partial_dir(parent);

    let mut results = Vec::new();
    for p in &parents {
        // Open directory with libc
        let p_bytes = p.as_bytes();
        let mut dir_buf = [0u8; 4096];
        if p_bytes.len() >= dir_buf.len() {
            continue;
        }
        dir_buf[..p_bytes.len()].copy_from_slice(p_bytes);
        dir_buf[p_bytes.len()] = 0;

        let dp = unsafe { libc::opendir(dir_buf.as_ptr() as *const libc::c_char) };
        if dp.is_null() {
            continue;
        }

        // Stack buffer for "parent/name\0" for stat
        let mut path_buf = [0u8; 4096];
        let prefix_len = if p == "." {
            0
        } else {
            let len = p_bytes.len();
            path_buf[..len].copy_from_slice(p_bytes);
            if p_bytes.last() != Some(&b'/') {
                path_buf[len] = b'/';
                len + 1
            } else {
                len
            }
        };

        loop {
            let ent = unsafe { libc::readdir(dp) };
            if ent.is_null() {
                break;
            }
            let name_cstr = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
            let name_bytes = name_cstr.to_bytes();

            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            if !name_bytes.starts_with(comp_bytes) {
                continue;
            }
            if name_bytes.first() == Some(&b'.') && !comp_bytes.starts_with(b".") {
                continue;
            }

            // stat to check if it's a directory
            let total = prefix_len + name_bytes.len();
            if total >= path_buf.len() {
                continue;
            }
            path_buf[prefix_len..total].copy_from_slice(name_bytes);
            path_buf[total] = 0;

            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::stat(path_buf.as_ptr() as *const libc::c_char, &mut st) } != 0 {
                continue;
            }
            if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
                continue;
            }

            let name = match std::str::from_utf8(name_bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if p == "/" {
                results.push(format!("/{name}"));
            } else if p == "." {
                results.push(name.to_string());
            } else {
                results.push(format!("{p}/{name}"));
            }
        }

        unsafe { libc::closedir(dp) };

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
/// Uses environ directly — only allocates the arena + entries Vec.
pub fn complete_env(partial: &str) -> Completions {
    unsafe extern "C" {
        static environ: *const *const libc::c_char;
    }

    let prefix = partial.strip_prefix('$').unwrap_or(partial);
    let prefix_bytes = prefix.as_bytes();
    let mut comp = Completions::new();

    // SAFETY: Single-threaded shell. environ is a NULL-terminated array of
    // "KEY=VALUE\0" pointers, valid until the environment is next modified.
    unsafe {
        let mut ep = environ;
        if ep.is_null() {
            return comp;
        }
        while !(*ep).is_null() {
            let entry = std::ffi::CStr::from_ptr(*ep).to_bytes();
            // Find the '=' separator
            if let Some(eq_pos) = entry.iter().position(|&b| b == b'=') {
                let key = &entry[..eq_pos];
                if key.starts_with(prefix_bytes)
                    && let Ok(name) = std::str::from_utf8(key)
                {
                    comp.push(name, false, false, false);
                }
            }
            ep = ep.add(1);
        }
    }

    let names = &comp.names;
    comp.entries.sort_by(|a, b| {
        let an = &names[a.name_start as usize..][..a.name_len as usize];
        let bn = &names[b.name_start as usize..][..b.name_len as usize];
        an.cmp(bn)
    });
    comp
}

/// Like `complete_env` but appends into a caller-owned `Completions` (zero-alloc reuse).
pub fn complete_env_into(partial: &str, comp: &mut Completions) {
    unsafe extern "C" {
        static environ: *const *const libc::c_char;
    }

    let prefix = partial.strip_prefix('$').unwrap_or(partial);
    let prefix_bytes = prefix.as_bytes();

    unsafe {
        let mut ep = environ;
        if ep.is_null() {
            return;
        }
        while !(*ep).is_null() {
            let entry = std::ffi::CStr::from_ptr(*ep).to_bytes();
            if let Some(eq_pos) = entry.iter().position(|&b| b == b'=') {
                let key = &entry[..eq_pos];
                if key.starts_with(prefix_bytes)
                    && let Ok(name) = std::str::from_utf8(key)
                {
                    comp.push(name, false, false, false);
                }
            }
            ep = ep.add(1);
        }
    }

    let names = comp.names.as_bytes();
    comp.entries.sort_unstable_by(|a, b| {
        let an = &names[a.name_start as usize..][..a.name_len as usize];
        let bn = &names[b.name_start as usize..][..b.name_len as usize];
        an.cmp(bn)
    });
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
        let mut comp = Completions::new();
        for i in 0..7 {
            comp.push(&format!("file{i}.rs"), false, false, false);
        }
        let (cols, rows) = compute_grid(&comp.entries, 80);
        assert!(cols >= 1);
        assert!(rows >= 1);
        assert!(cols * rows >= 7);
    }

    #[test]
    fn partial_path_resolves_this_repo() {
        let resolved = resolve_partial_dir("sr");
        assert!(
            resolved.iter().any(|d| d == "src"),
            "expected 'src' in {resolved:?}"
        );
    }

    #[test]
    fn partial_path_two_levels() {
        let resolved = resolve_partial_dir("sr");
        assert!(!resolved.is_empty());
    }

    #[test]
    fn partial_path_complete_finds_entries() {
        let (comp, groups) = complete_partial_path("./sr/m", false);
        let names: Vec<&str> = groups
            .iter()
            .flat_map(|(_, start, count)| (*start..*start + *count).map(|i| comp.name(i)))
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with("main")),
            "expected main.rs in {names:?}"
        );
    }

    #[test]
    fn partial_path_existing_dir_returns_empty() {
        let (_comp, groups) = complete_partial_path("./src/m", false);
        assert!(groups.is_empty());
    }

    #[test]
    fn partial_path_nonexistent_returns_empty() {
        let (_comp, groups) = complete_partial_path("./zzzzz/m", false);
        assert!(groups.is_empty());
    }

    #[test]
    fn partial_path_absolute_tilde_expanded() {
        let home = std::env::var("HOME").unwrap();
        let expanded = format!("{home}/de/s");
        let (comp, groups) = complete_partial_path(&expanded, true);
        let all_names: Vec<&str> = groups
            .iter()
            .flat_map(|(_, start, count)| (*start..*start + *count).map(|i| comp.name(i)))
            .collect();
        assert!(
            all_names.contains(&"sentry"),
            "expected 'sentry' in {all_names:?}"
        );
    }
}
