/// Set an environment variable. Single-threaded shell -- always safe.
pub fn shell_setenv(key: &str, val: &str) {
    unsafe { std::env::set_var(key, val) }
}

/// Remove an environment variable. Single-threaded shell -- always safe.
pub fn shell_unsetenv(key: &str) {
    unsafe { std::env::remove_var(key) }
}

pub mod alias;
pub mod builtin;
pub mod complete;
pub mod config;
pub mod denv;
pub mod error;
pub mod exec;
pub mod expand;
pub mod history;
pub mod input;
pub mod job;
pub mod line;
pub mod ls;
pub mod math;
pub mod parse;
pub mod prompt;
pub mod render;
pub mod signal;
pub mod sys;
pub mod term;
