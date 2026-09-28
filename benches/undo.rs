//! Recovery benchmarks: ordinary versus protected removal, undo and redo,
//! scoped checkpoints, and per-input transaction overhead.
//!
//! Every sample gets a fresh fixture built outside the timed closure, and
//! validation reads happen after it. Before the timed runs, `main` prints a
//! one-shot report of entry counts, preservation backends, and bytes copied
//! in userspace for each scenario, so the timings can be read against what
//! actually happened. Sizes are bounded; the sparse file is labeled
//! separately because its timing says nothing about data movement.
//! Run: `cargo bench --bench undo`.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ish_undo::journal::Strength;
use ish_undo::replay::{self, Model};
use ish_undo::store::Home;
use ish_undo::{Config, Io, Session, ops, scope};
use rustybench::{AllocProfiler, Bencher, black_box};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const SMALL_FILES: usize = 2000;
const SMALL_SIZE: usize = 4096;
const LARGE_SIZE: usize = 128 << 20;
const SPARSE_SIZE: u64 = 4 << 30;

static NEVER: AtomicBool = AtomicBool::new(false);

/// A private fixture root with separate work and home children.
struct Fixture {
    root: PathBuf,
    work: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "ish-undo-bench-{label}-{}-{stamp}",
            std::process::id()
        ));
        let work = root.join("work");
        let home = root.join("home");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&home).unwrap();
        let store = ish_undo::store::root_for_home(home.as_os_str());
        Fixture { root, work, store }
    }

    fn small_tree(label: &str) -> Fixture {
        let fx = Fixture::new(label);
        let data = vec![b's'; SMALL_SIZE];
        for d in 0..20 {
            let dir = fx.work.join(format!("tree/d{d:02}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..SMALL_FILES / 20 {
                fs::write(dir.join(format!("f{f:03}")), &data).unwrap();
            }
        }
        fx
    }

    fn large_file(label: &str) -> Fixture {
        let fx = Fixture::new(label);
        let chunk: Vec<u8> = (0..1 << 20).map(|i: u32| (i * 31 % 251) as u8).collect();
        let mut f = fs::File::create(fx.work.join("large.bin")).unwrap();
        for _ in 0..LARGE_SIZE >> 20 {
            std::io::Write::write_all(&mut f, &chunk).unwrap();
        }
        f.sync_all().unwrap();
        fx
    }

    fn sparse_file(label: &str) -> Fixture {
        let fx = Fixture::new(label);
        let f = fs::File::create(fx.work.join("sparse.img")).unwrap();
        f.set_len(SPARSE_SIZE).unwrap();
        rustix::io::pwrite(&f, &[1u8; 4096], SPARSE_SIZE / 2).unwrap();
        fx
    }

    /// Run protected `rm -r` as one sealed transaction; returns its id.
    fn protected_rm(&self, target: &str) -> u64 {
        let mut session = Session::new();
        let txn = session.begin(self.store.clone(), Config::default(), &self.work, "rm -r");
        let mut out = std::io::sink();
        let mut err = std::io::stderr();
        let mut confirm = |_: &str| Some(true);
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            confirm: &mut confirm,
            cancel: &NEVER,
        };
        let status = ops::rm(&txn, &self.work, &["-r".into(), target.into()], &mut io);
        assert_eq!(status, 0);
        session.finish(txn, status).expect("recorded")
    }

    fn replay(&self, id: u64, redo: bool) -> replay::Report {
        let home = Home::open_existing(&self.store).unwrap().unwrap();
        let opts = replay::Options {
            redo,
            force: false,
            dry_run: false,
            only: Vec::new(),
            copy_limit: Config::default().copy_limit,
            min_free: Config::default().min_free,
            cancel: &NEVER,
        };
        replay::run(&home, id, &opts).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        if self.root.parent() == Some(base.as_path())
            && self
                .root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ish-undo-bench-")
        {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn strengths(fx: &Fixture, id: u64) -> (usize, usize, usize, u64) {
    let home = Home::open_existing(&fx.store).unwrap().unwrap();
    let model = Model::load(&home, id).unwrap();
    let mut counts = (0, 0, 0, 0);
    for s in model.saved_versions() {
        match s.strength() {
            Some(Strength::Clone) => counts.0 += 1,
            Some(Strength::Link) => counts.1 += 1,
            Some(Strength::Copy) => counts.2 += 1,
            None => {}
        }
        counts.3 += s.copied;
    }
    counts
}

fn report_row(name: &str, entries: usize, fx: &Fixture, id: u64, elapsed: std::time::Duration) {
    let (clone, link, copy, copied) = strengths(fx, id);
    println!(
        "  {name:<28} {entries:>6} entries  {elapsed:>10.2?}  backends: {clone} clone, {link} linked, {copy} copy  copied: {copied} B"
    );
}

/// One-shot measurements of what each scenario actually did.
fn report() {
    let probe = Fixture::new("probe");
    let fs_type = {
        let fd = ish_undo::sys::open_dir(&probe.work).unwrap();
        ish_undo::sys::fs_type_name(std::os::fd::AsFd::as_fd(&fd))
    };
    println!(
        "ish-undo scenarios on {} {} ({fs_type}):",
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    let fx = Fixture::small_tree("report-small");
    let t = Instant::now();
    let id = fx.protected_rm("tree");
    report_row(
        "protected rm, small files",
        SMALL_FILES + 21,
        &fx,
        id,
        t.elapsed(),
    );
    let t = Instant::now();
    let r = fx.replay(id, false);
    let undo = t.elapsed();
    assert!(r.conflicts.is_empty());
    assert_eq!(
        fs::read(fx.work.join("tree/d07/f042")).unwrap().len(),
        SMALL_SIZE
    );
    println!(
        "  {:<28} {:>6} steps    {undo:>10.2?}",
        "undo, small files", r.done
    );
    let t = Instant::now();
    let r = fx.replay(id, true);
    println!(
        "  {:<28} {:>6} steps    {:>10.2?}",
        "redo, small files",
        r.done,
        t.elapsed()
    );

    let fx = Fixture::large_file("report-large");
    let t = Instant::now();
    let id = fx.protected_rm("large.bin");
    report_row("protected rm, 128 MiB file", 1, &fx, id, t.elapsed());
    let t = Instant::now();
    fx.replay(id, false);
    let elapsed = t.elapsed();
    assert_eq!(
        fs::metadata(fx.work.join("large.bin")).unwrap().len(),
        LARGE_SIZE as u64
    );
    println!(
        "  {:<28} {:>6} step     {elapsed:>10.2?}",
        "undo, 128 MiB file", 1
    );

    let fx = Fixture::sparse_file("report-sparse");
    let blocks = fs::metadata(fx.work.join("sparse.img")).unwrap().blocks();
    let t = Instant::now();
    let id = fx.protected_rm("sparse.img");
    report_row("protected rm, 4 GiB sparse", 1, &fx, id, t.elapsed());
    fx.replay(id, false);
    let after = fs::metadata(fx.work.join("sparse.img")).unwrap().blocks();
    println!(
        "  {:<28} blocks before {blocks}, after undo {after}",
        "sparse file allocation"
    );

    let fx = Fixture::small_tree("report-scope");
    let t = Instant::now();
    let cp = scope::begin(
        &fx.store,
        &Config::default(),
        1,
        std::process::id(),
        &fx.work,
        "bench",
        &fx.work.join("tree"),
        &NEVER,
    )
    .unwrap();
    let before = t.elapsed();
    let id = cp.id;
    let t = Instant::now();
    let summary = cp.finish(0, &NEVER).unwrap();
    let after = t.elapsed();
    let home = Home::open_existing(&fx.store).unwrap().unwrap();
    let model = Model::load(&home, id).unwrap();
    let (entries, cloned, copied) = model.checkpoint.unwrap();
    println!(
        "  {:<28} {entries:>6} entries  before {before:.2?}, after {after:.2?}  {cloned} cloned, {copied} B copied, {} changes",
        "scoped checkpoint",
        summary.created + summary.modified + summary.removed
    );
    println!();
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn rm_ordinary_small_files(bencher: Bencher) {
    bencher
        .with_inputs(|| Fixture::small_tree("plain-small"))
        .bench_local_values(|fx| {
            fs::remove_dir_all(fx.work.join("tree")).unwrap();
            fx
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn rm_protected_small_files(bencher: Bencher) {
    bencher
        .with_inputs(|| Fixture::small_tree("prot-small"))
        .bench_local_values(|fx| {
            black_box(fx.protected_rm("tree"));
            fx
        });
}

#[rustybench::bench(sample_count = 5, sample_size = 1)]
fn rm_ordinary_large_file(bencher: Bencher) {
    bencher
        .with_inputs(|| Fixture::large_file("plain-large"))
        .bench_local_values(|fx| {
            fs::remove_file(fx.work.join("large.bin")).unwrap();
            fx
        });
}

#[rustybench::bench(sample_count = 5, sample_size = 1)]
fn rm_protected_large_file(bencher: Bencher) {
    bencher
        .with_inputs(|| Fixture::large_file("prot-large"))
        .bench_local_values(|fx| {
            black_box(fx.protected_rm("large.bin"));
            fx
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn rm_protected_sparse_file(bencher: Bencher) {
    bencher
        .with_inputs(|| Fixture::sparse_file("prot-sparse"))
        .bench_local_values(|fx| {
            black_box(fx.protected_rm("sparse.img"));
            fx
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn undo_small_files(bencher: Bencher) {
    bencher
        .with_inputs(|| {
            let fx = Fixture::small_tree("undo-small");
            let id = fx.protected_rm("tree");
            (fx, id)
        })
        .bench_local_values(|(fx, id)| {
            black_box(fx.replay(id, false));
            fx
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn redo_small_files(bencher: Bencher) {
    bencher
        .with_inputs(|| {
            let fx = Fixture::small_tree("redo-small");
            let id = fx.protected_rm("tree");
            fx.replay(id, false);
            (fx, id)
        })
        .bench_local_values(|(fx, id)| {
            black_box(fx.replay(id, true));
            fx
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn scoped_checkpoint_small_files(bencher: Bencher) {
    bencher
        .with_inputs(|| Fixture::small_tree("scope-small"))
        .bench_local_values(|fx| {
            let cp = scope::begin(
                &fx.store,
                &Config::default(),
                1,
                std::process::id(),
                &fx.work,
                "bench",
                &fx.work.join("tree"),
                &NEVER,
            )
            .unwrap();
            black_box(cp.finish(0, &NEVER).unwrap().id);
            fx
        });
}

/// The per-input cost when nothing is mutated: no file I/O is expected.
#[rustybench::bench]
fn transaction_without_mutation(bencher: Bencher) {
    let fx = Fixture::new("txn-idle");
    let mut session = Session::new();
    bencher.bench_local(|| {
        let txn = session.begin(fx.store.clone(), Config::default(), &fx.work, "echo hi");
        black_box(session.finish(txn, 0))
    });
    assert!(
        !Path::new(&fx.store).exists(),
        "an idle input created the store"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--test" || a == "--list") {
        report();
    }
    rustybench::main();
}
