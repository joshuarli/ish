use ish::prompt::Prompt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
const CHILD_ENV: &str = "ISH_PROMPT_GIT_REFRESH_CHILD";
const CWD_ENV: &str = "ISH_PROMPT_GIT_REFRESH_CWD";
const BRANCH_ENV: &str = "ISH_PROMPT_GIT_REFRESH_BRANCH";

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
        std::fs::create_dir_all(&parent).unwrap();
        loop {
            let path = parent.join(format!(
                "ish-prompt-git-refresh-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("failed to create test directory: {error}"),
            }
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn refresh_after_git_creation(cwd: &str, branch: &str) {
    assert!(std::path::Path::new(cwd).is_dir());
    let mut prompt = Prompt::new();
    let mut rendered = String::new();
    prompt.render_into(&mut rendered, 0, cwd, false);

    std::fs::create_dir(".git").unwrap();
    std::fs::write(".git/HEAD", format!("ref: refs/heads/{branch}\n")).unwrap();

    prompt.render_into(&mut rendered, 0, cwd, false);
    assert!(rendered.contains(&format!(" {branch}")), "{rendered:?}");
}

fn run_refresh_child(test_name: &str, cwd: &str, branch: &str) {
    let directory = TestDirectory::new();
    if cwd == "child" {
        let child = directory.path().join("child");
        std::fs::create_dir(&child).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(test_name)
        .env(CHILD_ENV, "1")
        .env(CWD_ENV, cwd)
        .env(BRANCH_ENV, branch)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "prompt refresh subprocess failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn prompt_refreshes_negative_git_lookup_after_repository_creation() {
    if std::env::var_os(CHILD_ENV).is_some() {
        refresh_after_git_creation(
            &std::env::var(CWD_ENV).unwrap(),
            &std::env::var(BRANCH_ENV).unwrap(),
        );
    } else {
        run_refresh_child(
            "prompt_refreshes_negative_git_lookup_after_repository_creation",
            ".",
            "late-branch",
        );
    }
}

#[test]
fn prompt_refreshes_negative_git_lookup_after_ancestor_repository_creation() {
    if std::env::var_os(CHILD_ENV).is_some() {
        refresh_after_git_creation(
            &std::env::var(CWD_ENV).unwrap(),
            &std::env::var(BRANCH_ENV).unwrap(),
        );
    } else {
        run_refresh_child(
            "prompt_refreshes_negative_git_lookup_after_ancestor_repository_creation",
            "child",
            "ancestor-branch",
        );
    }
}
