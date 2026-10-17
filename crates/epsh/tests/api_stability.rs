//! API stability tests.
//!
//! These tests verify that the public API surface hasn't changed accidentally.
//! If a test fails after a deliberate API change, update the test to match.
//! This prevents accidental breaking changes in the library interface.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Verify all public modules are accessible
use epsh::ast;
use epsh::builtins;
use epsh::encoding;
use epsh::error;
use epsh::eval;
use epsh::expand;
use epsh::glob;
use epsh::lexer;
use epsh::parser;
use epsh::shell_bytes;
use epsh::var;

#[test]
fn shell_construction() {
    // Shell::new() exists and returns a Shell
    let _shell = eval::Shell::new();

    // Shell::default() works
    let _shell: eval::Shell = Default::default();

    // Shell::builder() returns a ShellBuilder
    let _shell = eval::Shell::builder().build();
}

#[test]
fn shell_builder_methods() {
    // Every builder method returns Self for chaining
    let cancel = Arc::new(AtomicBool::new(false));
    let sink: Arc<Mutex<dyn Write + Send>> = Arc::new(Mutex::new(Vec::<u8>::new()));
    let handler: eval::ExternalHandler = Box::new(|_args, _env| Ok(error::ExitStatus::SUCCESS));
    let _shell = eval::Shell::builder()
        .cwd(PathBuf::from("/"))
        .errexit(true)
        .nounset(true)
        .xtrace(true)
        .pipefail(true)
        .interactive(true)
        .cancel_flag(cancel)
        .stdout_sink(sink.clone())
        .stderr_sink(sink)
        .timeout(Duration::from_secs(1))
        .env_clear()
        .external_handler(handler)
        .build();
}

#[test]
fn shell_public_methods() {
    let mut shell = eval::Shell::new();

    // run_script returns i32
    let _code: i32 = shell.run_script("true");

    // run_program returns ExitStatus
    let program = parser::Parser::new("true").parse().unwrap();
    let _status: error::ExitStatus = shell.run_program(&program);

    // set/get methods
    let _ = shell.set_var("X", "1");
    let _val: Option<&str> = shell.get_var("X");
    shell.set_args(&["arg0", "arg1"]);
    shell.set_cwd(PathBuf::from("/tmp"));
    shell.set_timeout(Duration::from_secs(60));
    shell.set_cancel_flag(Arc::new(AtomicBool::new(false)));
    shell.set_stdout_sink(Arc::new(Mutex::new(Vec::<u8>::new())));
    shell.set_stderr_sink(Arc::new(Mutex::new(Vec::<u8>::new())));
    shell.set_external_handler(Box::new(|_args, _env| Ok(error::ExitStatus::SUCCESS)));
    shell.set_redirect_open_handler(Box::new(|request: &eval::RedirectOpen<'_>| {
        let _mode: eval::RedirectOpenMode = request.mode;
        None
    }));
    let _cwd: Option<PathBuf> = eval::external_command_cwd();

    // ShellOpts fields
    shell.opts_mut().interactive = true;

    // resolve_path
    let _p: PathBuf = shell.resolve_path("relative");
    let _p: PathBuf = shell.resolve_path("/absolute");

    // eval_command
    let cmd = &parser::Parser::new("true").parse().unwrap().commands[0];
    let _status = shell.eval_command(cmd);
}

#[test]
fn shell_accessor_methods() {
    let mut shell = eval::Shell::new();

    // Accessor methods
    let _vars: &var::Variables = shell.vars();
    let _funcs: &fxhash::FxHashMap<String, ast::Command> = shell.functions();
    let _status: error::ExitStatus = shell.exit_status();
    let _pid: u32 = shell.pid();
    let _cwd: &std::path::Path = shell.cwd();
    let _opts: &eval::ShellOpts = shell.opts();

    // ShellOpts fields via opts_mut()
    shell.opts_mut().errexit = true;
    shell.opts_mut().nounset = true;
    shell.opts_mut().xtrace = true;
    shell.opts_mut().pipefail = true;
}

#[test]
fn exit_status_api() {
    // Constants
    let _: error::ExitStatus = error::ExitStatus::SUCCESS;
    let _: error::ExitStatus = error::ExitStatus::FAILURE;
    let _: error::ExitStatus = error::ExitStatus::MISUSE;
    let _: error::ExitStatus = error::ExitStatus::NOT_FOUND;
    let _: error::ExitStatus = error::ExitStatus::NOT_EXECUTABLE;

    // Methods
    let s = error::ExitStatus::SUCCESS;
    let _: i32 = s.code();
    let _: bool = s.success();
    let _: error::ExitStatus = s.inverted();
    let _: error::ExitStatus = error::ExitStatus::from_bool(true);
    let _: error::ExitStatus = error::ExitStatus::from_signal(2);

    // Conversions
    let _: error::ExitStatus = error::ExitStatus::from(42);
    let _: i32 = i32::from(s);

    // Traits
    assert_eq!(format!("{s}"), "0");
    let _ = format!("{s:?}");
    assert_eq!(s, s);
    let _copy = s;
}

#[test]
fn shell_error_api() {
    // Variant construction
    let _ = error::ShellError::Exit(error::ExitStatus::SUCCESS);
    let _ = error::ShellError::Return(error::ExitStatus::FAILURE);
    let _ = error::ShellError::Break(1);
    let _ = error::ShellError::Continue(1);
    let _ = error::ShellError::Syntax {
        msg: "test".into(),
        span: error::Span::default(),
    };
    let _ = error::ShellError::CommandNotFound("x".into());
    let _ = error::ShellError::Io(std::io::Error::other("test"));
    let _ = error::ShellError::Runtime {
        msg: "test".into(),
        span: error::Span::default(),
    };
    let cancelled = error::ShellError::Cancelled;
    let timed_out = error::ShellError::TimedOut;
    let stopped = error::ShellError::Stopped {
        pid: 1234,
        pgid: 1234,
    };

    // Helper methods
    assert!(cancelled.is_cancelled());
    assert!(!cancelled.is_timed_out());
    assert!(cancelled.is_interrupted());
    assert!(timed_out.is_timed_out());
    assert!(!timed_out.is_cancelled());
    assert!(timed_out.is_interrupted());
    assert!(stopped.is_stopped());
    assert!(!stopped.is_interrupted());

    let exit = error::ShellError::Exit(error::ExitStatus::from(42));
    assert_eq!(exit.exit_code().unwrap().code(), 42);
    assert!(cancelled.exit_code().is_none());

    // Display and Error traits
    let _ = format!("{cancelled}");
    let _ = format!("{cancelled:?}");
    let err: &dyn std::error::Error = &cancelled;
    let _ = err.source();

    // From<io::Error>
    let _: error::ShellError = std::io::Error::other("test").into();
}

#[test]
fn parser_api() {
    // Parser construction and parsing
    let mut parser = parser::Parser::new("echo hello");
    let program: ast::Program = parser.parse().unwrap();
    assert!(!program.commands.is_empty());

    // Program has public commands field
    let _cmds: &Vec<ast::Command> = &program.commands;
}

#[test]
fn ast_types_accessible() {
    // All AST types are constructible and matchable
    let _: ast::Program = ast::Program { commands: vec![] };
    let _: ast::Word = ast::Word {
        parts: vec![],
        span: error::Span::default(),
    };

    // Command variants are matchable
    let program = parser::Parser::new("echo hi").parse().unwrap();
    match &program.commands[0] {
        ast::Command::Simple {
            assigns,
            args,
            redirs,
            span: _,
        } => {
            let _: &Vec<ast::Assignment> = assigns;
            let _: &Vec<ast::Word> = args;
            let _: &Vec<ast::Redir> = redirs;
        }
        _ => panic!("expected Simple"),
    }

    // WordPart variants
    let _ = ast::WordPart::Literal("test".into());
    let _ = ast::WordPart::SingleQuoted("test".into());
    let _ = ast::WordPart::DoubleQuoted(vec![]);
    let _ = ast::WordPart::Tilde("".into());
}

#[test]
fn builtin_list_api() {
    // BUILTIN_NAMES is a &[&str]
    let names: &[&str] = builtins::BUILTIN_NAMES;
    assert!(names.contains(&"echo"));
    assert!(names.contains(&"cd"));
    assert!(names.len() >= 25);

    // is_builtin function
    assert!(builtins::is_builtin("echo"));
    assert!(!builtins::is_builtin("nonexistent"));
}

#[test]
fn encoding_api() {
    // bytes_to_str and str_to_bytes are public
    let s: String = encoding::bytes_to_str(b"hello");
    let b: Vec<u8> = encoding::str_to_bytes(&s);
    assert_eq!(b, b"hello");

    // Roundtrip with non-UTF-8
    let input = &[0x80u8, 0xFF];
    let encoded = encoding::bytes_to_str(input);
    let decoded = encoding::str_to_bytes(&encoded);
    assert_eq!(decoded, input);
}

#[test]
fn expand_trait_accessible() {
    // ShellExpand trait is public (for custom implementations)
    fn _takes_expand(_: &mut dyn expand::ShellExpand) {}
}

#[test]
fn glob_api() {
    use std::path::Path;
    // Public functions
    let _: bool = glob::has_glob_chars("*.txt");
    let _: bool = glob::fnmatch("*.txt", "file.txt");
    let _: Vec<String> = glob::glob("*.nonexistent", Path::new("/tmp"));
}

#[test]
fn variables_api() {
    let mut vars = var::Variables::new();
    let _ = var::Variables::new_clean();

    // set/get
    vars.set("X", "1").unwrap();
    let _: Option<&str> = vars.get("X");
    let _: Option<i64> = vars.get_int("X");
    vars.set_int("Y", 42).unwrap();

    // mutations
    vars.export("X");
    vars.set_readonly("X");
    let _ = vars.unset("nonexistent");

    // scope
    vars.push_scope();
    vars.make_local("Z");
    vars.pop_scope();

    // special
    let _: Option<String> = vars.get_special("?", error::ExitStatus::SUCCESS, 1, "", None);
    let _: String = vars.ifs();
    let _: Vec<(String, String)> = vars.exported_env();

    // positional
    vars.positional = vec!["a".into()];
    let _: &shell_bytes::ShellBytes = &vars.arg0;
}

#[test]
fn span_api() {
    let span = error::Span {
        offset: 0,
        line: 1,
        col: 1,
    };
    let _default = error::Span::default();
    assert_eq!(format!("{span}"), "1:1");
    let _ = format!("{span:?}");
    assert_eq!(span, span);
    let _copy = span;
}

#[test]
fn lexer_ctlesc_public() {
    // CTLESC constant is public (for custom pattern handling)
    let _: char = lexer::CTLESC;
}

#[test]
fn result_type_alias() {
    // error::Result<T> is available
    fn _example() -> error::Result<()> {
        Ok(())
    }
}
