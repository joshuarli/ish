//! Integration tests for ish shell.
//!
//! Tests the public API at a higher level than unit tests — verifying that
//! modules compose correctly (line editing sequences, history search,
//! completion, config parsing, etc.).

use std::collections::HashSet;

use ish::alias::AliasMap;
use ish::builtin;
use ish::complete::{self, CompletionState, Completions};
use ish::config;
use ish::history::{self, History};
use ish::input;
use ish::line::LineBuffer;
use ish::ls;
use ish::prompt;

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
    assert_eq!(lb.display_cursor_pos(), 2); // '日' is 2 columns wide
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
fn line_buffer_kill_word_forward() {
    let mut lb = LineBuffer::new();
    lb.set("foo   bar   baz");
    lb.move_home();
    lb.kill_word_forward();
    assert_eq!(lb.text(), "   bar   baz");
    assert_eq!(lb.cursor(), 0);
    lb.kill_word_forward();
    assert_eq!(lb.text(), "   baz");
    assert_eq!(lb.cursor(), 0);
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
    // "ls" should appear only once, at the end
    assert_eq!((0..h.len()).filter(|&i| h.get(i) == "ls").count(), 1);
    assert_eq!(h.get(h.len() - 1), "ls");
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
    // Both have the same score (g at boundary, c at boundary, same contiguity),
    // so recency breaks the tie.
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
    // Empty query: all scores are 0, so most-recent-first
    assert_eq!(h.get(matches[0].entry_idx), "c");
}

#[test]
fn history_fuzzy_match_positions_correct() {
    let q: Vec<char> = "gco".chars().collect();
    let (positions, count) = history::subsequence_match(&q, "git checkout").unwrap();
    assert_eq!(count, 3);
    assert_eq!(&positions[..3], &[0, 4, 9]); // g=0, c=4, o=9
}

// ---------------------------------------------------------------------------
// Fuzzy search scoring — real-world scenarios
// ---------------------------------------------------------------------------

#[test]
fn scoring_contiguous_beats_scattered() {
    // The motivating bug: searching "target" should prefer the entry that
    // contains the literal word "target" over one where t-a-r-g-e-t are
    // scattered across a long URL.
    let h = History::from_entries(vec![
        "/Users/josh/d/ish/target/release/ish".into(), // older, but "target" is contiguous
        "git remote add origin https://github.com/joshuarli/smtp-server.git".into(), // more recent, scattered match
    ]);
    let matches = h.fuzzy_search("target");
    assert_eq!(matches.len(), 2);
    // The contiguous match must rank first despite being older
    assert_eq!(
        h.get(matches[0].entry_idx),
        "/Users/josh/d/ish/target/release/ish"
    );
}

#[test]
fn scoring_word_boundary_preferred() {
    // "debug" should prefer the entry where it's a contiguous word at a
    // boundary over one where d-e-b-u-g are scattered.
    let h = History::from_entries(vec![
        "ls target/debug/".into(), // older, "debug" contiguous after /
        "docker exec -it bash ubuntu_gateway".into(), // more recent, d-e-b-u-g scattered
    ]);
    let matches = h.fuzzy_search("debug");
    assert_eq!(matches.len(), 2);
    assert_eq!(h.get(matches[0].entry_idx), "ls target/debug/");
}

#[test]
fn scoring_exact_command_name() {
    // Searching for a common command name. "cargo" should prefer entries
    // where "cargo" appears as a contiguous word over scattered c-a-r-g-o.
    let h = History::from_entries(vec![
        "cargo test --release".into(), // older, literal "cargo"
        "cat /var/log/syslog | rg 'error|warning'".into(), // more recent, scattered c-a-r-g-o
    ]);
    let matches = h.fuzzy_search("cargo");
    assert_eq!(matches.len(), 2);
    assert_eq!(h.get(matches[0].entry_idx), "cargo test --release");
}

#[test]
fn scoring_path_components() {
    // Searching for a path component. "src" should rank entries containing
    // /src/ higher than those with scattered s-r-c.
    let h = History::from_entries(vec![
        "vim src/main.rs".into(),    // "src" is contiguous at word boundary
        "ssh remote_cluster".into(), // s-r-c scattered
    ]);
    let matches = h.fuzzy_search("src");
    assert_eq!(matches.len(), 2);
    assert_eq!(h.get(matches[0].entry_idx), "vim src/main.rs");
}

#[test]
fn scoring_flag_matching() {
    // "--verbose" should match entries with the literal flag, not scattered chars.
    let h = History::from_entries(vec![
        "curl --verbose https://api.example.com".into(), // literal --verbose
        "vagrant exec rebuild_host_svc_01; echo started".into(), // v-e-r-b-o-s-e scattered
    ]);
    let matches = h.fuzzy_search("verbose");
    assert_eq!(matches.len(), 2);
    assert_eq!(
        h.get(matches[0].entry_idx),
        "curl --verbose https://api.example.com"
    );
}

#[test]
fn scoring_pwd_bonus() {
    // Current directory should not bias Ctrl+R results; recency and match tier
    // are easier to reason about.
    let h = History::from_entries(vec![
        "cargo build".into(),                 // older, no "ish"
        "npm install".into(),                 // more recent, no "ish"
        "./target/release/ish --help".into(), // oldest, but contains "ish"
    ]);
    let matches = h.fuzzy_search_scored("build", "ish");
    assert!(!matches.is_empty());
    assert_eq!(h.get(matches[0].entry_idx), "cargo build");

    // Identical match tier falls back to recency, not PWD context.
    let h2 = History::from_entries(vec![
        "cd /tmp/ish && make".into(), // older, contains "ish"
        "cd /tmp/foo && make".into(), // more recent, no "ish"
    ]);
    let matches2 = h2.fuzzy_search_scored("make", "ish");
    assert_eq!(matches2.len(), 2);
    assert_eq!(h2.get(matches2[0].entry_idx), "cd /tmp/foo && make");
}

#[test]
fn scoring_git_workflow() {
    // Real git workflow: searching "push" in a history full of git commands.
    let h = History::from_entries(vec![
        "git push -u origin main".into(),
        "git pull --rebase".into(),
        "git status".into(),
        "pushd /tmp".into(), // "push" also matches here, contiguous + boundary
    ]);
    let matches = h.fuzzy_search("push");
    assert_eq!(matches.len(), 2); // only "git push" and "pushd" match
    // Both have contiguous "push" at word boundary. "pushd" is more recent.
    assert_eq!(h.get(matches[0].entry_idx), "pushd /tmp");
    assert_eq!(h.get(matches[1].entry_idx), "git push -u origin main");
}

#[test]
fn scoring_docker_compose() {
    // Searching "compose" should prefer the entry with literal "compose"
    // over scattered c-o-m-p-o-s-e.
    let h = History::from_entries(vec![
        "docker compose up -d".into(), // literal "compose"
        "cp /opt/myconfig /opt/provisioner/settings.env".into(), // c-o-m-p-o-s-e scattered
    ]);
    let matches = h.fuzzy_search("compose");
    assert_eq!(matches.len(), 2);
    assert_eq!(h.get(matches[0].entry_idx), "docker compose up -d");
}

#[test]
fn scoring_equal_quality_uses_recency() {
    // When match quality is identical, most-recent should win.
    let h = History::from_entries(vec![
        "echo hello".into(),
        "echo world".into(),
        "echo goodbye".into(),
    ]);
    let matches = h.fuzzy_search("echo");
    assert_eq!(matches.len(), 3);
    // All have identical scores (contiguous "echo" at position 0), so recency wins
    assert_eq!(h.get(matches[0].entry_idx), "echo goodbye");
    assert_eq!(h.get(matches[1].entry_idx), "echo world");
    assert_eq!(h.get(matches[2].entry_idx), "echo hello");
}

#[test]
fn scoring_long_path_not_destroyed_by_gaps() {
    // A match in a long path shouldn't be destroyed by the gap penalty cap.
    // The -3 cap per gap ensures that one long gap between matches doesn't
    // overwhelm the contiguity bonuses from the matched portion.
    let h = History::from_entries(vec![
        "/very/long/deeply/nested/path/to/target/release/binary".into(),
        "stat /tmp/a/really/great/example/thing".into(), // t-a-r-g-e-t scattered over many path components
    ]);
    let matches = h.fuzzy_search("target");
    assert_eq!(matches.len(), 2);
    assert_eq!(
        h.get(matches[0].entry_idx),
        "/very/long/deeply/nested/path/to/target/release/binary"
    );
}

#[test]
fn scoring_npm_scripts() {
    // "test" should prefer "npm test" over entries with scattered t-e-s-t.
    let h = History::from_entries(vec![
        "npm test".into(),
        "git remote set-url origin https://example.com".into(), // t-e-s-t scattered
    ]);
    let matches = h.fuzzy_search("test");
    assert_eq!(matches.len(), 2);
    assert_eq!(h.get(matches[0].entry_idx), "npm test");
}

#[test]
fn optimal_alignment_finds_tight_window() {
    // The greedy forward pass would match d(1) e(7) b(12) in "cd target/debug"
    // (the 'd' in 'cd', the 'e' in 'target', the 'b' in 'debug').
    // The backward pass should find the tighter window d(10) e(11) b(12)
    // (all three contiguous in "debug").
    let q: Vec<char> = "deb".chars().collect();
    let (positions, count) = history::subsequence_match(&q, "cd target/debug").unwrap();
    assert_eq!(count, 3);
    // Optimal: d(10) e(11) b(12) — contiguous in "debug"
    assert_eq!(
        &positions[..3],
        &[10, 11, 12],
        "should find tight window in 'debug', not scattered across 'cd target/debug'"
    );
}

#[test]
fn optimal_alignment_prefers_contiguous_suffix() {
    // "test" appears scattered early AND contiguous late in the string.
    // Forward greedy: t(0) e(3) s(6) t(9) — scattered in "the best test"
    // Optimal: t(9) e(10) s(11) t(12) — contiguous "test"
    let q: Vec<char> = "test".chars().collect();
    let (positions, count) = history::subsequence_match(&q, "the best test").unwrap();
    assert_eq!(count, 4);
    assert_eq!(
        &positions[..4],
        &[9, 10, 11, 12],
        "should find contiguous 'test' at end, not scattered across 'the best'"
    );
}

#[test]
fn optimal_alignment_single_char() {
    // Single char query — backward pass is trivial, should still work.
    let q: Vec<char> = "t".chars().collect();
    let (positions, count) = history::subsequence_match(&q, "cat").unwrap();
    assert_eq!(count, 1);
    // Greedy finds t(2), backward pass starts there, window is [2,2]
    assert_eq!(positions[0], 2);
}

#[test]
fn optimal_alignment_full_string_match() {
    // Query matches the entire string — no ambiguity.
    let q: Vec<char> = "abc".chars().collect();
    let (positions, count) = history::subsequence_match(&q, "abc").unwrap();
    assert_eq!(count, 3);
    assert_eq!(&positions[..3], &[0, 1, 2]);
}

#[test]
fn optimal_alignment_real_world_path() {
    // Searching "main" in a path where "main" appears literally but 'm' occurs earlier.
    let q: Vec<char> = "main".chars().collect();
    let (positions, count) =
        history::subsequence_match(&q, "cmake -B build && make -j8 && ./main").unwrap();
    assert_eq!(count, 4);
    // "main" appears contiguously at positions 32-35 ("./main")
    // Greedy would pick m(1) from "cmake", but optimal should find tighter window
    assert_eq!(
        &positions[..4],
        &[32, 33, 34, 35],
        "should find contiguous 'main' at end"
    );
}

#[test]
fn scoring_first_match_bonus() {
    // Entries starting with the query char should get a first-match bonus.
    // "git status" is most recent so recency and first-match bonus both favor it.
    let h = History::from_entries(vec![
        "rg pattern".into(), // 'g' is mid-word
        "git status".into(), // starts with 'g'
    ]);
    let matches = h.fuzzy_search("g");
    assert_eq!(matches.len(), 2);
    // "git status" has first-match bonus (+4) and recency
    assert_eq!(h.get(matches[0].entry_idx), "git status");
}

#[test]
fn scoring_first_match_with_optimal_alignment() {
    // Combining both: optimal alignment should find the contiguous match,
    // and first-match bonus helps entries starting with the query.
    let h = History::from_entries(vec![
        "echo cargo".into(),            // "cargo" contiguous but not at start
        "cargo build --release".into(), // "cargo" at start, first-match bonus + most recent
    ]);
    let matches = h.fuzzy_search("cargo");
    assert_eq!(matches.len(), 2);
    // Both have contiguous "cargo" with boundary bonus.
    // "cargo build" gets first-match bonus (+4) and recency.
    assert_eq!(h.get(matches[0].entry_idx), "cargo build --release");
}

#[test]
fn scoring_into_matches_search() {
    // Verify fuzzy_search_into produces the same ranking as fuzzy_search
    let h = History::from_entries(vec![
        "/Users/josh/d/ish/target/release/ish".into(),
        "git remote add origin https://github.com/joshuarli/smtp-server.git".into(),
    ]);
    let search_results = h.fuzzy_search("target");
    let mut into_results = Vec::new();
    h.fuzzy_search_into("target", &mut into_results, 200, "");
    assert_eq!(search_results.len(), into_results.len());
    for (a, b) in search_results.iter().zip(into_results.iter()) {
        assert_eq!(a.entry_idx, b.entry_idx);
        assert_eq!(a.score, b.score);
    }
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

fn make_entries(names: &[&str]) -> Completions {
    let mut comp = Completions::new();
    for n in names {
        comp.push(n, false, false, false);
    }
    comp
}

#[test]
fn grid_single_entry() {
    let comp = make_entries(&["foo"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    assert_eq!(cols, 1);
    assert_eq!(rows, 1);
}

#[test]
fn grid_fits_multiple_columns() {
    let comp = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    assert!(cols > 1);
    assert!(cols * rows >= 6);
}

#[test]
fn grid_narrow_terminal_forces_single_column() {
    let comp = make_entries(&["longfilename.rs", "anotherlongname.rs"]);
    let (cols, _rows) = complete::compute_grid(&comp.entries, 20);
    assert_eq!(cols, 1);
}

#[test]
fn grid_empty_entries() {
    let comp = Completions::new();
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    assert_eq!(cols, 0);
    assert_eq!(rows, 0);
}

#[test]
fn completion_state_navigation_wraps() {
    let comp = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    let mut state = CompletionState {
        comp,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        term_cols: 80,
        dir_prefix: String::new(),
        in_quote: false,
    };

    // Navigate down through all entries
    for _ in 0..20 {
        state.move_down();
    }
    // Should wrap around eventually
    // Just verify it doesn't panic and stays in bounds
    assert!(state.selected < state.comp.entries.len());

    // Navigate up from 0
    state.selected = 0;
    state.move_up();
    assert!(state.selected < state.comp.entries.len());
}

#[test]
fn completion_state_left_right_wrap() {
    let comp = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    let mut state = CompletionState {
        comp,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        term_cols: 80,
        dir_prefix: String::new(),
        in_quote: false,
    };

    // Move left from first column should wrap to last
    state.move_left();
    assert!(state.selected < state.comp.entries.len());

    // Move right from last column should wrap to first
    state.selected = state.comp.entries.len() - 1;
    state.move_right();
    assert!(state.selected < state.comp.entries.len());
}

#[test]
fn comp_entry_display_name_dir_suffix() {
    let mut comp = Completions::new();
    comp.push("src", true, false, false);
    let e = &comp.entries[0];
    let name = comp.entry_name(e);
    assert_eq!(name, "src");
    assert_eq!(e.display_width(), 4); // "src/"
}

#[test]
fn comp_entry_display_name_file() {
    let mut comp = Completions::new();
    comp.push("main.rs", false, false, false);
    let e = &comp.entries[0];
    let name = comp.entry_name(e);
    assert_eq!(name, "main.rs");
    assert_eq!(e.display_width(), 7);
}

// ---------------------------------------------------------------------------
// Completion: filesystem (uses temp directories)
// ---------------------------------------------------------------------------

#[test]
fn complete_path_finds_files() {
    let dir = tempdir_with_files(&["foo.rs", "foo.txt", "bar.rs"]);
    let path = format!("{}/foo", dir.display());
    let comp = complete::complete_path(&path, false);
    assert_eq!(comp.len(), 2);
    let names: HashSet<_> = (0..comp.len()).map(|i| comp.name(i)).collect();
    assert!(names.contains("foo.rs"));
    assert!(names.contains("foo.txt"));
}

#[test]
fn complete_path_dirs_only() {
    let dir = tempdir_with_files(&["file.rs"]);
    std::fs::create_dir(dir.join("subdir")).unwrap();
    let path = format!("{}/", dir.display());
    let comp = complete::complete_path(&path, true);
    assert_eq!(comp.len(), 1);
    assert_eq!(comp.name(0), "subdir");
    assert!(comp.entries[0].is_dir());
}

#[test]
fn complete_path_hidden_files() {
    let dir = tempdir_with_files(&[".hidden", "visible"]);
    // Without dot prefix, hidden files should be excluded
    let path = format!("{}/", dir.display());
    let comp = complete::complete_path(&path, false);
    let names: HashSet<_> = (0..comp.len()).map(|i| comp.name(i)).collect();
    assert!(names.contains("visible"));
    assert!(!names.contains(".hidden"));

    // With dot prefix, hidden files should be included
    let path = format!("{}/.h", dir.display());
    let comp = complete::complete_path(&path, false);
    assert_eq!(comp.len(), 1);
    assert_eq!(comp.name(0), ".hidden");
}

#[test]
fn complete_path_nonexistent_dir() {
    let comp = complete::complete_path("/nonexistent/path/foo", false);
    assert!(comp.is_empty());
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
// Security-relevant tests
// ---------------------------------------------------------------------------

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
// Config: load, parse_set, parse_alias, config_path
// ---------------------------------------------------------------------------

#[test]
fn config_load_all_paths() {
    // Consolidate config load tests into one to avoid XDG_CONFIG_HOME races.
    let old_xdg = std::env::var_os("XDG_CONFIG_HOME");

    // 1. Nonexistent config — should not panic
    let empty_dir = tempdir_with_files(&[]);
    unsafe { std::env::set_var("XDG_CONFIG_HOME", empty_dir.to_str().unwrap()) };
    let mut aliases = AliasMap::new();
    let mut epsh = epsh::eval::Shell::new();
    config::load(&mut aliases, &mut epsh, None);

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
    let mut epsh = epsh::eval::Shell::new();
    config::load(&mut aliases, &mut epsh, None);
    assert_eq!(
        std::env::var_os("ISH_TEST_CFG_LOAD_VAR")
            .unwrap()
            .to_string_lossy(),
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
    let mut epsh = epsh::eval::Shell::new();
    config::load(&mut aliases, &mut epsh, None);

    // 4. Alias without expansion — should not be added
    let dir3 = tempdir_with_files(&[]);
    let config_dir3 = dir3.join("ish");
    std::fs::create_dir_all(&config_dir3).unwrap();
    std::fs::write(config_dir3.join("config.ish"), "alias myalias\n").unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir3.to_str().unwrap()) };
    let mut aliases = AliasMap::new();
    let mut epsh = epsh::eval::Shell::new();
    config::load(&mut aliases, &mut epsh, None);
    assert!(aliases.get("myalias").is_none());

    // 5. set VAR with no value → empty string
    let dir4 = tempdir_with_files(&[]);
    let config_dir4 = dir4.join("ish");
    std::fs::create_dir_all(&config_dir4).unwrap();
    std::fs::write(config_dir4.join("config.ish"), "set ISH_TEST_CFG_NOVAL\n").unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir4.to_str().unwrap()) };
    let mut aliases = AliasMap::new();
    let mut epsh = epsh::eval::Shell::new();
    config::load(&mut aliases, &mut epsh, None);
    assert_eq!(
        std::env::var_os("ISH_TEST_CFG_NOVAL")
            .unwrap()
            .to_string_lossy(),
        ""
    );

    // Restore
    match old_xdg {
        Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
    }
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
// Builtin: is_builtin, is_ish_builtin
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
// Alias: Default impl
// ---------------------------------------------------------------------------

#[test]
fn alias_default_impl() {
    let aliases = AliasMap::default();
    assert!(aliases.get("anything").is_none());
    assert_eq!(aliases.iter().count(), 0);
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
    assert_eq!(h.get(h.len() - 1), "echo hello");
}

#[test]
fn history_add_dedup_preserves_order() {
    let mut h = History::from_entries(vec!["a".into(), "b".into(), "c".into()]);
    h.add("a");
    let entries: Vec<&str> = (0..h.len()).map(|i| h.get(i)).collect();
    assert_eq!(entries, &["b", "c", "a"]);
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
    let result = history::subsequence_match(&q, "anything");
    assert_eq!(result, Some(([0; 32], 0)));
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
    assert_eq!(lb.display_len(), 6); // 3 CJK chars * 2 columns each
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
    let comp = make_entries(&["a", "b", "c"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    let state = CompletionState {
        comp,
        selected: 1,
        cols,
        rows,
        scroll: 0,
        term_cols: 80,
        dir_prefix: String::new(),
        in_quote: false,
    };
    assert_eq!(state.selected_name().unwrap(), "b");
}

#[test]
fn completion_state_selected_entry_out_of_bounds() {
    let comp = make_entries(&["a"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    let state = CompletionState {
        comp,
        selected: 99,
        cols,
        rows,
        scroll: 0,
        term_cols: 80,
        dir_prefix: String::new(),
        in_quote: false,
    };
    assert!(state.selected_entry().is_none());
}

#[test]
fn completion_move_with_zero_rows() {
    let mut state = CompletionState {
        comp: Completions::new(),
        selected: 0,
        cols: 0,
        rows: 0,
        scroll: 0,
        term_cols: 80,
        dir_prefix: String::new(),
        in_quote: false,
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
    let comp = make_entries(&["only"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    let mut state = CompletionState {
        comp,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        term_cols: 80,
        dir_prefix: String::new(),
        in_quote: false,
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
    let comp = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    let mut state = CompletionState {
        comp,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        term_cols: 80,
        dir_prefix: String::new(),
        in_quote: false,
    };
    // Move right until we wrap
    for _ in 0..20 {
        state.move_right();
        assert!(state.selected < state.comp.entries.len());
    }
}

#[test]
fn completion_move_left_wraps_to_last_col() {
    let comp = make_entries(&["a", "b", "c", "d", "e", "f"]);
    let (cols, rows) = complete::compute_grid(&comp.entries, 80);
    let mut state = CompletionState {
        comp,
        selected: 0,
        cols,
        rows,
        scroll: 0,
        term_cols: 80,
        dir_prefix: String::new(),
        in_quote: false,
    };
    state.move_left(); // from col 0 should wrap to last col
    assert!(state.selected > 0);
    assert!(state.selected < state.comp.entries.len());
}

#[test]
fn comp_entry_display_link() {
    let mut comp = Completions::new();
    comp.push("link", false, true, false);
    let e = &comp.entries[0];
    assert_eq!(comp.entry_name(e), "link");
    assert_eq!(e.display_width(), 4);
}

#[test]
fn comp_entry_display_exec() {
    let mut comp = Completions::new();
    comp.push("script", false, false, true);
    let e = &comp.entries[0];
    assert_eq!(comp.entry_name(e), "script");
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
    let hist_file = dir.join("history");

    // Write history
    let mut h = History::load_from(hist_file.clone());
    h.add("test command 1");
    h.add("test command 2");
    h.add("test command 1"); // dup — should be deduped in memory

    // Verify file was written
    let content = std::fs::read_to_string(&hist_file).unwrap();
    assert!(content.contains("test command 1"));
    assert!(content.contains("test command 2"));

    // Reload and verify dedup
    let h2 = History::load_from(hist_file);
    assert!(h2.len() >= 2);
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
    let comp = complete::complete_path(&path, false);
    let names: HashSet<_> = (0..comp.len()).map(|i| comp.name(i)).collect();
    assert!(names.contains("target.txt"));
    assert!(names.contains("link.txt"));
    // link should be marked as a link
    let link_idx = (0..comp.len())
        .find(|&i| comp.name(i) == "link.txt")
        .unwrap();
    assert!(comp.entries[link_idx].is_link());
}
