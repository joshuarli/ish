use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use ish_fs::sync::{SyncOperands, SyncOptions, SyncRequest};
use ish_fs::{Cancellation, Context, Event};

#[derive(Clone, Copy, Default)]
struct RusageStats {
    user_time: f64,
    sys_time: f64,
    max_rss_kb: i64,
}

fn get_rusage(who: libc::c_int) -> RusageStats {
    unsafe {
        let mut r = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(who, &mut r);
        let user_time = r.ru_utime.tv_sec as f64 + r.ru_utime.tv_usec as f64 / 1_000_000.0;
        let sys_time = r.ru_stime.tv_sec as f64 + r.ru_stime.tv_usec as f64 / 1_000_000.0;
        #[cfg(target_os = "macos")]
        let max_rss_kb = r.ru_maxrss / 1024;
        #[cfg(not(target_os = "macos"))]
        let max_rss_kb = r.ru_maxrss;
        RusageStats {
            user_time,
            sys_time,
            max_rss_kb,
        }
    }
}

fn median(mut vals: Vec<f64>) -> f64 {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    vals[vals.len() / 2]
}

fn main() -> io::Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp_root = repo_root.join(".work/tmp");
    fs::create_dir_all(&tmp_root)?;

    let rsync_path = "/usr/bin/rsync";
    let rsync_ver = Command::new(rsync_path)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_owned())
        .unwrap_or_else(|_| "unknown".to_owned());

    println!("Reference: {rsync_path} ({rsync_ver})");

    let gate_status = Command::new("perf-gate-ready").status();
    if gate_status.map(|s| s.success()).unwrap_or(false) {
        println!("perf-gate-ready: pass (0)");
    } else {
        println!("perf-gate-ready: not available or busy (deferred)");
    }

    let cancel = Cancellation::default();
    let sink = |_event: Event| Ok::<(), io::Error>(());
    let context = Context {
        cwd: tmp_root.clone(),
        home: None,
        scratch: tmp_root.join("scratch"),
    };

    println!("\nPreparing Tier 1 Fixture (Small Tree: 201 files, ~1.78 MiB)...");
    let fixture_small = tmp_root.join("sync-bench-fixture-small");
    let _ = fs::remove_dir_all(&fixture_small);
    fs::create_dir_all(fixture_small.join("subdir"))?;
    for i in 1..=200 {
        let content = vec![b'A'; 4096];
        fs::write(fixture_small.join("subdir").join(format!("file-{i}.bin")), &content)?;
    }
    fs::write(fixture_small.join("large.bin"), vec![b'B'; 1024 * 1024])?;

    println!("=== Workload 1: sync-fresh-tree ===");
    let mut ish_fresh_times = Vec::new();
    let mut ref_fresh_times = Vec::new();

    for sample in 1..=5 {
        let dst_ish = tmp_root.join(format!("dst-ish-fresh-{sample}"));
        let dst_ref = tmp_root.join(format!("dst-ref-fresh-{sample}"));
        let _ = fs::remove_dir_all(&dst_ish);
        let _ = fs::remove_dir_all(&dst_ref);

        let t0 = Instant::now();
        let req = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture_small.clone(),
                directory: dst_ish.clone(),
            },
            options: SyncOptions::default(),
            context: context.clone(),
        };
        fs::create_dir_all(&dst_ish)?;
        let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
        assert_eq!(outcome.status(), 0);
        let t_ish = t0.elapsed();
        ish_fresh_times.push(t_ish.as_secs_f64());

        let t0 = Instant::now();
        fs::create_dir_all(&dst_ref)?;
        let status = Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture_small.display()), &format!("{}/", dst_ref.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_fresh_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture_small, &dst_ish)?;
        verify_trees(&fixture_small, &dst_ref)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());
        let _ = fs::remove_dir_all(&dst_ish);
        let _ = fs::remove_dir_all(&dst_ref);
    }

    println!("\n=== Workload 2: sync-noop-tree ===");
    let dst_ish_noop = tmp_root.join("dst-ish-noop");
    let dst_ref_noop = tmp_root.join("dst-ref-noop");
    let _ = fs::remove_dir_all(&dst_ish_noop);
    let _ = fs::remove_dir_all(&dst_ref_noop);
    fs::create_dir_all(&dst_ish_noop)?;
    fs::create_dir_all(&dst_ref_noop)?;

    let req_seed = SyncRequest {
        operands: SyncOperands::Contents {
            source: fixture_small.clone(),
            directory: dst_ish_noop.clone(),
        },
        options: SyncOptions::default(),
        context: context.clone(),
    };
    ish_fs::sync::execute(&req_seed, &cancel, &sink);
    Command::new(rsync_path)
        .args(["-a", &format!("{}/", fixture_small.display()), &format!("{}/", dst_ref_noop.display())])
        .status()?;

    let mut ish_noop_times = Vec::new();
    let mut ref_noop_times = Vec::new();

    for sample in 1..=5 {
        let t0 = Instant::now();
        let outcome = ish_fs::sync::execute(&req_seed, &cancel, &sink);
        assert_eq!(outcome.status(), 0);
        assert_eq!(outcome.progress.transferred_files, 0);
        assert_eq!(outcome.progress.skipped_files, 201);
        let t_ish = t0.elapsed();
        ish_noop_times.push(t_ish.as_secs_f64());

        let t0 = Instant::now();
        let status = Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture_small.display()), &format!("{}/", dst_ref_noop.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_noop_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture_small, &dst_ish_noop)?;
        verify_trees(&fixture_small, &dst_ref_noop)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());
    }
    let _ = fs::remove_dir_all(&dst_ish_noop);
    let _ = fs::remove_dir_all(&dst_ref_noop);

    println!("\n=== Workload 3: sync-update-small ===");
    let dst_ish_up = tmp_root.join("dst-ish-update");
    let dst_ref_up = tmp_root.join("dst-ref-update");
    let _ = fs::remove_dir_all(&dst_ish_up);
    let _ = fs::remove_dir_all(&dst_ref_up);
    fs::create_dir_all(&dst_ish_up)?;
    fs::create_dir_all(&dst_ref_up)?;

    let mut ish_up_times = Vec::new();
    let mut ref_up_times = Vec::new();

    for sample in 1..=5 {
        let req_seed = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture_small.clone(),
                directory: dst_ish_up.clone(),
            },
            options: SyncOptions::default(),
            context: context.clone(),
        };
        ish_fs::sync::execute(&req_seed, &cancel, &sink);
        Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture_small.display()), &format!("{}/", dst_ref_up.display())])
            .status()?;

        let modified_file = fixture_small.join("subdir/file-1.bin");
        fs::write(&modified_file, b"MODIFIED_CONTENT_SAMPLE")?;

        let t0 = Instant::now();
        let outcome = ish_fs::sync::execute(&req_seed, &cancel, &sink);
        assert_eq!(outcome.status(), 0);
        assert_eq!(outcome.progress.transferred_files, 1);
        let t_ish = t0.elapsed();
        ish_up_times.push(t_ish.as_secs_f64());

        let t0 = Instant::now();
        let status = Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture_small.display()), &format!("{}/", dst_ref_up.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_up_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture_small, &dst_ish_up)?;
        verify_trees(&fixture_small, &dst_ref_up)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());
        fs::write(&modified_file, vec![b'A'; 4096])?;
    }
    let _ = fs::remove_dir_all(&dst_ish_up);
    let _ = fs::remove_dir_all(&dst_ref_up);

    println!("\n=== Workload 4: sync-delete ===");
    let dst_ish_del = tmp_root.join("dst-ish-delete");
    let dst_ref_del = tmp_root.join("dst-ref-delete");
    let _ = fs::remove_dir_all(&dst_ish_del);
    let _ = fs::remove_dir_all(&dst_ref_del);
    fs::create_dir_all(&dst_ish_del)?;
    fs::create_dir_all(&dst_ref_del)?;

    let mut ish_del_times = Vec::new();
    let mut ref_del_times = Vec::new();

    for sample in 1..=5 {
        let req_seed = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture_small.clone(),
                directory: dst_ish_del.clone(),
            },
            options: SyncOptions::default(),
            context: context.clone(),
        };
        ish_fs::sync::execute(&req_seed, &cancel, &sink);
        Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture_small.display()), &format!("{}/", dst_ref_del.display())])
            .status()?;

        for f in 1..=5 {
            let _ = fs::remove_file(fixture_small.join("subdir").join(format!("file-{f}.bin")));
        }

        let t0 = Instant::now();
        let req_del = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture_small.clone(),
                directory: dst_ish_del.clone(),
            },
            options: SyncOptions {
                delete: true,
                dry_run: false,
                jobs: None,
            },
            context: context.clone(),
        };
        let outcome = ish_fs::sync::execute(&req_del, &cancel, &sink);
        assert_eq!(outcome.status(), 0);
        assert_eq!(outcome.progress.deleted_files, 5);
        let t_ish = t0.elapsed();
        ish_del_times.push(t_ish.as_secs_f64());

        let t0 = Instant::now();
        let status = Command::new(rsync_path)
            .args(["-a", "--delete", &format!("{}/", fixture_small.display()), &format!("{}/", dst_ref_del.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_del_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture_small, &dst_ish_del)?;
        verify_trees(&fixture_small, &dst_ref_del)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());

        for f in 1..=5 {
            fs::write(fixture_small.join("subdir").join(format!("file-{f}.bin")), vec![b'A'; 4096])?;
        }
    }
    let _ = fs::remove_dir_all(&dst_ish_del);
    let _ = fs::remove_dir_all(&dst_ref_del);
    let _ = fs::remove_dir_all(&fixture_small);

    println!("\n=== Workload 5 (Scale): sync-flat-4k (4,096 files in 1 dir, 16 MiB) ===");
    let fixture_flat = tmp_root.join("sync-bench-fixture-flat-4k");
    let _ = fs::remove_dir_all(&fixture_flat);
    fs::create_dir_all(&fixture_flat)?;
    for i in 1..=4096 {
        fs::write(fixture_flat.join(format!("item-{i:05}.bin")), vec![b'Z'; 4096])?;
    }
    let mut ish_flat_times = Vec::new();
    let mut ref_flat_times = Vec::new();

    for sample in 1..=5 {
        let dst_ish = tmp_root.join(format!("dst-ish-flat-{sample}"));
        let dst_ref = tmp_root.join(format!("dst-ref-flat-{sample}"));
        let _ = fs::remove_dir_all(&dst_ish);
        let _ = fs::remove_dir_all(&dst_ref);

        let t0 = Instant::now();
        let req = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture_flat.clone(),
                directory: dst_ish.clone(),
            },
            options: SyncOptions::default(),
            context: context.clone(),
        };
        fs::create_dir_all(&dst_ish)?;
        let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
        assert_eq!(outcome.status(), 0);
        let t_ish = t0.elapsed();
        ish_flat_times.push(t_ish.as_secs_f64());

        let t0 = Instant::now();
        fs::create_dir_all(&dst_ref)?;
        let status = Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture_flat.display()), &format!("{}/", dst_ref.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_flat_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture_flat, &dst_ish)?;
        verify_trees(&fixture_flat, &dst_ref)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());
        let _ = fs::remove_dir_all(&dst_ish);
        let _ = fs::remove_dir_all(&dst_ref);
    }
    let _ = fs::remove_dir_all(&fixture_flat);

    println!("\n=== Workload 6 (Scale): sync-deep-96 (96 directory levels deep) ===");
    let fixture_deep = tmp_root.join("sync-bench-fixture-deep-96");
    let _ = fs::remove_dir_all(&fixture_deep);
    let mut curr = fixture_deep.clone();
    for d in 1..=96 {
        curr = curr.join(format!("d{d:02}"));
        fs::create_dir_all(&curr)?;
        fs::write(curr.join("leaf.txt"), format!("depth {d}"))?;
    }
    let mut ish_deep_times = Vec::new();
    let mut ref_deep_times = Vec::new();

    for sample in 1..=5 {
        let dst_ish = tmp_root.join(format!("dst-ish-deep-{sample}"));
        let dst_ref = tmp_root.join(format!("dst-ref-deep-{sample}"));
        let _ = fs::remove_dir_all(&dst_ish);
        let _ = fs::remove_dir_all(&dst_ref);

        let t0 = Instant::now();
        let req = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture_deep.clone(),
                directory: dst_ish.clone(),
            },
            options: SyncOptions::default(),
            context: context.clone(),
        };
        fs::create_dir_all(&dst_ish)?;
        let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
        assert_eq!(outcome.status(), 0);
        let t_ish = t0.elapsed();
        ish_deep_times.push(t_ish.as_secs_f64());

        let t0 = Instant::now();
        fs::create_dir_all(&dst_ref)?;
        let status = Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture_deep.display()), &format!("{}/", dst_ref.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_deep_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture_deep, &dst_ish)?;
        verify_trees(&fixture_deep, &dst_ref)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());
        let _ = fs::remove_dir_all(&dst_ish);
        let _ = fs::remove_dir_all(&dst_ref);
    }
    let _ = fs::remove_dir_all(&fixture_deep);

    println!("\n=== Workload 7 (Scale): sync-large-128m (1 x 128 MiB binary file) ===");
    let fixture_large = tmp_root.join("sync-bench-fixture-large-128m");
    let _ = fs::remove_dir_all(&fixture_large);
    fs::create_dir_all(&fixture_large)?;
    let large_file = fixture_large.join("payload.bin");
    {
        use std::io::Write;
        let mut f = fs::File::create(&large_file)?;
        let chunk = vec![0x5au8; 1024 * 1024];
        for _ in 0..128 {
            f.write_all(&chunk)?;
        }
    }
    let mut ish_large_times = Vec::new();
    let mut ref_large_times = Vec::new();

    for sample in 1..=5 {
        let dst_ish = tmp_root.join(format!("dst-ish-large-{sample}"));
        let dst_ref = tmp_root.join(format!("dst-ref-large-{sample}"));
        let _ = fs::remove_dir_all(&dst_ish);
        let _ = fs::remove_dir_all(&dst_ref);

        let t0 = Instant::now();
        let req = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture_large.clone(),
                directory: dst_ish.clone(),
            },
            options: SyncOptions::default(),
            context: context.clone(),
        };
        fs::create_dir_all(&dst_ish)?;
        let outcome = ish_fs::sync::execute(&req, &cancel, &sink);
        assert_eq!(outcome.status(), 0);
        let t_ish = t0.elapsed();
        ish_large_times.push(t_ish.as_secs_f64());

        let t0 = Instant::now();
        fs::create_dir_all(&dst_ref)?;
        let status = Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture_large.display()), &format!("{}/", dst_ref.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_large_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture_large, &dst_ish)?;
        verify_trees(&fixture_large, &dst_ref)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());
        let _ = fs::remove_dir_all(&dst_ish);
        let _ = fs::remove_dir_all(&dst_ref);
    }
    let _ = fs::remove_dir_all(&fixture_large);

    let post_gate = Command::new("perf-gate-ready").arg("-1").status();
    if post_gate.map(|s| s.success()).unwrap_or(false) {
        println!("\nperf-gate-ready -1: pass (0)");
    } else {
        println!("\nperf-gate-ready -1: busy or unavailable (deferred)");
    }

    let rusage_self = get_rusage(libc::RUSAGE_SELF);
    let rusage_children = get_rusage(libc::RUSAGE_CHILDREN);

    println!("\n=== Resource Usage (Cumulative) ===");
    println!("ish engine (in-process): user={:.4}s sys={:.4}s peak_rss={} KiB", rusage_self.user_time, rusage_self.sys_time, rusage_self.max_rss_kb);
    println!("rsync subprocesses:      user={:.4}s sys={:.4}s peak_rss={} KiB", rusage_children.user_time, rusage_children.sys_time, rusage_children.max_rss_kb);

    println!("\n=== Comprehensive Benchmark Results (Medians across 5 samples) ===");
    println!("Tier 1 (Small Tree: 201 files, ~1.78 MiB):");
    println!("  sync-fresh-tree:  ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_fresh_times.clone()), median(ref_fresh_times.clone()), median(ish_fresh_times) / median(ref_fresh_times));
    println!("  sync-noop-tree:   ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_noop_times.clone()), median(ref_noop_times.clone()), median(ish_noop_times) / median(ref_noop_times));
    println!("  sync-update-small: ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_up_times.clone()), median(ref_up_times.clone()), median(ish_up_times) / median(ref_up_times));
    println!("  sync-delete:      ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_del_times.clone()), median(ref_del_times.clone()), median(ish_del_times) / median(ref_del_times));
    println!("Tier 2 (Scale Workloads):");
    println!("  sync-flat-4k (4,096 files): ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_flat_times.clone()), median(ref_flat_times.clone()), median(ish_flat_times) / median(ref_flat_times));
    println!("  sync-deep-96 (96 levels):   ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_deep_times.clone()), median(ref_deep_times.clone()), median(ish_deep_times) / median(ref_deep_times));
    println!("  sync-large-128m (128 MiB):  ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_large_times.clone()), median(ref_large_times.clone()), median(ish_large_times) / median(ref_large_times));

    Ok(())
}

fn verify_trees(src: &Path, dst: &Path) -> io::Result<()> {
    let diff = Command::new("diff")
        .args(["-r", &src.to_string_lossy(), &dst.to_string_lossy()])
        .status()?;
    if !diff.success() {
        return Err(io::Error::other("trees differ"));
    }
    Ok(())
}
