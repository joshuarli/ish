use std::ffi::OsStr;

pub fn shell_setenv(key: &str, val: &str) {
    unsafe { std::env::set_var(key, val) }
}

pub fn shell_setenv_os(key: &str, val: impl AsRef<OsStr>) {
    unsafe { std::env::set_var(key, val) }
}

pub fn shell_unsetenv(key: &str) {
    unsafe { std::env::remove_var(key) }
}

pub mod alias;
pub mod builtin;
pub mod capture;
pub mod cd_rewrite;
pub mod complete;
pub mod config;
pub mod denv;
pub mod diagnostics;
pub mod dispatch;
pub mod external;
pub mod frecency;
mod fs_command;
pub mod foreground;
pub mod history;
pub mod input;
pub mod job;
pub mod line;
pub mod mode;
pub mod ls;
pub mod path;
pub mod prompt;
pub mod render;
pub mod signal;
mod static_word;
pub(crate) mod sys;
pub mod term;
