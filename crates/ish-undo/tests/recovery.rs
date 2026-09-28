//! Failure recovery: deterministic crashes at journal, preservation,
//! mutation, and replay boundaries, torn records, and injected copy, write,
//! sync, space, and cross-device failures. Recovery always runs in a fresh
//! process and never invents success or overwrites live data.

mod common;

use std::io::Write;

use common::*;
use ish_undo::replay::Committed;

fn rm_tree(fx: &Fixture, n: usize) {
    for i in 0..n {
        fx.write(&format!("d/f{i:03}"), format!("file {i}").as_bytes());
    }
}

fn assert_tree(fx: &Fixture, n: usize) {
    for i in 0..n {
        assert_eq!(
            fx.read(&format!("d/f{i:03}")),
            format!("file {i}").as_bytes(),
            "d/f{i:03}"
        );
    }
}

fn only_txn(fx: &Fixture) -> u64 {
    let ids = fx.home_store().txn_ids().unwrap();
    assert_eq!(ids.len(), 1, "{ids:?}");
    ids[0]
}

#[test]
fn crash_after_prepare_before_mutation_leaves_data_and_an_ambiguous_record() {
    let fx = Fixture::new("crash-prepared");
    fx.write("f", b"data");
    let (status, _, _) = fx.shell_with(
        SESSION,
        &["--fault", "prepared=abort"],
        &[&format!("rm:{}", argv(&["f"]))],
    );
    assert_ne!(status, 0);
    assert_eq!(fx.read("f"), b"data", "nothing was removed");
    let id = only_txn(&fx);
    let model = fx.model(id);
    assert_eq!(model.actions[0].committed, Committed::Ambiguous);
    assert!(model.end.is_none());
    let (_, list, _) = fx.undo(&["list"]);
    assert!(list.contains("interrupted"), "{list}");
    // Recovery sees the file already in its pre-state and does nothing to it.
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"data");
}

#[test]
fn crash_between_mutation_and_commit_is_recovered() {
    let fx = Fixture::new("crash-commit");
    fx.write("f", b"data");
    // The second journal append is the commit record.
    let (status, _, _) = fx.shell_with(
        SESSION,
        &["--fault", "journal-append:1=abort"],
        &[&format!("rm:{}", argv(&["f"]))],
    );
    assert_ne!(status, 0);
    assert!(!fx.exists("f"));
    let id = only_txn(&fx);
    assert_eq!(fx.model(id).actions[0].committed, Committed::Ambiguous);
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"data");
}

#[test]
fn crash_in_the_middle_of_a_recursive_batch() {
    let fx = Fixture::new("crash-batch");
    rm_tree(&fx, 40);
    let (status, _, _) = fx.shell_with(
        SESSION,
        &["--fault", "unlink:17=abort"],
        &[&format!("rm:{}", argv(&["-r", "d"]))],
    );
    assert_ne!(status, 0);
    let present = (0..40).filter(|i| fx.exists(&format!("d/f{i:03}"))).count();
    assert_eq!(present, 40 - 17, "17 entries were removed before the crash");
    let id = only_txn(&fx);
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_tree(&fx, 40);
}

#[test]
fn crash_during_preservation_removes_nothing_and_keeps_orphans() {
    let fx = Fixture::new("crash-preserve");
    rm_tree(&fx, 10);
    let (status, _, _) = fx.shell_with(
        SESSION,
        &["--fault", "clone:4=abort", "--fault", "link:4=abort"],
        &[&format!("rm:{}", argv(&["-r", "d"]))],
    );
    assert_ne!(status, 0);
    assert_tree(&fx, 10);
    let id = only_txn(&fx);
    assert!(
        fx.model(id).actions.is_empty(),
        "no prepare record was written"
    );
    // The saved objects from before the crash stay as orphan candidates,
    // protected with their interrupted transaction.
    let mut stores = ish_undo::store::Stores::new(fx.home_store(), id).unwrap();
    assert_eq!(ish_undo::retention::usage(&mut stores, id).objects, 4);
    let (status, out, _) = fx.undo_with(SESSION, &["--config", "ISH_UNDO_MAX_ENTRIES=0"], &["gc"]);
    assert_eq!(status, 0);
    assert!(out.contains("deleted 0"), "{out}");
    assert!(fx.home_store().txn_dir(id).exists());
}

#[test]
fn crash_during_replay_resumes_without_repeating_steps() {
    let fx = Fixture::new("crash-replay");
    rm_tree(&fx, 12);
    let (status, id, err) = fx.shell(&[&format!("rm:{}", argv(&["-r", "d"]))]);
    assert_eq!(status, 0, "{err}");
    // Crash after the fifth completed replay step.
    let (status, _, _) = fx.undo_with(
        SESSION,
        &["--fault", "replay-step:4=abort"],
        &[&id.to_string()],
    );
    assert_ne!(status, 0);
    let restored: Vec<usize> = (0..12)
        .filter(|i| fx.exists(&format!("d/f{i:03}")))
        .collect();
    assert!(!restored.is_empty() && restored.len() < 12, "{restored:?}");
    // Edit an already restored file: the resumed run must leave it alone.
    let first = restored[0];
    std::fs::write(fx.path(&format!("d/f{first:03}")), b"edited").unwrap();

    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    for i in 0..12 {
        let want = if i == first {
            b"edited".to_vec()
        } else {
            format!("file {i}").into_bytes()
        };
        assert_eq!(fx.read(&format!("d/f{i:03}")), want);
    }
    let leftovers: Vec<_> = std::fs::read_dir(fx.path("d"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".ish-undo-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary restore names were cleaned up"
    );
}

#[test]
fn crash_between_restore_and_publish_is_cleaned_up() {
    let fx = Fixture::new("crash-publish");
    fx.write("f", b"data");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    // The restore's own rename is the first rename in the replay process.
    let (status, _, _) = fx.undo_with(SESSION, &["--fault", "rename=abort"], &[&id.to_string()]);
    assert_ne!(status, 0);
    assert!(!fx.exists("f"));
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"data");
    let names: Vec<String> = std::fs::read_dir(&fx.work)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["f".to_string()]);
}

#[test]
fn torn_journal_tail_is_ignored_then_trimmed() {
    let fx = Fixture::new("torn");
    fx.write("f", b"data");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    let journal = fx.home_store().journal_path(id);
    let clean_len = std::fs::metadata(&journal).unwrap().len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(&[40, 0, 0, 0, 1, 2, 3, 4, 9, 9])
        .unwrap();
    let (_, show, _) = fx.undo(&["show", &id.to_string()]);
    assert!(show.contains("incomplete"), "{show}");
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"data");
    let scan = ish_undo::journal::read(&journal).unwrap();
    assert_eq!(scan.torn_bytes, 0, "the next append cut the torn tail");
    assert!(std::fs::metadata(&journal).unwrap().len() > clean_len);
}

#[test]
fn journal_and_sync_failures_fail_closed() {
    for fault in ["journal-append=28", "sync=5", "prepared=28"] {
        let fx = Fixture::new("fail-closed");
        fx.write("f", b"data");
        let (status, _, err) = fx.shell_with(
            SESSION,
            &["--fault", fault],
            &[&format!("rm:{}", argv(&["f"]))],
        );
        assert_eq!(status, 1, "{fault}: {err}");
        assert_eq!(fx.read("f"), b"data", "{fault}: the file was kept");
        let (status, _, err) = fx.shell_with(SESSION, &["--fault", fault], &["write:f=replaced"]);
        assert_eq!(status, 1, "{fault}: {err}");
        assert_eq!(fx.read("f"), b"data", "{fault}: the file was not truncated");
    }
}

#[test]
fn copy_and_space_failures_during_preservation_fail_closed() {
    for fault in ["copy=5", "write=28"] {
        let fx = Fixture::new("copy-fail");
        fx.write("f", b"data that needs a frozen copy");
        let (status, _, err) = fx.shell_with(
            SESSION,
            &["--fault", "clone=ENOTSUP", "--fault", fault],
            &["write:f=replaced"],
        );
        assert_eq!(status, 1, "{fault}: {err}");
        assert_eq!(fx.read("f"), b"data that needs a frozen copy");
        // The failed copy left no partial object behind.
        if let Ok(Some(home)) = ish_undo::store::Home::open_existing(&fx.store) {
            for id in home.txn_ids().unwrap() {
                let mut stores = ish_undo::store::Stores::new(home.clone(), id).unwrap();
                assert_eq!(
                    ish_undo::retention::usage(&mut stores, id).objects,
                    0,
                    "{fault}"
                );
            }
        }
    }
}

#[test]
fn free_space_reserve_refuses_large_copies() {
    let fx = Fixture::new("reserve");
    fx.write("f", b"needs a copy");
    let (status, _, err) = fx.shell_with(
        SESSION,
        &[
            "--config",
            "ISH_UNDO_MIN_FREE=1000000T",
            "--fault",
            "clone=ENOTSUP",
        ],
        &["write:f=x"],
    );
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("free-space reserve"), "{err}");
    assert_eq!(fx.read("f"), b"needs a copy");
}

#[test]
fn cross_device_move_is_staged_published_then_removed() {
    let fx = Fixture::new("exdev");
    fx.write("src/a", b"a");
    fx.write("src/sub/b", b"b");
    std::os::unix::fs::symlink("a", fx.path("src/link")).unwrap();
    fx.write("dest", b"old dest");
    std::fs::remove_file(fx.path("dest")).unwrap();
    // The first rename reports EXDEV, as between filesystems.
    let (status, id, err) = fx.shell_with(
        SESSION,
        &["--fault", "rename=EXDEV"],
        &[&format!("mv:{}", argv(&["src", "dest"]))],
    );
    assert_eq!(status, 0, "{err}");
    assert!(!fx.exists("src"));
    assert_eq!(fx.read("dest/a"), b"a");
    assert_eq!(fx.read("dest/sub/b"), b"b");
    assert_eq!(
        std::fs::read_link(fx.path("dest/link"))
            .unwrap()
            .as_os_str(),
        "a"
    );
    let actions = fx.actions(id);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, ish_undo::journal::Action::Create { .. }))
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, ish_undo::journal::Action::Unlink { .. }))
    );

    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("src/a"), b"a");
    assert_eq!(fx.read("src/sub/b"), b"b");
    assert!(!fx.exists("dest"));
}

#[test]
fn failed_cross_device_publication_keeps_the_source() {
    let fx = Fixture::new("exdev-fail");
    fx.write("src/a", b"a");
    // EXDEV on the move, then the staging copy runs out of space.
    let (status, _, err) = fx.shell_with(
        SESSION,
        &["--fault", "rename=EXDEV", "--fault", "copy=28"],
        &[&format!("mv:{}", argv(&["src", "dest"]))],
    );
    assert_eq!(status, 1, "{err}");
    assert_eq!(
        fx.read("src/a"),
        b"a",
        "the source stays until publication succeeds"
    );
    assert!(!fx.exists("dest"));
    let staging: Vec<_> = std::fs::read_dir(&fx.work)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".ish-undo-mv-"))
        .collect();
    assert!(
        staging.is_empty(),
        "the unpublished staging copy was removed"
    );
}
