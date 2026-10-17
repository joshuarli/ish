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
    assert_eq!(fields("${ASSIGNED:='*'}", &mut shell), ["*"]);
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
    let Command::Simple { args, .. } = &program.commands[0] else { panic!("expected simple command") };
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
    let Command::Simple { args, .. } = &program.commands[0] else { panic!("expected simple command") };
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
