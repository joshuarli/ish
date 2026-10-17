use fxhash::FxHashMap;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::ast::*;

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

/// True if `path` is a regular file with any execute bit set.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// The interpreter path from a script's `#!` line, or None if the first line
/// is not a shebang (or the file cannot be read). Mirrors bash's `getinterp`.
fn read_shebang_interpreter(path: &Path) -> Option<String> {
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
    Some(String::from_utf8_lossy(&buf[start..i]).into_owned())
}

/// The platform's strerror text for an errno, e.g. "Not a directory".
fn os_error_string(code: i32) -> String {
    unsafe {
        let msg = libc::strerror(code);
        if msg.is_null() {
            return format!("os error {code}");
        }
        std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned()
    }
}

/// The file exec would attempt for `cmd_name`: slash paths resolve against
/// `cwd`; slashless names search `path_env` like `execve_with_env`. Returns
/// None for a slashless name found in no PATH directory.
fn resolve_exec_path(cmd_name: &str, cwd: &Path, path_env: &str) -> Option<PathBuf> {
    let name_path = PathBuf::from(cmd_name);
    if cmd_name.contains('/') {
        return Some(if name_path.is_absolute() {
            name_path
        } else {
            cwd.join(name_path)
        });
    }
    for directory in path_env.split(':') {
        let dir = if directory.is_empty() {
            PathBuf::from(".")
        } else {
            PathBuf::from(directory)
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
    // E2BIG/ENOMEM are resource errors, not interpreter problems.
    if matches!(code, libc::E2BIG | libc::ENOMEM) {
        return None;
    }
    let path = resolve_exec_path(cmd_name, cwd, path_env)?;
    if !is_executable_file(&path) {
        return None;
    }
    let interp = read_shebang_interpreter(&path)?;
    Some(format!(
        "{cmd_name}: {interp}: bad interpreter: {}",
        os_error_string(code)
    ))
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

/// Builtins safe to run in-process for command substitution.
/// Either pure (no shell state modification) or delegates to pure commands.
/// `command` is included because it dispatches to builtins/externals which
/// work correctly with the output sink capture mechanism.
/// Spawn a thread that relays data from a readable stream to a sink.
fn spawn_relay(
    stream: impl std::io::Read + Send + 'static,
    sink: Arc<Mutex<dyn Write + Send>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut reader = std::io::BufReader::new(stream);
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut w) = sink.lock() {
                        let _ = w.write_all(&buf[..n]);
                    }
                }
            }
        }
    })
}

fn is_command_substitution_builtin(name: &str) -> bool {
    matches!(
        name,
        "echo" | "printf" | "true" | "false" | ":" | "pwd" | "type" | "test" | "[" | "command"
    )
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

thread_local! {
    static EXTERNAL_COMMAND_CWD: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
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
    /// Defined functions: name → AST body
    pub(crate) functions: FxHashMap<String, Command>,
    /// Last command's exit status ($?)
    pub(crate) exit_status: ExitStatus,
    /// Shell's PID ($$)
    pub(crate) pid: u32,
    /// Current working directory — per-Shell, not process-global.
    /// All relative paths (redirections, glob, source) resolve against this.
    pub(crate) cwd: PathBuf,
    /// True when `cwd` contains no unresolved symlinks.
    pub(crate) cwd_is_canonical: bool,
    /// Number of nested loops (for break/continue counting)
    pub(crate) loop_depth: usize,
    /// Shell options
    pub(crate) opts: ShellOpts,
    /// Trap handlers: signal name → command string
    pub(crate) traps: FxHashMap<String, String>,
    /// True when evaluating a condition (if test, while cond, && / || operands).
    /// Suppresses set -e (errexit). Mirrors dash's EV_TESTED flag.
    pub(crate) tested: bool,
    /// True when executing inside a forked child (pipeline stage, command subst).
    /// Subshells skip the extra fork to avoid pipe fd leaks.
    pub(crate) in_forked_child: bool,
    /// True when the current eval context will exit after the command completes.
    /// Allows eval_external to exec directly instead of fork+exec (dash's EV_EXIT).
    pub(crate) ev_exit: bool,
    /// Optional cancellation flag. When set to true, the shell aborts execution.
    cancel: Option<Arc<AtomicBool>>,
    /// Optional stdout sink. When set, builtin output goes here instead of fd 1.
    pub(crate) stdout_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    /// Optional stderr sink. When set, error output goes here instead of fd 2.
    pub(crate) stderr_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    /// PIDs of currently running child processes (for cancellation cleanup).
    pub(crate) child_pids: Vec<i32>,
    /// Optional execution deadline. When exceeded, shell aborts with TimedOut.
    timeout: Option<std::time::Instant>,
    /// Optional callback that replaces eval_external for spawning processes.
    /// Lets embedders control process creation (job control, sandboxing, etc.).
    external_handler: Option<ExternalHandler>,
    /// Optional provider that opens writable redirection targets.
    pub(crate) redirect_open_handler: Option<RedirectOpenHandler>,
    /// PID of the last background command ($!).
    last_bg_pid: Option<u32>,
    /// Pending stdin fd for the next external command, set by setup_redirections
    /// when a heredoc targets fd 0 in sink mode (avoids process-wide dup2 on fd 0
    /// which would race with other threads reading stdin, e.g. nerv's input loop).
    pub(crate) pending_stdin: Option<RawFd>,
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
    /// -o pipefail: return highest nonzero status from any pipeline stage
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
    env_clear: bool,
    external_handler: Option<ExternalHandler>,
    redirect_open_handler: Option<RedirectOpenHandler>,
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
            env_clear: false,
            external_handler: None,
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
        self.env_clear = true;
        self
    }
    /// Set a callback for external command execution.
    pub fn external_handler(mut self, handler: ExternalHandler) -> Self {
        self.external_handler = Some(handler);
        self
    }
    /// Set a provider that opens writable redirection targets.
    pub fn redirect_open_handler(mut self, handler: RedirectOpenHandler) -> Self {
        self.redirect_open_handler = Some(handler);
        self
    }

    pub fn build(self) -> Shell {
        let vars = if self.env_clear {
            Variables::new_clean()
        } else {
            Variables::new()
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
            ev_exit: false,
            cancel: self.cancel,
            stdout_sink: self.stdout_sink,
            stderr_sink: self.stderr_sink,
            child_pids: Vec::new(),
            timeout: self.timeout.map(|d| std::time::Instant::now() + d),
            external_handler: self.external_handler,
            redirect_open_handler: self.redirect_open_handler,
            last_bg_pid: None,
            pending_stdin: None,
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
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
            ev_exit: false,
            cancel: None,
            stdout_sink: None,
            stderr_sink: None,
            child_pids: Vec::new(),
            timeout: None,
            external_handler: None,
            redirect_open_handler: None,
            last_bg_pid: None,
            pending_stdin: None,
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

    /// Check cancellation flag and timeout deadline.
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

    /// Write a string to stdout (sink or fd 1).
    /// Decodes PUA-encoded bytes back to raw bytes for non-UTF-8 preservation.
    pub(crate) fn write_out(&self, s: &str) {
        let data = crate::encoding::str_to_bytes(s);
        if let Some(ref sink) = self.stdout_sink {
            if let Ok(mut w) = sink.lock() {
                let _ = w.write_all(&data);
            }
        } else {
            // SAFETY: fd 1 (stdout) is always valid; data pointer and length are from a live Vec.
            sys::write(1, data.as_ptr() as *const _, data.len());
        }
    }

    /// Write a string to stderr (sink or fd 2).
    pub(crate) fn write_err(&self, s: &str) {
        let data = crate::encoding::str_to_bytes(s);
        if let Some(ref sink) = self.stderr_sink {
            if let Ok(mut w) = sink.lock() {
                let _ = w.write_all(&data);
            }
        } else {
            // SAFETY: fd 2 (stderr) is always valid; data pointer and length are from a live Vec.
            sys::write(2, data.as_ptr() as *const _, data.len());
        }
    }

    /// Write a formatted error message to stderr.
    pub(crate) fn err_msg(&self, msg: &str) {
        self.write_err(msg);
        if !msg.ends_with('\n') {
            self.write_err("\n");
        }
    }

    /// Wait for a child process, polling the cancel flag.
    /// On cancellation, kills the child's process group and returns Cancelled.
    /// In interactive mode, uses WUNTRACED to detect stopped processes.
    /// `pgid` is the process group (for Stopped reporting); 0 means use pid.
    fn wait_child(&mut self, pid: i32) -> crate::error::Result<ExitStatus> {
        self.wait_child_pgid(pid, pid)
    }

    fn wait_child_pgid(&mut self, pid: i32, pgid: i32) -> crate::error::Result<ExitStatus> {
        let wuntraced = if self.opts.interactive { 1 } else { 0 };

        // If no cancel flag or timeout, just block
        if self.cancel.is_none() && self.timeout.is_none() {
            let mut status = 0i32;
            // SAFETY: pid is a valid child PID obtained from fork().
            sys::waitpid(pid, &mut status, wuntraced);
            if sys::wifstopped(status) {
                // Don't remove from child_pids — process is still alive
                return Err(ShellError::Stopped { pid, pgid });
            }
            self.child_pids.retain(|&p| p != pid);
            return Ok(ExitStatus::from_wait(status));
        }

        // Spawn a thread to do blocking waitpid; check cancel while waiting.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut status = 0i32;
            // SAFETY: pid is a valid child PID obtained from fork().
            sys::waitpid(pid, &mut status, wuntraced);
            let _ = tx.send(status);
        });
        loop {
            // Check for result with a short timeout to allow cancel checks
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(status) => {
                    if sys::wifstopped(status) {
                        return Err(ShellError::Stopped { pid, pgid });
                    }
                    self.child_pids.retain(|&p| p != pid);
                    return Ok(ExitStatus::from_wait(status));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.child_pids.retain(|&p| p != pid);
                    return Ok(ExitStatus::FAILURE);
                }
            }
            if let Err(e) = self.check_cancel() {
                // Cancel or timeout — kill and reap
                // SAFETY: pid is a valid child PID; negated for process group kill. SIGKILL is always valid.
                let _ = rustix::process::kill_process_group(
                    rustix::process::Pid::from_raw(pid).unwrap(),
                    rustix::process::Signal::KILL,
                );
                // The thread will complete waitpid and send the status; just drain it
                let _ = rx.recv();
                self.child_pids.retain(|&p| p != pid);
                return Err(e);
            }
        }
    }

    /// Kill all tracked child processes (process groups).
    fn kill_children(&mut self) {
        for &pid in &self.child_pids {
            // Kill the process group (negative PID)
            // SAFETY: pid is a valid child PID from fork(); negated for process group kill.
            let _ = rustix::process::kill_process_group(
                rustix::process::Pid::from_raw(pid).unwrap(),
                rustix::process::Signal::KILL,
            );
        }
        // Reap them
        for &pid in &self.child_pids {
            let mut status = 0i32;
            // SAFETY: pid is a valid child PID; reaping after kill.
            sys::waitpid(pid, &mut status, 0);
        }
        self.child_pids.clear();
    }

    /// Build the xtrace prefix string ($PS4, default "+ ").
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
        self.external_handler = Some(handler);
    }

    /// Install a provider that owns the open of writable redirections
    /// (`>`, `>|`, `>>`, `<>`). See [`RedirectOpenHandler`].
    pub fn set_redirect_open_handler(&mut self, handler: RedirectOpenHandler) {
        self.redirect_open_handler = Some(handler);
    }

    pub(crate) fn has_external_handler(&self) -> bool {
        self.external_handler.is_some()
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
        self.resolve_path_bytes(&ShellBytes::from_str_lossless(p))
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
                return 2;
            }
        };

        self.run_program(&program).code()
    }

    /// Execute a pre-parsed program. Returns the final exit status.
    /// This is the library-facing entry point — parse once with `Parser`,
    /// then execute via `run_program`.
    pub fn run_program(&mut self, program: &crate::ast::Program) -> ExitStatus {
        for cmd in &program.commands {
            match self.eval_command(cmd) {
                Ok(status) => {
                    self.exit_status = status;
                }
                Err(ShellError::Exit(n)) => {
                    self.run_exit_trap();
                    return n;
                }
                Err(ShellError::Cancelled) | Err(ShellError::TimedOut) => {
                    self.kill_children();
                    return ExitStatus::from_signal(2); // 130 = SIGINT
                }
                Err(ShellError::Stopped { .. }) => {
                    // A foreground process was stopped (SIGTSTP). Return 148
                    // (128 + SIGTSTP) so the embedder can detect and save the job.
                    // Do not print anything — the embedder handles the UI.
                    return ExitStatus::from(148);
                }
                Err(e) => {
                    self.err_msg(&format!("epsh: {e}"));
                    self.exit_status = ExitStatus::MISUSE;
                    self.run_exit_trap();
                    return self.exit_status;
                }
            }
            self.run_pending_traps();
        }

        self.run_exit_trap();
        self.exit_status
    }

    /// Check for pending signals and run their trap handlers.
    fn run_pending_traps(&mut self) {
        for sig_name in crate::signal::take_pending() {
            if let Some(action) = self.traps.get(sig_name) {
                // Empty action means ignore (trap '' SIG)
                if action.is_empty() {
                    continue;
                }
                let action = action.clone();
                let mut parser = Parser::new(&action);
                if let Ok(program) = parser.parse() {
                    for cmd in &program.commands {
                        let _ = self.eval_command(cmd);
                    }
                }
            }
        }
    }

    /// Run the EXIT trap if one is set.
    pub(crate) fn run_exit_trap(&mut self) {
        // Remove the trap to prevent recursive invocation
        if let Some(action) = self.traps.remove("EXIT") {
            let mut parser = Parser::new(&action);
            if let Ok(program) = parser.parse() {
                for cmd in &program.commands {
                    let _ = self.eval_command(cmd);
                }
            }
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

    /// Expand a word to a list of fields (with field splitting and globbing).
    pub(crate) fn expand_fields(&mut self, word: &Word) -> crate::error::Result<Vec<String>> {
        expand::expand_word_to_fields(word, self)
    }

    /// Expand a word to a single string (no field splitting or globbing).
    pub(crate) fn expand_string(&mut self, word: &Word) -> crate::error::Result<String> {
        expand::expand_word_to_string(word, self)
    }

    /// Expand word parts into a fnmatch-ready pattern string.
    /// Glob chars from quoted regions are escaped.
    pub(crate) fn expand_pattern(&mut self, word: &Word) -> crate::error::Result<String> {
        expand::expand_pattern(&word.parts, self)
    }

    /// Evaluate a command node, returning its exit status.
    pub fn eval_command(&mut self, cmd: &Command) -> crate::error::Result<ExitStatus> {
        self.check_cancel()?;
        if self.opts.noexec {
            return Ok(ExitStatus::SUCCESS);
        }
        let status = self.eval_command_inner(cmd)?;
        self.exit_status = status;

        // Errexit check — mirrors dash's eval.c line 330.
        // Only fires from "leaf" nodes (simple commands, pipelines, subshells)
        // where checkexit = EV_TESTED. Compound commands (if, while, for, etc.)
        // propagate status but don't trigger errexit directly.
        // Bang (!) pipelines suppress errexit on the inverted result (POSIX).
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
        // ev_exit only applies to the final simple command in an exit context.
        // Clear it for compound commands so nested commands don't exec-direct.
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
                // bang (!) suppresses errexit for both the inner pipeline
                // AND the inverted result (POSIX requirement).
                let saved = self.tested;
                if *bang {
                    self.tested = true;
                }
                let status = self.eval_pipeline(commands)?;
                self.tested = saved;
                Ok(if *bang { status.inverted() } else { status })
            }

            Command::And(left, right) => {
                // Left side of && is always tested (errexit suppressed)
                let saved = self.tested;
                self.tested = true;
                let status = self.eval_command(left)?;
                self.tested = saved;
                self.exit_status = status;
                if status.success() {
                    // Right side inherits parent's tested flag (like dash)
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
                if self.in_forked_child {
                    // Already in a forked child (pipeline stage, command subst) —
                    // execute directly without double-forking. Mirrors dash's
                    // EV_EXIT optimization. Prevents pipe fd leaks.
                    let saved = self.setup_redirections(redirs)?;
                    let result = self.eval_command(body);
                    self.restore_redirections(saved);
                    result
                } else {
                    self.eval_in_subshell(body, redirs)
                }
            }

            Command::BraceGroup {
                body,
                redirs,
                span: _,
            } => {
                let saved = self.setup_redirections(redirs)?;
                let result = self.eval_command(body);
                self.restore_redirections(saved);
                result
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
                    // No 'in' clause: use positional parameters
                    self.vars.positional_shell_strings()
                };

                self.loop_depth += 1;
                let mut last_status = ExitStatus::SUCCESS;
                for value in &word_list {
                    self.check_cancel()?;
                    if let Err(e) = self.vars.set(var, value) {
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
                        if glob::fnmatch(&pat, &expanded) {
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
                let status = self.eval_command(cmd)?;
                self.tested = saved;
                Ok(status.inverted())
            }

            Command::Background { cmd, redirs } => {
                // Fork and run in background
                self.eval_background(cmd, redirs)
            }
        }
    }

    /// Evaluate a simple command (the most common case).
    fn eval_simple(
        &mut self,
        assigns: &[Assignment],
        args: &[Word],
        redirs: &[Redir],
        span: Span,
    ) -> crate::error::Result<ExitStatus> {
        // Expand arguments
        let mut expanded_args: Vec<String> = Vec::new();
        for arg in args {
            expanded_args.extend(self.expand_fields(arg)?);
        }

        // No command name: just apply assignments to the shell environment.
        // Return current exit_status (may have been set by command substitutions
        // during argument expansion — e.g., `$(false)` with empty result).
        if expanded_args.is_empty() {
            // xtrace for bare assignments
            if self.opts.xtrace && !assigns.is_empty() {
                let mut trace = self.xtrace_prefix();
                for assign in assigns {
                    let value = self.expand_string(&assign.value)?;
                    if !trace.ends_with(' ') {
                        trace.push(' ');
                    }
                    trace.push_str(&assign.name);
                    trace.push('=');
                    trace.push_str(&value);
                }
                trace.push('\n');
                self.write_err(&trace);
            }
            for assign in assigns {
                let value = self.expand_string(&assign.value)?;
                self.vars
                    .set(&assign.name, &value)
                    .map_err(|msg| ShellError::Runtime { msg, span })?;
            }
            return Ok(self.exit_status);
        }

        // xtrace: print $PS4, assignments, then args (like dash's evalcommand)
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

        // exec is special: its redirections are permanent (no save/restore)
        let is_exec = cmd_name == "exec";

        // Setup redirections before executing (applies to builtins, functions, externals)
        let saved_fds = self.setup_redirections(redirs)?;

        // Every outcome, including builtin errors, falls through to the
        // descriptor restoration below.
        let result = match self.try_builtin(cmd_name, &expanded_args, assigns, &[], span) {
            Ok(Some(status)) => Ok(status),
            Err(e) => Err(e),
            Ok(None) => {
                if let Some(func_body) = self.functions.get(cmd_name).cloned() {
                    self.ev_exit = false; // function body may have multiple commands
                    self.eval_function(&func_body, &expanded_args, assigns, &[], span)
                } else if self.external_handler.is_some() {
                    self.call_external_handler(&expanded_args, assigns)
                } else {
                    self.eval_external(&expanded_args, assigns, &[], span)
                }
            }
        };

        if is_exec {
            // exec redirections are permanent — close saved copies instead of restoring
            for s in saved_fds {
                if let Some(copy) = s.saved_copy {
                    // SAFETY: copy is a valid fd from fcntl_dupfd_cloexec.
                    sys::close(copy);
                }
            }
        } else {
            self.restore_redirections(saved_fds);
        }
        result
    }

    /// Run an external command through the embedder's handler.
    ///
    /// Builds the complete child environment for the handler: inherited
    /// non-shell-name entries, exported shell variables, and prefix
    /// assignments (which override exported values). This mirrors what
    /// eval_external would hand to exec, so the handler's child inherits
    /// nothing from the embedder's process environment beyond the store.
    pub(crate) fn call_external_handler(
        &mut self,
        args: &[String],
        assigns: &[Assignment],
    ) -> crate::error::Result<ExitStatus> {
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
        let args_bytes: Vec<ShellBytes> = args.iter().cloned().map(Into::into).collect();
        let handler = self
            .external_handler
            .as_mut()
            .expect("call_external_handler requires an installed handler");
        let previous = EXTERNAL_COMMAND_CWD.with(|cwd| cwd.borrow_mut().replace(self.cwd.clone()));
        let result = handler(&args_bytes, &env_pairs);
        EXTERNAL_COMMAND_CWD.with(|cwd| *cwd.borrow_mut() = previous);
        result
    }

    /// Evaluate a function call.
    fn eval_function(
        &mut self,
        body: &Command,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        // Save and set positional parameters (take avoids clone)
        let saved_positional = std::mem::replace(
            &mut self.vars.positional,
            args[1..].iter().cloned().map(Into::into).collect(),
        );

        // Push scope for local variables
        self.vars.push_scope();

        // Apply temporary assignments
        for assign in assigns {
            let value = self.expand_string(&assign.value)?;
            self.vars.make_local(&assign.name);
            let _ = self.vars.set(&assign.name, &value);
        }

        // Setup redirections
        let saved_fds = self.setup_redirections(redirs)?;

        // Execute function body
        let result = match self.eval_command(body) {
            Ok(s) => Ok(s),
            Err(ShellError::Return(n)) => Ok(n),
            Err(e) => Err(e),
        };

        // Restore state
        self.restore_redirections(saved_fds);
        self.vars.pop_scope();
        self.vars.positional = saved_positional;

        result
    }

    /// Handle exec/spawn failure — report error and return appropriate status.
    fn handle_exec_error(
        &self,
        e: &std::io::Error,
        cmd_name: &str,
    ) -> crate::error::Result<ExitStatus> {
        // An executable script whose interpreter is broken surfaces as a bare
        // ENOTDIR/ENOENT/EACCES; report it bash-style instead of a raw errno.
        if let Some(code) = e.raw_os_error() {
            let path_env = self
                .vars
                .get("PATH")
                .unwrap_or("/usr/local/bin:/usr/bin:/bin");
            if let Some(msg) = bad_interpreter_message(cmd_name, code, &self.cwd, path_env) {
                self.err_msg(&msg);
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

    /// Execute an external command.
    pub(crate) fn eval_external(
        &mut self,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        self.check_cancel()?;
        let saved = self.setup_redirections(redirs)?;
        let args_bytes: Vec<ShellBytes> = args.iter().cloned().map(Into::into).collect();
        let mut assign_bytes = Vec::new();
        for assign in assigns {
            let value = self.expand_string(&assign.value)?;
            assign_bytes.push((assign.name.clone(), ShellBytes::from_str_lossless(&value)));
        }

        // When ev_exit is set (pipeline child about to _exit), exec directly
        // instead of fork+exec. Mirrors dash's shellexec fast-path.
        if self.ev_exit {
            // Avoid mutating the process environment after fork: another thread
            // may hold Rust's environment lock when the child is created.
            let env = self.vars.env_for_command_os(&assign_bytes);
            let _ = std::env::set_current_dir(&self.cwd);
            let err = crate::builtins::exec::execve_with_env(&args_bytes[0], &args_bytes, &env);
            self.restore_redirections(saved);
            return self.handle_exec_error(&err, &args[0]);
        }

        let mut cmd = std::process::Command::new(args_bytes[0].to_os_string());
        cmd.args(args_bytes.iter().skip(1).map(ShellBytes::to_os_string));
        cmd.current_dir(&self.cwd);
        // NOTE: no pre_exec here — allows Rust to use posix_spawn (1.4x faster).
        // Process group isolation is done from parent after spawn via setpgid.

        // Set environment: exported vars + temporary assignments
        cmd.env_clear();
        for (k, v) in self.vars.env_for_command_os(&assign_bytes) {
            cmd.env(k, v);
        }

        // When sinks are set, capture child stdout/stderr and relay to sinks.
        if self.stdout_sink.is_some() {
            cmd.stdout(std::process::Stdio::piped());
        }
        if self.stderr_sink.is_some() {
            cmd.stderr(std::process::Stdio::piped());
        }
        // Consume a pending heredoc stdin fd: setup_redirections stored this
        // instead of dup2-ing onto fd 0 to avoid racing with the host's stdin reader.
        if let Some(fd) = self.pending_stdin.take() {
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            cmd.stdin(file);
        }

        let result = match cmd.spawn() {
            Ok(mut child) => {
                let child_id = child.id() as i32;
                // Isolate in own process group from parent (allows posix_spawn path)
                // SAFETY: child_id is a valid PID just returned by spawn().
                let _ = rustix::process::setpgid(
                    rustix::process::Pid::from_raw(child_id),
                    rustix::process::Pid::from_raw(child_id),
                );
                self.child_pids.push(child_id);

                // Spawn relay threads for sinks, keeping handles to join later
                let mut handles = Vec::new();
                if let Some(stdout) = child.stdout.take() {
                    handles.push(spawn_relay(stdout, self.stdout_sink.clone().unwrap()));
                }
                if let Some(stderr) = child.stderr.take() {
                    handles.push(spawn_relay(stderr, self.stderr_sink.clone().unwrap()));
                }

                // Cancel-aware wait: spawn thread for blocking wait, check cancel in loop
                let wait_result = if self.cancel.is_some() || self.timeout.is_some() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(child.wait());
                    });
                    loop {
                        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                            Ok(result) => break result,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                break Err(std::io::Error::other("wait thread panicked"));
                            }
                        }
                        if let Err(e) = self.check_cancel() {
                            // Kill via PID since child was moved to the thread
                            // SAFETY: child_id is a valid PID from the spawned process.
                            let _ = rustix::process::kill_process(
                                rustix::process::Pid::from_raw(child_id).unwrap(),
                                rustix::process::Signal::KILL,
                            );
                            let _ = rx.recv(); // wait for the thread to finish
                            self.child_pids.retain(|&p| p != child_id);
                            for h in handles {
                                let _ = h.join();
                            }
                            self.restore_redirections(saved);
                            return Err(e);
                        }
                    }
                } else {
                    child.wait()
                };
                self.child_pids.retain(|&p| p != child_id);

                // Join relay threads to ensure all output is flushed
                for h in handles {
                    let _ = h.join();
                }

                match wait_result {
                    Ok(status) => Ok(ExitStatus::from(status.code().unwrap_or(128))),
                    Err(e) => Err(ShellError::Io(e)),
                }
            }
            Err(e) => self.handle_exec_error(&e, &args[0]),
        };

        self.restore_redirections(saved);
        result
    }

    /// Execute a pipeline.
    fn eval_pipeline(&mut self, commands: &[Command]) -> crate::error::Result<ExitStatus> {
        if commands.len() == 1 {
            return self.eval_command(&commands[0]);
        }

        // Create pipes between stages
        let mut pipes: Vec<(RawFd, RawFd)> = Vec::new();
        for _ in 0..commands.len() - 1 {
            let mut fds = [0i32; 2];
            // SAFETY: fds is a valid 2-element array for pipe() to write into.
            if sys::pipe(fds.as_mut_ptr()) != 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }
            pipes.push((fds[0], fds[1]));
        }

        let mut children = Vec::new();
        let mut pgid: i32 = 0; // first child's PID becomes the pipeline's PGID

        for (i, cmd) in commands.iter().enumerate() {
            // SAFETY: Standard POSIX fork/dup2/close/setpgid pattern. Pipe fds are
            // valid from pipe() above. Child sets up its stdin/stdout from pipe fds
            // then closes all pipe fds before executing.
            unsafe {
                let pid = sys::fork();
                if pid < 0 {
                    // Fork failed — clean up pipes and kill already-forked children
                    for &(r, w) in &pipes {
                        sys::close(r);
                        sys::close(w);
                    }
                    self.kill_children();
                    return Err(ShellError::Io(std::io::Error::last_os_error()));
                }

                if pid == 0 {
                    // Child: join pipeline process group
                    // First child creates the group (pgid==0 → setpgid(0,0))
                    // Subsequent children join it
                    let _ = rustix::process::setpgid(
                        None,
                        if pgid == 0 {
                            None
                        } else {
                            rustix::process::Pid::from_raw(pgid)
                        },
                    );
                    self.in_forked_child = true;
                    // Connect stdin from previous pipe
                    if i > 0 {
                        sys::dup2(pipes[i - 1].0, 0);
                    }
                    // Connect stdout to next pipe
                    if i < commands.len() - 1 {
                        sys::dup2(pipes[i].1, 1);
                    }
                    // Close all pipe fds
                    for &(read_fd, write_fd) in &pipes {
                        sys::close(read_fd);
                        sys::close(write_fd);
                    }

                    // Mark this as exit context — eval_external can exec
                    // directly instead of fork+exec (dash's EV_EXIT).
                    self.ev_exit = true;
                    let status = match self.eval_command(cmd) {
                        Ok(s) => s,
                        Err(ShellError::Exit(n)) => n,
                        Err(_) => ExitStatus::FAILURE,
                    };
                    sys::exit_child(status);
                }

                // Parent: also call setpgid to avoid race with child
                if pgid == 0 {
                    pgid = pid; // first child's PID is the group leader
                }
                let _ = rustix::process::setpgid(
                    rustix::process::Pid::from_raw(pid),
                    rustix::process::Pid::from_raw(pgid),
                );
                children.push(pid);
                self.child_pids.push(pid);
            }
        }

        // Parent: close all pipe fds
        for &(read_fd, write_fd) in &pipes {
            // SAFETY: fds are valid from pipe() and not yet closed in parent.
            sys::close(read_fd);
            sys::close(write_fd);
        }

        // Interactive mode: give the pipeline the terminal
        if self.opts.interactive && pgid > 0 {
            // SAFETY: pgid is a valid process group from setpgid; fd 0 is stdin.
            let _ = rustix::termios::tcsetpgrp(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
                rustix::process::Pid::from_raw(pgid).unwrap(),
            );
        }

        // Wait for all children, checking cancel between stages
        let mut last_status = ExitStatus::SUCCESS;
        let mut pipefail_status = ExitStatus::SUCCESS;
        for (i, &pid) in children.iter().enumerate() {
            match self.wait_child_pgid(pid, pgid) {
                Ok(stage_status) => {
                    if !stage_status.success() {
                        pipefail_status = stage_status;
                    }
                    if i == children.len() - 1 {
                        last_status = stage_status;
                    }
                }
                Err(ShellError::Stopped { pid: _, pgid: _ }) if self.opts.interactive => {
                    // Reclaim terminal before propagating
                    // SAFETY: getpgrp() and tcsetpgrp are always safe with valid fd.
                    let _ = rustix::termios::tcsetpgrp(
                        unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
                        rustix::process::getpgrp(),
                    );
                    return Err(ShellError::Stopped {
                        pid: children[children.len() - 1],
                        pgid,
                    });
                }
                Err(e @ (ShellError::Cancelled | ShellError::TimedOut)) => {
                    if self.opts.interactive {
                        let _ = rustix::termios::tcsetpgrp(
                            unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
                            rustix::process::getpgrp(),
                        );
                    }
                    self.kill_children();
                    return Err(e);
                }
                Err(e) => {
                    if self.opts.interactive {
                        let _ = rustix::termios::tcsetpgrp(
                            unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
                            rustix::process::getpgrp(),
                        );
                    }
                    return Err(e);
                }
            }
        }

        // Interactive mode: reclaim the terminal
        if self.opts.interactive && pgid > 0 {
            // SAFETY: getpgrp() returns our process group; fd 0 is stdin.
            let _ = rustix::termios::tcsetpgrp(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
                rustix::process::getpgrp(),
            );
        }

        if self.opts.pipefail && !pipefail_status.success() {
            Ok(pipefail_status)
        } else {
            Ok(last_status)
        }
    }

    /// Execute a command in a subshell (fork).
    fn eval_in_subshell(
        &mut self,
        body: &Command,
        redirs: &[Redir],
    ) -> crate::error::Result<ExitStatus> {
        // SAFETY: Standard POSIX fork pattern. Child isolates into its own
        // process group, executes the body, then _exit()s. Parent waits.
        unsafe {
            let pid = sys::fork();
            if pid < 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
                // Child — isolate in own process group
                let _ = rustix::process::setpgid(None, None);
                let parent_exit_trap = self
                    .traps
                    .remove("EXIT")
                    .or_else(|| self.traps.remove("exit"));
                drop(parent_exit_trap); // parent EXIT trap doesn't run in subshell
                self.in_forked_child = true;

                let _saved = match self.setup_redirections(redirs) {
                    Ok(s) => s,
                    Err(_) => sys::exit_child(ExitStatus::FAILURE),
                };
                let status = match self.eval_command(body) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => ExitStatus::FAILURE,
                };
                // Run subshell's own EXIT trap if it set one
                self.run_exit_trap();
                sys::exit_child(status);
            }

            // Parent
            self.child_pids.push(pid);
            self.wait_child(pid)
        }
    }

    /// Execute a command in the background.
    fn eval_background(
        &mut self,
        cmd: &Command,
        redirs: &[Redir],
    ) -> crate::error::Result<ExitStatus> {
        // SAFETY: Standard POSIX fork pattern. Child isolates into its own
        // process group and runs the command. Parent does not wait (background).
        unsafe {
            let pid = sys::fork();
            if pid < 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
                let _ = rustix::process::setpgid(None, None);
                let _saved = match self.setup_redirections(redirs) {
                    Ok(s) => s,
                    Err(_) => sys::exit_child(ExitStatus::FAILURE),
                };
                let status = match self.eval_command(cmd) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => ExitStatus::FAILURE,
                };
                sys::exit_child(status);
            }

            self.child_pids.push(pid);
            self.last_bg_pid = Some(pid as u32);
            // Don't wait — background process
            Ok(ExitStatus::SUCCESS)
        }
    }

    /// Execute a command substitution and return its output.
    pub fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String> {
        // In-process command substitution for safe builtins: capture output without forking.
        // Pure builtins (echo, printf, true, false, :, pwd, type, test/[) don't
        // modify shell state, so running them in-process is safe and avoids the
        // fork+pipe+waitpid overhead entirely.
        if let Command::Simple {
            assigns,
            args,
            redirs,
            span,
        } = cmd
            && assigns.is_empty()
            && redirs.is_empty()
            && !args.is_empty()
        {
            let mut expanded = Vec::new();
            let expand_ok = (|| -> crate::error::Result<()> {
                for a in args {
                    expanded.extend(self.expand_fields(a)?);
                }
                Ok(())
            })();
            if expand_ok.is_ok()
                && !expanded.is_empty()
                && is_command_substitution_builtin(&expanded[0])
            {
                // Capture output to buffer instead of forking
                let saved_sink = self.stdout_sink.take();
                let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
                self.stdout_sink = Some(buf.clone());
                let status = self.try_builtin(&expanded[0], &expanded, &[], &[], *span);
                self.stdout_sink = saved_sink;
                if let Ok(Some(s)) = status {
                    self.exit_status = s;
                    let captured = buf.lock().unwrap();
                    let mut output = crate::encoding::bytes_to_str(&captured);
                    while output.ends_with('\n') {
                        output.pop();
                    }
                    return Ok(output);
                }
                // Fallback: not a builtin after all, continue to fork path
            }
        }

        // $(<file) optimization: read file directly without forking
        if let Command::Simple {
            assigns,
            args,
            redirs,
            ..
        } = cmd
            && assigns.is_empty()
            && args.is_empty()
            && redirs.len() == 1
            && let RedirKind::Input(ref word) = redirs[0].kind
        {
            let filename = self.expand_string(word)?;
            let filepath = self.resolve_path_bytes(&ShellBytes::from_str_lossless(&filename));
            match std::fs::read(&filepath) {
                Ok(bytes) => {
                    let mut content = crate::encoding::bytes_to_str(&bytes);
                    // Remove trailing newlines (like command substitution)
                    while content.ends_with('\n') {
                        content.pop();
                    }
                    self.exit_status = ExitStatus::SUCCESS;
                    return Ok(content);
                }
                Err(e) => {
                    self.err_msg(&format!("epsh: {filename}: {e}"));
                    self.exit_status = ExitStatus::FAILURE;
                    return Err(ShellError::Runtime {
                        msg: format!("{filename}: {e}"),
                        span: redirs[0].span,
                    });
                }
            }
        }

        let mut fds = [0i32; 2];
        // SAFETY: Standard POSIX pipe+fork pattern for command substitution.
        // Child redirects stdout to write end of pipe, parent reads from read end.
        // All fds are valid from pipe(). from_raw_fd takes ownership of fds[0].
        unsafe {
            if sys::pipe(fds.as_mut_ptr()) != 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            let pid = sys::fork();
            if pid < 0 {
                sys::close(fds[0]);
                sys::close(fds[1]);
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
                // Child: redirect stdout to write end of pipe
                let _ = rustix::process::setpgid(None, None);
                self.in_forked_child = true;
                sys::close(fds[0]);
                sys::dup2(fds[1], 1);
                sys::close(fds[1]);

                let status = match self.eval_command(cmd) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => ExitStatus::FAILURE,
                };
                self.run_exit_trap();
                sys::exit_child(status);
            }

            // Parent: read from read end
            sys::close(fds[1]);
            self.child_pids.push(pid);

            let mut raw_output = Vec::new();
            let mut read_file = std::fs::File::from_raw_fd(fds[0]);
            let _ = read_file.read_to_end(&mut raw_output);
            let mut output = crate::encoding::bytes_to_str(&raw_output);

            self.exit_status = self.wait_child(pid)?;

            // Remove trailing newlines (POSIX requirement)
            while output.ends_with('\n') {
                output.pop();
            }

            Ok(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Use a counter that doesn't need arithmetic expansion
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
        // Simple pipeline test using external commands
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

        let status = shell.run_script(&format!(
            "/usr/bin/env | /usr/bin/grep EPSH_PIPELINE_ENV > {output_path}"
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
        // set -e should cause exit on first failure
        // The script sets X=before, then false fails, so X=after never runs
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
        // Subshell changes shouldn't affect parent
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
        // echo receives the output of the subcommand
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

    // Regression test: heredoc stdin must not dup2 fd 0 in the parent process.
    //
    // When epsh runs on a background thread with stdout/stderr sinks (as nerv
    // does), the old code did dup2(pipe_read_fd, 0) in the parent to set up
    // heredoc stdin.  dup2 is process-wide, so between setup_redirections and
    // restore_redirections the main thread's fd 0 pointed at the (soon-to-be-
    // exhausted) pipe instead of the terminal.  The main thread read EOF and
    // set should_quit, causing nerv to exit mid-session.
    //
    // The fix: when sinks are set, store the heredoc pipe read fd in
    // Shell::pending_stdin and apply it via cmd.stdin() after fork so that
    // the parent's fd 0 is never touched.
    #[test]
    fn heredoc_with_sinks_does_not_corrupt_fd0() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let corrupted = Arc::new(AtomicBool::new(false));
        let corrupted_clone = corrupted.clone();

        // Watcher thread: continuously polls fd 0 to detect corruption.
        // fcntl(0, F_GETFD) returns -1 if fd 0 is closed/replaced with a pipe
        // that has already been consumed.
        let watcher = std::thread::spawn(move || {
            for _ in 0..2000 {
                let r = rustix::io::fcntl_getfd(unsafe { std::os::fd::BorrowedFd::borrow_raw(0) });
                if r.is_err() {
                    corrupted_clone.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        });

        // Run 20 heredoc commands via sinks on a background thread, mirroring
        // how nerv's epsh tool runs commands.
        let worker = std::thread::spawn(move || {
            for i in 0..20 {
                let out: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
                let out2 = out.clone();
                let script = format!("python3 - << 'PYEOF'\nprint('iter {i}')\nPYEOF");
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
