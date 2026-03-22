//! Benchmark harness for ish shell.
//!
//! Tracks wall time (criterion) and heap allocations (counting allocator).
//! Run: `cargo bench`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};

use ish::alias::AliasMap;
use ish::complete;
use ish::error::Error;
use ish::exec;
use ish::expand;
use ish::history::History;
use ish::line::LineBuffer;
use ish::ls;
use ish::parse;
use ish::prompt;

// ---------------------------------------------------------------------------
// Counting allocator — tracks allocations and live bytes
// ---------------------------------------------------------------------------

struct CountingAlloc;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

fn update_peak() {
    let live = LIVE_BYTES.load(Relaxed);
    let mut peak = PEAK_BYTES.load(Relaxed);
    while live > peak {
        match PEAK_BYTES.compare_exchange_weak(peak, live, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(actual) => peak = actual,
        }
    }
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Relaxed);
        LIVE_BYTES.fetch_add(layout.size(), Relaxed);
        update_peak();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Relaxed);
        if new_size > layout.size() {
            let delta = new_size - layout.size();
            ALLOC_BYTES.fetch_add(delta, Relaxed);
            LIVE_BYTES.fetch_add(delta, Relaxed);
        } else {
            let delta = layout.size() - new_size;
            LIVE_BYTES.fetch_sub(delta, Relaxed);
        }
        update_peak();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn reset_alloc_counters() {
    ALLOC_COUNT.store(0, Relaxed);
    ALLOC_BYTES.store(0, Relaxed);
    PEAK_BYTES.store(LIVE_BYTES.load(Relaxed), Relaxed);
}

fn alloc_count() -> usize {
    ALLOC_COUNT.load(Relaxed)
}

fn alloc_bytes() -> usize {
    ALLOC_BYTES.load(Relaxed)
}

struct AllocStats {
    count: usize,
    bytes: usize,
}

impl std::fmt::Display for AllocStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn human(n: usize) -> String {
            if n >= 1_048_576 {
                format!("{:.1} MiB", n as f64 / 1_048_576.0)
            } else if n >= 1024 {
                format!("{:.1} KiB", n as f64 / 1024.0)
            } else {
                format!("{n} B")
            }
        }
        write!(f, "{} allocs, {}", self.count, human(self.bytes))
    }
}

fn measure_allocs<F: FnOnce()>(f: F) -> AllocStats {
    reset_alloc_counters();
    f();
    AllocStats {
        count: alloc_count(),
        bytes: alloc_bytes(),
    }
}

// ---------------------------------------------------------------------------
// Parse benchmarks
// ---------------------------------------------------------------------------

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");

    group.bench_function("simple_command", |b| {
        b.iter(|| black_box(parse::parse("ls -la")));
    });

    group.bench_function("pipeline_3_stage", |b| {
        b.iter(|| black_box(parse::parse("find . -name '*.rs' | grep main | wc -l")));
    });

    group.bench_function("complex_chain", |b| {
        b.iter(|| {
            black_box(parse::parse(
                "make build && ./test --all || echo fail ; echo done",
            ))
        });
    });

    group.bench_function("heavy_redirects", |b| {
        b.iter(|| black_box(parse::parse("cmd < input > output 2> err >> append &> all")));
    });

    group.bench_function("quoted_strings", |b| {
        b.iter(|| {
            black_box(parse::parse(
                r#"echo "hello $USER" 'literal $HOME' "escaped \" quote""#,
            ))
        });
    });

    // Long command line — measures parser scaling
    let long_cmd = (0..100)
        .map(|i| format!("arg{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let long_cmd_str = format!("echo {long_cmd}");
    group.bench_function("100_args", |b| {
        b.iter(|| black_box(parse::parse(&long_cmd_str)));
    });

    group.bench_function("needs_continuation", |b| {
        b.iter(|| {
            black_box(parse::needs_continuation("ls |"));
            black_box(parse::needs_continuation("echo 'unclosed"));
            black_box(parse::needs_continuation("ls -la"));
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Expand benchmarks
// ---------------------------------------------------------------------------

fn bench_expand(c: &mut Criterion) {
    let mut group = c.benchmark_group("expand");

    let mut no_subst = |_: &str| -> Result<String, Error> { Ok(String::new()) };

    group.bench_function("tilde_expand", |b| {
        b.iter(|| {
            black_box(expand::expand_word(
                "~/src/project",
                "/home/user",
                &mut no_subst,
                0,
            ))
        });
    });

    group.bench_function("variable_expand", |b| {
        unsafe { std::env::set_var("ISH_BENCH_VAR", "benchmark_value") };
        b.iter(|| {
            black_box(expand::expand_word(
                "$ISH_BENCH_VAR",
                "/home/user",
                &mut no_subst,
                0,
            ))
        });
    });

    group.bench_function("no_expansion_needed", |b| {
        b.iter(|| {
            black_box(expand::expand_word(
                "simple_word",
                "/home/user",
                &mut no_subst,
                0,
            ))
        });
    });

    // Multi-word expansion
    let words: Vec<String> = (0..50).map(|i| format!("word{i}")).collect();
    group.bench_function("expand_argv_50_words", |b| {
        b.iter(|| black_box(expand::expand_argv(&words, "/home/user", &mut no_subst, 0)));
    });

    group.finish();
}

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

    // Subsequence match only — measures the optimal alignment overhead
    group.bench_function("subsequence_match_hit", |b| {
        let query: Vec<char> = "gco".chars().collect();
        b.iter(|| black_box(ish::history::subsequence_match(&query, "git checkout main")));
    });

    group.bench_function("subsequence_match_miss", |b| {
        let query: Vec<char> = "zzz".chars().collect();
        b.iter(|| black_box(ish::history::subsequence_match(&query, "git checkout main")));
    });

    // Optimal alignment: query chars appear early AND late — exercises the
    // backward pass from two endpoints.
    group.bench_function("subsequence_match_alignment", |b| {
        let query: Vec<char> = "test".chars().collect();
        b.iter(|| {
            black_box(ish::history::subsequence_match(
                &query,
                "the best integration test suite",
            ))
        });
    });

    // Score match in isolation — measures scoring overhead per result
    group.bench_function("score_match_contiguous", |b| {
        let mut positions = [0u16; 32];
        positions[0] = 10;
        positions[1] = 11;
        positions[2] = 12;
        positions[3] = 13;
        positions[4] = 14;
        positions[5] = 15;
        b.iter(|| {
            black_box(ish::history::score_match(
                &positions,
                6,
                "cd /home/target/release/bin",
                "myproject",
            ))
        });
    });

    group.bench_function("score_match_scattered", |b| {
        let mut positions = [0u16; 32];
        positions[0] = 3;
        positions[1] = 12;
        positions[2] = 18;
        b.iter(|| {
            black_box(ish::history::score_match(
                &positions,
                3,
                "git remote add origin https://example.com",
                "",
            ))
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

    let mut comp5 = complete::Completions::new();
    for i in 0..5 {
        comp5.push(&format!("f{i}.rs"), false, false, false);
    }

    group.bench_function("compute_grid_5_entries", |b| {
        b.iter(|| black_box(complete::compute_grid(&comp5.entries, 80)));
    });

    // Filesystem completion (real I/O — measures readdir performance)
    group.bench_function("complete_path_cwd", |b| {
        b.iter(|| black_box(complete::complete_path("./src/", false)));
    });

    // Sort benchmark: isolated sort of typical directory listing (~17 entries)
    group.bench_function("sort_17_filenames", |b| {
        // Realistic filenames from a Rust project src/ dir
        let names = [
            "main.rs",
            "lib.rs",
            "prompt.rs",
            "render.rs",
            "complete.rs",
            "history.rs",
            "parse.rs",
            "expand.rs",
            "exec.rs",
            "builtin.rs",
            "line.rs",
            "term.rs",
            "input.rs",
            "signal.rs",
            "config.rs",
            "alias.rs",
            "error.rs",
        ];
        b.iter(|| {
            let mut comp = complete::Completions::new();
            // Push in reverse to force real sorting work
            for n in names.iter().rev() {
                comp.push(n, false, false, false);
            }
            comp.sort_entries();
            black_box(&comp);
        });
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
// Prompt benchmarks
// ---------------------------------------------------------------------------

fn bench_prompt(c: &mut Criterion) {
    let mut group = c.benchmark_group("prompt");

    group.bench_function("shorten_pwd_deep", |b| {
        b.iter(|| {
            black_box(prompt::shorten_pwd(
                "/home/user/projects/rust/ish/src/main.rs",
                "/home/user",
            ))
        });
    });

    group.bench_function("shorten_pwd_home", |b| {
        b.iter(|| black_box(prompt::shorten_pwd("/home/user", "/home/user")));
    });

    group.bench_function("shorten_pwd_outside_home", |b| {
        b.iter(|| {
            black_box(prompt::shorten_pwd(
                "/var/log/syslog/messages",
                "/home/user",
            ))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// End-to-end: parse → expand (the Enter-key hot path)
// ---------------------------------------------------------------------------

fn bench_parse_expand(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_expand");
    let mut no_subst = |_: &str| -> Result<String, Error> { Ok(String::new()) };

    unsafe { std::env::set_var("ISH_BENCH_HOME", "/home/user") };
    unsafe { std::env::set_var("ISH_BENCH_DIR", "/tmp/build") };

    // Typical: simple command with tilde
    group.bench_function("simple_with_tilde", |b| {
        b.iter(|| {
            let cmd = parse::parse("ls ~/projects").unwrap();
            let argv = &cmd.segments[0].0.commands[0].cmd.argv;
            black_box(expand::expand_argv(argv, "/home/user", &mut no_subst, 0))
        });
    });

    // Typical: pipeline with variables
    group.bench_function("pipeline_with_vars", |b| {
        b.iter(|| {
            let cmd = parse::parse("grep -r $ISH_BENCH_DIR | sort | head -20").unwrap();
            let argv = &cmd.segments[0].0.commands[0].cmd.argv;
            black_box(expand::expand_argv(argv, "/home/user", &mut no_subst, 0))
        });
    });

    // Realistic: git workflow command
    group.bench_function("git_workflow", |b| {
        b.iter(|| {
            let cmd = parse::parse(
                "git add -A && git commit -m 'fix: resolve issue' && git push origin main",
            )
            .unwrap();
            for (pipeline, _) in &cmd.segments {
                for pcmd in &pipeline.commands {
                    let _ = black_box(expand::expand_argv(
                        &pcmd.cmd.argv,
                        "/home/user",
                        &mut no_subst,
                        0,
                    ));
                }
            }
        });
    });

    // Worst case: many words with mixed expansion
    group.bench_function("mixed_expansion_20_words", |b| {
        b.iter(|| {
            let cmd = parse::parse(
                r#"echo ~/file $ISH_BENCH_DIR "quoted $ISH_BENCH_HOME" plain 'literal $X' a b c d e f g h i j k l m n"#,
            )
            .unwrap();
            let argv = &cmd.segments[0].0.commands[0].cmd.argv;
            black_box(expand::expand_argv(argv, "/home/user", &mut no_subst, 0))
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

    // display_len computation
    group.bench_function("display_len", |b| {
        let p = prompt::Prompt::new();
        let sample = "\x1b[38;5;10muser@host ~/d/ish\x1b[0m \x1b[38;5;1m*\x1b[0m master $ ";
        b.iter(|| black_box(p.display_len(sample)));
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
// Alias lookup
// ---------------------------------------------------------------------------

fn bench_alias(c: &mut Criterion) {
    let mut group = c.benchmark_group("alias");

    let mut aliases = AliasMap::new();
    aliases.set("g".into(), vec!["git".into()]);
    aliases.set("gc".into(), vec!["git".into(), "commit".into()]);
    aliases.set("ll".into(), vec!["ls".into(), "-la".into()]);
    aliases.set(
        "deploy".into(),
        vec![
            "make".into(),
            "build".into(),
            "&&".into(),
            "./deploy.sh".into(),
        ],
    );
    for i in 0..50 {
        aliases.set(format!("alias_{i}"), vec![format!("cmd_{i}")]);
    }

    group.bench_function("hit", |b| {
        b.iter(|| black_box(aliases.get("gc")));
    });

    group.bench_function("miss", |b| {
        b.iter(|| black_box(aliases.get("nonexistent")));
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
// denv output parsing
// ---------------------------------------------------------------------------

fn bench_denv(c: &mut Criterion) {
    let mut group = c.benchmark_group("denv");

    // Typical denv export output: 5-10 vars
    let small_output = "export FOO='bar';\nexport BAZ='qux';\nexport PATH='/usr/bin:/bin';\n";
    group.bench_function("parse_small_output", |b| {
        b.iter(|| black_box(ish::denv::apply_bash_output_bench(small_output)));
    });

    // Larger: 50 vars
    let large_output: String = (0..50)
        .map(|i| format!("export DENV_BENCH_{i}='value_{i}';"))
        .collect::<Vec<_>>()
        .join("\n");
    group.bench_function("parse_50_vars", |b| {
        b.iter(|| black_box(ish::denv::apply_bash_output_bench(&large_output)));
    });

    // Mixed export + unset
    let mixed = "export A='1';\nexport B='two';\nunset C;\nexport D='it'\\''s here';\nunset E;\n";
    group.bench_function("parse_mixed", |b| {
        b.iter(|| black_box(ish::denv::apply_bash_output_bench(mixed)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Allocation audit
// ---------------------------------------------------------------------------

fn bench_alloc_audit(c: &mut Criterion) {
    let mut group = c.benchmark_group("alloc_audit");

    // One-shot allocation reports
    {
        eprintln!();
        eprintln!("  -- allocation audit --");

        let stats = measure_allocs(|| {
            let _ = black_box(parse::parse("find . -name '*.rs' | grep main | wc -l"));
        });
        eprintln!("  [alloc] parse_pipeline:            {stats}");

        let stats = measure_allocs(|| {
            let mut no_subst = |_: &str| -> Result<String, Error> { Ok(String::new()) };
            let _ = black_box(expand::expand_word(
                "~/src/project",
                "/home/user",
                &mut no_subst,
                0,
            ));
        });
        eprintln!("  [alloc] expand_tilde:              {stats}");

        let stats = measure_allocs(|| {
            let mut lb = LineBuffer::new();
            for c in "echo hello world this is a test".chars() {
                lb.insert_char(c);
            }
            black_box(lb.text());
        });
        eprintln!("  [alloc] line_buffer_30_chars:      {stats}");

        let entries: Vec<String> = (0..1000)
            .map(|i| format!("command_{i} --arg={i}"))
            .collect();
        let history = History::from_entries(entries);
        let stats = measure_allocs(|| {
            black_box(history.fuzzy_search("cmd99"));
        });
        eprintln!("  [alloc] fuzzy_search_1k:           {stats}");

        // Warm: reuse pre-allocated Vec — should be 0 allocs
        {
            let mut results = Vec::with_capacity(200);
            history.fuzzy_search_into("cmd99", &mut results, 200, "");
            let stats = measure_allocs(|| {
                history.fuzzy_search_into("cmd99", &mut results, 200, "");
                black_box(&results);
            });
            eprintln!("  [alloc] fuzzy_search_warm:         {stats}");
        }

        let stats = measure_allocs(|| {
            black_box(prompt::shorten_pwd(
                "/home/user/projects/rust/ish/src",
                "/home/user",
            ));
        });
        eprintln!("  [alloc] shorten_pwd:               {stats}");

        // --- New practical operation audits ---

        let stats = measure_allocs(|| {
            let cmd = parse::parse("grep -r ~/src | sort | head -20").unwrap();
            let mut no_subst = |_: &str| -> Result<String, Error> { Ok(String::new()) };
            let argv = &cmd.segments[0].0.commands[0].cmd.argv;
            let _ = black_box(expand::expand_argv(argv, "/home/user", &mut no_subst, 0));
        });
        eprintln!("  [alloc] parse_expand_pipeline:     {stats}");

        // Cold start (includes Prompt::new allocations)
        let stats = measure_allocs(|| {
            let mut p = prompt::Prompt::new();
            black_box(p.render(0));
        });
        eprintln!("  [alloc] prompt_render_cold:         {stats}");

        // Warm: second render reuses all buffers — should be 0 allocs
        {
            let mut p = prompt::Prompt::new();
            let mut buf = String::with_capacity(128);
            let pwd = std::env::var("PWD").unwrap_or_default();
            p.render_into(&mut buf, 0, &pwd, false); // warm up caches
            let stats = measure_allocs(|| {
                p.render_into(&mut buf, 0, &pwd, false);
                black_box(&buf);
            });
            eprintln!("  [alloc] prompt_render_warm:         {stats}");
        }

        let stats = measure_allocs(|| {
            let entries: Vec<String> = (0..1000)
                .map(|i| format!("command_{i} --arg={i}"))
                .collect();
            let mut h = History::from_entries(entries);
            h.add("brand_new_command --flag");
        });
        eprintln!("  [alloc] history_add_1k:            {stats}");

        let stats = measure_allocs(|| {
            let _ = black_box(exec::scan_path("ls"));
        });
        eprintln!("  [alloc] scan_path_ls:              {stats}");

        let stats = measure_allocs(|| {
            let _ = black_box(complete::complete_path("./src/", false));
        });
        eprintln!("  [alloc] complete_path_src:         {stats}");

        // Warm: reuse pre-allocated Completions — should be 0 allocs
        {
            let mut comp = complete::Completions::with_capacity(2048, 64);
            complete::complete_path_into("./src/", false, &mut comp);
            let stats = measure_allocs(|| {
                comp.clear();
                complete::complete_path_into("./src/", false, &mut comp);
                black_box(&comp);
            });
            eprintln!("  [alloc] complete_path_warm:        {stats}");
        }

        let stats = measure_allocs(|| {
            let _ = black_box(ish::denv::apply_bash_output_bench(
                "export A='1';\nexport B='two';\nunset C;\n",
            ));
        });
        eprintln!("  [alloc] denv_parse_3_directives:   {stats}");

        // Finder: sync find
        let stats = measure_allocs(|| {
            black_box(ish::finder::find(".", "main", false, 100));
        });
        eprintln!("  [alloc] finder_normal:              {stats}");

        // Finder: async drain
        {
            let stats = measure_allocs(|| {
                let handle = ish::finder::find_async(".", false);
                let mut buf = Vec::new();
                loop {
                    handle.drain_into(&mut buf);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    handle.drain_into(&mut buf);
                    if buf.len() > 10 {
                        break;
                    }
                }
                black_box(buf.len());
            });
            eprintln!("  [alloc] finder_async_drain:         {stats}");
        }

        // Finder: client-side filter (the hot path on each keystroke)
        {
            let entries: Vec<(usize, String)> = (0..500)
                .map(|i| (i % 5, format!("src/module_{i}/main.rs")))
                .collect();
            let mut filtered = Vec::new();
            let mut selected = 0usize;
            // Warm the Vec
            ish::finder::filter_entries_pub("mai", &entries, &mut filtered, &mut selected);
            let stats = measure_allocs(|| {
                ish::finder::filter_entries_pub("mai", &entries, &mut filtered, &mut selected);
                black_box(&filtered);
            });
            eprintln!("  [alloc] finder_filter_warm:         {stats}");
        }

        eprintln!();
    }

    // Criterion timing for the same operations
    group.bench_function("parse_pipeline", |b| {
        b.iter(|| {
            let _ = black_box(parse::parse("find . -name '*.rs' | grep main | wc -l"));
        });
    });

    group.bench_function("line_buffer_30_chars", |b| {
        b.iter(|| {
            let mut lb = LineBuffer::new();
            for c in "echo hello world this is a test".chars() {
                lb.insert_char(c);
            }
            black_box(lb.text());
        });
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
            ish::config::load(&mut aliases, None);

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
            ish::config::load(&mut aliases, None);

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

    group.bench_function("denv_init", |b| {
        b.iter(|| black_box(ish::denv::init()));
    });

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

    // Verify zero allocations for a suggestion hit
    group.bench_function("prefix_search_allocs", |b| {
        b.iter(|| {
            let stats = measure_allocs(|| {
                let entry = history.prefix_search("git commit", 0);
                let suffix = entry.and_then(|e| e.strip_prefix("git commit"));
                black_box(suffix);
            });
            assert_eq!(stats.count, 0, "autosuggestion should be zero-alloc");
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

    // Verify zero allocations for a cached lookup
    group.bench_function("path_cache_allocs", |b| {
        b.iter(|| {
            let stats = measure_allocs(|| {
                black_box(cache.contains("git"));
            });
            assert_eq!(stats.count, 0, "cached lookup should be zero-alloc");
        });
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

    // Async finder: measure time to spawn + drain all entries
    group.bench_function("find_async_normal_drain", |b| {
        b.iter(|| {
            let handle = ish::finder::find_async(".", false);
            let mut buf = Vec::new();
            loop {
                handle.drain_into(&mut buf);
                if handle.receiver.try_recv().is_err() {
                    // Channel empty + no more coming = walk done
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    handle.drain_into(&mut buf);
                    if handle.receiver.try_recv().is_err() {
                        break;
                    }
                }
            }
            black_box(buf.len());
        });
    });

    group.bench_function("find_async_hidden_drain", |b| {
        b.iter(|| {
            let handle = ish::finder::find_async(".", true);
            let mut buf = Vec::new();
            loop {
                handle.drain_into(&mut buf);
                if handle.receiver.try_recv().is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    handle.drain_into(&mut buf);
                    if handle.receiver.try_recv().is_err() {
                        break;
                    }
                }
            }
            black_box(buf.len());
        });
    });

    group.finish();
}

criterion_group!(
    name = benches;
    config = fast_config();
    targets =
        bench_startup,
        bench_parse,
        bench_expand,
        bench_line_buffer,
        bench_history,
        bench_completion,
        bench_prompt,
        bench_parse_expand,
        bench_prompt_render,
        bench_history_add,
        bench_ls,
        bench_path_lookup,
        bench_alias,
        bench_completion_fs,
        bench_denv,
        bench_alloc_audit,
        bench_autosuggestion,
        bench_command_coloring,
        bench_finder,
);
criterion_main!(benches);
