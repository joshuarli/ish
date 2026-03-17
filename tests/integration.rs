//! Integration tests for ish shell.
//!
//! Tests the public API at a higher level than unit tests — verifying that
//! modules compose correctly (parse → expand, line editing sequences,
//! history search, completion, config parsing, etc.).

use std::collections::HashSet;

use ish::alias::AliasMap;
use ish::complete::{self, CompEntry, CompletionState};
use ish::config;
use ish::error::Error;
use ish::expand;
use ish::history::{self, History};
use ish::input;
use ish::line::LineBuffer;
use ish::parse::{self, Connector, RedirectKind};
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
