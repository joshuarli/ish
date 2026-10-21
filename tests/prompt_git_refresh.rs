use ish::prompt::Prompt;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
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

fn directory_fd_path(directory: &std::path::Path) -> (File, String) {
    let handle = File::open(directory).unwrap();
    let path = format!("/dev/fd/{}", handle.as_raw_fd());
    (handle, path)
}

#[test]
fn prompt_refreshes_negative_git_lookup_after_repository_creation() {
    let directory = TestDirectory::new();
    let (_directory_handle, cwd) = directory_fd_path(directory.path());
    let mut prompt = Prompt::new();

    let mut rendered = String::new();
    prompt.render_into(&mut rendered, 0, &cwd, false);

    let git_dir = directory.path().join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/late-branch\n").unwrap();

    prompt.render_into(&mut rendered, 0, &cwd, false);
    assert!(rendered.contains(" late-branch"), "{rendered:?}");
}

#[test]
fn prompt_refreshes_negative_git_lookup_after_ancestor_repository_creation() {
    let directory = TestDirectory::new();
    let child = directory.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let directory_handle = File::open(directory.path()).unwrap();
    let cwd = format!("/dev/fd/{}/child", directory_handle.as_raw_fd());
    let mut prompt = Prompt::new();

    let mut rendered = String::new();
    prompt.render_into(&mut rendered, 0, &cwd, false);

    let git_dir = directory.path().join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/ancestor-branch\n").unwrap();

    prompt.render_into(&mut rendered, 0, &cwd, false);
    assert!(rendered.contains(" ancestor-branch"), "{rendered:?}");
}
