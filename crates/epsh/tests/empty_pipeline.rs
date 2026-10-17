use epsh::ast::{Command, Program};
use epsh::error::{ExitStatus, ShellError, Span};
use epsh::eval::Shell;
use epsh::parser::Parser;
use std::panic::{catch_unwind, AssertUnwindSafe};

#[test]
fn eval_empty_pipeline_returns_an_error_without_panicking() {
    let pipeline = Command::Pipeline {
        commands: Vec::new(),
        bang: false,
        span: Span::default(),
    };
    let mut shell = Shell::new();

    let evaluation = catch_unwind(AssertUnwindSafe(|| shell.eval_command(&pipeline)));

    assert!(evaluation.is_ok(), "evaluation panicked for an empty pipeline");
    assert!(matches!(
        evaluation.unwrap(),
        Err(ShellError::Runtime { msg, span }) if msg == "empty pipeline" && span == Span::default()
    ));
}

#[test]
fn eval_nonempty_pipeline_still_runs() {
    let program = Parser::new("true | true").parse().unwrap();
    let mut shell = Shell::new();

    assert_eq!(shell.run_program(&program), ExitStatus::SUCCESS);
}

#[test]
fn run_program_rejects_empty_pipeline_with_misuse_status() {
    let program = Program {
        commands: vec![Command::Pipeline {
            commands: Vec::new(),
            bang: false,
            span: Span::default(),
        }],
    };
    let mut shell = Shell::new();

    assert_eq!(shell.run_program(&program), ExitStatus::MISUSE);
}
