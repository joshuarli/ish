use crate::ast::*;
use crate::error::{ExitStatus, ShellError, Span};
use crate::eval::{CommandIdentity, Shell};
use crate::parser::Parser;
use crate::shell_bytes::ShellBytes;
use crate::sys;
use crate::test_cmd::test_eval;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

pub(crate) enum CommandAction<'a> {
    Empty,
    Describe { verbose: bool, names: &'a [String] },
    Execute(&'a [String]),
}

pub(crate) fn command_action(args: &[String]) -> CommandAction<'_> {
    let mut mode = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "-v" => {
                mode = Some(false);
                index += 1;
            }
            "-V" => {
                mode = Some(true);
                index += 1;
            }
            "-p" => index += 1,
            "--" => {
                index += 1;
                break;
            }
            _ => break,
        }
    }
    if index >= args.len() {
        return CommandAction::Empty;
    }
    if let Some(verbose) = mode {
        CommandAction::Describe {
            verbose,
            names: &args[index..],
        }
    } else {
        CommandAction::Execute(&args[index..])
    }
}

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
                let code = self.builtin_status_operand(name, args)?;
                return Err(ShellError::Exit(code));
            }
            "return" => {
                let code = self.builtin_status_operand(name, args)?;
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
            "wait" => return self.builtin_wait(args).map(Some),
            "trap" => Some(self.builtin_trap(args)),
            "kill" => Some(self.builtin_kill(args)),
            "umask" => Some(self.builtin_umask(args)),
            "getopts" => Some(self.builtin_getopts(args)),
            "printf" => Some(self.builtin_printf(args)),
            _ => None,
        };

        Ok(status)
    }

    fn builtin_status_operand(&self, name: &str, args: &[String]) -> crate::error::Result<ExitStatus> {
        let Some(operand) = args.get(1) else {
            return Ok(self.exit_status);
        };
        match operand.parse::<i32>() {
            Ok(code) => Ok(ExitStatus::from(code)),
            Err(_) => {
                self.err_msg(&format!("{name}: Illegal number: {operand}"));
                Err(ShellError::Exit(ExitStatus::MISUSE))
            }
        }
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
                if let Err(e) = self.vars.set_shell(name, value) {
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
                if let Err(e) = self.vars.set_shell(name, value) {
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
                self.vars.positional = args[i..].iter().map(|s| ShellBytes::from_str_lossless(s)).collect();
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
                if let Err(e) = self.vars.set_shell(&name, &value) {
                    self.err_msg(&format!("set: {e}"));
                    return ExitStatus::FAILURE;
                }
                self.vars.export(&name);
                return ExitStatus::SUCCESS;
            } else {
                // Positional parameters
                self.vars.positional = args[i..].iter().map(|s| ShellBytes::from_str_lossless(s)).collect();
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
        let mut parser = Parser::from_shell_string(source);
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
        test_eval(args, &self.cwd)
    }

    fn builtin_read(&mut self, args: &[String]) -> ExitStatus {
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

        let stdin_fd = self.pending_stdin_fd().unwrap_or(0);
        let mut line = Vec::new();
        let mut buf = [0u8; 1];
        let mut hit_eof = true;
        let mut continued = false;
        loop {
            let n = sys::read(stdin_fd, buf.as_mut_ptr().cast(), 1);
            if n <= 0 {
                break;
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
                let n2 = sys::read(stdin_fd, buf.as_mut_ptr().cast(), 1);
                if n2 <= 0 {
                    line.push(b'\\');
                    break;
                }
                if buf[0] == b'\n' {
                    continued = true;
                    continue;
                }
                line.push(buf[0]);
                continued = false;
                continue;
            }
            continued = false;
            line.push(buf[0]);
        }

        if hit_eof && line.is_empty() {
            for name in &var_names {
                if let Err(e) = self.vars.set_shell(name, "") {
                    self.err_msg(&format!("read: {e}"));
                    return ExitStatus::FAILURE;
                }
            }
            return ExitStatus::FAILURE;
        }

        let line = crate::encoding::bytes_to_str(&line);
        let ifs = self.vars.ifs();
        let delimiters: Vec<&str> = crate::encoding::shell_chars(&ifs).collect();
        if var_names.len() == 1 {
            let units: Vec<&str> = crate::encoding::shell_chars(&line).collect();
            let start = units.iter().position(|unit| !delimiters.contains(unit)).unwrap_or(units.len());
            let end = units.iter().rposition(|unit| !delimiters.contains(unit)).map_or(start, |index| index + 1);
            let trimmed: String = units[start..end].iter().copied().collect();
            if let Err(e) = self.vars.set_shell(var_names[0], &trimmed) {
                self.err_msg(&format!("read: {e}"));
                return ExitStatus::FAILURE;
            }
        } else {
            let mut fields = Vec::new();
            let chars: Vec<&str> = crate::encoding::shell_chars(&line).collect();
            let mut pos = 0;

            while pos < chars.len() && delimiters.contains(&chars[pos]) && chars[pos].chars().all(char::is_whitespace) {
                pos += 1;
            }

            for vi in 0..var_names.len() {
                if vi == var_names.len() - 1 {
                    let end = chars[pos..].iter().rposition(|unit| !delimiters.contains(unit) || !unit.chars().all(char::is_whitespace))
                        .map_or(pos, |index| pos + index + 1);
                    fields.push(chars[pos..end].iter().copied().collect::<String>());
                } else {
                    let start = pos;
                    while pos < chars.len() && !delimiters.contains(&chars[pos]) {
                        pos += 1;
                    }
                    fields.push(chars[start..pos].iter().copied().collect::<String>());
                    while pos < chars.len() && delimiters.contains(&chars[pos]) {
                        pos += 1;
                    }
                }
            }

            for (vi, name) in var_names.iter().enumerate() {
                let value = fields.get(vi).map(|s| s.as_str()).unwrap_or("");
                if let Err(e) = self.vars.set_shell(name, value) {
                    self.err_msg(&format!("read: {e}"));
                    return ExitStatus::FAILURE;
                }
            }
        }
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
                let _ = self.vars.set_shell(name, value);
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
        if args.len() <= 1 {
            return Ok(ExitStatus::SUCCESS);
        }

        let args_bytes: Vec<ShellBytes> = args.iter().map(|s| ShellBytes::from_str_lossless(s)).collect();
        let env = self.vars.env_for_command_os(&[]);
        #[cfg(target_os = "linux")]
        let original_cwd = rustix::fs::open(
            ".",
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        );
        #[cfg(target_os = "macos")]
        let original_cwd = std::fs::File::open(".");
        let original_cwd = match original_cwd {
            Ok(directory) => directory,
            Err(err) => {
                self.err_msg(&format!("exec: cannot save current directory: {err}"));
                return Ok(ExitStatus::NOT_EXECUTABLE);
            }
        };
        if let Err(err) = std::env::set_current_dir(&self.cwd) {
            self.err_msg(&format!("exec: {}: {err}", args[1]));
            return Ok(ExitStatus::NOT_EXECUTABLE);
        }
        let err = exec::execve_with_env(&args_bytes[1], &args_bytes[1..], &env);
        self.err_msg(&format!("exec: {}: {err}", args[1]));
        if let Err(restore_err) = rustix::process::fchdir(&original_cwd) {
            self.err_msg(&format!("exec: cannot restore current directory: {restore_err}"));
        }
        Ok(ExitStatus::NOT_EXECUTABLE)
    }

    fn builtin_command(&mut self, args: &[String], span: Span) -> crate::error::Result<ExitStatus> {
        match command_action(args) {
            CommandAction::Empty => Ok(ExitStatus::SUCCESS),
            CommandAction::Describe { verbose, names } => {
                let mut status = ExitStatus::SUCCESS;
                for name in names {
                    if verbose {
                        if self.functions.contains_key(name.as_str()) {
                            self.write_out(&format!("{name} is a function\n"));
                        } else if is_builtin(name)
                            || self.command_identity(name) == Some(CommandIdentity::Builtin)
                        {
                            self.write_out(&format!("{name} is a shell builtin\n"));
                        } else if let Ok(path) = self.which(name) {
                            self.write_out(&format!("{name} is {path}\n"));
                        } else {
                            self.err_msg(&format!("{name}: not found"));
                            status = ExitStatus::FAILURE;
                        }
                    } else if self.functions.contains_key(name.as_str())
                        || is_builtin(name)
                        || self.command_identity(name) == Some(CommandIdentity::Builtin)
                    {
                        self.write_out(&format!("{name}\n"));
                    } else if let Ok(path) = self.which(name) {
                        self.write_out(&format!("{path}\n"));
                    } else {
                        status = ExitStatus::FAILURE;
                    }
                }
                Ok(status)
            }
            CommandAction::Execute(new_args) => {
                if let Some(diagnostic) = self.parent_process_refusal(new_args) {
                    self.err_msg(&diagnostic);
                    return Ok(ExitStatus::MISUSE);
                }
                self.check_command_policy_for_dispatch(new_args, span)?;
                if let Some(status) = self.try_builtin(&new_args[0], new_args, &[], &[], span)? {
                    Ok(status)
                } else if self.has_external_handler() {
                    self.call_external_handler(new_args, &[])
                } else {
                    self.eval_external(new_args, &[], &[], span)
                }
            }
        }
    }

    fn builtin_type(&self, args: &[String]) -> ExitStatus {
        let mut status = ExitStatus::SUCCESS;
        for name in &args[1..] {
            if self.functions.contains_key(name.as_str()) {
                self.write_out(&format!("{name} is a function\n"));
            } else if is_builtin(name)
                || self.command_identity(name) == Some(CommandIdentity::Builtin)
            {
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
        if name_bytes.as_bytes().contains(&b'/') {
            let path = name_bytes.to_path_buf();
            let candidate = if path.is_absolute() {
                path
            } else {
                self.cwd.join(path)
            };
            return candidate.is_file().then(|| name.to_owned()).ok_or(());
        }
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

    fn builtin_wait(&mut self, args: &[String]) -> crate::error::Result<ExitStatus> {
        if args.len() == 1 {
            for pid in self.child_pids.clone() {
                if self.child_pids.contains(&pid) {
                    self.wait_owned_child(pid)?;
                }
            }
            return Ok(ExitStatus::SUCCESS);
        }

        let mut status = ExitStatus::SUCCESS;
        for operand in &args[1..] {
            let pid = operand.parse::<i32>().ok();
            match pid.filter(|pid| self.child_pids.contains(pid)) {
                Some(pid) => status = self.wait_owned_child(pid)?,
                None => {
                    self.err_msg(&format!("wait: {operand}: not a child"));
                    status = ExitStatus::from(127);
                }
            }
        }
        Ok(status)
    }

    fn builtin_trap(&mut self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            for (sig, action) in &self.traps {
                self.write_out(&format!("trap -- '{}' {}\n", action, sig));
            }
            return ExitStatus::SUCCESS;
        }
        let offset = if args[1] == "--" { 2 } else { 1 };
        if args.len() <= offset {
            return ExitStatus::SUCCESS;
        }
        if args.len() == offset + 1 {
            let sig = &args[offset];
            if sig == "-" {
                for (sig, action) in &self.traps {
                    self.write_out(&format!("trap -- '{}' {}\n", action, sig));
                }
                return ExitStatus::SUCCESS;
            }
            return self.reset_named_trap(sig);
        }
        let action = &args[offset];
        let mut status = ExitStatus::SUCCESS;
        for sig_name in &args[offset + 1..] {
            let normalized = sig_name.to_uppercase();
            if is_exit_signal_specifier(&normalized) {
                if action == "-" {
                    self.traps.remove("EXIT");
                } else {
                    self.traps.insert("EXIT".to_owned(), action.clone());
                }
                continue;
            }
            let Some(signum) = crate::signal::name_to_signal(&normalized) else {
                self.err_msg(&format!("trap: {sig_name}: invalid signal specification"));
                status = ExitStatus::FAILURE;
                continue;
            };
            let Some(canonical) = crate::signal::signal_to_name(signum.as_raw()) else {
                self.err_msg(&format!("trap: {sig_name}: invalid signal specification"));
                status = ExitStatus::FAILURE;
                continue;
            };
            let result = if action == "-" {
                crate::signal::reset_handler(signum)
            } else if action.is_empty() {
                crate::signal::ignore_signal(signum)
            } else {
                crate::signal::install_handler(signum)
            };
            if let Err(err) = result {
                self.err_msg(&format!("trap: {sig_name}: {err}"));
                status = ExitStatus::FAILURE;
                continue;
            }
            if action == "-" {
                self.traps.remove(canonical);
            } else {
                self.traps.insert(canonical.to_owned(), action.clone());
            }
        }
        status
    }

    fn reset_named_trap(&mut self, name: &str) -> ExitStatus {
        let normalized = name.to_uppercase();
        if is_exit_signal_specifier(&normalized) {
            self.traps.remove("EXIT");
            return ExitStatus::SUCCESS;
        }
        let Some(signal) = crate::signal::name_to_signal(&normalized) else {
            self.err_msg(&format!("trap: {name}: invalid signal specification"));
            return ExitStatus::FAILURE;
        };
        let Some(canonical) = crate::signal::signal_to_name(signal.as_raw()) else {
            self.err_msg(&format!("trap: {name}: invalid signal specification"));
            return ExitStatus::FAILURE;
        };
        match crate::signal::reset_handler(signal) {
            Ok(()) => {
                self.traps.remove(canonical);
                ExitStatus::SUCCESS
            }
            Err(err) => {
                self.err_msg(&format!("trap: {name}: {err}"));
                ExitStatus::FAILURE
            }
        }
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
            let _ = self.vars.set_shell(name, "?");
            return ExitStatus::FAILURE;
        }

        let arg = &argv[optind - 1];
        if !arg.starts_with('-') || arg == "-" || arg == "--" {
            let _ = self.vars.set_shell(name, "?");
            return ExitStatus::FAILURE;
        }

        let optchars: Vec<&str> = crate::encoding::shell_chars(optstring).collect();
        let arg_chars: Vec<&str> = crate::encoding::shell_chars(arg).collect();

        // Track position within the current arg (for grouped short opts)
        let optpos: usize = self
            .vars
            .get("_OPTPOS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        if optpos >= arg_chars.len() {
            // Move to next arg
            let _ = self.vars.set_shell("OPTIND", &(optind + 1).to_string());
            let _ = self.vars.set_shell("_OPTPOS", "1");
            return self.builtin_getopts(args);
        }

        let opt = arg_chars[optpos];

        // Find opt in optstring
        let found = optchars.iter().position(|&c| c == opt);
        match found {
            Some(pos) => {
                let _ = self.vars.set_shell(name, opt);
                let needs_arg = pos + 1 < optchars.len() && optchars[pos + 1] == ":";
                if needs_arg {
                    // Option argument
                    if optpos + 1 < arg_chars.len() {
                        // Argument is rest of current word
                        let optarg: String = arg_chars[optpos + 1..].iter().copied().collect();
                        let _ = self.vars.set_shell("OPTARG", &optarg);
                        let _ = self.vars.set_shell("OPTIND", &(optind + 1).to_string());
                        let _ = self.vars.set_shell("_OPTPOS", "1");
                    } else if optind < argv.len() {
                        // Argument is next word
                        let _ = self.vars.set_shell("OPTARG", &argv[optind]);
                        let _ = self.vars.set_shell("OPTIND", &(optind + 2).to_string());
                        let _ = self.vars.set_shell("_OPTPOS", "1");
                    } else {
                        self.err_msg(&format!("getopts: option requires argument -- {opt}"));
                        let _ = self.vars.set_shell(name, "?");
                        let _ = self.vars.set_shell("OPTIND", &(optind + 1).to_string());
                        return ExitStatus::SUCCESS;
                    }
                } else {
                    let _ = self.vars.unset("OPTARG");
                    if optpos + 1 >= arg_chars.len() {
                        let _ = self.vars.set_shell("OPTIND", &(optind + 1).to_string());
                        let _ = self.vars.set_shell("_OPTPOS", "1");
                    } else {
                        let _ = self.vars.set_shell("_OPTPOS", &(optpos + 1).to_string());
                    }
                }
                ExitStatus::SUCCESS
            }
            None => {
                let silent = optstring.starts_with(':');
                if silent {
                    let _ = self.vars.set_shell("OPTARG", opt);
                } else {
                    self.err_msg(&format!("getopts: illegal option -- {opt}"));
                }
                let _ = self.vars.set_shell(name, "?");
                if optpos + 1 >= arg_chars.len() {
                    let _ = self.vars.set_shell("OPTIND", &(optind + 1).to_string());
                    let _ = self.vars.set_shell("_OPTPOS", "1");
                } else {
                    let _ = self.vars.set_shell("_OPTPOS", &(optpos + 1).to_string());
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
                                out.push_str(&crate::encoding::bytes_to_str(&[n]));
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
                            let val = if let Some(precision) = precision {
                                let bytes = crate::encoding::str_to_bytes(val);
                                crate::encoding::bytes_to_str(&bytes[..bytes.len().min(precision)])
                            } else {
                                val.to_string()
                            };
                            let padding = width.saturating_sub(crate::encoding::shell_chars(&val).count());
                            if !left_align {
                                out.extend(std::iter::repeat_n(' ', padding));
                            }
                            out.push_str(&val);
                            if left_align {
                                out.extend(std::iter::repeat_n(' ', padding));
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
                            let val = args.get(arg_idx).and_then(|s| crate::encoding::shell_chars(s).next());
                            arg_idx += 1;
                            if let Some(val) = val.filter(|value| *value != "\0") {
                                out.push_str(val);
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

fn is_exit_signal_specifier(specifier: &str) -> bool {
    specifier.eq_ignore_ascii_case("EXIT")
        || (specifier.bytes().all(|byte| byte.is_ascii_digit())
            && specifier.parse::<i32>().ok() == Some(0))
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
