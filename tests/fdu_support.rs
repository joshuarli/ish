#![allow(dead_code)]

use ptytest::{CommandSpec, ExitStatus, Key, PtyTest, Scenario, ScreenSnapshot, Size, TerminalBaseline, TestEnv};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static TEMP_ID: AtomicUsize = AtomicUsize::new(0);
pub const STEP: Duration = Duration::from_secs(30);

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> io::Result<Self> {
        loop {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
            fs::create_dir_all(&scratch)?;
            let path = scratch.join(format!("fdu-pty-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub fn standard_tree() -> io::Result<(Self, PathBuf)> {
        let temp = Self::new()?;
        let root = temp.0.join("root");
        fs::create_dir_all(root.join("alpha"))?;
        fs::create_dir_all(root.join("beta"))?;
        fs::write(root.join("alpha/inner.txt"), vec![b'a'; 3000])?;
        fs::write(root.join("alpha/other.txt"), b"other")?;
        fs::write(root.join("beta/deep.txt"), vec![b'b'; 9000])?;
        fs::write(root.join("top.txt"), vec![b't'; 5000])?;
        Ok((temp, root))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::process::Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.0).status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct Session {
    pub terminal: PtyTest,
    pub baseline: TerminalBaseline,
    pub step: Duration,
    home: PathBuf,
}

impl Session {
    pub fn start(label: &str, root: &Path, args: &[&str], columns: u16, rows: u16) -> Self {
        Self::start_with_env(label, root, args, columns, rows, &[])
    }

    pub fn start_with_env(label: &str, root: &Path, args: &[&str], columns: u16, rows: u16, env: &[(&str, &str)]) -> Self {
        let ish_bin = PathBuf::from(env!("CARGO_BIN_EXE_ish"));
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
        let home = scratch.join(format!("fdu-home-{}-{id}", std::process::id()));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(home.join(".config/ish")).unwrap();

        let mut command = CommandSpec::new(ish_bin)
            .env("HOME", &home)
            .env("USER", "testuser")
            .env("PWD", env!("CARGO_MANIFEST_DIR"))
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        for (key, value) in env {
            command = command.env(key, value);
        }
        let scenario = Scenario::new(label)
            .unwrap()
            .command(command.current_dir(env!("CARGO_MANIFEST_DIR")))
            .size(Size::new(columns, rows).unwrap())
            .environment(TestEnv::hermetic().unwrap());
        let mut terminal = PtyTest::spawn(scenario).unwrap();

        let deadline = terminal.deadline(Duration::from_secs(15));
        terminal.wait_for_screen(deadline, "initial prompt", |screen| {
            let cursor = screen.cursor();
            screen.row(usize::from(cursor.row)).is_some_and(|r| {
                r.get(..usize::from(cursor.column)).is_some_and(|p| p.ends_with("$ "))
            })
        }).unwrap();

        let mut cmd = "fdu --interactive".to_owned();
        for arg in args {
            cmd.push(' ');
            cmd.push_str(arg);
        }
        cmd.push(' ');
        cmd.push_str(&root.to_string_lossy());
        cmd.push('\r');
        terminal.send_bytes(terminal.deadline(Duration::from_secs(5)), cmd.as_bytes()).unwrap();

        let baseline = terminal.terminal_baseline();
        Self { terminal, baseline, step: STEP, home }
    }

    pub fn wait(&mut self, description: &str, predicate: impl Fn(&ScreenSnapshot) -> bool) -> ScreenSnapshot {
        let deadline = self.terminal.deadline(self.step);
        self.terminal.wait_for_screen(deadline, description, predicate).unwrap();
        let deadline = self.terminal.deadline(self.step);
        assert!(self.terminal.wait_for_quiescence(deadline, Duration::from_millis(25)).unwrap());
        self.terminal.screen()
    }

    pub fn wait_for(&mut self, text: &str) -> ScreenSnapshot {
        self.wait(text, |screen| screen.shows(text))
    }

    pub fn wait_until_absent(&mut self, text: &str) -> ScreenSnapshot {
        self.wait(&format!("{text} to disappear"), |screen| !screen.shows(text))
    }

    pub fn wait_ready(&mut self) -> ScreenSnapshot {
        self.wait("scan to finish", |screen| {
            let status = screen.lines().pop().unwrap_or_default();
            let status = status.trim_start();
            status.split_whitespace().next().is_some_and(|count| count.parse::<usize>().is_ok())
                && status.contains(" entries ")
        })
    }

    pub fn ready_by_name(&mut self) {
        self.wait_ready();
        self.text("s");
        self.wait_for("Sorted by name");
        self.key(Key::Home);
    }

    pub fn text(&mut self, text: &str) {
        let deadline = self.terminal.deadline(self.step);
        self.terminal.send_text(deadline, text).unwrap();
    }

    pub fn key(&mut self, key: Key) {
        let deadline = self.terminal.deadline(self.step);
        self.terminal.send_key(deadline, key).unwrap();
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.terminal.resize(Size::new(columns, rows).unwrap()).unwrap();
    }

    pub fn quit(mut self) {
        self.text("q");
        let deadline = self.terminal.deadline(self.step);
        self.terminal.wait_for_screen(deadline, "prompt returns after fdu exit", |screen| {
            let cursor = screen.cursor();
            screen.row(usize::from(cursor.row)).is_some_and(|r| {
                r.get(..usize::from(cursor.column)).is_some_and(|p| p.ends_with("$ "))
            })
        }).unwrap();
        self.terminal.send_bytes(deadline, b"exit\r").unwrap();
        let status = self.terminal.wait_for_exit(deadline).unwrap();
        assert_eq!(status, ExitStatus::Code(0));
        let _ = fs::remove_dir_all(&self.home);
    }
}

pub trait ScreenText {
    fn lines(&self) -> Vec<String>;
    fn shows(&self, text: &str) -> bool;
    fn shows_all(&self, candidates: &[&str]) -> bool;
}

impl ScreenText for ScreenSnapshot {
    fn lines(&self) -> Vec<String> {
        (0..self.row_count()).map(|row| self.row(row).unwrap_or_default()).collect()
    }

    fn shows(&self, text: &str) -> bool {
        self.contains(text)
    }

    fn shows_all(&self, candidates: &[&str]) -> bool {
        candidates.iter().all(|candidate| self.shows(candidate))
    }
}

pub fn list_is_dimmed(screen: &ScreenSnapshot) -> bool {
    screen.cell(1, 0).is_some_and(|cell| cell.attributes().dim)
}

pub fn row_containing<'a>(screen: &'a ScreenSnapshot, needle: &str) -> String {
    screen
        .lines()
        .into_iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no row contains {needle:?}"))
}

pub const LAYOUT_FIXTURE: &str = ".work/tmp/fdu-layout-fixture";
const LAYOUT_FIXTURE_MARKER: &str = ".work/tmp/fdu-layout-fixture.ready";
const LAYOUT_FIXTURE_DESCRIPTION: &str = "seed=0 entries=192238";

pub fn layout_fixture() -> &'static Path {
    static BUILT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(LAYOUT_FIXTURE);
        let marker_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(LAYOUT_FIXTURE_MARKER);
        let scratch = fixture.parent().expect("fixture has a parent directory");
        fs::create_dir_all(scratch).expect("create the fixture scratch directory");
        let marker = match fs::read_to_string(&marker_path) {
            Ok(marker) => marker,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("read the fixture marker: {error}"),
        };
        if marker.trim() == LAYOUT_FIXTURE_DESCRIPTION && fixture.is_dir() {
            return;
        }
        if marker_path.exists() {
            fs::remove_file(&marker_path).expect("remove the stale fixture marker");
        }
        let partial = fixture.with_file_name("fdu-layout-fixture.partial");
        if partial.exists() {
            fs::remove_dir_all(&partial).expect("remove a partial fixture tree");
        }
        if fixture.exists() {
            fs::remove_dir_all(&fixture).expect("remove the stale fixture tree");
        }
        let output = std::process::Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/generate_layout_fixture.py"))
            .arg(&partial)
            .output()
            .expect("python3 generates the layout fixture");
        assert!(output.status.success(), "fixture generation failed: {}", String::from_utf8_lossy(&output.stderr));
        let report = String::from_utf8_lossy(&output.stdout);
        assert!(report.contains("entries=192238"), "unexpected fixture shape: {report}");
        fs::rename(&partial, &fixture).expect("publish the layout fixture");
        fs::write(&marker_path, LAYOUT_FIXTURE_DESCRIPTION).expect("record the fixture marker");
    });
    Path::new(LAYOUT_FIXTURE)
}
