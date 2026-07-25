//! Benchmark harness for ish shell.
//!
//! Tracks wall time and allocations for user-visible and interactive hot paths.
//! Run: `cargo bench --bench bench`.

use std::ffi::OsStr;
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
use ish::history::History;
use ish::ls;
use ish::prompt;

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
    bench_with_syscall_trace(bencher, run);
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
