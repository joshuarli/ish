use std::path::{Path, PathBuf};

/// Cached prompt state — reused across renders.
pub struct Prompt {
    user: String,
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
    NoRepo {
        from: PathBuf,
        checked_markers: Vec<PathBuf>,
    },
}

impl Default for Prompt {
    fn default() -> Self {
        Self::new()
    }
}

impl Prompt {
    pub fn new() -> Self {
        Self::with_identity("", "")
    }

    /// Create prompt state from values owned by the shell's variable store.
    pub fn with_identity(user: &str, home: &str) -> Self {
        Self {
            user: user.to_string(),
            user_host: prompt_identity(user, &hostname()),
            home: home.to_string(),
            git: GitCache::Unknown,
            pwd_last: String::new(),
            pwd_short: String::new(),
            branch_buf: String::new(),
        }
    }

    /// Update identity values after the shell's variable store changes.
    pub fn set_identity(&mut self, user: &str, home: &str) {
        if self.user != user {
            self.user.clear();
            self.user.push_str(user);
            self.user_host = prompt_identity(user, &hostname());
        }
        if self.home != home {
            self.home.clear();
            self.home.push_str(home);
            self.pwd_last.clear();
            self.pwd_short.clear();
        }
    }

    /// Layout: `user@host colored_pwd[*][ branch][ [Xs]] $ `
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
            if self.pwd_short.chars().any(char::is_control) {
                self.pwd_short = self.pwd_short.chars().map(crate::line::display_char).collect();
            }
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
        let pwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.render_with_state(last_status, &pwd, false)
    }

    /// Legacy render helper with explicit shell-owned prompt state.
    pub fn render_with_state(&mut self, last_status: i32, pwd: &str, denv_dirty: bool) -> String {
        let mut out = String::with_capacity(128);
        self.render_into(&mut out, last_status, pwd, denv_dirty);
        out
    }

    /// Compute the display length of the prompt (excluding ANSI escapes).
    pub fn display_len(&self, prompt_str: &str) -> usize {
        let mut len = 0;
        let mut in_escape = false;
        for c in prompt_str.chars() {
            if in_escape {
                if c == 'm' {
                    in_escape = false;
                }
            } else if c == '\x1b' {
                in_escape = true;
            } else {
                len += crate::line::char_width(c);
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
        match &self.git {
            GitCache::Repo { root, head } if cwd.starts_with(root) => {
                self.branch_buf.clear();
                return read_head_into(head, &mut self.branch_buf);
            }
            GitCache::NoRepo {
                from,
                checked_markers,
            } if (from.starts_with(cwd) || cwd.starts_with(from))
                && checked_markers
                    .iter()
                    .all(|marker| std::fs::symlink_metadata(marker).is_err()) =>
            {
                if from.starts_with(cwd) {
                    return false;
                }
                if let Some(location) = find_git_dir(cwd, Some(from), None) {
                    return self.cache_git_dir(location);
                }
                return false;
            }
            _ => {}
        }

        self.resolve_git(cwd)
    }

    fn resolve_git(&mut self, cwd: &Path) -> bool {
        let mut checked_markers = Vec::new();
        match find_git_dir(cwd, None, Some(&mut checked_markers)) {
            Some(location) => self.cache_git_dir(location),
            None => {
                self.git = GitCache::NoRepo {
                    from: cwd.to_path_buf(),
                    checked_markers,
                };
                false
            }
        }
    }

    fn cache_git_dir(&mut self, location: GitLocation) -> bool {
        let head_path = location.git_dir.join("HEAD");
        self.branch_buf.clear();
        let found = read_head_into(&head_path, &mut self.branch_buf);
        self.git = GitCache::Repo {
            root: location.root,
            head: head_path,
        };
        found
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

struct GitLocation {
    root: PathBuf,
    git_dir: PathBuf,
}

fn find_git_dir(
    start: &Path,
    ceiling: Option<&Path>,
    mut checked_markers: Option<&mut Vec<PathBuf>>,
) -> Option<GitLocation> {
    let mut dir = start.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&dot_git) {
            if meta.is_dir() {
                return Some(GitLocation {
                    root: dir,
                    git_dir: dot_git,
                });
            }
            if meta.is_file() {
                let git_dir = resolve_gitdir_file(&dot_git);
                if git_dir.is_none()
                    && let Some(markers) = checked_markers.as_deref_mut()
                {
                    markers.push(dot_git);
                }
                return git_dir.map(|git_dir| GitLocation { root: dir, git_dir });
            }
        }
        if let Some(markers) = checked_markers.as_deref_mut() {
            markers.push(dot_git);
        }
        if ceiling.is_some_and(|c| dir == c) {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn read_regular_file_prefix(path: &Path, buf: &mut [u8]) -> Option<usize> {
    use rustix::fs::{FileType, Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let stat = rustix::fs::fstat(&fd).ok()?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return None;
    }
    rustix::io::read(&fd, buf).ok()
}

fn resolve_gitdir_file(path: &Path) -> Option<PathBuf> {
    let mut buf = [0u8; 512];
    let n = read_regular_file_prefix(path, &mut buf)?;
    let raw = std::str::from_utf8(&buf[..n]).ok()?.trim();
    let target = Path::new(raw.strip_prefix("gitdir: ")?);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        path.parent()?.join(target)
    })
}

fn read_head_into(head_path: &Path, out: &mut String) -> bool {
    let mut buf = [0u8; 256];
    let Some(n) = read_regular_file_prefix(head_path, &mut buf) else {
        return false;
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
        let Some(abbreviation) = detached_head_abbreviation(line) else {
            return false;
        };
        abbreviation
    };
    if branch.is_empty() {
        return false;
    }
    out.extend(branch.chars().map(crate::line::display_char));
    true
}

fn detached_head_abbreviation(object_id: &str) -> Option<&str> {
    let bytes = object_id.as_bytes();
    if !matches!(bytes.len(), 40 | 64) || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    Some(&object_id[..8])
}

/// Append `bytes` percent-encoded byte-wise: only URI unreserved bytes (and
/// `/` when `keep_slash`) pass through.
pub fn push_percent_encoded(out: &mut String, bytes: &[u8], keep_slash: bool) {
    for &b in bytes {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

fn prompt_identity(user: &str, host: &str) -> String {
    user.chars()
        .chain(['@'])
        .chain(host.chars())
        .chain([' '])
        .map(crate::line::display_char)
        .collect()
}

fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .split('.')
        .next()
        .unwrap_or("localhost")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_replaces_host_controls() {
        assert_eq!(
            prompt_identity("Zoë", "h\n\r\t\x1b]52;c;x\x07\x7f\u{0085}"),
            "Zoë@h����]52;c;x��� "
        );
    }

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
    fn prompt_neutralizes_control_characters() {
        let mut prompt = Prompt::with_identity("u", "/home/u");
        let pwd = "/tmp/a\x1b]52;c;aGk=\x07b";
        let out = prompt.render_with_state(0, pwd, false);
        assert!(!out.contains('\x07'));
        assert!(!out.contains("\x1b]"));
    }

    #[test]
    fn head_with_escape_is_sanitized() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work/tmp")
            .join(format!("head-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let head = dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\x1b]52;c;aGk=\x07\x1b[2J").unwrap();
        let mut out = String::new();
        assert!(read_head_into(&head, &mut out));
        assert!(!out.chars().any(char::is_control));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_detached_head_is_unavailable() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work/tmp")
            .join(format!("head-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let head = dir.join("HEAD");
        for (value, expected) in [
            ("ééé€x", None),
            ("0123456789abcdef0123456789abcdef01234567", Some("01234567")),
            (
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                Some("01234567"),
            ),
        ] {
            std::fs::write(&head, value).unwrap();
            let mut out = String::new();
            assert_eq!(read_head_into(&head, &mut out), expected.is_some());
            assert_eq!(out.as_str(), expected.unwrap_or(""));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work/tmp")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fifo_head_returns_without_branch() {
        let dir = scratch_dir("head-fifo");
        let head = dir.join("HEAD");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &head,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = head.clone();
        std::thread::spawn(move || {
            let mut out = String::new();
            let found = read_head_into(&reader, &mut out);
            let _ = tx.send((found, out));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(5));
        if result.is_err() {
            drop(std::fs::OpenOptions::new().write(true).open(&head));
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result.expect("read_head_into blocked on a FIFO"), (false, String::new()));
    }

    #[test]
    fn symlinked_head_is_unavailable() {
        let dir = scratch_dir("head-symlink");
        std::fs::write(dir.join("real"), "ref: refs/heads/main\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("HEAD")).unwrap();
        let mut out = String::new();
        assert!(!read_head_into(&dir.join("HEAD"), &mut out));
        assert!(out.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_gitdir_file_hits_cache_below_repo_root() {
        let dir = scratch_dir("worktree-cache");
        let store = dir.join("main/.git/worktrees/wt");
        let repo = dir.join("wt");
        let cwd = repo.join("src/deep");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(store.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        let dot_git = repo.join(".git");
        std::fs::write(&dot_git, format!("gitdir: {}\n", store.display())).unwrap();

        let mut prompt = Prompt::with_identity("u", "/home/u");
        assert!(prompt.git_branch_into(&cwd));
        assert_eq!(prompt.branch_buf, "feature");

        std::fs::remove_file(&dot_git).unwrap();
        assert!(prompt.git_branch_into(&cwd));
        assert_eq!(prompt.branch_buf, "feature");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn osc7_path_is_percent_encoded_bytewise() {
        let mut out = String::new();
        push_percent_encoded(&mut out, "/a b\x07\u{e9}".as_bytes(), true);
        assert_eq!(out, "/a%20b%07%C3%A9");
    }

    #[test]
    fn pwd_no_false_tilde() {
        assert_eq!(shorten_pwd("/home/user2/foo", "/home/user"), "/h/u/foo");
    }
}
