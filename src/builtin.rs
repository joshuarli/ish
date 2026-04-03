use crate::ls;
use crate::path;

/// Builtins handled by ish's interactive layer (not by epsh).
const ISH_BUILTINS: &[&str] = &[
    "fg", "z", "l", "c", "w", "which", "type",
    "copy-scrollback", "history",
    "alias", "denv",
];

pub fn is_ish_builtin(name: &str) -> bool {
    ISH_BUILTINS.contains(&name)
}

/// Check if a command name is a builtin in either ish or epsh.
pub fn is_builtin(name: &str) -> bool {
    is_ish_builtin(name) || epsh::builtins::is_builtin(name)
}

/// All builtin names (ish + epsh), for completion.
pub fn all_builtin_names() -> impl Iterator<Item = &'static str> {
    ISH_BUILTINS
        .iter()
        .copied()
        .chain(epsh::builtins::BUILTIN_NAMES.iter().copied())
}

/// Run a `w`/`which`/`type` lookup. Checks aliases, builtins, functions, PATH.
pub fn builtin_w(
    args: &[String],
    aliases: &crate::alias::AliasMap,
    functions: &std::collections::HashMap<String, epsh::ast::Command>,
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

    if functions.contains_key(name.as_str()) {
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

/// Run the `l` builtin (native ls).
pub fn builtin_l(args: &[String]) -> i32 {
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
