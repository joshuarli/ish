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
use ish_undo::testing::journal::Strength;
use ish_undo::testing::retention::collect;
use ish_undo::testing::store::{self, Home, VolumeState};

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
fn real_cross_device_move_is_refused_without_falling_back() {
    let Some(vol) = Volume::new("xdev") else {
        return skip("cross-device moves");
    };
    let fx = Fixture::new("xdev");
    fx.write("tree/a", b"a");
    fx.write("tree/sub/b", b"b");
    let dest = vol.0.join("tree");
    let (status, _, err) = fx.shell(&[&format!("mv:{}", argv(&["tree", dest.to_str().unwrap()]))]);
    assert_eq!(status, 1, "{err}");
    assert!(
        err.contains("different filesystems") && err.contains("/bin/mv"),
        "{err}"
    );
    assert_eq!(fx.read("tree/sub/b"), b"b", "the source is untouched");
    assert!(!dest.exists(), "nothing was copied to the other filesystem");
    // The refusal comes first: nobody is asked to confirm an overwrite that
    // cannot happen.
    std::fs::create_dir(&dest).unwrap();
    let (status, _, err) = fx.shell(&[&format!(
        "mv:{}",
        argv(&["-i", "tree", dest.to_str().unwrap()])
    )]);
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("different filesystems"), "{err}");
    assert!(!err.contains("asked:"), "{err}");
    std::fs::remove_dir(&dest).unwrap();
    // A rename within the destination filesystem is still protected.
    std::fs::create_dir(vol.0.join("from")).unwrap();
    std::fs::write(vol.0.join("from/f"), b"f").unwrap();
    let inside = vol.0.join("to");
    let (status, _, err) = fx.shell(&[&format!(
        "mv:{}",
        argv(&[
            vol.0.join("from").to_str().unwrap(),
            inside.to_str().unwrap()
        ])
    )]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(std::fs::read(inside.join("f")).unwrap(), b"f");
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
    ish_undo::testing::retention::maybe_collect(&fx.store, &config);
    assert_eq!(fx.home_store().txn_ids().unwrap().len(), 1);
    assert!(fx.store.join("gc-stamp").exists());
    fx.write("again", b"x");
    fx.shell(&[&format!("rm:{}", argv(&["again"]))]);
    ish_undo::testing::retention::maybe_collect(&fx.store, &config);
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

/// Start an undo of `id` that pauses after its first completed step. It holds
/// the transaction's replay lock the way any long replay does, and finishes
/// once a line is written to its stdin.
fn paused_undo(fx: &Fixture, id: u64) -> std::process::Child {
    let mut child = fx
        .fixture()
        .args(["--fault", "replay-step=block"])
        .arg("undo")
        .arg(&fx.store)
        .arg(&fx.work)
        .arg(SESSION.to_string())
        .arg(id.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "blocked replay-step");
    child
}

#[test]
fn collection_and_purge_leave_a_transaction_that_is_being_replayed() {
    let fx = Fixture::new("gc-replaying");
    fx.write("a", b"a");
    fx.write("b", b"b");
    // Older than the next one, so the keep-the-newest rule does not cover it.
    let (_, older, _) = fx.shell(&[&format!("rm:{}", argv(&["a", "b"]))]);
    fx.write("c", b"c");
    let (_, newer, _) = fx.shell(&[&format!("rm:{}", argv(&["c"]))]);
    let mut child = paused_undo(&fx, older);

    let (_, list, _) = fx.undo(&["list"]);
    let row = list
        .lines()
        .find(|l| {
            l.trim_start_matches(['*', ' '])
                .starts_with(&older.to_string())
        })
        .unwrap_or_else(|| panic!("{list}"));
    assert!(row.contains("replaying"), "{row}");
    let (status, _, err) = fx.undo(&[&older.to_string()]);
    assert_eq!(status, 1);
    assert!(err.contains("being replayed"), "{err}");

    // Every budget says it should go; the replay's lock says otherwise.
    let (status, out, err) =
        fx.undo_with(SESSION, &["--config", "ISH_UNDO_MAX_ENTRIES=0"], &["gc"]);
    assert_eq!(status, 0, "{err}");
    assert!(
        out.contains("deleted 0") && out.contains("kept 1 protected"),
        "{out}"
    );
    assert!(fx.home_store().txn_dir(older).exists());
    let (status, _, err) = fx.undo(&["purge", &older.to_string()]);
    assert_eq!(status, 1);
    assert!(err.contains("being replayed"), "{err}");
    assert!(fx.home_store().txn_dir(older).exists());

    // The replay was not disturbed.
    writeln!(child.stdin.take().unwrap()).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(fx.read("a"), b"a");
    assert_eq!(fx.read("b"), b"b");
    let (status, _, err) = fx.undo(&["purge", &older.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert!(!fx.home_store().txn_dir(older).exists());
    assert!(fx.home_store().txn_dir(newer).exists());
}

#[test]
fn a_replay_started_while_collection_holds_the_lock_is_refused() {
    let fx = Fixture::new("replay-vs-gc");
    fx.write("f", b"data");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    // Collection and purge hold the same lock for the whole deletion.
    let _lock = fx.home_store().lock_replay(id).unwrap().expect("free");
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 1);
    assert!(err.contains("being replayed"), "{err}");
    assert!(!fx.exists("f"));
}

#[test]
fn sealing_records_the_retained_size_for_maintenance() {
    let fx = Fixture::new("size-cache");
    let mut ids = Vec::new();
    for i in 0..4 {
        fx.write(&format!("f{i}"), &vec![b'x'; 1000]);
        let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&[&format!("f{i}")]))]);
        ids.push(id);
    }
    for id in &ids {
        let size = std::fs::read_to_string(fx.store.join("txn").join(id.to_string()).join("size"))
            .unwrap_or_else(|e| panic!("transaction {id} has no size file: {e}"));
        assert_eq!(size.trim(), "1000 sealed");
    }
    // Nothing is over budget, so a pass reads the size files, loads no
    // journal, and needs no work at all.
    let config = ish_undo::Config {
        min_free: 0,
        ..ish_undo::Config::default()
    };
    let report = collect(&fx.home_store(), &config, Some(0)).unwrap();
    assert_eq!(
        (report.work, report.more, report.deleted.len()),
        (0, false, 0),
        "{report:?}"
    );

    // Replay changes what a transaction retains, so its cached size is
    // dropped and recomputed rather than trusted.
    let (status, _, err) = fx.undo(&[&ids[0].to_string()]);
    assert_eq!(status, 0, "{err}");
    assert!(
        !fx.store
            .join("txn")
            .join(ids[0].to_string())
            .join("size")
            .exists()
    );
}

#[test]
fn bounded_maintenance_makes_progress_past_protected_transactions() {
    let fx = Fixture::new("gc-progress");
    // Two interrupted transactions at the old end: their shells died right
    // after the prepare record, so they are protected but cost a journal
    // load each until their size is known.
    for i in 0..2 {
        fx.write(&format!("stuck{i}"), b"x");
        fx.shell_with(
            SESSION,
            &["--fault", "prepared=abort"],
            &[&format!("rm:{}", argv(&[&format!("stuck{i}")]))],
        );
    }
    let mut done = Vec::new();
    for i in 0..4 {
        fx.write(&format!("f{i}"), b"x");
        let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&[&format!("f{i}")]))]);
        done.push(id);
    }
    let home = fx.home_store();
    let config = ish_undo::Config {
        max_entries: 1,
        min_free: 0,
        ..ish_undo::Config::default()
    };
    let mut passes = 0;
    loop {
        let report = collect(&home, &config, Some(2)).unwrap();
        assert!(
            report.work <= 2,
            "a pass stays within its budget: {report:?}"
        );
        passes += 1;
        if !report.more {
            break;
        }
        assert!(passes < 20, "maintenance never got past the protected ones");
    }
    assert!(passes > 1, "the budget really bounded the first pass");
    assert_eq!(
        home.txn_ids().unwrap(),
        vec![1, 2, done[3]],
        "interrupted transactions and the newest completed one remain"
    );
}

#[test]
fn unreadable_journals_are_kept_for_inspection_and_cost_work_once() {
    let fx = Fixture::new("gc-unreadable");
    fx.write("a", b"a");
    let (_, damaged, _) = fx.shell(&[&format!("rm:{}", argv(&["a"]))]);
    fx.write("b", b"b");
    fx.shell(&[&format!("rm:{}", argv(&["b"]))]);
    std::fs::write(fx.home_store().journal_path(damaged), b"not a journal").unwrap();
    std::fs::remove_file(fx.store.join("txn").join(damaged.to_string()).join("size")).unwrap();

    let home = fx.home_store();
    let config = ish_undo::Config {
        max_entries: 0,
        min_free: 0,
        ..ish_undo::Config::default()
    };
    let first = collect(&home, &config, Some(10)).unwrap();
    assert!(first.deleted.is_empty(), "{first:?}");
    assert_eq!(first.kept_protected, 1, "{first:?}");
    assert!(home.txn_dir(damaged).exists());
    // Its size is now known to be nothing, so later passes do not load it.
    let second = collect(&home, &config, Some(10)).unwrap();
    assert!(second.work < first.work, "{first:?} then {second:?}");
    assert!(home.txn_dir(damaged).exists());

    let (status, out, err) =
        fx.undo_with(SESSION, &["--config", "ISH_UNDO_MAX_ENTRIES=0"], &["gc"]);
    assert_eq!(status, 0, "{err}");
    assert!(out.contains("kept 1 protected"), "{out}");
    let (_, list, _) = fx.undo(&["list"]);
    assert!(list.contains("unreadable"), "{list}");
    assert!(
        list.contains(&format!("`undo purge {damaged}` removes it")),
        "{list}"
    );
}

#[test]
fn a_size_measured_during_a_replay_is_not_cached() {
    let fx = Fixture::new("size-during-replay");
    fx.write("a", b"a");
    fx.write("b", b"b");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["a", "b"]))]);
    let size = fx.store.join("txn").join(id.to_string()).join("size");
    assert!(size.exists());
    let mut child = paused_undo(&fx, id);
    // The replay dropped the cached size before appending anything, and it
    // holds the lock: measuring now may look but must not write.
    assert!(!size.exists());
    let config = ish_undo::Config {
        min_free: 0,
        ..ish_undo::Config::default()
    };
    let report = collect(&fx.home_store(), &config, Some(10)).unwrap();
    assert_eq!(report.work, 1, "the journal was loaded to measure it");
    assert!(!size.exists(), "a size read mid-replay was cached");

    writeln!(child.stdin.take().unwrap()).unwrap();
    assert!(child.wait().unwrap().success());
    // With the replay over, the next pass measures and remembers it.
    collect(&fx.home_store(), &config, Some(10)).unwrap();
    let text = std::fs::read_to_string(&size).unwrap();
    assert!(text.ends_with(" sealed\n"), "{text:?}");
}

#[test]
fn many_unsealed_transactions_at_the_new_end_do_not_stall_maintenance() {
    let fx = Fixture::new("gc-many-open");
    let mut completed = Vec::new();
    for i in 0..3 {
        fx.write(&format!("f{i}"), b"x");
        let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&[&format!("f{i}")]))]);
        completed.push(id);
    }
    // More interrupted transactions than one pass may examine, all newer
    // than every completed one.
    for i in 0..12 {
        fx.write(&format!("stuck{i}"), b"x");
        fx.shell_with(
            SESSION,
            &["--fault", "prepared=abort"],
            &[&format!("rm:{}", argv(&[&format!("stuck{i}")]))],
        );
    }
    let home = fx.home_store();
    let config = ish_undo::Config {
        max_entries: 1,
        min_free: 0,
        ..ish_undo::Config::default()
    };
    let mut passes = 0;
    loop {
        let report = collect(&home, &config, Some(3)).unwrap();
        assert!(report.work <= 3, "{report:?}");
        passes += 1;
        if !report.more {
            break;
        }
        assert!(passes < 40, "maintenance never got past the open ones");
    }
    // What remains: the newest completed transaction, kept whatever the
    // budgets say, and every interrupted one.
    let left = home.txn_ids().unwrap();
    assert_eq!(left.len(), 13, "{left:?}");
    assert_eq!(left[0], completed[2]);
    // Once they are known, a pass costs no journal loads for them.
    let report = collect(&home, &config, Some(0)).unwrap();
    assert_eq!((report.work, report.more), (0, false), "{report:?}");
}

#[test]
fn explicit_collection_reports_everything_it_left_alone() {
    let fx = Fixture::new("gc-census");
    fx.write("a", b"a");
    fx.shell(&[&format!("rm:{}", argv(&["a"]))]);
    fx.write("b", b"b");
    fx.shell_with(
        SESSION,
        &["--fault", "prepared=abort"],
        &[&format!("rm:{}", argv(&["b"]))],
    );
    // Nothing is over any budget, yet the interrupted transaction is one
    // that no budget could have collected.
    let (status, out, err) = fx.undo_with(SESSION, &["--config", "ISH_UNDO_MIN_FREE=0"], &["gc"]);
    assert_eq!(status, 0, "{err}");
    assert!(
        out.contains("deleted 0") && out.contains("kept 1 protected"),
        "{out}"
    );
}
