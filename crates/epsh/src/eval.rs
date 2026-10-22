use fxhash::FxHashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::RawFd;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::ast::*;
use crate::callback::Registered;
pub use crate::callback::CallbackKind;

#[path = "jobs.rs"]
mod jobs;
pub use jobs::{ForegroundJob, ProgramOutcome};

fn is_dot_navigation(path: &Path) -> bool {
    let mut has_component = false;
    for component in path.components() {
        has_component = true;
        if !matches!(component, Component::CurDir | Component::ParentDir) {
            return false;
        }
    }
    has_component
}

fn escape_command_name(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b' '..=b'~' if byte != b'\\' => escaped.push(byte as char),
            b'\\' => escaped.push_str("\\\\"),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\x{byte:02x}");
            }
        }
    }
    escaped
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn read_shebang_interpreter(path: &Path) -> Option<OsString> {
    let mut buf = [0u8; 128];
    let mut file = std::fs::File::open(path).ok()?;
    let n = file.read(&mut buf).ok()?;
    if n < 2 || buf[0] != b'#' || buf[1] != b'!' {
        return None;
    }
    let mut i = 2;
    while i < n && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    while i < n && !buf[i].is_ascii_whitespace() {
        i += 1;
    }
    if start >= i {
        return None;
    }
    Some(OsString::from_vec(buf[start..i].to_vec()))
}

fn os_error_string(code: i32) -> String {
    unsafe {
        let msg = libc::strerror(code);
        if msg.is_null() {
            return format!("os error {code}");
        }
        std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned()
    }
}

fn resolve_exec_path(cmd_name: &OsStr, cwd: &Path, path_env: &OsStr) -> Option<PathBuf> {
    let name_path = PathBuf::from(cmd_name);
    if cmd_name.as_bytes().contains(&b'/') {
        return Some(if name_path.is_absolute() {
            name_path
        } else {
            cwd.join(name_path)
        });
    }
    for directory in path_env.as_bytes().split(|byte| *byte == b':') {
        let dir = if directory.is_empty() {
            PathBuf::from(".")
        } else {
            PathBuf::from(OsStr::from_bytes(directory))
        };
        let full = if dir.is_absolute() {
            dir.join(cmd_name)
        } else {
            cwd.join(dir).join(cmd_name)
        };
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

/// bash-style `cmd: interp: bad interpreter: <errno>` message when `cmd`
/// resolves to an executable script whose `#!` interpreter cannot run.
/// Returns None when the error is unrelated to a shebang script.
///
/// `cwd` is the directory relative paths resolve against; `path_env` is the
/// PATH used for slashless command lookup. Embedders with a custom
/// [`Shell::set_external_handler`] handler can call this with the same cwd and
/// PATH their spawn uses to match the default exec error reporting.
pub fn bad_interpreter_message(
    cmd_name: &str,
    code: i32,
    cwd: &Path,
    path_env: &str,
) -> Option<String> {
    bad_interpreter_message_os(OsStr::new(cmd_name), code, cwd, OsStr::new(path_env))
        .map(|message| message.to_string_lossy().into_owned())
}

/// Return a bad-interpreter diagnostic with command and interpreter bytes intact.
/// `cmd_name` and `path_env` are the raw OS arguments used for execution.
pub fn bad_interpreter_message_os(
    cmd_name: &OsStr,
    code: i32,
    cwd: &Path,
    path_env: &OsStr,
) -> Option<OsString> {
    if matches!(code, libc::E2BIG | libc::ENOMEM) {
        return None;
    }
    let path = resolve_exec_path(cmd_name, cwd, path_env)?;
    if !is_executable_file(&path) {
        return None;
    }
    let interp = read_shebang_interpreter(&path)?;
    let mut message = cmd_name.as_bytes().to_vec();
    message.extend_from_slice(b": ");
    message.extend_from_slice(interp.as_bytes());
    message.extend_from_slice(b": bad interpreter: ");
    message.extend_from_slice(os_error_string(code).as_bytes());
    Some(OsString::from_vec(message))
}

fn apply_dot_navigation(cwd: &Path, path: &Path) -> PathBuf {
    let mut result = cwd.to_path_buf();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => unreachable!("is_dot_navigation filters non-navigation components"),
        }
    }
    result
}

fn spawn_relay(stream: HeldDescriptor, sink: OutputSink) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || {
        let _reader_owner = RelayReader(stream.clone());
        let mut buffer = [0u8; 8192];
        loop {
            let reader = stream.lock().unwrap();
            let Some(descriptor) = reader.as_ref() else { return Ok(()); };
            match rustix::io::read(descriptor, &mut buffer) {
                Ok(0) => return Ok(()),
                Ok(length) => {
                    drop(reader);
                    sink.lock().map_err(|_| std::io::Error::other("output sink is poisoned"))?.write_all(&buffer[..length])?;
                }
                Err(rustix::io::Errno::INTR) => {}
                Err(rustix::io::Errno::AGAIN) => {
                    drop(reader);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
    })
}

pub(crate) type OutputSink = Arc<Mutex<dyn Write + Send>>;
type HeldDescriptor = Arc<Mutex<Option<OwnedFd>>>;

struct RelayReader(HeldDescriptor);

impl Drop for RelayReader {
    fn drop(&mut self) {
        drop(self.0.lock().unwrap().take());
    }
}

struct ChildRelays {
    handles: Vec<std::thread::JoinHandle<std::io::Result<()>>>,
    descriptors: Vec<usize>,
}

struct ForkOutput {
    target: RawFd,
    read: OwnedFd,
    write: OwnedFd,
    sink: OutputSink,
}
use crate::error::{ExitStatus, ShellError, Span};
use crate::expand;
use crate::glob;
use crate::parser::Parser;
use crate::shell_bytes::ShellBytes;
use crate::sys;
use crate::var::Variables;

/// Callback for external command execution.
///
/// Receives expanded argv (`argv[0]` is the command name) and the complete
/// environment the command should run with, as raw bytes: inherited
/// non-shell-name entries, exported shell variables, and any prefix assignment
/// pairs (which override exported values). This is exactly the environment
/// `eval_external` would construct (`env_for_command_os`), so a handler can
/// replace epsh's exec path without inheriting the embedder's process
/// environment. Redirections are already applied to file descriptors before
/// the handler is called. Return the exit status of the command.
pub type ExternalHandler = Box<
    dyn FnMut(&[ShellBytes], &[(ShellBytes, ShellBytes)]) -> crate::error::Result<ExitStatus>
        + Send,
>;

/// Callback for expanded command targets. Return `None` to allow a target or
/// `Some(diagnostic)` to refuse it with status 2.
pub type CommandPolicy = Box<dyn FnMut(&ShellBytes) -> Option<String> + Send>;

/// Optional raw-argv validation before target redirections and dispatch assignments.
pub type CommandArgvPolicy = Box<dyn FnMut(&[ShellBytes]) -> Option<String> + Send>;

/// The result of an external dispatcher, including delegation to epsh's executor.
pub enum ExternalDispatch {
    Handled(ExitStatus),
    NotHandled,
}

/// A callback selected by owned command names that may delegate to epsh.
pub type ExternalDispatcher = Box<dyn FnMut(&[ShellBytes], &[(ShellBytes, ShellBytes)]) -> crate::error::Result<ExternalDispatch> + Send>;

/// Owned command validation that runs without an embedding callback.
#[derive(Clone)]
pub enum CommandRule {
    Deny(String),
    VariableAssignment,
}


impl CommandRule {
    /// Validate raw argv (command name followed by operands) without a host callback.
    pub fn validate_arguments(&self, args: &[ShellBytes]) -> Result<(), String> {
        match self {
            Self::Deny(message) => Err(message.clone()),
            Self::VariableAssignment if args.get(1).is_some_and(|name| crate::var::is_shell_name_bytes(name.as_bytes()).is_none()) => {
                let name = args.first().map(ShellBytes::to_shell_string).unwrap_or_default();
                Err(format!("{name}: expected a variable name"))
            }
            Self::VariableAssignment => Ok(()),
        }
    }
}


/// Required process placement for an expanded command name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandPlacement {
    /// Run only in the process that created the `Shell`.
    ParentProcess,
}

/// How a registered command is reported by command lookup builtins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandIdentity {
    /// Report the command as a shell builtin.
    Builtin,
}

thread_local! {
    static EXTERNAL_COMMAND_CWD: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

struct ExternalCwdGuard(Option<PathBuf>);

impl ExternalCwdGuard {
    fn enter(cwd: PathBuf) -> Self {
        Self(EXTERNAL_COMMAND_CWD.with(|current| current.borrow_mut().replace(cwd)))
    }
}

impl Drop for ExternalCwdGuard {
    fn drop(&mut self) {
        EXTERNAL_COMMAND_CWD.with(|current| *current.borrow_mut() = self.0.take());
    }
}

/// Working directory of the command currently being passed to the external
/// handler, or `None` outside a handler call.
///
/// `cd` changes only the shell's own working directory, not the process's, so
/// a handler that spawns or resolves paths itself needs this to behave like
/// `eval_external`, which runs children in the shell's directory.
pub fn external_command_cwd() -> Option<PathBuf> {
    EXTERNAL_COMMAND_CWD.with(|cwd| cwd.borrow().clone())
}

/// How a writable redirection opens its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirectOpenMode {
    /// `>` and `>|`: write-only, create, truncate.
    Truncate,
    /// `>>`: write-only, create, append.
    Append,
    /// `<>`: read-write, create, no truncation.
    ReadWrite,
}

/// A writable redirection about to be opened.
#[derive(Debug)]
pub struct RedirectOpen<'a> {
    /// Target path, resolved against the shell's working directory.
    pub path: &'a Path,
    pub mode: RedirectOpenMode,
    /// Descriptor number the opened file will be duplicated onto.
    pub fd: RawFd,
}

/// Callback that owns the `open(2)` of writable redirections.
///
/// Return `None` to use epsh's default open, or the descriptor the provider
/// opened itself. The provider must honor `mode` exactly (creation, truncation,
/// append) because epsh only duplicates the returned descriptor onto
/// [`RedirectOpen::fd`]. Errors are reported as `<path>: <error>` like default
/// open failures. Input redirections, descriptor duplication, and heredocs do
/// not call the provider.
pub type RedirectOpenHandler =
    Box<dyn FnMut(&RedirectOpen<'_>) -> Option<std::io::Result<std::os::fd::OwnedFd>> + Send>;

/// A POSIX shell interpreter instance.
///
/// Each `Shell` maintains its own variable scope, working directory, function
/// definitions, and options — safe for concurrent use across threads (one Shell
/// per thread; Shell itself is not Sync).
///
/// # Quick start
/// ```no_run
/// use epsh::eval::Shell;
/// let mut shell = Shell::new();
/// let exit_code = shell.run_script("echo hello");
/// ```
///
/// # Builder pattern
/// ```no_run
/// use epsh::eval::Shell;
/// use std::path::PathBuf;
/// let mut shell = Shell::builder()
///     .cwd(PathBuf::from("/project"))
///     .errexit(true)
///     .build();
/// ```
pub struct Shell {
    pub(crate) vars: Variables,
    pub(crate) functions: FxHashMap<String, Command>,
    pub(crate) exit_status: ExitStatus,
    pub(crate) pid: u32,
    pub(crate) cwd: PathBuf,
    pub(crate) cwd_is_canonical: bool,
    pub(crate) loop_depth: usize,
    pub(crate) opts: ShellOpts,
    pub(crate) traps: FxHashMap<String, String>,
    pub(crate) tested: bool,
    pub(crate) in_forked_child: bool,
    owner_pid: u32,
    job_owner: Arc<()>,
    pending_job: Option<ForegroundJob>,
    terminal_descriptor: Option<HeldDescriptor>,
    command_placements: FxHashMap<Vec<u8>, CommandPlacement>,
    command_identities: FxHashMap<Vec<u8>, CommandIdentity>,
    placement_context: PlacementContext,
    pub(crate) ev_exit: bool,
    cancel: Option<Arc<AtomicBool>>,
    pub(crate) stdout_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    pub(crate) stderr_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    pub(crate) descriptor_sinks: FxHashMap<RawFd, OutputSink>,
    pub(crate) child_pids: Vec<i32>,
    child_groups: FxHashMap<i32, i32>,
    child_relays: FxHashMap<i32, ChildRelays>,
    child_controls: FxHashMap<i32, HeldDescriptor>,
    callback_reporter: Option<HeldDescriptor>,
    pub(crate) private_descriptors: Vec<std::sync::Weak<Mutex<Option<OwnedFd>>>>,
    timeout: Option<std::time::Instant>,
    external_handler: Option<Registered<ExternalHandler>>,
    external_dispatcher: Option<Registered<ExternalDispatcher>>,
    external_dispatch_names: Vec<ShellBytes>,
    command_rules: FxHashMap<Vec<u8>, CommandRule>,
    builtin_allowlist: Option<Vec<ShellBytes>>,
    command_policy: Option<Registered<CommandPolicy>>,
    command_argv_policy: Option<Registered<CommandArgvPolicy>>,
    pub(crate) redirect_open_handler: Option<Registered<RedirectOpenHandler>>,
    last_bg_pid: Option<u32>,
    substitution_count: u64,
    pub(crate) pending_stdin: Option<usize>,
    pub(crate) redirection_fds: Vec<Option<HeldDescriptor>>,
}

#[derive(Clone, Copy, Default)]
struct PlacementContext {
    pipeline_depth: usize,
    command_substitution_depth: usize,
}

/// Shell option flags (`set -e`, `set -u`, etc.).
#[derive(Debug, Default)]
pub struct ShellOpts {
    /// -e: exit on error
    pub errexit: bool,
    /// -u: treat unset variables as error
    pub nounset: bool,
    /// -x: print commands before execution (xtrace)
    pub xtrace: bool,
    pub pipefail: bool,
    /// -f: disable pathname expansion (globbing)
    pub noglob: bool,
    /// -n: parse but do not execute commands (syntax check)
    pub noexec: bool,
    /// Interactive mode: tcsetpgrp for terminal control, WUNTRACED for job control.
    pub interactive: bool,
}

impl ShellOpts {
    /// Return the POSIX `$-` flags string (e.g. "eux").
    pub fn flags_string(&self) -> String {
        let mut s = String::new();
        if self.errexit {
            s.push('e');
        }
        if self.nounset {
            s.push('u');
        }
        if self.xtrace {
            s.push('x');
        }
        if self.noglob {
            s.push('f');
        }
        if self.noexec {
            s.push('n');
        }
        if self.interactive {
            s.push('i');
        }
        s
    }
}

impl expand::ShellExpand for Shell {
    fn vars(&self) -> &Variables {
        &self.vars
    }
    fn vars_mut(&mut self) -> &mut Variables {
        &mut self.vars
    }
    fn exit_status(&self) -> ExitStatus {
        self.exit_status
    }
    fn pid(&self) -> u32 {
        self.pid
    }
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    fn shell_flags(&self) -> String {
        self.opts.flags_string()
    }
    fn last_bg_pid(&self) -> Option<u32> {
        self.last_bg_pid
    }
    fn nounset(&self) -> bool {
        self.opts.nounset
    }
    fn noglob(&self) -> bool {
        self.opts.noglob
    }
    fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String> {
        self.command_subst(cmd)
    }
}

/// Builder for configuring a [`Shell`] instance.
///
/// Use [`Shell::builder()`] to configure options, then call `.build()`.
/// All settings have sensible defaults — you only need to set what you want.
pub struct ShellBuilder {
    cwd: Option<PathBuf>,
    errexit: bool,
    nounset: bool,
    xtrace: bool,
    pipefail: bool,
    noglob: bool,
    noexec: bool,
    interactive: bool,
    cancel: Option<Arc<AtomicBool>>,
    stdout_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    stderr_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    timeout: Option<std::time::Duration>,
    environment: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
    external_handler: Option<Registered<ExternalHandler>>,
    external_dispatcher: Option<Registered<ExternalDispatcher>>,
    external_dispatch_names: Vec<ShellBytes>,
    command_rules: FxHashMap<Vec<u8>, CommandRule>,
    builtin_allowlist: Option<Vec<ShellBytes>>,
    command_policy: Option<Registered<CommandPolicy>>,
    command_argv_policy: Option<Registered<CommandArgvPolicy>>,
    redirect_open_handler: Option<Registered<RedirectOpenHandler>>,
}

impl Default for ShellBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellBuilder {
    pub fn new() -> Self {
        ShellBuilder {
            cwd: None,
            errexit: false,
            nounset: false,
            xtrace: false,
            pipefail: false,
            noglob: false,
            noexec: false,
            interactive: false,
            cancel: None,
            stdout_sink: None,
            stderr_sink: None,
            timeout: None,
            environment: None,
            external_handler: None,
            external_dispatcher: None,
            external_dispatch_names: Vec::new(),
            command_rules: FxHashMap::default(),
            builtin_allowlist: None,
            command_policy: None,
            command_argv_policy: None,
            redirect_open_handler: None,
        }
    }

    pub fn cwd(mut self, path: PathBuf) -> Self {
        self.cwd = Some(path);
        self
    }
    pub fn errexit(mut self, v: bool) -> Self {
        self.errexit = v;
        self
    }
    pub fn nounset(mut self, v: bool) -> Self {
        self.nounset = v;
        self
    }
    pub fn xtrace(mut self, v: bool) -> Self {
        self.xtrace = v;
        self
    }
    pub fn pipefail(mut self, v: bool) -> Self {
        self.pipefail = v;
        self
    }
    /// Disable pathname expansion (globbing).
    pub fn noglob(mut self, v: bool) -> Self {
        self.noglob = v;
        self
    }
    /// Parse but do not execute commands. Useful for syntax validation.
    pub fn noexec(mut self, v: bool) -> Self {
        self.noexec = v;
        self
    }
    /// Enable interactive mode (tcsetpgrp, WUNTRACED for job control).
    pub fn interactive(mut self, v: bool) -> Self {
        self.interactive = v;
        self
    }
    pub fn cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }
    pub fn stdout_sink(mut self, sink: Arc<Mutex<dyn Write + Send>>) -> Self {
        self.stdout_sink = Some(sink);
        self
    }
    pub fn stderr_sink(mut self, sink: Arc<Mutex<dyn Write + Send>>) -> Self {
        self.stderr_sink = Some(sink);
        self
    }
    pub fn timeout(mut self, duration: std::time::Duration) -> Self {
        self.timeout = Some(duration);
        self
    }
    /// Don't inherit process environment variables.
    pub fn env_clear(mut self) -> Self {
        self.environment = Some(Vec::new());
        self
    }
    /// Replace inherited environment input with owned, byte-preserving OS entries.
    pub fn environment(mut self, entries: Vec<(std::ffi::OsString, std::ffi::OsString)>) -> Self {
        self.environment = Some(entries);
        self
    }
    /// Set a callback for external command execution.
    pub fn external_handler(mut self, handler: ExternalHandler) -> Self {
        self.external_handler = Some(Registered::new(handler));
        self
    }
    /// Set a callback that can refuse expanded command targets. Without a callback, all targets are allowed.
    pub fn command_policy(mut self, policy: CommandPolicy) -> Self {
        self.command_policy = Some(Registered::new(policy));
        self
    }
    /// Set a provider that opens writable redirection targets.
    pub fn command_argv_policy(mut self, policy: CommandArgvPolicy) -> Self {
        self.command_argv_policy = Some(Registered::new(policy));
        self
    }

    pub fn redirect_open_handler(mut self, handler: RedirectOpenHandler) -> Self {
        self.redirect_open_handler = Some(Registered::new(handler));
        self
    }

    pub fn build(self) -> Shell {
        let vars = match self.environment {
            Some(entries) => Variables::from_environment(entries),
            None => Variables::new(),
        };
        let (cwd, cwd_is_canonical) = match self.cwd {
            Some(cwd) => (cwd, false),
            None => (
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
                true,
            ),
        };
        Shell {
            vars,
            functions: FxHashMap::default(),
            exit_status: ExitStatus::SUCCESS,
            pid: std::process::id(),
            cwd,
            cwd_is_canonical,
            loop_depth: 0,
            opts: ShellOpts {
                errexit: self.errexit,
                nounset: self.nounset,
                xtrace: self.xtrace,
                pipefail: self.pipefail,
                noglob: self.noglob,
                noexec: self.noexec,
                interactive: self.interactive,
            },
            traps: FxHashMap::default(),
            tested: false,
            in_forked_child: false,
            owner_pid: std::process::id(),
            job_owner: Arc::new(()),
            pending_job: None,
            terminal_descriptor: None,
            command_placements: FxHashMap::default(),
            command_identities: FxHashMap::default(),
            placement_context: PlacementContext::default(),
            ev_exit: false,
            cancel: self.cancel,
            stdout_sink: self.stdout_sink,
            stderr_sink: self.stderr_sink,
            child_pids: Vec::new(),
            descriptor_sinks: FxHashMap::default(),
            child_groups: FxHashMap::default(),
            child_relays: FxHashMap::default(),
            child_controls: FxHashMap::default(),
            callback_reporter: None,
            private_descriptors: Vec::new(),
            timeout: self.timeout.map(|d| std::time::Instant::now() + d),
            external_handler: self.external_handler,
            external_dispatcher: self.external_dispatcher,
            external_dispatch_names: self.external_dispatch_names,
            command_rules: self.command_rules,
            builtin_allowlist: self.builtin_allowlist,
            command_policy: self.command_policy,
            command_argv_policy: self.command_argv_policy,
            redirect_open_handler: self.redirect_open_handler,
            last_bg_pid: None,
            substitution_count: 0,
            pending_stdin: None,
            redirection_fds: Vec::new(),
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        if std::process::id() != self.owner_pid { return; }
        self.kill_children();
        let orphaned_relays: Vec<_> = self.child_relays.keys().copied().collect();
        for pid in orphaned_relays { self.discard_child_relays(pid); }
    }
}

impl Shell {
    pub(crate) fn output_sink(&self, fd: RawFd) -> Option<OutputSink> {
        match fd {
            1 => self.stdout_sink.clone(),
            2 => self.stderr_sink.clone(),
            _ => self.descriptor_sinks.get(&fd).cloned(),
        }
    }

    pub(crate) fn set_output_sink(&mut self, fd: RawFd, sink: Option<OutputSink>) {
        match fd {
            1 => self.stdout_sink = sink,
            2 => self.stderr_sink = sink,
            _ => match sink {
                Some(sink) => { self.descriptor_sinks.insert(fd, sink); }
                None => { self.descriptor_sinks.remove(&fd); }
            },
        }
    }

    pub(crate) fn ensure_terminal_descriptor(&mut self) -> crate::error::Result<()> {
        if !self.opts.interactive || self.in_forked_child || self.terminal_descriptor.is_some() { return Ok(()); }
        let descriptor = sys::duplicate_fd_above(0, 3)?;
        if !rustix::termios::isatty(&descriptor) { return Ok(()); }
        let descriptor = Arc::new(Mutex::new(Some(descriptor)));
        self.private_descriptors.push(Arc::downgrade(&descriptor));
        self.terminal_descriptor = Some(descriptor);
        Ok(())
    }

    fn terminal_raw_fd(&self) -> RawFd {
        self.terminal_descriptor.as_ref().and_then(|descriptor| descriptor.lock().unwrap().as_ref().map(AsRawFd::as_raw_fd)).unwrap_or(0)
    }

    fn fork_outputs(&self, capture_stdout: bool) -> crate::error::Result<Vec<ForkOutput>> {
        let mut outputs = Vec::new();
        let mut targets: Vec<_> = self.descriptor_sinks.keys().copied().collect();
        targets.extend([1, 2]);
        for target in targets {
            if target == 1 && !capture_stdout { continue; }
            if let Some(sink) = self.output_sink(target) {
                let (read, write) = sys::pipe_owned()?;
                outputs.push(ForkOutput { target, read, write, sink });
            }
        }
        Ok(outputs)
    }

    fn enter_forked_child(&mut self, outputs: &[ForkOutput], pgid: i32) -> std::io::Result<()> {
        rustix::process::setpgid(None, rustix::process::Pid::from_raw(pgid))?;
        self.in_forked_child = true;
        self.terminal_descriptor = None;
        self.child_pids.clear();
        self.child_groups.clear();
        let inherited_readers: Vec<_> = self.child_relays.values().flat_map(|relays| relays.descriptors.iter().copied()).collect();
        for index in inherited_readers {
            if let Some(descriptor) = self.redirection_fds[index].take() {
                drop(descriptor.lock().unwrap().take());
            }
        }
        self.child_relays.clear();
        if let Some(fd) = self.pending_stdin_fd() {
            sys::dup2_result(fd, 0)?;
            self.pending_stdin = None;
        }
        self.stdout_sink = None;
        self.stderr_sink = None;
        self.descriptor_sinks.clear();
        for output in outputs {
            sys::dup2_result(output.write.as_raw_fd(), output.target)?;
        }
        for output in outputs {
            sys::close(output.read.as_raw_fd())?;
            sys::close(output.write.as_raw_fd())?;
        }
        Ok(())
    }

    fn start_relay(&mut self, stream: OwnedFd, sink: OutputSink) -> std::io::Result<(std::thread::JoinHandle<std::io::Result<()>>, usize)> {
        let flags = rustix::fs::fcntl_getfl(&stream)?;
        rustix::fs::fcntl_setfl(&stream, flags | rustix::fs::OFlags::NONBLOCK)?;
        let stream = Arc::new(Mutex::new(Some(stream)));
        let index = self.redirection_fds.len();
        self.redirection_fds.push(Some(stream.clone()));
        Ok((spawn_relay(stream, sink), index))
    }

    fn relay_fork_outputs(&mut self, pid: i32, outputs: Vec<ForkOutput>, children: &[i32]) -> crate::error::Result<()> {
        let streams = outputs.into_iter().map(|output| {
            drop(output.write);
            (output.read, output.sink)
        }).collect();
        self.relay_streams(pid, streams, children)
    }

    fn relay_streams(&mut self, pid: i32, streams: Vec<(OwnedFd, OutputSink)>, children: &[i32]) -> crate::error::Result<()> {
        self.child_relays.insert(pid, ChildRelays { handles: Vec::new(), descriptors: Vec::new() });
        for (stream, sink) in streams {
            let (handle, index) = match self.start_relay(stream, sink) {
                Ok(relay) => relay,
                Err(error) => {
                    self.kill_owned_children(children);
                    return Err(error.into());
                }
            };
            let relays = self.child_relays.get_mut(&pid).unwrap();
            relays.handles.push(handle);
            relays.descriptors.push(index);
        }
        Ok(())
    }

    fn fork_evaluator(&mut self) -> crate::error::Result<i32> {
        let (control_read, control_write) = sys::pipe_owned()?;
        let stdout = std::io::stdout();
        let mut stdout_guard = stdout.lock();
        stdout_guard.flush()?;
        self.private_descriptors.retain(|descriptor| descriptor.strong_count() > 0);
        let private: Vec<_> = self.private_descriptors.iter().filter_map(std::sync::Weak::upgrade).collect();
        let private_guards: Vec<_> = private.iter().map(|descriptor| descriptor.lock().unwrap()).collect();
        let guards: Vec<_> = self.redirection_fds.iter().flatten().map(|descriptor| descriptor.lock().unwrap()).collect();
        // SAFETY: stdout is flushed and locked, and descriptor readers are paused; both processes release copied guards before evaluating.
        let pid = unsafe { sys::fork() };
        drop(guards);
        drop(private_guards);
        drop(private);
        drop(stdout_guard);
        if pid == 0 {
            self.private_descriptors.clear();
            let reporter = Arc::new(Mutex::new(Some(control_write)));
            self.private_descriptors.push(Arc::downgrade(&reporter));
            self.callback_reporter = Some(reporter);
            self.child_controls.clear();
            drop(control_read);
        } else if pid > 0 {
            let reader = Arc::new(Mutex::new(Some(control_read)));
            self.private_descriptors.push(Arc::downgrade(&reader));
            self.child_controls.insert(pid, reader);
        }
        Ok(pid)
    }

    fn report_child_refusal(&self, error: &ShellError) {
        let tag = match error {
            ShellError::CallbackProcess { kind: CallbackKind::External } => 1,
            ShellError::CallbackProcess { kind: CallbackKind::CommandPolicy } => 2,
            ShellError::CallbackProcess { kind: CallbackKind::RedirectOpen } => 3,
            ShellError::CommandRefused { .. } => 4,
            _ => return,
        };
        if let Some(reporter) = &self.callback_reporter {
            let held = reporter.lock().unwrap();
            let Some(writer) = held.as_ref() else { return; };
            while let Err(rustix::io::Errno::INTR) = rustix::io::write(writer, &[tag]) {}
        }
    }

    fn read_child_refusal(descriptor: &HeldDescriptor) -> crate::error::Result<()> {
        let held = descriptor.lock().unwrap();
        let reader = held.as_ref().ok_or_else(|| ShellError::Io(std::io::Error::other("callback control channel was closed")))?;
        let mut tag = [0u8];
        let length = loop {
            match rustix::io::read(reader, &mut tag) {
                Err(rustix::io::Errno::INTR) => continue,
                result => break result.map_err(std::io::Error::from)?,
            }
        };
        if length == 0 { return Ok(()); }
        let kind = match tag[0] {
            1 => CallbackKind::External,
            2 => CallbackKind::CommandPolicy,
            3 => CallbackKind::RedirectOpen,
            4 => return Err(ShellError::CommandRefused { diagnostic: "command refused by policy in child evaluator".to_owned(), span: Span::default() }),
            _ => return Err(ShellError::Io(std::io::Error::other("invalid child callback refusal"))),
        };
        Err(ShellError::CallbackProcess { kind })
    }

    fn take_child_refusal(&mut self, pid: i32) -> crate::error::Result<()> {
        if let Some(reader) = self.child_controls.remove(&pid) { Self::read_child_refusal(&reader)?; }
        Ok(())
    }

    fn track_child(&mut self, pid: i32, pgid: i32) {
        let _ = rustix::process::setpgid(rustix::process::Pid::from_raw(pid), rustix::process::Pid::from_raw(pgid));
        self.child_pids.push(pid);
        self.child_groups.insert(pid, pgid);
    }

    fn child_group(&self, pid: i32) -> i32 {
        self.child_groups.get(&pid).copied().unwrap_or(pid)
    }

    fn terminate_group(&self, pgid: i32) {
        if let Some(group) = rustix::process::Pid::from_raw(pgid) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }

    fn forget_child(&mut self, pid: i32) {
        self.child_pids.retain(|&child| child != pid);
        self.child_groups.remove(&pid);
    }

    fn finish_child_relays(&mut self, pid: i32) -> crate::error::Result<()> {
        let Some(relays) = self.child_relays.remove(&pid) else { return Ok(()); };
        let mut interruption = None;
        while relays.handles.iter().any(|handle| !handle.is_finished()) {
            if interruption.is_none() && let Err(error) = self.check_cancel() {
                self.terminate_group(self.child_group(pid));
                interruption = Some(error);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut failure = None;
        for handle in relays.handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failure = Some(ShellError::Io(error)),
                Err(_) => failure = Some(ShellError::Io(std::io::Error::other("output relay panicked"))),
            }
        }
        for index in relays.descriptors { self.redirection_fds[index].take(); }
        self.trim_redirection_fds();
        interruption.or(failure).map_or(Ok(()), Err)
    }

    fn discard_child_relays(&mut self, pid: i32) {
        if let Some(relays) = self.child_relays.remove(&pid) {
            for &index in &relays.descriptors {
                if let Some(descriptor) = &self.redirection_fds[index] {
                    drop(descriptor.lock().unwrap().take());
                }
            }
            for handle in relays.handles { let _ = handle.join(); }
            for index in relays.descriptors { self.redirection_fds[index].take(); }
            self.trim_redirection_fds();
        }
    }

    pub(crate) fn wait_owned_child(&mut self, pid: i32) -> crate::error::Result<ExitStatus> {
        if !self.child_pids.contains(&pid) {
            return Err(ShellError::Io(std::io::Error::from_raw_os_error(libc::ECHILD)));
        }
        self.wait_child_pgid(pid, self.child_group(pid))
    }

    /// Create a [`ShellBuilder`] for configuring a new shell instance.
    pub fn builder() -> ShellBuilder {
        ShellBuilder::new()
    }

    /// Create a new shell with default settings and inherited environment.
    pub fn new() -> Self {
        Shell {
            vars: Variables::new(),
            functions: FxHashMap::default(),
            exit_status: ExitStatus::SUCCESS,
            pid: std::process::id(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            cwd_is_canonical: true,
            loop_depth: 0,
            opts: ShellOpts::default(),
            traps: FxHashMap::default(),
            tested: false,
            in_forked_child: false,
            owner_pid: std::process::id(),
            job_owner: Arc::new(()),
            pending_job: None,
            terminal_descriptor: None,
            command_placements: FxHashMap::default(),
            command_identities: FxHashMap::default(),
            placement_context: PlacementContext::default(),
            ev_exit: false,
            cancel: None,
            stdout_sink: None,
            stderr_sink: None,
            child_pids: Vec::new(),
            descriptor_sinks: FxHashMap::default(),
            child_groups: FxHashMap::default(),
            child_relays: FxHashMap::default(),
            child_controls: FxHashMap::default(),
            callback_reporter: None,
            private_descriptors: Vec::new(),
            timeout: None,
            external_handler: None,
            external_dispatcher: None,
            external_dispatch_names: Vec::new(),
            command_rules: FxHashMap::default(),
            builtin_allowlist: None,
            command_policy: None,
            command_argv_policy: None,
            redirect_open_handler: None,
            last_bg_pid: None,
            substitution_count: 0,
            pending_stdin: None,
            redirection_fds: Vec::new(),
        }
    }

    /// Set a cancellation flag. When the flag is set to true, the shell
    /// will abort execution at the next check point with `ShellError::Cancelled`.
    pub fn set_cancel_flag(&mut self, flag: Arc<AtomicBool>) {
        self.cancel = Some(flag);
    }

    /// Set an execution timeout. The shell will abort with `ShellError::TimedOut`
    /// when the deadline is exceeded, checked at the same points as cancellation.
    pub fn set_timeout(&mut self, duration: std::time::Duration) {
        self.timeout = Some(std::time::Instant::now() + duration);
    }

    /// Variable storage.
    pub fn vars(&self) -> &Variables {
        &self.vars
    }
    /// Mutable variable storage.
    pub fn vars_mut(&mut self) -> &mut Variables {
        &mut self.vars
    }
    /// Defined functions.
    pub fn functions(&self) -> &FxHashMap<String, Command> {
        &self.functions
    }
    /// Last command's exit status (`$?`).
    pub fn exit_status(&self) -> ExitStatus {
        self.exit_status
    }

    /// Set the status exposed by `$?` after a command executed by the embedder.
    pub fn set_exit_status(&mut self, status: ExitStatus) {
        self.exit_status = status;
    }
    /// Shell's PID (`$$`).
    pub fn pid(&self) -> u32 {
        self.pid
    }
    /// Current working directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    /// Shell options.
    pub fn opts(&self) -> &ShellOpts {
        &self.opts
    }
    /// Mutable shell options.
    pub fn opts_mut(&mut self) -> &mut ShellOpts {
        &mut self.opts
    }
    /// Trap handlers.
    pub fn traps(&self) -> &FxHashMap<String, String> {
        &self.traps
    }

    pub(crate) fn check_command_policy(
        &mut self,
        target: &str,
        span: Span,
    ) -> crate::error::Result<()> {
        let Some(policy) = self.command_policy.as_mut() else {
            return Ok(());
        };
        let target = ShellBytes::from_str_lossless(target);
        if let Some(diagnostic) = policy.get(CallbackKind::CommandPolicy)?(&target) {
            return Err(ShellError::CommandRefused { diagnostic, span });
        }
        Ok(())
    }

    pub(crate) fn check_command_policy_for_dispatch(
        &mut self,
        args: &[String],
        span: Span,
    ) -> crate::error::Result<()> {
        let mut args = args;
        loop {
            let Some(target) = args.first() else {
                return Ok(());
            };
            let target_bytes = ShellBytes::from_str_lossless(target);
            if let Some(allowed) = &self.builtin_allowlist
                && crate::builtins::is_builtin(target)
                && !allowed.iter().any(|name| name == &target_bytes)
            {
                return Err(ShellError::CommandRefused { diagnostic: format!("{target}: builtin is not available"), span });
            }
            if let Some(rule) = self.command_rules.get(target_bytes.as_bytes()) {
                let argv: Vec<_> = args.iter().map(|arg| ShellBytes::from_str_lossless(arg)).collect();
                if let Err(diagnostic) = rule.validate_arguments(&argv) { return Err(ShellError::CommandRefused { diagnostic, span }); }
            }
            self.check_command_policy(target, span)?;
            if let Some(policy) = self.command_argv_policy.as_mut() {
                let callback = policy.get(CallbackKind::CommandPolicy)?;
                let argv: Vec<_> = args.iter().map(|arg| ShellBytes::from_str_lossless(arg)).collect();
                if let Some(diagnostic) = callback(&argv) { return Err(ShellError::CommandRefused { diagnostic, span }); }
            }
            if target != "command" {
                return Ok(());
            }
            match crate::builtins::command_action(args) {
                crate::builtins::CommandAction::Execute(nested_args) => args = nested_args,
                crate::builtins::CommandAction::Empty
                | crate::builtins::CommandAction::Describe { .. } => return Ok(()),
            }
        }
    }

    pub(crate) fn parent_process_refusal(&self, args: &[String]) -> Option<String> {
        let mut dispatch_args = args;
        loop {
            let target = dispatch_args.first()?;
            let target_bytes = ShellBytes::from_str_lossless(target);
            if self
                .command_placements
                .get(target_bytes.as_bytes())
                .is_some_and(|placement| *placement == CommandPlacement::ParentProcess)
                && (std::process::id() != self.owner_pid
                    || self.placement_context.pipeline_depth > 0
                    || self.placement_context.command_substitution_depth > 0)
            {
                return Some(format!(
                    "{} requires the original shell process and cannot run in a forked context",
                    escape_command_name(target_bytes.as_bytes())
                ));
            }
            if target != "command" {
                return None;
            }
            match crate::builtins::command_action(dispatch_args) {
                crate::builtins::CommandAction::Execute(nested_args) => {
                    dispatch_args = nested_args;
                }
                crate::builtins::CommandAction::Empty
                | crate::builtins::CommandAction::Describe { .. } => return None,
            }
        }
    }

    pub(crate) fn command_lookup_available(&self, name: &str) -> bool {
        self.command_identity(name).is_some()
            || !crate::builtins::is_builtin(name)
            || self.builtin_allowlist.as_ref().is_none_or(|allowed| allowed.iter().any(|candidate| candidate == &ShellBytes::from_str_lossless(name)))
    }

    pub(crate) fn command_identity(&self, name: &str) -> Option<CommandIdentity> {
        let name = ShellBytes::from_str_lossless(name);
        if name.as_bytes().contains(&b'/') {
            return None;
        }
        self.command_identities.get(name.as_bytes()).copied()
    }

    fn check_cancel(&self) -> crate::error::Result<()> {
        if let Some(ref flag) = self.cancel
            && flag.load(Ordering::Relaxed)
        {
            return Err(ShellError::Cancelled);
        }
        if let Some(deadline) = self.timeout
            && std::time::Instant::now() >= deadline
        {
            return Err(ShellError::TimedOut);
        }
        Ok(())
    }

    /// Set a stdout sink. Builtin output will be written here instead of fd 1.
    pub fn set_stdout_sink(&mut self, sink: Arc<Mutex<dyn Write + Send>>) {
        self.stdout_sink = Some(sink);
    }

    /// Set a stderr sink. Error output will be written here instead of fd 2.
    pub fn set_stderr_sink(&mut self, sink: Arc<Mutex<dyn Write + Send>>) {
        self.stderr_sink = Some(sink);
    }

    pub(crate) fn write_out(&self, s: &str) {
        let data = crate::encoding::str_to_bytes(s);
        if let Some(ref sink) = self.stdout_sink {
            if let Ok(mut w) = sink.lock() {
                let _ = w.write_all(&data);
            }
        } else {
            sys::write(1, &data);
        }
    }

    pub(crate) fn write_err(&self, s: &str) {
        let data = crate::encoding::str_to_bytes(s);
        if let Some(ref sink) = self.stderr_sink {
            if let Ok(mut w) = sink.lock() {
                let _ = w.write_all(&data);
            }
        } else {
            sys::write(2, &data);
        }
    }

    pub(crate) fn err_msg(&self, msg: &str) {
        self.write_err(msg);
        if !msg.ends_with('\n') {
            self.write_err("\n");
        }
    }

    fn wait_child(&mut self, pid: i32) -> crate::error::Result<ExitStatus> {
        self.wait_owned_child(pid)
    }

    fn wait_child_pgid(&mut self, pid: i32, pgid: i32) -> crate::error::Result<ExitStatus> {
        let mut options = rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT;
        if self.opts.interactive { options |= rustix::process::WaitIdOptions::STOPPED; }
        if self.cancel.is_some() || self.timeout.is_some() { options |= rustix::process::WaitIdOptions::NOHANG; }
        let child = rustix::process::Pid::from_raw(pid).unwrap();
        let mut interruption = None;
        loop {
            match rustix::process::waitid(rustix::process::WaitId::Pid(child), options) {
                Ok(Some(status)) if status.stopped() => {
                    match rustix::process::waitid(rustix::process::WaitId::Pid(child), rustix::process::WaitIdOptions::STOPPED | rustix::process::WaitIdOptions::NOHANG) {
                        Ok(Some(_)) => return Err(ShellError::Stopped { pid, pgid }),
                        Ok(None) | Err(rustix::io::Errno::INTR) => continue,
                        Err(error) => return Err(ShellError::Io(error.into())),
                    }
                }
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => {
                    self.discard_child_relays(pid);
                    self.forget_child(pid);
                    self.child_controls.remove(&pid);
                    return Err(ShellError::Io(error.into()));
                }
            }
            if interruption.is_none() && let Err(error) = self.check_cancel() {
                self.terminate_group(pgid);
                interruption = Some(error);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let relays = self.finish_child_relays(pid);
        let waited = loop {
            match rustix::process::waitpid(Some(child), rustix::process::WaitOptions::empty()) {
                Err(rustix::io::Errno::INTR) => continue,
                result => break result,
            }
        };
        self.forget_child(pid);
        self.take_child_refusal(pid)?;
        if let Some(error) = interruption { return Err(error); }
        relays?;
        self.check_cancel()?;
        let (_, status) = waited.map_err(std::io::Error::from)?.ok_or_else(|| ShellError::Io(std::io::Error::other("child exited without a wait status")))?;
        Ok(ExitStatus::from_wait(status.as_raw()))
    }

    fn kill_children(&mut self) {
        let children = std::mem::take(&mut self.child_pids);
        self.kill_owned_children(&children);
    }

    fn kill_owned_children(&mut self, children: &[i32]) {
        let mut owned_children = Vec::new();
        for &pid in children {
            self.child_controls.remove(&pid);
            let observation = loop {
                match rustix::process::waitid(rustix::process::WaitId::Pid(rustix::process::Pid::from_raw(pid).unwrap()), rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT | rustix::process::WaitIdOptions::NOHANG) {
                    Err(rustix::io::Errno::INTR) => continue,
                    result => break result,
                }
            };
            if observation.is_ok() {
                let pgid = self.child_group(pid);
                if pgid == rustix::process::getpgrp().as_raw_pid() {
                    let _ = rustix::process::kill_process(rustix::process::Pid::from_raw(pid).unwrap(), rustix::process::Signal::KILL);
                } else {
                    self.terminate_group(pgid);
                }
                owned_children.push(pid);
            } else {
                self.discard_child_relays(pid);
                self.forget_child(pid);
            }
        }
        for pid in owned_children {
            let _ = self.finish_child_relays(pid);
            while let Err(rustix::io::Errno::INTR) = rustix::process::waitpid(rustix::process::Pid::from_raw(pid), rustix::process::WaitOptions::empty()) {}
            self.forget_child(pid);
        }
    }

    fn xtrace_prefix(&self) -> String {
        self.vars.get_shell("PS4").unwrap_or_else(|| "+ ".into())
    }

    /// Set a callback that replaces the default external command execution.
    ///
    /// When set, the handler is called instead of `eval_external` for commands
    /// that are not builtins or functions. Redirections are already applied to
    /// fds before the handler runs. The handler receives:
    /// - `argv`: expanded arguments (`argv[0]` is the command name)
    /// - `env`: the complete child environment as raw byte pairs, including
    ///   inherited non-shell-name entries, exported variables, and prefix
    ///   assignments
    pub fn set_external_handler(&mut self, handler: ExternalHandler) {
        self.external_handler = Some(Registered::new(handler));
    }

    /// Register a process-bound external dispatcher for an owned set of raw names.
    pub fn set_external_dispatcher(&mut self, names: Vec<ShellBytes>, dispatcher: ExternalDispatcher) {
        self.external_dispatch_names = names;
        self.external_dispatcher = Some(Registered::new(dispatcher));
    }

    /// Register an external handler audited for execution in inherited child processes.
    ///
    /// # Safety
    /// Every reachable operation, including destructors and error paths, must be valid
    /// after every possible fork. Inherited host locks, runtime services, environment
    /// mutation, and dependencies on vanished threads are prohibited. A function pointer, immutable capture,
    /// or Send bound alone does not establish this contract. Child mutations remain child-local.
    pub unsafe fn set_external_handler_in_children(&mut self, handler: ExternalHandler) {
        // SAFETY: the caller accepts the complete child-execution contract above.
        self.external_handler = Some(unsafe { Registered::in_children(handler) });
    }

    /// Register a command policy audited for inherited child execution.
    ///
    /// # Safety
    /// The complete operation and destructor contract of
    /// [`Self::set_external_handler_in_children`] applies to this policy.
    pub unsafe fn set_command_policy_in_children(&mut self, policy: CommandPolicy) {
        // SAFETY: the caller accepts the complete child-execution contract above.
        self.command_policy = Some(unsafe { Registered::in_children(policy) });
    }

    /// Register a redirection opener audited for inherited child execution.
    ///
    /// # Safety
    /// The complete operation and destructor contract of
    /// [`Self::set_external_handler_in_children`] applies to this opener.
    pub unsafe fn set_redirect_open_handler_in_children(&mut self, handler: RedirectOpenHandler) {
        // SAFETY: the caller accepts the complete child-execution contract above.
        self.redirect_open_handler = Some(unsafe { Registered::in_children(handler) });
    }

    /// Replace the process-bound callback that validates raw expanded argv.
    pub fn set_command_argv_policy(&mut self, policy: CommandArgvPolicy) {
        self.command_argv_policy = Some(Registered::new(policy));
    }

    /// Register an argv policy audited for inherited child execution.
    ///
    /// # Safety
    /// The complete operation and destructor contract of
    /// [`Self::set_external_handler_in_children`] applies to this policy.
    pub unsafe fn set_command_argv_policy_in_children(&mut self, policy: CommandArgvPolicy) {
        // SAFETY: the caller accepts the complete child-execution contract above.
        self.command_argv_policy = Some(unsafe { Registered::in_children(policy) });
    }

    /// Register a named external dispatcher audited for inherited child execution.
    ///
    /// # Safety
    /// The complete operation and destructor contract of
    /// [`Self::set_external_handler_in_children`] applies to this dispatcher.
    pub unsafe fn set_external_dispatcher_in_children(&mut self, names: Vec<ShellBytes>, dispatcher: ExternalDispatcher) {
        self.external_dispatch_names = names;
        // SAFETY: the caller accepts the complete child-execution contract above.
        self.external_dispatcher = Some(unsafe { Registered::in_children(dispatcher) });
    }

    /// Install owned validation for a command name without a callback.
    pub fn set_command_rule(&mut self, name: ShellBytes, rule: CommandRule) {
        self.command_rules.insert(name.as_bytes().to_vec(), rule);
    }

    /// Permit only the listed epsh builtins. External paths remain ordinary commands.
    pub fn set_builtin_allowlist(&mut self, names: Vec<ShellBytes>) {
        self.builtin_allowlist = Some(names);
    }

    /// Replace the process-bound callback that can refuse expanded targets.
    pub fn set_command_policy(&mut self, policy: CommandPolicy) {
        self.command_policy = Some(Registered::new(policy));
    }

    /// Require an expanded command name to run in the original shell process.
    /// Names are stored as owned raw bytes, matching the shell's command names.
    pub fn set_command_placement(
        &mut self,
        name: ShellBytes,
        placement: CommandPlacement,
    ) {
        self.command_placements.insert(name.into_vec(), placement);
    }

    /// Register how an expanded command name appears in `command -v`,
    /// `command -V`, and `type` lookup.
    ///
    /// The table owns the command-name bytes. Identity does not affect command
    /// dispatch or process placement. Names containing `/` are looked up as
    /// paths.
    pub fn set_command_identity(&mut self, name: ShellBytes, identity: CommandIdentity) {
        self.command_identities.insert(name.into_vec(), identity);
    }

    /// Install a provider that owns the open of writable redirections
    /// (`>`, `>|`, `>>`, `<>`). See [`RedirectOpenHandler`].
    pub fn set_redirect_open_handler(&mut self, handler: RedirectOpenHandler) {
        self.redirect_open_handler = Some(Registered::new(handler));
    }

    pub(crate) fn has_external_handler(&self, target: &str) -> bool {
        self.external_handler.is_some() || self.external_dispatch_names.iter().any(|name| name == &ShellBytes::from_str_lossless(target))
    }

    /// Set the shell's working directory.
    pub fn set_cwd(&mut self, dir: PathBuf) {
        self.cwd = dir;
        self.cwd_is_canonical = false;
    }

    pub(crate) fn resolve_cd_path(&self, path: &Path) -> std::io::Result<PathBuf> {
        if self.cwd_is_canonical && path.is_relative() && is_dot_navigation(path) {
            return Ok(apply_dot_navigation(&self.cwd, path));
        }

        let target = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        target.canonicalize()
    }

    /// Set shell arguments as raw shell bytes.
    pub fn set_args_bytes(&mut self, args: &[ShellBytes]) {
        if let Some(first) = args.first() {
            self.vars.arg0 = first.clone();
        }
        self.vars.positional = args.iter().skip(1).cloned().collect();
    }

    /// Resolve a path against the shell's working directory.
    /// Absolute paths are returned as-is; relative paths are joined with `self.cwd`.
    pub fn resolve_path(&self, p: &str) -> PathBuf {
        self.resolve_path_bytes(&ShellBytes::from(p))
    }

    /// Resolve a path against the shell's working directory from raw shell bytes.
    pub fn resolve_path_bytes(&self, p: &ShellBytes) -> PathBuf {
        let path = PathBuf::from(p.to_os_string());
        if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        }
    }

    /// Execute a shell script from source text.
    pub fn run_script(&mut self, source: &str) -> i32 {
        let mut parser = Parser::new(source);
        let program = match parser.parse() {
            Ok(p) => p,
            Err(e) => {
                self.err_msg(&format!("epsh: {e}"));
                self.exit_status = ExitStatus::MISUSE;
                return self.exit_status.code();
            }
        };

        self.run_program(&program).code()
    }

    /// Execute a pre-parsed program. Returns the final exit status.
    /// This is the library-facing entry point — parse once with `Parser`,
    /// then execute via `run_program`.
    pub fn run_program(&mut self, program: &crate::ast::Program) -> ExitStatus {
        match self.run_program_outcome(program) {
            ProgramOutcome::Completed(status) => status,
            ProgramOutcome::Stopped(job) => {
                drop(job);
                self.err_msg("epsh: numeric execution cannot retain a stopped job; terminated it");
                self.exit_status
            }
        }
    }

    /// Execute a program, transferring an owned foreground job when execution stops.
    /// The remainder of a stopped command list is discarded.
    pub fn run_program_outcome(&mut self, program: &crate::ast::Program) -> ProgramOutcome {
        for cmd in &program.commands {
            match self.eval_command(cmd) {
                Ok(status) => {
                    self.exit_status = status;
                }
                Err(ShellError::Exit(n)) => {
                    self.exit_status = n;
                    self.run_exit_trap();
                    return ProgramOutcome::Completed(self.exit_status);
                }
                Err(ShellError::Cancelled) | Err(ShellError::TimedOut) => {
                    self.exit_status = ExitStatus::from_signal(2);
                    self.kill_children();
                    return ProgramOutcome::Completed(self.exit_status);
                }
                Err(ShellError::Stopped { .. }) => {
                    self.exit_status = ExitStatus::from(148);
                    if let Some(job) = self.pending_job.take() { return ProgramOutcome::Stopped(job); }
                    return ProgramOutcome::Completed(self.exit_status);
                }
                Err(e) => {
                    self.err_msg(&format!("epsh: {e}"));
                    self.exit_status = ExitStatus::MISUSE;
                    self.run_exit_trap();
                    return ProgramOutcome::Completed(self.exit_status);
                }
            }
            self.run_pending_traps();
        }

        self.run_exit_trap();
        ProgramOutcome::Completed(self.exit_status)
    }

    fn eval_trap_program(&mut self, program: &Program) {
        for command in &program.commands {
            if let Err(error @ (ShellError::CallbackProcess { .. } | ShellError::CommandRefused { .. })) = self.eval_command(command) {
                self.report_child_refusal(&error);
                self.err_msg(&format!("epsh: {error}"));
                break;
            }
        }
    }

    fn run_pending_traps(&mut self) {
        for sig_name in crate::signal::take_pending() {
            if let Some(action) = self.traps.get(sig_name) {
                if action.is_empty() {
                    continue;
                }
                let action = action.clone();
                let mut parser = Parser::from_shell_string(&action);
                if let Ok(program) = parser.parse() {
                    self.eval_trap_program(&program);
                }
            }
        }
    }

    pub(crate) fn run_exit_trap(&mut self) {
        if let Some(action) = self.traps.remove("EXIT") {
            let status = self.exit_status;
            let mut parser = Parser::from_shell_string(&action);
            if let Ok(program) = parser.parse() {
                self.eval_trap_program(&program);
            }
            self.exit_status = status;
        }
    }

    /// Set shell arguments ($0, $1, $2, ...).
    pub fn set_args(&mut self, args: &[&str]) {
        let args: Vec<ShellBytes> = args.iter().map(|s| (*s).into()).collect();
        self.set_args_bytes(&args);
    }

    /// Set a variable. Returns error if the variable is readonly.
    pub fn set_var(&mut self, name: &str, value: &str) -> Result<(), String> {
        self.vars.set(name, value)
    }

    /// Set a variable from raw shell bytes.
    pub fn set_var_bytes(&mut self, name: &str, value: ShellBytes) -> Result<(), String> {
        self.vars.set_bytes(name, value)
    }

    /// Get a variable's value.
    pub fn get_var(&self, name: &str) -> Option<&str> {
        self.vars.get(name)
    }

    /// Get a variable's raw bytes.
    pub fn get_var_bytes(&self, name: &str) -> Option<&ShellBytes> {
        self.vars.get_bytes(name)
    }

    pub(crate) fn expand_fields(&mut self, word: &Word) -> crate::error::Result<Vec<String>> {
        expand::expand_word_to_fields(word, self)
    }

    pub(crate) fn expand_string(&mut self, word: &Word) -> crate::error::Result<String> {
        expand::expand_word_to_string(word, self)
    }

    pub(crate) fn expand_pattern(&mut self, word: &Word) -> crate::error::Result<String> {
        expand::expand_pattern(&word.parts, self)
    }

    /// Evaluate a command node, returning its exit status.
    pub fn eval_command(&mut self, cmd: &Command) -> crate::error::Result<ExitStatus> {
        if let Command::Pipeline { commands, span, .. } = cmd
            && commands.is_empty()
        {
            return Err(ShellError::Runtime {
                msg: "empty pipeline".into(),
                span: *span,
            });
        }
        self.check_cancel()?;
        if self.opts.noexec {
            return Ok(ExitStatus::SUCCESS);
        }
        let status = self.eval_command_inner(cmd)?;
        self.exit_status = status;

        let is_bang = matches!(cmd, Command::Pipeline { bang: true, .. });
        let is_leaf = matches!(
            cmd,
            Command::Simple { .. }
                | Command::Pipeline { .. }
                | Command::Subshell { .. }
                | Command::Background { .. }
        );
        if self.opts.errexit && is_leaf && !self.tested && !is_bang && !status.success() {
            return Err(ShellError::Exit(status));
        }

        Ok(status)
    }

    fn eval_command_inner(&mut self, cmd: &Command) -> crate::error::Result<ExitStatus> {
        if !matches!(cmd, Command::Simple { .. }) {
            self.ev_exit = false;
        }
        match cmd {
            Command::Simple {
                assigns,
                args,
                redirs,
                span,
            } => self.eval_simple(assigns, args, redirs, *span),

            Command::Pipeline {
                commands,
                bang,
                span: _,
            } => {
                let saved = self.tested;
                if *bang {
                    self.tested = true;
                }
                let previous_context = self.placement_context;
                if commands.len() > 1 || !*bang {
                    self.placement_context.pipeline_depth += 1;
                }
                let pipeline_result = self.eval_pipeline(commands);
                self.placement_context = previous_context;
                self.tested = saved;
                if *bang && let Some(job) = self.pending_job.as_mut() { job.invert(); }
                let status = pipeline_result?;
                Ok(if *bang { status.inverted() } else { status })
            }

            Command::And(left, right) => {
                let saved = self.tested;
                self.tested = true;
                let status = self.eval_command(left)?;
                self.tested = saved;
                self.exit_status = status;
                if status.success() {
                    self.eval_command(right)
                } else {
                    Ok(status)
                }
            }

            Command::Or(left, right) => {
                let saved = self.tested;
                self.tested = true;
                let status = self.eval_command(left)?;
                self.tested = saved;
                self.exit_status = status;
                if !status.success() {
                    self.eval_command(right)
                } else {
                    Ok(status)
                }
            }

            Command::Sequence(left, right) => {
                let status = self.eval_command(left)?;
                self.exit_status = status;
                self.eval_command(right)
            }

            Command::Subshell {
                body,
                redirs,
                span: _,
            } => {
                self.eval_in_subshell(body, redirs)
            }

            Command::BraceGroup {
                body,
                redirs,
                span: _,
            } => {
                let saved = self.setup_redirections(redirs)?;
                let result = self.eval_command(body);
                self.finish_redirections(result, saved)
            }

            Command::If {
                cond,
                then_part,
                else_part,
                ..
            } => {
                let saved = self.tested;
                self.tested = true;
                let cond_status = self.eval_command(cond)?;
                self.tested = saved;
                self.exit_status = cond_status;
                if cond_status.success() {
                    self.eval_command(then_part)
                } else if let Some(else_part) = else_part {
                    self.eval_command(else_part)
                } else {
                    Ok(ExitStatus::SUCCESS)
                }
            }

            Command::While { cond, body, .. } => {
                self.loop_depth += 1;
                let mut last_status = ExitStatus::SUCCESS;
                loop {
                    self.check_cancel()?;
                    let saved = self.tested;
                    self.tested = true;
                    let cond_status = self.eval_command(cond)?;
                    self.tested = saved;
                    self.exit_status = cond_status;
                    if !cond_status.success() {
                        break;
                    }
                    match self.eval_command(body) {
                        Ok(s) => last_status = s,
                        Err(ShellError::Break(1)) => break,
                        Err(ShellError::Break(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Break(n - 1));
                        }
                        Err(ShellError::Continue(1)) => continue,
                        Err(ShellError::Continue(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Continue(n - 1));
                        }
                        Err(e) => {
                            self.loop_depth -= 1;
                            return Err(e);
                        }
                    }
                }
                self.loop_depth -= 1;
                Ok(last_status)
            }

            Command::Until { cond, body, .. } => {
                self.loop_depth += 1;
                let mut last_status = ExitStatus::SUCCESS;
                loop {
                    self.check_cancel()?;
                    let saved = self.tested;
                    self.tested = true;
                    let cond_status = self.eval_command(cond)?;
                    self.tested = saved;
                    self.exit_status = cond_status;
                    if cond_status.success() {
                        break;
                    }
                    match self.eval_command(body) {
                        Ok(s) => last_status = s,
                        Err(ShellError::Break(1)) => break,
                        Err(ShellError::Break(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Break(n - 1));
                        }
                        Err(ShellError::Continue(1)) => continue,
                        Err(ShellError::Continue(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Continue(n - 1));
                        }
                        Err(e) => {
                            self.loop_depth -= 1;
                            return Err(e);
                        }
                    }
                }
                self.loop_depth -= 1;
                Ok(last_status)
            }

            Command::For {
                var, words, body, ..
            } => {
                let word_list = if let Some(words) = words {
                    let mut expanded = Vec::new();
                    for w in words {
                        expanded.extend(self.expand_fields(w)?);
                    }
                    expanded
                } else {
                    self.vars.positional_shell_strings()
                };

                self.loop_depth += 1;
                let mut last_status = ExitStatus::SUCCESS;
                for value in &word_list {
                    self.check_cancel()?;
                    if let Err(e) = self.vars.set_shell(var, value) {
                        self.err_msg(&e);
                        return Ok(ExitStatus::FAILURE);
                    }
                    match self.eval_command(body) {
                        Ok(s) => last_status = s,
                        Err(ShellError::Break(1)) => break,
                        Err(ShellError::Break(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Break(n - 1));
                        }
                        Err(ShellError::Continue(1)) => continue,
                        Err(ShellError::Continue(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Continue(n - 1));
                        }
                        Err(e) => {
                            self.loop_depth -= 1;
                            return Err(e);
                        }
                    }
                }
                self.loop_depth -= 1;
                Ok(last_status)
            }

            Command::Case { word, arms, .. } => {
                let expanded = expand::remove_glob_escapes(&self.expand_string(word)?);

                for arm in arms {
                    for pattern in &arm.patterns {
                        let pat = self.expand_pattern(pattern)?;
                        if glob::fnmatch_shell(&pat, &expanded) {
                            if let Some(ref body) = arm.body {
                                return self.eval_command(body);
                            } else {
                                return Ok(ExitStatus::SUCCESS);
                            }
                        }
                    }
                }
                Ok(ExitStatus::SUCCESS)
            }

            Command::FuncDef { name, body, .. } => {
                self.functions.insert(name.clone(), *body.clone());
                Ok(ExitStatus::SUCCESS)
            }

            Command::Not(cmd) => {
                let saved = self.tested;
                self.tested = true;
                let result = self.eval_command(cmd);
                if let Some(job) = self.pending_job.as_mut() { job.invert(); }
                self.tested = saved;
                Ok(result?.inverted())
            }

            Command::Background { cmd, redirs } => {
                self.eval_background(cmd, redirs)
            }
        }
    }

    fn eval_simple(
        &mut self,
        assigns: &[Assignment],
        args: &[Word],
        redirs: &[Redir],
        span: Span,
    ) -> crate::error::Result<ExitStatus> {
        let substitutions_before = self.substitution_count;
        let mut expanded_args: Vec<String> = Vec::new();
        for arg in args {
            expanded_args.extend(self.expand_fields(arg)?);
        }

        if expanded_args.is_empty() {
            let saved = self.setup_redirections(redirs)?;
            let result = (|| {
                let mut trace = self.xtrace_prefix();
                for assign in assigns {
                    let value = self.expand_string(&assign.value)?;
                    if self.opts.xtrace {
                        if !trace.ends_with(' ') { trace.push(' '); }
                        trace.push_str(&assign.name);
                        trace.push('=');
                        trace.push_str(&value);
                    }
                    self.vars.set_shell(&assign.name, &value).map_err(|msg| ShellError::Runtime { msg, span })?;
                }
                if self.opts.xtrace && !assigns.is_empty() {
                    trace.push('\n');
                    self.write_err(&trace);
                }
                Ok(if self.substitution_count == substitutions_before { ExitStatus::SUCCESS } else { self.exit_status })
            })();
            return self.finish_redirections(result, saved);
        }

        if let Some(diagnostic) = self.parent_process_refusal(&expanded_args) {
            self.err_msg(&diagnostic);
            return Ok(ExitStatus::MISUSE);
        }
        self.check_command_policy_for_dispatch(&expanded_args, span)?;
        self.preflight_external(&expanded_args)?;
        self.preflight_redirects(redirs)?;

        if self.opts.xtrace {
            let mut trace = self.xtrace_prefix();
            for assign in assigns {
                if let Ok(value) = self.expand_string(&assign.value) {
                    if !trace.ends_with(' ') {
                        trace.push(' ');
                    }
                    trace.push_str(&assign.name);
                    trace.push('=');
                    trace.push_str(&value);
                }
            }
            for arg in &expanded_args {
                if !trace.ends_with(' ') {
                    trace.push(' ');
                }
                trace.push_str(arg);
            }
            trace.push('\n');
            self.write_err(&trace);
        }

        let cmd_name = &expanded_args[0];

        let mut dispatch_args = expanded_args.as_slice();
        while dispatch_args.first().is_some_and(|name| name == "command") {
            match crate::builtins::command_action(dispatch_args) {
                crate::builtins::CommandAction::Execute(args) => dispatch_args = args,
                _ => break,
            }
        }
        let is_exec = dispatch_args.first().is_some_and(|name| name == "exec");

        let saved_fds = self.setup_redirections(redirs)?;

        let result = match self.try_builtin(cmd_name, &expanded_args, assigns, &[], span) {
            Ok(Some(status)) => Ok(status),
            Err(e) => Err(e),
            Ok(None) => {
                if let Some(func_body) = self.functions.get(cmd_name).cloned() {
                    self.ev_exit = false;
                    self.eval_function(&func_body, &expanded_args, assigns, &[], span)
                } else if self.has_external_handler(cmd_name) {
                    self.call_external_handler(&expanded_args, assigns)
                } else {
                    self.eval_external(&expanded_args, assigns, &[], span)
                }
            }
        };

        if is_exec {
            self.finish_saved_redirections(result, saved_fds)
        } else {
            self.finish_redirections(result, saved_fds)
        }
    }

    fn preflight_external(&self, args: &[String]) -> crate::error::Result<()> {
        let mut target = args;
        let mut bypass_functions = false;
        while target.first().is_some_and(|name| name == "command") {
            match crate::builtins::command_action(target) {
                crate::builtins::CommandAction::Execute(nested) => { target = nested; bypass_functions = true; }
                _ => return Ok(()),
            }
        }
        let Some(name) = target.first() else { return Ok(()); };
        if crate::builtins::is_builtin(name) || (!bypass_functions && self.functions.contains_key(name)) { return Ok(()); }
        if self.external_dispatch_names.iter().any(|registered| registered == &ShellBytes::from_str_lossless(name)) {
            if let Some(dispatcher) = &self.external_dispatcher { dispatcher.check(CallbackKind::External)?; }
        } else if let Some(handler) = &self.external_handler { handler.check(CallbackKind::External)?; }
        Ok(())
    }

    pub(crate) fn preflight_redirects(&self, redirs: &[Redir]) -> crate::error::Result<()> {
        if redirs.iter().any(|redir| matches!(redir.kind, RedirKind::Output(_) | RedirKind::Clobber(_) | RedirKind::Append(_) | RedirKind::ReadWrite(_)))
            && let Some(handler) = &self.redirect_open_handler
        { handler.check(CallbackKind::RedirectOpen)?; }
        Ok(())
    }

    pub(crate) fn call_external_handler(
        &mut self,
        args: &[String],
        assigns: &[Assignment],
    ) -> crate::error::Result<ExitStatus> {
        self.preflight_external(args)?;
        let mut assign_bytes: Vec<(String, ShellBytes)> = Vec::with_capacity(assigns.len());
        for assign in assigns {
            let value = self.expand_string(&assign.value)?;
            assign_bytes.push((assign.name.clone(), ShellBytes::from_str_lossless(&value)));
        }
        let env_pairs: Vec<(ShellBytes, ShellBytes)> = self
            .vars
            .env_for_command_os(&assign_bytes)
            .into_iter()
            .map(|(name, value)| {
                (
                    ShellBytes::from_os_string(name),
                    ShellBytes::from_os_string(value),
                )
            })
            .collect();
        let args_bytes: Vec<ShellBytes> = args.iter().map(|s| ShellBytes::from_str_lossless(s)).collect();
        let cwd_guard = ExternalCwdGuard::enter(self.cwd.clone());
        let result = (|| {
            if self.external_dispatch_names.iter().any(|name| name == &args_bytes[0]) {
                self.external_dispatcher.as_mut().expect("registered dispatcher").get(CallbackKind::External)?(&args_bytes, &env_pairs)
            } else {
                self.external_handler.as_mut().expect("registered external handler").get(CallbackKind::External)?(&args_bytes, &env_pairs).map(ExternalDispatch::Handled)
            }
        })();
        drop(cwd_guard);
        match result? {
            ExternalDispatch::Handled(status) => Ok(status),
            ExternalDispatch::NotHandled => {
                let env = env_pairs.into_iter().map(|(name, value)| (name.to_os_string(), value.to_os_string())).collect();
                self.eval_external_prepared(args, &[], &[], Span::default(), Some(env))
            }
        }
    }

    fn eval_function(
        &mut self,
        body: &Command,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        let saved_positional = std::mem::replace(
            &mut self.vars.positional,
            args[1..].iter().map(|s| ShellBytes::from_str_lossless(s)).collect(),
        );

        self.vars.push_scope();

        for assign in assigns {
            let value = self.expand_string(&assign.value)?;
            self.vars.make_local(&assign.name);
            let _ = self.vars.set_shell(&assign.name, &value);
        }

        let saved_fds = self.setup_redirections(redirs)?;

        let result = match self.eval_command(body) {
            Ok(s) => Ok(s),
            Err(ShellError::Return(n)) => Ok(n),
            Err(e) => Err(e),
        };

        let result = self.finish_redirections(result, saved_fds);
        self.vars.pop_scope();
        self.vars.positional = saved_positional;

        result
    }

    fn handle_exec_error(
        &self,
        e: &std::io::Error,
        cmd_name: &str,
    ) -> crate::error::Result<ExitStatus> {
        if let Some(code) = e.raw_os_error() {
            let path_env = self.vars.get_bytes("PATH")
                .map(|value| value.to_os_string())
                .unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"));
            let cmd_name = ShellBytes::from_str_lossless(cmd_name).to_os_string();
            if let Some(msg) = bad_interpreter_message_os(&cmd_name, code, &self.cwd, &path_env) {
                self.err_msg(&crate::encoding::bytes_to_str(msg.as_bytes()));
                return Ok(ExitStatus::NOT_EXECUTABLE);
            }
        }
        if e.kind() == std::io::ErrorKind::NotFound {
            self.err_msg(&format!("{cmd_name}: not found"));
            Ok(ExitStatus::NOT_FOUND)
        } else if e.kind() == std::io::ErrorKind::PermissionDenied {
            self.err_msg(&format!("{cmd_name}: permission denied"));
            Ok(ExitStatus::NOT_EXECUTABLE)
        } else {
            Err(ShellError::Io(std::io::Error::new(e.kind(), e.to_string())))
        }
    }

    pub(crate) fn eval_external(
        &mut self,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        self.eval_external_prepared(args, assigns, redirs, _span, None)
    }

    fn eval_external_prepared(
        &mut self, args: &[String], assigns: &[Assignment], redirs: &[Redir], _span: Span,
        environment: Option<Vec<(OsString, OsString)>>,
    ) -> crate::error::Result<ExitStatus> {
        self.check_cancel()?;
        let saved = self.setup_redirections(redirs)?;
        let args_bytes: Vec<ShellBytes> = args.iter().map(|s| ShellBytes::from_str_lossless(s)).collect();
        let mut assign_bytes = Vec::new();
        for assign in assigns {
            let value = self.expand_string(&assign.value)?;
            assign_bytes.push((assign.name.clone(), ShellBytes::from_str_lossless(&value)));
        }

        let env = environment.unwrap_or_else(|| self.vars.env_for_command_os(&assign_bytes));
        if self.ev_exit {
            if self.opts.interactive {
                for signal in [rustix::process::Signal::INT, rustix::process::Signal::QUIT, rustix::process::Signal::TSTP, rustix::process::Signal::TTOU, rustix::process::Signal::TTIN, rustix::process::Signal::PIPE] {
                    crate::signal::reset_handler(signal)?;
                }
            }
            if let Err(error) = std::env::set_current_dir(&self.cwd) {
                return self.finish_redirections(Err(error.into()), saved);
            }
            let err = crate::builtins::exec::execve_with_env(&args_bytes[0], &args_bytes, &env);
            let result = self.handle_exec_error(&err, &args[0]);
            return self.finish_redirections(result, saved);
        }

        let mut cmd = std::process::Command::new(args_bytes[0].to_os_string());
        cmd.args(args_bytes.iter().skip(1).map(ShellBytes::to_os_string));
        cmd.current_dir(&self.cwd);
        if self.opts.interactive {
            let handoff = !self.in_forked_child;
            let terminal_fd = self.terminal_raw_fd();
            // SAFETY: the child uses only signal disposition syscalls and a terminal process-group ioctl before exec.
            unsafe {
                cmd.pre_exec(move || {
                    if handoff {
                        crate::signal::ignore_signal(rustix::process::Signal::TTOU)?;
                        sys::set_foreground_group(terminal_fd, rustix::process::getpgrp().as_raw_pid())?;
                    }
                    for signal in [rustix::process::Signal::INT, rustix::process::Signal::QUIT, rustix::process::Signal::TSTP, rustix::process::Signal::TTOU, rustix::process::Signal::TTIN, rustix::process::Signal::PIPE] {
                        crate::signal::reset_handler(signal)?;
                    }
                    Ok(())
                });
            }
        }
        if !self.in_forked_child { cmd.process_group(0); }

        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }

        if self.stdout_sink.is_some() {
            cmd.stdout(std::process::Stdio::piped());
        }
        if self.stderr_sink.is_some() {
            cmd.stderr(std::process::Stdio::piped());
        }
        if let Some(fd) = self.pending_stdin_fd() {
            let child_stdin = match sys::duplicate_fd_above(fd, 3) {
                Ok(fd) => std::fs::File::from(fd),
                Err(error) => {
                    return self.finish_redirections(Err(ShellError::Io(error)), saved);
                }
            };
            cmd.stdin(child_stdin);
        }

        let mut extra_outputs = Vec::new();
        for (&target, sink) in &self.descriptor_sinks {
            let (read, write) = sys::pipe_owned()?;
            extra_outputs.push(ForkOutput { target, read, write, sink: sink.clone() });
        }
        if !extra_outputs.is_empty() {
            let targets: Vec<_> = extra_outputs.iter().map(|output| (output.write.as_raw_fd(), output.target)).collect();
            // SAFETY: the callback only duplicates already-open descriptors with dup2; all endpoints live until spawn returns.
            unsafe {
                cmd.pre_exec(move || {
                    for &(source, target) in &targets { sys::dup2_result(source, target)?; }
                    Ok(())
                });
            }
        }

        let result = match cmd.spawn() {
            Ok(mut child) => {
                let child_id = child.id() as i32;
                let pgid = if self.in_forked_child { rustix::process::getpgrp().as_raw_pid() } else { child_id };
                self.track_child(child_id, pgid);
                let mut streams = Vec::new();
                if let Some(stdout) = child.stdout.take() {
                    streams.push((OwnedFd::from(stdout), self.stdout_sink.clone().unwrap()));
                }
                if let Some(stderr) = child.stderr.take() {
                    streams.push((OwnedFd::from(stderr), self.stderr_sink.clone().unwrap()));
                }
                for output in extra_outputs {
                    drop(output.write);
                    streams.push((output.read, output.sink));
                }
                self.relay_streams(child_id, streams, &[child_id])?;
                drop(child);
                self.wait_foreground(&[child_id], pgid)
            }
            Err(error) => self.handle_exec_error(&error, &args[0]),
        };

        if self.opts.interactive && !self.in_forked_child {
            let reclaimed = self.foreground_terminal(rustix::process::getpgrp().as_raw_pid());
            match (&result, reclaimed) {
                (Ok(_), Err(error)) => return self.finish_redirections(Err(error), saved),
                (Err(_), Err(error)) => self.err_msg(&format!("epsh: cannot reclaim terminal: {error}")),
                _ => {}
            }
        }
        self.finish_redirections(result, saved)
    }

    fn eval_pipeline(&mut self, commands: &[Command]) -> crate::error::Result<ExitStatus> {
        self.ensure_terminal_descriptor()?;
        if commands.len() == 1 {
            return self.eval_command(&commands[0]);
        }

        let mut pipes = Vec::new();
        for _ in 0..commands.len() - 1 {
            pipes.push(sys::pipe_owned()?);
        }
        let outputs = self.fork_outputs(true)?;
        let (gate_reader, gate_writer) = sys::pipe_owned()?;
        let mut children = Vec::new();
        let mut pgid = if self.in_forked_child { rustix::process::getpgrp().as_raw_pid() } else { 0 };
        for (i, cmd) in commands.iter().enumerate() {
            let pid = match self.fork_evaluator() {
                Ok(pid) => pid,
                Err(error) => {
                    drop(pipes);
                    drop(outputs);
                    self.kill_owned_children(&children);
                    return Err(error);
                }
            };
            if pid < 0 {
                let error = std::io::Error::last_os_error();
                drop(pipes);
                drop(outputs);
                self.kill_owned_children(&children);
                return Err(ShellError::Io(error));
            }
            if pid == 0 {
                if self.enter_forked_child(&outputs, pgid).is_err() {
                    sys::exit_child(ExitStatus::FAILURE);
                }
                if i > 0 && sys::dup2_result(pipes[i - 1].0.as_raw_fd(), 0).is_err() {
                    sys::exit_child(ExitStatus::FAILURE);
                }
                if i < commands.len() - 1 && sys::dup2_result(pipes[i].1.as_raw_fd(), 1).is_err() {
                    sys::exit_child(ExitStatus::FAILURE);
                }
                drop(pipes);
                drop(gate_writer);
                let mut gate = [0u8];
                loop {
                    match rustix::io::read(&gate_reader, &mut gate) {
                        Ok(0) => break,
                        Err(rustix::io::Errno::INTR) => continue,
                        _ => sys::exit_child(ExitStatus::FAILURE),
                    }
                }
                drop(gate_reader);
                self.ev_exit = true;
                let status = match self.eval_command(cmd) {
                    Ok(status) | Err(ShellError::Exit(status)) => status,
                    Err(error) => {
                        self.report_child_refusal(&error);
                        self.err_msg(&format!("epsh: {error}"));
                        if matches!(error, ShellError::CallbackProcess { .. } | ShellError::CommandRefused { .. }) { ExitStatus::MISUSE } else { ExitStatus::FAILURE }
                    }
                };
                sys::exit_child(status);
            }
            if pgid == 0 { pgid = pid; }
            self.track_child(pid, pgid);
            children.push(pid);
        }
        drop(pipes);
        drop(gate_reader);
        if self.opts.interactive && !self.in_forked_child
            && let Err(error) = self.foreground_terminal(pgid)
        {
            self.kill_owned_children(&children);
            return Err(error);
        }
        drop(gate_writer);
        self.relay_fork_outputs(*children.last().unwrap(), outputs, &children)?;

        self.wait_foreground(&children, pgid)
    }

    fn eval_in_subshell(
        &mut self,
        body: &Command,
        redirs: &[Redir],
    ) -> crate::error::Result<ExitStatus> {
        let outputs = self.fork_outputs(true)?;
        let parent_group = if self.in_forked_child { rustix::process::getpgrp().as_raw_pid() } else { 0 };
        let pid = self.fork_evaluator()?;
        if pid < 0 { return Err(ShellError::Io(std::io::Error::last_os_error())); }
        if pid == 0 {
            if self.enter_forked_child(&outputs, parent_group).is_err() {
                sys::exit_child(ExitStatus::FAILURE);
            }
            self.traps.remove("EXIT");
            let _saved = match self.setup_redirections(redirs) {
                Ok(saved) => saved,
                Err(error) => {
                    self.report_child_refusal(&error);
                    self.err_msg(&format!("epsh: {error}"));
                    sys::exit_child(ExitStatus::FAILURE);
                }
            };
            let status = match self.eval_command(body) {
                Ok(status) | Err(ShellError::Exit(status)) => status,
                Err(error) => {
                    self.report_child_refusal(&error);
                    self.err_msg(&format!("epsh: {error}"));
                    if matches!(error, ShellError::CallbackProcess { .. } | ShellError::CommandRefused { .. }) { ExitStatus::MISUSE } else { ExitStatus::FAILURE }
                }
            };
            self.run_exit_trap();
            sys::exit_child(status);
        }
        let pgid = if parent_group == 0 { pid } else { parent_group };
        self.track_child(pid, pgid);
        self.relay_fork_outputs(pid, outputs, &[pid])?;
        self.wait_child(pid)
    }

    fn eval_background(
        &mut self,
        cmd: &Command,
        redirs: &[Redir],
    ) -> crate::error::Result<ExitStatus> {
        let outputs = self.fork_outputs(true)?;
        let parent_group = if self.in_forked_child { rustix::process::getpgrp().as_raw_pid() } else { 0 };
        let pid = self.fork_evaluator()?;
        if pid < 0 { return Err(ShellError::Io(std::io::Error::last_os_error())); }
        if pid == 0 {
            if self.enter_forked_child(&outputs, parent_group).is_err() {
                sys::exit_child(ExitStatus::FAILURE);
            }
            self.traps.remove("EXIT");
            let _saved = match self.setup_redirections(redirs) {
                Ok(saved) => saved,
                Err(error) => {
                    self.report_child_refusal(&error);
                    self.err_msg(&format!("epsh: {error}"));
                    sys::exit_child(ExitStatus::FAILURE);
                }
            };
            let status = match self.eval_command(cmd) {
                Ok(status) | Err(ShellError::Exit(status)) => status,
                Err(error) => {
                    self.report_child_refusal(&error);
                    self.err_msg(&format!("epsh: {error}"));
                    if matches!(error, ShellError::CallbackProcess { .. } | ShellError::CommandRefused { .. }) { ExitStatus::MISUSE } else { ExitStatus::FAILURE }
                }
            };
            sys::exit_child(status);
        }
        let pgid = if parent_group == 0 { pid } else { parent_group };
        self.track_child(pid, pgid);
        self.relay_fork_outputs(pid, outputs, &[pid])?;
        self.last_bg_pid = Some(pid as u32);
        Ok(ExitStatus::SUCCESS)
    }

    /// Execute a command substitution and return its encoded output.
    pub fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String> {
        let outputs = self.fork_outputs(false)?;
        let (read_end, write_end) = sys::pipe_owned()?;
        let flags = rustix::fs::fcntl_getfl(&read_end).map_err(std::io::Error::from)?;
        rustix::fs::fcntl_setfl(&read_end, flags | rustix::fs::OFlags::NONBLOCK).map_err(std::io::Error::from)?;
        let parent_group = if self.in_forked_child { rustix::process::getpgrp().as_raw_pid() } else { 0 };
        let pid = self.fork_evaluator()?;
        if pid < 0 { return Err(ShellError::Io(std::io::Error::last_os_error())); }
        if pid == 0 {
            if self.enter_forked_child(&outputs, parent_group).is_err()
                || sys::dup2_result(write_end.as_raw_fd(), 1).is_err() {
                sys::exit_child(ExitStatus::FAILURE);
            }
            drop(read_end);
            drop(write_end);
            self.traps.remove("EXIT");
            let status = match self.eval_in_command_substitution(cmd) {
                Ok(status) | Err(ShellError::Exit(status)) => status,
                Err(error) => {
                    self.report_child_refusal(&error);
                    self.err_msg(&format!("epsh: {error}"));
                    ExitStatus::MISUSE
                }
            };
            self.run_exit_trap();
            sys::exit_child(status);
        }
        drop(write_end);
        let pgid = if parent_group == 0 { pid } else { parent_group };
        self.track_child(pid, pgid);
        self.relay_fork_outputs(pid, outputs, &[pid])?;

        let mut raw_output = Vec::new();
        let mut buffer = [0u8; 8192];
        let read_result = loop {
            if let Err(error) = self.check_cancel() { break Err(error); }
            match rustix::io::read(&read_end, &mut buffer) {
                Ok(0) => break Ok(()),
                Ok(length) => raw_output.extend_from_slice(&buffer[..length]),
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::AGAIN) => {
                    let mut events = [rustix::event::PollFd::new(&read_end, rustix::event::PollFlags::IN)];
                    let timeout = rustix::event::Timespec { tv_sec: 0, tv_nsec: 5_000_000 };
                    match rustix::event::poll(&mut events, Some(&timeout)) {
                        Ok(_) | Err(rustix::io::Errno::INTR) => {}
                        Err(error) => break Err(ShellError::Io(error.into())),
                    }
                }
                Err(error) => break Err(ShellError::Io(error.into())),
            }
        };
        if read_result.is_err() { self.terminate_group(pgid); }
        let waited = self.wait_child(pid);
        read_result?;
        self.exit_status = waited?;
        self.substitution_count = self.substitution_count.wrapping_add(1);
        let mut output = crate::encoding::bytes_to_str(&raw_output);
        while output.ends_with('\n') { output.pop(); }
        Ok(output)
    }

    fn eval_in_command_substitution(
        &mut self,
        cmd: &Command,
    ) -> crate::error::Result<ExitStatus> {
        let previous_context = self.placement_context;
        self.placement_context.command_substitution_depth += 1;
        let result = self.eval_command(cmd);
        self.placement_context = previous_context;
        result
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_trap_callback_refusal_reaches_the_parent_without_changing_trap_status() {
        let mut shell = Shell::new();
        shell.set_external_handler(Box::new(|_, _| Ok(ExitStatus::SUCCESS)));
        shell.traps.insert("USR1".to_owned(), "/bin/true".to_owned());
        shell.exit_status = ExitStatus::from(7);
        let pid = shell.fork_evaluator().unwrap();
        assert!(pid >= 0);
        if pid == 0 {
            shell.enter_forked_child(&[], 0).unwrap();
            crate::signal::install_handler(rustix::process::Signal::USR1).unwrap();
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::USR1).unwrap();
            shell.run_pending_traps();
            sys::exit_child(shell.exit_status);
        }
        shell.track_child(pid, pid);
        let status = rustix::process::waitid(
            rustix::process::WaitId::Pid(rustix::process::Pid::from_raw(pid).unwrap()),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
        ).unwrap().unwrap();
        assert_eq!(status.exit_status(), Some(7));
        assert!(matches!(shell.wait_owned_child(pid), Err(ShellError::CallbackProcess { kind: CallbackKind::External })));
    }

    #[test]
    fn run_echo() {
        let mut shell = Shell::new();
        let status = shell.run_script("echo hello");
        assert_eq!(status, 0);
    }

    #[test]
    fn dot_navigation_is_lexically_resolved() {
        assert!(is_dot_navigation(Path::new(".././..")));
        assert!(!is_dot_navigation(Path::new("../child")));
        assert!(!is_dot_navigation(Path::new("/../")));

        assert_eq!(
            apply_dot_navigation(Path::new("/tmp/a"), Path::new(".././..")),
            Path::new("/")
        );
        assert_eq!(
            apply_dot_navigation(Path::new("/"), Path::new("../..")),
            Path::new("/")
        );
    }

    #[test]
    fn run_true_false() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("true"), 0);
        assert_eq!(shell.run_script("false"), 1);
    }

    #[test]
    fn run_variable_assignment() {
        let mut shell = Shell::new();
        shell.run_script("FOO=bar");
        assert_eq!(shell.get_var("FOO"), Some("bar"));
    }

    #[test]
    fn run_variable_expansion() {
        let mut shell = Shell::new();
        shell.run_script("X=hello");
        assert_eq!(shell.get_var("X"), Some("hello"));
    }

    #[test]
    fn run_and_list() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("true && true"), 0);
        assert_eq!(shell.run_script("false && true"), 1);
        assert_eq!(shell.run_script("true && false"), 1);
    }

    #[test]
    fn run_or_list() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("false || true"), 0);
        assert_eq!(shell.run_script("true || false"), 0);
        assert_eq!(shell.run_script("false || false"), 1);
    }

    #[test]
    fn run_if_true() {
        let mut shell = Shell::new();
        shell.run_script("if true; then X=yes; else X=no; fi");
        assert_eq!(shell.get_var("X"), Some("yes"));
    }

    #[test]
    fn run_if_false() {
        let mut shell = Shell::new();
        shell.run_script("if false; then X=yes; else X=no; fi");
        assert_eq!(shell.get_var("X"), Some("no"));
    }

    #[test]
    fn run_for_loop() {
        let mut shell = Shell::new();
        shell.run_script("RESULT=''; for x in a b c; do RESULT=\"${RESULT}${x}\"; done");
        assert_eq!(shell.get_var("RESULT"), Some("abc"));
    }

    #[test]
    fn run_while_loop() {
        let mut shell = Shell::new();
        shell.run_script("I=x; while test $I != xxx; do I=${I}x; done");
        assert_eq!(shell.get_var("I"), Some("xxx"));
    }

    #[test]
    fn run_function() {
        let mut shell = Shell::new();
        shell.run_script("greet() { RESULT=$1; }; greet world");
        assert_eq!(shell.get_var("RESULT"), Some("world"));
    }

    #[test]
    fn run_case() {
        let mut shell = Shell::new();
        shell.run_script("X=b; case $X in a) R=first;; b) R=second;; esac");
        assert_eq!(shell.get_var("R"), Some("second"));
    }

    #[test]
    fn run_negation() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("! true"), 1);
        assert_eq!(shell.run_script("! false"), 0);
    }

    #[test]
    fn run_exit_status() {
        let mut shell = Shell::new();
        shell.run_script("false");
        assert_eq!(shell.exit_status, ExitStatus::FAILURE);
        shell.run_script("true");
        assert_eq!(shell.exit_status, ExitStatus::SUCCESS);
    }

    #[test]
    fn run_pipeline_external() {
        let mut shell = Shell::new();
        let status = shell.run_script("echo hello | cat");
        assert_eq!(status, 0);
    }

    #[test]
    fn run_pipeline_external_preserves_shell_environment() {
        let output = tempfile::NamedTempFile::new().unwrap();
        let output_path = output.path().to_string_lossy();
        let mut shell = Shell::builder().env_clear().build();
        shell.set_var("EPSH_PIPELINE_ENV", "works").unwrap();
        shell.vars_mut().export("EPSH_PIPELINE_ENV");
        shell.set_var("PATH", "/bin:/usr/bin").unwrap();
        shell.vars_mut().export("PATH");

        let status = shell.run_script(&format!(
            "/usr/bin/env | grep EPSH_PIPELINE_ENV > {output_path}"
        ));
        assert_eq!(status, 0);
        assert_eq!(
            std::fs::read_to_string(output.path()).unwrap(),
            "EPSH_PIPELINE_ENV=works\n"
        );
    }

    #[test]
    fn run_set_errexit() {
        let mut shell = Shell::new();
        let status = shell.run_script("set -e; X=before; false; X=after");
        assert_eq!(status, 1);
        assert_eq!(shell.get_var("X"), Some("before"));
    }

    #[test]
    fn run_test_builtin() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("test -z ''"), 0);
        assert_eq!(shell.run_script("test -z 'notempty'"), 1);
        assert_eq!(shell.run_script("test -n 'notempty'"), 0);
        assert_eq!(shell.run_script("[ 1 -eq 1 ]"), 0);
        assert_eq!(shell.run_script("[ 1 -eq 2 ]"), 1);
    }

    #[test]
    fn run_arithmetic_expansion() {
        let mut shell = Shell::new();
        shell.run_script("X=$((2 + 3 * 4))");
        assert_eq!(shell.get_var("X"), Some("14"));
    }

    #[test]
    fn run_arithmetic_with_variables() {
        let mut shell = Shell::new();
        shell.run_script("A=10; B=3; C=$((A + B))");
        assert_eq!(shell.get_var("C"), Some("13"));
    }

    #[test]
    fn run_arithmetic_assignment() {
        let mut shell = Shell::new();
        shell.run_script("I=0; I=$((I + 1)); I=$((I + 1)); I=$((I + 1))");
        assert_eq!(shell.get_var("I"), Some("3"));
    }

    #[test]
    fn run_while_with_arithmetic() {
        let mut shell = Shell::new();
        shell.run_script("I=0; while test $I -lt 5; do I=$((I + 1)); done");
        assert_eq!(shell.get_var("I"), Some("5"));
    }

    #[test]
    fn run_subshell() {
        let mut shell = Shell::new();
        shell.run_script("X=outer; (X=inner)");
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn run_local_vars() {
        let mut shell = Shell::new();
        shell.run_script("X=global; f() { local X=local; }; f");
        assert_eq!(shell.get_var("X"), Some("global"));
    }

    #[test]
    fn run_shift() {
        let mut shell = Shell::new();
        shell.set_args(&["test", "a", "b", "c"]);
        shell.run_script("shift; R=$1");
        assert_eq!(shell.get_var("R"), Some("b"));
    }

    #[test]
    fn run_command_substitution() {
        let mut shell = Shell::new();
        shell.run_script("X=$(echo hello)");
        assert_eq!(shell.get_var("X"), Some("hello"));
    }

    #[test]
    fn command_substitution_context_refuses_parent_placement_without_forking() {
        let directory = tempfile::tempdir().unwrap();
        let handler_called = Arc::new(AtomicBool::new(false));
        let handler_called_copy = handler_called.clone();
        let policy_called = Arc::new(AtomicBool::new(false));
        let policy_called_copy = policy_called.clone();
        let mut shell = Shell::builder().cwd(directory.path().to_path_buf()).build();
        shell.set_command_placement(
            ShellBytes::from("cp"),
            CommandPlacement::ParentProcess,
        );
        shell.set_external_handler(Box::new(move |_args, _environment| {
            handler_called_copy.store(true, Ordering::Relaxed);
            Ok(ExitStatus::SUCCESS)
        }));
        shell.set_command_policy(Box::new(move |target| {
            if target.as_bytes() == b"cp" {
                policy_called_copy.store(true, Ordering::Relaxed);
            }
            None
        }));

        let command = Parser::new("command cp source destination > substitution-target")
            .parse()
            .unwrap()
            .commands
            .remove(0);
        assert_eq!(
            shell.eval_in_command_substitution(&command).unwrap(),
            ExitStatus::MISUSE
        );
        assert!(!handler_called.load(Ordering::Relaxed));
        assert!(!policy_called.load(Ordering::Relaxed));
        assert!(!directory.path().join("substitution-target").exists());
    }

    #[test]
    fn run_command_substitution_backtick() {
        let mut shell = Shell::new();
        shell.run_script("X=`echo world`");
        assert_eq!(shell.get_var("X"), Some("world"));
    }

    #[test]
    fn run_nested_command_substitution() {
        let mut shell = Shell::new();
        shell.run_script("X=$(echo $(echo nested))");
        assert_eq!(shell.get_var("X"), Some("nested"));
    }

    #[test]
    fn run_command_substitution_in_args() {
        let mut shell = Shell::new();
        let status = shell.run_script("R=$(echo hello world); true");
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("R"), Some("hello world"));
    }

    #[test]
    fn run_heredoc() {
        let mut shell = Shell::new();
        shell.run_script("R=$(cat <<EOF\nhello world\nEOF\n)");
        assert_eq!(shell.get_var("R"), Some("hello world"));
    }

    #[test]
    fn run_test_compound() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("[ 1 -eq 1 -a 2 -eq 2 ]"), 0);
        assert_eq!(shell.run_script("[ 1 -eq 1 -a 2 -eq 3 ]"), 1);
        assert_eq!(shell.run_script("[ 1 -eq 2 -o 2 -eq 2 ]"), 0);
        assert_eq!(shell.run_script("[ 1 -eq 2 -o 2 -eq 3 ]"), 1);
    }

    #[test]
    fn run_test_file_ops() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("test -d /tmp"), 0);
        assert_eq!(shell.run_script("test -f /tmp"), 1);
        assert_eq!(shell.run_script("test -e /nonexistent"), 1);
    }

    #[test]
    fn run_trap() {
        let mut shell = Shell::new();
        shell.run_script("trap 'echo caught' INT");
        assert!(shell.traps.contains_key("INT"));
    }

    #[test]
    fn heredoc_with_sinks_does_not_corrupt_fd0() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let corrupted = Arc::new(AtomicBool::new(false));
        let corrupted_clone = corrupted.clone();

        let watcher = std::thread::spawn(move || {
            for _ in 0..2000 {
                let r = rustix::io::fcntl_getfd(unsafe { std::os::fd::BorrowedFd::borrow_raw(0) });
                if r.is_err() {
                    corrupted_clone.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        });

        let worker = std::thread::spawn(move || {
            for i in 0..20 {
                let out: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                let out2 = out.clone();
                let script = format!("/bin/cat << 'EOF'\niter {i}\nEOF");
                let mut shell = Shell::builder()
                    .stdout_sink(Arc::new(Mutex::new(crate::eval::tests::VecSink(out2))))
                    .stderr_sink(Arc::new(Mutex::new(std::io::sink())))
                    .build();
                let status = shell.run_script(&script);
                assert_eq!(status, 0, "iter {i} failed");
                let output = String::from_utf8_lossy(&out.lock().unwrap()).into_owned();
                assert!(
                    output.contains(&format!("iter {i}")),
                    "bad output at iter {i}: {output:?}"
                );
            }
        });

        worker.join().unwrap();
        watcher.join().unwrap();

        assert!(
            !corrupted.load(Ordering::Relaxed),
            "fd 0 was corrupted during heredoc execution (dup2 race)"
        );
    }

    pub(super) struct VecSink(pub Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for VecSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
