//! Benchmark harness for ish shell.
//!
//! Tracks wall time for user-visible and interactive hot paths.
//! Run: `cargo bench`

use std::time::Duration;

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};

use ish::alias::AliasMap;
use ish::complete;
use ish::history::History;
use ish::line::LineBuffer;
use ish::ls;
use ish::path as exec;
use ish::prompt;
use ish::render;
use ish::term::TermWriter;

// ---------------------------------------------------------------------------
// Line buffer benchmarks
// ---------------------------------------------------------------------------

fn bench_line_buffer(c: &mut Criterion) {
    let mut group = c.benchmark_group("line_buffer");

    group.bench_function("insert_100_chars", |b| {
        b.iter(|| {
            let mut lb = LineBuffer::new();
            for c in "echo hello world this is a long command line with many characters and words"
                .chars()
            {
                lb.insert_char(c);
            }
            black_box(lb.text());
        });
    });

    group.bench_function("insert_delete_cycle", |b| {
        b.iter(|| {
            let mut lb = LineBuffer::new();
            for _ in 0..50 {
                lb.insert_str("hello ");
                lb.delete_back();
                lb.delete_back();
            }
            black_box(lb.text());
        });
    });

    group.bench_function("word_navigation", |b| {
        let mut lb = LineBuffer::new();
        lb.set("the quick brown fox jumps over the lazy dog and more words here");
        b.iter(|| {
            lb.move_home();
            for _ in 0..10 {
                lb.move_word_right();
            }
            for _ in 0..10 {
                lb.move_word_left();
            }
            black_box(lb.cursor());
        });
    });

    group.bench_function("kill_yank_cycle", |b| {
        b.iter(|| {
            let mut lb = LineBuffer::new();
            lb.set("hello world foo bar baz");
            lb.kill_word_back();
            lb.move_home();
            lb.yank();
            lb.kill_to_end();
            lb.move_end();
            lb.yank();
            black_box(lb.text());
        });
    });

    group.bench_function("utf8_insert", |b| {
        b.iter(|| {
            let mut lb = LineBuffer::new();
            for c in "日本語のテストです。これは長い文字列のテストです。".chars()
            {
                lb.insert_char(c);
            }
            black_box(lb.text());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Synthetic history generator — reproducible, no dependency on real files
// ---------------------------------------------------------------------------

/// Generate 45k realistic shell commands (~45 bytes avg, varied patterns).
fn synthetic_history_45k() -> Vec<String> {
    let templates = [
        "git commit -m 'fix issue #{}' --no-verify",
        "git checkout -b feature/task-{}",
        "cargo test --package ish -- test_{}",
        "rg '{}' src/ --type rust",
        "/opt/homebrew/bin/git diff HEAD~{}",
        "cd ~/projects/project-{}/src",
        "make -j{} build",
        "docker compose up -d service-{}",
        "ssh deploy@prod-{}.example.com",
        "curl -s https://api.example.com/v{}/status",
        "python3 scripts/migrate_{}.py --dry-run",
        "npm run build -- --env=staging-{}",
        "kubectl get pods -n namespace-{}",
        "vim src/module_{}/lib.rs",
        "tar czf backup-{}.tar.gz data/",
    ];
    (0..45_000)
        .map(|i| {
            let t = templates[i % templates.len()];
            t.replace("{}", &i.to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// History benchmarks
// ---------------------------------------------------------------------------

fn bench_history(c: &mut Criterion) {
    let mut group = c.benchmark_group("history");

    let entries_45k = synthetic_history_45k();
    let history = History::from_entries(entries_45k.clone());

    group.bench_function("prefix_search_45k", |b| {
        b.iter(|| black_box(history.prefix_search("git commit", 0)));
    });

    group.bench_function("fuzzy_search_45k", |b| {
        b.iter(|| black_box(history.fuzzy_search("gco")));
    });

    group.bench_function("fuzzy_search_miss_45k", |b| {
        b.iter(|| black_box(history.fuzzy_search("zzzznotfound")));
    });

    group.bench_function("fuzzy_search_empty_45k", |b| {
        b.iter(|| {
            let results = history.fuzzy_search("");
            black_box(results.len());
        });
    });

    group.bench_function("fuzzy_search_into_45k", |b| {
        let mut results = Vec::with_capacity(200);
        history.fuzzy_search_into("gco", &mut results, 200, "");
        b.iter(|| {
            history.fuzzy_search_into("gco", &mut results, 200, "");
            black_box(&results);
        });
    });

    group.bench_function("fuzzy_search_into_pwd_45k", |b| {
        let mut results = Vec::with_capacity(200);
        history.fuzzy_search_into("gco", &mut results, 200, "myproject");
        b.iter(|| {
            history.fuzzy_search_into("gco", &mut results, 200, "myproject");
            black_box(&results);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Completion benchmarks
// ---------------------------------------------------------------------------

fn bench_completion(c: &mut Criterion) {
    let mut group = c.benchmark_group("completion");

    // Grid layout computation
    let mut comp100 = complete::Completions::new();
    for i in 0..100 {
        comp100.push(&format!("file_{i:03}.rs"), i % 5 == 0, false, i % 10 == 0);
    }

    group.bench_function("compute_grid_100_entries", |b| {
        b.iter(|| black_box(complete::compute_grid(&comp100.entries, 120)));
    });

    group.bench_function("compute_grid_100_narrow", |b| {
        b.iter(|| black_box(complete::compute_grid(&comp100.entries, 40)));
    });

    // Filesystem completion (real I/O — measures readdir performance)
    group.bench_function("complete_path_cwd", |b| {
        b.iter(|| black_box(complete::complete_path("./src/", false)));
    });

    // Sort: large directory (100 entries)
    group.bench_function("sort_100_filenames", |b| {
        b.iter(|| {
            let mut comp = complete::Completions::new();
            for i in (0..100).rev() {
                comp.push(&format!("file_{i:03}.rs"), false, false, false);
            }
            comp.sort_entries();
            black_box(&comp);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Prompt render (full — the before-every-command hot path)
// ---------------------------------------------------------------------------

fn bench_prompt_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("prompt_render");

    // Full render in a git repo (this repo)
    group.bench_function("full_in_git_repo", |b| {
        let mut p = prompt::Prompt::new();
        b.iter(|| black_box(p.render(0)));
    });

    // Render with error status
    group.bench_function("full_error_status", |b| {
        let mut p = prompt::Prompt::new();
        b.iter(|| black_box(p.render(1)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Interactive render hot paths (VT repaint only)
// ---------------------------------------------------------------------------

fn make_line(s: &str) -> LineBuffer {
    let mut lb = LineBuffer::new();
    for ch in s.chars() {
        lb.insert_char(ch);
    }
    lb
}

fn sample_history() -> History {
    History::from_entries(vec![
        "gh auth login".to_string(),
        "gh api repos/openai/openai/contents".to_string(),
        "gh api user".to_string(),
        "gh pr status".to_string(),
        "gh api rate_limit".to_string(),
        "gh api notifications".to_string(),
        "gh api orgs/openai/repos".to_string(),
        "gh api repos/openai/openai/pulls".to_string(),
        "gh api repos/openai/openai/issues".to_string(),
        "gh api repos/openai/openai/actions/runs".to_string(),
        "gh api repos/openai/openai/releases".to_string(),
        "gh api repos/openai/openai/branches".to_string(),
    ])
}

fn sample_completion_state(cols: u16) -> complete::CompletionState {
    let mut comp = complete::Completions::new();
    for entry in [
        "aaa.txt", "aab.txt", "aac.txt", "aad.txt", "aae.txt", "aaf.txt", "aag.txt", "aah.txt",
    ] {
        comp.push(entry, false, false, false);
    }
    let (grid_cols, grid_rows) = complete::compute_grid(&comp.entries, cols);
    complete::CompletionState {
        comp,
        selected: 0,
        cols: grid_cols,
        rows: grid_rows,
        scroll: 0,
        term_cols: cols,
        dir_prefix: String::new(),
        in_quote: false,
    }
}

fn sample_file_picker_entries() -> Vec<(usize, String)> {
    vec![
        (0, "abc1".to_string()),
        (0, "abc2".to_string()),
        (0, "abc3".to_string()),
        (0, "abd1".to_string()),
        (0, "abd2".to_string()),
    ]
}

fn bench_interactive_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("interactive_render");

    group.bench_function("prompt_initial_single_line", |b| {
        let line = make_line("gh api repos/openai/openai/issues");
        let opts = render::RenderOpts::default();
        let mut tw = TermWriter::new();
        b.iter(|| {
            tw.clear_buffer();
            let info = render::render_line(
                &mut tw,
                "$ ",
                2,
                &line,
                80,
                render::RenderedRegion::default(),
                &opts,
            );
            black_box(info);
            black_box(&tw);
        });
    });

    group.bench_function("prompt_rerender_same_rows", |b| {
        let line_a = make_line("gh api");
        let line_b = make_line("gh api?");
        let opts = render::RenderOpts::default();
        let mut tw = TermWriter::new();
        let mut prev = render::render_line(
            &mut tw,
            "$ ",
            2,
            &line_a,
            80,
            render::RenderedRegion::default(),
            &opts,
        );
        tw.clear_buffer();
        let mut toggle = false;
        b.iter(|| {
            let line = if toggle { &line_a } else { &line_b };
            toggle = !toggle;
            tw.clear_buffer();
            prev = render::render_line(&mut tw, "$ ", 2, line, 80, prev, &opts);
            black_box(prev);
            black_box(&tw);
        });
    });

    group.bench_function("prompt_rerender_grow_rows", |b| {
        let line_short = make_line("gh api");
        let line_long = make_line("gh api repos/openai/openai/issues/123/comments");
        let opts = render::RenderOpts::default();
        let mut tw = TermWriter::new();
        let mut prev = render::render_line(
            &mut tw,
            "$ ",
            2,
            &line_short,
            20,
            render::RenderedRegion::default(),
            &opts,
        );
        tw.clear_buffer();
        let mut toggle = false;
        b.iter(|| {
            let line = if toggle { &line_short } else { &line_long };
            toggle = !toggle;
            tw.clear_buffer();
            prev = render::render_line(&mut tw, "$ ", 2, line, 20, prev, &opts);
            black_box(prev);
            black_box(&tw);
        });
    });

    group.bench_function("history_pager_query_edit", |b| {
        let history = sample_history();
        let query_a = "gh ap";
        let query_b = "gh api";
        let mut tw = TermWriter::new();
        let mut cache = render::HistoryPagerCache::default();
        let mut prev = render::render_history_pager_cached(
            &mut tw,
            query_a,
            &history.fuzzy_search(query_a),
            &history,
            0,
            24,
            20,
            query_a.len(),
            render::RenderedRegion::default(),
            &mut cache,
        );
        tw.clear_buffer();
        let mut toggle = false;
        b.iter(|| {
            let query = if toggle { query_a } else { query_b };
            toggle = !toggle;
            let matches = history.fuzzy_search(query);
            tw.clear_buffer();
            prev = render::render_history_pager_cached(
                &mut tw,
                query,
                &matches,
                &history,
                0,
                24,
                20,
                query.len(),
                prev,
                &mut cache,
            );
            black_box(prev);
            black_box(&tw);
        });
    });

    group.bench_function("history_pager_selection_move", |b| {
        let history = sample_history();
        let query = "gh api";
        let matches = history.fuzzy_search(query);
        let mut tw = TermWriter::new();
        let mut cache = render::HistoryPagerCache::default();
        let mut selected = 0usize;
        let mut prev = render::render_history_pager_cached(
            &mut tw,
            query,
            &matches,
            &history,
            selected,
            24,
            20,
            query.len(),
            render::RenderedRegion::default(),
            &mut cache,
        );
        tw.clear_buffer();
        b.iter(|| {
            selected = if selected == 0 { 1 } else { 0 };
            tw.clear_buffer();
            prev = render::render_history_pager_cached(
                &mut tw,
                query,
                &matches,
                &history,
                selected,
                24,
                20,
                query.len(),
                prev,
                &mut cache,
            );
            black_box(prev);
            black_box(selected);
            black_box(&tw);
        });
    });

    group.bench_function("file_picker_query_edit", |b| {
        let all_entries = sample_file_picker_entries();
        let filtered_a = vec![0usize, 1, 2, 3, 4];
        let filtered_b = vec![0usize, 1, 2];
        let mut tw = TermWriter::new();
        let mut prev = render::render_file_picker(
            &mut tw,
            "ab",
            &all_entries,
            &filtered_a,
            0,
            24,
            20,
            2,
            false,
            false,
            render::RenderedRegion::default(),
        );
        tw.clear_buffer();
        let mut toggle = false;
        b.iter(|| {
            let (query, filtered, cursor) = if toggle {
                ("ab", &filtered_a, 2)
            } else {
                ("abc", &filtered_b, 3)
            };
            toggle = !toggle;
            tw.clear_buffer();
            prev = render::render_file_picker(
                &mut tw,
                query,
                &all_entries,
                filtered,
                0,
                24,
                20,
                cursor,
                false,
                false,
                prev,
            );
            black_box(prev);
            black_box(&tw);
        });
    });

    group.bench_function("completion_repaint_navigation", |b| {
        let line = make_line("echo aa");
        let opts = render::RenderOpts::default();
        let mut state = sample_completion_state(20);
        let mut tw = TermWriter::new();
        let mut info = render::render_line(
            &mut tw,
            "$ ",
            2,
            &line,
            20,
            render::RenderedRegion::default(),
            &opts,
        );
        render::render_completions(&mut tw, &state, info, true);
        tw.clear_buffer();
        b.iter(|| {
            state.selected = (state.selected + 1) % state.comp.entries.len();
            tw.clear_buffer();
            info = render::render_line(&mut tw, "$ ", 2, &line, 20, info, &opts);
            render::render_completions(&mut tw, &state, info, false);
            black_box(info);
            black_box(state.selected);
            black_box(&tw);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// History add (dedup on every command)
// ---------------------------------------------------------------------------

fn bench_history_add(c: &mut Criterion) {
    let mut group = c.benchmark_group("history_add");

    let entries_45k = synthetic_history_45k();

    group.bench_function("add_new_45k", |b| {
        b.iter_batched(
            || History::from_entries(entries_45k.clone()),
            |mut h| {
                h.add("brand_new_command_xyz --flag");
                h
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("add_dup_45k", |b| {
        let mid = entries_45k[entries_45k.len() / 2].clone();
        b.iter_batched(
            || History::from_entries(entries_45k.clone()),
            |mut h| {
                h.add(&mid);
                h
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// ls builtin (real I/O — the most-used builtin)
// ---------------------------------------------------------------------------

fn bench_ls(c: &mut Criterion) {
    let mut group = c.benchmark_group("ls");

    // List the repo's src/ directory (~17 files)
    group.bench_function("src_dir", |b| {
        b.iter(|| black_box(ls::list_dir("./src")));
    });

    // List the repo root
    group.bench_function("repo_root", |b| {
        b.iter(|| black_box(ls::list_dir(".")));
    });

    // Single file
    group.bench_function("single_file", |b| {
        b.iter(|| black_box(ls::list_dir("Cargo.toml")));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// PATH lookup (every external command)
// ---------------------------------------------------------------------------

fn bench_path_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("path_lookup");

    // scan_path: find ls (typical case)
    group.bench_function("scan_path_ls", |b| {
        b.iter(|| black_box(exec::scan_path("ls")));
    });

    // scan_path: not found (worst case — scans all dirs)
    group.bench_function("scan_not_found", |b| {
        b.iter(|| black_box(exec::scan_path("nonexistent_command_xyz")));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Completion: realistic filesystem
// ---------------------------------------------------------------------------

fn bench_completion_fs(c: &mut Criterion) {
    let mut group = c.benchmark_group("completion_fs");

    // Complete in project root (mixed files and dirs)
    group.bench_function("complete_root", |b| {
        b.iter(|| black_box(complete::complete_path("./", false)));
    });

    // Complete with prefix filter
    group.bench_function("complete_with_prefix", |b| {
        b.iter(|| black_box(complete::complete_path("./src/l", false)));
    });

    // Dirs only (cd completion)
    group.bench_function("complete_dirs_only", |b| {
        b.iter(|| black_box(complete::complete_path("./", true)));
    });

    // /usr/bin — large directory, stress test
    group.bench_function("complete_usr_bin", |b| {
        b.iter(|| black_box(complete::complete_path("/usr/bin/z", false)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Startup (time-to-prompt) benchmarks
// ---------------------------------------------------------------------------

fn bench_startup(c: &mut Criterion) {
    let mut group = c.benchmark_group("startup");

    // Full cold startup in a git repo (this repo).
    // Skips terminal setup (requires real tty) and signal::init (creates
    // pipe per call). Everything else mirrors main() order.
    // denv::init() is now deferred to first cd — not part of startup.
    group.bench_function("git_repo", |b| {
        b.iter(|| {
            let _history = black_box(History::load());

            let mut aliases = AliasMap::new();
            ish::config::load(&mut aliases, &mut epsh::eval::Shell::new(), None);

            // Fresh prompt — git cache is cold
            let mut p = prompt::Prompt::new();
            black_box(p.render(0));
        });
    });

    // Full cold startup outside a git repo (/tmp)
    group.bench_function("no_git", |b| {
        let original_dir = std::env::current_dir().ok();
        let _ = std::env::set_current_dir("/tmp");

        b.iter(|| {
            let _history = black_box(History::load());

            let mut aliases = AliasMap::new();
            ish::config::load(&mut aliases, &mut epsh::eval::Shell::new(), None);

            let mut p = prompt::Prompt::new();
            black_box(p.render(0));
        });

        if let Some(d) = original_dir {
            let _ = std::env::set_current_dir(d);
        }
    });

    // Individual startup components
    group.bench_function("history_load", |b| {
        b.iter(|| black_box(History::load()));
    });

    // Synthetic: from_entries (measures arena+hash construction for 45k)
    {
        let entries = synthetic_history_45k();
        group.bench_function("history_from_entries_45k", |b| {
            b.iter(|| black_box(History::from_entries(entries.clone())));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Autosuggestion + command coloring benchmarks
// ---------------------------------------------------------------------------

fn bench_autosuggestion(c: &mut Criterion) {
    let mut group = c.benchmark_group("autosuggestion");

    let entries = synthetic_history_45k();
    let history = History::from_entries(entries);

    // Typical case: user typed a few chars, suggestion found quickly
    group.bench_function("prefix_search_hit", |b| {
        b.iter(|| {
            let entry = history.prefix_search("git commit", 0);
            let suffix = entry.and_then(|e| e.strip_prefix("git commit"));
            black_box(suffix);
        });
    });

    // Worst case: no match, scans all 45k entries
    group.bench_function("prefix_search_miss", |b| {
        b.iter(|| {
            black_box(history.prefix_search("zzzznotfound", 0));
        });
    });

    group.finish();
}

fn bench_command_coloring(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_coloring");

    let mut cache = exec::PathCache::new();

    // First call rebuilds the cache — measure that separately
    group.bench_function("path_cache_rebuild", |b| {
        b.iter(|| {
            let mut fresh = exec::PathCache::new();
            black_box(fresh.contains("ls"));
        });
    });

    // Warm the cache, then measure O(1) lookups
    cache.contains("ls"); // force rebuild

    group.bench_function("path_cache_hit", |b| {
        b.iter(|| black_box(cache.contains("ls")));
    });

    group.bench_function("path_cache_miss", |b| {
        b.iter(|| black_box(cache.contains("zzzznotacommand")));
    });

    // Full per-keystroke cost: builtin check + alias check + path cache
    let aliases = AliasMap::new();
    group.bench_function("full_cmd_check", |b| {
        b.iter(|| {
            let cmd = "git";
            let valid =
                ish::builtin::is_builtin(cmd) || aliases.get(cmd).is_some() || cache.contains(cmd);
            black_box(valid);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------

fn fast_config() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(1))
}

// ---------------------------------------------------------------------------
// Finder benchmarks (real filesystem searches against this repo)
// ---------------------------------------------------------------------------

fn bench_finder(c: &mut Criterion) {
    let mut group = c.benchmark_group("finder");

    group.bench_function("find_rs_normal", |b| {
        b.iter(|| black_box(ish::finder::find(".", "rs", false, 100)));
    });

    group.bench_function("find_main_normal", |b| {
        b.iter(|| black_box(ish::finder::find(".", "main", false, 100)));
    });

    group.bench_function("find_ish_normal", |b| {
        b.iter(|| black_box(ish::finder::find(".", "ish", false, 1000)));
    });

    group.bench_function("find_ish_hidden", |b| {
        b.iter(|| black_box(ish::finder::find(".", "ish", true, 1000)));
    });

    group.bench_function("find_all_hidden", |b| {
        b.iter(|| black_box(ish::finder::find(".", "", true, 1000)));
    });

    group.bench_function("find_all_normal", |b| {
        b.iter(|| black_box(ish::finder::find(".", "", false, 1000)));
    });

    group.finish();
}

criterion_group!(
    name = benches;
    config = fast_config();
    targets =
        bench_startup,
        bench_line_buffer,
        bench_history,
        bench_completion,
        bench_prompt_render,
        bench_interactive_render,
        bench_history_add,
        bench_ls,
        bench_path_lookup,
        bench_completion_fs,
        bench_autosuggestion,
        bench_command_coloring,
        bench_finder,
);
criterion_main!(benches);
