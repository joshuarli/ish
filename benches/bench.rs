//! Benchmark harness for ish shell.
//!
//! Tracks wall time and allocations for user-visible and interactive hot paths.
//! Run: `cargo bench --bench bench`.

use std::ffi::OsStr;

use divan::{AllocProfiler, Bencher, black_box};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

use ish::alias::AliasMap;
use ish::complete;
use ish::history::History;
use ish::line::LineBuffer;
use ish::ls;
use ish::path as exec;
use ish::prompt;
use ish::render;
use ish::term::TermWriter;

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
        .map(|i| templates[i % templates.len()].replace("{}", &i.to_string()))
        .collect()
}

#[divan::bench]
fn line_buffer_insert_100_chars() {
    let mut lb = LineBuffer::new();
    for c in "echo hello world this is a long command line with many characters and words".chars() {
        lb.insert_char(c);
    }
    black_box(lb.text());
}

#[divan::bench]
fn history_fuzzy_search_into_45k(bencher: Bencher) {
    let history = History::from_entries(synthetic_history_45k());
    let mut results = Vec::with_capacity(200);
    history.fuzzy_search_into("gco", &mut results, 200, "");
    bencher.bench_local(|| {
        history.fuzzy_search_into("gco", &mut results, 200, "");
        black_box(&results);
    });
}

#[divan::bench]
fn completion_path_cwd() {
    black_box(complete::complete_path("./src/", false));
}

#[divan::bench]
fn prompt_full_in_git_repo() {
    let mut p = prompt::Prompt::new();
    black_box(p.render(0));
}

fn make_line(s: &str) -> LineBuffer {
    let mut lb = LineBuffer::new();
    for ch in s.chars() {
        lb.insert_char(ch);
    }
    lb
}

#[divan::bench]
fn interactive_render_grow_rows(bencher: Bencher) {
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
    bencher.bench_local(|| {
        let line = if toggle { &line_short } else { &line_long };
        toggle = !toggle;
        tw.clear_buffer();
        prev = render::render_line(&mut tw, "$ ", 2, line, 20, prev, &opts);
        black_box(prev);
        black_box(&tw);
    });
}

#[divan::bench]
fn history_add_duplicate_45k(bencher: Bencher) {
    let entries = synthetic_history_45k();
    let duplicate = entries[entries.len() / 2].clone();
    bencher
        .with_inputs(|| History::from_entries(entries.clone()))
        .bench_local_values(|mut history| {
            history.add(&duplicate);
            black_box(history);
        });
}

#[divan::bench]
fn ls_src_dir() {
    black_box(ls::list_dir("./src"));
}

#[divan::bench]
fn path_lookup_ls() {
    black_box(exec::scan_path("ls"));
}

#[divan::bench]
fn completion_with_prefix() {
    black_box(complete::complete_path("./src/l", false));
}

#[divan::bench]
fn startup_git_repo() {
    let _history = black_box(History::load());

    let mut aliases = AliasMap::new();
    ish::config::load(
        &mut aliases,
        &mut epsh::eval::Shell::new(),
        Some(OsStr::new("/dev/null")),
    );

    let mut p = prompt::Prompt::new();
    let mut out = String::with_capacity(128);
    p.render_into(&mut out, 0, env!("CARGO_MANIFEST_DIR"), false);
    black_box(out);
}

#[divan::bench]
fn autosuggestion_prefix_search_miss() {
    let history = History::from_entries(synthetic_history_45k());
    black_box(history.prefix_search("zzzznotfound", 0));
}

#[divan::bench]
fn command_coloring_full_check(bencher: Bencher) {
    let mut cache = exec::PathCache::new();
    cache.contains("ls");
    let aliases = AliasMap::new();
    bencher.bench_local(|| {
        let valid = ish::builtin::is_builtin("git")
            || aliases.get("git").is_some()
            || cache.contains("git");
        black_box(valid);
    });
}

#[divan::bench]
fn finder_ish_normal() {
    black_box(ish::finder::find(".", "ish", false, 1000));
}

fn main() {
    divan::main();
}
