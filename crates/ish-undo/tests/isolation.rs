//! Version isolation: after preservation, write through another hard link,
//! an open descriptor, and a shared writable mapping. Frozen versions (clone
//! or copy) must not change; linked versions are honestly weaker; captures
//! that cannot be frozen must refuse before mutating.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::process::{Child, Stdio};

use common::*;
use ish_undo::testing::journal::Strength;
use ish_undo::testing::store::Stores;

struct Holder {
    child: Child,
    lines: BufReader<std::process::ChildStdout>,
}

impl Holder {
    /// Open the file in another process and wait until it reports ready.
    fn open(fx: &Fixture, rel: &str) -> Holder {
        let mut child = fx
            .fixture()
            .arg("hold-open")
            .arg(fx.path(rel))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");
        Holder { child, lines }
    }

    fn command(&mut self, cmd: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{cmd}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.lines.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "done");
    }

    fn finish(mut self) {
        drop(self.child.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}

fn saved_bytes(fx: &Fixture, id: u64) -> Vec<u8> {
    let model = fx.model(id);
    let saved = model
        .saved_versions()
        .next()
        .expect("a saved version")
        .clone();
    let mut stores = Stores::new(fx.home_store(), id).unwrap();
    let fd = stores.open_object(saved.obj().unwrap()).unwrap();
    let len = ish_undo::testing::sys::fstat(fd.as_fd()).unwrap().size as usize;
    let mut buf = vec![0u8; len];
    let n = ish_undo::testing::sys::read_full_at(fd.as_fd(), &mut buf, 0).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn frozen_pre_image_ignores_later_writes_through_every_path() {
    let fx = Fixture::new("frozen");
    fx.write("f", b"ORIGINAL");
    std::fs::hard_link(fx.path("f"), fx.path("alias")).unwrap();
    let mut holder = Holder::open(&fx, "f");

    // An append needs a frozen pre-image: clone, or a bounded copy.
    let (status, id, err) = fx.shell(&["append:f=+more"]);
    assert_eq!(status, 0, "{err}");
    let strength = fx.strengths(id)[0];
    assert!(strength.frozen(), "append preserved with {strength:?}");
    if clone_supported(&fx.work) {
        assert_eq!(strength, Strength::Clone);
    }

    std::fs::OpenOptions::new()
        .write(true)
        .open(fx.path("alias"))
        .unwrap()
        .write_all(b"AL")
        .unwrap();
    holder.command("write");
    holder.command("map");
    holder.finish();
    assert_eq!(&fx.read("f")[..3], b"FDM");
    assert_eq!(
        saved_bytes(&fx, id),
        b"ORIGINAL",
        "the saved version stayed frozen"
    );

    // The live file no longer matches what the transaction left, so undo
    // reports a conflict and leaves the newer data alone.
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("conflict"), "{err}");
    assert_eq!(&fx.read("f")[..3], b"FDM");
    assert_eq!(
        saved_bytes(&fx, id),
        b"ORIGINAL",
        "the conflict did not touch the saved version"
    );
}

#[test]
fn removal_capture_isolation_matches_its_backend() {
    let fx = Fixture::new("rm-isolation");
    fx.write("f", b"ORIGINAL");
    std::fs::hard_link(fx.path("f"), fx.path("alias")).unwrap();
    let mut holder = Holder::open(&fx, "f");
    let (status, id, err) = fx.shell(&[&format!("rm:{}", argv(&["f"]))]);
    assert_eq!(status, 0, "{err}");
    let strength = fx.strengths(id)[0];
    std::fs::OpenOptions::new()
        .write(true)
        .open(fx.path("alias"))
        .unwrap()
        .write_all(b"AL")
        .unwrap();
    holder.command("write");
    holder.command("map");
    holder.finish();
    match strength {
        Strength::Clone | Strength::Copy => assert_eq!(saved_bytes(&fx, id), b"ORIGINAL"),
        Strength::Link => {
            // The retained inode is shared with the alias and the open
            // descriptor, so their writes are visible: this is the weaker
            // contract `undo show` labels.
            assert_eq!(&saved_bytes(&fx, id)[..3], b"FDM");
            let (_, out, _) = fx.undo(&["show", &id.to_string()]);
            assert!(out.contains("linked"), "{out}");
        }
    }
}

#[test]
fn linked_fallback_is_labeled_weaker() {
    let fx = Fixture::new("linked");
    fx.write("f", b"ORIGINAL");
    std::fs::hard_link(fx.path("f"), fx.path("alias")).unwrap();
    // Make cloning unavailable so removal falls back to a retained inode.
    let (status, id, err) = fx.shell_with(
        SESSION,
        &["--fault", "clone=ENOTSUP"],
        &[&format!("rm:{}", argv(&["f"]))],
    );
    assert_eq!(status, 0, "{err}");
    assert_eq!(fx.strengths(id), vec![Strength::Link]);
    std::fs::write(fx.path("alias"), b"CHANGED").unwrap();
    assert_eq!(
        saved_bytes(&fx, id),
        b"CHANGED",
        "a linked version is not frozen"
    );

    let (_, out, _) = fx.undo(&["show", &id.to_string()]);
    assert!(out.contains("1 linked"), "{out}");
    assert!(out.contains("retained inode"), "{out}");
    let (_, list, _) = fx.undo(&["list"]);
    assert!(list.contains("linked"), "{list}");

    // Restoring relinks the retained inode, which the alias still shares.
    let (status, _, err) = fx.undo(&[]);
    assert_eq!(status, 0, "{err}");
    assert_eq!(ino(&fx.path("f")), ino(&fx.path("alias")));
}

#[test]
fn unfreezable_write_capture_refuses_without_mutating() {
    let fx = Fixture::new("refuse");
    fx.write("big", &vec![b'z'; 4096]);
    let (status, _, err) = fx.shell_with(
        SESSION,
        &[
            "--config",
            "ISH_UNDO_COPY_LIMIT=1K",
            "--fault",
            "clone=ENOTSUP",
        ],
        &["write:big=truncated"],
    );
    assert_eq!(status, 1);
    assert!(err.contains("cannot preserve existing contents"), "{err}");
    assert!(err.contains("copy limit"), "{err}");
    assert_eq!(fx.read("big"), vec![b'z'; 4096], "nothing was truncated");
}

#[test]
fn rm_force_does_not_waive_preservation_failures() {
    let fx = Fixture::new("rm-force");
    fx.write("keep", b"data");
    let (status, _, err) = fx.shell_with(
        SESSION,
        &[
            "--config",
            "ISH_UNDO_COPY_LIMIT=0",
            "--fault",
            "clone=ENOTSUP",
            "--fault",
            "link=EIO",
        ],
        &[&format!("rm:{}", argv(&["-f", "keep"]))],
    );
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("not removed"), "{err}");
    assert_eq!(fx.read("keep"), b"data");
}

#[test]
fn permission_failure_is_not_reported_as_unsupported() {
    let fx = Fixture::new("perm");
    fx.write("secret", b"data");
    std::fs::set_permissions(
        fx.path("secret"),
        std::os::unix::fs::PermissionsExt::from_mode(0o200),
    )
    .unwrap();
    // A write-only file cannot be read for a frozen pre-image.
    let (status, _, err) = fx.shell(&["write:secret=x"]);
    if rustix::process::geteuid().is_root() {
        return;
    }
    assert_eq!(status, 1, "{err}");
    assert!(
        err.contains("Permission denied") || err.contains("permission"),
        "{err}"
    );
    assert!(!err.contains("unsupported"), "{err}");
    std::fs::set_permissions(
        fx.path("secret"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(fx.read("secret"), b"data");
}
