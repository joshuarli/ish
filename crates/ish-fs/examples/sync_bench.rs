use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use ish_fs::sync::{SyncOperands, SyncOptions, SyncRequest};
use ish_fs::{Cancellation, Context, Event};

fn main() -> io::Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp_root = repo_root.join(".work/tmp");
    fs::create_dir_all(&tmp_root)?;

    let fixture = tmp_root.join("sync-bench-fixture");
    let _ = fs::remove_dir_all(&fixture);
    fs::create_dir_all(fixture.join("subdir"))?;

    for i in 1..=200 {
        let content = vec![b'A'; 4096];
        fs::write(fixture.join("subdir").join(format!("file-{i}.bin")), &content)?;
    }
    let large = vec![b'B'; 1024 * 1024];
    fs::write(fixture.join("large.bin"), &large)?;

    let rsync_path = "/usr/bin/rsync";
    let rsync_ver = Command::new(rsync_path)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_owned())
        .unwrap_or_else(|_| "unknown".to_owned());

    println!("Fixture: 201 files, ~1.78 MiB under {}", fixture.display());
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

    println!("\n=== Workload 1: sync-fresh-tree ===");
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
                source: fixture.clone(),
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
            .args(["-a", &format!("{}/", fixture.display()), &format!("{}/", dst_ref.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_fresh_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture, &dst_ish)?;
        verify_trees(&fixture, &dst_ref)?;

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
            source: fixture.clone(),
            directory: dst_ish_noop.clone(),
        },
        options: SyncOptions::default(),
        context: context.clone(),
    };
    ish_fs::sync::execute(&req_seed, &cancel, &sink);
    Command::new(rsync_path)
        .args(["-a", &format!("{}/", fixture.display()), &format!("{}/", dst_ref_noop.display())])
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
            .args(["-a", &format!("{}/", fixture.display()), &format!("{}/", dst_ref_noop.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_noop_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture, &dst_ish_noop)?;
        verify_trees(&fixture, &dst_ref_noop)?;

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
        // seed
        let req_seed = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture.clone(),
                directory: dst_ish_up.clone(),
            },
            options: SyncOptions::default(),
            context: context.clone(),
        };
        ish_fs::sync::execute(&req_seed, &cancel, &sink);
        Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture.display()), &format!("{}/", dst_ref_up.display())])
            .status()?;

        // modify 1 file in fixture
        let modified_file = fixture.join("subdir/file-1.bin");
        fs::write(&modified_file, b"MODIFIED_CONTENT_SAMPLE")?;

        let t0 = Instant::now();
        let outcome = ish_fs::sync::execute(&req_seed, &cancel, &sink);
        assert_eq!(outcome.status(), 0);
        assert_eq!(outcome.progress.transferred_files, 1);
        let t_ish = t0.elapsed();
        ish_up_times.push(t_ish.as_secs_f64());

        let t0 = Instant::now();
        let status = Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture.display()), &format!("{}/", dst_ref_up.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_up_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture, &dst_ish_up)?;
        verify_trees(&fixture, &dst_ref_up)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());

        // restore file-1.bin
        let content = vec![b'A'; 4096];
        fs::write(&modified_file, &content)?;
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
        // seed
        let req_seed = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture.clone(),
                directory: dst_ish_del.clone(),
            },
            options: SyncOptions::default(),
            context: context.clone(),
        };
        ish_fs::sync::execute(&req_seed, &cancel, &sink);
        Command::new(rsync_path)
            .args(["-a", &format!("{}/", fixture.display()), &format!("{}/", dst_ref_del.display())])
            .status()?;

        // delete 5 files from fixture
        for f in 1..=5 {
            let _ = fs::remove_file(fixture.join("subdir").join(format!("file-{f}.bin")));
        }

        let t0 = Instant::now();
        let req_del = SyncRequest {
            operands: SyncOperands::Contents {
                source: fixture.clone(),
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
            .args(["-a", "--delete", &format!("{}/", fixture.display()), &format!("{}/", dst_ref_del.display())])
            .status()?;
        assert!(status.success());
        let t_ref = t0.elapsed();
        ref_del_times.push(t_ref.as_secs_f64());

        verify_trees(&fixture, &dst_ish_del)?;
        verify_trees(&fixture, &dst_ref_del)?;

        println!("Sample {sample}: ish = {:.4}s, rsync = {:.4}s", t_ish.as_secs_f64(), t_ref.as_secs_f64());

        // restore the 5 files
        for f in 1..=5 {
            let content = vec![b'A'; 4096];
            fs::write(fixture.join("subdir").join(format!("file-{f}.bin")), &content)?;
        }
    }
    let _ = fs::remove_dir_all(&dst_ish_del);
    let _ = fs::remove_dir_all(&dst_ref_del);

    let post_gate = Command::new("perf-gate-ready").arg("-1").status();
    if post_gate.map(|s| s.success()).unwrap_or(false) {
        println!("\nperf-gate-ready -1: pass (0)");
    } else {
        println!("\nperf-gate-ready -1: busy or unavailable (deferred)");
    }

    fn median(mut vals: Vec<f64>) -> f64 {
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        vals[vals.len() / 2]
    }

    println!("\n=== Final Benchmark Results (Medians across 5 samples) ===");
    println!("sync-fresh-tree:  ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_fresh_times.clone()), median(ref_fresh_times.clone()), median(ish_fresh_times) / median(ref_fresh_times));
    println!("sync-noop-tree:   ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_noop_times.clone()), median(ref_noop_times.clone()), median(ish_noop_times) / median(ref_noop_times));
    println!("sync-update-small: ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_up_times.clone()), median(ref_up_times.clone()), median(ish_up_times) / median(ref_up_times));
    println!("sync-delete:      ish = {:.4}s, rsync = {:.4}s (ratio: {:.2}x)", median(ish_del_times.clone()), median(ref_del_times.clone()), median(ish_del_times) / median(ref_del_times));

    let _ = fs::remove_dir_all(&fixture);
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
