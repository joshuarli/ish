use crate::ls;
use crate::path;

/// Additional builtins provided by ish beyond epsh.
///
/// Builtins that override epsh commands are dispatched separately in main.rs.
const ISH_EXTENSION_BUILTINS: &[&str] = &[
    "fg",
    "z",
    "l",
    "c",
    "w",
    "which",
    "type",
    "copy-scrollback",
    "history",
    "alias",
    "denv",
    "ish-dump",
];

pub fn is_ish_extension_builtin(name: &str) -> bool {
    ISH_EXTENSION_BUILTINS.contains(&name)
}

/// Check if a command name is a builtin in either ish or epsh.
pub fn is_builtin(name: &str) -> bool {
    is_ish_extension_builtin(name) || epsh::builtins::is_builtin(name)
}

/// All builtin names (ish + epsh), for completion.
pub fn all_builtin_names() -> impl Iterator<Item = &'static str> {
    ISH_EXTENSION_BUILTINS
        .iter()
        .copied()
        .chain(epsh::builtins::BUILTIN_NAMES.iter().copied())
}

/// Locate a command through aliases, builtins, functions, or PATH.
pub fn locate_command(
    args: &[String],
    aliases: &crate::alias::AliasMap,
    function_exists: impl Fn(&str) -> bool,
) -> i32 {
    if args.is_empty() {
        eprintln!("ish: w: expected command name");
        return 1;
    }

    let name = &args[0];

    if let Some(exp) = aliases.get(name) {
        println!("alias: {} {}", name, exp.join(" "));
        return 0;
    }

    if is_builtin(name) {
        println!("builtin");
        return 0;
    }

    if function_exists(name) {
        println!("function");
        return 0;
    }

    if let Some(p) = path::scan_path(name) {
        println!("{}", p.display());
        return 0;
    }

    eprintln!("ish: w: not found: {name}");
    1
}

/// List directories using ish's native directory listing.
pub fn list_directory(args: &[String]) -> i32 {
    if args.is_empty() {
        ls::list_dir(".")
    } else {
        let mut status = 0;
        let label = args.len() > 1;
        for (i, arg) in args.iter().enumerate() {
            if label {
                if i > 0 {
                    println!();
                }
                println!("{arg}:");
            }
            let s = ls::list_dir(arg);
            if s != 0 {
                status = s;
            }
        }
        status
    }
}
