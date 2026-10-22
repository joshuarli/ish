use epsh::{ast::Command, eval::Shell, expand::expand_word_to_fields, parser::Parser};

fn fields(source: &str, shell: &mut Shell) -> Vec<String> {
    let program = Parser::new(source).parse().unwrap();
    let Command::Simple { args: words, .. } = &program.commands[0] else { panic!("expected simple command") };
    words.iter().flat_map(|word| expand_word_to_fields(word, shell).unwrap()).collect()
}

#[test]
fn quoted_glob_bytes_remain_literal_in_existing_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("matched"), []).unwrap();
    let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
    assert_eq!(fields("'*' \"*\"", &mut shell), ["*", "*"]);
}

#[test]
fn empty_quoted_at_preserves_surrounding_text() {
    let mut shell = Shell::new();
    shell.set_args(&[]);
    assert_eq!(fields("\"prefix$@suffix\"", &mut shell), ["prefixsuffix"]);
    assert!(fields("\"$@\"", &mut shell).is_empty());
}

#[test]
fn quote_masks_survive_mixed_globbing_and_field_splitting() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["*one", "*two", "other"] {
        std::fs::write(dir.path().join(name), []).unwrap();
    }
    let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
    shell.set_var("X", "*").unwrap();
    shell.set_var("Y", "left right").unwrap();
    assert_eq!(fields("'*'*", &mut shell), ["*one", "*two"]);
    assert_eq!(fields("\"$X\"", &mut shell), ["*"]);
    assert_eq!(fields("'*'$Y", &mut shell), ["*left", "right"]);
    assert_eq!(fields("${UNSET:-'*'}", &mut shell), ["*"]);
    assert_eq!(fields("${ASSIGNED:='*'}", &mut shell), ["*one", "*two", "other"]);
    assert_eq!(shell.get_var("ASSIGNED"), Some("*"));
}

#[test]
fn quoted_at_retains_order_and_first_last_concatenation() {
    let mut shell = Shell::new();
    shell.set_args(&["ish", "a", "", "b"]);
    assert_eq!(fields("\"prefix$@suffix\"", &mut shell), ["prefixa", "", "bsuffix"]);
    shell.set_args(&["ish", "*"]);
    assert_eq!(fields("\"$@\"", &mut shell), ["*"]);
}

#[test]
fn scalar_expansion_retains_literal_glob_bytes() {
    let program = Parser::new("\"*?[]\\\\\"").parse().unwrap();
    let epsh::ast::Command::Simple { args, .. } = &program.commands[0] else { panic!("expected simple command") };
    let mut shell = Shell::new();
    assert_eq!(epsh::expand::expand_word_to_string(&args[0], &mut shell).unwrap(), "*?[]\\");
    shell.set_var("HOME", "*?").unwrap();
    assert_eq!(fields("~", &mut shell), ["*?"]);
}

#[test]
fn repeated_quoted_at_merges_adjacent_edge_fields() {
    let mut shell = Shell::new();
    shell.set_args(&["ish", "a", "b"]);
    assert_eq!(fields("\"$@$@\"", &mut shell), ["a", "ba", "b"]);
}

#[test]
fn quoted_raw_bytes_and_effect_results_preserve_expansion_contract() {
    let program = Parser::new_bytes(b"'\xff\x00*' \"$RAW\"").parse().unwrap();
    let epsh::ast::Command::Simple { args, .. } = &program.commands[0] else { panic!("expected simple command") };
    let mut shell = Shell::new();
    shell.set_var_bytes("RAW", vec![255, 0, b'*'].into()).unwrap();
    let expanded: Vec<Vec<u8>> = args.iter()
        .flat_map(|word| expand_word_to_fields(word, &mut shell).unwrap())
        .map(|field| epsh::shell_bytes::ShellBytes::from_str_lossless(&field).into_vec())
        .collect();
    assert_eq!(expanded, [vec![255, 0, b'*'], vec![255, 0, b'*']]);
    assert_eq!(fields("\"$(printf 'a\\n\\n')\" \"$((1 + 2))\"", &mut shell), ["a", "3"]);
}

#[test]
fn command_substitution_does_not_inherit_locked_stdout() {
    const PROBE: &str = "EPSH_STDOUT_LOCK_PROBE";
    const NAME: &str = "command_substitution_does_not_inherit_locked_stdout";

    if std::env::var_os(PROBE).is_some() {
        let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let stdout = std::io::stdout();
            let _stdout = stdout.lock();
            locked_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
        locked_rx.recv().unwrap();

        let timeout = std::time::Duration::from_secs(1);
        let mut shell = Shell::builder().timeout(timeout).build();
        let result = fields("\"$(printf value)\"", &mut shell);
        holder.join().unwrap();
        assert_eq!(result, ["value"]);

        use std::io::Write as _;
        let stdout = std::io::stdout();
        stdout.lock().write_all(b"pending-parent-output").unwrap();
        let mut shell = Shell::builder().timeout(timeout).build();
        assert_eq!(fields("\"$(printf value)\"", &mut shell), ["value"]);

        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("callback-ran");
        let callback_marker = marker.clone();
        let handler: epsh::eval::ExternalHandler = Box::new(move |_, _| {
            std::fs::write(&callback_marker, b"called")?;
            Ok(epsh::error::ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).timeout(timeout).build();
        let program = Parser::new("\"$(handler)\"").parse().unwrap();
        let epsh::ast::Command::Simple { args, .. } = &program.commands[0] else { panic!("expected simple command") };
        assert!(matches!(expand_word_to_fields(&args[0], &mut shell), Err(epsh::error::ShellError::CallbackProcess { kind: epsh::eval::CallbackKind::External })));
        assert!(!marker.exists());
        return;
    }

    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture"])
        .env(PROBE, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .unwrap();
    let process_group = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL).unwrap();
            let status = child.wait().unwrap();
            panic!("stdout lock probe timed out: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.success(), "stdout lock probe failed: {status}");
}

#[test]
fn quoted_literal_directory_component_allows_following_glob() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["*dir", "?dir", "[dir]", "\\dir"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
        std::fs::write(dir.path().join(name).join("file"), []).unwrap();
    }
    let mut shell = Shell::builder().cwd(dir.path().to_path_buf()).build();
    assert_eq!(fields("'*dir'/* '?dir'/* '[dir]'/* '\\dir'/*", &mut shell),
        ["*dir/file", "?dir/file", "[dir]/file", "\\dir/file"]);
}

#[test]
fn assignment_rhs_joins_positional_values_before_outer_field_splitting() {
    let mut shell = Shell::new();
    shell.set_args(&["ish", "one", "two three", "four"]);
    shell.set_var("IFS", "").unwrap();
    assert_eq!(fields("${EMPTY_IFS=$*}", &mut shell), ["onetwo threefour"]);
    assert_eq!(shell.get_var("EMPTY_IFS"), Some("onetwo threefour"));
    shell.set_var("IFS", ":").unwrap();
    assert_eq!(fields("${COLON_IFS=$*}", &mut shell), ["one", "two three", "four"]);
    assert_eq!(shell.get_var("COLON_IFS"), Some("one:two three:four"));
    assert_eq!(fields("\"${QUOTED=$*}\"", &mut shell), ["one:two three:four"]);
    assert_eq!(shell.get_var("QUOTED"), Some("one:two three:four"));
}
