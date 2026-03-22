/// Completion entry — mtime + u32 offset + u8 len + u8 display_width + u8 flags.
/// name_len is u8: NAME_MAX is 255 on Linux/macOS.
pub struct CompEntry {
    mtime: i64, // st_mtime from stat(), 0 for non-path entries (hosts, builtins)
    name_start: u32,
    name_len: u8,
    name_display_width: u8,
    flags: u8, // bit 0: is_dir, bit 1: is_link, bit 2: is_exec, bit 3: is_host
}

impl CompEntry {
    pub fn display_width(&self) -> usize {
        self.name_display_width as usize
            + if self.is_dir() || self.is_host() {
                1
            } else {
                0
            }
    }

    pub fn is_dir(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn is_link(&self) -> bool {
        self.flags & 2 != 0
    }

    pub fn is_exec(&self) -> bool {
        self.flags & 4 != 0
    }

    pub fn is_host(&self) -> bool {
        self.flags & 8 != 0
    }
}

fn pack_flags(is_dir: bool, is_link: bool, is_exec: bool) -> u8 {
    (is_dir as u8) | ((is_link as u8) << 1) | ((is_exec as u8) << 2)
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
        self.push_with_mtime(name, is_dir, is_link, is_exec, 0);
    }

    pub fn push_with_mtime(
        &mut self,
        name: &str,
        is_dir: bool,
        is_link: bool,
        is_exec: bool,
        mtime: i64,
    ) {
        let start = self.names.len() as u32;
        self.names.push_str(name);
        self.entries.push(CompEntry {
            mtime,
            name_start: start,
            name_len: name.len().min(255) as u8,
            name_display_width: crate::line::str_width(name).min(255) as u8,
            flags: pack_flags(is_dir, is_link, is_exec),
        });
    }

    /// Begin a name by recording the current arena position.
    /// Call `finish_entry` after pushing name parts to `names`.
    pub fn begin_entry(&self) -> u32 {
        self.names.len() as u32
    }

    /// Finish an entry whose name starts at `start` in the arena.
    pub fn finish_entry(&mut self, start: u32, is_dir: bool, is_link: bool, is_exec: bool) {
        self.finish_entry_with_mtime(start, is_dir, is_link, is_exec, 0);
    }

    pub fn finish_entry_with_mtime(
        &mut self,
        start: u32,
        is_dir: bool,
        is_link: bool,
        is_exec: bool,
        mtime: i64,
    ) {
        let name = &self.names[start as usize..];
        let name_len = name.len().min(255) as u8;
        let name_display_width = crate::line::str_width(name).min(255) as u8;
        self.entries.push(CompEntry {
            mtime,
            name_start: start,
            name_len,
            name_display_width,
            flags: pack_flags(is_dir, is_link, is_exec),
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

    /// Sort entries by modification time (most recent first), alphabetical tiebreaker.
    pub fn sort_by_mtime(&mut self) {
        let names = self.names.as_bytes();
        self.entries.sort_unstable_by(|a, b| {
            b.mtime
                .cmp(&a.mtime)
                .then_with(|| cmp_icase_arena(names, a, b))
        });
    }

    /// Remove duplicate adjacent entries (by exact name). Call after `sort_entries`.
    pub fn dedup_sorted(&mut self) {
        let mut i = 1;
        while i < self.entries.len() {
            let prev = &self.entries[i - 1];
            let curr = &self.entries[i];
            if self.names[prev.name_start as usize..][..prev.name_len as usize]
                == self.names[curr.name_start as usize..][..curr.name_len as usize]
            {
                self.entries.remove(i);
            } else {
                i += 1;
            }
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
    /// Whether the user was inside a single-quote when completion started.
    pub in_quote: bool,
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
        if e.is_dir() {
            tw.write_str("/");
        } else if e.is_host() {
            tw.write_str(":");
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

/// Complete entries in `dir` matching `prefix`.
/// Single readdir pass: collects prefix and substring matches together,
/// preferring prefix matches when any exist. Substring fallback is
/// case-insensitive (like fish) so "tom" matches "Cargo.toml".
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
    let before = comp.entries.len();
    let mut prefix_count = 0usize;

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
        // Classify: prefix match vs substring match
        let is_prefix = name_bytes.starts_with(prefix_bytes);
        if !is_prefix && (prefix_bytes.is_empty() || !contains_icase(name_bytes, prefix_bytes)) {
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

        comp.push_with_mtime(name, is_dir, is_link, is_exec, st.st_mtime);
        if is_prefix {
            prefix_count += 1;
        }
    }

    // SAFETY: dp is a valid DIR* from opendir.
    unsafe { libc::closedir(dp) };

    // If we have prefix matches, discard substring-only matches
    let added = comp.entries.len() - before;
    if prefix_count > 0 && prefix_count < added {
        let mut i = before;
        while i < comp.entries.len() {
            let e = &comp.entries[i];
            let name = &comp.names.as_bytes()[e.name_start as usize..][..e.name_len as usize];
            if name.starts_with(prefix_bytes) {
                i += 1;
            } else {
                comp.entries.remove(i);
            }
        }
    }

    comp.sort_by_mtime();
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

/// Case-insensitive substring search: does `haystack` contain `needle`?
fn contains_icase(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    let first = needle[0].to_ascii_lowercase();
    for i in 0..=(haystack.len() - needle.len()) {
        if haystack[i].to_ascii_lowercase() == first
            && haystack[i..i + needle.len()]
                .iter()
                .zip(needle)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return true;
        }
    }
    false
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

// -- SSH Completion --

/// Parse hostnames from ~/.ssh/config and ~/.ssh/known_hosts.
fn parse_ssh_hosts(home: &str) -> Vec<String> {
    let mut hosts = Vec::new();

    // ~/.ssh/config: extract Host directives (skip wildcards)
    if let Ok(data) = std::fs::read_to_string(format!("{home}/.ssh/config")) {
        for line in data.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed
                .strip_prefix("Host ")
                .or_else(|| trimmed.strip_prefix("Host\t"))
            {
                for host in rest.split_whitespace() {
                    if !host.contains('*') && !host.contains('?') && host != "." {
                        hosts.push(host.to_string());
                    }
                }
            }
        }
    }

    // ~/.ssh/known_hosts: first field is hostname (skip hashed entries)
    if let Ok(data) = std::fs::read_to_string(format!("{home}/.ssh/known_hosts")) {
        for line in data.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('|') {
                continue;
            }
            if let Some(host_field) = trimmed.split_whitespace().next() {
                // May contain comma-separated aliases and [host]:port
                for entry in host_field.split(',') {
                    let host = entry
                        .strip_prefix('[')
                        .and_then(|s| s.split(']').next())
                        .unwrap_or(entry);
                    if !host.is_empty() && !host.contains('*') {
                        hosts.push(host.to_string());
                    }
                }
            }
        }
    }

    hosts.sort();
    hosts.dedup();
    hosts
}

/// Complete SSH hostnames matching `prefix`.
pub fn complete_hostnames(prefix: &str, home: &str, comp: &mut Completions) {
    for host in parse_ssh_hosts(home) {
        if host.starts_with(prefix) {
            let start = comp.names.len() as u32;
            comp.names.push_str(&host);
            comp.entries.push(CompEntry {
                mtime: 0,
                name_start: start,
                name_len: host.len().min(255) as u8,
                name_display_width: host.len().min(255) as u8, // hostnames are ASCII
                flags: 8,                                      // is_host
            });
        }
    }
}

/// Complete remote paths via SSH. Runs `ssh -o BatchMode=yes -o ConnectTimeout=2`
/// to list files on the remote host. Nearly instant when ControlMaster is active.
/// Returns after at most ~2 seconds.
pub fn complete_remote_path(host: &str, path_prefix: &str, comp: &mut Completions) {
    // Build the remote ls command — shell-quote the glob pattern
    let cmd = format!(
        "ssh -o BatchMode=yes -o ConnectTimeout=2 {} 'ls -dp {}* 2>/dev/null'",
        host,
        shell_escape(path_prefix),
    );

    let (pid, pipe_r) = match crate::sys::spawn_command_subst(&cmd) {
        Ok(v) => v,
        Err(_) => return,
    };

    // Read output with a 3-second deadline
    let deadline_ns = monotonic_ns() + 3_000_000_000;
    let mut output = String::new();
    let mut buf = [0u8; 4096];

    // Set pipe to non-blocking so we can enforce the deadline
    // SAFETY: pipe_r is a valid fd from spawn_command_subst.
    unsafe {
        let flags = libc::fcntl(pipe_r, libc::F_GETFL);
        libc::fcntl(pipe_r, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    loop {
        // SAFETY: reading from a valid pipe fd into a stack buffer.
        let n = unsafe { libc::read(pipe_r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
                output.push_str(s);
            }
        } else if n == 0 {
            break; // EOF
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN)
                || err.raw_os_error() == Some(libc::EWOULDBLOCK)
            {
                if monotonic_ns() >= deadline_ns {
                    // SAFETY: kill the timed-out child.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    break;
                }
                // Brief poll to avoid busy-spinning
                let mut pfd = libc::pollfd {
                    fd: pipe_r,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll on a single valid fd with 100ms timeout.
                unsafe { libc::poll(&mut pfd, 1, 100) };
                continue;
            }
            if err.kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
    }

    // SAFETY: close pipe and reap child.
    unsafe {
        libc::close(pipe_r);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }

    // Determine the directory prefix to strip (everything up to and including the last /)
    let dir_prefix = match path_prefix.rfind('/') {
        Some(i) => &path_prefix[..=i],
        None => "",
    };

    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let is_dir = line.ends_with('/');
        let path = line.trim_end_matches('/');
        // Strip the directory prefix to get just the filename
        let name = if !dir_prefix.is_empty() {
            path.strip_prefix(dir_prefix).unwrap_or(path)
        } else {
            // ls -dp without a leading / returns relative names
            path.rsplit('/').next().unwrap_or(path)
        };
        if !name.is_empty() {
            comp.push(name, is_dir, false, false);
        }
    }
}

/// Minimal shell escaping for use inside single quotes in an ssh command.
fn shell_escape(s: &str) -> String {
    // Inside single quotes, only ' needs escaping (as '\'' — end quote, escaped quote, reopen)
    if !s.contains('\'') {
        return s.to_string();
    }
    s.replace('\'', "'\\''")
}

/// Monotonic clock in nanoseconds.
fn monotonic_ns() -> u64 {
    // SAFETY: clock_gettime writes into a stack-allocated timespec.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    #[cfg(target_os = "linux")]
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts)
    };
    #[cfg(target_os = "macos")]
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts)
    };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_flag() {
        let mut comp = Completions::new();
        // Push a host entry manually
        let start = comp.names.len() as u32;
        comp.names.push_str("myhost");
        comp.entries.push(CompEntry {
            mtime: 0,
            name_start: start,
            name_len: 6,
            name_display_width: 6,
            flags: 8,
        });
        assert!(comp.entries[0].is_host());
        assert!(!comp.entries[0].is_dir());
        assert_eq!(comp.entries[0].display_width(), 7); // "myhost" + ":"
    }

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape("hello"), "hello");
        assert_eq!(shell_escape("/tmp/foo"), "/tmp/foo");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "it'\\''s");
    }

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
    fn contains_icase_basic() {
        assert!(contains_icase(b"Cargo.toml", b"tom"));
        assert!(contains_icase(b"Cargo.toml", b"TOM"));
        assert!(contains_icase(b"Cargo.toml", b"cargo"));
        assert!(contains_icase(b"Cargo.toml", b"Cargo"));
        assert!(!contains_icase(b"Cargo.toml", b"xyz"));
        assert!(contains_icase(b"anything", b""));
        assert!(!contains_icase(b"ab", b"abc"));
    }

    #[test]
    fn substring_fallback_finds_toml() {
        // This repo has Cargo.toml — "tom" should match via substring fallback
        let comp = complete_path("tom", false);
        let names: Vec<&str> = (0..comp.len()).map(|i| comp.name(i)).collect();
        assert!(
            names.iter().any(|n| n.contains("toml")),
            "expected Cargo.toml in {names:?}"
        );
    }

    #[test]
    fn prefix_match_preferred_over_substring() {
        // "src" prefix-matches "src" directly — should not fall back to substring
        let comp = complete_path("src", false);
        let names: Vec<&str> = (0..comp.len()).map(|i| comp.name(i)).collect();
        assert!(names.contains(&"src"), "expected exact 'src' in {names:?}");
    }

    #[test]
    fn partial_path_absolute() {
        // Use /usr as a stable path that exists on all platforms.
        // /usr/bi → should resolve to /usr/bin, then find entries starting with "t"
        let (comp, groups) = complete_partial_path("/usr/bi/t", false);
        let all_names: Vec<&str> = groups
            .iter()
            .flat_map(|(_, start, count)| (*start..*start + *count).map(|i| comp.name(i)))
            .collect();
        assert!(
            all_names.iter().any(|n| n.starts_with("t")),
            "expected entries starting with 't' in /usr/bin: {all_names:?}"
        );
    }
}
