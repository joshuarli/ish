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
        }
    }

    /// Render the prompt string with ANSI colors.
    /// Layout: `user@host colored_pwd[*][ branch] $ `
    pub fn render(&mut self, last_status: i32) -> String {
        let pwd = std::env::var("PWD").unwrap_or_default();
        let color = if last_status == 0 {
            "\x1b[38;5;10m" // bright green
        } else {
            "\x1b[38;5;1m" // red
        };
        let reset = "\x1b[0m";

        // Update shortened PWD if directory changed
        if pwd != self.pwd_last {
            self.pwd_short = shorten_pwd(&pwd, &self.home);
            self.pwd_last = pwd.clone();
        }

        let mut out = String::with_capacity(128);
        out.push_str(&self.user_host);
        out.push_str(color);
        out.push_str(&self.pwd_short);
        out.push_str(reset);

        // Dirty indicator
        if std::env::var("__DENV_DIRTY").as_deref() == Ok("1") {
            out.push_str("\x1b[38;5;1m *\x1b[0m");
        }

        // Git branch
        if let Some(branch) = self.git_branch(Path::new(&pwd)) {
            out.push(' ');
            out.push_str(&branch);
        }

        out.push_str(" $ ");
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

    fn git_branch(&mut self, cwd: &Path) -> Option<String> {
        // Check cache first
        match &self.git {
            GitCache::Repo { root, head } if cwd.starts_with(root) => {
                return read_head(head);
            }
            GitCache::NoRepo { from } if from.starts_with(cwd) => {
                return None;
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

    fn resolve_git(&mut self, cwd: &Path, ceiling: Option<&PathBuf>) -> Option<String> {
        match find_git_dir(cwd, ceiling) {
            Some(git_dir) => {
                let root = git_dir.parent().unwrap_or(cwd).to_path_buf();
                let head_path = git_dir.join("HEAD");
                let branch = read_head(&head_path);
                self.git = GitCache::Repo {
                    root,
                    head: head_path,
                };
                branch
            }
            None => {
                self.git = GitCache::NoRepo {
                    from: cwd.to_path_buf(),
                };
                None
            }
        }
    }
}

// -- PWD shortening --

/// Shorten PWD: tilde-contract home, abbreviate middle components to 1 char.
pub fn shorten_pwd(pwd: &str, home: &str) -> String {
    let (tilde, remainder) = if !home.is_empty()
        && pwd.starts_with(home)
        && (pwd.len() == home.len() || pwd.as_bytes().get(home.len()) == Some(&b'/'))
    {
        (true, &pwd[home.len()..])
    } else {
        (false, pwd)
    };

    if remainder.is_empty() && tilde {
        return "~".to_string();
    }

    let n_parts = remainder.split('/').count();

    let mut out = String::with_capacity(remainder.len());
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

    out
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

fn read_head(head_path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 256];
    let n = std::fs::File::open(head_path)
        .and_then(|mut f| f.read(&mut buf))
        .ok()?;
    let line = std::str::from_utf8(&buf[..n]).ok()?.trim_end();
    let branch = if let Some(b) = line.strip_prefix("ref: refs/heads/") {
        b
    } else if let Some(b) = line.strip_prefix("ref: ") {
        b
    } else {
        // Detached HEAD — show short hash
        &line[..line.len().min(8)]
    };
    if branch.is_empty() {
        return None;
    }
    Some(branch.to_string())
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
