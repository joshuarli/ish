//! Native .envrc/.env loading for ish.
//!
//! ish owns discovery, trust, active-state restore, and prompt flags directly.
//! `.env` is parsed in Rust. Trusted `.envrc` files are evaluated via `bash`
//! and diffed against the current environment.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const BASH_STDLIB: &str = include_str!("denv_stdlib.sh");

/// A single environment variable change applied by denv.
pub enum EnvChange {
    Set(String, String),
    Unset(String),
}

pub struct CommandOutcome {
    pub status: i32,
    pub changes: Vec<EnvChange>,
}

struct EnvFiles {
    dir: PathBuf,
    envrc: Option<(PathBuf, u64)>,
    dotenv: Option<(PathBuf, u64)>,
}

struct ActiveState {
    dir: PathBuf,
    envrc_mtime: u64,
    dotenv_mtime: u64,
    prev: Vec<PrevVar>,
}

enum PrevVar {
    Restore(String, String),
    Unset(String),
}

struct EnvDiff {
    set: Vec<(String, String)>,
    unset: Vec<String>,
}

/// Initialize ish's native denv state vars.
pub fn init() {
    let pid = std::process::id().to_string();
    crate::shell_setenv("__DENV_PID", &pid);
    crate::shell_setenv("__DENV_SHELL", "bash");
}

/// Called once at shell startup.
pub fn on_startup() -> Vec<EnvChange> {
    refresh(false).unwrap_or_else(report_error)
}

/// Called after any directory change.
pub fn on_cd() -> Vec<EnvChange> {
    refresh(false).unwrap_or_else(report_error)
}

/// Handle `denv allow|deny|reload`.
pub fn command(args: &[String]) -> CommandOutcome {
    match args.first().map(|s| s.as_str()) {
        Some("allow") => match allow_current_dir() {
            Ok(changes) => CommandOutcome { status: 0, changes },
            Err(err) => {
                eprintln!("denv: {err}");
                CommandOutcome {
                    status: 1,
                    changes: Vec::new(),
                }
            }
        },
        Some("deny") => match deny_current_dir() {
            Ok(changes) => CommandOutcome { status: 0, changes },
            Err(err) => {
                eprintln!("denv: {err}");
                CommandOutcome {
                    status: 1,
                    changes: Vec::new(),
                }
            }
        },
        Some("reload") => match refresh(true) {
            Ok(changes) => CommandOutcome { status: 0, changes },
            Err(err) => {
                eprintln!("denv: {err}");
                CommandOutcome {
                    status: 1,
                    changes: Vec::new(),
                }
            }
        },
        _ => {
            eprintln!("usage: denv <allow|deny|reload>");
            CommandOutcome {
                status: 1,
                changes: Vec::new(),
            }
        }
    }
}

fn report_error(err: String) -> Vec<EnvChange> {
    eprintln!("denv: {err}");
    Vec::new()
}

fn current_dir_from_env() -> PathBuf {
    env::var_os("PWD")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_default())
}

fn refresh(force: bool) -> Result<Vec<EnvChange>, String> {
    let cwd = current_dir_from_env();
    let pid = std::process::id().to_string();

    // Fast path: skip the parent-directory walk entirely. Go straight to the
    // cached dir and stat only files that previously existed.
    if !force && state_var_fast_path_ok(&cwd) {
        return Ok(Vec::new());
    }

    let found = find_env_files(&cwd);

    if !force && let Some(ref found) = found {
        if state_var_matches(found) {
            return Ok(Vec::new());
        }
        if let Some(active) = load_active(&pid)
            && active.envrc_mtime == found.envrc.as_ref().map(|(_, m)| *m).unwrap_or(0)
            && active.dotenv_mtime == found.dotenv.as_ref().map(|(_, m)| *m).unwrap_or(0)
            && paths_match(&active.dir, &found.dir)
        {
            return Ok(Vec::new());
        }
    }

    let active = load_active(&pid);
    let mut changes = Vec::new();

    if let Some(ref state) = active {
        apply_restore(&state.prev, &mut changes);
        clear_active(&pid);
    }

    let Some(found) = found else {
        clear_runtime_state(&mut changes);
        if let Some(ref state) = active {
            print_restore_summary(&state.prev);
        }
        return Ok(changes);
    };

    let envrc_mtime = found.envrc.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let dotenv_mtime = found.dotenv.as_ref().map(|(_, m)| *m).unwrap_or(0);

    if let Some((envrc_path, _)) = found.envrc.as_ref()
        && !is_allowed(envrc_path)
    {
        let dir = canonicalize_fallback(&found.dir);
        let envrc = canonicalize_fallback(envrc_path);
        eprintln!(
            "denv: {} is blocked. Run `denv allow` to trust it.",
            envrc.display()
        );
        set_change(
            &mut changes,
            "__DENV_DIR",
            dir.to_string_lossy().into_owned(),
        );
        set_change(&mut changes, "__DENV_DIRTY", "1".to_string());
        unset_change(&mut changes, "__DENV_STATE");
        if let Some(ref state) = active {
            print_restore_summary(&state.prev);
        }
        return Ok(changes);
    }

    let dotenv_entries = load_dotenv_entries(&found)?;
    if found.envrc.is_some() {
        eprintln!("denv: loading .envrc");
    }
    if found.dotenv.is_some() {
        eprintln!("denv: loading .env");
    }

    let diff = if found.envrc.is_none() {
        diff_dotenv_only(&dotenv_entries)
    } else {
        let dir = canonicalize_fallback(&found.dir);
        let envrc = found
            .envrc
            .as_ref()
            .map(|(path, _)| canonicalize_fallback(path));
        match eval_env(&dir, envrc.as_deref(), &dotenv_entries, &pid) {
            Ok(diff) => diff,
            Err(err) => {
                let dir = canonicalize_fallback(&found.dir);
                eprintln!("denv: {err}");
                set_change(
                    &mut changes,
                    "__DENV_DIR",
                    dir.to_string_lossy().into_owned(),
                );
                set_change(&mut changes, "__DENV_DIRTY", "1".to_string());
                unset_change(&mut changes, "__DENV_STATE");
                return Ok(changes);
            }
        }
    };

    let prev = capture_prev(&diff);
    apply_diff(&diff, &mut changes);
    let dir = canonicalize_fallback(&found.dir);
    set_change(
        &mut changes,
        "__DENV_DIR",
        dir.to_string_lossy().into_owned(),
    );
    unset_change(&mut changes, "__DENV_DIRTY");
    set_change(
        &mut changes,
        "__DENV_STATE",
        format!("{} {} {}", envrc_mtime, dotenv_mtime, found.dir.display()),
    );
    print_summary(
        diff.set
            .iter()
            .map(|(key, _)| ('+', key.as_str()))
            .chain(diff.unset.iter().map(|key| ('-', key.as_str()))),
    );
    save_active(
        &pid,
        &ActiveState {
            dir,
            envrc_mtime,
            dotenv_mtime,
            prev,
        },
    )?;
    Ok(changes)
}

fn allow_current_dir() -> Result<Vec<EnvChange>, String> {
    let cwd = current_dir_from_env();
    let found = find_env_files(&cwd).ok_or("no .envrc or .env found")?;
    let Some((envrc, _)) = found.envrc else {
        return Err("no .envrc found".to_string());
    };
    let envrc = envrc.canonicalize().unwrap_or(envrc);
    allow_envrc(&envrc)?;
    refresh(true)
}

fn deny_current_dir() -> Result<Vec<EnvChange>, String> {
    let cwd = current_dir_from_env();
    let found = find_env_files(&cwd).ok_or("no .envrc or .env found")?;
    let Some((envrc, _)) = found.envrc else {
        return Err("no .envrc found".to_string());
    };
    let envrc = envrc.canonicalize().unwrap_or(envrc);
    deny_envrc(&envrc)?;
    refresh(true)
}

fn find_env_files(start: &Path) -> Option<EnvFiles> {
    let mut dir = start.to_path_buf();
    loop {
        let envrc = stat_file(&dir, ".envrc");
        let dotenv = stat_file(&dir, ".env");
        if envrc.is_some() || dotenv.is_some() {
            return Some(EnvFiles { dir, envrc, dotenv });
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn stat_file(dir: &Path, name: &str) -> Option<(PathBuf, u64)> {
    let path = dir.join(name);
    let meta = fs::metadata(&path).ok().filter(|m| m.is_file())?;
    Some((path, meta.mtime() as u64))
}

fn paths_match(a: &Path, b: &Path) -> bool {
    a == b
        || a.canonicalize()
            .ok()
            .zip(b.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn canonicalize_fallback(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn parse_denv_state(s: &str) -> Option<(u64, u64, &str)> {
    let (envrc_str, rest) = s.split_once(' ')?;
    let (dotenv_str, dir) = rest.split_once(' ')?;
    Some((envrc_str.parse().ok()?, dotenv_str.parse().ok()?, dir))
}

fn state_var_fast_path_ok(cwd: &Path) -> bool {
    let Ok(state) = env::var("__DENV_STATE") else {
        return false;
    };
    let Some((envrc_mtime, dotenv_mtime, dir)) = parse_denv_state(&state) else {
        return false;
    };
    let cached = Path::new(dir);
    if !cwd.starts_with(cached) {
        return false;
    }

    let envrc_ok = envrc_mtime == 0
        || cached
            .join(".envrc")
            .metadata()
            .ok()
            .is_some_and(|meta| meta.is_file() && meta.mtime() as u64 == envrc_mtime);
    if !envrc_ok {
        return false;
    }

    dotenv_mtime == 0
        || cached
            .join(".env")
            .metadata()
            .ok()
            .is_some_and(|meta| meta.is_file() && meta.mtime() as u64 == dotenv_mtime)
}

fn state_var_matches(found: &EnvFiles) -> bool {
    let envrc_mtime = found.envrc.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let dotenv_mtime = found.dotenv.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let Ok(state) = env::var("__DENV_STATE") else {
        return false;
    };
    let Some((st_envrc, st_dotenv, st_dir)) = parse_denv_state(&state) else {
        return false;
    };
    st_envrc == envrc_mtime
        && st_dotenv == dotenv_mtime
        && (st_dir == found.dir.to_string_lossy().as_ref()
            || found
                .dir
                .canonicalize()
                .is_ok_and(|dir| st_dir == dir.to_string_lossy().as_ref()))
}

fn trust_key(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut key = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    key
}

fn data_dir() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("DENV_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("denv"));
    }
    let home = env::var_os("HOME").ok_or("HOME, XDG_DATA_HOME, and DENV_DATA_DIR are all unset")?;
    Ok(PathBuf::from(home).join(".local/share/denv"))
}

fn allow_dir() -> Result<PathBuf, String> {
    Ok(data_dir()?.join("allow"))
}

fn active_path(pid: &str) -> Result<PathBuf, String> {
    Ok(data_dir()?.join(format!("active_{pid}")))
}

fn is_allowed(envrc: &Path) -> bool {
    let allow_dir = match allow_dir() {
        Ok(dir) => dir,
        Err(_) => return false,
    };
    let stored = match fs::read_to_string(allow_dir.join(trust_key(envrc))) {
        Ok(stored) => stored,
        Err(_) => {
            let canonical = canonicalize_fallback(envrc);
            if canonical == envrc {
                return false;
            }
            match fs::read_to_string(allow_dir.join(trust_key(&canonical))) {
                Ok(stored) => stored,
                Err(_) => return false,
            }
        }
    };
    let current = match canonicalize_fallback(envrc).metadata() {
        Ok(meta) => meta.mtime() as u64,
        Err(_) => return false,
    };
    stored.trim().parse::<u64>() == Ok(current)
}

fn allow_envrc(envrc: &Path) -> Result<(), String> {
    let dir = allow_dir()?;
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create allow dir: {e}"))?;
    let mtime = envrc
        .metadata()
        .map(|meta| meta.mtime() as u64)
        .map_err(|e| format!("failed to read .envrc mtime: {e}"))?;
    fs::write(dir.join(trust_key(envrc)), mtime.to_string())
        .map_err(|e| format!("failed to write trust file: {e}"))?;
    eprintln!("denv: allowed {}", envrc.display());
    Ok(())
}

fn deny_envrc(envrc: &Path) -> Result<(), String> {
    let trust_file = allow_dir()?.join(trust_key(envrc));
    match fs::remove_file(&trust_file) {
        Ok(_) => eprintln!("denv: denied {}", envrc.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            eprintln!("denv: not currently allowed");
        }
        Err(err) => return Err(format!("failed to remove trust file: {err}")),
    }
    Ok(())
}

fn escape_newlines(s: &str) -> Cow<'_, str> {
    if !s.as_bytes().iter().any(|&b| b == b'\\' || b == b'\n') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut start = 0;
    for i in 0..bytes.len() {
        match bytes[i] {
            b'\\' => {
                out.push_str(&s[start..i]);
                out.push_str("\\\\");
                start = i + 1;
            }
            b'\n' => {
                out.push_str(&s[start..i]);
                out.push_str("\\n");
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push_str(&s[start..]);
    Cow::Owned(out)
}

fn unescape_newlines(s: &str) -> Cow<'_, str> {
    if !s.contains('\\') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

fn load_active(pid: &str) -> Option<ActiveState> {
    let content = fs::read_to_string(active_path(pid).ok()?).ok()?;
    let mut lines = content.lines();
    let dir = PathBuf::from(lines.next()?);
    let mtimes = lines.next()?;
    let (envrc_mtime, dotenv_mtime) = mtimes.split_once(' ')?;
    let envrc_mtime = envrc_mtime.parse().ok()?;
    let dotenv_mtime = dotenv_mtime.parse().ok()?;
    let mut prev = Vec::new();
    for line in lines {
        if let Some(eq) = line.find('=') {
            prev.push(PrevVar::Restore(
                line[..eq].to_string(),
                unescape_newlines(&line[eq + 1..]).into_owned(),
            ));
        } else if !line.is_empty() {
            prev.push(PrevVar::Unset(line.to_string()));
        }
    }
    Some(ActiveState {
        dir,
        envrc_mtime,
        dotenv_mtime,
        prev,
    })
}

fn save_active(pid: &str, state: &ActiveState) -> Result<(), String> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create data dir: {e}"))?;
    let mut buf = String::new();
    buf.push_str(&state.dir.to_string_lossy());
    buf.push('\n');
    write!(buf, "{} {}", state.envrc_mtime, state.dotenv_mtime).unwrap();
    buf.push('\n');
    for prev in &state.prev {
        match prev {
            PrevVar::Restore(key, value) => {
                buf.push_str(key);
                buf.push('=');
                buf.push_str(&escape_newlines(value));
                buf.push('\n');
            }
            PrevVar::Unset(key) => {
                buf.push_str(key);
                buf.push('\n');
            }
        }
    }
    fs::write(dir.join(format!("active_{pid}")), buf)
        .map_err(|e| format!("failed to write active file: {e}"))
}

fn clear_active(pid: &str) {
    if let Ok(path) = active_path(pid) {
        let _ = fs::remove_file(path);
    }
}

fn load_dotenv_entries(found: &EnvFiles) -> Result<Vec<(String, String)>, String> {
    let Some((path, _)) = &found.dotenv else {
        return Ok(Vec::new());
    };
    let content = fs::read_to_string(path).map_err(|e| format!("read .env: {e}"))?;
    Ok(parse_dotenv(&content)
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.into_owned()))
        .collect())
}

fn parse_dotenv(content: &str) -> Vec<(&str, Cow<'_, str>)> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim();
        if key.is_empty() {
            continue;
        }
        let value = line[eq + 1..].trim();
        let value = if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            let inner = &value[1..value.len() - 1];
            if inner.contains('\\') {
                let mut out = String::with_capacity(inner.len());
                let mut chars = inner.chars();
                while let Some(c) = chars.next() {
                    if c == '\\' {
                        match chars.next() {
                            Some('n') => out.push('\n'),
                            Some('t') => out.push('\t'),
                            Some('\\') => out.push('\\'),
                            Some('"') => out.push('"'),
                            Some('$') => out.push('$'),
                            Some(other) => {
                                out.push('\\');
                                out.push(other);
                            }
                            None => out.push('\\'),
                        }
                    } else {
                        out.push(c);
                    }
                }
                Cow::Owned(out)
            } else {
                Cow::Borrowed(inner)
            }
        } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
            Cow::Borrowed(&value[1..value.len() - 1])
        } else {
            Cow::Borrowed(value)
        };
        entries.push((key, value));
    }
    entries
}

fn diff_dotenv_only(dotenv_entries: &[(String, String)]) -> EnvDiff {
    let mut set = Vec::new();
    for (key, value) in dotenv_entries {
        if !env::var(key).is_ok_and(|cur| cur == *value) {
            set.push((key.clone(), value.clone()));
        }
    }
    EnvDiff {
        set,
        unset: Vec::new(),
    }
}

fn parse_env_null(data: &[u8]) -> Vec<(&str, &str)> {
    let mut entries = Vec::new();
    for entry in data.split(|&b| b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Ok(s) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some(eq) = s.find('=') else {
            continue;
        };
        let key = &s[..eq];
        if matches!(key, "_" | "SHLVL" | "PWD" | "OLDPWD") {
            continue;
        }
        entries.push((key, &s[eq + 1..]));
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    entries
}

fn diff_sorted_env(before: &[(&str, &str)], after: &[(&str, &str)]) -> EnvDiff {
    let (mut before_idx, mut after_idx) = (0, 0);
    let mut set = Vec::new();
    let mut unset = Vec::new();
    while before_idx < before.len() && after_idx < after.len() {
        match before[before_idx].0.cmp(after[after_idx].0) {
            Ordering::Less => {
                unset.push(before[before_idx].0.to_string());
                before_idx += 1;
            }
            Ordering::Greater => {
                set.push((
                    after[after_idx].0.to_string(),
                    after[after_idx].1.to_string(),
                ));
                after_idx += 1;
            }
            Ordering::Equal => {
                if before[before_idx].1 != after[after_idx].1 {
                    set.push((
                        after[after_idx].0.to_string(),
                        after[after_idx].1.to_string(),
                    ));
                }
                before_idx += 1;
                after_idx += 1;
            }
        }
    }
    for (key, _) in &before[before_idx..] {
        unset.push((*key).to_string());
    }
    for (key, value) in &after[after_idx..] {
        set.push(((*key).to_string(), (*value).to_string()));
    }
    EnvDiff { set, unset }
}

fn push_sh_escaped(out: &mut String, value: &str) {
    out.push('\'');
    let bytes = value.as_bytes();
    let mut start = 0;
    for i in 0..bytes.len() {
        if bytes[i] == b'\'' {
            out.push_str(&value[start..i]);
            out.push_str("'\\''");
            start = i + 1;
        }
    }
    out.push_str(&value[start..]);
    out.push('\'');
}

fn eval_env(
    dir: &Path,
    envrc: Option<&Path>,
    dotenv_entries: &[(String, String)],
    pid: &str,
) -> Result<EnvDiff, String> {
    let data = data_dir()?;
    fs::create_dir_all(&data).map_err(|e| format!("create data dir: {e}"))?;
    let before_path = data.join(format!("before_{pid}"));
    let after_path = data.join(format!("after_{pid}"));

    let mut script = String::with_capacity(BASH_STDLIB.len() + 256);
    script.push_str(BASH_STDLIB);
    script.push('\n');
    script.push_str("env -0 > ");
    push_sh_escaped(&mut script, &before_path.to_string_lossy());
    script.push('\n');
    if let Some(envrc) = envrc {
        script.push_str(". ");
        push_sh_escaped(&mut script, &envrc.to_string_lossy());
        script.push('\n');
    }
    for (key, value) in dotenv_entries {
        script.push_str("export ");
        script.push_str(key);
        script.push('=');
        push_sh_escaped(&mut script, value);
        script.push('\n');
    }
    script.push_str("env -0 > ");
    push_sh_escaped(&mut script, &after_path.to_string_lossy());
    script.push('\n');

    let stderr_dup = io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup stderr: {e}"))?;
    let status = Command::new("bash")
        .arg("-e")
        .arg("-c")
        .arg(&script)
        .current_dir(dir)
        .stdout(stderr_dup)
        .stderr(
            io::stderr()
                .as_fd()
                .try_clone_to_owned()
                .map_err(|e| format!("dup stderr for child: {e}"))?,
        )
        .status()
        .map_err(|e| format!("failed to run bash: {e}"))?;

    if !status.success() {
        let _ = fs::remove_file(&before_path);
        let _ = fs::remove_file(&after_path);
        return Err(".envrc evaluation failed".to_string());
    }

    let before_data = fs::read(&before_path).map_err(|e| format!("read before env: {e}"))?;
    let after_data = fs::read(&after_path).map_err(|e| format!("read after env: {e}"))?;
    let _ = fs::remove_file(&before_path);
    let _ = fs::remove_file(&after_path);

    let before = parse_env_null(&before_data);
    let after = parse_env_null(&after_data);
    Ok(diff_sorted_env(&before, &after))
}

fn capture_prev(diff: &EnvDiff) -> Vec<PrevVar> {
    let mut prev = Vec::new();
    for (key, _) in &diff.set {
        match env::var(key) {
            Ok(value) => prev.push(PrevVar::Restore(key.clone(), value)),
            Err(_) => prev.push(PrevVar::Unset(key.clone())),
        }
    }
    for key in &diff.unset {
        if let Ok(value) = env::var(key) {
            prev.push(PrevVar::Restore(key.clone(), value));
        }
    }
    prev
}

fn apply_restore(prev: &[PrevVar], changes: &mut Vec<EnvChange>) {
    for item in prev {
        match item {
            PrevVar::Restore(key, value) => set_change(changes, key, value.clone()),
            PrevVar::Unset(key) => unset_change(changes, key),
        }
    }
}

fn apply_diff(diff: &EnvDiff, changes: &mut Vec<EnvChange>) {
    for (key, value) in &diff.set {
        set_change(changes, key, value.clone());
    }
    for key in &diff.unset {
        unset_change(changes, key);
    }
}

fn clear_runtime_state(changes: &mut Vec<EnvChange>) {
    unset_change(changes, "__DENV_DIR");
    unset_change(changes, "__DENV_DIRTY");
    unset_change(changes, "__DENV_STATE");
}

fn set_change(changes: &mut Vec<EnvChange>, key: &str, value: String) {
    crate::shell_setenv(key, &value);
    changes.push(EnvChange::Set(key.to_string(), value));
}

fn unset_change(changes: &mut Vec<EnvChange>, key: &str) {
    crate::shell_unsetenv(key);
    changes.push(EnvChange::Unset(key.to_string()));
}

fn print_restore_summary(prev: &[PrevVar]) {
    print_summary(prev.iter().map(|item| match item {
        PrevVar::Restore(key, _) | PrevVar::Unset(key) => ('-', key.as_str()),
    }));
}

fn print_summary<'a>(items: impl Iterator<Item = (char, &'a str)>) {
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut first = true;
    for (sign, key) in items {
        if key.starts_with("__DENV_") {
            continue;
        }
        if first {
            let _ = write!(err, "denv: {sign}{key}");
            first = false;
        } else {
            let _ = write!(err, " {sign}{key}");
        }
    }
    if !first {
        let _ = writeln!(err);
    }
}

/// Benchmark-only compatibility helper for the old export/unset text parser.
/// Returns the number of directives parsed.
pub fn apply_bash_output_bench(output: &str) -> usize {
    let mut count = 0;
    for line in output.lines() {
        let line = line.trim_end_matches(';');
        if let Some(rest) = line.strip_prefix("export ") {
            if rest.contains('=') {
                count += 1;
            }
        } else if line.strip_prefix("unset ").is_some() {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), unique));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parse_dotenv_skips_empty_and_comments() {
        assert!(parse_dotenv("").is_empty());
        assert!(parse_dotenv("# comment\n\n# another\n").is_empty());
    }

    #[test]
    fn parse_dotenv_plain_and_export_prefix() {
        let entries = parse_dotenv("FOO=bar\nexport BAZ=qux");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "FOO");
        assert_eq!(entries[0].1.as_ref(), "bar");
        assert_eq!(entries[1].0, "BAZ");
        assert_eq!(entries[1].1.as_ref(), "qux");
    }

    #[test]
    fn parse_dotenv_double_quotes_unescape_common_sequences() {
        let entries =
            parse_dotenv("A=\"a\\nb\"\nB=\"c\\td\"\nC=\"say \\\"hi\\\"\"\nD=\"cost \\$5\"");
        assert_eq!(entries[0].1.as_ref(), "a\nb");
        assert_eq!(entries[1].1.as_ref(), "c\td");
        assert_eq!(entries[2].1.as_ref(), "say \"hi\"");
        assert_eq!(entries[3].1.as_ref(), "cost $5");
    }

    #[test]
    fn parse_dotenv_single_quotes_are_literal() {
        let entries = parse_dotenv("A='a\\nb'");
        assert_eq!(entries[0].1.as_ref(), "a\\nb");
    }

    #[test]
    fn parse_env_null_skips_shell_internal_vars() {
        let parsed = parse_env_null(b"PWD=/tmp\0OLDPWD=/old\0SHLVL=1\0KEEP=yes\0");
        assert_eq!(parsed, vec![("KEEP", "yes")]);
    }

    #[test]
    fn diff_sorted_env_reports_sets_and_unsets() {
        let before = [("A", "1"), ("B", "2"), ("C", "3")];
        let after = [("A", "1"), ("B", "changed"), ("D", "4")];
        let diff = diff_sorted_env(&before, &after);
        assert_eq!(
            diff.set,
            vec![
                ("B".to_string(), "changed".to_string()),
                ("D".to_string(), "4".to_string())
            ]
        );
        assert_eq!(diff.unset, vec!["C".to_string()]);
    }

    #[test]
    fn escape_roundtrip_preserves_newlines_and_backslashes() {
        let original = "line1\nline2\\tail";
        let escaped = escape_newlines(original);
        let restored = unescape_newlines(&escaped);
        assert_eq!(restored.as_ref(), original);
    }

    #[test]
    fn push_sh_escaped_handles_single_quotes() {
        let mut out = String::new();
        push_sh_escaped(&mut out, "it's here");
        assert_eq!(out, "'it'\\''s here'");
    }

    #[test]
    fn parse_denv_state_supports_spaces_in_dir() {
        let parsed = parse_denv_state("1 2 /tmp/path with spaces").unwrap();
        assert_eq!(parsed.0, 1);
        assert_eq!(parsed.1, 2);
        assert_eq!(parsed.2, "/tmp/path with spaces");
    }

    #[test]
    fn apply_bash_output_bench_counts_directives() {
        let count = apply_bash_output_bench("export A='1';\nunset B;\nexport C='3';");
        assert_eq!(count, 3);
    }

    #[test]
    fn state_var_fast_path_ok_uses_cached_dir() {
        let tmp = TempDir::new("ish_denv_fast_path");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let envrc = project.join(".envrc");
        std::fs::write(&envrc, "export OK=1\n").unwrap();
        let mtime = std::fs::metadata(&envrc).unwrap().mtime() as u64;
        let state = format!("{mtime} 0 {}", project.display());

        // SAFETY: tests run in a single process; this test sets and restores
        // only the env vars it uses.
        unsafe {
            std::env::set_var("__DENV_STATE", &state);
        }
        assert!(state_var_fast_path_ok(&project));

        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&envrc, "export OK=2\n").unwrap();
        assert!(!state_var_fast_path_ok(&project));

        // SAFETY: see comment above.
        unsafe {
            std::env::remove_var("__DENV_STATE");
        }
    }
}
