use epsh::error::{ExitStatus, ShellError};
use epsh::eval::{CallbackKind, Shell};
use epsh::parser::Parser;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn isolated(name: &str, probe: impl FnOnce()) {
    if std::env::var("EPSH_CALLBACK_PROBE").as_deref() == Ok(name) {
        probe();
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("EPSH_CALLBACK_PROBE", name)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit())
        .process_group(0).spawn().unwrap();
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || sender.send(child.wait()).unwrap());
    let result = receiver.recv_timeout(Duration::from_secs(5));
    if result.is_err() {
        let _ = rustix::process::kill_process_group(rustix::process::Pid::from_raw(pid as i32).unwrap(), rustix::process::Signal::KILL);
    }
    waiter.join().unwrap();
    assert!(result.expect("callback probe hung").unwrap().success(), "probe failed: {name}");
}

fn guarded_shell(kind: CallbackKind, lock: Arc<Mutex<()>>) -> Shell {
    let owner = std::process::id();
    let mut shell = Shell::new();
    match kind {
        CallbackKind::External => shell.set_external_handler(Box::new(move |_, _| {
            if std::process::id() != owner { drop(lock.lock().unwrap()); }
            Ok(ExitStatus::SUCCESS)
        })),
        CallbackKind::CommandPolicy => shell.set_command_policy(Box::new(move |_| {
            if std::process::id() != owner { drop(lock.lock().unwrap()); }
            None
        })),
        CallbackKind::RedirectOpen => shell.set_redirect_open_handler(Box::new(move |_| {
            if std::process::id() != owner { drop(lock.lock().unwrap()); }
            None
        })),
    }
    shell
}

#[test]
fn callbacks_refuse_every_fork_path_before_target_effects() {
    isolated("callbacks_refuse_every_fork_path_before_target_effects", || {
        for kind in [CallbackKind::External, CallbackKind::CommandPolicy, CallbackKind::RedirectOpen] {
            for script in [
                "command command /bin/true >target | /bin/cat",
                "(/bin/true >target)",
                "echo \"$(/bin/true >target)\" >outer",
                "echo \"$(/bin/true >target | /bin/cat)\" >outer",
                "/bin/true >target & wait",
                "echo \"$(>target)\" >outer",
            ] {
                if script.contains("$(>target)") && kind != CallbackKind::RedirectOpen { continue; }
                let dir = tempfile::tempdir().unwrap();
                let lock = Arc::new(Mutex::new(()));
                let held = lock.lock().unwrap();
                let mut shell = guarded_shell(kind, lock.clone());
                let cwd = format!("cd '{}'; {script}", dir.path().display());
                let program = Parser::new(&cwd).parse().unwrap();
                let mut refusal = None;
                for command in &program.commands {
                    if let Err(error) = shell.eval_command(command) { refusal = Some(error); break; }
                }
                assert!(matches!(refusal, Some(ShellError::CallbackProcess { kind: actual }) if actual == kind), "wrong refusal for {kind:?}, {script}: {refusal:?}");
                assert!(!dir.path().join("target").exists(), "target opened: {kind:?}, {script}");
                assert!(!dir.path().join("outer").exists(), "enclosing target opened: {kind:?}, {script}");
                drop(held);
            }
        }
    });
}

#[test]
fn callbacks_refuse_embedder_forks_and_skip_inherited_destructors() {
    isolated("callbacks_refuse_embedder_forks_and_skip_inherited_destructors", || {
        struct Marker(std::path::PathBuf);
        impl Drop for Marker {
            fn drop(&mut self) { std::fs::write(&self.0, std::process::id().to_string()).unwrap(); }
        }
        let dir = tempfile::tempdir().unwrap();
        for kind in [CallbackKind::External, CallbackKind::CommandPolicy, CallbackKind::RedirectOpen] {
            let marker = dir.path().join(format!("{kind:?}"));
            let destructor = Marker(marker.clone());
            let mut shell = Shell::new();
            match kind {
                CallbackKind::External => shell.set_external_handler(Box::new(move |_, _| { let _ = &destructor; Ok(ExitStatus::SUCCESS) })),
                CallbackKind::CommandPolicy => shell.set_command_policy(Box::new(move |_| { let _ = &destructor; None })),
                CallbackKind::RedirectOpen => shell.set_redirect_open_handler(Box::new(move |_| { let _ = &destructor; None })),
            }
            let program = Parser::new("/bin/true >target").parse().unwrap();
            // SAFETY: the isolated probe checks callback refusal and destroys the inherited shell before immediate child exit.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let refused = matches!(shell.eval_command(&program.commands[0]), Err(ShellError::CallbackProcess { kind: actual }) if actual == kind);
                shell.set_command_policy(Box::new(|_| None));
                drop(shell);
                // SAFETY: the child returns no inherited host state to the test harness.
                unsafe { libc::_exit(if refused { 0 } else { 1 }); }
            }
            let (_, status) = rustix::process::waitpid(rustix::process::Pid::from_raw(pid), rustix::process::WaitOptions::empty()).unwrap().unwrap();
            assert_eq!(status.exit_status(), Some(0));
            assert!(!marker.exists(), "inherited callback destructor ran");
            drop(shell);
            assert_eq!(std::fs::read_to_string(marker).unwrap(), std::process::id().to_string());
        }
    });
}

#[test]
fn ordinary_substitution_exit_two_keeps_output_and_executes_enclosing_target() {
    isolated("ordinary_substitution_exit_two_keeps_output_and_executes_enclosing_target", || {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
        assert_eq!(shell.run_script("printf '%s' \"$(/bin/sh -c 'printf retained; exit 2')\" >outer"), 0);
        assert_eq!(std::fs::read(dir.path().join("outer")).unwrap(), b"retained");
    });
}

#[test]
fn audited_child_callback_preserves_raw_arguments_environment_descriptors_and_status() {
    isolated("audited_child_callback_preserves_raw_arguments_environment_descriptors_and_status", || {
        static CHILD_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
        shell.set_var_bytes("raw", epsh::shell_bytes::ShellBytes::from_vec(vec![b'x', 0xff])).unwrap();
        shell.vars_mut().export("raw");
        // SAFETY: this zero-sized closure has no captured state or destructor work. Every branch uses only borrowed byte comparisons, an atomic, and raw descriptor writes; no allocator, host lock, environment mutation, or runtime service is reachable.
        unsafe {
            shell.set_external_handler_in_children(Box::new(|args, environment| {
                if args.len() != 2 || args[1].as_bytes() != b"x\xff" || !environment.iter().any(|(name, value)| name.as_bytes() == b"raw" && value.as_bytes() == b"x\xff") {
                    return Ok(ExitStatus::from(93));
                }
                CHILD_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // SAFETY: write accepts raw descriptors and borrowed bytes; a closed descriptor returns an error.
                let written = libc::write(1, args[1].as_bytes().as_ptr().cast(), args[1].as_bytes().len());
                Ok(ExitStatus::from(if written == 2 { 7 } else { 94 }))
            }));
        }
        assert_eq!(shell.run_script("probe \"$raw\" >parent"), 7);
        assert_eq!(CHILD_CALLS.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(shell.run_script("printf '%s' \"$(probe \"$raw\")\" >substitution"), 0);
        assert_eq!(shell.run_script("printf ignored | probe \"$raw\" >pipeline"), 7);
        assert_eq!(CHILD_CALLS.load(std::sync::atomic::Ordering::Relaxed), 1);
        for name in ["parent", "substitution", "pipeline"] {
            assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), b"x\xff");
        }
    });
}

#[test]
fn argv_policy_refuses_before_assignments_and_is_process_bound() {
    isolated("argv_policy_refuses_before_assignments_and_is_process_bound", || {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
        shell.set_command_argv_policy(Box::new(|args| {
            (args.first().is_some_and(|name| name.as_bytes() == b"set") && args.get(1).is_some_and(|arg| arg.as_bytes() == b"-e")).then(|| "option refused".to_owned())
        }));
        assert_eq!(shell.run_script("operand=-e; X=$(/bin/touch assigned) command -p command -- set \"$operand\" >target"), 2);
        assert!(!dir.path().join("assigned").exists());
        assert!(!dir.path().join("target").exists());
        let program = Parser::new("printf '%s' \"$(/bin/true)\" >outer").parse().unwrap();
        assert!(matches!(shell.eval_command(&program.commands[0]), Err(ShellError::CallbackProcess { kind: CallbackKind::CommandPolicy })));
        assert!(!dir.path().join("outer").exists());
    });
}

#[test]
fn owned_policy_refusal_survives_pipeline_and_substitution_boundaries() {
    isolated("owned_policy_refusal_survives_pipeline_and_substitution_boundaries", || {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
        shell.set_command_rule(epsh::shell_bytes::ShellBytes::from("blocked"), epsh::eval::CommandRule::Deny("blocked by owner".to_owned()));
        let program = Parser::new("printf '%s' \"$(blocked >target | /bin/cat)\" >outer").parse().unwrap();
        assert!(matches!(shell.eval_command(&program.commands[0]), Err(ShellError::CommandRefused { .. })));
        assert!(!dir.path().join("target").exists());
        assert!(!dir.path().join("outer").exists());
    });
}

#[test]
fn external_callback_cwd_is_restored_after_callback_failure() {
    let dir = tempfile::tempdir().unwrap();
    let expected = dir.path().to_path_buf();
    let mut shell = Shell::builder().cwd(expected.clone()).build();
    shell.set_external_handler(Box::new(move |_, _| {
        assert_eq!(epsh::eval::external_command_cwd().as_ref(), Some(&expected));
        Err(ShellError::Io(std::io::Error::other("callback failed")))
    }));
    assert_eq!(shell.run_script("probe"), 2);
    assert_eq!(epsh::eval::external_command_cwd(), None);
}

#[test]
fn declining_dispatcher_preserves_expanded_environment_cwd_and_redirections() {
    let dir = tempfile::tempdir().unwrap();
    let expected = dir.path().to_path_buf();
    let mut shell = Shell::builder().cwd(expected.clone()).build();
    shell.set_var_bytes("RAW", epsh::shell_bytes::ShellBytes::from_vec(vec![b'x', 0xff])).unwrap();
    shell.vars_mut().export("RAW");
    shell.set_external_dispatcher(vec![epsh::shell_bytes::ShellBytes::from("/usr/bin/env")], Box::new(move |args, environment| {
        assert_eq!(args[0].as_bytes(), b"/usr/bin/env");
        assert_eq!(epsh::eval::external_command_cwd(), Some(expected.clone()));
        assert!(environment.iter().any(|(name, value)| name.as_bytes() == b"PREFIX" && value.as_bytes() == b"value"));
        Ok(epsh::eval::ExternalDispatch::NotHandled)
    }));
    assert_eq!(shell.run_script("PREFIX=$(/bin/sh -c 'printf value; printf x >>expanded-once') /usr/bin/env >environment"), 0);
    assert_eq!(std::fs::read(dir.path().join("expanded-once")).unwrap(), b"x");
    let environment = std::fs::read(dir.path().join("environment")).unwrap();
    assert!(environment.split(|byte| *byte == b'\n').any(|line| line == b"PREFIX=value"));
    assert!(environment.split(|byte| *byte == b'\n').any(|line| line == b"RAW=x\xff"));
    assert_eq!(epsh::eval::external_command_cwd(), None);
}

#[test]
fn exit_trap_callback_refusal_suppresses_the_enclosing_target() {
    isolated("exit_trap_callback_refusal_suppresses_the_enclosing_target", || {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("callback-entered");
        let callback_marker = marker.clone();
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
        shell.set_external_handler(Box::new(move |_, _| {
            std::fs::write(&callback_marker, b"entered").unwrap();
            Ok(ExitStatus::SUCCESS)
        }));
        assert_eq!(shell.run_script("printf '%s' \"$(trap '/bin/true' EXIT; printf ok)\" >outer"), 2);
        assert!(!dir.path().join("outer").exists());
        assert!(!marker.exists());
    });
}

#[test]
fn exit_trap_ordinary_errors_preserve_substitution_status_and_output() {
    isolated("exit_trap_ordinary_errors_preserve_substitution_status_and_output", || {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
        assert_eq!(shell.run_script("printf '%s' \"$(trap 'echo $((1 / 0))' EXIT; printf ok; exit 7)\" >outer"), 0);
        assert_eq!(std::fs::read(dir.path().join("outer")).unwrap(), b"ok");
        assert_eq!(shell.run_script("value=$(trap 'false' EXIT; printf retained; exit 7)"), 7);
        assert_eq!(shell.get_var("value"), Some("retained"));
    });
}

#[test]
fn variable_assignment_rule_preserves_options_positionals_and_raw_values() {
    isolated("variable_assignment_rule_preserves_options_positionals_and_raw_values", || {
        let dir = tempfile::tempdir().unwrap();
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).stdout_sink(output.clone()).build();
        shell.set_command_rule(epsh::shell_bytes::ShellBytes::from("set"), epsh::eval::CommandRule::VariableAssignment);
        let arguments = [epsh::shell_bytes::ShellBytes::from("ish"), epsh::shell_bytes::ShellBytes::from("first"), epsh::shell_bytes::ShellBytes::from_vec(vec![0xff])];
        shell.set_args_bytes(&arguments);
        for operand in ["-e", "+e", "-o", "-", "--", "7", "NAME=value", ""] {
            shell.set_var("OPERAND", operand).unwrap();
            std::fs::write(dir.path().join("target"), b"preserved").unwrap();
            assert_eq!(shell.run_script("SIDE=$(/bin/touch assigned) command -p command -- set \"$OPERAND\" value >target"), 2);
            assert_eq!(std::fs::read(dir.path().join("target")).unwrap(), b"preserved");
            assert!(!dir.path().join("assigned").exists());
            assert!(shell.get_var("SIDE").is_none());
            assert_eq!(shell.opts().flags_string(), "");
            assert!(!shell.opts().pipefail);
            assert!(!shell.opts().noexec);
            assert_eq!(shell.vars().positional, arguments[1..]);
        }
        shell.set_var_bytes("RAW", epsh::shell_bytes::ShellBytes::from_vec(vec![0xff, b' ', 0x80])).unwrap();
        assert_eq!(shell.run_script("set VALUE \"$RAW\" -option +value; set EMPTY; set"), 0);
        let expected = b"\xff \x80 -option +value";
        assert_eq!(shell.get_var_bytes("VALUE").unwrap().as_bytes(), expected);
        assert_eq!(shell.get_var_bytes("EMPTY").unwrap().as_bytes(), b"");
        let output = output.lock().unwrap();
        assert!(output.split(|byte| *byte == b'\n').any(|line| line == b"VALUE=\xff \x80 -option +value"));
        assert_eq!(shell.vars().positional, arguments[1..]);
        let mut standalone = Shell::new();
        assert_eq!(standalone.run_script("set -- retained; set -e; set -o pipefail"), 0);
        assert!(standalone.opts().errexit);
        assert!(standalone.opts().pipefail);
        assert_eq!(standalone.vars().positional, [epsh::shell_bytes::ShellBytes::from("retained")]);
    });
}
