use std::path::{Path, PathBuf};

/// Cached prompt state — reused across renders.
pub struct Prompt {
    user_host: String,
    home: String,
    git: GitCache,
    /// Last-seen PWD for change detection.
    pwd_last: String,
    /// Cached shortened PWD string.
    pwd_short: String,
    /// Reusable buffer for git branch — avoids allocation per render.
    branch_buf: String,
}

enum GitCache {
    Unknown,
    Repo { root: PathBuf, head: PathBuf },
    NoRepo { from: PathBuf },
}

impl Default for Prompt {
    fn default() -> Self {
        Self::new()
    }
}

impl Prompt {
    pub fn new() -> Self {
        let user = std::env::var("USER").unwrap_or_default();
        let host = hostname();
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            user_host: format!("{user}@{host} "),
            home,
            git: GitCache::Unknown,
            pwd_last: String::new(),
            pwd_short: String::new(),
            branch_buf: String::new(),
        }
    }

    /// Render the prompt into a caller-owned buffer (zero allocation steady-state).
    /// Layout: `user@host colored_pwd[*][ branch] $ `
    /// `pwd` and `denv_dirty` are passed in to avoid env var reads.
    pub fn render_into(&mut self, out: &mut String, last_status: i32, pwd: &str, denv_dirty: bool) {
        let color = if last_status == 0 {
            "\x1b[38;5;10m" // bright green
        } else {
            "\x1b[38;5;1m" // red
        };
        let reset = "\x1b[0m";

        // Update shortened PWD if directory changed
        if pwd != self.pwd_last {
            self.pwd_short.clear();
            shorten_pwd_into(pwd, &self.home, &mut self.pwd_short);
            self.pwd_last.clear();
            self.pwd_last.push_str(pwd);
        }

        out.clear();
        out.push_str(&self.user_host);
        out.push_str(color);
        out.push_str(&self.pwd_short);
        out.push_str(reset);

        // Dirty indicator
        if denv_dirty {
            out.push_str("\x1b[38;5;1m *\x1b[0m");
        }

        // Git branch
        if self.git_branch_into(Path::new(pwd)) {
            out.push(' ');
            out.push_str(&self.branch_buf);
        }

        out.push_str(" $ ");
    }

    /// Legacy render — allocates. Used by benchmarks and tests.
    pub fn render(&mut self, last_status: i32) -> String {
        let pwd = std::env::var("PWD").unwrap_or_default();
        let denv_dirty = std::env::var("__DENV_DIRTY").as_deref() == Ok("1");
        let mut out = String::with_capacity(128);
        self.render_into(&mut out, last_status, &pwd, denv_dirty);
        out
    }

    /// Compute the display length of the prompt (excluding ANSI escapes).
    pub fn display_len(&self, prompt_str: &str) -> usize {
        let mut len = 0;
        let mut in_escape = false;
        // Byte-based: prompt is ASCII + ANSI escapes. Non-ASCII chars
        // (if any) are each one display column, same as the byte count.
        for &b in prompt_str.as_bytes() {
            if in_escape {
                if b == b'm' {
                    in_escape = false;
                }
            } else if b == 0x1b {
                in_escape = true;
            } else if b & 0xC0 != 0x80 {
                // Count lead bytes (ASCII or UTF-8 start), skip continuation bytes
                len += 1;
            }
        }
        len
    }

    /// Invalidate git cache (call on `cd`).
    pub fn invalidate_git(&mut self) {
        self.git = GitCache::Unknown;
    }

    /// Write git branch into self.branch_buf. Returns true if branch found.
    fn git_branch_into(&mut self, cwd: &Path) -> bool {
        // Check cache first
        match &self.git {
            GitCache::Repo { root, head } if cwd.starts_with(root) => {
                self.branch_buf.clear();
                return read_head_into(head, &mut self.branch_buf);
            }
            GitCache::NoRepo { from } if from.starts_with(cwd) => {
                return false;
            }
            _ => {}
        }

        // Walk up from cwd, using cached NoRepo as ceiling if applicable
        let ceiling = match &self.git {
            GitCache::NoRepo { from } if cwd.starts_with(from) => Some(from.clone()),
            _ => None,
        };
        self.resolve_git(cwd, ceiling.as_ref())
    }

    fn resolve_git(&mut self, cwd: &Path, ceiling: Option<&PathBuf>) -> bool {
        match find_git_dir(cwd, ceiling) {
            Some(git_dir) => {
                let root = git_dir.parent().unwrap_or(cwd).to_path_buf();
                let head_path = git_dir.join("HEAD");
                self.branch_buf.clear();
                let found = read_head_into(&head_path, &mut self.branch_buf);
                self.git = GitCache::Repo {
                    root,
                    head: head_path,
                };
                found
            }
            None => {
                self.git = GitCache::NoRepo {
                    from: cwd.to_path_buf(),
                };
                false
            }
        }
    }
}

// -- PWD shortening --

/// Shorten PWD: tilde-contract home, abbreviate middle components to 1 char.
pub fn shorten_pwd(pwd: &str, home: &str) -> String {
    let mut out = String::with_capacity(pwd.len());
    shorten_pwd_into(pwd, home, &mut out);
    out
}

/// Shorten PWD into a caller-owned buffer (zero allocation).
fn shorten_pwd_into(pwd: &str, home: &str, out: &mut String) {
    let (tilde, remainder) = if !home.is_empty()
        && pwd.starts_with(home)
        && (pwd.len() == home.len() || pwd.as_bytes().get(home.len()) == Some(&b'/'))
    {
        (true, &pwd[home.len()..])
    } else {
        (false, pwd)
    };

    if remainder.is_empty() && tilde {
        out.push('~');
        return;
    }

    let n_parts = remainder.split('/').count();

    for (i, part) in remainder.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        if tilde && i == 0 {
            out.push('~');
        } else if i < n_parts - 1 && !part.is_empty() {
            // Abbreviate: keep leading dot + 1 char
            let skip = if part.starts_with('.') { 1 } else { 0 };
            let take_chars = skip + 1;
            // Fast path: paths are almost always ASCII
            let byte_end = if part.is_ascii() {
                take_chars.min(part.len())
            } else {
                part.char_indices()
                    .nth(take_chars)
                    .map(|(i, _)| i)
                    .unwrap_or(part.len())
            };
            out.push_str(&part[..byte_end]);
        } else {
            out.push_str(part);
        }
    }
}

// -- Git helpers --

fn find_git_dir(start: &Path, ceiling: Option<&PathBuf>) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&dot_git) {
            if meta.is_dir() {
                return Some(dot_git);
            }
            if meta.is_file() {
                return resolve_gitdir_file(&dot_git);
            }
        }
        if ceiling.is_some_and(|c| dir == *c) {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn resolve_gitdir_file(path: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let mut buf = [0u8; 512];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    let raw = std::str::from_utf8(&buf[..n]).ok()?.trim();
    let target = Path::new(raw.strip_prefix("gitdir: ")?);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        path.parent()?.join(target)
    })
}

/// Read HEAD into `out`. Returns true if branch found. Zero allocation.
fn read_head_into(head_path: &Path, out: &mut String) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 256];
    let n = match std::fs::File::open(head_path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let line = match std::str::from_utf8(&buf[..n]) {
        Ok(s) => s.trim_end(),
        Err(_) => return false,
    };
    let branch = if let Some(b) = line.strip_prefix("ref: refs/heads/") {
        b
    } else if let Some(b) = line.strip_prefix("ref: ") {
        b
    } else {
        // Detached HEAD — show short hash
        &line[..line.len().min(8)]
    };
    if branch.is_empty() {
        return false;
    }
    out.push_str(branch);
    true
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes a NUL-terminated hostname into the buffer.
    // 256 bytes is well above HOST_NAME_MAX (typically 64) on all targets.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..len])
            .split('.')
            .next()
            .unwrap_or("localhost")
            .to_string()
    } else {
        "localhost".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pwd_shortens_middle() {
        assert_eq!(
            shorten_pwd("/home/u/dev/fish-shell", "/home/u"),
            "~/d/fish-shell"
        );
    }

    #[test]
    fn pwd_home_exactly() {
        assert_eq!(shorten_pwd("/home/u", "/home/u"), "~");
    }

    #[test]
    fn pwd_preserves_dot() {
        assert_eq!(shorten_pwd("/home/u/.config/fish", "/home/u"), "~/.c/fish");
    }

    #[test]
    fn pwd_root() {
        assert_eq!(shorten_pwd("/", "/home/u"), "/");
    }

    #[test]
    fn pwd_outside_home() {
        assert_eq!(shorten_pwd("/var/log/syslog", "/home/u"), "/v/l/syslog");
    }

    #[test]
    fn pwd_no_false_tilde() {
        assert_eq!(shorten_pwd("/home/user2/foo", "/home/user"), "/h/u/foo");
    }
}
