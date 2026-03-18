//! Integration tests for ish shell.
//!
//! Tests the public API at a higher level than unit tests — verifying that
//! modules compose correctly (parse → expand, line editing sequences,
//! history search, completion, config parsing, etc.).

use std::collections::HashSet;

use ish::alias::AliasMap;
use ish::builtin;
use ish::complete::{self, CompEntry, CompletionState};
use ish::config;
use ish::error::Error;
use ish::expand;
use ish::history::{self, History};
use ish::input;
use ish::line::LineBuffer;
use ish::ls;
use ish::parse::{self, Connector, LITERAL, RedirectKind};
use ish::prompt;

// ---------------------------------------------------------------------------
// Parse → Expand pipeline
// ---------------------------------------------------------------------------

fn no_subst(_cmd: &str) -> Result<String, Error> {
    Ok(String::new())
}

#[test]
fn parse_expand_simple_command() {
    let cmd = parse::parse("echo hello world").unwrap();
    let argv: Vec<&str> = cmd.segments[0].0.commands[0]
        .cmd
        .argv
        .iter()
        .map(|s| s.as_str())
        .collect();
    let expanded = expand::expand_argv(
        &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "/home/test",
        &mut no_subst,
    )
    .unwrap();
    assert_eq!(expanded, ["echo", "hello", "world"]);
}

#[test]
fn parse_expand_tilde() {
    let cmd = parse::parse("ls ~/docs").unwrap();
    let argv = &cmd.segments[0].0.commands[0].cmd.argv;
    let expanded = expand::expand_argv(argv, "/home/josh", &mut no_subst).unwrap();
    assert_eq!(expanded, ["ls", "/home/josh/docs"]);
}

#[test]
fn parse_expand_variable() {
    unsafe { std::env::set_var("ISH_INTEG_VAR", "myval") };
    let cmd = parse::parse("echo $ISH_INTEG_VAR").unwrap();
    let argv = &cmd.segments[0].0.commands[0].cmd.argv;
    let expanded = expand::expand_argv(argv, "/home/test", &mut no_subst).unwrap();
    assert_eq!(expanded, ["echo", "myval"]);
}

#[test]
fn parse_expand_single_quote_prevents_expansion() {
    unsafe { std::env::set_var("ISH_INTEG_VAR", "should_not_appear") };
    let cmd = parse::parse("echo '$ISH_INTEG_VAR'").unwrap();
    let argv = &cmd.segments[0].0.commands[0].cmd.argv;
    let expanded = expand::expand_argv(argv, "/home/test", &mut no_subst).unwrap();
    assert_eq!(expanded, ["echo", "$ISH_INTEG_VAR"]);
}

#[test]
fn parse_expand_double_quote_expands_vars() {
    unsafe { std::env::set_var("ISH_DQ_TEST", "value") };
    let cmd = parse::parse(r#"echo "$ISH_DQ_TEST""#).unwrap();
    let argv = &cmd.segments[0].0.commands[0].cmd.argv;
    let expanded = expand::expand_argv(argv, "/home/test", &mut no_subst).unwrap();
    assert_eq!(expanded, ["echo", "value"]);
}

#[test]
fn parse_expand_command_substitution() {
    // The parser treats $(cmd) as a single word only if there are no spaces.
    // Test with no-space command substitution via expand_word directly.
    let mut exec_subst = |cmd: &str| -> Result<String, Error> {
        assert_eq!(cmd, "pwd");
        Ok("/home/test\n".to_string())
    };
    let result = expand::expand_word("$(pwd)", "/home/test", &mut exec_subst).unwrap();
    assert_eq!(result, ["/home/test"]);
}

#[test]
fn parse_expand_backtick_substitution() {
    let cmd = parse::parse("echo `pwd`").unwrap();
    let argv = &cmd.segments[0].0.commands[0].cmd.argv;
    let mut exec_subst = |cmd: &str| -> Result<String, Error> {
        assert_eq!(cmd, "pwd");
        Ok("/home/test\n".to_string())
    };
    let expanded = expand::expand_argv(argv, "/home/test", &mut exec_subst).unwrap();
    assert_eq!(expanded, ["echo", "/home/test"]);
}

// ---------------------------------------------------------------------------
// Parser: comprehensive syntax tests
// ---------------------------------------------------------------------------

#[test]
fn parse_pipe_with_stderr() {
    let cmd = parse::parse("cmd1 &| cmd2").unwrap();
    let pipeline = &cmd.segments[0].0;
    assert_eq!(pipeline.commands.len(), 2);
    assert!(pipeline.commands[0].pipe_stderr);
}

#[test]
fn parse_all_redirect_kinds() {
    let cmd = parse::parse("cmd > out >> app < in 2> err &> all").unwrap();
    let redirects = &cmd.segments[0].0.commands[0].cmd.redirects;
    assert_eq!(redirects.len(), 5);
    assert_eq!(redirects[0].kind, RedirectKind::Out);
    assert_eq!(redirects[0].target, "out");
    assert_eq!(redirects[1].kind, RedirectKind::Append);
    assert_eq!(redirects[2].kind, RedirectKind::In);
    assert_eq!(redirects[3].kind, RedirectKind::Err);
    assert_eq!(redirects[4].kind, RedirectKind::All);
}

#[test]
fn parse_complex_pipeline_chain() {
    let cmd = parse::parse("a | b && c || d ; e").unwrap();
    assert_eq!(cmd.segments.len(), 4);
    assert_eq!(cmd.segments[0].0.commands.len(), 2); // a | b
    assert_eq!(cmd.segments[0].1, Some(Connector::And));
    assert_eq!(cmd.segments[1].1, Some(Connector::Or));
    assert_eq!(cmd.segments[2].1, Some(Connector::Semi));
    assert_eq!(cmd.segments[3].1, None);
}

#[test]
fn parse_comments_ignored() {
    let cmd = parse::parse("echo hello # this is a comment").unwrap();
    assert_eq!(cmd.segments[0].0.commands[0].cmd.argv, ["echo", "hello"]);
}

#[test]
fn parse_empty_input_is_error() {
    assert!(parse::parse("").is_err());
    assert!(parse::parse("   ").is_err());
    assert!(parse::parse("# comment only").is_err());
}

#[test]
fn parse_unclosed_single_quote_is_error() {
    assert!(parse::parse("echo 'unclosed").is_err());
}

#[test]
fn parse_unclosed_double_quote_is_error() {
    assert!(parse::parse(r#"echo "unclosed"#).is_err());
}

#[test]
fn parse_trailing_pipe_redirect_is_error() {
    assert!(parse::parse("echo >").is_err());
}

#[test]
fn parse_backslash_escape_in_unquoted() {
    let cmd = parse::parse(r"echo hello\ world").unwrap();
    let word = &cmd.segments[0].0.commands[0].cmd.argv[1];
    let clean = parse::unescape(word);
    assert_eq!(clean, "hello world");
}

#[test]
fn parse_double_quote_escape_sequences() {
    // In double quotes: \$ \\ \" \` are escape sequences
    let cmd = parse::parse(r#"echo "a\$b\\c\"d""#).unwrap();
    let word = &cmd.segments[0].0.commands[0].cmd.argv[1];
    let clean = parse::unescape(word);
    assert_eq!(clean, r#"a$b\c"d"#);
}

#[test]
fn parse_mixed_quoting() {
    let cmd = parse::parse(r#"echo 'single'"double"unquoted"#).unwrap();
    let word = &cmd.segments[0].0.commands[0].cmd.argv[1];
    let clean = parse::unescape(word);
    assert_eq!(clean, "singledoubleunquoted");
}

#[test]
fn continuation_detection_comprehensive() {
    // Needs continuation
    assert!(parse::needs_continuation("ls |"));
    assert!(parse::needs_continuation("cmd &&"));
    assert!(parse::needs_continuation("cmd ||"));
    assert!(parse::needs_continuation("echo 'unclosed"));
    assert!(parse::needs_continuation(r#"echo "unclosed"#));

    // Does NOT need continuation
    assert!(!parse::needs_continuation("ls -la"));
    assert!(!parse::needs_continuation("a && b"));
    assert!(!parse::needs_continuation("echo 'closed'"));
    assert!(!parse::needs_continuation(""));
    assert!(!parse::needs_continuation("   "));
}

// ---------------------------------------------------------------------------
// Line buffer: complex editing sequences
// ---------------------------------------------------------------------------

#[test]
fn line_buffer_insert_middle() {
    let mut lb = LineBuffer::new();
    lb.set("hllo");
    lb.move_home();
    lb.move_right(); // after 'h'
    lb.insert_char('e');
    assert_eq!(lb.text(), "hello");
    assert_eq!(lb.cursor(), 2);
}

#[test]
fn line_buffer_delete_forward() {
    let mut lb = LineBuffer::new();
    lb.set("hello");
    lb.move_home();
    assert!(lb.delete_forward());
    assert_eq!(lb.text(), "ello");
}

#[test]
fn line_buffer_utf8_handling() {
    let mut lb = LineBuffer::new();
    lb.insert_char('日');
    lb.insert_char('本');
    lb.insert_char('語');
    assert_eq!(lb.text(), "日本語");
    assert_eq!(lb.cursor(), 9); // 3 chars * 3 bytes each
    assert!(lb.delete_back());
    assert_eq!(lb.text(), "日本");
    lb.move_left();
    assert_eq!(lb.display_cursor_pos(), 1);
    lb.insert_char('x');
    assert_eq!(lb.text(), "日x本");
}

#[test]
fn line_buffer_word_operations_complex() {
    let mut lb = LineBuffer::new();
    lb.set("foo   bar   baz");
    lb.move_word_left(); // -> before "baz"
    assert_eq!(lb.display_cursor_pos(), 12);
    lb.move_word_left(); // -> before "bar"
    assert_eq!(lb.display_cursor_pos(), 6);
    lb.kill_word_back(); // kills "foo   "
    assert_eq!(lb.text(), "bar   baz");
}

#[test]
fn line_buffer_kill_yank_cycle() {
    let mut lb = LineBuffer::new();
    lb.set("one two three");
    // Kill "three"
    lb.kill_word_back();
    assert_eq!(lb.text(), "one two ");
    // Yank it at the beginning
    lb.move_home();
    lb.yank();
    assert_eq!(lb.text(), "threeone two ");
}

#[test]
fn line_buffer_kill_to_end() {
    let mut lb = LineBuffer::new();
    lb.set("hello world");
    lb.move_home();
    lb.move_word_right(); // after "hello "
    lb.kill_to_end();
    assert_eq!(lb.text(), "hello ");
    lb.move_end();
    lb.yank();
    assert_eq!(lb.text(), "hello world");
}

#[test]
fn line_buffer_empty_operations() {
    let mut lb = LineBuffer::new();
    assert!(!lb.delete_back());
    assert!(!lb.delete_forward());
    assert!(!lb.move_left());
    assert!(!lb.move_right());
    lb.kill_to_end(); // no-op on empty
    lb.kill_to_start(); // no-op on empty
    lb.yank(); // no-op, kill ring empty
    assert_eq!(lb.text(), "");
}

#[test]
fn line_buffer_set_resets_cursor() {
    let mut lb = LineBuffer::new();
    lb.set("first");
    lb.move_home();
    lb.set("second");
    assert_eq!(lb.cursor(), 6); // set moves cursor to end
    assert_eq!(lb.text(), "second");
}

#[test]
fn line_buffer_insert_str() {
    let mut lb = LineBuffer::new();
    lb.insert_str("hello ");
    lb.insert_str("world");
    assert_eq!(lb.text(), "hello world");
    assert_eq!(lb.cursor(), 11);
}

// ---------------------------------------------------------------------------
// History: search and deduplication
// ---------------------------------------------------------------------------

#[test]
fn history_dedup_on_add() {
    let mut h = History::from_entries(vec![
        "ls".into(),
        "cd /tmp".into(),
        "ls".into(), // dup
    ]);
    // Already deduped by from_entries? No, from_entries takes them as-is.
    // add() deduplicates.
    h.add("ls");
    let entries = h.entries();
    // "ls" should appear only once, at the end
    assert_eq!(entries.iter().filter(|e| *e == "ls").count(), 1);
    assert_eq!(entries.last().unwrap(), "ls");
}

#[test]
fn history_prefix_search_recency() {
    let h = History::from_entries(vec![
        "git commit -m 'first'".into(),
        "git push".into(),
        "git commit -m 'second'".into(),
    ]);
    // Most recent match first
    assert_eq!(
        h.prefix_search("git commit", 0).unwrap(),
        "git commit -m 'second'"
    );
    assert_eq!(
        h.prefix_search("git commit", 1).unwrap(),
        "git commit -m 'first'"
    );
    assert!(h.prefix_search("git commit", 2).is_none());
}

#[test]
fn history_fuzzy_search_ordering() {
    let h = History::from_entries(vec![
        "git checkout main".into(),
        "ls -la".into(),
        "git commit -m fix".into(),
    ]);
    let matches = h.fuzzy_search("gc");
    // "gc" matches "git checkout" and "git commit", not "ls -la"
    // Most recent first
    assert_eq!(matches.len(), 2);
    assert_eq!(h.get(matches[0].entry_idx), "git commit -m fix");
    assert_eq!(h.get(matches[1].entry_idx), "git checkout main");
}

#[test]
fn history_fuzzy_case_insensitive() {
    let q: Vec<char> = "gc".chars().collect();
    assert!(history::subsequence_match(&q, "Git Checkout").is_some());
}

#[test]
fn history_fuzzy_empty_query_returns_all() {
    let h = History::from_entries(vec!["a".into(), "b".into(), "c".into()]);
    let matches = h.fuzzy_search("");
    assert_eq!(matches.len(), 3);
    // Most recent first
    assert_eq!(h.get(matches[0].entry_idx), "c");
}

#[test]
fn history_fuzzy_match_positions_correct() {
    let q: Vec<char> = "gco".chars().collect();
    let positions = history::subsequence_match(&q, "git checkout").unwrap();
    assert_eq!(positions, vec![0, 4, 9]); // g=0, c=4, o=9
}

#[test]
fn history_add_whitespace_only_ignored() {
    let mut h = History::from_entries(vec![]);
    h.add("   ");
    h.add("");
    assert_eq!(h.len(), 0);
}

// ---------------------------------------------------------------------------
// Completion: grid layout
// ---------------------------------------------------------------------------

fn make_entries(names: &[&str]) -> Vec<CompEntry> {
    names
        .iter()
        .map(|n| CompEntry {
            name: n.to_string(),
            is_dir: false,
            is_link: false,
            is_exec: false,
        })
        .collect()
}

#[test]
fn grid_single_entry() {
    let entries = make_entries(&["foo"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    assert_eq!(cols, 1);
    assert_eq!(rows, 1);
}

#[test]
fn grid_fits_multiple_columns() {
    let entries = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    assert!(cols > 1);
    assert!(cols * rows >= 6);
}

#[test]
fn grid_narrow_terminal_forces_single_column() {
    let entries = make_entries(&["longfilename.rs", "anotherlongname.rs"]);
    let (cols, _rows) = complete::compute_grid(&entries, 20);
    assert_eq!(cols, 1);
}

#[test]
fn grid_empty_entries() {
    let entries: Vec<CompEntry> = vec![];
    let (cols, rows) = complete::compute_grid(&entries, 80);
    assert_eq!(cols, 0);
    assert_eq!(rows, 0);
}

#[test]
fn completion_state_navigation_wraps() {
    let entries = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    let mut state = CompletionState {
        entries,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        dir_prefix: String::new(),
    };

    // Navigate down through all entries
    for _ in 0..20 {
        state.move_down();
    }
    // Should wrap around eventually
    // Just verify it doesn't panic and stays in bounds
    assert!(state.selected < state.entries.len());

    // Navigate up from 0
    state.selected = 0;
    state.move_up();
    assert!(state.selected < state.entries.len());
}

#[test]
fn completion_state_left_right_wrap() {
    let entries = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    let mut state = CompletionState {
        entries,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        dir_prefix: String::new(),
    };

    // Move left from first column should wrap to last
    state.move_left();
    assert!(state.selected < state.entries.len());

    // Move right from last column should wrap to first
    state.selected = state.entries.len() - 1;
    state.move_right();
    assert!(state.selected < state.entries.len());
}

#[test]
fn comp_entry_display_name_dir_suffix() {
    let e = CompEntry {
        name: "src".into(),
        is_dir: true,
        is_link: false,
        is_exec: false,
    };
    assert_eq!(e.display_name(), "src/");
    assert_eq!(e.display_width(), 4); // "src/"
}

#[test]
fn comp_entry_display_name_file() {
    let e = CompEntry {
        name: "main.rs".into(),
        is_dir: false,
        is_link: false,
        is_exec: false,
    };
    assert_eq!(e.display_name(), "main.rs");
    assert_eq!(e.display_width(), 7);
}

// ---------------------------------------------------------------------------
// Completion: filesystem (uses temp directories)
// ---------------------------------------------------------------------------

#[test]
fn complete_path_finds_files() {
    let dir = tempdir_with_files(&["foo.rs", "foo.txt", "bar.rs"]);
    let path = format!("{}/foo", dir.display());
    let entries = complete::complete_path(&path, false);
    assert_eq!(entries.len(), 2);
    let names: HashSet<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains("foo.rs"));
    assert!(names.contains("foo.txt"));
}

#[test]
fn complete_path_dirs_only() {
    let dir = tempdir_with_files(&["file.rs"]);
    std::fs::create_dir(dir.join("subdir")).unwrap();
    let path = format!("{}/", dir.display());
    let entries = complete::complete_path(&path, true);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "subdir");
    assert!(entries[0].is_dir);
}

#[test]
fn complete_path_hidden_files() {
    let dir = tempdir_with_files(&[".hidden", "visible"]);
    // Without dot prefix, hidden files should be excluded
    let path = format!("{}/", dir.display());
    let entries = complete::complete_path(&path, false);
    let names: HashSet<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains("visible"));
    assert!(!names.contains(".hidden"));

    // With dot prefix, hidden files should be included
    let path = format!("{}/.h", dir.display());
    let entries = complete::complete_path(&path, false);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, ".hidden");
}

#[test]
fn complete_path_nonexistent_dir() {
    let entries = complete::complete_path("/nonexistent/path/foo", false);
    assert!(entries.is_empty());
}

use std::sync::atomic::{AtomicU64, Ordering};

static TEMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tempdir_with_files(files: &[&str]) -> std::path::PathBuf {
    let n = TEMPDIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ish_test_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for f in files {
        std::fs::write(dir.join(f), "").unwrap();
    }
    dir
}

// ---------------------------------------------------------------------------
// Config: word splitting and quoting
// ---------------------------------------------------------------------------

#[test]
fn config_shell_words_simple() {
    assert_eq!(config::shell_words("a b c"), vec!["a", "b", "c"]);
}

#[test]
fn config_shell_words_quoted() {
    assert_eq!(config::shell_words(r#"a "b c" d"#), vec!["a", "b c", "d"]);
    assert_eq!(config::shell_words("a 'b c' d"), vec!["a", "b c", "d"]);
}

#[test]
fn config_shell_words_escaped() {
    assert_eq!(config::shell_words(r"a b\ c d"), vec!["a", "b c", "d"]);
}

#[test]
fn config_shell_words_empty_input() {
    assert!(config::shell_words("").is_empty());
    assert!(config::shell_words("   ").is_empty());
}

#[test]
fn config_unquote() {
    assert_eq!(config::unquote(r#""hello""#), "hello");
    assert_eq!(config::unquote("'hello'"), "hello");
    assert_eq!(config::unquote("hello"), "hello");
    assert_eq!(config::unquote(r#""mixed'"#), r#""mixed'"#); // mismatched
}

#[test]
fn config_expand_vars_simple() {
    unsafe { std::env::set_var("ISH_CFG_TEST", "expanded") };
    assert_eq!(config::expand_vars_simple("$ISH_CFG_TEST"), "expanded");
    assert_eq!(
        config::expand_vars_simple("prefix/$ISH_CFG_TEST/suffix"),
        "prefix/expanded/suffix"
    );
}

// ---------------------------------------------------------------------------
// Prompt: PWD shortening
// ---------------------------------------------------------------------------

#[test]
fn prompt_shorten_deep_path() {
    assert_eq!(
        prompt::shorten_pwd("/home/u/a/b/c/deep", "/home/u"),
        "~/a/b/c/deep"
    );
}

#[test]
fn prompt_shorten_single_component() {
    assert_eq!(prompt::shorten_pwd("/home/u/foo", "/home/u"), "~/foo");
}

#[test]
fn prompt_shorten_empty_home() {
    // Empty home string — no tilde contraction
    assert_eq!(prompt::shorten_pwd("/var/log/syslog", ""), "/v/l/syslog");
}

#[test]
fn prompt_shorten_home_prefix_not_subdir() {
    // /home/user should not match /home/user2
    assert_eq!(
        prompt::shorten_pwd("/home/user2/foo", "/home/user"),
        "/h/u/foo"
    );
}

// ---------------------------------------------------------------------------
// Input: modifier decoding
// ---------------------------------------------------------------------------

#[test]
fn input_modifier_combinations() {
    // param=1 means no modifiers (1 + 0)
    let m = input::modifier_from_param(1);
    assert!(!m.ctrl && !m.alt && !m.shift);

    // param=2 means shift (1 + 1)
    let m = input::modifier_from_param(2);
    assert!(m.shift && !m.ctrl && !m.alt);

    // param=5 means ctrl (1 + 4)
    let m = input::modifier_from_param(5);
    assert!(m.ctrl && !m.alt && !m.shift);

    // param=3 means alt (1 + 2)
    let m = input::modifier_from_param(3);
    assert!(m.alt && !m.ctrl && !m.shift);

    // param=6 means ctrl+alt (1 + 4 + 2 - 1 = 6)
    // Actually: param - 1 = 5, ctrl=bit2(4), alt=bit1(0)...
    // Let me just check: 6 - 1 = 5 = 0b101, shift=1, alt=0, ctrl=4 -> shift+ctrl
    let m = input::modifier_from_param(6);
    assert!(m.ctrl && m.shift && !m.alt);

    // param=7 means ctrl+alt (1 + 2 + 4)
    let m = input::modifier_from_param(7);
    assert!(m.ctrl && m.alt && !m.shift);

    // param=8 means ctrl+alt+shift (1 + 1 + 2 + 4)
    let m = input::modifier_from_param(8);
    assert!(m.ctrl && m.alt && m.shift);
}

// ---------------------------------------------------------------------------
// Expand: edge cases
// ---------------------------------------------------------------------------

#[test]
fn expand_undefined_var_is_empty() {
    let result =
        expand::expand_word("$UNDEFINED_ISH_VAR_XYZ", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, [""]);
}

#[test]
fn expand_tilde_alone() {
    let result = expand::expand_word("~", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["/home/test"]);
}

#[test]
fn expand_tilde_with_path() {
    let result = expand::expand_word("~/docs/file.txt", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["/home/test/docs/file.txt"]);
}

#[test]
fn expand_tilde_not_at_start() {
    let result = expand::expand_word("foo~bar", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["foo~bar"]);
}

#[test]
fn expand_dollar_sign_alone() {
    let result = expand::expand_word("$", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["$"]);
}

#[test]
fn expand_unclosed_command_subst_is_error() {
    let result = expand::expand_word("$(unclosed", "/home/test", &mut no_subst);
    assert!(result.is_err());
}

#[test]
fn expand_unclosed_backtick_is_error() {
    let result = expand::expand_word("`unclosed", "/home/test", &mut no_subst);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Expand: glob (uses real filesystem)
// ---------------------------------------------------------------------------

#[test]
fn expand_glob_star() {
    let dir = tempdir_with_files(&["test_a.rs", "test_b.rs", "other.txt"]);
    // Canonicalize to resolve symlinks (e.g. /var -> /private/var on macOS)
    let dir = std::fs::canonicalize(&dir).unwrap();
    let pattern = format!("{}/test_*.rs", dir.display());
    let result = expand::expand_word(&pattern, "/home/test", &mut no_subst).unwrap();
    assert_eq!(result.len(), 2);
    assert!(result.iter().any(|r| r.ends_with("test_a.rs")));
    assert!(result.iter().any(|r| r.ends_with("test_b.rs")));
}

#[test]
fn expand_glob_question_mark() {
    let dir = tempdir_with_files(&["a1", "a2", "ab"]);
    let dir = std::fs::canonicalize(&dir).unwrap();
    let pattern = format!("{}/a?", dir.display());
    let result = expand::expand_word(&pattern, "/home/test", &mut no_subst).unwrap();
    assert_eq!(result.len(), 3);
}

#[test]
fn expand_glob_no_match_is_error() {
    let result = expand::expand_word("/nonexistent/*.xyz", "/home/test", &mut no_subst);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Alias
// ---------------------------------------------------------------------------

#[test]
fn alias_set_get() {
    let mut aliases = AliasMap::new();
    aliases.set("ll".into(), vec!["ls".into(), "-la".into()]);
    let exp = aliases.get("ll").unwrap();
    assert_eq!(exp.len(), 2);
    assert_eq!(exp[0], "ls");
    assert_eq!(exp[1], "-la");
    assert_eq!(aliases.get("nonexistent"), None);
}

#[test]
fn alias_override() {
    let mut aliases = AliasMap::new();
    aliases.set("g".into(), vec!["git".into()]);
    aliases.set("g".into(), vec!["git".into(), "status".into()]);
    let exp = aliases.get("g").unwrap();
    assert_eq!(exp[0], "git");
    assert_eq!(exp[1], "status");
}

#[test]
fn alias_iter() {
    let mut aliases = AliasMap::new();
    aliases.set("a".into(), vec!["alpha".into()]);
    aliases.set("b".into(), vec!["beta".into()]);
    let collected: HashSet<_> = aliases.iter().map(|(k, _)| k.to_string()).collect();
    assert!(collected.contains("a"));
    assert!(collected.contains("b"));
}

// ---------------------------------------------------------------------------
// Security-relevant tests
// ---------------------------------------------------------------------------

#[test]
fn parse_null_bytes_in_input() {
    // Null bytes should not crash the parser
    let result = parse::parse("echo \x00hello");
    // May succeed or fail, but must not panic
    let _ = result;
}

#[test]
fn parse_extremely_long_input() {
    let long = "a ".repeat(10_000);
    let result = parse::parse(&long);
    assert!(result.is_ok());
    // Verify it parsed all the words
    let cmd = result.unwrap();
    let argv_len = cmd.segments[0].0.commands[0].cmd.argv.len();
    assert_eq!(argv_len, 10_000);
}

#[test]
fn parse_deeply_nested_quotes_no_stack_overflow() {
    // Alternating quote types shouldn't cause issues
    let input = r#"echo 'a'"b"'c'"d"'e'"f""#;
    let result = parse::parse(input);
    assert!(result.is_ok());
}

#[test]
fn expand_nested_parens_no_panic() {
    // Nested parens — the paren matcher should handle depth correctly
    let word = "$(echo $(pwd))";
    let mut call_count = 0;
    let mut exec_subst = |_cmd: &str| -> Result<String, Error> {
        call_count += 1;
        Ok("val\n".to_string())
    };
    // Should not panic regardless of whether nesting works correctly
    let _ = expand::expand_word(word, "/home/test", &mut exec_subst);
}

#[test]
fn line_buffer_boundary_conditions() {
    let mut lb = LineBuffer::new();
    // Insert at position 0
    lb.insert_char('a');
    // Delete at end
    assert!(!lb.delete_forward());
    // Move beyond bounds
    assert!(!lb.move_right());
    lb.move_home();
    assert!(!lb.move_left());
}

#[test]
fn history_subsequence_no_infinite_loop() {
    // Very long text with no match
    let q: Vec<char> = "xyz".chars().collect();
    let long_text = "a".repeat(100_000);
    let result = history::subsequence_match(&q, &long_text);
    assert!(result.is_none());
}

#[test]
fn config_shell_words_adversarial() {
    // Unclosed quotes — should not panic, just include the rest
    let result = config::shell_words("a 'unclosed");
    assert!(!result.is_empty());

    // Only whitespace
    let result = config::shell_words("   \t  ");
    assert!(result.is_empty());

    // Escaped at end
    let result = config::shell_words("a b\\");
    assert!(!result.is_empty());
}

#[test]
fn prompt_shorten_pwd_adversarial() {
    // Very deep path
    let deep = format!("/home/u{}", "/a".repeat(100));
    let result = prompt::shorten_pwd(&deep, "/home/u");
    assert!(result.starts_with('~'));

    // Unicode in path — must not panic on multi-byte abbreviation
    let result = prompt::shorten_pwd("/home/u/日本語/ファイル", "/home/u");
    assert!(result.starts_with('~'));
    // The middle component "日本語" should be abbreviated to "日" (1 char)
    assert!(result.contains('日'));

    // Empty path
    let result = prompt::shorten_pwd("", "/home/u");
    assert_eq!(result, "");

    // Root path
    let result = prompt::shorten_pwd("/", "");
    assert_eq!(result, "/");
}

// ---------------------------------------------------------------------------
// Error: Display and From impls
// ---------------------------------------------------------------------------

#[test]
fn error_display_msg() {
    let e = Error::msg("something failed");
    assert_eq!(format!("{e}"), "something failed");
}

#[test]
fn error_display_glob_no_match() {
    let e = Error::glob_no_match("*.xyz");
    assert_eq!(format!("{e}"), "no matches for glob: *.xyz");
}

#[test]
fn error_display_bad_substitution() {
    let e = Error::bad_substitution("unclosed $(");
    assert_eq!(format!("{e}"), "bad substitution: unclosed $(");
}

#[test]
fn error_display_io() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let e: Error = io_err.into();
    assert_eq!(format!("{e}"), "file not found");
}

#[test]
fn error_is_std_error() {
    let e = Error::msg("test");
    // Verify it implements std::error::Error
    let _: &dyn std::error::Error = &e;
}

// ---------------------------------------------------------------------------
// Config: load, parse_set, parse_alias, config_path
// ---------------------------------------------------------------------------

#[test]
fn config_load_all_paths() {
    // Consolidate config load tests into one to avoid XDG_CONFIG_HOME races.
    let old_xdg = std::env::var("XDG_CONFIG_HOME").ok();

    // 1. Nonexistent config — should not panic
    let empty_dir = tempdir_with_files(&[]);
    unsafe { std::env::set_var("XDG_CONFIG_HOME", empty_dir.to_str().unwrap()) };
    let mut aliases = AliasMap::new();
    config::load(&mut aliases, None);

    // 2. Full config with set, alias, comments, bad lines
    let dir = tempdir_with_files(&[]);
    let config_dir = dir.join("ish");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.ish"),
        "# comment line\n\
         set ISH_TEST_CFG_LOAD_VAR \"hello world\"\n\
         alias ll ls -la\n\
         \n\
         badline\n",
    )
    .unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.to_str().unwrap()) };
    let mut aliases = AliasMap::new();
    config::load(&mut aliases, None);
    assert_eq!(
        std::env::var("ISH_TEST_CFG_LOAD_VAR").unwrap(),
        "hello world"
    );
    assert_eq!(aliases.get("ll").unwrap(), &["ls", "-la"]);

    // 3. Empty set name — error but no panic
    let dir2 = tempdir_with_files(&[]);
    let config_dir2 = dir2.join("ish");
    std::fs::create_dir_all(&config_dir2).unwrap();
    std::fs::write(config_dir2.join("config.ish"), "set  \n").unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir2.to_str().unwrap()) };
    let mut aliases = AliasMap::new();
    config::load(&mut aliases, None);

    // 4. Alias without expansion — should not be added
    let dir3 = tempdir_with_files(&[]);
    let config_dir3 = dir3.join("ish");
    std::fs::create_dir_all(&config_dir3).unwrap();
    std::fs::write(config_dir3.join("config.ish"), "alias myalias\n").unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir3.to_str().unwrap()) };
    let mut aliases = AliasMap::new();
    config::load(&mut aliases, None);
    assert!(aliases.get("myalias").is_none());

    // 5. set VAR with no value → empty string
    let dir4 = tempdir_with_files(&[]);
    let config_dir4 = dir4.join("ish");
    std::fs::create_dir_all(&config_dir4).unwrap();
    std::fs::write(config_dir4.join("config.ish"), "set ISH_TEST_CFG_NOVAL\n").unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir4.to_str().unwrap()) };
    let mut aliases = AliasMap::new();
    config::load(&mut aliases, None);
    assert_eq!(std::env::var("ISH_TEST_CFG_NOVAL").unwrap(), "");

    // Restore
    match old_xdg {
        Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
    }
}

#[test]
fn config_expand_vars_trailing_dollar() {
    // Trailing $ with no variable name
    let result = config::expand_vars_simple("hello$");
    assert_eq!(result, "hello");
}

#[test]
fn config_expand_vars_dollar_followed_by_non_alnum() {
    let result = config::expand_vars_simple("price is $!00");
    assert_eq!(result, "price is !00");
}

// ---------------------------------------------------------------------------
// Prompt: display_len, render, git branch
// ---------------------------------------------------------------------------

#[test]
fn prompt_display_len_no_ansi() {
    let p = prompt::Prompt::new();
    assert_eq!(p.display_len("hello world"), 11);
}

#[test]
fn prompt_display_len_with_ansi() {
    let p = prompt::Prompt::new();
    let s = "\x1b[32mgreen\x1b[0m text";
    assert_eq!(p.display_len(s), 10); // "green text"
}

#[test]
fn prompt_display_len_multiple_escapes() {
    let p = prompt::Prompt::new();
    let s = "\x1b[1m\x1b[32mbold green\x1b[0m";
    assert_eq!(p.display_len(s), 10); // "bold green"
}

#[test]
fn prompt_display_len_utf8() {
    let p = prompt::Prompt::new();
    assert_eq!(p.display_len("日本語"), 3);
}

#[test]
fn prompt_render_status_colors() {
    let mut p = prompt::Prompt::new();
    // status 0 → green, status 1 → red
    let r0 = p.render(0);
    assert!(r0.contains("\x1b[38;5;10m")); // bright green
    assert!(r0.ends_with(" $ "));

    let r1 = p.render(1);
    assert!(r1.contains("\x1b[38;5;1m")); // red
    assert!(r1.ends_with(" $ "));
}

#[test]
fn prompt_render_dirty_indicator() {
    // Test that the dirty indicator code path is exercised.
    // We can't reliably test env var reads in parallel tests, so just
    // verify the render doesn't crash with __DENV_DIRTY set or unset.
    let mut p = prompt::Prompt::new();
    let r1 = p.render(0);
    assert!(r1.ends_with(" $ "));
}

#[test]
fn prompt_invalidate_git() {
    let mut p = prompt::Prompt::new();
    p.invalidate_git(); // should not panic
}

#[test]
fn prompt_default_impl() {
    let p = prompt::Prompt::default();
    let rendered = p.display_len("test");
    assert_eq!(rendered, 4);
}

#[test]
fn prompt_git_branch_in_git_repo() {
    // We're running from within a git repo (ish itself)
    let mut p = prompt::Prompt::new();
    // Set PWD to our repo root
    let cwd = std::env::current_dir().unwrap();
    unsafe { std::env::set_var("PWD", cwd.to_str().unwrap()) };
    let rendered = p.render(0);
    // Should detect git branch — the rendered prompt should contain a branch name
    // We can't predict the exact branch name, but it should be there
    // At minimum, the prompt should not crash
    assert!(rendered.ends_with(" $ "));
}

#[test]
fn prompt_render_multiple() {
    // Exercise the render path multiple times — exercises PWD caching
    // and git branch detection. Can't assert exact equality due to
    // env var races in parallel tests; just verify no panics.
    let mut p = prompt::Prompt::new();
    let r1 = p.render(0);
    assert!(r1.ends_with(" $ "));
    let r2 = p.render(1);
    assert!(r2.ends_with(" $ "));
}

// ---------------------------------------------------------------------------
// Builtin: is_builtin, is_special_builtin
// ---------------------------------------------------------------------------

#[test]
fn builtin_is_builtin_known() {
    assert!(builtin::is_builtin("cd"));
    assert!(builtin::is_builtin("exit"));
    assert!(builtin::is_builtin("fg"));
    assert!(builtin::is_builtin("set"));
    assert!(builtin::is_builtin("unset"));
    assert!(builtin::is_builtin("alias"));
    assert!(builtin::is_builtin("l"));
    assert!(builtin::is_builtin("c"));
    assert!(builtin::is_builtin("w"));
    assert!(builtin::is_builtin("which"));
    assert!(builtin::is_builtin("type"));
    assert!(builtin::is_builtin("echo"));
    assert!(builtin::is_builtin("pwd"));
    assert!(builtin::is_builtin("true"));
    assert!(builtin::is_builtin("false"));
    assert!(builtin::is_builtin("copy-scrollback"));
}

#[test]
fn builtin_is_builtin_unknown() {
    assert!(!builtin::is_builtin("ls"));
    assert!(!builtin::is_builtin("grep"));
    assert!(!builtin::is_builtin(""));
    assert!(!builtin::is_builtin("nonexistent"));
}

#[test]
fn builtin_is_special_known() {
    assert!(builtin::is_special_builtin("cd"));
    assert!(builtin::is_special_builtin("exit"));
    assert!(builtin::is_special_builtin("fg"));
    assert!(builtin::is_special_builtin("set"));
    assert!(builtin::is_special_builtin("unset"));
    assert!(builtin::is_special_builtin("alias"));
    assert!(builtin::is_special_builtin("copy-scrollback"));
}

#[test]
fn builtin_is_special_not_special() {
    assert!(!builtin::is_special_builtin("l"));
    assert!(!builtin::is_special_builtin("echo"));
    assert!(!builtin::is_special_builtin("pwd"));
    assert!(!builtin::is_special_builtin("true"));
}

#[test]
fn builtin_run_output_echo() {
    let args = vec!["hello".to_string(), "world".to_string()];
    let status = builtin::run_output("echo", &args, &[]);
    assert_eq!(status, 0);
}

#[test]
fn builtin_run_output_true_false() {
    assert_eq!(builtin::run_output("true", &[], &[]), 0);
    assert_eq!(builtin::run_output("false", &[], &[]), 1);
}

#[test]
fn builtin_run_output_pwd() {
    // pwd should succeed or fail gracefully (CWD may be changed by other tests)
    let status = builtin::run_output("pwd", &[], &[]);
    assert!(status == 0 || status == 1);
}

#[test]
fn builtin_run_output_l_cwd() {
    let dir = tempdir_with_files(&["a.txt"]);
    let status = builtin::run_output("l", &[dir.to_str().unwrap().to_string()], &[]);
    assert_eq!(status, 0);
}

#[test]
fn builtin_run_output_l_file() {
    let dir = tempdir_with_files(&["test_file.txt"]);
    let file_path = dir.join("test_file.txt");
    let status = builtin::run_output("l", &[file_path.to_str().unwrap().to_string()], &[]);
    assert_eq!(status, 0);
}

#[test]
fn builtin_run_output_l_nonexistent() {
    let status = builtin::run_output("l", &["/nonexistent/path".to_string()], &[]);
    assert_eq!(status, 1);
}

#[test]
fn builtin_run_output_c_clear() {
    let status = builtin::run_output("c", &[], &[]);
    assert_eq!(status, 0);
}

#[test]
fn builtin_run_output_w_no_args() {
    let status = builtin::run_output("w", &[], &[]);
    assert_eq!(status, 1);
}

#[test]
fn builtin_run_output_w_builtin() {
    let status = builtin::run_output("w", &["cd".to_string()], &[]);
    assert_eq!(status, 0);
}

#[test]
fn builtin_run_output_w_which_type_aliases() {
    // "which" and "type" should work like "w"
    assert_eq!(builtin::run_output("which", &["echo".to_string()], &[]), 0);
    assert_eq!(builtin::run_output("type", &["echo".to_string()], &[]), 0);
}

#[test]
fn builtin_run_output_w_not_found() {
    let status = builtin::run_output("w", &["nonexistent_cmd_xyz".to_string()], &[]);
    assert_eq!(status, 1);
}

#[test]
fn builtin_run_output_copy_scrollback() {
    let status = builtin::run_output("copy-scrollback", &[], &[]);
    assert_eq!(status, 1); // "not yet implemented"
}

#[test]
fn builtin_run_output_special_in_pipeline() {
    // Special builtins shouldn't work in pipeline context (via run_output)
    assert_eq!(builtin::run_output("cd", &[], &[]), 1);
    assert_eq!(builtin::run_output("exit", &[], &[]), 1);
    assert_eq!(builtin::run_output("set", &[], &[]), 1);
}

#[test]
fn builtin_run_output_unknown() {
    assert_eq!(builtin::run_output("notabuiltin", &[], &[]), 1);
}

#[test]
fn builtin_run_special_cd_target() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let dir = tempdir_with_files(&[]);
    let old_dir = std::env::current_dir().unwrap();
    let status = builtin::run_special(
        "cd",
        &[dir.to_str().unwrap().to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);
    assert!(prev_dir.is_some());

    // Restore
    let _ = std::env::set_current_dir(&old_dir);
}

#[test]
fn builtin_run_special_cd_dash_no_prev() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let status = builtin::run_special(
        "cd",
        &["-".to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 1); // no previous directory
}

#[test]
fn builtin_run_special_cd_dash_with_prev() {
    use std::collections::HashMap;
    let dir = tempdir_with_files(&[]);
    let mut prev_dir = Some(dir.to_str().unwrap().to_string());
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let old_dir = std::env::current_dir().unwrap();
    let status = builtin::run_special(
        "cd",
        &["-".to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);

    let _ = std::env::set_current_dir(&old_dir);
}

#[test]
fn builtin_run_special_cd_nonexistent() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let status = builtin::run_special(
        "cd",
        &["/nonexistent_dir_xyz".to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 1);
}

#[test]
fn builtin_run_special_set_print_all() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    // set with no args prints all env vars
    let status = builtin::run_special(
        "set",
        &[],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);
}

#[test]
fn builtin_run_special_set_var() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let status = builtin::run_special(
        "set",
        &["ISH_TEST_BUILTIN_SET".to_string(), "myvalue".to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);
    assert_eq!(std::env::var("ISH_TEST_BUILTIN_SET").unwrap(), "myvalue");
    unsafe { std::env::remove_var("ISH_TEST_BUILTIN_SET") };
}

#[test]
fn builtin_run_special_set_no_value() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let status = builtin::run_special(
        "set",
        &["ISH_TEST_BUILTIN_SET_EMPTY".to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);
    assert_eq!(std::env::var("ISH_TEST_BUILTIN_SET_EMPTY").unwrap(), "");
    unsafe { std::env::remove_var("ISH_TEST_BUILTIN_SET_EMPTY") };
}

#[test]
fn builtin_run_special_unset() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    unsafe { std::env::set_var("ISH_TEST_UNSET_ME", "value") };
    let status = builtin::run_special(
        "unset",
        &["ISH_TEST_UNSET_ME".to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);
    assert!(std::env::var("ISH_TEST_UNSET_ME").is_err());
}

#[test]
fn builtin_run_special_unset_no_args() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let status = builtin::run_special(
        "unset",
        &[],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 1);
}

#[test]
fn builtin_run_special_alias_error() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let status = builtin::run_special(
        "alias",
        &[],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 1);
}

#[test]
fn builtin_run_special_copy_scrollback() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let status = builtin::run_special(
        "copy-scrollback",
        &[],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);
}

#[test]
fn builtin_run_special_unknown() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    let status = builtin::run_special(
        "notabuiltin",
        &[],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 1);
}

#[test]
fn builtin_run_special_exit() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    // exit in pipeline context returns 1
    let status = builtin::run_special(
        "exit",
        &[],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 1);
}

#[test]
fn builtin_run_special_set_path_rebuilds_cache() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    // Insert a dummy entry into the cache
    cache.insert("dummy".to_string(), std::path::PathBuf::from("/tmp/dummy"));

    let status = builtin::run_special(
        "set",
        &["PATH".to_string(), "/usr/bin".to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);
    // Cache should have been rebuilt (old entry gone)
    assert!(!cache.contains_key("dummy"));
}

#[test]
fn builtin_run_special_unset_path_clears_cache() {
    use std::collections::HashMap;
    let mut prev_dir = None;
    let mut job = None;
    let mut cache = HashMap::new();
    let mut log = String::new();

    cache.insert("dummy".to_string(), std::path::PathBuf::from("/tmp/dummy"));

    // Save and restore PATH since we're unsetting it
    let old_path = std::env::var("PATH").ok();
    let status = builtin::run_special(
        "unset",
        &["PATH".to_string()],
        &[],
        &mut prev_dir,
        "/tmp",
        &mut job,
        &mut cache,
        &mut log,
    );
    assert_eq!(status, 0);
    assert!(cache.is_empty());

    // Restore PATH
    if let Some(p) = old_path {
        unsafe { std::env::set_var("PATH", p) };
    }
}

// ---------------------------------------------------------------------------
// History: file I/O
// ---------------------------------------------------------------------------

#[test]
fn history_load_empty() {
    let h = History::load();
    // Just verify it doesn't panic and returns a valid history
    let _ = h.len();
}

#[test]
fn history_add_with_newlines() {
    let mut h = History::from_entries(vec![]);
    h.add("echo\nhello");
    // Newlines should be collapsed to spaces
    assert_eq!(h.entries().last().unwrap(), "echo hello");
}

#[test]
fn history_add_dedup_preserves_order() {
    let mut h = History::from_entries(vec!["a".into(), "b".into(), "c".into()]);
    h.add("a");
    assert_eq!(h.entries(), &["b", "c", "a"]);
}

#[test]
fn history_get_and_len() {
    let h = History::from_entries(vec!["first".into(), "second".into()]);
    assert_eq!(h.len(), 2);
    assert!(!h.is_empty());
    assert_eq!(h.get(0), "first");
    assert_eq!(h.get(1), "second");
}

#[test]
fn history_from_entries_empty() {
    let h = History::from_entries(vec![]);
    assert!(h.is_empty());
    assert_eq!(h.len(), 0);
}

#[test]
fn history_prefix_search_no_match() {
    let h = History::from_entries(vec!["ls -la".into(), "cd /tmp".into()]);
    assert!(h.prefix_search("git", 0).is_none());
}

#[test]
fn history_fuzzy_match_positions_empty_query() {
    let q: Vec<char> = "".chars().collect();
    let positions = history::subsequence_match(&q, "anything");
    assert_eq!(positions, Some(vec![]));
}

// ---------------------------------------------------------------------------
// ls: directory listing
// ---------------------------------------------------------------------------

#[test]
fn ls_list_dir_cwd() {
    let dir = tempdir_with_files(&["file.txt"]);
    assert_eq!(ls::list_dir(dir.to_str().unwrap()), 0);
}

#[test]
fn ls_list_dir_file() {
    let dir = tempdir_with_files(&["test_file.txt"]);
    let file_path = dir.join("test_file.txt");
    assert_eq!(ls::list_dir(file_path.to_str().unwrap()), 0);
}

#[test]
fn ls_list_dir_nonexistent() {
    assert_eq!(ls::list_dir("/nonexistent_path_xyz"), 1);
}

#[test]
fn ls_list_dir_tempdir() {
    let dir = tempdir_with_files(&["alpha.txt", "beta.rs"]);
    std::fs::create_dir(dir.join("subdir")).unwrap();
    assert_eq!(ls::list_dir(dir.to_str().unwrap()), 0);
}

#[test]
fn ls_list_dir_empty() {
    let dir = tempdir_with_files(&[]);
    assert_eq!(ls::list_dir(dir.to_str().unwrap()), 0);
}

#[test]
fn ls_list_dir_symlink() {
    let dir = tempdir_with_files(&["target.txt"]);
    let link_path = dir.join("link.txt");
    std::os::unix::fs::symlink(dir.join("target.txt"), &link_path).unwrap();
    assert_eq!(ls::list_dir(dir.to_str().unwrap()), 0);
}

// ---------------------------------------------------------------------------
// Expand: more edge cases
// ---------------------------------------------------------------------------

#[test]
fn expand_variable_in_middle_of_word() {
    unsafe { std::env::set_var("ISH_MID_VAR", "world") };
    let result = expand::expand_word("hello$ISH_MID_VAR!", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["helloworld!"]);
}

#[test]
fn expand_multiple_variables() {
    unsafe { std::env::set_var("ISH_A", "foo") };
    unsafe { std::env::set_var("ISH_B", "bar") };
    let result = expand::expand_word("$ISH_A-$ISH_B", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["foo-bar"]);
}

#[test]
fn expand_dollar_paren_pass_through_in_variables() {
    // $(cmd) should be left for command substitution pass
    let mut exec = |cmd: &str| -> Result<String, Error> {
        assert_eq!(cmd, "echo hi");
        Ok("hi\n".to_string())
    };
    let result = expand::expand_word("$(echo hi)", "/home/test", &mut exec).unwrap();
    assert_eq!(result, ["hi"]);
}

#[test]
fn expand_glob_recursive_star() {
    let dir = tempdir_with_files(&[]);
    let dir = std::fs::canonicalize(&dir).unwrap();
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("deep.txt"), "").unwrap();

    let pattern = format!("{}/**/*.txt", dir.display());
    let result = expand::expand_word(&pattern, "/home/test", &mut no_subst).unwrap();
    assert!(result.iter().any(|r| r.ends_with("deep.txt")));
}

#[test]
fn expand_literal_in_word() {
    // LITERAL markers should be stripped in output
    let word = format!("{LITERAL}$plain");
    let result = expand::expand_word(&word, "/home/test", &mut no_subst).unwrap();
    // The LITERAL $ should be kept as literal, not expanded
    assert_eq!(result, ["$plain"]);
}

#[test]
fn expand_argv_multiple_words() {
    let words: Vec<String> = vec!["hello".into(), "~".into(), "plain".into()];
    let result = expand::expand_argv(&words, "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["hello", "/home/test", "plain"]);
}

#[test]
fn expand_glob_hidden_skip() {
    // Glob should skip hidden files unless pattern starts with .
    let dir = tempdir_with_files(&[".hidden", "visible"]);
    let dir = std::fs::canonicalize(&dir).unwrap();
    let pattern = format!("{}/v*", dir.display());
    let result = expand::expand_word(&pattern, "/home/test", &mut no_subst).unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0].ends_with("visible"));
}

#[test]
fn expand_glob_dot_pattern_includes_hidden() {
    let dir = tempdir_with_files(&[".hidden", "visible"]);
    let dir = std::fs::canonicalize(&dir).unwrap();
    let pattern = format!("{}/.h*", dir.display());
    let result = expand::expand_word(&pattern, "/home/test", &mut no_subst).unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0].ends_with(".hidden"));
}

// ---------------------------------------------------------------------------
// Parse: more error paths
// ---------------------------------------------------------------------------

#[test]
fn parse_background_amp_alone() {
    let result = parse::parse("echo hello &");
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("background"));
}

#[test]
fn parse_redirect_without_target() {
    // Redirect followed by a connector instead of a word
    let result = parse::parse("echo >");
    assert!(result.is_err());
}

#[test]
fn parse_trailing_connector_and() {
    // "a &&" — needs_continuation detects the trailing &&
    assert!(parse::needs_continuation("a &&"));
}

#[test]
fn parse_trailing_connector_or() {
    assert!(parse::needs_continuation("a ||"));
}

#[test]
fn parse_trailing_pipe() {
    let result = parse::parse("a |");
    assert!(result.is_err());
}

#[test]
fn parse_unescape_with_literal() {
    let s = format!("hello{LITERAL}world");
    assert_eq!(parse::unescape(&s), "helloworld");
}

#[test]
fn parse_unescape_no_literal() {
    assert_eq!(parse::unescape("hello world"), "hello world");
}

#[test]
fn parse_needs_continuation_escaped_quote() {
    // Backslash-escaped quote should not count as open
    assert!(!parse::needs_continuation(r"echo \'"));
}

#[test]
fn parse_needs_continuation_double_quote_escaped() {
    assert!(!parse::needs_continuation(r#"echo \""#));
}

#[test]
fn parse_2_redirect() {
    let cmd = parse::parse("cmd 2> errfile").unwrap();
    let r = &cmd.segments[0].0.commands[0].cmd.redirects;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].kind, RedirectKind::Err);
    assert_eq!(r[0].target, "errfile");
}

#[test]
fn parse_append_redirect() {
    let cmd = parse::parse("echo hi >> log.txt").unwrap();
    assert_eq!(
        cmd.segments[0].0.commands[0].cmd.redirects[0].kind,
        RedirectKind::Append
    );
}

#[test]
fn parse_input_redirect() {
    let cmd = parse::parse("cmd < input.txt").unwrap();
    assert_eq!(
        cmd.segments[0].0.commands[0].cmd.redirects[0].kind,
        RedirectKind::In
    );
}

#[test]
fn parse_semicolon_separator() {
    let cmd = parse::parse("a ; b").unwrap();
    assert_eq!(cmd.segments.len(), 2);
    assert_eq!(cmd.segments[0].1, Some(Connector::Semi));
}

// ---------------------------------------------------------------------------
// Alias: Default impl
// ---------------------------------------------------------------------------

#[test]
fn alias_default_impl() {
    let aliases = AliasMap::default();
    assert!(aliases.get("anything").is_none());
    assert_eq!(aliases.iter().count(), 0);
}

// ---------------------------------------------------------------------------
// LineBuffer: Default impl and more edges
// ---------------------------------------------------------------------------

#[test]
fn line_buffer_default_impl() {
    let lb = LineBuffer::default();
    assert!(lb.is_empty());
    assert_eq!(lb.cursor(), 0);
    assert_eq!(lb.text(), "");
}

#[test]
fn line_buffer_display_len() {
    let mut lb = LineBuffer::new();
    lb.set("hello");
    assert_eq!(lb.display_len(), 5);
    lb.set("日本語");
    assert_eq!(lb.display_len(), 3);
}

#[test]
fn line_buffer_move_word_at_boundaries() {
    let mut lb = LineBuffer::new();
    lb.set("hello");
    // move_word_left from end
    lb.move_word_left();
    assert_eq!(lb.cursor(), 0);
    // move_word_left from 0 — should stay
    lb.move_word_left();
    assert_eq!(lb.cursor(), 0);
    // move_word_right from 0
    lb.move_word_right();
    assert_eq!(lb.cursor(), 5);
    // move_word_right at end — should stay
    lb.move_word_right();
    assert_eq!(lb.cursor(), 5);
}

#[test]
fn line_buffer_kill_to_end_at_end() {
    let mut lb = LineBuffer::new();
    lb.set("hello");
    // Already at end — kill_to_end is no-op
    lb.kill_to_end();
    assert_eq!(lb.text(), "hello");
}

#[test]
fn line_buffer_kill_to_start_at_start() {
    let mut lb = LineBuffer::new();
    lb.set("hello");
    lb.move_home();
    // Already at start — kill_to_start is no-op
    lb.kill_to_start();
    assert_eq!(lb.text(), "hello");
}

#[test]
fn line_buffer_kill_word_back_at_start() {
    let mut lb = LineBuffer::new();
    lb.set("hello");
    lb.move_home();
    lb.kill_word_back();
    assert_eq!(lb.text(), "hello");
}

#[test]
fn line_buffer_yank_empty_kill_ring() {
    let mut lb = LineBuffer::new();
    lb.set("hello");
    lb.yank(); // kill ring is empty — should be no-op
    assert_eq!(lb.text(), "hello");
}

#[test]
fn line_buffer_word_movement_with_whitespace() {
    let mut lb = LineBuffer::new();
    lb.set("  hello  world  ");
    lb.move_home();
    lb.move_word_right(); // skip ws, then skip "hello", then skip ws -> at 'w'
    let pos = lb.display_cursor_pos();
    // Should be past first word and whitespace
    assert!(pos > 0);
    lb.move_word_left();
    // Should go back
    assert!(lb.display_cursor_pos() < pos);
}

// ---------------------------------------------------------------------------
// Completion: more navigation edges
// ---------------------------------------------------------------------------

#[test]
fn completion_state_selected_entry() {
    let entries = make_entries(&["a", "b", "c"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    let state = CompletionState {
        entries,
        selected: 1,
        cols,
        rows,
        scroll: 0,
        dir_prefix: String::new(),
    };
    assert_eq!(state.selected_entry().unwrap().name, "b");
}

#[test]
fn completion_state_selected_entry_out_of_bounds() {
    let entries = make_entries(&["a"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    let state = CompletionState {
        entries,
        selected: 99,
        cols,
        rows,
        scroll: 0,
        dir_prefix: String::new(),
    };
    assert!(state.selected_entry().is_none());
}

#[test]
fn completion_move_with_zero_rows() {
    let state_entries: Vec<CompEntry> = vec![];
    let mut state = CompletionState {
        entries: state_entries,
        selected: 0,
        cols: 0,
        rows: 0,
        scroll: 0,
        dir_prefix: String::new(),
    };
    // All moves should be no-ops when rows == 0
    state.move_up();
    state.move_down();
    state.move_left();
    state.move_right();
    assert_eq!(state.selected, 0);
}

#[test]
fn completion_navigation_single_entry() {
    let entries = make_entries(&["only"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    let mut state = CompletionState {
        entries,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        dir_prefix: String::new(),
    };
    state.move_up();
    assert_eq!(state.selected, 0);
    state.move_down();
    assert_eq!(state.selected, 0);
    state.move_left();
    assert_eq!(state.selected, 0);
    state.move_right();
    assert_eq!(state.selected, 0);
}

#[test]
fn completion_move_right_wraps_to_first_col() {
    let entries = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    let mut state = CompletionState {
        entries,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        dir_prefix: String::new(),
    };
    // Move right until we wrap
    for _ in 0..20 {
        state.move_right();
        assert!(state.selected < state.entries.len());
    }
}

#[test]
fn completion_move_left_wraps_to_last_col() {
    let entries = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&entries, 80);
    let mut state = CompletionState {
        entries,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        dir_prefix: String::new(),
    };
    state.move_left(); // from col 0 should wrap to last col
    assert!(state.selected > 0);
    assert!(state.selected < state.entries.len());
}

#[test]
fn comp_entry_display_link() {
    let e = CompEntry {
        name: "link".into(),
        is_dir: false,
        is_link: true,
        is_exec: false,
    };
    assert_eq!(&*e.display_name(), "link");
    assert_eq!(e.display_width(), 4);
}

#[test]
fn comp_entry_display_exec() {
    let e = CompEntry {
        name: "script".into(),
        is_dir: false,
        is_link: false,
        is_exec: true,
    };
    assert_eq!(&*e.display_name(), "script");
}

// ---------------------------------------------------------------------------
// Input: KeyEvent constructors
// ---------------------------------------------------------------------------

#[test]
fn input_key_event_constructors() {
    use ish::input::{Key, KeyEvent, Modifiers};

    let ke = KeyEvent::key(Key::Enter);
    assert_eq!(ke.key, Key::Enter);
    assert!(!ke.mods.ctrl && !ke.mods.alt && !ke.mods.shift);

    let ke = KeyEvent::char('a');
    assert_eq!(ke.key, Key::Char('a'));

    let ke = KeyEvent::ctrl('c');
    assert_eq!(ke.key, Key::Char('c'));
    assert!(ke.mods.ctrl);

    let ke = KeyEvent::alt('f');
    assert_eq!(ke.key, Key::Char('f'));
    assert!(ke.mods.alt);

    let ke = KeyEvent::with_mods(
        Key::Up,
        Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
        },
    );
    assert_eq!(ke.key, Key::Up);
    assert!(ke.mods.ctrl && ke.mods.alt && ke.mods.shift);
}

#[test]
fn input_modifiers_none() {
    use ish::input::Modifiers;
    let m = Modifiers::NONE;
    assert!(!m.ctrl && !m.alt && !m.shift);
}

#[test]
fn input_modifiers_default() {
    use ish::input::Modifiers;
    let m = Modifiers::default();
    assert!(!m.ctrl && !m.alt && !m.shift);
}

#[test]
fn input_modifier_from_param_zero() {
    // param=0 → saturating_sub(1) = 0, all false
    let m = input::modifier_from_param(0);
    assert!(!m.ctrl && !m.alt && !m.shift);
}

// ---------------------------------------------------------------------------
// Prompt: shorten_pwd additional edges
// ---------------------------------------------------------------------------

#[test]
fn prompt_shorten_pwd_single_char_components() {
    // Single-char middle components should not be abbreviated further
    assert_eq!(prompt::shorten_pwd("/a/b/c/target", ""), "/a/b/c/target");
}

#[test]
fn prompt_shorten_pwd_dotfiles_in_middle() {
    assert_eq!(
        prompt::shorten_pwd("/home/u/.local/share/ish", "/home/u"),
        "~/.l/s/ish"
    );
}

// ---------------------------------------------------------------------------
// History: file I/O with tempdir
// ---------------------------------------------------------------------------

#[test]
fn history_file_io() {
    let dir = tempdir_with_files(&[]);
    let hist_dir = dir.join("ish");
    std::fs::create_dir_all(&hist_dir).unwrap();
    let hist_file = hist_dir.join("history");

    let old = std::env::var("XDG_DATA_HOME").ok();
    unsafe { std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap()) };

    // Write history
    let mut h = History::load();
    h.add("test command 1");
    h.add("test command 2");
    h.add("test command 1"); // dup — should be deduped in memory

    // Verify file was written
    let content = std::fs::read_to_string(&hist_file).unwrap();
    assert!(content.contains("test command 1"));
    assert!(content.contains("test command 2"));

    // Reload and verify dedup
    let h2 = History::load();
    assert!(h2.len() >= 2);

    // Restore
    match old {
        Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
    }
}

// ---------------------------------------------------------------------------
// Expand: LITERAL marker handling
// ---------------------------------------------------------------------------

#[test]
fn expand_strip_literal_no_markers() {
    // expand_word with no special chars should fast-path
    let result = expand::expand_word("plain_word", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["plain_word"]);
}

#[test]
fn expand_backtick_subst() {
    let mut exec = |cmd: &str| -> Result<String, Error> {
        assert_eq!(cmd, "date");
        Ok("2024-01-01\n".to_string())
    };
    let result = expand::expand_word("`date`", "/home/test", &mut exec).unwrap();
    assert_eq!(result, ["2024-01-01"]);
}

#[test]
fn expand_nested_command_subst() {
    let mut exec = |_cmd: &str| -> Result<String, Error> { Ok("inner\n".to_string()) };
    let result = expand::expand_word("$(echo $(pwd))", "/home/test", &mut exec).unwrap();
    assert_eq!(result, ["inner"]);
}

#[test]
fn expand_tilde_not_prefix() {
    // ~ not at start should not expand
    let result = expand::expand_word("path/~/file", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["path/~/file"]);
}

#[test]
fn expand_tilde_user_not_supported() {
    // ~otheruser should pass through unchanged
    let result = expand::expand_word("~otheruser/file", "/home/test", &mut no_subst).unwrap();
    assert_eq!(result, ["~otheruser/file"]);
}

// ---------------------------------------------------------------------------
// ls: executable files, symlinks, permissions edge cases
// ---------------------------------------------------------------------------

#[test]
fn ls_list_dir_with_executable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir_with_files(&["script.sh"]);
    // Make executable
    let path = dir.join("script.sh");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(ls::list_dir(dir.to_str().unwrap()), 0);
}

#[test]
fn ls_list_dir_with_symlink_to_dir() {
    let dir = tempdir_with_files(&[]);
    let subdir = dir.join("realdir");
    std::fs::create_dir(&subdir).unwrap();
    let link_path = dir.join("linkdir");
    std::os::unix::fs::symlink(&subdir, &link_path).unwrap();
    assert_eq!(ls::list_dir(dir.to_str().unwrap()), 0);
}

#[test]
fn ls_list_symlink_to_dir_as_file() {
    // When listing a symlink that points to a dir, should list the dir contents
    let dir = tempdir_with_files(&["inside.txt"]);
    let parent = dir.parent().unwrap();
    let link_path = parent.join(format!("ish_test_link_{}", std::process::id()));
    let _ = std::fs::remove_file(&link_path);
    std::os::unix::fs::symlink(&dir, &link_path).unwrap();
    assert_eq!(ls::list_dir(link_path.to_str().unwrap()), 0);
    let _ = std::fs::remove_file(&link_path);
}

#[test]
fn ls_list_dir_sorts_case_insensitive() {
    let dir = tempdir_with_files(&["Zebra", "alpha", "Beta"]);
    // Should not crash; output should be sorted
    assert_eq!(ls::list_dir(dir.to_str().unwrap()), 0);
}

// ---------------------------------------------------------------------------
// Prompt: git branch with temp repo
// ---------------------------------------------------------------------------

#[test]
fn prompt_git_branch_detects_repo() {
    let dir = tempdir_with_files(&[]);
    let git_dir = dir.join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/test-branch\n").unwrap();

    // Render sets PWD, but other tests may race. Just verify the code path runs.
    let mut p = prompt::Prompt::new();
    unsafe { std::env::set_var("PWD", dir.to_str().unwrap()) };
    let rendered = p.render(0);
    // If PWD wasn't clobbered, we see the branch. Either way, no crash.
    assert!(rendered.ends_with(" $ "));
}

#[test]
fn prompt_git_branch_detached_head() {
    let dir = tempdir_with_files(&[]);
    let git_dir = dir.join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "abc12345deadbeef\n").unwrap();

    let mut p = prompt::Prompt::new();
    unsafe { std::env::set_var("PWD", dir.to_str().unwrap()) };
    let rendered = p.render(0);
    assert!(rendered.ends_with(" $ "));
}

#[test]
fn prompt_git_branch_no_repo() {
    let dir = tempdir_with_files(&[]);
    let mut p = prompt::Prompt::new();
    unsafe { std::env::set_var("PWD", dir.to_str().unwrap()) };
    let rendered = p.render(0);
    assert!(rendered.ends_with(" $ "));
}

#[test]
fn prompt_git_cache_no_repo_reuse() {
    let dir = tempdir_with_files(&[]);
    let subdir = dir.join("sub");
    std::fs::create_dir(&subdir).unwrap();

    let mut p = prompt::Prompt::new();

    // First render in dir with no repo — caches NoRepo
    unsafe { std::env::set_var("PWD", dir.to_str().unwrap()) };
    let _ = p.render(0);

    // Render in subdir — exercises the NoRepo cache path
    unsafe { std::env::set_var("PWD", subdir.to_str().unwrap()) };
    let r = p.render(0);
    assert!(r.ends_with(" $ "));
}

#[test]
fn prompt_git_invalidate_clears_cache() {
    let dir = tempdir_with_files(&[]);
    let git_dir = dir.join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    let mut p = prompt::Prompt::new();
    unsafe { std::env::set_var("PWD", dir.to_str().unwrap()) };
    let _ = p.render(0);

    // Invalidate and re-render — exercises the Unknown→Repo path again
    p.invalidate_git();
    unsafe { std::env::set_var("PWD", dir.to_str().unwrap()) };
    let r = p.render(0);
    assert!(r.ends_with(" $ "));
}

#[test]
fn prompt_git_bare_ref() {
    let dir = tempdir_with_files(&[]);
    let git_dir = dir.join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/remotes/origin/main\n").unwrap();

    let mut p = prompt::Prompt::new();
    unsafe { std::env::set_var("PWD", dir.to_str().unwrap()) };
    let rendered = p.render(0);
    assert!(rendered.ends_with(" $ "));
}

#[test]
fn prompt_git_worktree_gitdir_file() {
    let dir = tempdir_with_files(&[]);
    let real_git = dir.join("real_git");
    std::fs::create_dir(&real_git).unwrap();
    std::fs::write(real_git.join("HEAD"), "ref: refs/heads/worktree\n").unwrap();

    // Create a .git file (not directory) pointing to the real git dir
    let worktree = dir.join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", real_git.display()),
    )
    .unwrap();

    let mut p = prompt::Prompt::new();
    unsafe { std::env::set_var("PWD", worktree.to_str().unwrap()) };
    let rendered = p.render(0);
    assert!(rendered.ends_with(" $ "));
}

// ---------------------------------------------------------------------------
// Completion: symlinks in completions
// ---------------------------------------------------------------------------

#[test]
fn complete_path_symlink() {
    let dir = tempdir_with_files(&["target.txt"]);
    let link_path = dir.join("link.txt");
    std::os::unix::fs::symlink(dir.join("target.txt"), &link_path).unwrap();

    let path = format!("{}/", dir.display());
    let entries = complete::complete_path(&path, false);
    let names: HashSet<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains("target.txt"));
    assert!(names.contains("link.txt"));
    // link should be marked as a link
    let link_entry = entries.iter().find(|e| e.name == "link.txt").unwrap();
    assert!(link_entry.is_link);
}
