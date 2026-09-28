//! Stores and retention: volume registration, missing and reappearing
//! stores, cross-device paths, capability fallback, budgets, protection of
//! active transactions, and reference-safe GC and purge.
//!
//! Tests that need a second filesystem use `ISH_UNDO_TEST_VOLUME`, a
//! writable directory on a different filesystem than the temp directory (the
//! Linux container run mounts a tmpfs for it). Without it they are skipped
//! explicitly; nothing here mounts or formats a volume.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Stdio;

use common::*;
use ish_undo::journal::Strength;
use ish_undo::store::{self, Home, VolumeState};

/// A private directory on the second test filesystem, removed on drop.
struct Volume(PathBuf);

impl Volume {
    fn new(label: &str) -> Option<Volume> {
        let base = PathBuf::from(std::env::var_os("ISH_UNDO_TEST_VOLUME")?);
        let dir = base.join(format!("ish-undo-vol-{label}-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        Some(Volume(dir))
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn skip(what: &str) {
    eprintln!("skipping {what}: set ISH_UNDO_TEST_VOLUME to a directory on another filesystem");
}

#[test]
fn same_filesystem_volume_is_rejected() {
    let fx = Fixture::new("vol-same");
    std::fs::create_dir(fx.path("mnt")).unwrap();
    let (status, _, err) = fx.undo(&["volume", "add", "mnt"]);
    assert_eq!(status, 1);
    assert!(err.contains("same filesystem as the home store"), "{err}");
}

#[test]
fn registry_tracks_missing_mismatched_and_reappearing_stores() {
    let fx = Fixture::new("vol-state");
    let home = Home::open_or_create(&fx.store).unwrap();
    std::fs::create_dir(fx.path("disk")).unwrap();
    let entry = store::create_volume(&home, &fx.path("disk")).unwrap();
    home.write_registry(std::slice::from_ref(&entry), &home.lock().unwrap())
        .unwrap();

    let (_, out, _) = fx.undo(&["volume", "list"]);
    assert!(out.contains("available"), "{out}");
    std::fs::rename(fx.path("disk"), fx.path("disk-away")).unwrap();
    assert_eq!(entry.state(home.id), VolumeState::Missing);
    let (_, out, _) = fx.undo(&["volume", "list"]);
    assert!(out.contains("unavailable (not mounted)"), "{out}");

    // Another store at the same path is not ours.
    std::fs::create_dir(fx.path("disk")).unwrap();
    let other = Home::open_or_create(&fx.root.join("other-store")).unwrap();
    store::create_volume(&other, &fx.path("disk")).unwrap();
    assert!(matches!(entry.state(home.id), VolumeState::Mismatch(_)));
    let (status, _, err) = fx.undo(&["volume", "add", "disk"]);
    assert_eq!(status, 1);
    assert!(
        err.contains("belongs to another") || err.contains("same filesystem"),
        "{err}"
    );

    std::fs::remove_dir_all(fx.path("disk")).unwrap();
    std::fs::rename(fx.path("disk-away"), fx.path("disk")).unwrap();
    assert!(matches!(
        entry.state(home.id),
        VolumeState::Available { .. }
    ));
}

#[test]
fn off_home_filesystem_uses_bounded_copy_or_refuses_without_a_store() {
    let Some(vol) = Volume::new("nostore") else {
        return skip("off-home copies");
    };
    let fx = Fixture::new("off-home");
    std::fs::write(vol.0.join("small"), b"small file").unwrap();
    std::fs::write(vol.0.join("big"), vec![1u8; 64 * 1024]).unwrap();
    let small = vol.0.join("small");
    let (status, id, err) = fx.shell_with(
        SESSION,
        &["--config", "ISH_UNDO_COPY_LIMIT=16K"],
        &[&format!("rm:{}", argv(&[small.to_str().unwrap()]))],
    );
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        fx.strengths(id),
        vec![Strength::Copy],
        "no store there: an independent copy"
    );
    let big = vol.0.join("big");
    let (status, _, err) = fx.shell_with(
        SESSION,
        &["--config", "ISH_UNDO_COPY_LIMIT=16K"],
        &[&format!("rm:{}", argv(&[big.to_str().unwrap()]))],
    );
    assert_eq!(status, 1);
    assert!(err.contains("copy limit"), "{err}");
    assert!(big.exists());
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(std::fs::read(&small).unwrap(), b"small file");
}

#[test]
fn registered_volume_store_holds_versions_and_survives_disconnection() {
    let Some(vol) = Volume::new("store") else {
        return skip("volume stores");
    };
    let fx = Fixture::new("vol-store");
    let (status, out, err) = fx.undo(&["volume", "add", vol.0.to_str().unwrap()]);
    assert_eq!(status, 0, "{err}");
    assert!(out.contains("registered"), "{out}");
    std::fs::write(vol.0.join("f"), b"on the volume").unwrap();
    let target = vol.0.join("f");
    let (status, id, err) = fx.shell(&[&format!("rm:{}", argv(&[target.to_str().unwrap()]))]);
    assert_eq!(status, 0, "{err}");
    let strength = fx.strengths(id)[0];
    assert!(
        matches!(strength, Strength::Link | Strength::Clone),
        "{strength:?}"
    );
    let volume_txn = vol.0.join(".ish-undo/txn").join(id.to_string());
    assert!(
        volume_txn.join("owner").exists(),
        "owner metadata kept with the payload"
    );
    assert!(std::fs::read_dir(volume_txn.join("obj")).unwrap().count() == 1);

    // Disconnect the volume: the transaction becomes unavailable, not lost.
    let store_dir = vol.0.join(".ish-undo");
    let away = vol.0.join(".ish-undo-away");
    std::fs::rename(&store_dir, &away).unwrap();
    let (_, list, _) = fx.undo(&["list"]);
    assert!(list.contains("unavailable"), "{list}");
    let (status, out, _) = fx.undo_with(SESSION, &["--config", "ISH_UNDO_MAX_ENTRIES=0"], &["gc"]);
    assert_eq!(status, 0);
    assert!(out.contains("deleted 0"), "{out}");
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 1, "{err}");
    assert!(!target.exists());

    // Reconnect: recovery works again.
    std::fs::rename(&away, &store_dir).unwrap();
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(std::fs::read(&target).unwrap(), b"on the volume");

    // The store cannot be unregistered while a transaction uses it.
    let (status, _, err) = fx.undo(&["volume", "remove", vol.0.to_str().unwrap()]);
    assert_eq!(status, 1);
    assert!(err.contains(&id.to_string()), "{err}");
    let (status, _, err) = fx.undo(&["purge", &id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert!(
        !volume_txn.exists(),
        "purge removed the volume-side payload"
    );
    let (status, _, err) = fx.undo(&["volume", "remove", vol.0.to_str().unwrap()]);
    assert_eq!(status, 0, "{err}");
}

#[test]
fn orphans_left_on_a_disconnected_volume_are_collected_later() {
    let Some(vol) = Volume::new("orphans") else {
        return skip("volume orphans");
    };
    let fx = Fixture::new("vol-orphans");
    fx.undo(&["volume", "add", vol.0.to_str().unwrap()]);
    std::fs::write(vol.0.join("f"), b"x").unwrap();
    let target = vol.0.join("f");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&[target.to_str().unwrap()]))]);
    let store_dir = vol.0.join(".ish-undo");
    let away = vol.0.join(".ish-undo-away");
    std::fs::rename(&store_dir, &away).unwrap();
    let (status, _, err) = fx.undo(&["purge", &id.to_string()]);
    assert_eq!(status, 0, "{err}");
    std::fs::rename(&away, &store_dir).unwrap();
    assert!(store_dir.join("txn").join(id.to_string()).exists());
    let (status, out, _) = fx.undo(&["gc"]);
    assert_eq!(status, 0);
    assert!(out.contains("removed 1 orphaned"), "{out}");
    assert!(!store_dir.join("txn").join(id.to_string()).exists());
}

#[test]
fn real_cross_device_move_round_trips() {
    let Some(vol) = Volume::new("xdev") else {
        return skip("cross-device moves");
    };
    let fx = Fixture::new("xdev");
    fx.write("tree/a", b"a");
    fx.write("tree/sub/b", b"b");
    let dest = vol.0.join("tree");
    let (status, _, err) = fx.shell(&[&format!("mv:{}", argv(&["tree", dest.to_str().unwrap()]))]);
    assert_eq!(status, 0, "{err}");
    assert!(!fx.exists("tree"));
    assert_eq!(std::fs::read(dest.join("sub/b")).unwrap(), b"b");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("tree/sub/b"), b"b");
    assert!(!dest.exists());
}

#[test]
fn retention_budgets_collect_oldest_first_and_keep_the_newest() {
    let fx = Fixture::new("budget");
    let mut ids = Vec::new();
    for i in 0..5 {
        fx.write(&format!("f{i}"), &vec![b'x'; 1000]);
        let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&[&format!("f{i}")]))]);
        ids.push(id);
    }
    let (status, out, _) = fx.undo_with(SESSION, &["--config", "ISH_UNDO_MAX_ENTRIES=2"], &["gc"]);
    assert_eq!(status, 0);
    assert!(out.contains("deleted 3"), "{out}");
    let left = fx.home_store().txn_ids().unwrap();
    assert_eq!(left, ids[3..].to_vec());

    // A byte budget smaller than one transaction still keeps the newest.
    let (_, out, _) = fx.undo_with(SESSION, &["--config", "ISH_UNDO_MAX_SIZE=1"], &["gc"]);
    assert!(out.contains("deleted 1"), "{out}");
    assert_eq!(fx.home_store().txn_ids().unwrap(), vec![ids[4]]);
    // Age budget.
    let (_, out, _) = fx.undo_with(SESSION, &["--config", "ISH_UNDO_MAX_DAYS=0"], &["gc"]);
    assert!(out.contains("deleted 0"), "{out}");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert!(fx.exists("f4"));
}

#[test]
fn active_transactions_are_protected_from_collection() {
    let fx = Fixture::new("gc-active");
    let mut child = fx
        .fixture()
        .arg("shell")
        .arg(&fx.store)
        .arg(&fx.work)
        .arg(OTHER_SESSION.to_string())
        .args(["--hold", "write:held=1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let held: u64 = line.trim().strip_prefix("ready ").unwrap().parse().unwrap();
    fx.shell(&["write:other=2"]);
    let (_, out, _) = fx.undo_with(SESSION, &["--config", "ISH_UNDO_MAX_ENTRIES=0"], &["gc"]);
    assert!(out.contains("kept 1 protected"), "{out}");
    assert!(fx.home_store().txn_dir(held).exists());
    let (status, _, err) = fx.undo(&["purge", &held.to_string()]);
    assert_eq!(status, 1);
    assert!(err.contains("still active"), "{err}");
    writeln!(child.stdin.take().unwrap()).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn purge_deletes_only_owned_objects_and_never_live_data() {
    let fx = Fixture::new("purge");
    fx.write("linked", b"live data");
    fx.write("other", b"other data");
    // A linked version shares its inode with the file once it is restored.
    let (_, linked_id, _) = fx.shell_with(
        SESSION,
        &["--fault", "clone=ENOTSUP"],
        &[&format!("rm:{}", argv(&["linked"]))],
    );
    let (_, other_id, _) = fx.shell(&[&format!("rm:{}", argv(&["other"]))]);
    let (status, _, err) = fx.undo(&[&linked_id.to_string()]);
    assert_eq!(status, 0, "{err}");
    let (status, _, err) = fx.undo(&["purge", &linked_id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        fx.read("linked"),
        b"live data",
        "purging a linked version keeps the live file"
    );
    assert!(!fx.home_store().txn_dir(linked_id).exists());
    let (status, _, err) = fx.undo(&[&other_id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        fx.read("other"),
        b"other data",
        "other transactions are intact"
    );
}

#[test]
fn command_boundary_maintenance_is_rate_limited() {
    let fx = Fixture::new("maintenance");
    for i in 0..3 {
        fx.write(&format!("f{i}"), b"x");
        fx.shell(&[&format!("rm:{}", argv(&[&format!("f{i}")]))]);
    }
    let config = ish_undo::Config {
        max_entries: 1,
        ..ish_undo::Config::default()
    };
    ish_undo::retention::maybe_collect(&fx.store, &config);
    assert_eq!(fx.home_store().txn_ids().unwrap().len(), 1);
    assert!(fx.store.join("gc-stamp").exists());
    fx.write("again", b"x");
    fx.shell(&[&format!("rm:{}", argv(&["again"]))]);
    ish_undo::retention::maybe_collect(&fx.store, &config);
    assert_eq!(
        fx.home_store().txn_ids().unwrap().len(),
        2,
        "not due again yet"
    );
}

#[test]
fn doctor_reports_probe_results() {
    let fx = Fixture::new("doctor");
    fx.write("f", b"x");
    fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    let (status, out, err) = fx.undo(&["doctor"]);
    assert_eq!(status, 0, "{err}");
    for probe in ["clone", "hard link", "no-replace rename", "xattr", "fsync"] {
        assert!(out.contains(probe), "{probe}: {out}");
    }
    let clone_line = if clone_supported(&fx.work) {
        "clone ok"
    } else {
        "clone failed"
    };
    assert!(out.contains(clone_line), "{out}");
    assert!(out.contains("mode 700"), "{out}");
}
