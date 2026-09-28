//! Conditional replay: newer data is never discarded by default.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;

use common::*;

#[test]
fn same_path_recreation_blocks_restore_until_newer_change_is_undone() {
    let fx = Fixture::new("recreate");
    fx.write("f", b"first");
    let (_, rm_id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    let (_, write_id, _) = fx.shell(&["write:f=second"]);
    assert_ne!(rm_id, write_id);

    // Restoring the removal would overwrite the newer file.
    let (status, _, err) = fx.undo(&[&rm_id.to_string()]);
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("something newer is in the way"), "{err}");
    assert_eq!(fx.read("f"), b"second");

    // Plain undo picks the latest transaction of the session.
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert!(!fx.exists("f"));
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"first");
}

#[test]
fn edits_after_capture_are_conflicts_not_casualties() {
    let fx = Fixture::new("edit-after");
    let (_, id, _) = fx.shell(&["write:new=generated"]);
    std::fs::write(fx.path("new"), b"generated, then edited by hand").unwrap();
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 1, "{err}");
    // "contents changed" with a frozen post-image (clones), "modified"
    // with stat evidence.
    assert!(
        err.contains("changed afterward") || err.contains("modified afterward"),
        "{err}"
    );
    assert_eq!(fx.read("new"), b"generated, then edited by hand");
    let model = fx.model(id);
    assert!(model.actions.iter().any(|a| a.conflict.is_some()));
    assert!(
        err.contains(&format!("undo {id}")),
        "the retry is by id: {err}"
    );
}

#[test]
fn dry_run_shows_the_plan_and_changes_nothing() {
    let fx = Fixture::new("dry-run");
    fx.write("d/x", b"x");
    fx.write("d/y", b"y");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["-r", "d"]))]);
    let journal = fx.home_store().journal_path(id);
    let before = std::fs::read(&journal).unwrap();

    let (status, out, err) = fx.undo(&["--dry-run"]);
    assert_eq!(status, 0, "{err}");
    assert!(out.contains("dry run for transaction"), "{out}");
    for step in ["recreate directory", "restore", "d/x", "d/y"] {
        assert!(out.contains(step), "{step}: {out}");
    }
    assert!(!fx.exists("d"), "nothing was restored");
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        before,
        "a dry run writes no journal record"
    );

    // The real run performs exactly the planned steps.
    let planned = out.lines().filter(|l| l.starts_with("  ")).count();
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert!(
        err.contains(&format!("reverted {planned} changes")),
        "{err}"
    );
}

#[test]
fn dry_run_output_is_bounded() {
    let fx = Fixture::new("dry-run-bounded");
    for i in 0..260 {
        fx.write(&format!("d/f{i:03}"), b"x");
    }
    fx.shell(&[&format!("rm:{}", argv(&["-r", "d"]))]);
    let (status, out, err) = fx.undo(&["--dry-run"]);
    assert_eq!(status, 0, "{err}");
    // 260 files and their directory: 261 steps, of which 200 are shown.
    assert!(out.contains("... and 61 more"), "{out}");
    assert_eq!(
        out.lines().filter(|l| l.starts_with("  restore")).count(),
        199
    );
}

#[test]
fn edits_after_undo_block_redo_until_the_user_resolves_them() {
    let fx = Fixture::new("edit-after-undo");
    fx.write("f", b"original");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    std::fs::write(fx.path("f"), b"edited after undo").unwrap();

    // The edit is newer than anything redo recorded: redo reports a conflict
    // and leaves it alone.
    let (status, _, err) = fx.undo(&["redo"]);
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("conflict"), "{err}");
    assert_eq!(fx.read("f"), b"edited after undo");
    let (status, _, err) = fx.undo(&["redo"]);
    assert_eq!(status, 1, "a retry still conflicts: {err}");
    assert_eq!(fx.read("f"), b"edited after undo");

    // The user decides what happens to their edit, then retries by id.
    std::fs::remove_file(fx.path("f")).unwrap();
    let (status, _, err) = fx.undo(&["redo", &id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert!(!fx.exists("f"));
    assert_eq!(fx.model(id).redoable(), 0);
}

#[test]
fn removed_options_and_subcommands_are_refused() {
    let fx = Fixture::new("removed-options");
    fx.write("f", b"data");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    let id = id.to_string();
    for args in [
        vec!["--force", id.as_str()],
        vec!["-f", id.as_str()],
        vec!["--only", "f", id.as_str()],
        vec!["redo", "--force"],
        vec!["run", "--scope", ".", "--", "true"],
        vec!["diff", id.as_str()],
    ] {
        let (status, _, err) = fx.undo(&args);
        assert_eq!(status, 2, "{args:?}: {err}");
        assert!(err.contains("unexpected argument"), "{args:?}: {err}");
        let usage = err
            .split("usage:")
            .nth(1)
            .unwrap_or_else(|| panic!("{args:?}: {err}"));
        for gone in ["--only", "--force", "scope", "diff", "undo run"] {
            assert!(!usage.contains(gone), "{gone} is still advertised: {usage}");
        }
    }
    assert!(!fx.exists("f"), "refused commands changed nothing");
    let (status, _, err) = fx.undo(&[&id]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"data");
}

#[test]
fn rename_cycle_is_reversed() {
    let fx = Fixture::new("cycle");
    fx.write("a", b"A");
    fx.write("b", b"B");
    let (a, b) = (ino(&fx.path("a")), ino(&fx.path("b")));
    let (status, _, err) = fx.shell(&[
        &format!("mv:{}", argv(&["a", "t"])),
        &format!("mv:{}", argv(&["b", "a"])),
        &format!("mv:{}", argv(&["t", "b"])),
    ]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("a"), b"B");
    // Edits made after the moves travel with the objects.
    std::fs::write(fx.path("a"), b"B edited").unwrap();
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(ino(&fx.path("a")), a);
    assert_eq!(ino(&fx.path("b")), b);
    assert_eq!(fx.read("a"), b"A");
    assert_eq!(fx.read("b"), b"B edited");
    assert!(!fx.exists("t"));
}

#[test]
fn overlapping_transactions_from_two_shells() {
    let fx = Fixture::new("two-shells");
    let (_, first, _) = fx.shell_with(SESSION, &[], &["write:f=1"]);
    let (_, second, _) = fx.shell_with(OTHER_SESSION, &[], &["append:f=2"]);
    assert_eq!(fx.read("f"), b"12");

    // Session one's plain undo only sees its own transaction, which the
    // other shell changed afterward.
    let (status, _, err) = fx.undo_with(SESSION, &[], &[]);
    assert_eq!(status, 1, "{err}");
    assert_eq!(fx.read("f"), b"12");
    let (_, list, _) = fx.undo_with(SESSION, &[], &["list"]);
    assert!(list.contains(&format!("* {first:>5}")), "{list}");
    assert!(list.contains(&format!("  {second:>5}")), "{list}");

    let (status, _, err) = fx.undo_with(OTHER_SESSION, &[], &[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"1");
    let (status, _, err) = fx.undo_with(SESSION, &[], &[]);
    assert_eq!(status, 0, "{err}");
    assert!(!fx.exists("f"));
}

#[test]
fn partial_conflict_retry_does_not_repeat_completed_steps() {
    let fx = Fixture::new("retry");
    fx.write("a", b"a");
    fx.write("b", b"b");
    let (_, id, _) = fx.shell(&[&format!("rm:{}", argv(&["a", "b"]))]);
    std::fs::write(fx.path("b"), b"blocker").unwrap();

    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 1, "{err}");
    assert_eq!(fx.read("a"), b"a");
    assert_eq!(fx.read("b"), b"blocker");
    std::fs::write(fx.path("a"), b"a edited").unwrap();
    std::fs::remove_file(fx.path("b")).unwrap();

    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("a"), b"a edited", "completed step not repeated");
    assert_eq!(fx.read("b"), b"b");
}

#[test]
fn active_transactions_are_not_replayed() {
    let fx = Fixture::new("active");
    let mut child = fx
        .fixture()
        .args(["shell"])
        .arg(&fx.store)
        .arg(&fx.work)
        .arg(OTHER_SESSION.to_string())
        .args(["--hold", "write:f=held"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    lines.read_line(&mut line).unwrap();
    let id: u64 = line.trim().strip_prefix("ready ").unwrap().parse().unwrap();

    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 1);
    assert!(err.contains("still active"), "{err}");
    let (_, list, _) = fx.undo(&["list"]);
    assert!(list.contains("active"), "{list}");

    writeln!(child.stdin.take().unwrap()).unwrap();
    assert!(child.wait().unwrap().success());
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert!(!fx.exists("f"));
}

#[test]
fn repeated_writes_delete_and_recreate_in_one_input_reconcile() {
    let fx = Fixture::new("reconcile");
    fx.write("f", b"original");
    let (status, id, err) = fx.shell(&[
        "write:f=a",
        "append:f=b",
        &format!("rm:{}", argv(&["f"])),
        "write:f=c",
        &format!("mv:{}", argv(&["f", "g"])),
        "append:g=d",
    ]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("g"), b"cd");
    // The second write to the same inode needed no new pre-image.
    let saved = fx.model(id).saved_versions().count();
    assert_eq!(
        saved, 2,
        "one pre-image for the first write, one for the removal"
    );

    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("f"), b"original");
    assert!(!fx.exists("g"));
    let (status, _, err) = fx.undo(&["redo"]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("g"), b"cd");
    assert!(!fx.exists("f"));
}
