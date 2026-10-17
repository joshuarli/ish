use epsh::error::ExitStatus;
use epsh::eval::Shell;
use std::sync::{Arc, Mutex};

#[test]
fn status_contract_normalizes_integer_construction() {
    for code in [i32::MIN, -257, -256, -1, 0, 1, 255, 256, 257, i32::MAX] {
        let status = ExitStatus::from(code);
        let expected = code.rem_euclid(256);
        assert_eq!(status.code(), expected, "{code}");
        assert_eq!(i32::from(status), expected);
        assert_eq!(status.to_string(), expected.to_string());
        assert_eq!(status.success(), expected == 0);
    }
}

#[test]
fn status_contract_normalizes_signal_construction() {
    for signal in [i32::MIN, -129, -1, 0, 2, 127, 128, i32::MAX] {
        let expected = (i64::from(signal) + 128).rem_euclid(256) as i32;
        assert_eq!(ExitStatus::from_signal(signal).code(), expected, "{signal}");
    }
    assert_eq!(ExitStatus::from_wait(42 << 8).code(), 42);
    assert_eq!(ExitStatus::from_wait(2).code(), 130);
}

#[test]
fn status_contract_normalizes_exit_and_return_operands() {
    for operand in [i32::MIN, -257, -1, 0, 255, 256, 257, i32::MAX] {
        let expected = operand.rem_euclid(256);
        for script in [
            format!("exit {operand}"),
            format!("f() {{ return {operand}; }}; f"),
        ] {
            let mut shell = Shell::new();
            assert_eq!(shell.run_script(&script), expected, "{script}");
        }
    }
}

#[test]
fn status_contract_rejects_invalid_exit_and_return_operands() {
    for operand in ["bad", "''", "1.5", "2147483648", "-2147483649"] {
        for builtin in ["exit", "return"] {
            let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
            let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
            let mut shell = Shell::builder()
                .stdout_sink(stdout.clone())
                .stderr_sink(stderr.clone())
                .build();
            shell.set_exit_status(ExitStatus::from(37));
            let script = if builtin == "return" {
                format!("f() {{ return {operand}; echo reached; }}; f; echo reached")
            } else {
                format!("exit {operand}; echo reached")
            };
            assert_eq!(shell.run_script(&script), 2, "{script}");
            assert!(stdout.lock().unwrap().is_empty(), "{script}");
            let diagnostic = String::from_utf8(stderr.lock().unwrap().clone()).unwrap();
            assert!(diagnostic.contains(builtin), "{script}: {diagnostic}");
            assert!(diagnostic.contains("Illegal number"), "{script}: {diagnostic}");
        }
    }
}

#[test]
fn status_contract_omitted_operands_preserve_previous_status() {
    for (script, expected) in [("exit", 37), ("f() { false; return; }; f", 1)] {
        let mut shell = Shell::new();
        shell.set_exit_status(ExitStatus::from(37));
        assert_eq!(shell.run_script(script), expected, "{script}");
    }
}
