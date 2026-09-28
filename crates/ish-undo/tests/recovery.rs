//! Failure recovery: deterministic crashes at journal, preservation,
//! mutation, and replay boundaries, torn records, and injected copy, write,
//! sync, space, and cross-device failures. Recovery always runs in a fresh
//! process and never invents success or overwrites live data.

mod common;

use std::io::Write;

use common::*;
use ish_undo::testing::journal::Displaced;
use ish_undo::testing::replay::Committed;

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
    let mut stores = ish_undo::testing::store::Stores::new(fx.home_store(), id).unwrap();
    assert_eq!(
        ish_undo::testing::retention::usage(&mut stores, id).objects,
        4
    );
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
    let scan = ish_undo::testing::journal::read(&journal).unwrap();
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
        if let Ok(Some(home)) = ish_undo::testing::store::Home::open_existing(&fx.store) {
            for id in home.txn_ids().unwrap() {
                let mut stores = ish_undo::testing::store::Stores::new(home.clone(), id).unwrap();
                assert_eq!(
                    ish_undo::testing::retention::usage(&mut stores, id).objects,
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
fn cross_device_move_is_refused_and_moves_nothing() {
    let fx = Fixture::new("exdev");
    fx.write("src/a", b"a");
    fx.write("src/sub/b", b"b");
    // The rename reports EXDEV, as between filesystems.
    let (status, _, err) = fx.shell_with(
        SESSION,
        &["--fault", "rename=EXDEV"],
        &[&format!("mv:{}", argv(&["src", "dest"]))],
    );
    assert_eq!(status, 1, "{err}");
    assert!(
        err.contains("different filesystems") && err.contains("/bin/mv"),
        "the refusal names the escape hatch: {err}"
    );
    assert_eq!(fx.read("src/a"), b"a");
    assert_eq!(fx.read("src/sub/b"), b"b");
    assert!(!fx.exists("dest"));
    assert!(
        staging_entries(&fx).is_empty(),
        "nothing was staged, and nothing falls back to an unprotected move"
    );
    // The refused move changed nothing, so there is nothing to undo.
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 1);
    assert!(err.contains("nothing to undo"), "{err}");
}

/// Entries in the work directory that look like replay staging names.
fn staging_entries(fx: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&fx.work)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".ish-undo-"))
        .collect();
    names.sort();
    names
}

#[test]
fn crash_between_restore_and_publish_leaves_a_recorded_entry_that_is_cleaned_up() {
    let fx = Fixture::new("crash-publish");
    fx.write("f", b"data");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    // Files that only look like staging names: another tool's leftovers, and
    // the names the deterministic scheme used to pick for this transaction.
    let decoys = [
        ".ish-undo-decoy".to_string(),
        format!(".ish-undo-{id}-0"),
        format!(".ish-undo-{id}-0-0000000000000000"),
    ];
    for name in &decoys {
        fx.write(name, name.as_bytes());
    }
    // The restore's own rename is the first rename in the replay process.
    let (status, _, _) = fx.undo_with(SESSION, &["--fault", "rename=abort"], &[&id.to_string()]);
    assert_ne!(status, 0);
    assert!(!fx.exists("f"));
    let left: Vec<String> = staging_entries(&fx)
        .into_iter()
        .filter(|n| !decoys.contains(n))
        .collect();
    assert_eq!(
        left.len(),
        1,
        "the interrupted restore left its entry: {left:?}"
    );
    let pending = fx.model(id).actions[0].pending.clone();
    let stage = pending
        .and_then(|p| p.stage)
        .expect("the entry was journaled");
    assert!(
        stage.path.ends_with(left[0].as_bytes()),
        "the journal names the entry"
    );

    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"data");
    let mut expected: Vec<String> = decoys.to_vec();
    expected.sort();
    assert_eq!(
        staging_entries(&fx),
        expected,
        "only the recorded entry was removed"
    );
    for name in &decoys {
        assert_eq!(fx.read(name), name.as_bytes(), "{name} was not touched");
    }
}

#[test]
fn restore_never_removes_an_existing_file_with_a_staging_like_name() {
    let fx = Fixture::new("sentinel");
    for i in 0..4 {
        fx.write(&format!("d/f{i}"), format!("file {i}").as_bytes());
    }
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["d/f0", "d/f1", "d/f2", "d/f3"]))]);
    // A plain file at each name the old deterministic scheme would have used
    // for this transaction's steps.
    let sentinels: Vec<String> = (0..4).map(|a| format!("d/.ish-undo-{id}-{a}")).collect();
    for name in &sentinels {
        fx.write(name, b"sentinel");
    }
    for round in 0..2 {
        let (status, _, err) = fx.undo(&[&id.to_string()]);
        assert_eq!(status, 0, "round {round}: {err}");
        for i in 0..4 {
            assert_eq!(fx.read(&format!("d/f{i}")), format!("file {i}").as_bytes());
        }
        for name in &sentinels {
            assert_eq!(fx.read(name), b"sentinel", "round {round}: {name} survived");
        }
        let (status, _, err) = fx.undo(&["redo", &id.to_string()]);
        assert_eq!(status, 0, "round {round}: {err}");
        assert!(!fx.exists("d/f0"));
    }
}

#[test]
fn an_entry_that_is_not_the_recorded_staging_object_is_left_alone() {
    let fx = Fixture::new("stage-identity");
    fx.write("f", b"data");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    let (status, _, _) = fx.undo_with(SESSION, &["--fault", "rename=abort"], &[&id.to_string()]);
    assert_ne!(status, 0);
    let left = staging_entries(&fx);
    assert_eq!(left.len(), 1, "{left:?}");
    // Move the recorded entry away (keeping its inode alive) and put a
    // different file under the recorded name.
    std::fs::rename(fx.path(&left[0]), fx.path("kept-aside")).unwrap();
    fx.write(&left[0], b"someone else's file");

    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert!(err.contains("left in place"), "{err}");
    assert_eq!(fx.read("f"), b"data");
    assert_eq!(fx.read(&left[0]), b"someone else's file");
}

/// Undo `id` in a process that aborts at `point`, then check that a fresh
/// process finishes the job and that redo can bring the displaced state back.
fn interrupted_replay_keeps_what_it_displaced(
    label: &str,
    point: &str,
    setup: impl Fn(&Fixture) -> u64,
    displaced_size: u64,
    before: &[(&str, Option<&[u8]>)],
    after_undo: &[(&str, Option<&[u8]>)],
    after_redo: &[(&str, Option<&[u8]>)],
) {
    let check = |fx: &Fixture, want: &[(&str, Option<&[u8]>)], when: &str| {
        for (path, contents) in want {
            match contents {
                Some(c) => assert_eq!(fx.read(path), *c, "{label}: {path} {when}"),
                None => assert!(!fx.exists(path), "{label}: {path} {when}"),
            }
        }
    };
    let fx = Fixture::new(label);
    let id = setup(&fx);
    let (status, _, _) = fx.undo_with(
        SESSION,
        &["--fault", &format!("{point}=abort")],
        &[&id.to_string()],
    );
    assert_ne!(status, 0, "{label}: the replay was interrupted");
    check(&fx, before, "after the interrupted undo");

    // The journal holds the prepare record, displaced version included, and
    // no completion for the step.
    let model = fx.model(id);
    assert!(model.actions[0].last.is_none(), "{label}");
    let pending = model.actions[0]
        .pending
        .as_ref()
        .unwrap_or_else(|| panic!("{label}: the step was not prepared"));
    let Displaced::Saved(prepared) = &pending.displaced else {
        panic!(
            "{label}: nothing displaced was recorded: {:?}",
            pending.displaced
        );
    };
    assert_eq!(prepared.ident.size, displaced_size, "{label}");

    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{label}: {err}");
    check(&fx, after_undo, "after the retry");
    let model = fx.model(id);
    assert!(model.actions[0].pending.is_none(), "{label}: settled");
    let last = model.actions[0].last.as_ref().expect("completed");
    let Displaced::Saved(saved) = &last.displaced else {
        panic!(
            "{label}: the retry recorded no displaced version: {:?}",
            last.displaced
        );
    };
    assert_eq!(saved.ident.size, displaced_size, "{label}");
    assert!(staging_entries(&fx).is_empty(), "{label}");

    // Redo needs exactly what was displaced.
    let (status, _, err) = fx.undo(&["redo", &id.to_string()]);
    assert_eq!(status, 0, "{label}: {err}");
    check(&fx, after_redo, "after redo");
}

#[test]
fn crash_after_replacement_before_completion_keeps_the_displaced_version() {
    // The window the completion-time journal missed: the rename that put the
    // old contents back has happened, `StepDone` has not been written.
    interrupted_replay_keeps_what_it_displaced(
        "crash-replaced",
        "replay-mutated",
        |fx| {
            fx.write("f", b"one");
            let (status, id, err) = fx.shell(&["write:f=second"]);
            assert_eq!(status, 0, "{err}");
            id
        },
        b"second".len() as u64,
        &[("f", Some(b"one"))],
        &[("f", Some(b"one"))],
        &[("f", Some(b"second"))],
    );
}

#[test]
fn crash_after_removal_before_completion_keeps_the_displaced_version() {
    interrupted_replay_keeps_what_it_displaced(
        "crash-removed",
        "replay-mutated",
        |fx| {
            let (status, id, err) = fx.shell(&["write:new=generated"]);
            assert_eq!(status, 0, "{err}");
            id
        },
        b"generated".len() as u64,
        &[("new", None)],
        &[("new", None)],
        &[("new", Some(b"generated"))],
    );
}

#[test]
fn crash_after_the_prepare_record_before_any_mutation_is_retried_cleanly() {
    // Only the prepare record is durable: the live file is still the newer
    // one, and the interrupted restore left a recorded staging entry.
    interrupted_replay_keeps_what_it_displaced(
        "crash-prepared-replace",
        "replay-prepared",
        |fx| {
            fx.write("f", b"one");
            let (status, id, err) = fx.shell(&["write:f=second"]);
            assert_eq!(status, 0, "{err}");
            id
        },
        b"second".len() as u64,
        &[("f", Some(b"second"))],
        &[("f", Some(b"one"))],
        &[("f", Some(b"second"))],
    );
}

#[test]
fn crash_before_the_prepare_record_displaces_nothing() {
    let fx = Fixture::new("crash-before-prepare");
    fx.write("f", b"one");
    let (_, id, _) = fx.shell(&["write:f=second"]);
    // The displaced version is preserved first; dying while doing so leaves
    // an orphan object and the live file untouched.
    let (status, _, _) = fx.undo_with(
        SESSION,
        &[
            "--fault",
            "clone=abort",
            "--fault",
            "link=abort",
            "--fault",
            "copy=abort",
        ],
        &[&id.to_string()],
    );
    assert_ne!(status, 0);
    assert_eq!(fx.read("f"), b"second");
    assert!(fx.model(id).actions[0].pending.is_none());
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"one");
    let (status, _, err) = fx.undo(&["redo", &id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"second");
}

#[test]
fn a_conflict_after_an_interrupted_step_does_not_lose_its_displaced_version() {
    let fx = Fixture::new("conflict-after-crash");
    let (_, id, _) = fx.shell(&["write:new=generated"]);
    let (status, _, _) = fx.undo_with(
        SESSION,
        &["--fault", "replay-mutated=abort"],
        &[&id.to_string()],
    );
    assert_ne!(status, 0);
    assert!(!fx.exists("new"), "the interrupted undo removed the file");

    // Somebody creates something new at the path before the retry.
    fx.write("new", b"unrelated");
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 1, "{err}");
    assert_eq!(fx.read("new"), b"unrelated");
    assert!(fx.model(id).actions[0].pending.is_some());

    // Once the user clears the path, the retry settles the step with the
    // version the interrupted attempt displaced.
    std::fs::remove_file(fx.path("new")).unwrap();
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    let (status, _, err) = fx.undo(&["redo", &id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("new"), b"generated");
}

#[test]
fn a_step_that_prepares_and_then_fails_is_settled_not_left_pending() {
    // The restore's rename fails after the prepare record was written. The
    // step removes its staging entry and reports the failure: nothing about
    // it is unfinished.
    let fx = Fixture::new("settled-place");
    fx.write("f", b"data");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    let (status, _, err) = fx.undo_with(SESSION, &["--fault", "rename=EIO"], &[&id.to_string()]);
    assert_eq!(status, 1, "{err}");
    assert!(!fx.exists("f"));
    assert!(
        staging_entries(&fx).is_empty(),
        "the failed step cleaned up"
    );
    assert!(fx.model(id).actions[0].pending.is_none());
    let (_, show, _) = fx.undo(&["show", &id.to_string()]);
    assert!(!show.contains("interrupted"), "{show}");
    let (_, list, _) = fx.undo(&["list"]);
    assert!(!list.contains("replay interrupted"), "{list}");
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"data");
}

#[test]
fn a_failed_removal_does_not_lend_its_displaced_version_to_a_later_attempt() {
    let fx = Fixture::new("settled-remove");
    let (_, id, _) = fx.shell(&["write:new=generated"]);
    // The step preserves the file, prepares, and then its unlink fails.
    let (status, _, err) = fx.undo_with(SESSION, &["--fault", "unlink=EIO"], &[&id.to_string()]);
    assert_eq!(status, 1, "{err}");
    assert_eq!(fx.read("new"), b"generated");
    assert!(fx.model(id).actions[0].pending.is_none(), "settled");

    // The user removes the file themselves; the retry then finds the goal
    // state in place. It displaced nothing, so it records nothing: the
    // version prepared by the failed attempt is not the retry's to claim.
    std::fs::remove_file(fx.path("new")).unwrap();
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    let last = fx.model(id).actions[0].last.clone().expect("done");
    assert_eq!(last.displaced, Displaced::None);
    let (status, _, err) = fx.undo(&["redo", &id.to_string()]);
    assert_eq!(status, 1, "nothing was preserved to redo from: {err}");
    assert!(err.contains("was not preserved"), "{err}");
}
