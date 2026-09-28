//! Recovery benchmarks: ordinary versus protected removal, undo and redo,
//! how protected removal scales with the number of entries, and per-input
//! transaction overhead.
//!
//! Every sample gets a fresh fixture built outside the timed closure, and
//! validation reads happen after it. Protected removal is timed the way the
//! shell runs it: the removal, sealing the transaction, and the maintenance
//! pass that runs before the prompt comes back. Before the timed runs, `main`
//! prints a one-shot report of entry counts, preservation backends, bytes
//! copied in userspace, and where the time went for each scenario, so the
//! timings can be read against what actually happened. Sizes are bounded; the
//! sparse file is labeled separately because its timing says nothing about
//! data movement. The scaling runs print no pass or fail: the point is how the
//! seal column grows with entries. The 100,000-entry run takes a while to
//! build, so it is opt-in.
//! Run: `cargo bench --bench undo`, or
//! `ISH_UNDO_BENCH_LARGE=1 cargo bench --bench undo` for the largest tree.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ish_undo::testing::journal::Strength;
use ish_undo::testing::replay::{self, Model};
use ish_undo::testing::store::Home;
use ish_undo::{Config, Io, Session, maybe_collect, rm};
use rustybench::{AllocProfiler, Bencher, black_box};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const SMALL_FILES: usize = 2000;
/// Files per directory in the scaling trees.
const PER_DIR: usize = 1000;
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
        let store = ish_undo::testing::store::root_for_home(home.as_os_str());
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

    /// A tree of `entries` tiny files, `PER_DIR` to a directory.
    fn wide_tree(label: &str, entries: usize) -> Fixture {
        let fx = Fixture::new(label);
        for d in 0..entries.div_ceil(PER_DIR) {
            let dir = fx.work.join(format!("tree/d{d:04}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..PER_DIR.min(entries - d * PER_DIR) {
                fs::write(dir.join(format!("f{f:04}")), b"tiny file").unwrap();
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

    /// Run protected `rm -r` as the shell does: one transaction, sealed when
    /// the command finishes, followed by the maintenance pass that runs
    /// before the prompt returns. Returns the transaction id and how long
    /// the removal, the seal, and the maintenance pass took.
    fn protected_rm_timed(&self, target: &str) -> (u64, [std::time::Duration; 3]) {
        let config = Config::default();
        let mut session = Session::new();
        let txn = session.begin(self.store.clone(), config.clone(), &self.work, "rm -r");
        let mut out = std::io::sink();
        let mut err = std::io::stderr();
        let mut confirm = |_: &str| Some(true);
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            confirm: &mut confirm,
            cancel: &NEVER,
        };
        let t = Instant::now();
        let status = rm(&txn, &self.work, &["-r".into(), target.into()], &mut io);
        let removed = t.elapsed();
        assert_eq!(status, 0);
        let t = Instant::now();
        let id = session.finish(txn, status).expect("recorded");
        let sealed = t.elapsed();
        let t = Instant::now();
        maybe_collect(&self.store, &config);
        (id, [removed, sealed, t.elapsed()])
    }

    fn protected_rm(&self, target: &str) -> u64 {
        self.protected_rm_timed(target).0
    }

    fn replay(&self, id: u64, redo: bool) -> replay::Report {
        let home = Home::open_existing(&self.store).unwrap().unwrap();
        let opts = replay::Options {
            redo,
            dry_run: false,
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
        let fd = ish_undo::testing::sys::open_dir(&probe.work).unwrap();
        ish_undo::testing::sys::fs_type_name(std::os::fd::AsFd::as_fd(&fd))
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

    println!("protected rm as the shell runs it, by entry count (removal / seal / maintenance):");
    for entries in scale_sizes() {
        let fx = Fixture::wide_tree("report-scale", entries);
        let total = entries + entries.div_ceil(PER_DIR) + 1;
        let t = Instant::now();
        let (id, [removed, sealed, maintenance]) = fx.protected_rm_timed("tree");
        let to_prompt = t.elapsed();
        let (clone, link, copy, copied) = strengths(&fx, id);
        println!(
            "  {total:>7} entries  {removed:>10.2?} / {sealed:>10.2?} / {maintenance:>10.2?}  to prompt {to_prompt:>10.2?}  backends: {clone} clone, {link} linked, {copy} copy  copied: {copied} B"
        );
        let t = Instant::now();
        let r = fx.replay(id, false);
        assert!(r.conflicts.is_empty() && r.failures.is_empty(), "{r:?}");
        println!(
            "  {:>7} entries  undo {:>10.2?} ({} steps)",
            total,
            t.elapsed(),
            r.done
        );
    }
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

/// Entry counts of the scaling runs.
fn scale_sizes() -> Vec<usize> {
    let mut sizes = vec![1_000, 10_000];
    if std::env::var_os("ISH_UNDO_BENCH_LARGE").is_some() {
        sizes.push(100_000);
    }
    sizes
}

#[rustybench::bench(sample_count = 5, sample_size = 1)]
fn rm_protected_1k_entries_to_prompt(bencher: Bencher) {
    bencher
        .with_inputs(|| Fixture::wide_tree("scale-1k", 1_000))
        .bench_local_values(|fx| {
            black_box(fx.protected_rm("tree"));
            fx
        });
}

#[rustybench::bench(sample_count = 3, sample_size = 1)]
fn rm_protected_10k_entries_to_prompt(bencher: Bencher) {
    bencher
        .with_inputs(|| Fixture::wide_tree("scale-10k", 10_000))
        .bench_local_values(|fx| {
            black_box(fx.protected_rm("tree"));
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
