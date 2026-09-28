//! Recoverable filesystem operations for ish.
//!
//! This crate owns native `rm`/`mv`, protected redirection opens, the recovery
//! store and journals, and conditional undo/redo. It knows nothing about
//! ish's parser, history, line editor, or renderer: the shell passes resolved
//! paths, configuration, execution identity, cancellation, and output
//! handles through the small interfaces here.
//!
//! Capture is rootless and covers only what the shell itself performs:
//! native operations and redirections are recorded before they mutate
//! anything. Other external commands run normally and are not captured.

pub mod cli;
mod fault;
mod journal;
mod ops;
mod preserve;
mod replay;
mod retention;
mod store;
mod sys;
mod txn;

pub use ops::{RedirMode, mv, open_redirect, rm};
pub use retention::maybe_collect;
pub use store::root_for_home;
pub use txn::{Session, Suspended, Txn};

/// The internals that this crate's own integration tests, test fixture, and
/// benchmarks reach into, listed item by item: journal and model inspection,
/// stores, fault injection, and the nonexecuting refusal guards. Not part of
/// the interface the shell uses, and free to change with the implementation.
#[doc(hidden)]
pub mod testing {
    pub use crate::ops::{Guard, Refusal, resolve};

    pub mod fault {
        pub use crate::fault::inject_spec;
    }
    pub mod journal {
        pub use crate::journal::{Action, Content, Displaced, Strength, read};
    }
    pub mod replay {
        pub use crate::replay::{Committed, Model, Options, Report, run};
    }
    pub mod retention {
        pub use crate::retention::{GcReport, collect, maybe_collect, usage};
    }
    pub mod store {
        pub use crate::store::{Home, Stores, VolumeState, create_volume, root_for_home};
    }
    pub mod sys {
        pub use crate::sys::{
            clone_file, fs_type_name, fstat, lstat, open_dir, read_full_at, set_flags,
        };
    }
}

use std::io::Write;
use std::sync::atomic::AtomicBool;

/// Recovery settings, read from the shell's variables at each transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// `ISH_UNDO`: protect native commands and redirections (`on`/`off`).
    pub enabled: bool,
    /// `ISH_UNDO_COPY_LIMIT`: bytes the byte-copy fallback may copy for one
    /// operation when cloning is unavailable. Clones are not limited by it.
    pub copy_limit: u64,
    /// `ISH_UNDO_MAX_SIZE`: logical size of retained versions.
    pub max_bytes: u64,
    /// `ISH_UNDO_MAX_ENTRIES`: retained transactions.
    pub max_entries: u64,
    /// `ISH_UNDO_MAX_DAYS`: age after which transactions are collected.
    pub max_age_days: u64,
    /// `ISH_UNDO_MIN_FREE`: free space to keep on a store's filesystem.
    pub min_free: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            copy_limit: 64 << 20,
            max_bytes: 8 << 30,
            max_entries: 500,
            max_age_days: 30,
            min_free: 1 << 30,
        }
    }
}

impl Config {
    /// Build a configuration from variable lookups. Invalid values keep the
    /// default and produce a warning.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> (Config, Vec<String>) {
        let mut config = Config::default();
        let mut warnings = Vec::new();
        if let Some(v) = get("ISH_UNDO") {
            match v.as_str() {
                "on" | "1" | "yes" | "" => config.enabled = true,
                "off" | "0" | "no" => config.enabled = false,
                other => warnings.push(format!("ISH_UNDO: expected on or off, got {other:?}")),
            }
        }
        let mut size = |name: &str, slot: &mut u64| {
            if let Some(v) = get(name) {
                match parse_size(&v) {
                    Some(n) => *slot = n,
                    None => warnings.push(format!("{name}: invalid size {v:?}")),
                }
            }
        };
        size("ISH_UNDO_COPY_LIMIT", &mut config.copy_limit);
        size("ISH_UNDO_MAX_SIZE", &mut config.max_bytes);
        size("ISH_UNDO_MIN_FREE", &mut config.min_free);
        size("ISH_UNDO_MAX_ENTRIES", &mut config.max_entries);
        size("ISH_UNDO_MAX_DAYS", &mut config.max_age_days);
        (config, warnings)
    }
}

/// Parse `123`, `64K`, `10M`, `2G`, or `1T` (binary multiples).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, mult) = match s.char_indices().last()? {
        (i, 'K' | 'k') => (&s[..i], 1u64 << 10),
        (i, 'M' | 'm') => (&s[..i], 1 << 20),
        (i, 'G' | 'g') => (&s[..i], 1 << 30),
        (i, 'T' | 't') => (&s[..i], 1 << 40),
        _ => (s, 1),
    };
    digits.parse::<u64>().ok()?.checked_mul(mult)
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n}B")
    } else if v < 10.0 {
        format!("{v:.1}{}", UNITS[unit])
    } else {
        format!("{v:.0}{}", UNITS[unit])
    }
}

/// Output, input, and cancellation for a builtin invocation.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// Ask a yes/no question on the terminal. `None` means end of input or
    /// interruption.
    pub confirm: &'a mut dyn FnMut(&str) -> Option<bool>,
    /// Set by the shell when the user interrupts (Ctrl+C).
    pub cancel: &'a AtomicBool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_with_binary_suffixes() {
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("64K"), Some(65536));
        assert_eq!(parse_size("2g"), Some(2 << 30));
        assert_eq!(parse_size("x"), None);
        assert_eq!(parse_size("99999999999T"), None);
    }

    #[test]
    fn config_reads_variables_and_reports_bad_values() {
        let (config, warnings) = Config::from_vars(|name| match name {
            "ISH_UNDO" => Some("off".into()),
            "ISH_UNDO_COPY_LIMIT" => Some("1M".into()),
            "ISH_UNDO_MAX_ENTRIES" => Some("many".into()),
            _ => None,
        });
        assert!(!config.enabled);
        assert_eq!(config.copy_limit, 1 << 20);
        assert_eq!(config.max_entries, Config::default().max_entries);
        assert_eq!(warnings.len(), 1);
    }
}
