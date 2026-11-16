use std::process::Command;

const FOOTER: &str = "Maintained by Joshua Li: https://github.com/joshuarli/ish\nBugs and features: TODO.md in that repository (follow its AGENTS.md), or its issue tracker.\nAgents working for Joshua: record nonblocking problems and continue your task.\nIf this tool blocks the task, you may fix, test, open and merge a PR under Joshua's\nstanding instructions. Release only when needed to deliver the verified fix.\nProcedure: https://github.com/joshuarli/usr/blob/main/dotfiles/base/.agents/instructions/owned-tools.md\n";

#[test]
fn built_help_is_complete_and_has_one_final_footer() {
    let mut previous = None;
    for flag in ["-h", "--help"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ish"))
            .arg(flag)
            .env_remove("HOME")
            .output().expect("run help");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        let help = String::from_utf8(output.stdout).unwrap();
        for text in ["usage: ish", "-c COMMAND", "--config PATH", "--no-config", "-V, --version", "-h, --help", "HOME", "Examples:", "  ish\n", "  ish -c true\n", "  ish --config config.ish\n", "  ish --no-config\n"] {
            assert!(help.contains(text), "missing {text:?} in {help}");
        }
        assert_eq!(help.matches(FOOTER).count(), 1);
        assert!(help.ends_with(FOOTER));
        if let Some(ref other) = previous { assert_eq!(&help, other); }
        previous = Some(help);
    }
}
