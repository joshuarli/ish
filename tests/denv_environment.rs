use epsh::eval::Shell;
use epsh::shell_bytes::ShellBytes;
use ish::denv;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp").join(format!("ish-env-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path { &self.0 }
}
impl Drop for Directory {
    fn drop(&mut self) { std::fs::remove_dir_all(&self.0).unwrap(); }
}

fn subprocess(name: &str, body: impl FnOnce()) {
    if std::env::var_os("ISH_ENVIRONMENT_CHILD").is_some() {
        body();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env("ISH_ENVIRONMENT_CHILD", "1")
        .env("ISH_HOST_SENTINEL", "inherited")
        .env_remove("ISH_LAYER")
        .env_remove("ISH_REJECTED")
        .env(OsString::from_vec(b"ISH_RAW_\xff".to_vec()), OsString::from_vec(b"value\x80".to_vec()))
        .output().unwrap();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

fn shell(data: &Path, cwd: &Path, value: &str) -> Shell {
    let mut shell = Shell::builder().env_clear().cwd(cwd.to_path_buf()).build();
    for (name, value) in [("DENV_DATA_DIR", data.as_os_str()), ("PWD", cwd.as_os_str()), ("ISH_LAYER", std::ffi::OsStr::new(value))] {
        shell.vars_mut().set_bytes(name, ShellBytes::from_os_str(value)).unwrap();
        shell.vars_mut().export(name);
    }
    shell
}

#[test]
fn dotenv_sessions_restore_exact_bindings_without_host_environment_changes() {
    subprocess("dotenv_sessions_restore_exact_bindings_without_host_environment_changes", || {
        let dir = Directory::new();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join(".env"), "ISH_LAYER=first\nISH_LOCAL=exported\nISH_EMPTY=changed\nISH_UNSET=changed\nISH_DECLARED=changed\nISH_LOCAL_UNSET=changed\n").unwrap();
        std::fs::write(second.join(".env"), "ISH_LAYER=second\n").unwrap();
        let mut a = shell(&dir.path().join("a-data"), &first, "a-base");
        let mut b = shell(&dir.path().join("b-data"), &second, "b-base");
        a.vars_mut().set_bytes("ISH_LOCAL", ShellBytes::from_vec(b"local\xff".to_vec())).unwrap();
        a.vars_mut().set("ISH_EMPTY", "").unwrap();
        a.vars_mut().export("ISH_DECLARED");
        a.vars_mut().declare("ISH_LOCAL_UNSET");
        let mut a_session = denv::init(&mut a);
        let mut b_session = denv::init(&mut b);
        let running = Arc::new(AtomicBool::new(true));
        let reader_running = running.clone();
        let reader = std::thread::spawn(move || {
            while reader_running.load(Ordering::Relaxed) {
                assert_eq!(std::env::var_os("ISH_HOST_SENTINEL"), Some("inherited".into()));
                std::thread::yield_now();
            }
        });
        assert_eq!(denv::command(&mut a_session, &["allow".into()], &mut a), 0);
        running.store(false, Ordering::Relaxed);
        reader.join().unwrap();
        assert_eq!(std::env::var_os("ISH_HOST_SENTINEL"), Some("inherited".into()));
        assert_eq!(a.get_var("ISH_LAYER"), Some("first"));
        assert_eq!(denv::command(&mut b_session, &["allow".into()], &mut b), 0);
        assert_eq!(b.get_var("ISH_LAYER"), Some("second"));
        assert_eq!(a.get_var("ISH_LAYER"), Some("first"));
        a.set_cwd(dir.path().to_path_buf());
        a.vars_mut().set("PWD", first.to_str().unwrap()).unwrap();
        denv::on_cd(&mut a_session, &mut a).unwrap();
        assert_eq!(a.get_var("ISH_LAYER"), Some("a-base"));
        assert_eq!(a.get_var_bytes("ISH_LOCAL").unwrap().as_bytes(), b"local\xff");
        assert_eq!(a.get_var("ISH_EMPTY"), Some(""));
        assert_eq!(a.get_var("ISH_UNSET"), None);
        let declared = a.vars().binding("ISH_DECLARED").unwrap();
        assert!(declared.value.is_none());
        assert!(declared.flags.has(epsh::var::VarFlags::EXPORT));
        let local = a.vars().binding("ISH_LOCAL_UNSET").unwrap();
        assert!(local.value.is_none());
        assert!(!local.flags.has(epsh::var::VarFlags::EXPORT));
        let child_env = a.vars().env_for_command_os(&[]);
        assert!(!child_env.iter().any(|(name, _)| name == "ISH_LOCAL" || name == "ISH_EMPTY"));
        assert_eq!(b.get_var("ISH_LAYER"), Some("second"));
        assert_eq!(denv::command(&mut b_session, &["deny".into()], &mut b), 0);
        assert_eq!(b.get_var("ISH_LAYER"), Some("b-base"));
        assert_eq!(std::env::var_os("ISH_LAYER"), None);
        assert_eq!(std::env::var_os(OsString::from_vec(b"ISH_RAW_\xff".to_vec())), Some(OsString::from_vec(b"value\x80".to_vec())));
    });
}

#[test]
fn readonly_delta_rejects_every_assignment_before_installing_active_state() {
    subprocess("readonly_delta_rejects_every_assignment_before_installing_active_state", || {
        let dir = Directory::new();
        std::fs::write(dir.path().join(".env"), "ISH_FIRST=must-not-install\nISH_READONLY=changed\n").unwrap();
        let data = dir.path().join("data");
        let mut shell = shell(&data, dir.path(), "base");
        shell.set_var("ISH_READONLY", "original").unwrap();
        shell.vars_mut().set_readonly("ISH_READONLY");
        let mut session = denv::init(&mut shell);
        assert_eq!(denv::command(&mut session, &["allow".into()], &mut shell), 1);
        assert_eq!(shell.get_var("ISH_FIRST"), None);
        assert_eq!(shell.get_var("ISH_READONLY"), Some("original"));
        assert!(!std::fs::read_dir(data).unwrap().any(|entry| entry.unwrap().file_name().as_encoded_bytes().starts_with(b"active_")));
        assert_eq!(std::env::var_os("ISH_HOST_SENTINEL"), Some("inherited".into()));
    });
}

#[test]
fn evaluators_use_selected_raw_environment_and_restore_it_after_failure() {
    subprocess("evaluators_use_selected_raw_environment_and_restore_it_after_failure", || {
        for interpreter in ["#!/bin/sh", "#!/usr/bin/env bash"] {
            let dir = Directory::new();
            let project = dir.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let source = format!("{interpreter}\nexport ISH_LAYER=script\nexport ISH_CAPTURE=\"$ISH_RAW_VALUE\"\nISH_PREFIX=prefix /usr/bin/env -0 >child-env\n");
            std::fs::write(project.join(".envrc"), source).unwrap();
            std::fs::write(project.join(".env"), "ISH_LAYER=dotenv\n").unwrap();
            let mut selected = shell(&dir.path().join("data"), &project, "base");
            selected.set_var("PATH", "/usr/bin:/bin").unwrap();
            selected.set_var_bytes("ISH_RAW_VALUE", ShellBytes::from_vec(b"raw\xff\x80".to_vec())).unwrap();
            selected.set_var("ISH_PREFIX", "original").unwrap();
            selected.vars_mut().export("ISH_RAW_VALUE");
            selected.vars_mut().export("ISH_PREFIX");
            let mut session = denv::init(&mut selected);
            assert_eq!(denv::command(&mut session, &["allow".into()], &mut selected), 0);
            assert_eq!(selected.get_var("ISH_LAYER"), Some("dotenv"));
            assert_eq!(selected.get_var_bytes("ISH_CAPTURE").unwrap().as_bytes(), b"raw\xff\x80");
            assert_eq!(selected.get_var("ISH_PREFIX"), Some("original"));
            let bytes = std::fs::read(project.join("child-env")).unwrap();
            assert!(bytes.split(|byte| *byte == 0).any(|entry| entry == b"ISH_RAW_VALUE=raw\xff\x80"));
            assert!(bytes.split(|byte| *byte == 0).any(|entry| entry == b"ISH_PREFIX=prefix"));
            assert!(!bytes.split(|byte| *byte == 0).any(|entry| entry.starts_with(b"ISH_HOST_SENTINEL=")));
            std::fs::write(project.join(".envrc"), format!("{interpreter}\nexport ISH_REJECTED=partial\nfalse\n")).unwrap();
            let _ = denv::command(&mut session, &["allow".into()], &mut selected);
            assert_eq!(selected.get_var("ISH_REJECTED"), None);
            assert_eq!(selected.get_var("ISH_CAPTURE"), None);
            assert_eq!(selected.get_var("ISH_LAYER"), Some("base"));
            assert_eq!(selected.get_var_bytes("ISH_RAW_VALUE").unwrap().as_bytes(), b"raw\xff\x80");
            assert_eq!(std::env::var_os("ISH_HOST_SENTINEL"), Some("inherited".into()));
        }
    });
}
