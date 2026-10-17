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
pub mod complete;
pub mod config;
pub mod denv;
pub mod dispatch;
pub mod frecency;
pub mod history;
pub mod input;
pub mod job;
pub mod line;
pub mod ls;
pub mod path;
pub mod prompt;
pub mod render;
pub mod signal;
mod static_word;
pub mod sys;
pub mod term;
