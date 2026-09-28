//! Shared harness for ish-undo integration suites.
//!
//! Each test gets a fresh private fixture root with separate `work` and
//! `home` children. The store lives under the fixture's home, all paths are
//! explicit, and child processes get an explicit environment and cwd.
//! Cleanup removes only the fixture root this harness created, and
//! `remove_dir_all` does not follow symlinks.

#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ish_undo::testing::journal::{Action, Strength};
use ish_undo::testing::replay::Model;
use ish_undo::testing::store::Home;

pub const SESSION: u64 = 0x5e55_1011;
pub const OTHER_SESSION: u64 = 0x5e55_2022;

pub struct Fixture {
    pub root: PathBuf,
    pub work: PathBuf,
    pub home: PathBuf,
    pub store: PathBuf,
}

impl Fixture {
    pub fn new(label: &str) -> Fixture {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = base.join(format!(
            "ish-undo-test-{label}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let work = root.join("work");
        let home = root.join("home");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&home).unwrap();
        let store = ish_undo::testing::store::root_for_home(home.as_os_str());
        Fixture {
            root,
            work,
            home,
            store,
        }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.work.join(rel)
    }

    pub fn write(&self, rel: &str, data: &[u8]) {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, data).unwrap();
    }

    pub fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.path(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    }

    pub fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.path(rel)).is_ok()
    }

    pub fn fixture(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ish-undo-fixture"));
        cmd.current_dir(&self.work)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null());
        cmd
    }

    /// Run fixture steps as one transaction; returns (status, txn id, stderr).
    pub fn shell_with(&self, session: u64, pre: &[&str], steps: &[&str]) -> (i32, u64, String) {
        let mut cmd = self.fixture();
        cmd.args(pre)
            .arg("shell")
            .arg(&self.store)
            .arg(&self.work)
            .arg(session.to_string())
            .args(steps);
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let id = stdout
            .lines()
            .find_map(|l| l.strip_prefix("txn "))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        (
            out.status.code().unwrap_or(-1),
            id,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    pub fn shell(&self, steps: &[&str]) -> (i32, u64, String) {
        self.shell_with(SESSION, &[], steps)
    }

    /// Run the `undo` builtin; returns (status, stdout, stderr).
    pub fn undo_with(&self, session: u64, pre: &[&str], args: &[&str]) -> (i32, String, String) {
        let out = self.undo_output(session, pre, args);
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    pub fn undo_output(&self, session: u64, pre: &[&str], args: &[&str]) -> Output {
        let mut cmd = self.fixture();
        cmd.args(pre)
            .arg("undo")
            .arg(&self.store)
            .arg(&self.work)
            .arg(session.to_string())
            .args(args);
        cmd.output().unwrap()
    }

    pub fn undo(&self, args: &[&str]) -> (i32, String, String) {
        self.undo_with(SESSION, &[], args)
    }

    pub fn home_store(&self) -> Home {
        Home::open_existing(&self.store)
            .unwrap()
            .expect("store exists")
    }

    pub fn model(&self, id: u64) -> Model {
        Model::load(&self.home_store(), id).unwrap()
    }

    /// Strengths of the saved versions a transaction's actions reference.
    pub fn strengths(&self, id: u64) -> Vec<Strength> {
        self.model(id)
            .actions
            .iter()
            .filter_map(|a| a.action.saved().and_then(|s| s.strength()))
            .collect()
    }

    pub fn actions(&self, id: u64) -> Vec<Action> {
        self.model(id)
            .actions
            .into_iter()
            .map(|a| a.action)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let name = self.root.file_name().unwrap().to_string_lossy();
        if self.root.parent() == Some(base.as_path()) && name.starts_with("ish-undo-test-") {
            // Restore permissions a test may have removed so cleanup works.
            let _ = Command::new("/bin/chmod")
                .arg("-R")
                .arg("u+rwx")
                .arg(&self.root)
                .status();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// Whether the filesystem holding `dir` supports clones, probed directly.
pub fn clone_supported(dir: &Path) -> bool {
    use std::os::fd::AsFd;
    let a = dir.join(".clone-probe-a");
    std::fs::write(&a, b"probe").unwrap();
    let src = std::fs::File::open(&a).unwrap();
    let d = ish_undo::testing::sys::open_dir(dir).unwrap();
    let ok = ish_undo::testing::sys::clone_file(src.as_fd(), d.as_fd(), c".clone-probe-b").is_ok();
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(dir.join(".clone-probe-b"));
    ok
}

/// Strength a rm-style preservation is expected to use on `dir`.
pub fn unlink_strength(dir: &Path) -> Strength {
    if clone_supported(dir) {
        Strength::Clone
    } else {
        Strength::Link
    }
}

pub fn raw_name(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

pub fn escape(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        if b.is_ascii_graphic() && b != b'\\' {
            s.push(b as char);
        } else {
            s.push_str(&format!("\\x{b:02x}"));
        }
    }
    s
}

pub fn ino(path: &Path) -> (u64, u64) {
    let m = std::fs::symlink_metadata(path).unwrap();
    (m.dev(), m.ino())
}

pub fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

pub fn os(p: &Path) -> &OsStr {
    OsStr::from_bytes(p.as_os_str().as_bytes())
}

/// Join step arguments with the fixture's unit separator.
pub fn argv(parts: &[&str]) -> String {
    parts.join("\u{1f}")
}
