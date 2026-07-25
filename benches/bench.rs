//! Benchmark harness for ish shell.
//!
//! Tracks wall time and allocations for user-visible and interactive hot paths.
//! Run: `cargo bench --bench bench`.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

struct BenchDir(PathBuf);

impl BenchDir {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ish-bench-{label}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for BenchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct SuppressStdout {
    saved: libc::c_int,
}

impl SuppressStdout {
    fn new() -> Self {
        let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved >= 0);
        let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY) };
        assert!(null >= 0);
        unsafe {
            libc::dup2(null, libc::STDOUT_FILENO);
            libc::close(null);
        }
        Self { saved }
    }
}

impl Drop for SuppressStdout {
    fn drop(&mut self) {
        unsafe {
            libc::fflush(std::ptr::null_mut());
            libc::dup2(self.saved, libc::STDOUT_FILENO);
            libc::close(self.saved);
        }
    }
}

fn make_fs_fixture(label: &str) -> BenchDir {
    let fixture = BenchDir::new(label);
    let src = fixture.path().join("src");
    let nested = src.join("nested");
    let bin = fixture.path().join("bin");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&bin).unwrap();
    for name in [
        "alias.rs",
        "builtin.rs",
        "complete.rs",
        "config.rs",
        "history.rs",
        "input.rs",
        "line.rs",
        "main.rs",
        "prompt.rs",
        "render.rs",
    ] {
        fs::write(src.join(name), b"fixture\n").unwrap();
    }
    for index in 0..12 {
        fs::write(nested.join(format!("module_{index}.rs")), b"fixture\n").unwrap();
    }
    let executable = bin.join("ish");
    fs::write(&executable, b"fixture\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    fixture
}

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
fn completion_candidates(bencher: Bencher) {
    let names: Vec<String> = (0..100)
        .map(|index| format!("file_{index:03}.rs"))
        .collect();
    let entries: Vec<(&str, bool, bool, bool)> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index % 5 == 0, false, index % 7 == 0))
        .collect();
    let mut comp = complete::Completions::with_capacity(2048, 100);
    complete::complete_candidates(&entries, "file_0", false, &mut comp);
    bencher.bench_local(|| {
        complete::complete_candidates(&entries, "file_0", false, &mut comp);
        black_box(&comp);
    });
}

#[divan::bench]
fn prompt_full_fixture(bencher: Bencher) {
    let fixture = BenchDir::new("prompt");
    let mut p = prompt::Prompt::new();
    let mut out = String::with_capacity(128);
    let path = fixture.path().to_str().unwrap().to_owned();
    p.render_into(&mut out, 0, &path, false);
    bencher.bench_local(|| {
        p.render_into(&mut out, 0, &path, false);
        black_box(&out);
    });
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
fn ls_fixture_dir(bencher: Bencher) {
    let fixture = make_fs_fixture("ls");
    let path = fixture.path().to_str().unwrap().to_owned();
    let _stdout = SuppressStdout::new();
    black_box(ls::list_dir(&path));
    bencher.bench_local(|| black_box(ls::list_dir(&path)));
}

#[divan::bench]
fn startup_fixture(bencher: Bencher) {
    let fixture = BenchDir::new("startup");
    let history_path = fixture.path().join("history");
    let entries = (0..2_000)
        .map(|i| format!("git status --short {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&history_path, entries).unwrap();

    let run = || {
        let _history = black_box(History::load_from(history_path.clone()));

        let mut aliases = AliasMap::new();
        ish::config::load(
            &mut aliases,
            &mut epsh::eval::Shell::new(),
            Some(OsStr::new("/dev/null")),
        );

        let mut p = prompt::Prompt::new();
        let mut out = String::with_capacity(128);
        p.render_into(&mut out, 0, fixture.path().to_str().unwrap(), false);
        black_box(out);
    };
    run();
    bencher.bench_local(run);
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
fn finder_filter_candidates(bencher: Bencher) {
    let entries: Vec<(usize, String)> = (0..500)
        .map(|index| (index % 5, format!("src/module_{index}/main.rs")))
        .collect();
    let mut filtered = Vec::with_capacity(entries.len());
    let mut selected = 0usize;
    ish::finder::filter_entries_pub("module", &entries, &mut filtered, &mut selected);
    bencher.bench_local(|| {
        ish::finder::filter_entries_pub("module", &entries, &mut filtered, &mut selected);
        black_box(&filtered);
    });
}

fn main() {
    divan::main();
}
