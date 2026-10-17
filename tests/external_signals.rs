use ptytest::{CommandSpec, ProtocolProfile, PtyTest, Scenario, TestEnv};
use std::path::PathBuf;
use std::time::Duration;

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut path = std::env::temp_dir().join("ish-signals-XXXXXX")
            .as_os_str().as_bytes().to_vec();
        path.push(0);
        assert!(!unsafe { libc::mkdtemp(path.as_mut_ptr().cast()) }.is_null());
        path.pop();
        Self(PathBuf::from(std::ffi::OsString::from_vec(path)))
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn wait_for(terminal: &mut PtyTest, offset: usize, needle: &str) -> usize {
    let deadline = terminal.deadline(Duration::from_secs(5));
    loop {
        let output = &terminal.raw_output()[offset..];
        if let Some(position) = output.windows(needle.len()).position(|bytes| bytes == needle.as_bytes()) {
            return offset + position + needle.len();
        }
        if !terminal.wait_for_output(deadline).unwrap() {
            let output = &terminal.raw_output()[offset..];
            panic!("missing {needle:?}: {}",
                String::from_utf8_lossy(&output[output.len().saturating_sub(3000)..]));
        }
    }
}

#[test]
#[ignore = "invoked as an external command by the PTY regression"]
fn external_signal_probe() {
    let label = std::env::var("ISH_SIGNAL_PROBE").unwrap();
    if label == "right" {
        std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
    }
    for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTOU, libc::SIGTTIN] {
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        assert_eq!(unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { action.assume_init() }.sa_sigaction, libc::SIG_DFL,
            "external command {label} inherited nondefault signal {signal}");
    }
    let marker = format!("signal-defaults:{label}\n");
    assert_eq!(rustix::io::write(std::io::stderr(), marker.as_bytes()).unwrap(), marker.len());
}

#[test]
fn external_commands_restore_terminal_signals_in_every_context() {
    let home = Home::new();
    let environment = TestEnv::hermetic_ascii().unwrap()
        .env("HOME", &home.0).env("USER", "testuser")
        .env("PWD", &home.0).env("PATH", "/usr/bin:/bin");
    let scenario = Scenario::new("external command signal dispositions").unwrap()
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_ish")).current_dir(&home.0))
        .environment(environment)
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).unwrap();
    wait_for(&mut terminal, 0, "$ ");
    let helper = std::env::current_exe().unwrap();
    let probe = |label: &str| format!(
        "ISH_SIGNAL_PROBE={label} '{}' --exact external_signal_probe --ignored --nocapture", helper.display());
    for (command, labels) in [
        (probe("direct"), vec!["direct"]),
        (format!("{} | {}", probe("left"), probe("right")), vec!["left", "right"]),
        (format!("true && {}", probe("list")), vec!["list"]),
        (format!("echo $({})", probe("substitution")), vec!["substitution"]),
    ] {
        let offset = terminal.raw_output().len();
        let deadline = terminal.deadline(Duration::from_secs(5));
        terminal.send_text(deadline, &format!("\x1b[200~{command}\x1b[201~\r")).unwrap();
        let mut result_offset = offset;
        for label in labels {
            result_offset = result_offset.max(wait_for(&mut terminal, offset, &format!("signal-defaults:{label}")));
        }
        wait_for(&mut terminal, result_offset, "$ ");
    }
}
