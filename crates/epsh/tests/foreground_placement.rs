use std::fs;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use epsh::ast::Command;
use epsh::error::ExitStatus;
use epsh::eval::{CommandPlacement, Shell};
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use tempfile::tempdir;

fn register_copy(shell: &mut Shell) {
    shell.set_command_placement(
        ShellBytes::from("cp"),
        CommandPlacement::ParentProcess,
    );
}

fn record_copy_calls(shell: &mut Shell) -> Arc<Mutex<Vec<Vec<Vec<u8>>>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    shell.set_external_handler(Box::new(move |args, _environment| {
        if args.first().is_some_and(|target| target.as_bytes() == b"cp") {
            recorded
                .lock()
                .unwrap()
                .push(args.iter().map(|argument| argument.as_bytes().to_vec()).collect());
        }
        Ok(ExitStatus::SUCCESS)
    }));
    calls
}

#[test]
fn parent_process_commands_run_in_lists_with_redirections_and_argument_substitution() {
    let directory = tempdir().unwrap();
    let mut shell = Shell::builder().cwd(directory.path().to_path_buf()).build();
    register_copy(&mut shell);
    let calls = record_copy_calls(&mut shell);

    assert_eq!(shell.run_script("cp source destination && command cp next target"), 0);
    let redirected = directory.path().join("copy.log");
    assert_eq!(
        shell.run_script("cp source destination > copy.log"),
        0
    );
    assert!(redirected.exists());
    assert_eq!(
        shell.run_script("cp \"$(printf expanded-source)\" destination"),
        0
    );

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0][0], b"cp");
    assert_eq!(calls[1][1], b"next");
    assert_eq!(calls[2][1], b"source");
    assert_eq!(calls[3][1], b"expanded-source");
}

#[test]
fn standalone_negation_runs_parent_process_commands_and_inverts_their_status() {
    let directory = tempdir().unwrap();
    let mut shell = Shell::builder().cwd(directory.path().to_path_buf()).build();
    register_copy(&mut shell);
    let calls = record_copy_calls(&mut shell);

    assert_eq!(shell.run_script("! cp source destination > copy.log"), 1);
    assert!(directory.path().join("copy.log").exists());
    assert_eq!(shell.run_script("! command cp next target"), 1);
    assert_eq!(shell.run_script("! cp third target && cp skipped target"), 1);

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0][0], b"cp");
    assert_eq!(calls[1][1], b"next");
    assert_eq!(calls[2][1], b"third");

    let mut ordinary = Shell::new();
    assert_eq!(ordinary.run_script("! /bin/sh -c 'exit 2'"), 0);
    assert_eq!(ordinary.run_script("! printf input | /bin/sh -c 'exit 253'"), 0);
}

#[test]
fn standalone_negation_does_not_invert_cancellation_or_continue_the_list() {
    let cancel = Arc::new(AtomicBool::new(true));
    let mut shell = Shell::builder().cancel_flag(cancel).build();
    register_copy(&mut shell);
    let calls = record_copy_calls(&mut shell);

    assert_eq!(shell.run_script("! true && cp source destination"), 130);
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn command_identity_is_independent_of_parent_process_placement() {
    let directory = tempdir().unwrap();
    let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder()
        .cwd(directory.path().to_path_buf())
        .env_clear()
        .stdout_sink(stdout.clone())
        .build();
    shell.set_var("PATH", "").unwrap();
    shell.set_command_identity(
        ShellBytes::from("cp"),
        epsh::eval::CommandIdentity::Builtin,
    );
    shell.set_command_identity(
        ShellBytes::from("/bin/cp"),
        epsh::eval::CommandIdentity::Builtin,
    );
    shell.set_command_placement(
        ShellBytes::from("parent-only"),
        CommandPlacement::ParentProcess,
    );

    assert_eq!(shell.run_script("command -v cp"), 0);
    assert_eq!(*stdout.lock().unwrap(), b"cp\n");
    stdout.lock().unwrap().clear();
    assert_eq!(shell.run_script("command -V cp"), 0);
    assert_eq!(*stdout.lock().unwrap(), b"cp is a shell builtin\n");
    stdout.lock().unwrap().clear();
    assert_eq!(shell.run_script("type cp"), 0);
    assert_eq!(*stdout.lock().unwrap(), b"cp is a shell builtin\n");
    stdout.lock().unwrap().clear();
    assert_eq!(shell.run_script("command -v /bin/cp"), 0);
    assert_eq!(*stdout.lock().unwrap(), b"/bin/cp\n");
    stdout.lock().unwrap().clear();
    assert_eq!(shell.run_script("command -v parent-only"), 1);
    assert!(stdout.lock().unwrap().is_empty());
}

#[test]
fn parent_process_refusal_precedes_redirections_and_captured_callbacks() {
    let directory = tempdir().unwrap();
    let policy_marker = directory.path().join("policy-called");
    let handler_marker = directory.path().join("handler-called");
    let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder()
        .cwd(directory.path().to_path_buf())
        .stderr_sink(stderr.clone())
        .build();
    register_copy(&mut shell);

    let policy_marker_copy = policy_marker.clone();
    shell.set_command_policy(Box::new(move |target| {
        if target.as_bytes() == b"cp" {
            fs::write(&policy_marker_copy, b"called").unwrap();
        }
        None
    }));
    let handler_marker_copy = handler_marker.clone();
    shell.set_external_handler(Box::new(move |args, _environment| {
        if args.first().is_some_and(|target| target.as_bytes() == b"cp") {
            fs::write(&handler_marker_copy, b"called").unwrap();
        }
        Ok(ExitStatus::SUCCESS)
    }));

    for (index, script) in [
        "printf input | cp source destination > static-redirection",
        "printf input | command command cp source destination > nested-redirection",
        "cmd=cp; printf input | $cmd source destination > dynamic-redirection",
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(shell.run_script(script), 2, "script: {script}");
        assert!(!directory.path().join(format!(
            "{}-redirection",
            ["static", "nested", "dynamic"][index]
        )).exists());
    }

    assert_eq!(
        shell.run_script("value=$(command cp source destination > substitution-redirection)"),
        2
    );
    assert_eq!(
        shell.run_script(
            "cmd=cp; value=$(command $cmd source destination > dynamic-substitution-redirection)"
        ),
        2
    );
    for name in [
        "static-redirection",
        "nested-redirection",
        "dynamic-redirection",
        "substitution-redirection",
        "dynamic-substitution-redirection",
    ] {
        assert!(!directory.path().join(name).exists(), "{name}");
    }
    assert!(!policy_marker.exists());
    assert!(!handler_marker.exists());
    let diagnostic = String::from_utf8(stderr.lock().unwrap().clone()).unwrap();
    assert!(diagnostic.contains("cp"), "{diagnostic}");
    assert!(diagnostic.contains("original shell process"), "{diagnostic}");
}

#[test]
fn negated_pipelines_and_substitutions_keep_parent_process_refusals() {
    let directory = tempdir().unwrap();
    let handler_marker = directory.path().join("handler-called");
    let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder()
        .cwd(directory.path().to_path_buf())
        .stderr_sink(stderr.clone())
        .build();
    register_copy(&mut shell);
    let handler_marker_copy = handler_marker.clone();
    shell.set_external_handler(Box::new(move |args, _environment| {
        if args.first().is_some_and(|target| target.as_bytes() == b"cp") {
            fs::write(&handler_marker_copy, b"called").unwrap();
        }
        Ok(ExitStatus::SUCCESS)
    }));

    assert_eq!(
        shell.run_script(
            "cmd=cp; ! printf input | command command $cmd source pipe-copy > negated-pipe-redirection"
        ),
        0
    );
    assert_eq!(
        shell.run_script(
            "cmd=cp; value=$(! command command $cmd source substitution-copy > negated-substitution-redirection)"
        ),
        0
    );
    for name in [
        "pipe-copy",
        "negated-pipe-redirection",
        "substitution-copy",
        "negated-substitution-redirection",
    ] {
        assert!(!directory.path().join(name).exists(), "{name}");
    }
    assert!(!handler_marker.exists());
    let diagnostic = String::from_utf8(stderr.lock().unwrap().clone()).unwrap();
    assert!(diagnostic.contains("cp"), "{diagnostic}");
    assert!(diagnostic.contains("original shell process"), "{diagnostic}");
}

#[test]
fn non_utf8_parent_process_targets_are_matched_and_escaped_in_diagnostics() {
    let directory = tempdir().unwrap();
    let target = vec![b'c', 0x1b, 0xff];
    let policy_marker = directory.path().join("policy-called");
    let handler_marker = directory.path().join("handler-called");
    let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut shell = Shell::builder()
        .cwd(directory.path().to_path_buf())
        .stderr_sink(stderr.clone())
        .build();
    shell.set_command_placement(
        ShellBytes::from_vec(target.clone()),
        CommandPlacement::ParentProcess,
    );

    let policy_target = target.clone();
    let policy_marker_copy = policy_marker.clone();
    shell.set_command_policy(Box::new(move |name| {
        if name.as_bytes() == policy_target {
            fs::write(&policy_marker_copy, b"called").unwrap();
        }
        None
    }));
    let handler_target = target.clone();
    let handler_marker_copy = handler_marker.clone();
    shell.set_external_handler(Box::new(move |args, _environment| {
        if args.first().is_some_and(|name| name.as_bytes() == handler_target) {
            fs::write(&handler_marker_copy, b"called").unwrap();
        }
        Ok(ExitStatus::SUCCESS)
    }));

    let mut source = b"printf input | '".to_vec();
    source.extend_from_slice(&target);
    source.extend_from_slice(b"' source destination > byte-name-redirection");
    let program = Parser::new_bytes(&source).parse().unwrap();
    assert_eq!(shell.run_program(&program).code(), 2);
    assert!(!directory.path().join("byte-name-redirection").exists());
    assert!(!policy_marker.exists());
    assert!(!handler_marker.exists());
    let diagnostic = String::from_utf8(stderr.lock().unwrap().clone()).unwrap();
    assert!(diagnostic.contains("c\\x1b\\xff"), "{diagnostic:?}");
    assert!(!diagnostic.as_bytes().contains(&0x1b));
}

#[test]
fn placement_names_are_owned_bytes_and_unregistered_embedders_keep_default_behavior() {
    let directory = tempdir().unwrap();
    let mut unrestricted = Shell::builder().cwd(directory.path().to_path_buf()).build();
    let default_marker = directory.path().join("default-handler-called");
    let default_marker_copy = default_marker.clone();
    unrestricted.set_external_handler(Box::new(move |args, _environment| {
        if args.first().is_some_and(|target| target.as_bytes() == b"cp") {
            fs::write(&default_marker_copy, b"called").unwrap();
        }
        Ok(ExitStatus::SUCCESS)
    }));
    assert_eq!(unrestricted.run_script("printf input | cp source destination"), 0);
    assert!(default_marker.exists());

    let mut placed = Shell::builder().cwd(directory.path().to_path_buf()).build();
    let calls = record_copy_calls(&mut placed);
    let mut name = b"cp".to_vec();
    placed.set_command_placement(
        ShellBytes::from_vec(name.clone()),
        CommandPlacement::ParentProcess,
    );
    name.fill(b'x');
    assert_eq!(placed.run_script("printf input | cp source destination"), 2);
    assert_eq!(calls.lock().unwrap().len(), 0);
}

#[test]
fn single_stage_pipeline_fast_path_still_refuses_parent_process_commands() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("handler-called");
    let marker_copy = marker.clone();
    let mut shell = Shell::builder().cwd(directory.path().to_path_buf()).build();
    register_copy(&mut shell);
    shell.set_external_handler(Box::new(move |args, _environment| {
        if args.first().is_some_and(|target| target.as_bytes() == b"cp") {
            fs::write(&marker_copy, b"called").unwrap();
        }
        Ok(ExitStatus::SUCCESS)
    }));

    let simple = Parser::new("cp source destination > logical-pipeline-redirection")
        .parse()
        .unwrap()
        .commands
        .remove(0);
    let pipeline = Command::Pipeline {
        commands: vec![simple],
        bang: false,
        span: Default::default(),
    };
    assert_eq!(shell.eval_command(&pipeline).unwrap(), ExitStatus::MISUSE);
    assert!(!marker.exists());
    assert!(!directory.path().join("logical-pipeline-redirection").exists());
}
