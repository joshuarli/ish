//! Benchmark harness for ish shell.
//!
//! Tracks wall time (criterion) and heap allocations (counting allocator).
//! Run: `cargo bench`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};

use ish::complete::{self, CompEntry};
use ish::error::Error;
use ish::expand;
use ish::history::History;
use ish::line::LineBuffer;
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

fn fast_config() -> Criterion {
    Criterion::default()
        .sample_size(50)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2))
}

criterion_group!(
    name = benches;
    config = fast_config();
    targets =
        bench_parse,
        bench_expand,
        bench_line_buffer,
        bench_history,
        bench_completion,
        bench_prompt,
        bench_alloc_audit,
);
criterion_main!(benches);
