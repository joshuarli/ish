
use std::borrow::Cow;
use std::cmp::Ordering;
#[cfg(test)]
use std::env;
use epsh::shell_bytes::ShellBytes;
use epsh::var::{Variables, VarFlags};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

mod failure;
mod sha256;

use failure::{ChannelState, Completion, FailureChannel};

const ENVRC_SOURCE_VAR: &str = "__DENV_ENVRC_SOURCE";

const DOTENV_VALUE_VAR_PREFIX: &str = "__DENV_DOT_";

const ENVRC_EVAL_TIMEOUT: Duration = Duration::from_secs(60);
const ACTIVE_TEMP_NAME_ATTEMPTS: u64 = 128;
const ACTIVE_FORMAT_HEADER: &str = "DENV-ACTIVE 2";
const BEFORE_SNAPSHOT_FD: RawFd = 3;
const AFTER_SNAPSHOT_FD: RawFd = 4;
const SNAPSHOT_SOURCE_FD_MIN: RawFd = AFTER_SNAPSHOT_FD + 1;

static NEXT_ACTIVE_TEMP_ID: AtomicU64 = AtomicU64::new(0);

const BASH_STDLIB: &str = include_str!("denv_stdlib.sh");

enum EnvChange {
    Set(String, ShellBytes),
    Unset(String),
    Unexport(String),
    Declare(String, bool),
}

pub struct Session {
    data_dir: Result<PathBuf, String>,
    record_id: Result<String, String>,
}

impl Session {
    fn data_dir(&self) -> Result<&Path, String> {
        self.data_dir.as_deref().map_err(Clone::clone)
    }

    fn record_id(&self) -> Result<&str, String> {
        self.record_id.as_deref().map_err(Clone::clone)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        on_exit(self);
    }
}

#[derive(Debug)]
enum ActiveLoadError {
    Unreadable(String),
    Malformed(String),
}

impl ActiveLoadError {
    fn into_message(self) -> String {
        match self {
            Self::Unreadable(message) | Self::Malformed(message) => message,
        }
    }
}

#[cfg(target_os = "linux")]
fn current_pid_namespace() -> Result<Option<u64>, String> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self/ns/pid")
        .map(|meta| Some(meta.ino()))
        .map_err(|err| format!("cannot read /proc/self/ns/pid: {err}"))
}

#[cfg(not(target_os = "linux"))]
fn current_pid_namespace() -> Result<Option<u64>, String> {
    Ok(None)
}

fn new_record_id(pid_namespace: Option<u64>) -> Result<String, String> {
    let mut token = [0u8; 16];
    loop {
        match rustix::rand::getrandom(&mut token, rustix::rand::GetRandomFlags::empty()) {
            Ok(filled) if filled == token.len() => break,
            Ok(filled) => {
                return Err(format!("getrandom returned {filled} of {} bytes", token.len()));
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(format!("getrandom failed: {err}")),
        }
    }
    let mut id = std::process::id().to_string();
    id.push('-');
    if let Some(namespace) = pid_namespace {
        write!(id, "{namespace}-").expect("writing to a String cannot fail");
    }
    for byte in token {
        write!(id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(id)
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
    Restore(String, ShellBytes, bool),
    Unassigned(String, bool),
    Unset(String),
}

struct EnvDiff {
    set: Vec<(String, ShellBytes)>,
    unset: Vec<String>,
}

enum EnvRcInterpreter {
    Bash,
    Epsh,
}

pub fn init(epsh: &mut epsh::eval::Shell) -> Session {
    let data_dir = data_dir(epsh);
    let pid_namespace = current_pid_namespace();
    let record_id = pid_namespace
        .clone()
        .and_then(new_record_id)
        .map_err(|err| format!("cannot name the active record: {err}"));
    let record_id = record_id.and_then(|record_id| {
        let mut variables = epsh.vars().clone();
        variables.set("__DENV_PID", &record_id)?;
        variables.export("__DENV_PID");
        variables.set("__DENV_SHELL", "bash")?;
        variables.export("__DENV_SHELL");
        *epsh.vars_mut() = variables;
        Ok(record_id)
    });
    if let Ok(data_dir) = &data_dir
        && let Ok(pid_namespace) = &pid_namespace
        && let Err(err) = remove_dead_shell_records(data_dir, *pid_namespace)
    {
        eprintln!("denv: {err}");
    }
    Session { data_dir, record_id }
}

pub fn on_exit(session: &Session) {
    let (Ok(data_dir), Ok(record_id)) = (session.data_dir(), session.record_id()) else {
        return;
    };
    if let Err(err) = clear_active(data_dir, record_id) {
        eprintln!("denv: {err}");
    }
}

/// Refresh the environment at startup; errors leave the caller's store unchanged.
pub fn on_startup(session: &Session, shell: &mut epsh::eval::Shell) -> Result<(), String> {
    refresh(session, false, shell).map(|_| ())
}

/// Refresh after a cwd change, using the supplied shell's cwd and variables.
pub fn on_cd(session: &Session, shell: &mut epsh::eval::Shell) -> Result<(), String> {
    refresh(session, false, shell).map(|_| ())
}

/// Execute a denv command and publish a validated, persisted environment transition.
pub fn command(session: &Session, args: &[String], shell: &mut epsh::eval::Shell) -> i32 {
    let result = match args.first().map(String::as_str) {
        Some("allow") => allow_current_dir(session, shell),
        Some("deny") => deny_current_dir(session, shell),
        Some("reload") => refresh(session, true, shell),
        _ => Err("usage: denv <allow|deny|reload>".into()),
    };
    match result {
        Ok(_) => 0,
        Err(error) => { eprintln!("denv: {error}"); 1 }
    }
}

fn current_dir_from_store(shell: &epsh::eval::Shell) -> Result<PathBuf, String> {
    let cwd = shell.cwd();
    if !cwd.is_absolute() { return Err("denv requires an absolute shell working directory".into()); }
    Ok(cwd.to_path_buf())
}

enum RecordUpdate { Keep, Clear, Save(ActiveState) }
struct Transition { changes: Vec<EnvChange>, record: RecordUpdate }

fn apply_changes(variables: &mut Variables, changes: &[EnvChange]) -> Result<(), String> {
    for change in changes {
        match change {
            EnvChange::Set(name, value) => { variables.set_bytes(name, value.clone())?; variables.export(name); }
            EnvChange::Unset(name) => variables.unset(name)?,
            EnvChange::Unexport(name) => variables.unexport(name),
            EnvChange::Declare(name, exported) => { variables.declare(name); if *exported { variables.export(name); } },
        }
    }
    Ok(())
}

fn refresh(session: &Session, force: bool, epsh: &mut epsh::eval::Shell) -> Result<Vec<EnvChange>, String> {
    let data_dir = session.data_dir()?;
    let record_id = session.record_id()?;
    let active = match load_active(data_dir, record_id) {
        Ok(active) => active,
        Err(ActiveLoadError::Malformed(message)) if force => {
            clear_active(data_dir, record_id)?;
            eprintln!(
                "denv: discarded unreadable active record ({message}); variables it saved cannot be restored"
            );
            None
        }
        Err(err) => return Err(err.into_message()),
    };
    let transition = refresh_inner(force, epsh, active, &data_dir.join("allow"))?;
    let mut candidate = epsh.vars().clone();
    apply_changes(&mut candidate, &transition.changes)?;
    match transition.record {
        RecordUpdate::Keep => {}
        RecordUpdate::Clear => clear_active(data_dir, record_id)?,
        RecordUpdate::Save(state) => save_active(data_dir, record_id, &state)?,
    }
    *epsh.vars_mut() = candidate;
    print_summary(transition.changes.iter().filter_map(|change| match change {
        EnvChange::Set(name, _) => Some(('+', name.as_str())),
        EnvChange::Unset(name) => Some(('-', name.as_str())),
        EnvChange::Unexport(_) | EnvChange::Declare(_, _) => None,
    }));
    Ok(transition.changes)
}

fn refresh_inner(
    force: bool,
    epsh: &epsh::eval::Shell,
    active: Option<ActiveState>,
    allow_dir: &Path,
) -> Result<Transition, String> {
    let cwd = current_dir_from_store(epsh)?;

    let found = find_env_files(&cwd);

    let envrc_file = found
        .as_ref()
        .and_then(|found| found.envrc.as_ref())
        .map(|(path, _)| read_trusted_candidate(path))
        .transpose()?;
    let dotenv_file = found
        .as_ref()
        .and_then(|found| found.dotenv.as_ref())
        .map(|(path, _)| read_trusted_candidate(path))
        .transpose()?;
    let files_allowed = [&envrc_file, &dotenv_file]
        .into_iter()
        .flatten()
        .all(|file| is_allowed_in(allow_dir, file));
    let dotenv_entries = if files_allowed {
        match &dotenv_file {
            Some(file) => load_dotenv_entries(&file.bytes)?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    if !force && let Some(ref found) = found {
        if files_allowed && state_var_matches(found, epsh.vars()) {
            return Ok(Transition { changes: Vec::new(), record: RecordUpdate::Keep });
        }
        if files_allowed
            && let Some(active) = &active
            && active.envrc_mtime == found.envrc.as_ref().map(|(_, m)| *m).unwrap_or(0)
            && active.dotenv_mtime == found.dotenv.as_ref().map(|(_, m)| *m).unwrap_or(0)
            && paths_match(&active.dir, &found.dir)
        {
            return Ok(Transition { changes: Vec::new(), record: RecordUpdate::Keep });
        }
    }

    let mut changes = Vec::new();

    if let Some(ref state) = active {
        apply_restore(&state.prev, &mut changes);
    }

    let mut working = epsh.vars().clone();
    apply_changes(&mut working, &changes)?;
    let env_pairs = working.env_for_command_os(&[]);

    let Some(found) = found else {
        clear_runtime_state(&mut changes);
        return Ok(Transition { changes, record: RecordUpdate::Clear });
    };

    let envrc_mtime = found.envrc.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let dotenv_mtime = found.dotenv.as_ref().map(|(_, m)| *m).unwrap_or(0);

    if let Some(blocked) = [&envrc_file, &dotenv_file]
        .into_iter()
        .flatten()
        .find(|file| !is_allowed_in(allow_dir, file))
    {
        let dir = canonicalize_fallback(&found.dir);
        eprintln!(
            "denv: {} is blocked. Run `denv allow` to trust it.",
            blocked.path.display()
        );
        set_change(
            &mut changes,
            "__DENV_DIR",
            ShellBytes::from_os_str(dir.as_os_str()),
        );
        set_change(&mut changes, "__DENV_DIRTY", "1".to_string());
        unset_change(&mut changes, "__DENV_STATE");
        return Ok(Transition { changes, record: RecordUpdate::Clear });
    }

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
                    ShellBytes::from_os_str(dir.as_os_str()),
                );
                set_change(&mut changes, "__DENV_DIRTY", "1".to_string());
                unset_change(&mut changes, "__DENV_STATE");
                return Ok(Transition { changes, record: RecordUpdate::Clear });
            }
        }
    } else {
        diff_dotenv_only(&dotenv_entries, &env_pairs)
    };

    let prev = capture_prev(&diff, &working);
    let dir = canonicalize_fallback(&found.dir);
    let state = ActiveState { dir: dir.clone(), envrc_mtime, dotenv_mtime, prev };
    apply_diff(&diff, &mut changes);
    set_change(
        &mut changes,
        "__DENV_DIR",
        ShellBytes::from_os_str(dir.as_os_str()),
    );
    unset_change(&mut changes, "__DENV_DIRTY");
    let mut state_value = format!("{envrc_mtime} {dotenv_mtime} ").into_bytes();
    state_value.extend_from_slice(found.dir.as_os_str().as_bytes());
    set_change(&mut changes, "__DENV_STATE", ShellBytes::from_vec(state_value));
    Ok(Transition { changes, record: RecordUpdate::Save(state) })
}

fn allow_current_dir(session: &Session, epsh: &mut epsh::eval::Shell) -> Result<Vec<EnvChange>, String> {
    let cwd = current_dir_from_store(epsh)?;
    let found = find_env_files(&cwd).ok_or("no .envrc or .env found")?;
    for (path, _) in found.envrc.iter().chain(found.dotenv.iter()) {
        let file = read_trusted_candidate(path)?;
        allow_file_in(&session.data_dir()?.join("allow"), &file)?;
        eprintln!("denv: allowed {}", file.path.display());
    }
    refresh(session, true, epsh)
}

fn deny_current_dir(session: &Session, epsh: &mut epsh::eval::Shell) -> Result<Vec<EnvChange>, String> {
    let cwd = current_dir_from_store(epsh)?;
    let found = find_env_files(&cwd).ok_or("no .envrc or .env found")?;
    for (path, _) in found.envrc.iter().chain(found.dotenv.iter()) {
        deny_file(session.data_dir()?, &canonicalize_fallback(path))?;
    }
    refresh(session, true, epsh)
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
    probe.clear();
    probe.push(dir);
    probe.push(name);
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

fn state_var_matches(found: &EnvFiles, variables: &Variables) -> bool {
    let envrc_mtime = found.envrc.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let dotenv_mtime = found.dotenv.as_ref().map(|(_, m)| *m).unwrap_or(0);
    let Some(state) = variables.get("__DENV_STATE") else {
        return false;
    };
    let Some((st_envrc, st_dotenv, st_dir)) = parse_denv_state(state) else {
        return false;
    };
    st_envrc == envrc_mtime
        && st_dotenv == dotenv_mtime
        && (found.dir.to_str().is_some_and(|directory| st_dir == directory)
            || found
                .dir
                .canonicalize()
                .is_ok_and(|dir| dir.to_str().is_some_and(|directory| st_dir == directory)))
}

fn trust_key(path: &Path) -> String {
    format!("sha256-{}", sha256::hex_digest(path.as_os_str().as_bytes()))
}

fn legacy_trust_key(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut key = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    key
}

fn data_dir(epsh: &epsh::eval::Shell) -> Result<PathBuf, String> {
    let dir = if let Some(path) = epsh.vars().get_bytes("DENV_DATA_DIR") {
        PathBuf::from(path.to_os_string())
    } else {
        let home = epsh.vars().get_bytes("HOME").ok_or("HOME and DENV_DATA_DIR are unset")?;
        PathBuf::from(home.to_os_string()).join(".local/share/denv")
    };
    Ok(current_dir_from_store(epsh)?.join(dir))
}

fn active_path(data_dir: &Path, record_id: &str) -> PathBuf {
    data_dir.join(format!("active_{record_id}"))
}

fn stat_regular_file_mtime(path: &Path) -> Option<u64> {
    let st = rustix::fs::stat(path).ok()?;
    (rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::RegularFile)
        .then_some(st.st_mtime as u64)
}

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

fn is_allowed_in(allow_dir: &Path, file: &TrustCandidate) -> bool {
    let stored = match fs::read_to_string(allow_dir.join(trust_key(&file.path))) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::read_to_string(allow_dir.join(legacy_trust_key(&file.path)))
        }
        result => result,
    };
    stored.is_ok_and(|stored| stored.trim() == file.hash)
}

fn allow_file_in(dir: &Path, file: &TrustCandidate) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create allow dir: {e}"))?;
    fs::write(dir.join(trust_key(&file.path)), &file.hash)
        .map_err(|e| format!("failed to write trust file: {e}"))
}

fn deny_file(data_dir: &Path, path: &Path) -> Result<(), String> {
    let allow = data_dir.join("allow");
    let mut removed = false;
    for (key, legacy) in [(legacy_trust_key(path), true), (trust_key(path), false)] {
        match fs::remove_file(allow.join(key)) {
            Ok(()) => removed = true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err)
                if legacy
                    && err.raw_os_error()
                        == Some(rustix::io::Errno::NAMETOOLONG.raw_os_error()) => {}
            Err(err) => return Err(format!("failed to remove trust file: {err}")),
        }
    }
    if removed {
        eprintln!("denv: denied {}", path.display());
    } else {
        eprintln!("denv: {} not currently allowed", path.display());
    }
    Ok(())
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

fn load_active(data_dir: &Path, record_id: &str) -> Result<Option<ActiveState>, ActiveLoadError> {
    let path = active_path(data_dir, record_id);
    let content = match fs::read(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ActiveLoadError::Unreadable(format!(
                "failed to read active file {}: {err}",
                path.display()
            )));
        }
    };
    let malformed =
        |err: &dyn std::fmt::Display| ActiveLoadError::Malformed(format!("invalid active file {}: {err}", path.display()));
    let content = std::str::from_utf8(&content).map_err(|err| malformed(&err))?;
    parse_active(content).map(Some).map_err(|err| malformed(&err))
}

fn parse_active(content: &str) -> Result<ActiveState, String> {
    if content.starts_with("DENV-ACTIVE ") {
        let mut lines = content.split_terminator('\n');
        let header = lines.next().ok_or("missing active format header")?;
        if header != ACTIVE_FORMAT_HEADER && header != "DENV-ACTIVE 1" {
            return Err(format!("unsupported active format {header:?}"));
        }
        let path_hex = lines.next().ok_or("missing active project path")?;
        let dir_bytes = decode_hex(path_hex)?;
        if dir_bytes.is_empty() || dir_bytes.contains(&0) {
            return Err("invalid active project path".to_string());
        }
        let dir = PathBuf::from(std::ffi::OsStr::from_bytes(&dir_bytes));
        validate_active_dir(&dir)?;
        let mtimes = lines.next().ok_or("missing active timestamps")?;
        let (envrc_mtime, dotenv_mtime) = parse_active_mtimes(mtimes)?;
        let prev = if header == ACTIVE_FORMAT_HEADER { parse_active_bindings(lines)? } else { parse_active_vars(lines, true)? };
        return Ok(ActiveState { dir, envrc_mtime, dotenv_mtime, prev });
    }

    parse_legacy_active(content)
}

fn write_hex(output: &mut String, bytes: &[u8]) {
    for byte in bytes { write!(output, "{byte:02x}").expect("writing to a String cannot fail"); }
}

fn parse_active_bindings<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Vec<PrevVar>, String> {
    lines.map(|line| {
        let (kind, rest) = line.split_once(' ').ok_or("invalid active binding")?;
        let (name, value) = if matches!(kind, "U" | "X" | "N") { (rest, None) } else {
            let (name, value) = rest.split_once(' ').ok_or("invalid active binding")?;
            (name, Some(value))
        };
        let name = String::from_utf8(decode_hex(name)?).map_err(|_| "invalid active variable name")?;
        validate_active_variable(&name)?;
        match (kind, value) {
            ("U", None) => Ok(PrevVar::Unset(name)),
            ("X" | "N", None) => Ok(PrevVar::Unassigned(name, kind == "X")),
            ("E" | "L", Some(value)) => {
                let value = decode_hex(value)?;
                if value.contains(&0) { return Err("invalid active variable value".into()); }
                Ok(PrevVar::Restore(name, ShellBytes::from_vec(value), kind == "E"))
            }
            _ => Err("invalid active binding kind".into()),
        }
    }).collect()
}

fn parse_legacy_active(content: &str) -> Result<ActiveState, String> {
    let mut lines = content.lines();
    let dir = PathBuf::from(lines.next().ok_or("missing legacy project path")?);
    validate_active_dir(&dir)?;
    let mtimes = lines.next().ok_or("missing legacy timestamps")?;
    let (envrc_mtime, dotenv_mtime) = parse_active_mtimes(mtimes)?;
    let prev = parse_active_vars(lines, false)?;
    Ok(ActiveState { dir, envrc_mtime, dotenv_mtime, prev })
}

fn validate_active_dir(dir: &Path) -> Result<(), String> {
    if !dir.is_absolute() || dir.as_os_str().as_bytes().contains(&0) {
        return Err("invalid active project path".to_string());
    }
    Ok(())
}

fn parse_active_mtimes(mtimes: &str) -> Result<(u64, u64), String> {
    let (envrc_mtime, dotenv_mtime) = mtimes
        .split_once(' ')
        .ok_or("invalid active timestamps")?;
    if envrc_mtime.is_empty() || dotenv_mtime.is_empty() || dotenv_mtime.contains(' ') {
        return Err("invalid active timestamps".to_string());
    }
    let envrc_mtime = envrc_mtime.parse().map_err(|_| "invalid envrc timestamp")?;
    let dotenv_mtime = dotenv_mtime.parse().map_err(|_| "invalid dotenv timestamp")?;
    Ok((envrc_mtime, dotenv_mtime))
}

fn parse_active_vars<'a>(
    lines: impl Iterator<Item = &'a str>,
    encoded_keys: bool,
) -> Result<Vec<PrevVar>, String> {
    let mut prev = Vec::new();
    for line in lines {
        if let Some(eq) = line.find('=') {
            let key = if encoded_keys {
                unescape_newlines(&line[..eq]).into_owned()
            } else {
                line[..eq].to_string()
            };
            validate_active_variable(&key)?;
            let value = unescape_newlines(&line[eq + 1..]).into_owned();
            if value.contains('\0') {
                return Err("invalid active variable value".to_string());
            }
            prev.push(PrevVar::Restore(
                key,
                ShellBytes::from(value),
                true,
            ));
        } else if !line.is_empty() {
            let key = if encoded_keys {
                unescape_newlines(line).into_owned()
            } else {
                line.to_string()
            };
            validate_active_variable(&key)?;
            prev.push(PrevVar::Unset(key));
        } else {
            return Err("empty active variable record".to_string());
        }
    }
    Ok(prev)
}

fn validate_active_variable(key: &str) -> Result<(), String> {
    if key.is_empty() || key.bytes().any(|byte| byte == b'=' || byte == 0) {
        return Err("invalid active variable name".to_string());
    }
    Ok(())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err("invalid project path encoding".to_string());
    }
    pairs
        .iter()
        .map(|pair| {
            let high = hex_digit(pair[0]).ok_or("invalid project path encoding")?;
            let low = hex_digit(pair[1]).ok_or("invalid project path encoding")?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn save_active(dir: &Path, record_id: &str, state: &ActiveState) -> Result<(), String> {
    validate_active_dir(&state.dir).map_err(|err| format!("failed to write active file: {err}"))?;
    for previous in &state.prev {
        match previous {
            PrevVar::Restore(key, value, _) => {
                validate_active_variable(key)
                    .map_err(|err| format!("failed to write active file: {err}"))?;
                if value.as_bytes().contains(&0) {
                    return Err("failed to write active file: invalid active variable value".to_string());
                }
            }
            PrevVar::Unset(key) | PrevVar::Unassigned(key, _) => validate_active_variable(key)
                .map_err(|err| format!("failed to write active file: {err}"))?,
        }
    }
    fs::create_dir_all(dir).map_err(|e| format!("failed to create data dir: {e}"))?;
    let mut buf = String::new();
    buf.push_str(ACTIVE_FORMAT_HEADER);
    buf.push('\n');
    for byte in state.dir.as_os_str().as_bytes() {
        write!(buf, "{byte:02x}").unwrap();
    }
    buf.push('\n');
    write!(buf, "{} {}", state.envrc_mtime, state.dotenv_mtime).unwrap();
    buf.push('\n');
    for previous in &state.prev {
        match previous {
            PrevVar::Restore(name, value, exported) => {
                buf.push(if *exported { 'E' } else { 'L' });
                buf.push(' ');
                write_hex(&mut buf, name.as_bytes());
                buf.push(' ');
                write_hex(&mut buf, value.as_bytes());
                buf.push('\n');
            }
            PrevVar::Unassigned(name, exported) => {
                buf.push_str(if *exported { "X " } else { "N " });
                write_hex(&mut buf, name.as_bytes());
                buf.push('\n');
            }
            PrevVar::Unset(name) => {
                buf.push_str("U ");
                write_hex(&mut buf, name.as_bytes());
                buf.push('\n');
            }
        }
    }
    let path = active_path(dir, record_id);
    let mut reserved = None;
    for _ in 0..ACTIVE_TEMP_NAME_ATTEMPTS {
        let id = NEXT_ACTIVE_TEMP_ID.fetch_add(1, AtomicOrdering::Relaxed);
        let temp_path = dir.join(format!(".active_{record_id}.{id}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
        {
            Ok(file) => {
                reserved = Some((temp_path, file));
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("failed to create active file: {err}")),
        }
    }
    let (temp_path, mut file) = reserved.ok_or_else(|| {
        "failed to create active file: temporary names are already occupied".to_string()
    })?;
    let result = file
        .write_all(buf.as_bytes())
        .map_err(|e| format!("failed to write active file: {e}"))
        .and_then(|()| {
            fs::rename(&temp_path, &path)
                .map_err(|e| format!("failed to replace active file: {e}"))
        });
    if let Err(err) = result {
        return match fs::remove_file(&temp_path) {
            Ok(()) => Err(err),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(err),
            Err(cleanup) => Err(format!(
                "{err}; failed to remove temporary active file: {cleanup}"
            )),
        };
    }
    Ok(())
}

fn clear_active(data_dir: &Path, record_id: &str) -> Result<(), String> {
    let path = active_path(data_dir, record_id);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("failed to remove active file: {err}")),
    }
}

struct RecordOwner {
    pid: rustix::process::Pid,
    pid_namespace: Option<u64>,
}

fn record_owner(name: &str) -> Option<RecordOwner> {
    let (rest, temporary) = match name.strip_prefix(".active_") {
        Some(rest) => (rest.strip_suffix(".tmp")?, true),
        None => (name.strip_prefix("active_")?, false),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (pid, suffix) = rest.split_at(digits);
    let suffix_ok = match suffix.bytes().next() {
        None => !temporary,
        Some(b'-') => true,
        Some(b'.') => temporary,
        Some(_) => false,
    };
    if !suffix_ok {
        return None;
    }
    let pid = rustix::process::Pid::from_raw(pid.parse().ok()?)?;
    let pid_namespace = suffix.strip_prefix('-').and_then(|rest| {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (namespace, after) = rest.split_at(digits);
        after.starts_with('-').then(|| namespace.parse().ok()).flatten()
    });
    Some(RecordOwner { pid, pid_namespace })
}

fn shell_is_alive(pid: rustix::process::Pid) -> bool {
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

fn remove_dead_shell_records(
    data_dir: &Path,
    own_pid_namespace: Option<u64>,
) -> Result<(), String> {
    let entries = match fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("failed to list active records: {err}")),
    };
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to list active records: {err}"))?;
        let Some(owner) = entry.file_name().to_str().and_then(record_owner) else {
            continue;
        };
        let foreign = matches!(
            (owner.pid_namespace, own_pid_namespace),
            (Some(theirs), Some(ours)) if theirs != ours
        );
        if foreign || shell_is_alive(owner.pid) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "failed to remove stale active record {}: {err}",
                    entry.path().display()
                ));
            }
        }
    }
    Ok(())
}

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

fn diff_dotenv_only(dotenv_entries: &[(String, String)], environment: &[(OsString, OsString)]) -> EnvDiff {
    let mut set = Vec::new();
    for (key, value) in dotenv_entries {
        if environment.iter().find(|(name, _)| name == OsStr::new(key)).is_none_or(|(_, current)| current != OsStr::new(value)) {
            set.push((key.clone(), ShellBytes::from(value.clone())));
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

fn parse_env_null(data: &[u8]) -> Vec<(String, ShellBytes)> {
    let entries: Vec<_> = data.split(|byte| *byte == 0).filter_map(|entry| {
        let separator = entry.iter().position(|byte| *byte == b'=')?;
        Some((OsStr::from_bytes(&entry[..separator]).to_owned(), OsStr::from_bytes(&entry[separator + 1..]).to_owned()))
    }).collect();
    filtered_env_from_snapshot(&entries)
}

fn diff_sorted_env(before: &[(String, ShellBytes)], after: &[(String, ShellBytes)]) -> EnvDiff {
    let (mut before_idx, mut after_idx) = (0, 0);
    let mut set = Vec::new();
    let mut unset = Vec::new();
    while before_idx < before.len() && after_idx < after.len() {
        match before[before_idx].0.cmp(&after[after_idx].0) {
            Ordering::Less => {
                unset.push(before[before_idx].0.to_string());
                before_idx += 1;
            }
            Ordering::Greater => {
                set.push((
                    after[after_idx].0.to_string(),
                    after[after_idx].1.clone(),
                ));
                after_idx += 1;
            }
            Ordering::Equal => {
                if before[before_idx].1 != after[after_idx].1 {
                    set.push((
                        after[after_idx].0.to_string(),
                        after[after_idx].1.clone(),
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
        set.push(((*key).to_string(), value.clone()));
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

fn filtered_env_from_snapshot(entries: &[(OsString, OsString)]) -> Vec<(String, ShellBytes)> {
    let mut filtered: Vec<_> = entries.iter().filter_map(|(key, value)| {
        let key = key.to_str()?;
        (is_valid_env_key(key) && !is_ignored_env_key(key)).then(|| (key.to_string(), ShellBytes::from_os_str(value)))
    }).collect();
    filtered.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    filtered
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
        return Ok(diff_dotenv_only(dotenv_entries, env_pairs));
    };
    match envrc_interpreter(source) {
        EnvRcInterpreter::Bash => eval_env_bash(
            dir,
            source,
            dotenv_entries,
            env_pairs,
            ENVRC_EVAL_TIMEOUT,
        ),
        EnvRcInterpreter::Epsh => {
            eval_env_epsh(dir, source, dotenv_entries, env_pairs, ENVRC_EVAL_TIMEOUT)
        }
    }
}

fn eval_env_bash(
    dir: &Path,
    source: &[u8],
    dotenv_entries: &[(String, String)],
    env_pairs: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<EnvDiff, String> {
    if source.contains(&0) {
        return Err(".envrc contains a NUL byte".to_string());
    }
    let failure = FailureChannel::new().map_err(|error| error.to_string())?;
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
    let (before_src, after_src) = duplicate_snapshot_writers(&before_w, &after_w)?;
    rustix::fs::fcntl_setfl(&before_r, rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| format!("set before pipe nonblocking: {e}"))?;
    rustix::fs::fcntl_setfl(&after_r, rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| format!("set after pipe nonblocking: {e}"))?;

    let mut script = String::with_capacity(BASH_STDLIB.len() + 256);
    script.push_str(BASH_STDLIB);
    script.push('\n');
    failure.install(&mut script);
    script.push_str("__denv_src=$");
    script.push_str(ENVRC_SOURCE_VAR);
    script.push_str("\nunset ");
    script.push_str(ENVRC_SOURCE_VAR);
    script.push_str("\nenv -0 >&3\neval \"$__denv_src\"\nunset __denv_src\n");
    script.push_str("env -0 >&4\n");
    writeln!(script, "printf '%s' Z >&{}", failure.reporters()[0]).unwrap();

    let stderr_dup = io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup stderr: {e}"))?;
    let mut before_read = fs::File::from(before_r);
    let mut after_read = fs::File::from(after_r);

    let mut cmd = Command::new("bash");
    cmd.process_group(0);
    cmd.env_clear();
    for (key, value) in env_pairs {
        cmd.env(key, value);
    }
    cmd.env(ENVRC_SOURCE_VAR, OsStr::from_bytes(source));
    let FailureChannel { read: failure_read, write: failure_write, backup: failure_backup } = failure;
    // SAFETY: the hook only resets signals, remaps owned snapshot descriptors and clears close-on-exec on owned reporters.
    unsafe {
        cmd.pre_exec(move || {
            crate::signal::restore_defaults();
            remap_snapshot_writers(&before_src, &after_src)?;
            rustix::io::fcntl_setfd(&failure_write, rustix::io::FdFlags::empty())?;
            rustix::io::fcntl_setfd(&failure_backup, rustix::io::FdFlags::empty())?;
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
    let bash_pid = rustix::process::Pid::from_child(&child);
    let process_group = bash_pid;
    let mut leader_exited = false;
    let mut before_open = true;
    let mut after_open = true;
    let mut failure_open = true;
    let mut completion = Completion::Pending;
    let mut before_data = Vec::new();
    let mut after_data = Vec::new();
    while !leader_exited || before_open || after_open || failure_open {
        match FailureChannel::read_state(&failure_read, &mut completion) {
            Ok(ChannelState::Complete) => failure_open = false,
            Ok(ChannelState::Open) => {}
            Err(error) => {
                let cleanup = stop_bash_group(&mut child, process_group, leader_exited);
                return Err(append_cleanup_error(error.to_string(), cleanup));
            }
        }
        if Instant::now() >= deadline {
            let cleanup = stop_bash_group(&mut child, process_group, leader_exited);
            let message = format!(
                ".envrc evaluation timed out after {}s",
                timeout.as_secs_f32()
            );
            return Err(append_cleanup_error(message, cleanup));
        }

        if !leader_exited {
            match rustix::process::waitid(
                rustix::process::WaitId::Pid(bash_pid),
                rustix::process::WaitIdOptions::EXITED
                    | rustix::process::WaitIdOptions::NOHANG
                    | rustix::process::WaitIdOptions::NOWAIT,
            ) {
                Ok(Some(_)) => leader_exited = true,
                Ok(None) => {}
                Err(err) if err == rustix::io::Errno::INTR => continue,
                Err(err) => {
                    let cleanup = stop_bash_group(&mut child, process_group, false);
                    return Err(append_cleanup_error(format!("wait for bash: {err}"), cleanup));
                }
            }
        }

        let mut readers = Vec::with_capacity(3);
        if before_open {
            readers.push(rustix::event::PollFd::from_borrowed_fd(
                before_read.as_fd(),
                rustix::event::PollFlags::IN,
            ));
        }
        if after_open {
            readers.push(rustix::event::PollFd::from_borrowed_fd(
                after_read.as_fd(),
                rustix::event::PollFlags::IN,
            ));
        }
        if failure_open {
            readers.push(rustix::event::PollFd::from_borrowed_fd(
                failure_read.as_fd(),
                rustix::event::PollFlags::IN,
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let poll_for = remaining.min(Duration::from_millis(10));
        let poll_timeout = rustix::event::Timespec {
            tv_sec: poll_for.as_secs() as i64,
            tv_nsec: poll_for.subsec_nanos() as i64,
        };
        match rustix::event::poll(&mut readers, Some(&poll_timeout)) {
            Err(err) if err == rustix::io::Errno::INTR => continue,
            Err(err) => {
                let cleanup = stop_bash_group(&mut child, process_group, leader_exited);
                return Err(append_cleanup_error(
                    format!("poll env snapshot pipes: {err}"),
                    cleanup,
                ));
            }
            Ok(_) => {}
        }

        let ready = rustix::event::PollFlags::IN
            | rustix::event::PollFlags::HUP
            | rustix::event::PollFlags::ERR;
        let mut reader_index = 0;
        let mut ready_to_drain = Vec::with_capacity(2);
        if before_open {
            ready_to_drain.push(readers[reader_index].revents().intersects(ready));
            reader_index += 1;
        }
        if after_open {
            ready_to_drain.push(readers[reader_index].revents().intersects(ready));
        }
        drop(readers);
        let mut ready_index = 0;
        if before_open {
            if ready_to_drain[ready_index] {
                match drain_env_pipe(&mut before_read, &mut before_data, "before") {
                    Ok(open) => before_open = open,
                    Err(err) => {
                        let cleanup = stop_bash_group(&mut child, process_group, leader_exited);
                        return Err(append_cleanup_error(err, cleanup));
                    }
                }
            }
            ready_index += 1;
        }
        if after_open && ready_to_drain[ready_index] {
            match drain_env_pipe(&mut after_read, &mut after_data, "after") {
                Ok(open) => after_open = open,
                Err(err) => {
                    let cleanup = stop_bash_group(&mut child, process_group, leader_exited);
                    return Err(append_cleanup_error(err, cleanup));
                }
            }
        }
    }

    let status = child
        .wait()
        .map_err(|err| format!("reap bash: {err}"))?;

    if !status.success() {
        return Err(".envrc evaluation failed".to_string());
    }

    let before = parse_env_null(&before_data);
    let mut after = parse_env_null(&after_data);
    for (key, value) in dotenv_entries {
        match after.binary_search_by(|(name, _)| name.as_str().cmp(key)) {
            Ok(index) => after[index].1 = ShellBytes::from(value.clone()),
            Err(index) => after.insert(index, (key.clone(), ShellBytes::from(value.clone()))),
        }
    }
    Ok(diff_sorted_env(&before, &after))
}

fn duplicate_snapshot_writers(
    before_w: &OwnedFd,
    after_w: &OwnedFd,
) -> Result<(OwnedFd, OwnedFd), String> {
    let before_src = rustix::io::fcntl_dupfd_cloexec(before_w, SNAPSHOT_SOURCE_FD_MIN)
        .map_err(|e| format!("dup before pipe: {e}"))?;
    let after_src = rustix::io::fcntl_dupfd_cloexec(after_w, SNAPSHOT_SOURCE_FD_MIN)
        .map_err(|e| format!("dup after pipe: {e}"))?;
    Ok((before_src, after_src))
}

fn remap_snapshot_writers(
    before_src: &OwnedFd,
    after_src: &OwnedFd,
) -> io::Result<()> {
    dup_snapshot_writer_to(before_src, BEFORE_SNAPSHOT_FD)?;
    dup_snapshot_writer_to(after_src, AFTER_SNAPSHOT_FD)?;
    Ok(())
}

fn dup_snapshot_writer_to(source: &OwnedFd, target: RawFd) -> io::Result<()> {
    // SAFETY: dup2 accepts a closed target and replaces it with the owned source descriptor.
    let result = unsafe { libc::dup2(source.as_raw_fd(), target) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn drain_env_pipe(file: &mut fs::File, data: &mut Vec<u8>, label: &str) -> Result<bool, String> {
    let mut buffer = [0; 8192];
    match file.read(&mut buffer) {
        Ok(0) => Ok(false),
        Ok(count) => {
            data.extend_from_slice(&buffer[..count]);
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(true),
        Err(err) => Err(format!("read {label} env: {err}")),
    }
}

fn stop_bash_group(
    child: &mut Child,
    process_group: rustix::process::Pid,
    leader_exited: bool,
) -> Result<(), String> {
    let group_result = match rustix::process::kill_process_group(
        process_group,
        rustix::process::Signal::KILL,
    ) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(err) => Err(format!("kill .envrc process group: {err}")),
    };
    let kill_result = if leader_exited {
        Ok(())
    } else {
        match child.kill() {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!("kill bash: {err}")),
        }
    };
    let reap_result = child
        .wait()
        .map(|_| ())
        .map_err(|err| format!("reap bash: {err}"));
    let wait_result = match (kill_result, reap_result) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(kill), Err(reap)) => Err(format!("{kill}; {reap}")),
    };
    match (group_result, wait_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(group), Ok(())) | (Ok(()), Err(group)) => Err(group),
        (Err(group), Err(wait)) => Err(format!("{group}; {wait}")),
    }
}

fn append_cleanup_error(primary: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => format!("{primary}; cleanup failed: {cleanup}"),
    }
}

fn eval_env_epsh(
    dir: &Path,
    source: &[u8],
    dotenv_entries: &[(String, String)],
    env_pairs: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<EnvDiff, String> {
    let source = std::str::from_utf8(source).map_err(|e| format!(".envrc is not UTF-8: {e}"))?;
    let failure = FailureChannel::new().map_err(|error| error.to_string())?;
    let sink = std::sync::Arc::new(std::sync::Mutex::new(SharedStderr));
    let deadline = Instant::now() + timeout;
    let mut shell = epsh::eval::Shell::builder()
        .cwd(dir.to_path_buf())
        .environment(env_pairs.to_vec())
        .stdout_sink(sink.clone())
        .stderr_sink(sink)
        .timeout(timeout)
        .build();

    let mut script = String::with_capacity(BASH_STDLIB.len() + 256);
    script.push_str(BASH_STDLIB);
    script.push('\n');
    failure.install(&mut script);
    script.push_str(source);
    script.push('\n');

    let status = shell.run_script(&script);
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
    let after_process = shell.vars().env_for_command_os(&[]);
    drop(shell);
    failure.finish().map_err(|error| error.to_string())?;
    if let Some(err) = dotenv_failed {
        return Err(err);
    }

    if status != 0 {
        if Instant::now() >= deadline {
            return Err(format!(
                ".envrc evaluation timed out after {}s",
                timeout.as_secs_f32()
            ));
        }
        return Err(".envrc evaluation failed".to_string());
    }

    let before = filtered_env_from_snapshot(env_pairs);
    let after = filtered_env_from_snapshot(&after_process);
    Ok(diff_sorted_env(&before, &after))
}

fn capture_prev(diff: &EnvDiff, variables: &Variables) -> Vec<PrevVar> {
    diff.set.iter().map(|(name, _)| name).chain(diff.unset.iter()).map(|name| {
        match variables.binding(name) {
            Some(binding) => match &binding.value {
                Some(value) => PrevVar::Restore(name.clone(), value.clone(), binding.flags.has(VarFlags::EXPORT)),
                None => PrevVar::Unassigned(name.clone(), binding.flags.has(VarFlags::EXPORT)),
            },
            None => PrevVar::Unset(name.clone()),
        }
    }).collect()
}

fn apply_restore(prev: &[PrevVar], changes: &mut Vec<EnvChange>) {
    for item in prev {
        match item {
            PrevVar::Restore(key, value, exported) => {
                set_change(changes, key, value.clone());
                if !exported { changes.push(EnvChange::Unexport(key.clone())); }
            },
            PrevVar::Unset(key) => unset_change(changes, key),
            PrevVar::Unassigned(key, exported) => { unset_change(changes, key); changes.push(EnvChange::Declare(key.clone(), *exported)); },
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

fn set_change(changes: &mut Vec<EnvChange>, key: &str, value: impl Into<ShellBytes>) {
    changes.push(EnvChange::Set(key.to_string(), value.into()));
}

fn unset_change(changes: &mut Vec<EnvChange>, key: &str) {
    changes.push(EnvChange::Unset(key.to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    static EVALUATION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join(format!("{prefix}_{}_{}", std::process::id(), unique));
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
        assert_eq!(parsed, vec![("KEEP".to_string(), ShellBytes::from("yes"))]);
    }

    #[test]
    fn diff_sorted_env_reports_sets_and_unsets() {
        let before = [("A", "1"), ("B", "2"), ("C", "3")].map(|(name, value)| (name.to_string(), ShellBytes::from(value)));
        let after = [("A", "1"), ("B", "changed"), ("D", "4")].map(|(name, value)| (name.to_string(), ShellBytes::from(value)));
        let diff = diff_sorted_env(&before, &after);
        assert_eq!(
            diff.set,
            vec![
                ("B".to_string(), ShellBytes::from("changed")),
                ("D".to_string(), ShellBytes::from("4"))
            ]
        );
        assert_eq!(diff.unset, vec!["C".to_string()]);
    }

    #[test]
    fn legacy_escape_reader_preserves_newlines_and_backslashes() {
        let original = "line1\nline2\\tail";
        let escaped = "line1\\nline2\\\\tail";
        let restored = unescape_newlines(escaped);
        assert_eq!(restored.as_ref(), original);
    }

    #[test]
    fn active_state_restores_variables_for_newline_project_path() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_newline_project");
        let data = tmp.path().join("data");
        let project = tmp.path().join("project\nbreak");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let mut epsh = epsh::eval::Shell::builder().env_clear().build();
        for (key, value) in [
            ("DENV_DATA_DIR", data.to_str().unwrap()),
            ("PWD", outside.to_str().unwrap()),
            ("DENV_NEWLINE_RESTORE", "project"),
        ] {
            epsh.vars_mut().set(key, value).unwrap();
            epsh.vars_mut().export(key);
        }
        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);
        save_active(
            &data,
            session.record_id().unwrap(),
            &ActiveState {
                dir: project,
                envrc_mtime: 1,
                dotenv_mtime: 2,
                prev: vec![PrevVar::Restore("DENV_NEWLINE_RESTORE".into(), "original".into(), true)],
            },
        )
        .unwrap();

        let changes = refresh(&session, false, &mut epsh).unwrap();

        assert!(changes.iter().any(|change| matches!(
            change,
            EnvChange::Set(key, value) if key == "DENV_NEWLINE_RESTORE" && value == "original"
        )));
        assert_eq!(epsh.get_var("DENV_NEWLINE_RESTORE"), Some("original"));
    }

    #[test]
    fn active_state_roundtrip_preserves_non_utf8_project_path() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_non_utf8_project");
        let data = tmp.path().join("data");
        let path = PathBuf::from(OsString::from_vec(b"/project/\xff".to_vec()));
        let state = ActiveState {
            dir: path.clone(),
            envrc_mtime: 3,
            dotenv_mtime: 4,
            prev: vec![PrevVar::Unset("DENV_TEST".into())],
        };

        save_active(&data, "test", &state).unwrap();
        let saved = std::fs::read_to_string(active_path(&data, "test")).unwrap();
        assert!(saved.starts_with("DENV-ACTIVE 2\n"));

        let restored = load_active(&data, "test").unwrap().unwrap();
        assert_eq!(restored.dir.as_os_str().as_bytes(), path.as_os_str().as_bytes());
        assert_eq!(restored.envrc_mtime, 3);
        assert_eq!(restored.dotenv_mtime, 4);
    }

    #[test]
    fn active_state_reads_legacy_records_and_rejects_malformed_or_unknown_formats() {
        let tmp = TempDir::new("ish_denv_legacy_active");
        std::fs::write(
            active_path(tmp.path(), "legacy"),
            "/project\n1 2\nA=value\\nnext\nB\n",
        )
        .unwrap();
        let legacy = load_active(tmp.path(), "legacy").unwrap().unwrap();
        assert!(matches!(
            legacy,
            ActiveState { dir, envrc_mtime: 1, dotenv_mtime: 2, prev }
                if dir == Path::new("/project")
                    && matches!(prev.as_slice(), [PrevVar::Restore(key, value, _), PrevVar::Unset(unset)]
                        if key == "A" && value == "value\nnext" && unset == "B")
        ));
        let version_one = parse_active("DENV-ACTIVE 1\n2f70726f6a656374\n1 2\nA=value\\nnext\nB\n").unwrap();
        assert!(matches!(version_one.prev.as_slice(), [PrevVar::Restore(name, value, true), PrevVar::Unset(unset)] if name == "A" && value == "value\nnext" && unset == "B"));
        let unknown = match parse_active("DENV-ACTIVE 99\n00\n1 2\n") {
            Ok(_) => panic!("expected unknown active format to fail"),
            Err(err) => err,
        };
        assert!(unknown.contains("unsupported"));
        assert!(parse_active("DENV-ACTIVE 1\n0g\n1 2\n").is_err());
        assert!(parse_active("DENV-ACTIVE 1\n2f\n1 2\n=bad\n").is_err());
        assert!(parse_active("DENV-ACTIVE 1\n72656c6174697665\n1 2\n").is_err());
        assert!(parse_active("DENV-ACTIVE 1\n2f00\n1 2\n").is_err());
        assert!(parse_active("DENV-ACTIVE 1\n2f\n1 2\nA=bad\0value\n").is_err());
        assert!(load_active(tmp.path(), "missing").unwrap().is_none());
    }

    #[test]
    fn malformed_active_state_fails_before_process_environment_changes() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_malformed_active");
        let data = tmp.path().join("data");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let mut epsh = epsh::eval::Shell::builder().env_clear().build();
        for (key, value) in [
            ("DENV_DATA_DIR", data.to_str().unwrap()),
            ("PWD", cwd.to_str().unwrap()),
            ("DENV_PROCESS_ENV_SENTINEL", "shell-value"),
        ] {
            epsh.vars_mut().set(key, value).unwrap();
            epsh.vars_mut().export(key);
        }
        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);
        std::fs::write(
            active_path(&data, session.record_id().unwrap()),
            b"DENV-ACTIVE 99\n",
        )
        .unwrap();

        let err = match refresh(&session, false, &mut epsh) {
            Ok(_) => panic!("expected malformed active format to fail"),
            Err(err) => err,
        };

        assert!(err.contains("unsupported active format"), "{err}");
        assert_eq!(epsh.get_var("DENV_PROCESS_ENV_SENTINEL"), Some("shell-value"));
    }

    fn active_record_names(data: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(data) else {
            return Vec::new();
        };
        entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("active_"))
            .collect()
    }

    fn denv_test_shell(data: &Path, cwd: &Path, vars: &[(&str, &str)]) -> epsh::eval::Shell {
        let mut epsh = epsh::eval::Shell::builder().env_clear().cwd(cwd.to_path_buf()).build();
        let data_dir = data.to_str().unwrap();
        let cwd = cwd.to_str().unwrap();
        for (key, value) in [("DENV_DATA_DIR", data_dir), ("PWD", cwd)]
            .into_iter()
            .chain(vars.iter().copied())
        {
            epsh.vars_mut().set(key, value).unwrap();
            epsh.vars_mut().export(key);
        }
        epsh
    }

    #[test]
    fn stale_record_from_earlier_shell_with_same_pid_is_not_applied() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_stale_record");
        let data = tmp.path().join("data");
        let project = tmp.path().join("project");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let mut epsh = denv_test_shell(&data, &outside, &[("DENV_STALE_TEST", "current")]);

        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let earlier = init(&mut epsh);
        let earlier_id = epsh.vars().get("__DENV_PID").unwrap().to_string();
        save_active(
            &data,
            &earlier_id,
            &ActiveState {
                dir: canonicalize_fallback(&project),
                envrc_mtime: 1,
                dotenv_mtime: 2,
                prev: vec![PrevVar::Restore("DENV_STALE_TEST".into(), "stale".into(), true)],
            },
        )
        .unwrap();
        std::mem::forget(earlier);

        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let later = init(&mut epsh);
        let changes = refresh(&later, false, &mut epsh).unwrap();

        assert!(!changes.iter().any(|change| matches!(
            change,
            EnvChange::Set(key, _) | EnvChange::Unset(key) if key == "DENV_STALE_TEST"
        )));
        assert_eq!(epsh.get_var("DENV_STALE_TEST"), Some("current"));
    }

    #[test]
    fn unparseable_record_does_not_wedge_denv_and_reload_discards_it() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_wedged_record");
        let data = tmp.path().join("data");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let mut epsh = denv_test_shell(&data, &outside, &[]);
        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);
        let id = epsh.vars().get("__DENV_PID").unwrap().to_string();
        let record = active_path(&data, &id);
        std::fs::write(&record, b"DENV-ACTIVE 99\n").unwrap();

        assert!(refresh(&session, false, &mut epsh).is_err());
        assert!(record.exists());

        let outcome = command(&session, &["reload".to_string()], &mut epsh);

        assert_eq!(outcome, 0);
        assert!(!record.exists());
        assert!(refresh(&session, false, &mut epsh).is_ok());
    }

    #[test]
    fn dropping_the_session_removes_its_active_record() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_exit_record");
        let data = tmp.path().join("data");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let dotenv = project.join(".env");
        std::fs::write(&dotenv, "DENV_EXIT_TEST=loaded\n").unwrap();
        allow_file_in(&data.join("allow"), &read_trusted_candidate(&dotenv).unwrap()).unwrap();
        let mut epsh = denv_test_shell(&data, &project, &[]);
        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);

        refresh(&session, false, &mut epsh).unwrap();
        assert_eq!(active_record_names(&data).len(), 1);

        drop(session);

        assert!(active_record_names(&data).is_empty());
    }

    #[test]
    fn startup_removes_records_of_dead_shells_and_keeps_live_ones() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_stale_records");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let live = std::process::id();
        let dead = i32::MAX - 1;
        let token = "00112233445566778899aabbccddeeff";
        let stale = [
            format!("active_{dead}-{token}"),
            format!(".active_{dead}-{token}.3.tmp"),
            format!("active_{dead}"),
            format!(".active_{dead}.7.tmp"),
        ];
        let kept = [
            format!("active_{live}-{token}"),
            format!(".active_{live}-{token}.3.tmp"),
            format!("active_{live}"),
            "active_notapid".to_string(),
            format!("allow_{dead}"),
        ];
        for name in stale.iter().chain(&kept) {
            std::fs::write(data.join(name), "x").unwrap();
        }
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut epsh = denv_test_shell(&data, &project, &[]);

        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);

        for name in &stale {
            assert!(!data.join(name).exists(), "{name} should be removed");
        }
        for name in &kept {
            assert!(data.join(name).exists(), "{name} should be kept");
        }
        let own = active_path(&data, session.record_id().unwrap());
        std::fs::write(&own, "x").unwrap();
        drop(session);
        assert!(!own.exists());
    }

    #[test]
    fn cleanup_keeps_records_of_other_pid_namespaces() {
        let tmp = TempDir::new("ish_denv_foreign_namespace");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let dead = i32::MAX - 1;
        let token = "00112233445566778899aabbccddeeff";
        let foreign = [
            format!("active_{dead}-4026531836-{token}"),
            format!(".active_{dead}-4026531836-{token}.3.tmp"),
        ];
        let own = [
            format!("active_{dead}-4026532000-{token}"),
            format!("active_{dead}-{token}"),
        ];
        for name in foreign.iter().chain(&own) {
            std::fs::write(data.join(name), "x").unwrap();
        }

        remove_dead_shell_records(&data, Some(4026532000)).unwrap();

        for name in &foreign {
            assert!(data.join(name).exists(), "{name} should be kept");
        }
        for name in &own {
            assert!(!data.join(name).exists(), "{name} should be removed");
        }
    }

    #[test]
    fn record_ids_carry_the_pid_namespace_and_parse_back() {
        let id = new_record_id(Some(4026531836)).unwrap();
        let owner = record_owner(&format!("active_{id}")).unwrap();
        assert_eq!(owner.pid.as_raw_nonzero().get() as u32, std::process::id());
        assert_eq!(owner.pid_namespace, Some(4026531836));
        let id = new_record_id(None).unwrap();
        assert_eq!(record_owner(&format!("active_{id}")).unwrap().pid_namespace, None);
    }

    #[test]
    fn parse_denv_state_supports_spaces_in_dir() {
        let parsed = parse_denv_state("1 2 /tmp/path with spaces").unwrap();
        assert_eq!(parsed.0, 1);
        assert_eq!(parsed.1, 2);
        assert_eq!(parsed.2, "/tmp/path with spaces");
    }

    #[test]
    fn first_line_uses_bash_detects_substring() {
        assert!(first_line_uses_bash(b"#!/usr/bin/env bash"));
        assert!(first_line_uses_bash(b"bash -eu"));
        assert!(!first_line_uses_bash(b"#!/bin/sh"));
    }

    #[test]
    fn evaluators_preserve_helper_rejection_with_a_64_descriptor_limit() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        const CHILD_MODE: &str = "ISH_DENV_LOW_FD_LIMIT_CHILD";
        if env::var_os(CHILD_MODE).is_none() {
            let output = Command::new(env::current_exe().unwrap())
                .args(["--exact", "denv::tests::evaluators_preserve_helper_rejection_with_a_64_descriptor_limit", "--test-threads=1"])
                .env(CHILD_MODE, "1")
                .output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        rustix::process::setrlimit(rustix::process::Resource::Nofile, rustix::process::Rlimit {
            current: Some(64),
            maximum: Some(64),
        }).unwrap();
        {
            let tmp = TempDir::new("ish_denv_low_fd_limit");
            let shell = epsh::eval::Shell::new();
            let env_pairs = shell.vars().env_for_command_os(&[]);
            let source = b"export DENV_LOW_FD=$(printf '%s' ok | cat)\n";
            let epsh = eval_env_epsh(tmp.path(), source, &[], &env_pairs, Duration::from_secs(2)).unwrap();
            assert!(epsh.set.contains(&("DENV_LOW_FD".into(), "ok".into())));
            let bash = eval_env_bash(tmp.path(), source, &[], &env_pairs, Duration::from_secs(2)).unwrap();
            assert!(bash.set.contains(&("DENV_LOW_FD".into(), "ok".into())));
        }
        retired_helpers_cannot_redirect_failure_channel(EnvRcInterpreter::Epsh);
        retired_helpers_cannot_redirect_failure_channel(EnvRcInterpreter::Bash);
    }

    #[test]
    fn retired_helpers_cannot_redirect_failure_channel_bash() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        retired_helpers_cannot_redirect_failure_channel(EnvRcInterpreter::Bash);
    }

    #[test]
    fn retired_helpers_cannot_redirect_failure_channel_epsh() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        retired_helpers_cannot_redirect_failure_channel(EnvRcInterpreter::Epsh);
    }

    fn retired_helpers_cannot_redirect_failure_channel(interpreter: EnvRcInterpreter) {
        let tmp = TempDir::new("ish_denv_redirect_failure");
        fs::write(tmp.path().join("shared"), "export DENV_REDIRECT_TARGET=loaded\n").unwrap();
        let shell = epsh::eval::Shell::new();
        let env_pairs = shell.vars().env_for_command_os(&[]);
        let probe = FailureChannel::new().unwrap();
        let reporters = probe.reporters();
        drop(probe);
        for fd in (3..=9).chain(reporters) {
            let source = format!("export DENV_REDIRECT_BEFORE=yes\nsource_env shared {fd}>/dev/null || true\nexport DENV_REDIRECT_AFTER=yes\n");
            let result = match interpreter {
                EnvRcInterpreter::Bash => eval_env_bash(tmp.path(), source.as_bytes(), &[], &env_pairs, Duration::from_secs(2)),
                EnvRcInterpreter::Epsh => eval_env_epsh(tmp.path(), source.as_bytes(), &[], &env_pairs, Duration::from_secs(2)),
            };
            let err = match result {
                Ok(_) => panic!("redirection of fd {fd} bypassed retired-helper rejection"),
                Err(err) => err,
            };
            assert!(err.contains("source_env"), "{err}");
            assert_eq!(env::var_os("DENV_REDIRECT_BEFORE"), None);
            assert_eq!(env::var_os("DENV_REDIRECT_AFTER"), None);
        }
    }

    #[test]
    fn retired_helpers_invalidate_masked_and_forked_candidates() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_retired");
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(tmp.path().join(".envrc"), "export DENV_RETIRED_TARGET=parent\nprintf touched > parent-marker\n").unwrap();
        fs::write(project.join("shared"), "export DENV_RETIRED_TARGET=shared\nprintf touched > shared-marker\n").unwrap();
        let shell = epsh::eval::Shell::new();
        let env_pairs = shell.vars().env_for_command_os(&[]);
        for helper in ["source_env", "source_env_if_exists", "source_up", "source_up_if_exists", "dotenv", "dotenv_if_exists"] {
            for target in ["shared", "missing"] {
                for invocation in [
                    format!("{helper} {target}"),
                    format!("{helper} {target} || true"),
                    format!("if {helper} {target}; then :; fi"),
                    format!("value=$({helper} {target}); true"),
                    format!("({helper} {target}); true"),
                    format!("{helper} {target} | cat; true"),
                ] {
                    let source = format!("export DENV_RETIRED_BEFORE=yes\n{invocation}\nexport DENV_RETIRED_AFTER=yes\n");
                    for bash in [false, true] {
                        let result = if bash {
                            eval_env_bash(&project, source.as_bytes(), &[], &env_pairs, Duration::from_secs(2))
                        } else {
                            eval_env_epsh(&project, source.as_bytes(), &[], &env_pairs, Duration::from_secs(2))
                        };
                        let err = match result {
                            Ok(_) => panic!("accepted retired helper: bash={bash}, {invocation}"),
                            Err(err) => err,
                        };
                        assert!(err.contains(helper), "missing helper diagnostic: {err}");
                        assert!(!project.join("shared-marker").exists());
                        assert!(!project.join("parent-marker").exists());
                        assert_eq!(env::var_os("DENV_RETIRED_BEFORE"), None);
                        assert_eq!(env::var_os("DENV_RETIRED_AFTER"), None);
                    }
                }
            }
        }
    }

    #[test]
    fn retired_helper_flood_respects_the_evaluation_deadline() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        const CHILD_MODE: &str = "ISH_DENV_FAILURE_FLOOD_CHILD";
        if env::var_os(CHILD_MODE).is_none() {
            let mut child = Command::new(env::current_exe().unwrap())
                .args(["--exact", "denv::tests::retired_helper_flood_respects_the_evaluation_deadline", "--test-threads=1"])
                .env(CHILD_MODE, "1")
                .spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("retired helper flood blocked its evaluator");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let tmp = TempDir::new("ish_denv_flood");
        let shell = epsh::eval::Shell::new();
        let env_pairs = shell.vars().env_for_command_os(&[]);
        let source = b"{ while true; do dotenv || :; done; } 2>/dev/null\n";
        for bash in [false, true] {
            let started = Instant::now();
            let result = if bash {
                eval_env_bash(tmp.path(), source, &[], &env_pairs, Duration::from_millis(200))
            } else {
                eval_env_epsh(tmp.path(), source, &[], &env_pairs, Duration::from_millis(200))
            };
            let err = match result {
                Ok(_) => panic!("accepted helper flood"),
                Err(err) => err,
            };
            assert!(err.contains("dotenv"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn bash_failure_channel_bounds_descendants_after_snapshot_close() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_failure_descendant");
        let shell = epsh::eval::Shell::new();
        let env_pairs = shell.vars().env_for_command_os(&[]);
        let source = b"(exec 3>&- 4>&-; sleep 30) &\n";
        let started = Instant::now();
        let result = eval_env_bash(tmp.path(), source, &[], &env_pairs, Duration::from_millis(100));
        let err = match result {
            Ok(_) => panic!("accepted incomplete failure channel"),
            Err(err) => err,
        };
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn bash_requires_a_completed_candidate_before_applying_exports() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_incomplete");
        let shell = epsh::eval::Shell::new();
        let env_pairs = shell.vars().env_for_command_os(&[]);
        for source in ["export DENV_INCOMPLETE=1; exit 0", "export DENV_INCOMPLETE=1; exec true"] {
            let result = eval_env_bash(tmp.path(), source.as_bytes(), &[], &env_pairs, Duration::from_secs(2));
            let err = match result {
                Ok(_) => panic!("accepted incomplete candidate: {source}"),
                Err(err) => err,
            };
            assert!(err.contains("incomplete"), "{err}");
        }
    }

    #[test]
    fn eval_env_uses_epsh_for_non_bash_envrc() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_bash_env");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source: &[u8] =
            b"#!/usr/bin/env bash\nif [ -n \"${DENV_AMBIENT_ONLY+x}\" ]; then export DENV_LEAK=1; fi\n";

        let epsh = epsh::eval::Shell::builder().env_clear().build();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let diff = eval_env(&project, Some(source), &[], &env_pairs).unwrap();
        assert!(!diff.set.iter().any(|(key, _)| key == "DENV_LEAK"));
    }

    #[test]
    fn eval_env_epsh_restores_process_env_after_failure() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_epsh_restore");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source: &[u8] = b"#!/bin/sh\nexport DENV_TMP_SHOULD_ROLLBACK=1\nfalse\n";

        let epsh = epsh::eval::Shell::new();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let err = match eval_env_epsh(&project, source, &[], &env_pairs, Duration::from_secs(30)) {
            Ok(_) => panic!("expected epsh envrc evaluation to fail"),
            Err(err) => err,
        };
        assert_eq!(err, ".envrc evaluation failed");
        assert!(std::env::var_os("DENV_TMP_SHOULD_ROLLBACK").is_none());
    }

    #[test]
    fn epsh_envrc_evaluation_times_out() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_epsh_timeout");
        let epsh = epsh::eval::Shell::new();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let start = Instant::now();
        let err = match eval_env_epsh(
            tmp.path(),
            b"#!/bin/sh\nsleep 30\n",
            &[],
            &env_pairs,
            Duration::from_millis(100),
        ) {
            Ok(_) => panic!("expected timeout"),
            Err(err) => err,
        };
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn explicit_environment_preserves_non_utf8_entries() {
        let name = OsString::from_vec(b"DENV_\xff".to_vec());
        let value = OsString::from_vec(b"value\x80".to_vec());
        let shell = epsh::eval::Shell::builder().environment(vec![(name.clone(), value.clone())]).build();
        assert_eq!(shell.vars().env_for_command_os(&[]), [(name, value)]);
    }

    #[test]
    fn home_change_restores_through_session_storage() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        project_storage_change_restores("HOME", ".env");
    }

    #[test]
    fn data_dir_change_restores_through_session_storage() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        project_storage_change_restores("DENV_DATA_DIR", ".envrc");
    }

    fn project_storage_change_restores(key: &str, filename: &str) {
        let tmp = TempDir::new("s6");
        let home = tmp.path().join("h");
        let project = tmp.path().join("p");
        let outside = tmp.path().join("o");
        let changed = tmp.path().join("changed");
        for dir in [&home, &project, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(
            project.join(filename),
            format!("export {key}={}\nexport DENV_SESSION_MARKER=project\n", changed.display()),
        )
        .unwrap();
        let mut epsh = epsh::eval::Shell::builder().env_clear().build();
        for (name, value) in [
            ("HOME", home.to_str().unwrap()),
            ("PWD", project.to_str().unwrap()),
            ("DENV_SESSION_MARKER", "original"),
        ] {
            epsh.vars_mut().set(name, value).unwrap();
            epsh.vars_mut().export(name);
        }
        let data = if key == "DENV_DATA_DIR" {
            let data = tmp.path().join("data");
            epsh.vars_mut().set(key, data.to_str().unwrap()).unwrap();
            epsh.vars_mut().export(key);
            data
        } else {
            home.join(".local/share/denv")
        };
        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);
        let outcome = command(&session, &["allow".to_string()], &mut epsh);
        assert_eq!(outcome, 0);
        assert_eq!(epsh.vars().get("DENV_SESSION_MARKER"), Some("project"));
        assert_eq!(epsh.vars().get(key), changed.to_str());
        let active = active_path(&data, session.record_id().unwrap());
        assert!(active.exists());

        let reload = command(&session, &["reload".to_string()], &mut epsh);
        assert_eq!(reload, 0);
        assert_eq!(epsh.vars().get("DENV_SESSION_MARKER"), Some("project"));
        assert_eq!(epsh.vars().get(key), changed.to_str());
        let deny = command(&session, &["deny".to_string()], &mut epsh);
        assert_eq!(deny, 0);
        assert_eq!(epsh.vars().get("DENV_SESSION_MARKER"), Some("original"));
        assert!(!active.exists());
        let allow = command(&session, &["allow".to_string()], &mut epsh);
        assert_eq!(allow, 0);
        assert_eq!(epsh.vars().get("DENV_SESSION_MARKER"), Some("project"));

        epsh.vars_mut().set("PWD", outside.to_str().unwrap()).unwrap();
        epsh.set_cwd(outside.clone());
        on_cd(&session, &mut epsh).unwrap();
        assert_eq!(epsh.vars().get("DENV_SESSION_MARKER"), Some("original"));
        assert_eq!(epsh.vars().get("HOME"), home.to_str());
        if key == "DENV_DATA_DIR" {
            assert_eq!(epsh.vars().get(key), data.to_str());
        }
        assert!(!active.exists());
        assert!(active_record_names(&changed).is_empty());
    }

    #[test]
    fn state_var_matches_uses_cached_dir() {
        let tmp = TempDir::new("ish_denv_fast_path");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let envrc = project.join(".envrc");
        std::fs::write(&envrc, "export OK=1\n").unwrap();
        let mtime = stat_regular_file_mtime(&envrc).unwrap();
        let state = format!("{mtime} 0 {}", project.display());

        let mut variables = Variables::new_clean();
        variables.set("__DENV_STATE", &state).unwrap();
        assert!(state_var_matches(&find_env_files(&project).unwrap(), &variables));

        std::fs::write(&envrc, "export OK=2\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&envrc)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(mtime + 2))
            .unwrap();
        assert!(!state_var_matches(&find_env_files(&project).unwrap(), &variables));
    }

    #[test]
    fn cached_ancestor_does_not_skip_child_or_same_mtime_content_changes() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_fast_path_stale");
        let data = tmp.path().join("data");
        let project = tmp.path().join("project");
        let child = project.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let parent_env = project.join(".env");
        let child_env = child.join(".env");
        std::fs::write(&parent_env, "DENV_CACHE_TEST=parent\n").unwrap();
        std::fs::write(&child_env, "DENV_CACHE_TEST=child\n").unwrap();
        let parent_mtime = stat_regular_file_mtime(&parent_env).unwrap();
        let state = format!("0 {parent_mtime} {}", project.display());
        let allow = data.join("allow");
        allow_file_in(&allow, &read_trusted_candidate(&parent_env).unwrap()).unwrap();
        allow_file_in(&allow, &read_trusted_candidate(&child_env).unwrap()).unwrap();

        let mut epsh = epsh::eval::Shell::builder().env_clear().build();
        for (key, value) in [
            ("DENV_DATA_DIR", data.to_str().unwrap()),
            ("PWD", child.to_str().unwrap()),
            ("DENV_CACHE_TEST", "parent"),
            ("__DENV_STATE", &state),
        ] {
            epsh.vars_mut().set(key, value).unwrap();
            epsh.vars_mut().export(key);
        }
        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);
        save_active(
            &data,
            session.record_id().unwrap(),
            &ActiveState {
                dir: canonicalize_fallback(&project),
                envrc_mtime: 0,
                dotenv_mtime: parent_mtime,
                prev: vec![PrevVar::Unset("DENV_CACHE_TEST".to_string())],
            },
        )
        .unwrap();

        let child_changes = refresh(&session, false, &mut epsh).unwrap();
        assert!(child_changes.iter().any(|change| matches!(
            change,
            EnvChange::Set(key, value) if key == "DENV_CACHE_TEST" && value == "child"
        )));

        let old_mtime = std::fs::metadata(&child_env).unwrap().modified().unwrap();
        std::fs::write(&child_env, "DENV_CACHE_TEST=changed\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&child_env)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();
        let changed = refresh(&session, false, &mut epsh).unwrap();
        assert!(changed.iter().any(|change| matches!(
            change,
            EnvChange::Unset(key) if key == "DENV_CACHE_TEST"
        )));
    }

    #[test]
    fn malformed_dotenv_keeps_active_restore_for_the_next_directory() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_malformed_restore");
        let data = tmp.path().join("data");
        let project = tmp.path().join("project");
        let bad = tmp.path().join("bad");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        std::fs::write(project.join(".env"), "DENV_RESTORE_TEST=leaked\n").unwrap();
        let malformed = bad.join(".env");
        std::fs::write(&malformed, b"DENV_RESTORE_TEST=\xff\n").unwrap();
        let allow = data.join("allow");
        allow_file_in(&allow, &read_trusted_candidate(&project.join(".env")).unwrap()).unwrap();
        allow_file_in(&allow, &read_trusted_candidate(&malformed).unwrap()).unwrap();

        let mut epsh = epsh::eval::Shell::builder().env_clear().build();
        for (key, value) in [
            ("DENV_DATA_DIR", data.to_str().unwrap()),
            ("PWD", bad.to_str().unwrap()),
            ("DENV_RESTORE_TEST", "leaked"),
        ] {
            epsh.vars_mut().set(key, value).unwrap();
            epsh.vars_mut().export(key);
        }
        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);
        let pid = session.record_id().unwrap().to_string();
        save_active(
            &data,
            &pid,
            &ActiveState {
                dir: canonicalize_fallback(&project),
                envrc_mtime: 0,
                dotenv_mtime: stat_regular_file_mtime(&project.join(".env")).unwrap(),
                prev: vec![PrevVar::Unset("DENV_RESTORE_TEST".to_string())],
            },
        )
        .unwrap();

        let err = match refresh(&session, false, &mut epsh) {
            Ok(_) => panic!("expected malformed dotenv to fail"),
            Err(err) => err,
        };
        assert!(err.contains("utf-8"), "{err}");
        assert!(load_active(&data, &pid).unwrap().is_some());

        epsh.vars_mut().set("PWD", outside.to_str().unwrap()).unwrap();
        epsh.set_cwd(outside.clone());
        let changes = refresh(&session, false, &mut epsh).unwrap();
        assert!(changes.iter().any(|change| matches!(
            change,
            EnvChange::Unset(key) if key == "DENV_RESTORE_TEST"
        )));
        assert!(load_active(&data, &pid).unwrap().is_none());
    }

    #[test]
    fn active_save_failure_preserves_shell_and_restore_state() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_active_save_failure");
        let data = tmp.path().join("data");
        let project = tmp.path().join("project");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let dotenv = project.join(".env");
        std::fs::write(&dotenv, "DENV_SAVE_TEST=loaded\n").unwrap();
        allow_file_in(
            &data.join("allow"),
            &read_trusted_candidate(&dotenv).unwrap(),
        )
        .unwrap();

        let mut epsh = epsh::eval::Shell::builder().env_clear().build();
        for (key, value) in [
            ("DENV_DATA_DIR", data.to_str().unwrap()),
            ("PWD", project.to_str().unwrap()),
            ("DENV_SAVE_TEST", "base"),
        ] {
            epsh.vars_mut().set(key, value).unwrap();
            epsh.vars_mut().export(key);
        }

        if let Some(pwd) = epsh.get_var_bytes("PWD") { epsh.set_cwd(pwd.to_path_buf()); }
        let session = init(&mut epsh);
        refresh(&session, false, &mut epsh).unwrap();
        let pid = session.record_id().unwrap().to_string();
        let active_path = active_path(&data, &pid);
        assert_eq!(
            std::fs::metadata(active_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let original_active = load_active(&data, &pid).unwrap().unwrap();
        assert!(original_active.prev.iter().any(|previous| matches!(
            previous,
            PrevVar::Restore(key, value, _) if key == "DENV_SAVE_TEST" && value == "base"
        )));

        let first_temp_id = NEXT_ACTIVE_TEMP_ID.load(AtomicOrdering::Relaxed);
        for id in first_temp_id..first_temp_id + ACTIVE_TEMP_NAME_ATTEMPTS {
            std::fs::write(data.join(format!(".active_{pid}.{id}.tmp")), b"occupied").unwrap();
        }
        let failed_refresh = refresh(&session, true, &mut epsh);
        let err = match failed_refresh {
            Ok(_) => panic!("expected active-state write to fail"),
            Err(err) => err,
        };
        assert!(err.contains("temporary names"), "{err}");
        assert_eq!(epsh.get_var("DENV_SAVE_TEST"), Some("loaded"));
        assert_eq!(
            epsh.vars().get_bytes("DENV_SAVE_TEST").unwrap().as_bytes(),
            b"loaded"
        );
        let preserved_active = load_active(&data, &pid).unwrap().unwrap();
        assert!(preserved_active.prev.iter().any(|previous| matches!(
            previous,
            PrevVar::Restore(key, value, _) if key == "DENV_SAVE_TEST" && value == "base"
        )));

        epsh.vars_mut().set("PWD", outside.to_str().unwrap()).unwrap();
        epsh.set_cwd(outside.clone());
        let restored = refresh(&session, false, &mut epsh).unwrap();
        assert!(restored.iter().any(|change| matches!(
            change,
            EnvChange::Set(key, value) if key == "DENV_SAVE_TEST" && value == "base"
        )));
        assert!(load_active(&data, &pid).unwrap().is_none());
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
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        assert_eq!(diff.set, vec![("DENV_DOTENV_VALUE".to_string(), value.into())]);
        assert!(!marker.exists());
    }

    #[test]
    fn bash_envrc_cannot_change_dotenv_precedence_inputs() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_dotenv_precedence");
        let dotenv_entries = vec![("DENV_PRECEDENCE".to_string(), "from_dotenv".to_string())];
        let epsh = epsh::eval::Shell::builder().env_clear().build();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let diff = eval_env_bash(
            tmp.path(),
            b"#!/usr/bin/env bash\nunset __DENV_DOT_0\nexport DENV_PRECEDENCE=from_envrc\n",
            &dotenv_entries,
            &env_pairs,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(diff.set, vec![("DENV_PRECEDENCE".to_string(), ShellBytes::from("from_dotenv"))]);
    }

    #[test]
    fn bash_snapshot_writers_survive_exec_when_low_fds_are_free() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        const CHILD_MODE: &str = "ISH_DENV_SNAPSHOT_FD_CHILD";
        if env::var_os(CHILD_MODE).is_none() {
            let output = std::process::Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "denv::tests::bash_snapshot_writers_survive_exec_when_low_fds_are_free",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_MODE, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }

        // SAFETY: this subprocess reserves descriptors 3, 4, 5, 10 and 11 for this test.
        unsafe {
            libc::close(BEFORE_SNAPSHOT_FD);
            libc::close(AFTER_SNAPSHOT_FD);
            libc::close(5);
            libc::close(failure::REPORT_FD_MIN);
            libc::close(failure::REPORT_FD_MIN + 1);
        }
        let blockers = [
            std::fs::File::open("/dev/null").unwrap(),
            std::fs::File::open("/dev/null").unwrap(),
            std::fs::File::open("/dev/null").unwrap(),
        ];
        assert_eq!(blockers[0].as_raw_fd(), BEFORE_SNAPSHOT_FD);
        assert_eq!(blockers[1].as_raw_fd(), AFTER_SNAPSHOT_FD);
        assert_eq!(blockers[2].as_raw_fd(), 5);

        let (before_read, before_w) = rustix::pipe::pipe().unwrap();
        let (after_read, after_w) = rustix::pipe::pipe().unwrap();
        let failure = FailureChannel::new().unwrap();
        drop(blockers);
        let (before_src, after_src) = duplicate_snapshot_writers(&before_w, &after_w).unwrap();
        let reporters = [
            rustix::io::fcntl_dupfd_cloexec(&failure.write, failure::REPORT_FD_MIN).unwrap(),
            rustix::io::fcntl_dupfd_cloexec(&failure.backup, failure::REPORT_FD_MIN).unwrap(),
        ];
        drop(before_w);
        drop(after_w);

        let mut script = String::new();
        FailureChannel::install_stubs(&mut script, reporters.each_ref().map(AsRawFd::as_raw_fd));
        script.push_str("env -0 >&3\n(source_env missing || :)\nexport DENV_FD_SNAPSHOT=after\nenv -0 >&4");
        let mut command = std::process::Command::new("bash");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("DENV_FD_SNAPSHOT", "before")
            .arg("-e")
            .arg("-c")
            .arg(script);
        // SAFETY: the child hook remaps owned snapshot sources and clears close-on-exec on owned reporters.
        unsafe {
            command.pre_exec(move || {
                remap_snapshot_writers(&before_src, &after_src)?;
                for reporter in &reporters {
                    rustix::io::fcntl_setfd(reporter, rustix::io::FdFlags::empty())?;
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        assert!(child.wait().unwrap().success());
        assert!(matches!(failure.finish(), Err(failure::EvaluationFailure::RetiredHelper(failure::RetiredHelper::SourceEnv))));

        let mut before_data = Vec::new();
        let mut after_data = Vec::new();
        std::fs::File::from(before_read)
            .read_to_end(&mut before_data)
            .unwrap();
        std::fs::File::from(after_read)
            .read_to_end(&mut after_data)
            .unwrap();
        let before = parse_env_null(&before_data);
        let after = parse_env_null(&after_data);
        let diff = diff_sorted_env(&before, &after);
        assert!(before.iter().any(|(key, value)| {
            key == "DENV_FD_SNAPSHOT" && value == "before"
        }));
        assert!(after
            .iter()
            .any(|(key, value)| key == "DENV_FD_SNAPSHOT" && value == "after"));
        assert!(diff
            .set
            .contains(&("DENV_FD_SNAPSHOT".into(), "after".into())));

        let tmp = TempDir::new("ish_denv_repeated_fds");
        let shell = epsh::eval::Shell::new();
        let env_pairs = shell.vars().env_for_command_os(&[]);
        let descriptor_directory = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
        let before = fs::read_dir(descriptor_directory).unwrap().count();
        for _ in 0..16 {
            for source in [b"export DENV_REPEAT=ok\n".as_slice(), b"dotenv_if_exists absent || true\n"] {
                let rejected = source.starts_with(b"dotenv");
                assert_eq!(eval_env_bash(tmp.path(), source, &[], &env_pairs, Duration::from_secs(2)).is_err(), rejected);
                assert_eq!(eval_env_epsh(tmp.path(), source, &[], &env_pairs, Duration::from_secs(2)).is_err(), rejected);
            }
        }
        assert_eq!(fs::read_dir(descriptor_directory).unwrap().count(), before, "repeated denv evaluations leaked descriptors");
    }

    #[test]
    fn dotenv_values_reach_epsh_as_data_not_script() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_dotenv_epsh");
        let marker = tmp.path().join("marker");
        let value = format!("it's $(touch {}) `x`", marker.display());
        let entries = vec![("DENV_DOTENV_VALUE".to_string(), value.clone())];
        let epsh = epsh::eval::Shell::new();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let diff = eval_env_epsh(
            tmp.path(),
            b"#!/bin/sh\n:\n",
            &entries,
            &env_pairs,
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(diff.set.contains(&("DENV_DOTENV_VALUE".to_string(), value.into())));
        assert!(!marker.exists());
    }

    #[test]
    fn bash_envrc_evaluation_times_out() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    fn bash_envrc_timeout_covers_descendants_holding_snapshot_pipes() {
        let _evaluation = EVALUATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new("ish_denv_descendant_timeout");
        let marker = tmp.path().join("descendant_completed");
        let epsh = epsh::eval::Shell::new();
        let env_pairs = epsh.vars().env_for_command_os(&[]);
        let start = Instant::now();
        let result = eval_env_bash(
            tmp.path(),
            format!(
                "#!/usr/bin/env bash\n(sleep 0.3; printf done > '{}') &\n",
                marker.display()
            )
            .as_bytes(),
            &[],
            &env_pairs,
            Duration::from_millis(50),
        );
        let err = match result {
            Ok(_) => panic!("expected descendant timeout"),
            Err(err) => err,
        };
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!marker.exists());
    }

    #[test]
    fn trust_long_project_paths_allow_edit_and_deny() {
        let tmp = TempDir::new("t14");
        let data = tmp.path().join("data");
        let project = tmp.path()
            .join("ordinary-project")
            .join("nested-directory")
            .join("another-directory")
            .join("extended-trust-project-directory")
            .join("project-workspace");
        fs::create_dir_all(&project).unwrap();
        let path = project.join(".env");
        fs::write(&path, "A=1\n").unwrap();
        let file = read_trusted_candidate(&path).unwrap();
        assert!(
            file.path.as_os_str().as_bytes().len() > 127,
            "path has {} bytes: {}",
            file.path.as_os_str().as_bytes().len(),
            file.path.display()
        );
        let allow = data.join("allow");

        allow_file_in(&allow, &file).unwrap();
        assert!(is_allowed_in(&allow, &file));
        fs::write(&path, "A=2\n").unwrap();
        let edited = read_trusted_candidate(&path).unwrap();
        assert!(!is_allowed_in(&allow, &edited));
        allow_file_in(&allow, &edited).unwrap();
        assert!(is_allowed_in(&allow, &edited));
        deny_file(&data, &edited.path).unwrap();
        assert!(!is_allowed_in(&allow, &edited));
        deny_file(&data, &edited.path).unwrap();
    }

    #[test]
    fn trust_legacy_approval_remains_valid_and_deny_removes_both_formats() {
        let tmp = TempDir::new("t14");
        let data = tmp.path().join("data");
        let allow = data.join("allow");
        fs::create_dir_all(&allow).unwrap();
        let path = tmp.path().join(".env");
        fs::write(&path, "A=1\n").unwrap();
        let file = read_trusted_candidate(&path).unwrap();
        let legacy_key: String = file.path.as_os_str().as_bytes().iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        fs::write(allow.join(&legacy_key), &file.hash).unwrap();

        assert!(is_allowed_in(&allow, &file));
        deny_file(&data, &file.path).unwrap();
        assert!(!is_allowed_in(&allow, &file));
        fs::write(allow.join(&legacy_key), &file.hash).unwrap();
        fs::write(&path, "A=2\n").unwrap();
        let edited = read_trusted_candidate(&path).unwrap();
        assert!(!is_allowed_in(&allow, &edited));
        allow_file_in(&allow, &edited).unwrap();
        assert!(is_allowed_in(&allow, &edited));
        assert!(!is_allowed_in(&allow, &file));
        fs::write(allow.join(trust_key(&file.path)), [0xff]).unwrap();
        assert!(!is_allowed_in(&allow, &file));
        assert!(!is_allowed_in(&allow, &edited));
        allow_file_in(&allow, &edited).unwrap();
        deny_file(&data, &file.path).unwrap();
        assert!(!is_allowed_in(&allow, &edited));
        assert!(!is_allowed_in(&allow, &file));
        assert!(!allow.join(legacy_key).exists());
    }

    #[test]
    fn trust_preserves_non_utf8_identity_and_canonical_aliases() {
        let tmp = TempDir::new("t14");
        let data = tmp.path().join("data");
        let project = tmp.path().join("ordinary-project").join("nested-directory");
        fs::create_dir_all(&project).unwrap();
        let first = project.join(OsString::from_vec(vec![b'.', b'e', b'n', b'v', 0xfe]));
        let second = project.join(OsString::from_vec(vec![b'.', b'e', b'n', b'v', 0xff]));
        fs::write(&first, "A=1\n").unwrap();
        fs::write(&second, "A=1\n").unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let allow = data.join("allow");
        let file = read_trusted_candidate(&first).unwrap();

        allow_file_in(&allow, &file).unwrap();
        assert!(is_allowed_in(&allow, &read_trusted_candidate(&alias).unwrap()));
        assert!(!is_allowed_in(&allow, &read_trusted_candidate(&second).unwrap()));
        deny_file(&data, &file.path).unwrap();
        assert!(!is_allowed_in(&allow, &read_trusted_candidate(&alias).unwrap()));
    }

    #[test]
    fn trust_binds_content_hash_not_mtime() {
        let tmp = TempDir::new("ish_denv_trust");
        let data = tmp.path().join("data");
        let file = tmp.path().join(".env");
        std::fs::write(&file, "A=1\n").unwrap();
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
