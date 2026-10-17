use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use epsh::error::ExitStatus;
use epsh::eval::Shell;
use epsh::parser::Parser;

const CHILD_MODE: &str = "EPSH_READ_HEREDOC_SINK_CHILD";

#[test]
fn read_builtin_consumes_heredoc_with_output_sink() {
    if std::env::var_os(CHILD_MODE).is_some() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        let program = Parser::new(
            r#"{ /bin/cat; read after; echo "after=$after"; } <<EOF
external one
external two
EOF
{ read first; read second; echo "$first/$second"; } <<EOF
first
second
EOF
{ read outer_first; read inner </dev/null; read outer_second; echo "$outer_first/$inner/$outer_second"; } <<OUTER
outer first
outer second
OUTER"#,
        )
        .parse()
        .unwrap();

        assert_eq!(shell.run_program(&program), ExitStatus::SUCCESS);

        let failing_program = Parser::new(
            "{ read before; read broken </dev/null/child; } <<EOF\nouter line\nEOF",
        )
        .parse()
        .unwrap();
        assert_eq!(shell.run_program(&failing_program), ExitStatus::MISUSE);

        let outside_program = Parser::new("read outside\necho \"outside=$outside\"")
            .parse()
            .unwrap();
        assert_eq!(shell.run_program(&outside_program), ExitStatus::SUCCESS);
        assert_eq!(
            *stdout.lock().unwrap(),
            b"external one\nexternal two\nafter=\nfirst/second\nouter first//outer second\noutside=fallback\n"
        );
        return;
    }

    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("read_builtin_consumes_heredoc_with_output_sink")
        .env(CHILD_MODE, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"fallback\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{output:?}");
}
