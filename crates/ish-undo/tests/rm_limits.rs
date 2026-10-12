//! Refusals that protect the recovery store and keep removals restorable.

mod common;

use common::*;

#[test]
fn rm_refuses_ancestor_of_store_that_does_not_exist_yet() {
    let fx = Fixture::new("guard-fresh");
    assert!(!fx.store.exists(), "the store must not exist before the first operation");
    fx.write("keep", b"x");
    let (status, _, err) = fx.shell(&[&format!("rm:{}", argv(&["-r", "../home"]))]);
    assert_ne!(status, 0, "{err}");
    assert!(fx.home.exists(), "{err}");
}

#[test]
fn mv_refuses_ancestor_of_store_that_does_not_exist_yet() {
    let fx = Fixture::new("guard-fresh-mv");
    let (status, _, err) = fx.shell(&[&format!("mv:{}", argv(&["../home", "moved"]))]);
    assert_ne!(status, 0, "{err}");
    assert!(fx.home.exists(), "{err}");
}

/// Builds a tree whose deepest paths exceed PATH_MAX by descending one
/// directory at a time with relative operations.
#[test]
fn rm_refuses_paths_replay_could_not_resolve() {
    let fx = Fixture::new("rm-deep");
    let saved = std::env::current_dir().unwrap();
    std::env::set_current_dir(&fx.work).unwrap();
    let name = "d".repeat(100);
    std::fs::create_dir(&name).unwrap();
    for _ in 0..60 {
        std::env::set_current_dir(&name).unwrap_or_else(|_| unreachable!());
        std::fs::create_dir(&name).unwrap();
    }
    std::env::set_current_dir(&saved).unwrap();
    let (status, _, err) = fx.shell(&[&format!("rm:{}", argv(&["-r", &name]))]);
    assert_ne!(status, 0, "{err}");
    assert!(err.contains("path too long"), "{err}");
    assert!(fx.exists(&name), "{err}");
}

/// A directory that is writable but not readable hides the target from
/// lookup. The redirection must be refused, not truncated unprotected.
#[test]
fn redirection_lookup_failure_is_refused_not_unprotected() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new("redir-refused");
    fx.write("d/f", b"precious");
    let dir = fx.path("d");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o333)).unwrap();
    let (status, _, err) = fx.shell(&["write:d/f=new"]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, 1, "{err}");
    assert_eq!(fx.read("d/f"), b"precious");
}
