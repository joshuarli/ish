//! # epsh — Embeddable POSIX Shell
//!
//! A non-interactive POSIX shell designed for embedding in Rust coding agents.
//! Executes scripts and commands in-process with full control over working
//! directory, output capture, cancellation, and timeouts.
//!
//! ## Quick start
//!
//! ```no_run
//! use epsh::eval::Shell;
//!
//! let mut shell = Shell::new();
//! let exit_code = shell.run_script("echo hello world");
//! ```
//!
//! ## Builder pattern
//!
//! ```no_run
//! use epsh::eval::Shell;
//! use std::sync::{Arc, Mutex};
//! use std::path::PathBuf;
//! use std::time::Duration;
//!
//! let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
//! let mut shell = Shell::builder()
//!     .cwd(PathBuf::from("/project"))
//!     .errexit(true)
//!     .stdout_sink(stdout.clone())
//!     .timeout(Duration::from_secs(120))
//!     .build();
//! ```

/// Arithmetic expression evaluator for `$((expr))`.
pub mod arith;
/// AST types: [`ast::Command`], [`ast::Word`], [`ast::WordPart`], etc.
pub mod ast;
/// Shell builtins and [`builtins::BUILTIN_NAMES`].
pub mod builtins;
/// Byte-preserving encoding for non-UTF-8 shell data.
pub mod encoding;
/// Error types: [`error::ShellError`], [`error::ExitStatus`].
pub mod error;
/// Shell interpreter: [`eval::Shell`], [`eval::ShellBuilder`].
pub mod eval;
/// Word expansion: tilde, parameter, arithmetic, field splitting, and pathname expansion.
pub mod expand;
/// Glob pattern matching and pathname expansion.
pub mod glob;
/// Shell lexer that converts source text into tokens and shell words.
pub mod lexer;
/// Recursive-descent parser producing [`ast::Program`].
pub mod parser;
mod redirect;
/// Byte-preserving shell runtime values and OS/libc conversion helpers.
pub mod shell_bytes;
pub(crate) mod signal;
pub(crate) mod sys;
mod test_cmd;
/// Variable storage with scope stack.
pub mod var;
