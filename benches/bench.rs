//! Benchmark harness for ish shell.
//!
//! Tracks wall time and allocations for user-visible and interactive hot paths.
//! Run: `cargo bench --bench bench`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
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
use ish::ls;
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

#[divan::bench]
fn completion_path_realistic(bencher: Bencher) {
    let fixture = make_fs_fixture("completion");
    for index in 0..200 {
        fs::write(
            fixture.path().join(format!("file_{index:03}.rs")),
            b"fixture\n",
        )
        .unwrap();
    }
    let partial = format!("{}/file_", fixture.path().display());
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
        tw.clear_buffer();
        render::render_completions(&mut tw, &state, RenderedRegion::default(), true);
        black_box((&state, &tw));
    });
}

#[divan::bench]
fn history_fuzzy_search_into_45k(bencher: Bencher) {
    let history = History::from_entries(synthetic_history_45k());
    let mut results = Vec::with_capacity(200);
    history.fuzzy_search_into("gco", &mut results, 200, "");
    bench_with_syscall_trace(bencher, || {
        history.fuzzy_search_into("gco", &mut results, 200, "");
        black_box(&results);
    });
}

#[divan::bench]
fn history_add_duplicate_45k(bencher: Bencher) {
    let entries = synthetic_history_45k();
    let duplicate = entries[entries.len() / 2].clone();
    bencher
        .with_inputs(|| History::from_entries(entries.clone()))
        .bench_local_values(|mut history| {
            trace_marker(TRACE_BEGIN);
            history.add(&duplicate);
            black_box(history);
            trace_marker(TRACE_END);
        });
}

#[divan::bench]
fn ls_fixture_dir(bencher: Bencher) {
    let fixture = make_fs_fixture("ls");
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
        ish::config::load(
            &mut aliases,
            &mut epsh::eval::Shell::new(),
            Some(config_path.as_os_str()),
        );

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

#[divan::bench]
fn cd_prompt_with_denv(bencher: Bencher) {
    let fixture = BenchDir::new("cd-denv");
    let first = fixture.path().join("first");
    let second = fixture.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    fs::write(first.join(".env"), b"ISH_BENCH_DIR=first\n").unwrap();
    fs::write(second.join(".env"), b"ISH_BENCH_DIR=second\n").unwrap();

    let original_dir = std::env::current_dir().unwrap();
    let original_pwd = std::env::var_os("PWD");
    let _stderr = SuppressStderr::new();
    ish::denv::init();
    std::env::set_current_dir(&first).unwrap();
    ish::shell_setenv_os("PWD", first.as_os_str());
    let _ = ish::denv::on_cd();
    std::env::set_current_dir(&second).unwrap();
    ish::shell_setenv_os("PWD", second.as_os_str());
    let _ = ish::denv::on_cd();

    let mut prompt = prompt::Prompt::new();
    let mut out = String::with_capacity(128);
    let mut use_first = false;
    bench_with_syscall_trace(bencher, || {
        let dir = if use_first { &first } else { &second };
        use_first = !use_first;
        std::env::set_current_dir(dir).unwrap();
        ish::shell_setenv_os("PWD", dir.as_os_str());
        let changes = ish::denv::on_cd();
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

#[divan::bench]
fn autosuggestion_prefix_search_miss(bencher: Bencher) {
    let history = History::from_entries(synthetic_history_45k());
    bench_with_syscall_trace(bencher, || {
        black_box(history.prefix_search("zzzznotfound", 0));
    });
}

fn main() {
    divan::main();
}
