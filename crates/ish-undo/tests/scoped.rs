//! Scoped execution: `undo run --scope <dir> -- <program>` records what an
//! arbitrary external program changed under the scope, including through
//! child processes, and replays it like any other transaction.

mod common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use common::*;

const FIXTURE: &str = env!("CARGO_BIN_EXE_ish-undo-fixture");

fn tree(fx: &Fixture) {
    fx.write("proj/keep", b"keep");
    fx.write("proj/same", b"AAAA");
    fx.write("proj/old", b"old name");
    fx.write("proj/del", b"deleted later");
    fx.write("proj/dir/inner", b"inner");
    fx.write("proj/.git/HEAD", b"ref: main\n");
}

fn snapshot(fx: &Fixture) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![fx.path("proj")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let rel = entry
                .path()
                .strip_prefix(fx.path("proj"))
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if entry.file_type().unwrap().is_dir() {
                out.push((rel + "/", Vec::new()));
                stack.push(entry.path());
            } else {
                out.push((rel, std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    out.sort();
    out
}

fn run(fx: &Fixture, pre: &[&str], steps: &[&str]) -> (i32, String, String) {
    let proj = fx.path("proj");
    let mut args = vec![
        "run",
        "--scope",
        "proj",
        "--",
        FIXTURE,
        "mutate",
        proj.to_str().unwrap(),
    ];
    args.extend_from_slice(steps);
    fx.undo_with(SESSION, pre, &args)
}

#[test]
fn child_process_changes_are_recorded_and_reversed() {
    let fx = Fixture::new("scoped");
    tree(&fx);
    let before = snapshot(&fx);
    let (status, _, err) = run(
        &fx,
        &[],
        &[
            "child:create/new=hello",
            "rewrite:same=BBBB",
            "child:rename/old=renamed",
            "delete:del",
            "mkdir:newdir",
            "child:create/newdir/f=nested",
            "rewrite:.git/HEAD=ref: dev!\n",
        ],
    );
    assert_eq!(status, 0, "{err}");
    assert!(err.contains("created"), "{err}");
    let after = snapshot(&fx);
    assert_ne!(before, after);

    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        snapshot(&fx),
        before,
        "scope restored, including .git and the same-size rewrite"
    );

    let (status, _, err) = fx.undo(&["redo"]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(snapshot(&fx), after);
}

#[test]
fn size_and_mtime_preserving_rewrite_is_not_missed() {
    let fx = Fixture::new("sneaky");
    fx.write("proj/same", b"AAAA");
    let old = std::fs::metadata(fx.path("proj/same")).unwrap();
    let (status, _, err) = run(&fx, &[], &["rewrite:same=BBBB"]);
    assert_eq!(status, 0, "{err}");
    let new = std::fs::metadata(fx.path("proj/same")).unwrap();
    assert_eq!(
        (old.len(), old.mtime(), old.mtime_nsec()),
        (new.len(), new.mtime(), new.mtime_nsec())
    );
    assert!(err.contains("1 modified"), "{err}");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("proj/same"), b"AAAA");
}

#[test]
fn failed_program_still_recovers() {
    let fx = Fixture::new("failed");
    tree(&fx);
    let before = snapshot(&fx);
    let (status, _, err) = run(&fx, &[], &["delete:keep", "create:half=written", "fail"]);
    assert_eq!(status, 3, "the program's status is returned: {err}");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(snapshot(&fx), before);
}

#[test]
fn interrupted_checkpoint_is_finished_on_recovery() {
    let fx = Fixture::new("scope-crash");
    tree(&fx);
    let before = snapshot(&fx);
    // The shell dies after the program exits but before the
    // after-checkpoint is recorded.
    let (status, _, _) = run(
        &fx,
        &["--fault", "scope-finish=abort"],
        &["delete:keep", "create:new=1"],
    );
    assert_ne!(status, 0);
    let (_, list, _) = fx.undo(&["list"]);
    assert!(list.contains("interrupted"), "{list}");
    let id = fx.home_store().txn_ids().unwrap()[0];
    let (status, _, err) = fx.undo(&[&id.to_string()]);
    assert_eq!(status, 0, "{err}");
    assert!(err.contains("interrupted"), "{err}");
    assert_eq!(snapshot(&fx), before);
}

#[test]
fn checkpoint_problems_are_refused_before_launch() {
    let fx = Fixture::new("refuse-scope");
    tree(&fx);
    let marker = fx.path("proj/launched");
    let launched = "create:launched=yes".to_string();

    // Budget: more entries than allowed.
    let (status, _, err) = run(&fx, &["--config", "ISH_UNDO_SCOPE_LIMIT=3"], &[&launched]);
    assert_eq!(status, 1);
    assert!(
        err.contains("ISH_UNDO_SCOPE_LIMIT") && err.contains("not started"),
        "{err}"
    );
    assert!(!marker.exists());

    // Without cloning, the byte copies exceed the copy limit.
    let (status, _, err) = run(
        &fx,
        &[
            "--config",
            "ISH_UNDO_COPY_LIMIT=8",
            "--fault",
            "clone=ENOTSUP",
        ],
        &[&launched],
    );
    assert_eq!(status, 1);
    assert!(err.contains("copy limit"), "{err}");
    assert!(!marker.exists());

    // Unreadable required data.
    if !rustix::process::geteuid().is_root() {
        std::fs::set_permissions(fx.path("proj/keep"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let (status, _, err) = run(&fx, &[], &[&launched]);
        assert_eq!(status, 1);
        assert!(err.contains("keep"), "{err}");
        assert!(!marker.exists());
        std::fs::set_permissions(fx.path("proj/keep"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }

    // A scope overlapping the undo store.
    let (status, _, err) = fx.undo_with(
        SESSION,
        &[],
        &[
            "run",
            "--scope",
            fx.root.to_str().unwrap(),
            "--",
            "/usr/bin/true",
        ],
    );
    assert_eq!(status, 1);
    assert!(err.contains("overlaps the undo store"), "{err}");

    // The filesystem root.
    let (status, _, err) = fx.undo_with(
        SESSION,
        &[],
        &["run", "--scope", "/", "--", "/usr/bin/true"],
    );
    assert_eq!(status, 1);
    assert!(err.contains("root"), "{err}");

    // No transaction was left behind by any refused checkpoint.
    let ids = ish_undo::store::Home::open_existing(&fx.store)
        .unwrap()
        .map(|h| h.txn_ids().unwrap())
        .unwrap_or_default();
    assert!(ids.is_empty(), "{ids:?}");
}

/// Mount crossings need a real mount inside the scope. The Linux container
/// run provides one through `ISH_UNDO_TEST_MOUNT_PARENT` (a directory with a
/// tmpfs mounted below it); elsewhere this is skipped explicitly.
#[test]
fn mount_crossing_is_refused_before_launch() {
    let Some(parent) = std::env::var_os("ISH_UNDO_TEST_MOUNT_PARENT") else {
        eprintln!("skipping: no test mount provided (set ISH_UNDO_TEST_MOUNT_PARENT)");
        return;
    };
    let fx = Fixture::new("mount");
    let (status, _, err) = fx.undo_with(
        SESSION,
        &[],
        &[
            "run",
            "--scope",
            parent.to_str().unwrap(),
            "--",
            "/usr/bin/true",
        ],
    );
    assert_eq!(status, 1);
    assert!(err.contains("different filesystem is mounted"), "{err}");
}

#[test]
fn wide_trees_checkpoint_within_a_small_descriptor_limit() {
    let fx = Fixture::new("wide");
    for i in 0..300 {
        fx.write(&format!("proj/d{i:03}/f"), b"x");
    }
    let (status, _, err) = run(&fx, &["--nofile", "64"], &["delete:d150/f"]);
    assert_eq!(status, 0, "{err}");
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.read("proj/d150/f"), b"x");
}
