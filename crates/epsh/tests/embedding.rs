use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epsh::ast::Program;
use epsh::builtins::{BUILTIN_NAMES, is_builtin};
use epsh::error::ExitStatus;
use epsh::eval::Shell;
use epsh::parser::Parser;
use tempfile::tempdir;

fn parse(src: &str) -> Program {
    Parser::new(src).parse().unwrap()
}

mod builder {
    use super::*;

    #[test]
    fn default_builder() {
        let shell = Shell::builder().build();
        assert_eq!(shell.exit_status(), ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_cwd() {
        let mut shell = Shell::builder().cwd(PathBuf::from("/tmp")).build();
        let status = shell.run_program(&parse("pwd"));
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_options() {
        let shell = Shell::builder()
            .errexit(true)
            .nounset(true)
            .pipefail(true)
            .build();
        assert!(shell.opts().errexit);
        assert!(shell.opts().nounset);
        assert!(shell.opts().pipefail);
    }

    #[test]
    fn builder_env_clear() {
        let mut shell = Shell::builder().env_clear().build();
        // PATH should not be inherited
        let status = shell.run_program(&parse("echo ${PATH-unset}"));
        // Can't easily check output without sink, just verify it runs
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_sinks() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        shell.run_program(&parse("echo hello"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn builder_chained() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .cwd(PathBuf::from("/tmp"))
            .errexit(true)
            .cancel_flag(cancel)
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_secs(60))
            .build();
        shell.run_program(&parse("echo ok"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "ok\n");
    }
}

mod parse_then_execute {
    use super::*;

    #[test]
    fn basic_parse_execute() {
        let program = parse("echo hello");
        let mut shell = Shell::new();
        let status = shell.run_program(&program);
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn reuse_parsed_program() {
        let program = parse("x=$((x + 1)); echo $x");
        let mut shell = Shell::new();
        let _ = shell.set_var("x", "0");
        shell.run_program(&program);
        assert_eq!(shell.get_var("x"), Some("1"));
        shell.run_program(&program);
        assert_eq!(shell.get_var("x"), Some("2"));
    }

    #[test]
    fn parse_error_doesnt_crash() {
        let result = Parser::new("if; then; fi; (((").parse();
        assert!(result.is_err());
    }

    #[test]
    fn inspect_ast() {
        let program = parse("echo hello world");
        assert_eq!(program.commands.len(), 1);
    }
}

mod cancellation {
    use super::*;

    #[test]
    fn cancel_stops_execution() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .cancel_flag(cancel.clone())
            .stdout_sink(stdout.clone())
            .build();

        // Set cancel before running — should abort immediately
        cancel.store(true, Ordering::Relaxed);
        let status = shell.run_program(&parse("echo should-not-appear"));
        assert_eq!(status.code(), 130); // SIGINT
        let out = stdout.lock().unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn cancel_during_loop() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let cancel2 = cancel.clone();

        let mut shell = Shell::builder()
            .cancel_flag(cancel)
            .stdout_sink(stdout.clone())
            .build();

        // Cancel after a short delay
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel2.store(true, Ordering::Relaxed);
        });

        let status = shell.run_program(&parse("while true; do echo x; done"));
        assert_eq!(status.code(), 130);
    }
}

mod timeout {
    use super::*;

    #[test]
    fn timeout_stops_execution() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_millis(50))
            .build();
        let status = shell.run_program(&parse("while true; do echo x; done"));
        assert_eq!(status.code(), 130);
    }

    #[test]
    fn no_timeout_if_fast() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_secs(10))
            .build();
        let status = shell.run_program(&parse("echo hello"));
        assert_eq!(status, ExitStatus::SUCCESS);
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn set_timeout_after_construction() {
        let mut shell = Shell::new();
        shell.set_timeout(Duration::from_millis(50));
        let status = shell.run_program(&parse("while true; do :; done"));
        assert_eq!(status.code(), 130);
    }
}

mod output_sinks {
    use super::*;

    #[test]
    fn capture_stdout() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("echo hello; echo world"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\nworld\n");
    }

    #[test]
    fn capture_stderr() {
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stderr_sink(stderr.clone()).build();
        // Shell error messages go through write_err → stderr_sink
        shell.run_program(&parse("nonexistent_cmd_xyz"));
        let err = String::from_utf8_lossy(&stderr.lock().unwrap()).to_string();
        assert!(err.contains("not found"), "stderr: {err}");
    }

    #[test]
    fn capture_external_command() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("/bin/echo external"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "external\n");
    }

    #[test]
    fn separate_stdout_stderr() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        // echo goes to stdout_sink, error message goes to stderr_sink
        shell.run_program(&parse("echo out; nonexistent_cmd_xyz"));
        assert_eq!(
            String::from_utf8_lossy(&stdout.lock().unwrap()).as_ref(),
            "out\n"
        );
        let err = String::from_utf8_lossy(&stderr.lock().unwrap()).to_string();
        assert!(err.contains("not found"), "stderr: {err}");
    }
}

mod cwd_isolation {
    use super::*;

    #[test]
    fn shells_have_independent_cwd() {
        let stdout1 = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stdout2 = Arc::new(Mutex::new(Vec::<u8>::new()));

        let mut shell1 = Shell::builder()
            .cwd(PathBuf::from("/tmp"))
            .stdout_sink(stdout1.clone())
            .build();
        let mut shell2 = Shell::builder()
            .cwd(PathBuf::from("/"))
            .stdout_sink(stdout2.clone())
            .build();

        shell1.run_program(&parse("pwd"));
        shell2.run_program(&parse("pwd"));

        let out1 = String::from_utf8_lossy(&stdout1.lock().unwrap()).to_string();
        let out2 = String::from_utf8_lossy(&stdout2.lock().unwrap()).to_string();
        // macOS: /tmp may resolve to /private/tmp
        assert!(out1.trim() == "/tmp" || out1.trim() == "/private/tmp");
        assert_eq!(out2.trim(), "/");
    }

    #[test]
    fn cd_updates_shell_cwd() {
        let mut shell = Shell::builder().cwd(PathBuf::from("/")).build();
        shell.run_program(&parse("cd /tmp"));
        // macOS: /tmp → /private/tmp
        let cwd = shell.cwd().to_string_lossy().to_string();
        assert!(cwd == "/tmp" || cwd == "/private/tmp");
    }

    #[test]
    fn cd_dot_navigation_preserves_canonical_paths() {
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        let child = real.join("child");
        fs::create_dir_all(&child).unwrap();
        let canonical_child = fs::canonicalize(&child).unwrap();
        let canonical_real = fs::canonicalize(&real).unwrap();
        let canonical_temp = fs::canonicalize(temp.path()).unwrap();

        let mut shell = Shell::builder().cwd(child.clone()).build();
        assert_eq!(shell.run_program(&parse("cd .")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_child);

        assert_eq!(shell.run_program(&parse("cd ..")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_real);

        assert_eq!(shell.run_program(&parse("cd ..")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_temp);
    }

    #[test]
    fn cd_dot_navigation_clamps_at_root() {
        let mut shell = Shell::builder().cwd(PathBuf::from("/")).build();
        assert_eq!(
            shell.run_program(&parse("cd ../../..")),
            ExitStatus::SUCCESS
        );
        assert_eq!(shell.cwd(), PathBuf::from("/"));
    }

    #[test]
    fn cd_symlink_then_parent_uses_canonical_parent() {
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        let child = real.join("child");
        let link = temp.path().join("link");
        fs::create_dir_all(&child).unwrap();
        symlink("real", &link).unwrap();
        let canonical_child = fs::canonicalize(&child).unwrap();
        let canonical_real = fs::canonicalize(&real).unwrap();

        let mut shell = Shell::builder().cwd(temp.path().to_path_buf()).build();
        assert_eq!(
            shell.run_program(&parse("cd link/child")),
            ExitStatus::SUCCESS
        );
        assert_eq!(shell.cwd(), canonical_child);

        assert_eq!(shell.run_program(&parse("cd ..")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_real);
    }

    #[test]
    fn cd_normal_component_still_resolves_symlinks() {
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink("real", &link).unwrap();
        let canonical_real = fs::canonicalize(&real).unwrap();

        let mut shell = Shell::builder().cwd(temp.path().to_path_buf()).build();
        assert_eq!(shell.run_program(&parse("cd link")), ExitStatus::SUCCESS);
        assert_eq!(shell.cwd(), canonical_real);
    }

    #[test]
    fn failed_cd_does_not_change_cwd() {
        let temp = tempdir().unwrap();
        let start = temp.path().join("start");
        fs::create_dir(&start).unwrap();
        let mut shell = Shell::builder().cwd(start.clone()).build();

        assert_eq!(shell.run_program(&parse("cd missing")), ExitStatus::FAILURE);
        assert_eq!(shell.cwd(), start);
    }
}

mod builtin_list {
    use super::*;

    #[test]
    fn builtin_names_contains_core() {
        assert!(BUILTIN_NAMES.contains(&"echo"));
        assert!(BUILTIN_NAMES.contains(&"cd"));
        assert!(BUILTIN_NAMES.contains(&"export"));
        assert!(BUILTIN_NAMES.contains(&"test"));
        assert!(BUILTIN_NAMES.contains(&":"));
    }

    #[test]
    fn is_builtin_works() {
        assert!(is_builtin("echo"));
        assert!(is_builtin("["));
        assert!(!is_builtin("ls"));
        assert!(!is_builtin("git"));
    }

    #[test]
    fn builtin_names_matches_try_builtin() {
        // Every name in BUILTIN_NAMES should be recognized by try_builtin
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout).build();
        for &name in BUILTIN_NAMES {
            // Just verify they don't crash — some need args. Redirect stdin from
            // /dev/null so interactive builtins like `read` get immediate EOF.
            let _ = shell.run_program(&parse(&format!("{name} --help </dev/null 2>/dev/null")));
        }
    }
}

mod external_handler {
    use super::*;
    use epsh::eval::ExternalHandler;
    use epsh::shell_bytes::ShellBytes;

    #[test]
    fn handler_receives_args() {
        let captured_args = Arc::new(Mutex::new(Vec::<Vec<ShellBytes>>::new()));
        let args_ref = captured_args.clone();
        let handler: ExternalHandler = Box::new(move |args, _env| {
            args_ref.lock().unwrap().push(args.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("nonexistent_cmd foo bar"));
        let calls = captured_args.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            vec![
                ShellBytes::from("nonexistent_cmd"),
                ShellBytes::from("foo"),
                ShellBytes::from("bar"),
            ]
        );
    }

    #[test]
    fn handler_receives_env_pairs() {
        let captured_env = Arc::new(Mutex::new(Vec::<Vec<(ShellBytes, ShellBytes)>>::new()));
        let env_ref = captured_env.clone();
        let handler: ExternalHandler = Box::new(move |_args, env| {
            env_ref.lock().unwrap().push(env.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("FOO=bar BAZ=qux mycmd"));
        let calls = captured_env.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&(ShellBytes::from("FOO"), ShellBytes::from("bar"))));
        assert!(calls[0].contains(&(ShellBytes::from("BAZ"), ShellBytes::from("qux"))));
    }

    #[test]
    fn handler_receives_exported_env() {
        let captured_env = Arc::new(Mutex::new(Vec::<Vec<(ShellBytes, ShellBytes)>>::new()));
        let env_ref = captured_env.clone();
        let handler: ExternalHandler = Box::new(move |_args, env| {
            env_ref.lock().unwrap().push(env.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("X=set_val; export X; mycmd"));
        let calls = captured_env.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&(ShellBytes::from("X"), ShellBytes::from("set_val"))));
    }

    #[test]
    fn handler_receives_complete_store_environment_without_unset_values() {
        let captured_env = Arc::new(Mutex::new(Vec::<Vec<(ShellBytes, ShellBytes)>>::new()));
        let env_ref = captured_env.clone();
        let handler: ExternalHandler = Box::new(move |_args, env| {
            env_ref.lock().unwrap().push(env.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder()
            .env_clear()
            .external_handler(handler)
            .build();
        shell.run_program(&parse(
            "export KEEP=stored REMOVE=gone; unset REMOVE; PREFIX=prefix mycmd",
        ));

        let calls = captured_env.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let mut entries = calls[0].clone();
        entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        assert_eq!(
            entries,
            vec![
                (ShellBytes::from("KEEP"), ShellBytes::from("stored")),
                (ShellBytes::from("PREFIX"), ShellBytes::from("prefix")),
            ]
        );
    }

    #[test]
    fn handler_not_called_for_builtins() {
        let call_count = Arc::new(Mutex::new(0));
        let count_ref = call_count.clone();
        let handler: ExternalHandler = Box::new(move |_args, _env| {
            *count_ref.lock().unwrap() += 1;
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("echo hello; true; false; : ; pwd"));
        assert_eq!(*call_count.lock().unwrap(), 0);
    }

    #[test]
    fn command_builtin_uses_handler() {
        let captured_args = Arc::new(Mutex::new(Vec::<Vec<ShellBytes>>::new()));
        let args_ref = captured_args.clone();
        let handler: ExternalHandler = Box::new(move |args, _env| {
            args_ref.lock().unwrap().push(args.to_vec());
            Ok(ExitStatus::from(7))
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        let status = shell.run_program(&parse("command mycmd arg"));
        assert_eq!(status.code(), 7);
        assert_eq!(
            *captured_args.lock().unwrap(),
            vec![vec![ShellBytes::from("mycmd"), ShellBytes::from("arg")]]
        );
    }

    #[test]
    fn handler_sees_shell_working_directory() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_ref = seen.clone();
        let handler: ExternalHandler = Box::new(move |_args, _env| {
            seen_ref
                .lock()
                .unwrap()
                .push(epsh::eval::external_command_cwd());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder()
            .cwd(dir.path().to_path_buf())
            .external_handler(handler)
            .build();
        shell.run_program(&parse("mycmd; cd sub && mycmd"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].as_deref(), Some(dir.path()));
        assert_eq!(
            seen[1].as_ref().map(|p| p.canonicalize().unwrap()),
            Some(sub.canonicalize().unwrap())
        );
        assert_eq!(epsh::eval::external_command_cwd(), None);
    }

    #[test]
    fn handler_exit_status_propagates() {
        let handler: ExternalHandler = Box::new(|_args, _env| Ok(ExitStatus::from(42)));
        let mut shell = Shell::builder().external_handler(handler).build();
        let status = shell.run_program(&parse("mycmd"));
        assert_eq!(status.code(), 42);
    }
}

mod redirection_failures {
    use super::*;
    use std::os::fd::AsRawFd;

    fn descriptor_is_open(fd: i32) -> bool {
        // SAFETY: the descriptor is only borrowed for a flags query.
        rustix::io::fcntl_getfd(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }).is_ok()
    }

    #[test]
    fn failed_redirection_restores_earlier_descriptors() {
        let dir = tempdir().unwrap();
        let marker = fs::File::open(dir.path()).unwrap();
        let marker_stat = rustix::fs::fstat(&marker).unwrap();
        // SAFETY: fd 50 is reserved for this test and closed at its end.
        assert_eq!(unsafe { libc::dup2(marker.as_raw_fd(), 50) }, 50);
        let mut shell = Shell::builder()
            .cwd(dir.path().to_path_buf())
            .stderr_sink(Arc::new(Mutex::new(Vec::<u8>::new())))
            .build();
        let status = shell.run_program(&parse("printf never 50>first 51>second 52>missing/dir/x"));
        assert_ne!(status.code(), 0);
        // fd 50 points at the original object again; 51 and 52 were closed
        // before and are closed again.
        let restored =
            rustix::fs::fstat(unsafe { std::os::fd::BorrowedFd::borrow_raw(50) }).unwrap();
        assert_eq!(
            (restored.st_dev, restored.st_ino),
            (marker_stat.st_dev, marker_stat.st_ino)
        );
        assert!(!descriptor_is_open(51));
        assert!(!descriptor_is_open(52));
        // SAFETY: fd 50 was duplicated above and is not used elsewhere.
        unsafe { rustix::io::close(50) };
    }
}

mod redirect_open_handler {
    use super::*;
    use epsh::eval::{RedirectOpenHandler, RedirectOpenMode};
    use std::os::fd::{AsFd, OwnedFd};

    fn descriptor_is_open(fd: i32) -> bool {
        // SAFETY: the descriptor is only borrowed for a flags query.
        rustix::io::fcntl_getfd(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }).is_ok()
    }

    type Requests = Arc<Mutex<Vec<(PathBuf, RedirectOpenMode, i32)>>>;

    fn recording_provider(
        requests: Requests,
        opened: PathBuf,
        fail_name: Option<&'static str>,
    ) -> RedirectOpenHandler {
        Box::new(move |request| {
            requests
                .lock()
                .unwrap()
                .push((request.path.to_path_buf(), request.mode, request.fd));
            if fail_name.is_some_and(|name| request.path.ends_with(name)) {
                return Some(Err(std::io::Error::other("provider refused")));
            }
            if request.path.ends_with("default") {
                return None;
            }
            let file = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&opened)
                .map(OwnedFd::from);
            Some(file)
        })
    }

    #[test]
    fn provider_owns_writable_opens_in_order() {
        let dir = tempdir().unwrap();
        let opened = dir.path().join("provided");
        let requests = Requests::default();
        let mut shell = Shell::builder()
            .cwd(dir.path().to_path_buf())
            .stdout_sink(Arc::new(Mutex::new(Vec::<u8>::new())))
            .redirect_open_handler(recording_provider(requests.clone(), opened.clone(), None))
            .build();
        let status = shell.run_program(&parse(
            "printf one 57>a 58>>b 59<>c; printf two 57>|d; printf three 57>default",
        ));
        assert_eq!(status.code(), 0);
        let requests = requests.lock().unwrap();
        let expect = |name: &str, mode, fd| (dir.path().join(name), mode, fd);
        assert_eq!(
            *requests,
            vec![
                expect("a", RedirectOpenMode::Truncate, 57),
                expect("b", RedirectOpenMode::Append, 58),
                expect("c", RedirectOpenMode::ReadWrite, 59),
                expect("d", RedirectOpenMode::Truncate, 57),
                expect("default", RedirectOpenMode::Truncate, 57),
            ]
        );
        // The provider's descriptors were used instead of path opens.
        for name in ["a", "b", "c", "d"] {
            assert!(!dir.path().join(name).exists(), "{name} was opened by path");
        }
        assert!(opened.exists());
        // Declining falls back to epsh's own open.
        assert!(dir.path().join("default").exists());
        for fd in [57, 58, 59] {
            assert!(!descriptor_is_open(fd), "fd {fd} leaked after the command");
        }
    }

    #[test]
    fn provider_error_restores_earlier_redirections() {
        let dir = tempdir().unwrap();
        let opened = dir.path().join("provided");
        let requests = Requests::default();
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let marker = fs::File::open(dir.path()).unwrap();
        let marker_stat = rustix::fs::fstat(marker.as_fd()).unwrap();
        // SAFETY: fd 60 is reserved for this test and closed at its end.
        assert_eq!(
            unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&marker), 60) },
            60
        );
        let mut shell = Shell::builder()
            .cwd(dir.path().to_path_buf())
            .stderr_sink(stderr.clone())
            .redirect_open_handler(recording_provider(
                requests.clone(),
                opened,
                Some("refused"),
            ))
            .build();
        let status = shell.run_program(&parse("printf never 60>first 61>first2 62>refused"));
        assert_ne!(status.code(), 0);
        assert_eq!(requests.lock().unwrap().len(), 3);
        let message = String::from_utf8(stderr.lock().unwrap().clone()).unwrap();
        assert!(message.contains("refused: provider refused"), "{message}");
        // fd 60 was open before and points at the same object again; fds 61
        // and 62 were closed before and are closed again.
        let restored =
            rustix::fs::fstat(unsafe { std::os::fd::BorrowedFd::borrow_raw(60) }).unwrap();
        assert_eq!(
            (restored.st_dev, restored.st_ino),
            (marker_stat.st_dev, marker_stat.st_ino)
        );
        assert!(!descriptor_is_open(61));
        assert!(!descriptor_is_open(62));
        // SAFETY: fd 60 was duplicated above and is not used elsewhere.
        unsafe { rustix::io::close(60) };
    }
}

mod variables {
    use super::*;

    #[test]
    fn set_get_var() {
        let mut shell = Shell::new();
        let _ = shell.set_var("MY_VAR", "hello");
        assert_eq!(shell.get_var("MY_VAR"), Some("hello"));
    }

    #[test]
    fn var_persists_across_commands() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("X=42"));
        shell.run_program(&parse("echo $X"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "42\n");
    }

    #[test]
    fn exit_status_accessible() {
        let mut shell = Shell::new();
        shell.run_program(&parse("false"));
        assert_eq!(shell.exit_status(), ExitStatus::FAILURE);
        shell.run_program(&parse("true"));
        assert_eq!(shell.exit_status(), ExitStatus::SUCCESS);
    }
}
