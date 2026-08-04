//! Benchmark harness for ish shell.
//!
//! Tracks wall time and allocations for user-visible and interactive hot paths.
//! Run: `cargo bench --bench bench`.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use divan::{AllocProfiler, Bencher, black_box};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

use ish::alias::AliasMap;
use ish::complete::{self, CompletionState};
use ish::history::History;
use ish::line::LineBuffer;
use ish::ls;
use ish::path::PathCache;
use ish::prompt;
use ish::render::{self, RenderedRegion};
use ish::term::TermWriter;

const TRACE_BEGIN: &[u8] = b"BENCH_BEGIN\0";
const TRACE_END: &[u8] = b"BENCH_END\0";

#[cfg(target_os = "linux")]
fn syscall_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("SYSCALL_TRACE").is_some())
}

fn trace_marker(marker: &[u8]) {
    #[cfg(target_os = "linux")]
    if syscall_trace_enabled() {
        unsafe {
            libc::syscall(libc::SYS_prctl, 15, marker.as_ptr(), 0, 0, 0);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = marker;
}

#[cfg(target_os = "linux")]
fn bench_with_syscall_trace<O>(bencher: Bencher, mut operation: impl FnMut() -> O) {
    bencher.bench_local(|| {
        trace_marker(TRACE_BEGIN);
        let result = operation();
        trace_marker(TRACE_END);
        black_box(result);
    });
}

#[cfg(not(target_os = "linux"))]
fn bench_with_syscall_trace<O>(bencher: Bencher, operation: impl FnMut() -> O) {
    bencher.bench_local(operation);
}

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

struct SuppressStderr {
    saved: libc::c_int,
}

impl SuppressStderr {
    fn new() -> Self {
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        assert!(saved >= 0);
        let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY) };
        assert!(null >= 0);
        unsafe {
            libc::dup2(null, libc::STDERR_FILENO);
            libc::close(null);
        }
        Self { saved }
    }
}

impl Drop for SuppressStderr {
    fn drop(&mut self) {
        unsafe {
            libc::fflush(std::ptr::null_mut());
            libc::dup2(self.saved, libc::STDERR_FILENO);
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

fn make_git_fixture(label: &str) -> BenchDir {
    let fixture = make_fs_fixture(label);
    let git = fixture.path().join(".git");
    fs::create_dir_all(&git).unwrap();
    fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
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

fn completion_path_benchmark(bencher: Bencher, file_count: usize, render_grid: bool) {
    let fixture = make_fs_fixture("completion");
    for index in 0..file_count {
        fs::write(
            fixture.path().join(format!("file_{index:03}.rs")),
            b"fixture\n",
        )
        .unwrap();
    }
    let partial = if file_count == 1 {
        format!("{}/file_0", fixture.path().display())
    } else {
        format!("{}/file_", fixture.path().display())
    };
    let mut state = CompletionState {
        comp: complete::Completions::with_capacity(8192, 256),
        selected: 0,
        cols: 0,
        rows: 0,
        scroll: 0,
        term_cols: 80,
        dir_prefix: fixture.path().to_string_lossy().into_owned(),
        in_quote: false,
    };
    let mut tw = TermWriter::new();

    bench_with_syscall_trace(bencher, || {
        state.comp.clear();
        complete::complete_path_into(&partial, false, &mut state.comp);
        (state.cols, state.rows) = complete::compute_grid(&state.comp.entries, state.term_cols);
        if render_grid {
            tw.clear_buffer();
            render::render_completions(&mut tw, &state, RenderedRegion::default(), true);
        }
        black_box((&state, &tw));
    });
}

#[divan::bench]
fn completion_path_realistic(bencher: Bencher) {
    completion_path_benchmark(bencher, 200, true);
}

#[divan::bench]
fn completion_path_single_match(bencher: Bencher) {
    completion_path_benchmark(bencher, 1, false);
}

#[divan::bench]
fn history_fuzzy_search_into_45k(bencher: Bencher) {
    let history = History::from_entries(synthetic_history_45k());
    let mut candidates = Vec::with_capacity(history.len());
    history.visible_entry_indices_into(&mut candidates);
    let mut scratch = Vec::with_capacity(history.len());
    let mut results = Vec::with_capacity(200);
    bench_with_syscall_trace(bencher, || {
        history.fuzzy_search_subset_into("gco", &candidates, &mut scratch, &mut results, 200);
        black_box((&scratch, &results));
    });
}

#[divan::bench]
fn ls_fixture_dir(bencher: Bencher) {
    let fixture = make_fs_fixture("ls");
    for index in 0..32 {
        fs::write(
            fixture.path().join(format!("entry_{index:02}.txt")),
            b"fixture\n",
        )
        .unwrap();
    }
    for index in 0..4 {
        fs::create_dir(fixture.path().join(format!("dir_{index}"))).unwrap();
    }
    let executable = fixture.path().join("run.sh");
    fs::write(&executable, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    symlink("entry_00.txt", fixture.path().join("latest.txt")).unwrap();
    let path = fixture.path().to_str().unwrap().to_owned();
    let _stdout = SuppressStdout::new();
    black_box(ls::list_dir(&path));
    bench_with_syscall_trace(bencher, || black_box(ls::list_dir(&path)));
}

fn startup_fixture(bencher: Bencher, cold: bool) {
    let fixture = make_git_fixture(if cold { "startup-cold" } else { "startup-warm" });
    let history_path = fixture.path().join("history");
    let config_path = fixture.path().join("config.ish");
    let history_contents = (0..2_000)
        .map(|i| format!("git status --short {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&history_path, &history_contents).unwrap();
    fs::write(
        &config_path,
        "set EDITOR \"vim\"\nalias gs \"git status\"\nalias ll \"l -la\"\n",
    )
    .unwrap();

    let run = || {
        let _history = black_box(History::load_from(history_path.clone()));

        let mut aliases = AliasMap::new();
        let mut epsh = epsh::eval::Shell::builder()
            .cwd(fixture.path().to_path_buf())
            .interactive(true)
            .build();
        ish::config::load(&mut aliases, &mut epsh, Some(config_path.as_os_str()));

        let mut p = prompt::Prompt::new();
        let mut out = String::with_capacity(128);
        p.render_into(&mut out, 0, fixture.path().to_str().unwrap(), false);
        black_box(out);
    };
    if !cold {
        run();
        bench_with_syscall_trace(bencher, run);
    } else {
        bencher
            .with_inputs(|| {
                fs::write(&history_path, &history_contents).unwrap();
                let _ = fs::remove_file(fixture.path().join("history.bin"));
            })
            .bench_local_values(|_| {
                trace_marker(TRACE_BEGIN);
                run();
                trace_marker(TRACE_END);
            });
    }
}

#[divan::bench]
fn startup_cold(bencher: Bencher) {
    startup_fixture(bencher, true);
}

#[divan::bench]
fn startup_warm(bencher: Bencher) {
    startup_fixture(bencher, false);
}

fn cd_prompt_benchmark(bencher: Bencher, with_denv: bool) {
    let fixture = BenchDir::new(if with_denv { "cd-denv" } else { "cd" });
    let first = fixture.path().join("first");
    let second = fixture.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    if with_denv {
        fs::write(first.join(".env"), b"ISH_BENCH_DIR=first\n").unwrap();
        fs::write(second.join(".env"), b"ISH_BENCH_DIR=second\n").unwrap();
    }

    let original_dir = std::env::current_dir().unwrap();
    let original_pwd = std::env::var_os("PWD");
    let _stderr = SuppressStderr::new();
    let mut epsh = epsh::eval::Shell::builder()
        .cwd(first.clone())
        .interactive(true)
        .build();
    ish::denv::init(&mut epsh);
    std::env::set_current_dir(&first).unwrap();
    sync_bench_pwd(&mut epsh, &first);
    let _ = ish::denv::on_cd(&epsh);
    std::env::set_current_dir(&second).unwrap();
    sync_bench_pwd(&mut epsh, &second);
    let _ = ish::denv::on_cd(&epsh);

    let mut prompt = prompt::Prompt::new();
    let mut out = String::with_capacity(128);
    let mut use_first = false;
    bench_with_syscall_trace(bencher, || {
        let dir = if use_first { &first } else { &second };
        use_first = !use_first;
        std::env::set_current_dir(dir).unwrap();
        sync_bench_pwd(&mut epsh, dir);
        let changes = ish::denv::on_cd(&epsh);
        prompt.invalidate_git();
        prompt.render_into(&mut out, 0, dir.to_str().unwrap(), !changes.is_empty());
        black_box((&changes, &out));
    });

    std::env::set_current_dir(original_dir).unwrap();
    match original_pwd {
        Some(pwd) => ish::shell_setenv_os("PWD", pwd),
        None => ish::shell_unsetenv("PWD"),
    }
}

// Mirror what change_directory does before denv::on_cd: PWD must agree in both
// the process environment and epsh's variable store, because denv::refresh
// starts by restoring the process environment from the store.
fn sync_bench_pwd(epsh: &mut epsh::eval::Shell, dir: &Path) {
    ish::shell_setenv_os("PWD", dir.as_os_str());
    let _ = epsh.vars_mut().set_bytes(
        "PWD",
        epsh::shell_bytes::ShellBytes::from_os_str(dir.as_os_str()),
    );
    epsh.vars_mut().export("PWD");
}

#[divan::bench]
fn cd_prompt_without_denv(bencher: Bencher) {
    cd_prompt_benchmark(bencher, false);
}

#[divan::bench]
fn cd_prompt_with_denv(bencher: Bencher) {
    cd_prompt_benchmark(bencher, true);
}

#[divan::bench]
fn autosuggestion_prefix_search_miss(bencher: Bencher) {
    let history = History::from_entries(synthetic_history_45k());
    bench_with_syscall_trace(bencher, || {
        black_box(history.session_prefix_search("zzzznotfound", 0));
    });
}

fn keypress_render_benchmark(bencher: Bencher, initial: &str, inserted: char) {
    let history = History::from_entries(vec![
        "git status --short --branch".to_owned(),
        "git stash list".to_owned(),
    ]);
    let mut line = LineBuffer::new();
    line.set(initial);
    let mut path_cache = PathCache::new();
    let mut tw = TermWriter::new();
    let mut region = RenderedRegion::default();

    bench_with_syscall_trace(bencher, || {
        line.insert_char(inserted);
        let text = line.text();
        let suggestion = if text.len() >= 3 && line.cursor() == text.len() {
            history
                .session_prefix_search(text, 0)
                .and_then(|entry| entry.strip_prefix(text))
                .unwrap_or("")
        } else {
            ""
        };
        let opts = render::RenderOpts {
            // PATH bytes from the store; a representative constant is fine here.
            cmd_color: Some(path_cache.contains("git", b"/usr/local/bin:/usr/bin:/bin")),
            suggestion,
        };
        tw.clear_buffer();
        region = render::render_line(&mut tw, "$ ", 2, &line, 80, region, &opts);
        line.delete_back();
        black_box((&line, &region, &tw));
    });
}

#[divan::bench]
fn normal_keypress_render_short(bencher: Bencher) {
    keypress_render_benchmark(bencher, "git s", 't');
}

#[divan::bench]
fn normal_keypress_render_long(bencher: Bencher) {
    keypress_render_benchmark(
        bencher,
        "git status --short --branch --untracked-files=all src/",
        'x',
    );
}

struct CommandEnterFixture {
    aliases: AliasMap,
    history: History,
    epsh: epsh::eval::Shell,
    prompt: prompt::Prompt,
    prompt_buf: String,
    pwd: String,
}

impl CommandEnterFixture {
    fn new(pwd: &Path) -> Self {
        let mut aliases = AliasMap::new();
        aliases.set("t".to_owned(), vec!["true".to_owned()]);
        let epsh = epsh::eval::Shell::builder()
            .cwd(pwd.to_path_buf())
            .interactive(true)
            .build();
        Self {
            aliases,
            history: History::from_entries(Vec::new()),
            epsh,
            prompt: prompt::Prompt::new(),
            prompt_buf: String::with_capacity(128),
            pwd: pwd.to_string_lossy().into_owned(),
        }
    }
}

#[divan::bench]
fn command_enter_to_prompt(bencher: Bencher) {
    let pwd = std::env::current_dir().unwrap();
    bencher
        .with_inputs(|| CommandEnterFixture::new(&pwd))
        .bench_local_values(|mut fixture| {
            trace_marker(TRACE_BEGIN);
            let expanded = fixture.aliases.expand_line("t");
            let status = fixture.epsh.run_script(&expanded);
            fixture.history.add("t");
            fixture
                .prompt
                .render_into(&mut fixture.prompt_buf, status, &fixture.pwd, false);
            trace_marker(TRACE_END);
            black_box(fixture);
        });
}

#[divan::bench]
fn history_search_trace(bencher: Bencher) {
    let history = History::from_entries(synthetic_history_45k());
    let mut all_candidates = Vec::with_capacity(history.len());
    history.visible_entry_indices_into(&mut all_candidates);
    let mut candidates = Vec::with_capacity(history.len());
    let mut scratch = Vec::with_capacity(history.len());
    let mut matches = Vec::with_capacity(200);
    let mut tw = TermWriter::new();
    let mut region = RenderedRegion::default();
    let mut cache = render::HistoryPagerCache::default();

    bench_with_syscall_trace(bencher, || {
        candidates.clear();
        candidates.extend_from_slice(&all_candidates);
        scratch.clear();
        matches.clear();
        cache.clear();
        tw.clear_buffer();
        history.fuzzy_search_subset_into("", &candidates, &mut scratch, &mut matches, 200);
        region = render::render_history_pager_cached(
            &mut tw, "", &matches, &history, 0, 24, 80, 0, region, &mut cache,
        );

        for query in ["g", "gc", "gco"] {
            history.fuzzy_search_subset_into(query, &candidates, &mut scratch, &mut matches, 200);
            std::mem::swap(&mut candidates, &mut scratch);
            tw.clear_buffer();
            region = render::render_history_pager_cached(
                &mut tw,
                query,
                &matches,
                &history,
                0,
                24,
                80,
                query.len(),
                region,
                &mut cache,
            );
        }
        black_box((&candidates, &scratch, &matches, &region, &tw));
    });
}

fn main() {
    divan::main();
}
