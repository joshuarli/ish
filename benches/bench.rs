//! Benchmark harness for ish shell.
//!
//! Tracks wall time for user-visible and interactive hot paths.
//! Run: `cargo bench`

#![feature(test)]

extern crate test;

use std::ffi::OsStr;
use test::{Bencher, black_box};

struct BenchmarkGroup<'a> {
    bencher: &'a mut Bencher,
}

impl<'a> BenchmarkGroup<'a> {
    fn new(bencher: &'a mut Bencher) -> Self {
        Self { bencher }
    }

    fn bench_function<F>(&mut self, _name: &str, f: F)
    where
        F: FnOnce(&mut Bencher),
    {
        f(self.bencher);
    }

    fn finish(self) {}
}

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

#[bench]
fn bench_line_buffer(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

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

#[bench]
fn bench_history(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    let entries_45k = synthetic_history_45k();
    let history = History::from_entries(entries_45k.clone());

    group.bench_function("fuzzy_search_into_45k", |b| {
        let mut results = Vec::with_capacity(200);
        history.fuzzy_search_into("gco", &mut results, 200, "");
        b.iter(|| {
            history.fuzzy_search_into("gco", &mut results, 200, "");
            black_box(&results);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Completion benchmarks
// ---------------------------------------------------------------------------

#[bench]
fn bench_completion(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    // Filesystem completion (real I/O — measures readdir performance)
    group.bench_function("complete_path_cwd", |b| {
        b.iter(|| black_box(complete::complete_path("./src/", false)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Prompt render (full — the before-every-command hot path)
// ---------------------------------------------------------------------------

#[bench]
fn bench_prompt_render(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    // Full render in a git repo (this repo)
    group.bench_function("full_in_git_repo", |b| {
        let mut p = prompt::Prompt::new();
        b.iter(|| black_box(p.render(0)));
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

#[bench]
fn bench_interactive_render(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

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

    group.finish();
}

// ---------------------------------------------------------------------------
// History add (dedup on every command)
// ---------------------------------------------------------------------------

#[bench]
fn bench_history_add(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    let entries_45k = synthetic_history_45k();

    group.bench_function("add_dup_45k", |b| {
        let mid = entries_45k[entries_45k.len() / 2].clone();
        b.iter(|| {
            let mut h = History::from_entries(entries_45k.clone());
            h.add(&mid);
            black_box(h);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// ls builtin (real I/O — the most-used builtin)
// ---------------------------------------------------------------------------

#[bench]
fn bench_ls(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    // List the repo's src/ directory (~17 files)
    group.bench_function("src_dir", |b| {
        b.iter(|| black_box(ls::list_dir("./src")));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// PATH lookup (every external command)
// ---------------------------------------------------------------------------

#[bench]
fn bench_path_lookup(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    // scan_path: find ls (typical case)
    group.bench_function("scan_path_ls", |b| {
        b.iter(|| black_box(exec::scan_path("ls")));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Completion: realistic filesystem
// ---------------------------------------------------------------------------

#[bench]
fn bench_completion_fs(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    // Complete with prefix filter
    group.bench_function("complete_with_prefix", |b| {
        b.iter(|| black_box(complete::complete_path("./src/l", false)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Startup (time-to-prompt) benchmarks
// ---------------------------------------------------------------------------

#[bench]
fn bench_startup(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    // Full cold startup in a git repo (this repo).
    // Skips terminal setup (requires real tty) and signal::init (creates
    // pipe per call). Everything else mirrors main() order.
    // denv::init() is now deferred to first cd — not part of startup.
    group.bench_function("git_repo", |b| {
        b.iter(|| {
            let _history = black_box(History::load());

            let mut aliases = AliasMap::new();
            ish::config::load(
                &mut aliases,
                &mut epsh::eval::Shell::new(),
                Some(OsStr::new("/dev/null")),
            );

            // Fresh prompt — git cache is cold
            let mut p = prompt::Prompt::new();
            let mut out = String::with_capacity(128);
            p.render_into(&mut out, 0, env!("CARGO_MANIFEST_DIR"), false);
            black_box(out);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Autosuggestion + command coloring benchmarks
// ---------------------------------------------------------------------------

#[bench]
fn bench_autosuggestion(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    let entries = synthetic_history_45k();
    let history = History::from_entries(entries);

    // Worst case: no match, scans all 45k entries
    group.bench_function("prefix_search_miss", |b| {
        b.iter(|| {
            black_box(history.prefix_search("zzzznotfound", 0));
        });
    });

    group.finish();
}

#[bench]
fn bench_command_coloring(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    let mut cache = exec::PathCache::new();

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
// Finder benchmarks (real filesystem searches against this repo)
// ---------------------------------------------------------------------------

#[bench]
fn bench_finder(b: &mut Bencher) {
    let mut group = BenchmarkGroup::new(b);

    group.bench_function("find_ish_normal", |b| {
        b.iter(|| black_box(ish::finder::find(".", "ish", false, 1000)));
    });

    group.finish();
}
