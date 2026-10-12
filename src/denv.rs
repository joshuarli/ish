//! Native .envrc/.env loading for ish.
//!
//! ish owns discovery, trust, active-state restore, and prompt flags directly.
//! `.env` is parsed in Rust. Trusted `.envrc` files are evaluated via `bash`
//! when line 1 mentions `bash`, otherwise via an embedded epsh shell, and then
//! diffed against the current environment.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod sha256;

/// Child-environment variable carrying the `.envrc` bytes that were hashed, so
/// bash evaluates exactly what was trusted rather than re-reading the path.
const ENVRC_SOURCE_VAR: &str = "__DENV_ENVRC_SOURCE";

/// Prefix of child-environment variables carrying `.env` values, so values
/// never become shell script text.
const DOTENV_VALUE_VAR_PREFIX: &str = "__DENV_DOT_";

/// Upper bound for one bash `.envrc` evaluation. The child holds the terminal
/// while ish waits, so a hung `.envrc` must fail loudly instead of wedging cd.
const ENVRC_EVAL_TIMEOUT: Duration = Duration::from_secs(60);

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

enum EnvRcInterpreter {
    Bash,
    Epsh,
}

/// Initialize ish's native denv state vars in epsh's variable store.
pub fn init(epsh: &mut epsh::eval::Shell) {
    let pid = std::process::id().to_string();
    let vars = epsh.vars_mut();
    let _ = vars.set("__DENV_PID", &pid);
    vars.export("__DENV_PID");
    let _ = vars.set("__DENV_SHELL", "bash");
    vars.export("__DENV_SHELL");
}

/// Called once at shell startup.
pub fn on_startup(epsh: &epsh::eval::Shell) -> Vec<EnvChange> {
    refresh(false, epsh).unwrap_or_else(report_error)
}

/// Called after any directory change.
pub fn on_cd(epsh: &epsh::eval::Shell) -> Vec<EnvChange> {
    refresh(false, epsh).unwrap_or_else(report_error)
}

/// Handle `denv allow|deny|reload`.
pub fn command(args: &[String], epsh: &epsh::eval::Shell) -> CommandOutcome {
    match args.first().map(|s| s.as_str()) {
        Some("allow") => match allow_current_dir(epsh) {
            Ok(changes) => CommandOutcome { status: 0, changes },
            Err(err) => {
                eprintln!("denv: {err}");
                CommandOutcome {
                    status: 1,
                    changes: Vec::new(),
                }
            }
        },
        Some("deny") => match deny_current_dir(epsh) {
            Ok(changes) => CommandOutcome { status: 0, changes },
            Err(err) => {
                eprintln!("denv: {err}");
                CommandOutcome {
                    status: 1,
                    changes: Vec::new(),
                }
            }
        },
        Some("reload") => match refresh(true, epsh) {
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

fn current_dir_from_store(epsh: &epsh::eval::Shell) -> PathBuf {
    epsh.vars()
        .get_bytes("PWD")
        .filter(|pwd| !pwd.as_bytes().is_empty())
        .map(|pwd| PathBuf::from(pwd.to_os_string()))
        .or_else(|| env::current_dir().ok())
        .unwrap_or_default()
}

fn refresh(force: bool, epsh: &epsh::eval::Shell) -> Result<Vec<EnvChange>, String> {
    let store_env_pairs = epsh.vars().env_for_command_os(&[]);
    // denv's bookkeeping and embedded epsh evaluator use process-global
    // environment APIs, so keep that environment as a temporary mirror.
    restore_process_env(&store_env_pairs);
    let cwd = current_dir_from_store(epsh);
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

    // Active denv state has now been restored in the process mirror. Capture
    // that post-restore baseline for the explicit child environment so reload
    // does not reintroduce values that should have been removed first.
    let env_pairs = capture_process_env();

    let Some(found) = found else {
        clear_runtime_state(&mut changes);
        if let Some(ref state) = active {
            print_restore_summary(&state.prev);
        }
        return Ok(changes);
    };

    let envrc_mtime = found.envrc.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let dotenv_mtime = found.dotenv.as_ref().map(|(_, m)| *m).unwrap_or(0);

    // Each file is read once; the trust check and the evaluation below both use
    // these bytes, so a file swapped after the check cannot run untrusted.
    let envrc_file = found
        .envrc
        .as_ref()
        .map(|(path, _)| read_trusted_candidate(path))
        .transpose()?;
    let dotenv_file = found
        .dotenv
        .as_ref()
        .map(|(path, _)| read_trusted_candidate(path))
        .transpose()?;

    // `.env` is as powerful as `.envrc` (it can set PATH or LD_*), so it needs
    // the same trust.
    if let Some(blocked) = [&envrc_file, &dotenv_file]
        .into_iter()
        .flatten()
        .find(|file| !is_allowed(file))
    {
        let dir = canonicalize_fallback(&found.dir);
        eprintln!(
            "denv: {} is blocked. Run `denv allow` to trust it.",
            blocked.path.display()
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

    let dotenv_entries = match &dotenv_file {
        Some(file) => load_dotenv_entries(&file.bytes)?,
        None => Vec::new(),
    };
    if found.envrc.is_some() {
        eprintln!("denv: loading .envrc");
    }
    if found.dotenv.is_some() {
        eprintln!("denv: loading .env");
    }

    let diff = if let Some(envrc_file) = &envrc_file {
        let dir = canonicalize_fallback(&found.dir);
        match eval_env(&dir, Some(&envrc_file.bytes), &dotenv_entries, &env_pairs) {
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
    } else {
        diff_dotenv_only(&dotenv_entries)
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

fn allow_current_dir(epsh: &epsh::eval::Shell) -> Result<Vec<EnvChange>, String> {
    let cwd = current_dir_from_store(epsh);
    let found = find_env_files(&cwd).ok_or("no .envrc or .env found")?;
    for (path, _) in found.envrc.iter().chain(found.dotenv.iter()) {
        allow_file(&read_trusted_candidate(path)?)?;
    }
    refresh(true, epsh)
}

fn deny_current_dir(epsh: &epsh::eval::Shell) -> Result<Vec<EnvChange>, String> {
    let cwd = current_dir_from_store(epsh);
    let found = find_env_files(&cwd).ok_or("no .envrc or .env found")?;
    for (path, _) in found.envrc.iter().chain(found.dotenv.iter()) {
        deny_file(&canonicalize_fallback(path))?;
    }
    refresh(true, epsh)
}

fn find_env_files(start: &Path) -> Option<EnvFiles> {
    let mut dir = start.to_path_buf();
    let mut probe = PathBuf::new();
    loop {
        let envrc = stat_file_with_probe(&dir, ".envrc", &mut probe);
        let dotenv = stat_file_with_probe(&dir, ".env", &mut probe);
        if envrc.is_some() || dotenv.is_some() {
            return Some(EnvFiles { dir, envrc, dotenv });
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn stat_file_with_probe(dir: &Path, name: &str, probe: &mut PathBuf) -> Option<(PathBuf, u64)> {
    set_probe_path(probe, dir, name);
    let mtime = stat_regular_file_mtime(probe)?;
    Some((probe.clone(), mtime))
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
    let Some(state) = env::var_os("__DENV_STATE") else {
        return false;
    };
    let state = state.to_string_lossy();
    let Some((envrc_mtime, dotenv_mtime, dir)) = parse_denv_state(&state) else {
        return false;
    };
    let cached = Path::new(dir);
    if !cwd.starts_with(cached) {
        return false;
    }

    let mut probe = PathBuf::new();
    let envrc_ok = envrc_mtime == 0
        || stat_file_mtime_with_probe(cached, ".envrc", &mut probe)
            .is_some_and(|mtime| mtime == envrc_mtime);
    if !envrc_ok {
        return false;
    }

    dotenv_mtime == 0
        || stat_file_mtime_with_probe(cached, ".env", &mut probe)
            .is_some_and(|mtime| mtime == dotenv_mtime)
}

fn stat_file_mtime_with_probe(dir: &Path, name: &str, probe: &mut PathBuf) -> Option<u64> {
    set_probe_path(probe, dir, name);
    stat_regular_file_mtime(probe)
}

fn set_probe_path(probe: &mut PathBuf, dir: &Path, name: &str) {
    probe.clear();
    probe.push(dir);
    probe.push(name);
}

fn state_var_matches(found: &EnvFiles) -> bool {
    let envrc_mtime = found.envrc.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let dotenv_mtime = found.dotenv.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let Some(state) = env::var_os("__DENV_STATE") else {
        return false;
    };
    let state = state.to_string_lossy();
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
    let home = env::var_os("HOME").ok_or("HOME and DENV_DATA_DIR are unset")?;
    Ok(PathBuf::from(home).join(".local/share/denv"))
}

fn allow_dir() -> Result<PathBuf, String> {
    Ok(data_dir()?.join("allow"))
}

fn active_path(pid: &str) -> Result<PathBuf, String> {
    Ok(data_dir()?.join(format!("active_{pid}")))
}

fn stat_regular_file_mtime(path: &Path) -> Option<u64> {
    let st = rustix::fs::stat(path).ok()?;
    (rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::RegularFile)
        .then_some(st.st_mtime as u64)
}

/// A `.envrc` or `.env` read once, with the canonical path it was read from
/// and the hash of exactly those bytes.
struct TrustCandidate {
    path: PathBuf,
    bytes: Vec<u8>,
    hash: String,
}

fn read_trusted_candidate(path: &Path) -> Result<TrustCandidate, String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("resolve {}: {e}", path.display()))?;
    let bytes = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let hash = sha256::hex_digest(&bytes);
    Ok(TrustCandidate { path, bytes, hash })
}

/// Trust binds the canonical path to the content hash, so touching mtimes or
/// replacing the file's contents revokes it.
fn is_allowed(file: &TrustCandidate) -> bool {
    allow_dir().is_ok_and(|dir| is_allowed_in(&dir, file))
}

fn is_allowed_in(allow_dir: &Path, file: &TrustCandidate) -> bool {
    fs::read_to_string(allow_dir.join(trust_key(&file.path)))
        .is_ok_and(|stored| stored.trim() == file.hash)
}

fn allow_file(file: &TrustCandidate) -> Result<(), String> {
    let dir = allow_dir()?;
    allow_file_in(&dir, file)?;
    eprintln!("denv: allowed {}", file.path.display());
    Ok(())
}

fn allow_file_in(dir: &Path, file: &TrustCandidate) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create allow dir: {e}"))?;
    fs::write(dir.join(trust_key(&file.path)), &file.hash)
        .map_err(|e| format!("failed to write trust file: {e}"))
}

fn deny_file(path: &Path) -> Result<(), String> {
    let trust_file = allow_dir()?.join(trust_key(path));
    match fs::remove_file(&trust_file) {
        Ok(_) => eprintln!("denv: denied {}", path.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            eprintln!("denv: {} not currently allowed", path.display());
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

/// Keys the `.env` file may never set: the trust store location and denv's own
/// bookkeeping.
fn is_reserved_dotenv_key(key: &str) -> bool {
    key == "DENV_DATA_DIR" || key.starts_with("__DENV_")
}

fn is_valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Parse `.env` bytes, skipping (with a warning) entries that cannot be set
/// safely: malformed keys, NUL in values, and reserved names.
fn load_dotenv_entries(bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let content = std::str::from_utf8(bytes).map_err(|e| format!("read .env: {e}"))?;
    let mut entries = Vec::new();
    for (key, value) in parse_dotenv(content) {
        if !is_valid_env_key(key) {
            eprintln!("denv: .env: skipping invalid variable name {key:?}");
        } else if is_reserved_dotenv_key(key) {
            eprintln!("denv: .env: refusing to set {key}");
        } else if value.contains('\0') {
            eprintln!("denv: .env: skipping {key}: value contains NUL");
        } else {
            entries.push((key.to_string(), value.into_owned()));
        }
    }
    Ok(entries)
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
        if env::var_os(key).is_none_or(|cur| cur != OsStr::new(value)) {
            set.push((key.clone(), value.clone()));
        }
    }
    EnvDiff {
        set,
        unset: Vec::new(),
    }
}

fn is_ignored_env_key(key: &str) -> bool {
    matches!(key, "_" | "SHLVL" | "PWD" | "OLDPWD")
        || key.starts_with(ENVRC_SOURCE_VAR)
        || key.starts_with(DOTENV_VALUE_VAR_PREFIX)
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
        if is_ignored_env_key(key) {
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

fn first_line_uses_bash(line: &[u8]) -> bool {
    line.windows(4).any(|window| window == b"bash")
}

fn envrc_interpreter(source: &[u8]) -> EnvRcInterpreter {
    let head = &source[..source.len().min(256)];
    let first_line = head.split(|&b| b == b'\n').next().unwrap_or_default();
    if first_line_uses_bash(first_line) {
        EnvRcInterpreter::Bash
    } else {
        EnvRcInterpreter::Epsh
    }
}

fn capture_process_env() -> Vec<(OsString, OsString)> {
    env::vars_os().collect()
}

fn filtered_env_from_snapshot(entries: &[(OsString, OsString)]) -> Vec<(&str, &str)> {
    let mut filtered: Vec<(&str, &str)> = entries
        .iter()
        .filter_map(|(key, value)| {
            let key = key.to_str()?;
            let value = value.to_str()?;
            (!is_ignored_env_key(key)).then_some((key, value))
        })
        .collect();
    filtered.sort_unstable_by(|a, b| a.0.cmp(b.0));
    filtered
}

fn filtered_env_from_owned(entries: &[(String, String)]) -> Vec<(&str, &str)> {
    let mut filtered: Vec<(&str, &str)> = entries
        .iter()
        .filter(|(key, _)| !is_ignored_env_key(key))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    filtered.sort_unstable_by(|a, b| a.0.cmp(b.0));
    filtered
}

fn restore_process_env(snapshot: &[(OsString, OsString)]) {
    for key in env::vars_os().map(|(key, _)| key).collect::<Vec<_>>() {
        if !snapshot.iter().any(|(saved_key, _)| saved_key == &key) {
            // SAFETY: ish is single-threaded.
            unsafe {
                env::remove_var(&key);
            }
        }
    }
    for (key, value) in snapshot {
        // SAFETY: ish is single-threaded.
        unsafe {
            env::set_var(key, value);
        }
    }
}

struct SharedStderr;

impl Write for SharedStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::stderr().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

fn eval_env(
    dir: &Path,
    envrc_source: Option<&[u8]>,
    dotenv_entries: &[(String, String)],
    env_pairs: &[(OsString, OsString)],
) -> Result<EnvDiff, String> {
    let Some(source) = envrc_source else {
        return Ok(diff_dotenv_only(dotenv_entries));
    };
    match envrc_interpreter(source) {
        EnvRcInterpreter::Bash => eval_env_bash(
            dir,
            source,
            dotenv_entries,
            env_pairs,
            ENVRC_EVAL_TIMEOUT,
        ),
        EnvRcInterpreter::Epsh => eval_env_epsh(dir, source, dotenv_entries, env_pairs),
    }
}

fn eval_env_bash(
    dir: &Path,
    source: &[u8],
    dotenv_entries: &[(String, String)],
    env_pairs: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<EnvDiff, String> {
    // An environment variable cannot carry NUL, and bash would truncate there.
    if source.contains(&0) {
        return Err(".envrc contains a NUL byte".to_string());
    }
    let (before_r, before_w) =
        crate::sys::pipe_cloexec().map_err(|e| format!("create before pipe: {e}"))?;
    let (after_r, after_w) =
        crate::sys::pipe_cloexec().map_err(|e| format!("create after pipe: {e}"))?;
    // SAFETY: these fds come directly from pipe_cloexec and are uniquely owned here.
    let before_r = unsafe { OwnedFd::from_raw_fd(before_r) };
    // SAFETY: these fds come directly from pipe_cloexec and are uniquely owned here.
    let before_w = unsafe { OwnedFd::from_raw_fd(before_w) };
    // SAFETY: these fds come directly from pipe_cloexec and are uniquely owned here.
    let after_r = unsafe { OwnedFd::from_raw_fd(after_r) };
    // SAFETY: these fds come directly from pipe_cloexec and are uniquely owned here.
    let after_w = unsafe { OwnedFd::from_raw_fd(after_w) };
    // Keep stable source fds for the child remap. The pipe fds can occupy 3 or
    // 4 themselves, so remapping directly from them can overwrite the other
    // source before it has been installed.
    let before_src = rustix::io::dup(&before_w).map_err(|e| format!("dup before pipe: {e}"))?;
    let after_src = rustix::io::dup(&after_w).map_err(|e| format!("dup after pipe: {e}"))?;
    rustix::io::fcntl_setfd(&before_src, rustix::io::FdFlags::CLOEXEC)
        .map_err(|e| format!("set before pipe close-on-exec: {e}"))?;
    rustix::io::fcntl_setfd(&after_src, rustix::io::FdFlags::CLOEXEC)
        .map_err(|e| format!("set after pipe close-on-exec: {e}"))?;

    let mut script = String::with_capacity(BASH_STDLIB.len() + 256);
    script.push_str(BASH_STDLIB);
    script.push('\n');
    // Keep the source out of the exported environment seen by the .envrc.
    script.push_str("__denv_src=$");
    script.push_str(ENVRC_SOURCE_VAR);
    script.push_str("\nunset ");
    script.push_str(ENVRC_SOURCE_VAR);
    script.push_str("\nenv -0 >&3\neval \"$__denv_src\"\nunset __denv_src\n");
    // Keys were validated as identifiers; values travel through the environment.
    for (i, (key, _)) in dotenv_entries.iter().enumerate() {
        writeln!(script, "export {key}=\"${DOTENV_VALUE_VAR_PREFIX}{i}\"").unwrap();
    }
    script.push_str("env -0 >&4\n");

    let stderr_dup = io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup stderr: {e}"))?;
    let before_read = std::thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut file = fs::File::from(before_r);
        file.read_to_end(&mut data)?;
        Ok(data)
    });
    let after_read = std::thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut file = fs::File::from(after_r);
        file.read_to_end(&mut data)?;
        Ok(data)
    });

    let mut cmd = Command::new("bash");
    cmd.env_clear();
    for (key, value) in env_pairs {
        cmd.env(key, value);
    }
    cmd.env(ENVRC_SOURCE_VAR, OsStr::from_bytes(source));
    for (i, (_, value)) in dotenv_entries.iter().enumerate() {
        cmd.env(format!("{DOTENV_VALUE_VAR_PREFIX}{i}"), value);
    }
    // SAFETY: pre_exec runs in the child after fork and before exec. We reset
    // signal dispositions (ish ignores SIGINT, which exec would otherwise
    // preserve, making the .envrc uninterruptible) and dup the dedicated pipe
    // fds into 3/4 for the shell snapshot script.
    unsafe {
        cmd.pre_exec(move || {
            crate::signal::restore_defaults();
            let mut before_target = OwnedFd::from_raw_fd(3);
            rustix::io::dup2(&before_src, &mut before_target).map_err(io::Error::from)?;
            std::mem::forget(before_target);
            let mut after_target = OwnedFd::from_raw_fd(4);
            rustix::io::dup2(&after_src, &mut after_target).map_err(io::Error::from)?;
            std::mem::forget(after_target);
            Ok(())
        });
    }
    let mut child = cmd
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
        .spawn()
        .map_err(|e| format!("failed to run bash: {e}"))?;
    drop(cmd);
    drop(before_w);
    drop(after_w);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // Reader threads are left detached: a grandchild may still hold
                // the pipe write ends open.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    ".envrc evaluation timed out after {}s",
                    timeout.as_secs_f32()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("wait for bash: {e}")),
        }
    };

    let before_data = before_read
        .join()
        .map_err(|_| "read before env thread panicked".to_string())?
        .map_err(|e| format!("read before env: {e}"))?;
    let after_data = after_read
        .join()
        .map_err(|_| "read after env thread panicked".to_string())?
        .map_err(|e| format!("read after env: {e}"))?;

    if !status.success() {
        return Err(".envrc evaluation failed".to_string());
    }

    let before = parse_env_null(&before_data);
    let after = parse_env_null(&after_data);
    Ok(diff_sorted_env(&before, &after))
}

fn eval_env_epsh(
    dir: &Path,
    source: &[u8],
    dotenv_entries: &[(String, String)],
    env_pairs: &[(OsString, OsString)],
) -> Result<EnvDiff, String> {
    let source = std::str::from_utf8(source).map_err(|e| format!(".envrc is not UTF-8: {e}"))?;
    let process_before = capture_process_env();
    restore_process_env(env_pairs);
    let before_process = env_pairs.to_vec();
    let sink = std::sync::Arc::new(std::sync::Mutex::new(SharedStderr));
    let mut shell = epsh::eval::Shell::builder()
        .cwd(dir.to_path_buf())
        .stdout_sink(sink.clone())
        .stderr_sink(sink)
        .build();

    let mut script = String::with_capacity(BASH_STDLIB.len() + 256);
    script.push_str(BASH_STDLIB);
    script.push('\n');
    script.push_str(source);
    script.push('\n');

    let status = shell.run_script(&script);
    // Applied through the variable store, never as script text.
    let mut dotenv_failed = None;
    if status == 0 {
        let vars = shell.vars_mut();
        for (key, value) in dotenv_entries {
            if let Err(err) = vars.set(key, value) {
                dotenv_failed = Some(format!("set {key} from .env: {err:?}"));
                break;
            }
            vars.export(key);
        }
    }
    let after_process = shell.vars().exported_env();
    restore_process_env(&process_before);
    if let Some(err) = dotenv_failed {
        return Err(err);
    }

    if status != 0 {
        return Err(".envrc evaluation failed".to_string());
    }

    let before = filtered_env_from_snapshot(&before_process);
    let after = filtered_env_from_owned(&after_process);
    Ok(diff_sorted_env(&before, &after))
}

fn capture_prev(diff: &EnvDiff) -> Vec<PrevVar> {
    let mut prev = Vec::new();
    for (key, _) in &diff.set {
        match env::var_os(key) {
            Some(value) => prev.push(PrevVar::Restore(
                key.clone(),
                value.to_string_lossy().into_owned(),
            )),
            None => prev.push(PrevVar::Unset(key.clone())),
        }
    }
    for key in &diff.unset {
        if let Some(value) = env::var_os(key) {
            prev.push(PrevVar::Restore(
                key.clone(),
                value.to_string_lossy().into_owned(),
            ));
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
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
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
    fn first_line_uses_bash_detects_substring() {
        assert!(first_line_uses_bash(b"#!/usr/bin/env bash"));
        assert!(first_line_uses_bash(b"bash -eu"));
        assert!(!first_line_uses_bash(b"#!/bin/sh"));
    }

    #[test]
    fn eval_env_uses_epsh_for_non_bash_envrc() {
        let tmp = TempDir::new("ish_denv_epsh_eval");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source: &[u8] = b"#!/bin/sh\nexport DENV_EPSH_TEST=ok\n";

        let epsh = epsh::eval::Shell::new();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let diff = eval_env(&project, Some(source), &[], &env_pairs).unwrap();
        assert!(
            diff.set
                .iter()
                .any(|(key, value)| key == "DENV_EPSH_TEST" && value == "ok")
        );
    }

    #[test]
    fn eval_env_bash_uses_explicit_store_environment() {
        let tmp = TempDir::new("ish_denv_bash_env");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source: &[u8] =
            b"#!/usr/bin/env bash\nif [ -n \"${DENV_AMBIENT_ONLY+x}\" ]; then export DENV_LEAK=1; fi\n";

        unsafe { std::env::set_var("DENV_AMBIENT_ONLY", "ambient") };
        let epsh = epsh::eval::Shell::builder().env_clear().build();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let diff = eval_env(&project, Some(source), &[], &env_pairs).unwrap();
        assert!(!diff.set.iter().any(|(key, _)| key == "DENV_LEAK"));
        unsafe { std::env::remove_var("DENV_AMBIENT_ONLY") };
    }

    #[test]
    fn eval_env_epsh_restores_process_env_after_failure() {
        let tmp = TempDir::new("ish_denv_epsh_restore");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source: &[u8] = b"#!/bin/sh\nexport DENV_TMP_SHOULD_ROLLBACK=1\nfalse\n";

        crate::shell_unsetenv("DENV_TMP_SHOULD_ROLLBACK");
        let epsh = epsh::eval::Shell::new();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let err = match eval_env_epsh(&project, source, &[], &env_pairs) {
            Ok(_) => panic!("expected epsh envrc evaluation to fail"),
            Err(err) => err,
        };
        assert_eq!(err, ".envrc evaluation failed");
        assert!(std::env::var_os("DENV_TMP_SHOULD_ROLLBACK").is_none());
    }

    #[test]
    fn restore_process_env_preserves_non_utf8_entries() {
        let key = OsString::from_vec(vec![b'D', b'E', b'N', b'V', b'_', 0xf0, 0x80, b'K']);
        let value = OsString::from_vec(vec![b'v', b'a', b'l', 0xff]);
        let extra = "DENV_TMP_SHOULD_BE_REMOVED";

        let saved_key = std::env::var_os(&key);
        let saved_extra = std::env::var_os(extra);
        let snapshot = vec![(key.clone(), value.clone())];

        crate::shell_setenv(extra, "1");
        restore_process_env(&snapshot);

        assert_eq!(std::env::var_os(&key), Some(value.clone()));
        assert!(std::env::var_os(extra).is_none());

        if let Some(saved) = saved_key {
            unsafe {
                std::env::set_var(&key, saved);
            }
        } else {
            unsafe {
                std::env::remove_var(&key);
            }
        }
        if let Some(saved) = saved_extra {
            crate::shell_setenv_os(extra, saved);
        } else {
            crate::shell_unsetenv(extra);
        }
    }

    #[test]
    fn state_var_fast_path_ok_uses_cached_dir() {
        let tmp = TempDir::new("ish_denv_fast_path");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let envrc = project.join(".envrc");
        std::fs::write(&envrc, "export OK=1\n").unwrap();
        let mtime = stat_regular_file_mtime(&envrc).unwrap();
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

    #[test]
    fn dotenv_rejects_injection_keys_nul_and_reserved_names() {
        let entries = load_dotenv_entries(
            b"A;touch X;B=1\nGOOD=ok\nBAD=a\0b\nDENV_DATA_DIR=/x\n__DENV_STATE=1\n1X=2\n",
        )
        .unwrap();
        assert_eq!(entries, vec![("GOOD".to_string(), "ok".to_string())]);
    }

    #[test]
    fn dotenv_values_reach_bash_as_data_not_script() {
        let tmp = TempDir::new("ish_denv_dotenv_data");
        let marker = tmp.path().join("marker");
        let value = format!("it's \"q\" $(touch {}) `x`\nline2", marker.display());
        let entries = vec![("DENV_DOTENV_VALUE".to_string(), value.clone())];
        let epsh = epsh::eval::Shell::builder().env_clear().build();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let diff = eval_env_bash(
            tmp.path(),
            b"#!/usr/bin/env bash\n:\n",
            &entries,
            &env_pairs,
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(diff.set, vec![("DENV_DOTENV_VALUE".to_string(), value)]);
        assert!(!marker.exists());
    }

    #[test]
    fn dotenv_values_reach_epsh_as_data_not_script() {
        let tmp = TempDir::new("ish_denv_dotenv_epsh");
        let marker = tmp.path().join("marker");
        let value = format!("it's $(touch {}) `x`", marker.display());
        let entries = vec![("DENV_DOTENV_VALUE".to_string(), value.clone())];
        let epsh = epsh::eval::Shell::new();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let diff = eval_env_epsh(tmp.path(), b"#!/bin/sh\n:\n", &entries, &env_pairs).unwrap();
        assert!(diff.set.contains(&("DENV_DOTENV_VALUE".to_string(), value)));
        assert!(!marker.exists());
    }

    #[test]
    fn bash_envrc_evaluation_times_out() {
        let tmp = TempDir::new("ish_denv_timeout");
        let epsh = epsh::eval::Shell::new();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let start = Instant::now();
        let err = match eval_env_bash(
            tmp.path(),
            b"#!/usr/bin/env bash\nexec sleep 30\n",
            &[],
            &env_pairs,
            Duration::from_millis(300),
        ) {
            Ok(_) => panic!("expected timeout"),
            Err(err) => err,
        };
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn trust_binds_content_hash_not_mtime() {
        let tmp = TempDir::new("ish_denv_trust");
        let data = tmp.path().join("data");
        let file = tmp.path().join(".env");
        std::fs::write(&file, "A=1\n").unwrap();
        // The process environment is shared with tests that rewrite it, so
        // the allow directory is passed explicitly.
        let allow = data.join("allow");

        let before = read_trusted_candidate(&file).unwrap();
        assert!(!is_allowed_in(&allow, &before));
        allow_file_in(&allow, &before).unwrap();
        assert!(is_allowed_in(&allow, &read_trusted_candidate(&file).unwrap()));

        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, "A=2\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert!(!is_allowed_in(&allow, &read_trusted_candidate(&file).unwrap()));
    }
}
