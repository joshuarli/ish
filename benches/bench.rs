//! Benchmark harness for ish shell.
//!
//! Tracks wall time (criterion) and heap allocations (counting allocator).
//! Run: `cargo bench`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};

use ish::alias::AliasMap;
use ish::complete::{self, CompEntry};
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
            ))
        });
    });

    group.bench_function("no_expansion_needed", |b| {
        b.iter(|| {
            black_box(expand::expand_word(
                "simple_word",
                "/home/user",
                &mut no_subst,
            ))
        });
    });

    // Multi-word expansion
    let words: Vec<String> = (0..50).map(|i| format!("word{i}")).collect();
    group.bench_function("expand_argv_50_words", |b| {
        b.iter(|| black_box(expand::expand_argv(&words, "/home/user", &mut no_subst)));
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
// History benchmarks
// ---------------------------------------------------------------------------

fn bench_history(c: &mut Criterion) {
    let mut group = c.benchmark_group("history");

    let entries: Vec<String> = (0..10_000)
        .map(|i| format!("command_{i} --arg={i} /path/to/file_{i}"))
        .collect();
    let history = History::from_entries(entries);

    group.bench_function("prefix_search_10k", |b| {
        b.iter(|| black_box(history.prefix_search("command_999", 0)));
    });

    group.bench_function("fuzzy_search_10k", |b| {
        b.iter(|| black_box(history.fuzzy_search("cmd99")));
    });

    group.bench_function("fuzzy_search_miss_10k", |b| {
        b.iter(|| black_box(history.fuzzy_search("zzzznotfound")));
    });

    group.bench_function("fuzzy_search_empty_query_10k", |b| {
        b.iter(|| {
            let results = history.fuzzy_search("");
            black_box(results.len());
        });
    });

    // Small history — more representative of typical use
    let small_entries: Vec<String> = vec![
        "git commit -m 'fix'",
        "cargo build",
        "cargo test",
        "git push origin main",
        "cd ~/projects",
        "ls -la",
        "vim src/main.rs",
        "make build",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let small_history = History::from_entries(small_entries);

    group.bench_function("fuzzy_search_small", |b| {
        b.iter(|| black_box(small_history.fuzzy_search("gb")));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Completion benchmarks
// ---------------------------------------------------------------------------

fn bench_completion(c: &mut Criterion) {
    let mut group = c.benchmark_group("completion");

    // Grid layout computation
    let entries: Vec<CompEntry> = (0..100)
        .map(|i| CompEntry {
            name: format!("file_{i:03}.rs"),
            is_dir: i % 5 == 0,
            is_link: false,
            is_exec: i % 10 == 0,
        })
        .collect();

    group.bench_function("compute_grid_100_entries", |b| {
        b.iter(|| black_box(complete::compute_grid(&entries, 120)));
    });

    group.bench_function("compute_grid_100_narrow", |b| {
        b.iter(|| black_box(complete::compute_grid(&entries, 40)));
    });

    let small: Vec<CompEntry> = (0..5)
        .map(|i| CompEntry {
            name: format!("f{i}.rs"),
            is_dir: false,
            is_link: false,
            is_exec: false,
        })
        .collect();

    group.bench_function("compute_grid_5_entries", |b| {
        b.iter(|| black_box(complete::compute_grid(&small, 80)));
    });

    // Filesystem completion (real I/O — measures readdir performance)
    group.bench_function("complete_path_cwd", |b| {
        b.iter(|| black_box(complete::complete_path("./src/", false)));
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
            black_box(expand::expand_argv(argv, "/home/user", &mut no_subst))
        });
    });

    // Typical: pipeline with variables
    group.bench_function("pipeline_with_vars", |b| {
        b.iter(|| {
            let cmd = parse::parse("grep -r $ISH_BENCH_DIR | sort | head -20").unwrap();
            let argv = &cmd.segments[0].0.commands[0].cmd.argv;
            black_box(expand::expand_argv(argv, "/home/user", &mut no_subst))
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
            black_box(expand::expand_argv(argv, "/home/user", &mut no_subst))
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

    // Add to a 1k history (typical size)
    group.bench_function("add_new_1k", |b| {
        let entries: Vec<String> = (0..1000)
            .map(|i| format!("command_{i} --arg={i}"))
            .collect();
        b.iter(|| {
            let mut h = History::from_entries(entries.clone());
            h.add("brand_new_command --flag");
            black_box(());
        });
    });

    // Add duplicate (triggers retain scan + push)
    group.bench_function("add_dup_1k", |b| {
        let entries: Vec<String> = (0..1000)
            .map(|i| format!("command_{i} --arg={i}"))
            .collect();
        b.iter(|| {
            let mut h = History::from_entries(entries.clone());
            h.add("command_500 --arg=500");
            black_box(());
        });
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

    // Environment variable completion
    group.bench_function("complete_env_all", |b| {
        b.iter(|| black_box(complete::complete_env("$")));
    });

    group.bench_function("complete_env_prefix", |b| {
        b.iter(|| black_box(complete::complete_env("$HO")));
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
            let _ = black_box(expand::expand_argv(argv, "/home/user", &mut no_subst));
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

        let stats = measure_allocs(|| {
            let _ = black_box(complete::complete_env("$HO"));
        });
        eprintln!("  [alloc] complete_env_prefix:       {stats}");

        let stats = measure_allocs(|| {
            let _ = black_box(ish::denv::apply_bash_output_bench(
                "export A='1';\nexport B='two';\nunset C;\n",
            ));
        });
        eprintln!("  [alloc] denv_parse_3_directives:   {stats}");

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

    group.bench_function("denv_init", |b| {
        b.iter(|| black_box(ish::denv::init()));
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
);
criterion_main!(benches);
