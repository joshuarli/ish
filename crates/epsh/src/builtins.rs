use crate::ast::*;
use crate::error::{ExitStatus, ShellError, Span};
use crate::eval::Shell;
use crate::parser::Parser;
use crate::shell_bytes::ShellBytes;
use crate::sys;
use crate::test_cmd::test_eval;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

fn kill_with_rustix(pid: i32, signum: i32) -> Result<(), rustix::io::Errno> {
    if signum == 0 {
        return match pid {
            0 => rustix::process::test_kill_current_process_group(),
            pid if pid > 0 => {
                let pid = rustix::process::Pid::from_raw(pid).unwrap();
                rustix::process::test_kill_process(pid)
            }
            pid => match pid.checked_neg().and_then(rustix::process::Pid::from_raw) {
                Some(group) => rustix::process::test_kill_process_group(group),
                None => Err(rustix::io::Errno::INVAL),
            },
        };
    }

    let Some(signal) = rustix::process::Signal::from_named_raw(signum) else {
        return Err(rustix::io::Errno::INVAL);
    };
    match pid {
        0 => rustix::process::kill_current_process_group(signal),
        pid if pid > 0 => {
            let pid = rustix::process::Pid::from_raw(pid).unwrap();
            rustix::process::kill_process(pid, signal)
        }
        pid => match pid.checked_neg().and_then(rustix::process::Pid::from_raw) {
            Some(group) => rustix::process::kill_process_group(group, signal),
            None => Err(rustix::io::Errno::INVAL),
        },
    }
}

impl Shell {
    /// Try to run a builtin command. Returns None if not a builtin.
    pub(crate) fn try_builtin(
        &mut self,
        name: &str,
        args: &[String],
        _assigns: &[Assignment],
        redirs: &[Redir],
        span: Span,
    ) -> crate::error::Result<Option<ExitStatus>> {
        let status = match name {
            ":" | "true" => Some(ExitStatus::SUCCESS),
            "false" => Some(ExitStatus::FAILURE),
            "echo" => Some(self.builtin_echo(args)),
            "cd" => Some(self.builtin_cd(args)),
            "pwd" => {
                let cwd = ShellBytes::from_os_str(self.cwd.as_os_str()).to_shell_string();
                self.write_out(&format!("{cwd}\n"));
                Some(ExitStatus::SUCCESS)
            }
            "exit" => {
                let code = args
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .map(ExitStatus::from)
                    .unwrap_or(self.exit_status);
                return Err(ShellError::Exit(code));
            }
            "return" => {
                let code = args
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .map(ExitStatus::from)
                    .unwrap_or(self.exit_status);
                return Err(ShellError::Return(code));
            }
            "break" => {
                if let Some(arg) = args.get(1) {
                    match arg.parse::<usize>() {
                        Ok(0) => {
                            self.err_msg(&format!("break: Illegal number: {arg}"));
                            return Err(ShellError::Exit(ExitStatus::FAILURE));
                        }
                        Ok(n) => {
                            if self.loop_depth == 0 {
                                Some(ExitStatus::SUCCESS)
                            } else {
                                return Err(ShellError::Break(n.min(self.loop_depth)));
                            }
                        }
                        Err(_) => {
                            self.err_msg(&format!("break: Illegal number: {arg}"));
                            return Err(ShellError::Exit(ExitStatus::FAILURE));
                        }
                    }
                } else if self.loop_depth == 0 {
                    Some(ExitStatus::SUCCESS)
                } else {
                    return Err(ShellError::Break(1));
                }
            }
            "continue" => {
                if let Some(arg) = args.get(1) {
                    match arg.parse::<usize>() {
                        Ok(0) => {
                            self.err_msg(&format!("continue: Illegal number: {arg}"));
                            return Err(ShellError::Exit(ExitStatus::FAILURE));
                        }
                        Ok(n) => {
                            if self.loop_depth == 0 {
                                Some(ExitStatus::SUCCESS)
                            } else {
                                return Err(ShellError::Continue(n.min(self.loop_depth)));
                            }
                        }
                        Err(_) => {
                            self.err_msg(&format!("continue: Illegal number: {arg}"));
                            return Err(ShellError::Exit(ExitStatus::FAILURE));
                        }
                    }
                } else if self.loop_depth == 0 {
                    Some(ExitStatus::SUCCESS)
                } else {
                    return Err(ShellError::Continue(1));
                }
            }
            "export" => Some(self.builtin_export(args)),
            "readonly" => Some(self.builtin_readonly(args)),
            "unset" => Some(self.builtin_unset(args)),
            "set" => Some(self.builtin_set(args)),
            "shift" => Some(self.builtin_shift(args)),
            "eval" => return self.builtin_eval(args).map(Some),
            "." | "source" => Some(self.builtin_dot(args, span)?),
            "test" | "[" => Some(self.builtin_test(args)),
            "read" => Some(self.builtin_read(args)),
            "local" => Some(self.builtin_local(args)),
            "exec" => return self.builtin_exec(args, redirs, span).map(Some),
            "command" => Some(self.builtin_command(args, span)?),
            "type" => Some(self.builtin_type(args)),
            "wait" => Some(self.builtin_wait(args)),
            "trap" => Some(self.builtin_trap(args)),
            "kill" => Some(self.builtin_kill(args)),
            "umask" => Some(self.builtin_umask(args)),
            "getopts" => Some(self.builtin_getopts(args)),
            "printf" => Some(self.builtin_printf(args)),
            _ => None,
        };

        Ok(status)
    }

    fn builtin_echo(&self, args: &[String]) -> ExitStatus {
        let mut i = 1;
        let mut newline = true;
        let mut escape = false;

        // Check for -n and -e/-E flags
        while i < args.len() {
            if args[i] == "-n" {
                newline = false;
                i += 1;
            } else if args[i] == "-e" {
                escape = true;
                i += 1;
            } else if args[i] == "-E" {
                escape = false;
                i += 1;
            } else {
                break;
            }
        }

        let mut text = args[i..].join(" ");
        if escape {
            text = unescape_echo(&text);
        }
        if newline {
            text.push('\n');
        }
        self.write_out(&text);
        ExitStatus::SUCCESS
    }

    fn builtin_cd(&mut self, args: &[String]) -> ExitStatus {
        let dir = if args.len() > 1 {
            args[1].clone()
        } else {
            match self.vars.get_shell("HOME") {
                Some(h) => h,
                None => {
                    self.err_msg("cd: HOME not set");
                    return ExitStatus::FAILURE;
                }
            }
        };

        let path = PathBuf::from(ShellBytes::from_str_lossless(&dir).to_os_string());
        match self.resolve_cd_path(&path) {
            Ok(canonical) => {
                if canonical.is_dir() {
                    self.cwd = canonical;
                    self.cwd_is_canonical = true;
                    let _ = self
                        .vars
                        .set_bytes("PWD", ShellBytes::from_os_str(self.cwd.as_os_str()));
                    ExitStatus::SUCCESS
                } else {
                    self.err_msg(&format!("cd: {dir}: Not a directory"));
                    ExitStatus::FAILURE
                }
            }
            Err(e) => {
                self.err_msg(&format!("cd: {dir}: {e}"));
                ExitStatus::FAILURE
            }
        }
    }

    fn builtin_export(&mut self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            // Print all exported variables
            for (k, v) in self.vars.exported_env() {
                self.write_out(&format!("export {k}=\"{v}\"\n"));
            }
            return ExitStatus::SUCCESS;
        }

        for arg in &args[1..] {
            if let Some(eq) = arg.find('=') {
                let name = &arg[..eq];
                let value = &arg[eq + 1..];
                if let Err(e) = self.vars.set(name, value) {
                    self.err_msg(&format!("export: {e}"));
                    return ExitStatus::FAILURE;
                }
                self.vars.export(name);
            } else {
                self.vars.export(arg);
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_readonly(&mut self, args: &[String]) -> ExitStatus {
        for arg in &args[1..] {
            if let Some(eq) = arg.find('=') {
                let name = &arg[..eq];
                let value = &arg[eq + 1..];
                if let Err(e) = self.vars.set(name, value) {
                    self.err_msg(&format!("readonly: {e}"));
                    return ExitStatus::FAILURE;
                }
                self.vars.set_readonly(name);
            } else {
                self.vars.set_readonly(arg);
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_unset(&mut self, args: &[String]) -> ExitStatus {
        let mut status = ExitStatus::SUCCESS;
        let mut unset_funcs = false;
        let mut i = 1;

        while i < args.len() {
            if args[i] == "-v" {
                unset_funcs = false;
                i += 1;
            } else if args[i] == "-f" {
                unset_funcs = true;
                i += 1;
            } else {
                break;
            }
        }

        for arg in &args[i..] {
            if unset_funcs {
                self.functions.remove(arg.as_str());
            } else if let Err(e) = self.vars.unset(arg) {
                self.err_msg(&format!("unset: {e}"));
                status = ExitStatus::FAILURE;
            }
        }
        status
    }

    fn builtin_set(&mut self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            // `set` with no arguments lists exported environment variables.
            for (k, v) in self.vars.exported_env() {
                self.write_out(&format!("{k}={v}\n"));
            }
            return ExitStatus::SUCCESS;
        }

        let mut i = 1;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                i += 1;
                // Remaining args become positional parameters
                self.vars.positional = args[i..].iter().cloned().map(Into::into).collect();
                return ExitStatus::SUCCESS;
            } else if arg == "-" {
                // POSIX: bare "set -" turns off -x and -v
                self.opts.xtrace = false;
                i += 1;
            } else if (arg == "-o" || arg == "+o") && i + 1 < args.len() {
                let enable = arg == "-o";
                i += 1;
                match args[i].as_str() {
                    "pipefail" => self.opts.pipefail = enable,
                    "errexit" => self.opts.errexit = enable,
                    "nounset" => self.opts.nounset = enable,
                    "xtrace" => self.opts.xtrace = enable,
                    "noglob" => self.opts.noglob = enable,
                    "noexec" => self.opts.noexec = enable,
                    other => {
                        self.err_msg(&format!("set: unknown option: {other}"));
                        return ExitStatus::FAILURE;
                    }
                }
                i += 1;
            } else if arg.starts_with('-') || arg.starts_with('+') {
                let enable = arg.starts_with('-');
                for ch in arg[1..].chars() {
                    match ch {
                        'e' => self.opts.errexit = enable,
                        'u' => self.opts.nounset = enable,
                        'x' => self.opts.xtrace = enable,
                        'f' => self.opts.noglob = enable,
                        'n' => self.opts.noexec = enable,
                        _ => {
                            self.err_msg(&format!("set: unknown option: -{ch}"));
                            return ExitStatus::FAILURE;
                        }
                    }
                }
                i += 1;
            } else if crate::var::is_shell_name_bytes(arg.as_bytes()).is_some() {
                // `set NAME [value...]` sets and exports an environment
                // variable (the ish interactive contract). Remaining words are
                // joined with spaces into the value; `set NAME` sets an empty
                // value. Explicit positional assignment stays behind `--`.
                let name = arg.clone();
                let value = args[i + 1..].join(" ");
                if let Err(e) = self.vars.set(&name, &value) {
                    self.err_msg(&format!("set: {e}"));
                    return ExitStatus::FAILURE;
                }
                self.vars.export(&name);
                return ExitStatus::SUCCESS;
            } else {
                // Positional parameters
                self.vars.positional = args[i..].iter().cloned().map(Into::into).collect();
                return ExitStatus::SUCCESS;
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_shift(&mut self, args: &[String]) -> ExitStatus {
        let n = args
            .get(1)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1);
        if n > self.vars.positional.len() {
            self.err_msg("shift: can't shift that many");
            return ExitStatus::FAILURE;
        }
        self.vars.positional = self.vars.positional[n..].to_vec();
        ExitStatus::SUCCESS
    }

    /// Parse and execute a script inline (propagates control flow).
    /// Used by eval and dot builtins.
    fn run_inline(&mut self, source: &str) -> crate::error::Result<ExitStatus> {
        let mut parser = Parser::new(source);
        let program = match parser.parse() {
            Ok(p) => p,
            Err(e) => {
                self.err_msg(&format!("epsh: {e}"));
                return Ok(ExitStatus::MISUSE);
            }
        };
        if program.commands.is_empty() {
            return Ok(ExitStatus::SUCCESS);
        }
        let mut status = ExitStatus::SUCCESS;
        for cmd in &program.commands {
            status = self.eval_command(cmd)?;
            self.exit_status = status;
        }
        Ok(status)
    }

    fn builtin_eval(&mut self, args: &[String]) -> crate::error::Result<ExitStatus> {
        if args.len() <= 1 {
            return Ok(ExitStatus::SUCCESS);
        }
        self.run_inline(&args[1..].join(" "))
    }

    fn builtin_dot(&mut self, args: &[String], _span: Span) -> crate::error::Result<ExitStatus> {
        if args.len() <= 1 {
            self.err_msg(".: filename argument required");
            return Err(ShellError::Exit(ExitStatus::MISUSE));
        }
        let filename = &args[1];
        let filepath = self.resolve_path_bytes(&ShellBytes::from_str_lossless(filename));
        let content = match std::fs::read(&filepath) {
            Ok(bytes) => crate::encoding::bytes_to_str(&bytes),
            Err(e) => {
                self.err_msg(&format!(".: {filename}: {e}"));
                return Err(ShellError::Exit(ExitStatus::NOT_FOUND));
            }
        };
        // Catch return — it exits the dot script, not the shell
        match self.run_inline(&content) {
            Err(ShellError::Return(n)) => Ok(n),
            other => other,
        }
    }

    fn builtin_test(&self, args: &[String]) -> ExitStatus {
        let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let args = if args[0] == "[" {
            if args.last() != Some(&"]") {
                self.err_msg("[: missing ]");
                return ExitStatus::MISUSE;
            }
            &args[1..args.len() - 1]
        } else {
            &args[1..]
        };
        test_eval(args)
    }

    fn builtin_read(&mut self, args: &[String]) -> ExitStatus {
        // Parse options
        let mut i = 1;
        let mut raw_mode = false;
        while i < args.len() && args[i].starts_with('-') {
            if args[i] == "-r" {
                raw_mode = true;
            }
            i += 1;
        }

        let var_names: Vec<&str> = if i < args.len() {
            args[i..].iter().map(|s| s.as_str()).collect()
        } else {
            vec!["REPLY"]
        };

        // Read a line char-by-char from fd 0 (works with redirections)
        let mut line = String::new();
        let mut buf = [0u8; 1];
        let mut hit_eof = true; // assume EOF until we see a newline
        let mut continued = false;
        loop {
            // SAFETY: fd 0 (stdin) is valid; buf is a live 1-byte array.
            let n = sys::read(0, buf.as_mut_ptr().cast(), 1);
            if n <= 0 {
                break; // EOF or error
            }
            let ch = buf[0] as char;
            if ch == '\n' {
                if continued {
                    continued = false;
                    continue;
                }
                hit_eof = false;
                break;
            }
            if ch == '\\' && !raw_mode {
                // Line continuation: peek at next char
                // SAFETY: fd 0 (stdin) is valid; buf is a live 1-byte array.
                let n2 = sys::read(0, buf.as_mut_ptr().cast(), 1);
                if n2 <= 0 {
                    line.push('\\');
                    break;
                }
                if buf[0] == b'\n' {
                    continued = true;
                    continue;
                }
                line.push(buf[0] as char);
                continued = false;
                continue;
            }
            continued = false;
            line.push(ch);
        }

        if hit_eof && line.is_empty() {
            // Pure EOF with no data: set variables to empty
            for name in &var_names {
                if let Err(e) = self.vars.set(name, "") {
                    self.err_msg(&format!("read: {e}"));
                    return ExitStatus::FAILURE;
                }
            }
            return ExitStatus::FAILURE;
        }

        // Split on IFS and assign to variables
        let ifs = self.vars.ifs().to_string();
        if var_names.len() == 1 {
            // Single variable gets the whole line (with leading/trailing IFS stripped)
            let trimmed = line.trim_matches(|c: char| ifs.contains(c));
            if let Err(e) = self.vars.set(var_names[0], trimmed) {
                self.err_msg(&format!("read: {e}"));
                return ExitStatus::FAILURE;
            }
        } else {
            // Multiple variables: split on IFS, last gets remainder
            let mut fields = Vec::new();
            let chars: Vec<char> = line.chars().collect();
            let mut pos = 0;

            // Skip leading IFS whitespace
            while pos < chars.len() && ifs.contains(chars[pos]) && chars[pos].is_whitespace() {
                pos += 1;
            }

            for vi in 0..var_names.len() {
                if vi == var_names.len() - 1 {
                    // Last variable gets the rest
                    let rest: String = chars[pos..].iter().collect();
                    // Trim trailing IFS whitespace
                    let trimmed =
                        rest.trim_end_matches(|c: char| ifs.contains(c) && c.is_whitespace());
                    fields.push(trimmed.to_string());
                } else {
                    // Accumulate until next IFS char
                    let start = pos;
                    while pos < chars.len() && !ifs.contains(chars[pos]) {
                        pos += 1;
                    }
                    fields.push(chars[start..pos].iter().collect());
                    // Skip IFS delimiters
                    while pos < chars.len() && ifs.contains(chars[pos]) {
                        pos += 1;
                    }
                }
            }

            for (vi, name) in var_names.iter().enumerate() {
                let value = fields.get(vi).map(|s| s.as_str()).unwrap_or("");
                if let Err(e) = self.vars.set(name, value) {
                    self.err_msg(&format!("read: {e}"));
                    return ExitStatus::FAILURE;
                }
            }
        }
        // POSIX: return >0 if EOF was detected, even if data was read
        if hit_eof {
            ExitStatus::FAILURE
        } else {
            ExitStatus::SUCCESS
        }
    }

    fn builtin_local(&mut self, args: &[String]) -> ExitStatus {
        for arg in &args[1..] {
            if let Some(eq) = arg.find('=') {
                let name = &arg[..eq];
                let value = &arg[eq + 1..];
                self.vars.make_local(name);
                let _ = self.vars.set(name, value);
            } else {
                self.vars.make_local(arg);
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_exec(
        &mut self,
        args: &[String],
        _redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        // Redirections are applied permanently by eval_simple (is_exec flag).
        if args.len() <= 1 {
            return Ok(ExitStatus::SUCCESS);
        }

        // exec with command — replace process
        let args_bytes: Vec<ShellBytes> = args.iter().cloned().map(Into::into).collect();
        let env = self.vars.env_for_command_os(&[]);
        let _ = std::env::set_current_dir(&self.cwd);
        let err = exec::execve_with_env(&args_bytes[0], &args_bytes, &env);
        self.err_msg(&format!("exec: {}: {err}", args[0]));
        Ok(ExitStatus::NOT_EXECUTABLE)
    }

    fn builtin_command(&mut self, args: &[String], span: Span) -> crate::error::Result<ExitStatus> {
        if args.len() <= 1 {
            return Ok(ExitStatus::SUCCESS);
        }
        let mut mode = None; // None = execute, Some('v') = -v, Some('V') = -V
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "-v" => {
                    mode = Some('v');
                    i += 1;
                }
                "-V" => {
                    mode = Some('V');
                    i += 1;
                }
                "-p" => {
                    i += 1;
                } // -p: use default PATH (ignored for now)
                "--" => {
                    i += 1;
                    break;
                }
                _ => break,
            }
        }
        if i >= args.len() {
            return Ok(ExitStatus::SUCCESS);
        }

        if let Some(m) = mode {
            // Describe commands instead of executing them
            let mut status = ExitStatus::SUCCESS;
            for name in &args[i..] {
                if m == 'V' {
                    // -V: verbose, like `type`
                    if self.functions.contains_key(name.as_str()) {
                        self.write_out(&format!("{name} is a function\n"));
                    } else if is_builtin(name) {
                        self.write_out(&format!("{name} is a shell builtin\n"));
                    } else if let Ok(path) = self.which(name) {
                        self.write_out(&format!("{name} is {path}\n"));
                    } else {
                        self.err_msg(&format!("{name}: not found"));
                        status = ExitStatus::FAILURE;
                    }
                } else {
                    // -v: terse — print path, or name for builtins/functions
                    if self.functions.contains_key(name.as_str()) || is_builtin(name) {
                        self.write_out(&format!("{name}\n"));
                    } else if let Ok(path) = self.which(name) {
                        self.write_out(&format!("{path}\n"));
                    } else {
                        status = ExitStatus::FAILURE;
                    }
                }
            }
            return Ok(status);
        }

        let new_args: Vec<String> = args[i..].to_vec();

        // `command` bypasses functions but NOT builtins. External commands use
        // the embedder's handler when one is installed, like ordinary lookup.
        if let Some(status) = self.try_builtin(&new_args[0], &new_args, &[], &[], span)? {
            Ok(status)
        } else if self.has_external_handler() {
            self.call_external_handler(&new_args, &[])
        } else {
            self.eval_external(&new_args, &[], &[], span)
        }
    }

    fn builtin_type(&self, args: &[String]) -> ExitStatus {
        let mut status = ExitStatus::SUCCESS;
        for name in &args[1..] {
            if self.functions.contains_key(name.as_str()) {
                self.write_out(&format!("{name} is a function\n"));
            } else if is_builtin(name) {
                self.write_out(&format!("{name} is a shell builtin\n"));
            } else if let Ok(path) = self.which(name) {
                self.write_out(&format!("{name} is {path}\n"));
            } else {
                self.err_msg(&format!("{name}: not found"));
                status = ExitStatus::FAILURE;
            }
        }
        status
    }

    fn which(&self, name: &str) -> Result<String, ()> {
        let name_bytes = ShellBytes::from_str_lossless(name);
        let path = self
            .vars
            .get_bytes("PATH")
            .cloned()
            .unwrap_or_else(|| ShellBytes::from(""));
        for dir in path.as_bytes().split(|b| *b == b':') {
            let mut candidate = Vec::new();
            if dir.is_empty() {
                candidate.extend_from_slice(self.cwd.as_os_str().as_bytes());
            } else {
                candidate.extend_from_slice(dir);
            }
            if !candidate.ends_with(b"/") {
                candidate.push(b'/');
            }
            candidate.extend_from_slice(name_bytes.as_bytes());
            let candidate = ShellBytes::from_vec(candidate).to_path_buf();
            if candidate.is_file() {
                return Ok(ShellBytes::from_os_str(candidate.as_os_str()).to_shell_string());
            }
        }
        Err(())
    }

    fn builtin_wait(&mut self, _args: &[String]) -> ExitStatus {
        // Wait for all background children
        loop {
            let mut status = 0i32;
            // SAFETY: -1 waits for any child process; standard POSIX usage.
            let pid = sys::waitpid(-1, &mut status, 0);
            if pid <= 0 {
                break;
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_trap(&mut self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            // Print current traps
            for (sig, action) in &self.traps {
                self.write_out(&format!("trap -- '{}' {}\n", action, sig));
            }
            return ExitStatus::SUCCESS;
        }
        // Skip -- if present
        let offset = if args[1] == "--" { 2 } else { 1 };
        if args.len() <= offset {
            return ExitStatus::SUCCESS;
        }
        // POSIX: when only one operand remains (no action), it's a signal name
        // and the trap is reset to default. e.g. `trap INT` resets SIGINT.
        if args.len() == offset + 1 {
            let sig = &args[offset];
            if sig == "-" {
                // `trap -` with no signals: print traps (same as bare `trap`)
                for (sig, action) in &self.traps {
                    self.write_out(&format!("trap -- '{}' {}\n", action, sig));
                }
            } else {
                let normalized = sig.to_uppercase();
                self.traps.remove(&normalized);
                if let Some(signum) = crate::signal::name_to_signal(&normalized) {
                    crate::signal::reset_handler(signum);
                }
            }
            return ExitStatus::SUCCESS;
        }
        let action = &args[offset];
        for sig_name in &args[offset + 1..] {
            let normalized = sig_name.to_uppercase();
            if action == "-" {
                self.traps.remove(&normalized);
                if let Some(signum) = crate::signal::name_to_signal(&normalized) {
                    crate::signal::reset_handler(signum);
                }
            } else if action.is_empty() {
                // trap '' SIG — ignore the signal
                self.traps.insert(normalized.clone(), action.clone());
                if let Some(signum) = crate::signal::name_to_signal(&normalized) {
                    crate::signal::ignore_signal(signum);
                }
            } else {
                self.traps.insert(normalized.clone(), action.clone());
                if let Some(signum) = crate::signal::name_to_signal(&normalized) {
                    crate::signal::install_handler(signum);
                }
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_kill(&self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            self.err_msg("kill: usage: kill [-s signal | -signal] pid ...");
            return ExitStatus::MISUSE;
        }

        let mut i = 1;
        let mut signum = rustix::process::Signal::TERM.as_raw();

        // Parse signal specification
        if args[i] == "-l" || args[i] == "-L" {
            // kill -l [exit_status] — list signals
            if args.len() > i + 1 {
                // Convert exit status to signal name
                if let Ok(status) = args[i + 1].parse::<i32>() {
                    let sig = if status > 128 { status - 128 } else { status };
                    if let Some(name) = crate::signal::signal_to_name(sig) {
                        self.write_out(&format!("{name}\n"));
                        return ExitStatus::SUCCESS;
                    }
                }
                self.err_msg(&format!(
                    "kill: {}: invalid signal specification",
                    args[i + 1]
                ));
                return ExitStatus::FAILURE;
            }
            // List all signals
            for sig in 1..32 {
                if let Some(name) = crate::signal::signal_to_name(sig) {
                    self.write_out(&format!("{sig}) SIG{name}\n"));
                }
            }
            return ExitStatus::SUCCESS;
        } else if args[i] == "-s" {
            // kill -s SIGNAL pid...
            i += 1;
            if i >= args.len() {
                self.err_msg("kill: -s requires a signal name");
                return ExitStatus::MISUSE;
            }
            let sig_name = args[i].to_uppercase();
            match crate::signal::name_to_signal(&sig_name) {
                Some(s) => signum = s.as_raw(),
                None => {
                    self.err_msg(&format!("kill: {}: invalid signal specification", args[i]));
                    return ExitStatus::FAILURE;
                }
            }
            i += 1;
        } else if args[i].starts_with('-') && args[i].len() > 1 {
            let spec = &args[i][1..];
            // kill -9 pid... or kill -TERM pid...
            if let Ok(n) = spec.parse::<i32>() {
                signum = n;
            } else {
                let sig_name = spec.to_uppercase();
                match crate::signal::name_to_signal(&sig_name) {
                    Some(s) => signum = s.as_raw(),
                    None => {
                        self.err_msg(&format!("kill: {}: invalid signal specification", spec));
                        return ExitStatus::FAILURE;
                    }
                }
            }
            i += 1;
        }

        if i >= args.len() {
            self.err_msg("kill: usage: kill [-s signal | -signal] pid ...");
            return ExitStatus::MISUSE;
        }

        let mut status = ExitStatus::SUCCESS;
        for pid_str in &args[i..] {
            match pid_str.parse::<i32>() {
                Ok(pid) => {
                    if let Err(err) = kill_with_rustix(pid, signum) {
                        self.err_msg(&format!("kill: ({pid}) - {err}"));
                        status = ExitStatus::FAILURE;
                    }
                }
                Err(_) => {
                    self.err_msg(&format!("kill: {pid_str}: arguments must be process IDs"));
                    status = ExitStatus::FAILURE;
                }
            }
        }
        status
    }

    fn builtin_umask(&self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            // SAFETY: umask() is always safe; no invalid arguments possible.
            let mask = sys::umask(0);
            sys::umask(mask);
            self.write_out(&format!("{mask:04o}\n"));
            return ExitStatus::SUCCESS;
        }
        if let Ok(mask) = u32::from_str_radix(&args[1], 8) {
            // SAFETY: umask() is always safe; any mode_t value is valid.
            sys::umask(mask);
            ExitStatus::SUCCESS
        } else {
            self.err_msg(&format!("umask: {}: invalid mask", args[1]));
            ExitStatus::FAILURE
        }
    }

    fn builtin_getopts(&mut self, args: &[String]) -> ExitStatus {
        if args.len() < 3 {
            self.err_msg("getopts: usage: getopts optstring name [arg ...]");
            return ExitStatus::MISUSE;
        }
        let optstring = &args[1];
        let name = &args[2];
        let argv: Vec<String> = if args.len() > 3 {
            args[3..].to_vec()
        } else {
            self.vars.positional_shell_strings()
        };

        let optind: usize = self
            .vars
            .get("OPTIND")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        if optind < 1 || optind > argv.len() {
            let _ = self.vars.set(name, "?");
            return ExitStatus::FAILURE;
        }

        let arg = &argv[optind - 1];
        if !arg.starts_with('-') || arg == "-" || arg == "--" {
            let _ = self.vars.set(name, "?");
            return ExitStatus::FAILURE;
        }

        let optchars: Vec<char> = optstring.chars().collect();
        let arg_chars: Vec<char> = arg.chars().collect();

        // Track position within the current arg (for grouped short opts)
        let optpos: usize = self
            .vars
            .get("_OPTPOS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        if optpos >= arg_chars.len() {
            // Move to next arg
            let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
            let _ = self.vars.set("_OPTPOS", "1");
            return self.builtin_getopts(args);
        }

        let opt = arg_chars[optpos];

        // Find opt in optstring
        let found = optchars.iter().position(|&c| c == opt);
        match found {
            Some(pos) => {
                let _ = self.vars.set(name, &opt.to_string());
                let needs_arg = pos + 1 < optchars.len() && optchars[pos + 1] == ':';
                if needs_arg {
                    // Option argument
                    if optpos + 1 < arg_chars.len() {
                        // Argument is rest of current word
                        let optarg: String = arg_chars[optpos + 1..].iter().collect();
                        let _ = self.vars.set("OPTARG", &optarg);
                        let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
                        let _ = self.vars.set("_OPTPOS", "1");
                    } else if optind < argv.len() {
                        // Argument is next word
                        let _ = self.vars.set("OPTARG", &argv[optind]);
                        let _ = self.vars.set("OPTIND", &(optind + 2).to_string());
                        let _ = self.vars.set("_OPTPOS", "1");
                    } else {
                        self.err_msg(&format!("getopts: option requires argument -- {opt}"));
                        let _ = self.vars.set(name, "?");
                        let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
                        return ExitStatus::SUCCESS;
                    }
                } else {
                    let _ = self.vars.unset("OPTARG");
                    if optpos + 1 >= arg_chars.len() {
                        let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
                        let _ = self.vars.set("_OPTPOS", "1");
                    } else {
                        let _ = self.vars.set("_OPTPOS", &(optpos + 1).to_string());
                    }
                }
                ExitStatus::SUCCESS
            }
            None => {
                let silent = optstring.starts_with(':');
                if silent {
                    let _ = self.vars.set("OPTARG", &opt.to_string());
                } else {
                    self.err_msg(&format!("getopts: illegal option -- {opt}"));
                }
                let _ = self.vars.set(name, "?");
                if optpos + 1 >= arg_chars.len() {
                    let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
                    let _ = self.vars.set("_OPTPOS", "1");
                } else {
                    let _ = self.vars.set("_OPTPOS", &(optpos + 1).to_string());
                }
                ExitStatus::SUCCESS
            }
        }
    }

    fn builtin_printf(&self, args: &[String]) -> ExitStatus {
        if args.len() < 2 {
            self.err_msg("printf: usage: printf format [arguments]");
            return ExitStatus::FAILURE;
        }

        let format = &args[1];
        let mut arg_idx = 2;
        let fmt_chars: Vec<char> = format.chars().collect();
        let mut out = String::new();

        use std::fmt::Write;

        // POSIX: reuse format string while arguments remain
        loop {
            let mut i = 0;
            let arg_start = arg_idx;
            while i < fmt_chars.len() {
                if fmt_chars[i] == '\\' {
                    i += 1;
                    if i < fmt_chars.len() {
                        match fmt_chars[i] {
                            'n' => out.push('\n'),
                            't' => out.push('\t'),
                            'r' => out.push('\r'),
                            '\\' => out.push('\\'),
                            '"' => out.push('"'),
                            'a' => out.push('\x07'),
                            'b' => out.push('\x08'),
                            'f' => out.push('\x0c'),
                            'v' => out.push('\x0b'),
                            '0' => {
                                i += 1;
                                let start = i;
                                while i < fmt_chars.len()
                                    && i < start + 3
                                    && matches!(fmt_chars[i], '0'..='7')
                                {
                                    i += 1;
                                }
                                let oct: String = fmt_chars[start..i].iter().collect();
                                let n = u8::from_str_radix(&oct, 8).unwrap_or(0);
                                out.push(n as char);
                                continue;
                            }
                            c => {
                                out.push('\\');
                                out.push(c);
                            }
                        }
                        i += 1;
                    } else {
                        out.push('\\');
                    }
                } else if fmt_chars[i] == '%' {
                    i += 1;
                    if i >= fmt_chars.len() {
                        out.push('%');
                        break;
                    }
                    let mut flags = String::new();
                    while i < fmt_chars.len() && matches!(fmt_chars[i], '-' | '+' | ' ' | '0' | '#')
                    {
                        flags.push(fmt_chars[i]);
                        i += 1;
                    }
                    let mut width_s = String::new();
                    while i < fmt_chars.len() && fmt_chars[i].is_ascii_digit() {
                        width_s.push(fmt_chars[i]);
                        i += 1;
                    }
                    let width: usize = width_s.parse().unwrap_or(0);
                    let mut precision = None;
                    if i < fmt_chars.len() && fmt_chars[i] == '.' {
                        i += 1;
                        let mut prec = String::new();
                        while i < fmt_chars.len() && fmt_chars[i].is_ascii_digit() {
                            prec.push(fmt_chars[i]);
                            i += 1;
                        }
                        precision = Some(prec.parse::<usize>().unwrap_or(0));
                    }
                    if i >= fmt_chars.len() {
                        break;
                    }
                    let conv = fmt_chars[i];
                    i += 1;
                    let left_align = flags.contains('-');
                    let zero_pad = flags.contains('0') && !left_align;
                    match conv {
                        's' => {
                            let val = args.get(arg_idx).map(|s| s.as_str()).unwrap_or("");
                            arg_idx += 1;
                            let val = if let Some(p) = precision {
                                &val[..val.len().min(p)]
                            } else {
                                val
                            };
                            if left_align {
                                let _ = write!(out, "{val:<width$}");
                            } else {
                                let _ = write!(out, "{val:>width$}");
                            }
                        }
                        'd' | 'i' => {
                            let val = args
                                .get(arg_idx)
                                .and_then(|s| s.parse::<i64>().ok())
                                .unwrap_or(0);
                            arg_idx += 1;
                            if zero_pad {
                                let _ = write!(out, "{val:0>width$}");
                            } else if left_align {
                                let _ = write!(out, "{val:<width$}");
                            } else {
                                let _ = write!(out, "{val:>width$}");
                            }
                        }
                        'o' => {
                            let val = args
                                .get(arg_idx)
                                .and_then(|s| s.parse::<i64>().ok())
                                .unwrap_or(0);
                            arg_idx += 1;
                            let _ = write!(out, "{val:o}");
                        }
                        'x' => {
                            let val = args
                                .get(arg_idx)
                                .and_then(|s| s.parse::<i64>().ok())
                                .unwrap_or(0);
                            arg_idx += 1;
                            let _ = write!(out, "{val:x}");
                        }
                        'X' => {
                            let val = args
                                .get(arg_idx)
                                .and_then(|s| s.parse::<i64>().ok())
                                .unwrap_or(0);
                            arg_idx += 1;
                            let _ = write!(out, "{val:X}");
                        }
                        'c' => {
                            let val = args
                                .get(arg_idx)
                                .and_then(|s| s.chars().next())
                                .unwrap_or('\0');
                            arg_idx += 1;
                            if val != '\0' {
                                out.push(val);
                            }
                        }
                        '%' => out.push('%'),
                        'b' => {
                            let val = args.get(arg_idx).map(|s| s.as_str()).unwrap_or("");
                            arg_idx += 1;
                            out.push_str(&unescape_echo(val));
                        }
                        c => {
                            out.push('%');
                            out.push(c);
                        }
                    }
                } else {
                    out.push(fmt_chars[i]);
                    i += 1;
                }
            }
            // If format consumed no args, or no args remain, stop looping
            if arg_idx == arg_start || arg_idx >= args.len() {
                break;
            }
        }

        self.write_out(&out);
        ExitStatus::SUCCESS
    }
}

/// All builtin command names recognized by the shell.
pub const BUILTIN_NAMES: &[&str] = &[
    ":", "true", "false", "echo", "printf", "cd", "pwd", "exit", "return", "break", "continue",
    "export", "readonly", "unset", "set", "shift", "eval", ".", "source", "test", "[", "read",
    "local", "exec", "command", "type", "wait", "trap", "kill", "umask", "getopts",
];

/// Check if a command name is a shell builtin.
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_NAMES.contains(&name)
}

fn unescape_echo(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('a') => result.push('\x07'),
                Some('b') => result.push('\x08'),
                Some('f') => result.push('\x0c'),
                Some('r') => result.push('\r'),
                Some('v') => result.push('\x0b'),
                Some(c) => {
                    result.push('\\');
                    result.push(c);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// exec family helpers
pub(crate) mod exec {
    use std::ffi::{CString, OsString};
    use std::os::unix::ffi::OsStrExt;

    use crate::shell_bytes::ShellBytes;

    fn execve_path(
        path: &[u8],
        c_args: &[*const std::os::raw::c_char],
        c_env: &[*const std::os::raw::c_char],
    ) -> std::io::Error {
        let Ok(c_path) = CString::new(path) else {
            return std::io::Error::from(rustix::io::Errno::INVAL);
        };
        // SAFETY: all pointers refer to live, null-terminated buffers for the
        // duration of the call. execve only returns on error.
        unsafe {
            crate::sys::execve(c_path.as_ptr(), c_args.as_ptr(), c_env.as_ptr());
        }
        std::io::Error::last_os_error()
    }

    pub fn execve_with_env(
        cmd: &ShellBytes,
        args: &[ShellBytes],
        env: &[(OsString, OsString)],
    ) -> std::io::Error {
        let Ok(c_args): Result<Vec<_>, _> = args.iter().map(ShellBytes::to_cstring).collect()
        else {
            return std::io::Error::from(rustix::io::Errno::INVAL);
        };
        let c_argv: Vec<*const std::os::raw::c_char> = c_args
            .iter()
            .map(|arg| arg.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        let c_env: Vec<CString> = match env
            .iter()
            .map(|(name, value)| {
                let mut bytes = name.as_bytes().to_vec();
                bytes.push(b'=');
                bytes.extend_from_slice(value.as_bytes());
                CString::new(bytes)
            })
            .collect()
        {
            Ok(env) => env,
            Err(_) => return std::io::Error::from(rustix::io::Errno::INVAL),
        };
        let c_envp: Vec<*const std::os::raw::c_char> = c_env
            .iter()
            .map(|entry| entry.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        if cmd.as_bytes().contains(&b'/') {
            return execve_path(cmd.as_bytes(), &c_argv, &c_envp);
        }

        let path = env
            .iter()
            .find(|(name, _)| name.as_bytes() == b"PATH")
            .map(|(_, value)| value.as_bytes())
            .unwrap_or(b"/usr/local/bin:/usr/bin:/bin");
        let mut permission_error = None;
        let mut last_error = None;
        for directory in path.split(|&byte| byte == b':') {
            let mut candidate = Vec::with_capacity(directory.len() + 1 + cmd.len());
            if directory.is_empty() {
                candidate.push(b'.');
            } else {
                candidate.extend_from_slice(directory);
            }
            candidate.push(b'/');
            candidate.extend_from_slice(cmd.as_bytes());
            let error = execve_path(&candidate, &c_argv, &c_envp);
            match error.raw_os_error() {
                Some(code) if code == libc::ENOENT || code == libc::ENOTDIR => {}
                Some(code) if code == libc::EACCES => permission_error = Some(error),
                _ => last_error = Some(error),
            }
        }

        permission_error
            .or(last_error)
            .unwrap_or_else(|| std::io::Error::from(rustix::io::Errno::NOENT))
    }
}
