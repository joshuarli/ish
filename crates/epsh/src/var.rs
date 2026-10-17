use fxhash::{FxHashMap, FxHashSet};
use std::env;
use std::ffi::OsString;

use crate::error::ExitStatus;
use crate::shell_bytes::ShellBytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VarFlags(u8);

impl VarFlags {
    pub const EXPORT: u8 = 0b0001;
    pub const READONLY: u8 = 0b0010;

    pub fn new() -> Self {
        VarFlags(0)
    }

    pub fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }

    pub fn set(&mut self, flag: u8) {
        self.0 |= flag;
    }

    pub fn clear(&mut self, flag: u8) {
        self.0 &= !flag;
    }
}

fn int_cache(value: Option<&ShellBytes>) -> Option<i64> {
    value
        .and_then(|s| s.as_utf8_str())
        .and_then(|s| s.parse::<i64>().ok())
}

pub(crate) fn is_shell_name_bytes(bytes: &[u8]) -> Option<String> {
    let first = *bytes.first()?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    if !bytes
        .iter()
        .all(|b| *b == b'_' || b.is_ascii_alphanumeric())
    {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(String::from)
}

/// A shell variable.
#[derive(Debug, Clone)]
pub struct Var {
    pub value: Option<ShellBytes>,
    pub flags: VarFlags,
    /// Cached integer parse of value. Avoids repeated string→i64 conversion
    /// in arithmetic and test builtins. Updated on every set.
    int_cache: Option<i64>,
}

impl Var {
    fn new(value: Option<ShellBytes>, flags: VarFlags) -> Self {
        let int_cache = int_cache(value.as_ref());
        Var {
            value,
            flags,
            int_cache,
        }
    }
}

/// Saved variable state for scope restoration.
#[derive(Debug)]
struct SavedVar {
    name: String,
    previous: Option<Var>,
}

/// Variable scope pushed on function call or dot-script.
#[derive(Debug)]
pub struct Scope {
    saved: Vec<SavedVar>,
}

/// Variable storage with scoping support.
pub struct Variables {
    /// All shell variables (flat namespace, latest value wins).
    vars: FxHashMap<String, Var>,
    /// Environment entries inherited from the parent that are not representable
    /// as shell variables. Preserved for child processes.
    inherited_env: Vec<(ShellBytes, ShellBytes)>,
    /// Stack of scopes for local variable restoration.
    scopes: Vec<Scope>,
    /// Positional parameters ($1, $2, ...).
    pub positional: Vec<ShellBytes>,
    /// $0 — script name or shell name.
    pub arg0: ShellBytes,
}

impl Default for Variables {
    fn default() -> Self {
        Self::new()
    }
}

impl Variables {
    /// Create variables with no inherited environment.
    pub fn new_clean() -> Self {
        let mut vars = FxHashMap::default();
        vars.insert(
            "IFS".into(),
            Var::new(Some(ShellBytes::from(" \t\n")), VarFlags::new()),
        );
        Variables {
            vars,
            inherited_env: Vec::new(),
            scopes: Vec::new(),
            positional: Vec::new(),
            arg0: ShellBytes::from("epsh"),
        }
    }

    pub fn new() -> Self {
        let mut vars = FxHashMap::default();
        let mut inherited_env = Vec::new();

        for (key, value) in env::vars_os() {
            let key_bytes = ShellBytes::from_os_string(key);
            let value_bytes = ShellBytes::from_os_string(value);
            if let Some(name) = is_shell_name_bytes(key_bytes.as_bytes()) {
                let mut f = VarFlags::new();
                f.set(VarFlags::EXPORT);
                vars.insert(name, Var::new(Some(value_bytes), f));
            } else {
                inherited_env.push((key_bytes, value_bytes));
            }
        }

        if !vars.contains_key("IFS") {
            vars.insert(
                "IFS".into(),
                Var::new(Some(ShellBytes::from(" \t\n")), VarFlags::new()),
            );
        }

        Variables {
            vars,
            inherited_env,
            scopes: Vec::new(),
            positional: Vec::new(),
            arg0: ShellBytes::from("epsh"),
        }
    }

    /// Get a variable's raw value. Returns None if unset.
    pub fn get_bytes(&self, name: &str) -> Option<&ShellBytes> {
        self.vars.get(name).and_then(|v| v.value.as_ref())
    }

    /// Get a variable's value as UTF-8, if valid. Returns None if unset or not UTF-8.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.get_bytes(name).and_then(ShellBytes::as_utf8_str)
    }

    /// Get a variable's value using the shell's byte-preserving string encoding.
    pub fn get_shell(&self, name: &str) -> Option<String> {
        self.get_bytes(name).map(ShellBytes::to_shell_string)
    }

    /// Set a variable from shell text. Returns Err if readonly.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        self.set_bytes(name, ShellBytes::from_str_lossless(value))
    }

    /// Set a variable from raw shell bytes. Returns Err if readonly.
    pub fn set_bytes(&mut self, name: &str, value: ShellBytes) -> Result<(), String> {
        if let Some(existing) = self.vars.get(name)
            && existing.flags.has(VarFlags::READONLY)
        {
            return Err(format!("{name}: readonly variable"));
        }

        let entry = self
            .vars
            .entry(name.to_string())
            .or_insert_with(|| Var::new(None, VarFlags::new()));
        entry.int_cache = int_cache(Some(&value));
        entry.value = Some(value);

        Ok(())
    }

    /// Get a variable's cached integer value (if it parses as i64).
    pub fn get_int(&self, name: &str) -> Option<i64> {
        self.vars.get(name).and_then(|v| v.int_cache)
    }

    /// Set a variable to an integer value.
    pub fn set_int(&mut self, name: &str, value: i64) -> Result<(), String> {
        if let Some(existing) = self.vars.get_mut(name) {
            if existing.flags.has(VarFlags::READONLY) {
                return Err(format!("{name}: readonly variable"));
            }
            existing.int_cache = Some(value);
            existing.value = Some(ShellBytes::from_vec(value.to_string().into_bytes()));
            return Ok(());
        }

        self.vars.insert(
            name.to_string(),
            Var {
                value: Some(ShellBytes::from_vec(value.to_string().into_bytes())),
                flags: VarFlags::new(),
                int_cache: Some(value),
            },
        );
        Ok(())
    }

    /// Unset a variable. Returns Err if readonly.
    pub fn unset(&mut self, name: &str) -> Result<(), String> {
        if let Some(existing) = self.vars.get(name)
            && existing.flags.has(VarFlags::READONLY)
        {
            return Err(format!("{name}: readonly variable"));
        }
        self.vars.remove(name);
        Ok(())
    }

    /// Mark a variable as exported.
    pub fn export(&mut self, name: &str) {
        let entry = self
            .vars
            .entry(name.to_string())
            .or_insert_with(|| Var::new(None, VarFlags::new()));
        entry.flags.set(VarFlags::EXPORT);
    }

    /// Mark a variable as readonly.
    pub fn set_readonly(&mut self, name: &str) {
        let entry = self
            .vars
            .entry(name.to_string())
            .or_insert_with(|| Var::new(None, VarFlags::new()));
        entry.flags.set(VarFlags::READONLY);
    }

    /// Push a new scope (for function calls).
    pub fn push_scope(&mut self) {
        self.scopes.push(Scope { saved: Vec::new() });
    }

    /// Pop the current scope, restoring all saved variables.
    pub fn pop_scope(&mut self) {
        if let Some(scope) = self.scopes.pop() {
            for saved in scope.saved.into_iter().rev() {
                match saved.previous {
                    Some(var) => {
                        self.vars.insert(saved.name, var);
                    }
                    None => {
                        self.vars.remove(&saved.name);
                    }
                }
            }
        }
    }

    /// Declare a local variable in the current scope.
    pub fn make_local(&mut self, name: &str) {
        if let Some(scope) = self.scopes.last_mut() {
            let previous = self.vars.get(name).cloned();
            scope.saved.push(SavedVar {
                name: name.to_string(),
                previous,
            });
        }
    }

    pub fn positional_shell_strings(&self) -> Vec<String> {
        self.positional
            .iter()
            .map(ShellBytes::to_shell_string)
            .collect()
    }

    pub fn positional_join_shell(&self, sep: &str) -> String {
        self.positional_shell_strings().join(sep)
    }

    pub fn arg0_shell(&self) -> String {
        self.arg0.to_shell_string()
    }

    /// Get a special parameter value ($?, $$, $#, $@, $*, $!, $-, $0, $1...).
    pub fn get_special(
        &self,
        name: &str,
        exit_status: ExitStatus,
        shell_pid: u32,
        shell_flags: &str,
        last_bg_pid: Option<u32>,
    ) -> Option<String> {
        match name {
            "?" => Some(exit_status.code().to_string()),
            "$" => Some(shell_pid.to_string()),
            "#" => Some(self.positional.len().to_string()),
            "0" => Some(self.arg0.to_shell_string()),
            "-" => Some(shell_flags.to_string()),
            "!" => last_bg_pid.map(|p| p.to_string()),
            "@" | "*" => Some(self.positional_shell_strings().join(" ")),
            _ => {
                if let Ok(n) = name.parse::<usize>() {
                    if n >= 1 {
                        self.positional.get(n - 1).map(ShellBytes::to_shell_string)
                    } else {
                        None
                    }
                } else {
                    self.get_shell(name)
                }
            }
        }
    }

    /// Get the IFS value (defaults to " \t\n").
    pub fn ifs(&self) -> String {
        self.get_shell("IFS").unwrap_or_else(|| " \t\n".into())
    }

    /// Exported shell variables as shell strings.
    pub fn exported_env(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(_, v)| v.flags.has(VarFlags::EXPORT) && v.value.is_some())
            .map(|(k, v)| (k.clone(), v.value.as_ref().unwrap().to_shell_string()))
            .collect()
    }

    /// Exported shell variables as raw bytes.
    pub fn exported_env_bytes(&self) -> Vec<(String, ShellBytes)> {
        self.vars
            .iter()
            .filter(|(_, v)| v.flags.has(VarFlags::EXPORT) && v.value.is_some())
            .map(|(k, v)| (k.clone(), v.value.as_ref().unwrap().clone()))
            .collect()
    }

    /// Build the child environment, preserving inherited non-shell entries.
    pub fn env_for_command_os(
        &self,
        assigns: &[(String, ShellBytes)],
    ) -> Vec<(OsString, OsString)> {
        let mut shadowed = FxHashSet::default();
        let mut assigned = FxHashSet::default();
        for name in self.vars.iter().filter_map(|(k, v)| {
            if v.flags.has(VarFlags::EXPORT) && v.value.is_some() {
                Some(k.as_bytes().to_vec())
            } else {
                None
            }
        }) {
            shadowed.insert(name);
        }
        for (name, _) in assigns {
            assigned.insert(name.as_bytes().to_vec());
            shadowed.insert(name.as_bytes().to_vec());
        }

        let mut env = Vec::new();
        for (name, value) in &self.inherited_env {
            if !shadowed.contains(name.as_bytes()) {
                env.push((name.to_os_string(), value.to_os_string()));
            }
        }
        for (name, value) in self.exported_env_bytes() {
            if !assigned.contains(name.as_bytes()) {
                env.push((OsString::from(name), value.to_os_string()));
            }
        }
        for (name, value) in assigns {
            env.push((OsString::from(name), value.to_os_string()));
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get() {
        let mut vars = Variables::new();
        vars.set("FOO", "bar").unwrap();
        assert_eq!(vars.get("FOO"), Some("bar"));
    }

    #[test]
    fn unset() {
        let mut vars = Variables::new();
        vars.set("FOO", "bar").unwrap();
        vars.unset("FOO").unwrap();
        assert_eq!(vars.get("FOO"), None);
    }

    #[test]
    fn readonly() {
        let mut vars = Variables::new();
        vars.set("FOO", "bar").unwrap();
        vars.set_readonly("FOO");
        assert!(vars.set("FOO", "baz").is_err());
        assert!(vars.unset("FOO").is_err());
    }

    #[test]
    fn scope_local() {
        let mut vars = Variables::new();
        vars.set("X", "outer").unwrap();
        vars.push_scope();
        vars.make_local("X");
        vars.set("X", "inner").unwrap();
        assert_eq!(vars.get("X"), Some("inner"));
        vars.pop_scope();
        assert_eq!(vars.get("X"), Some("outer"));
    }

    #[test]
    fn scope_new_local() {
        let mut vars = Variables::new();
        vars.push_scope();
        vars.make_local("Y");
        vars.set("Y", "local").unwrap();
        assert_eq!(vars.get("Y"), Some("local"));
        vars.pop_scope();
        assert_eq!(vars.get("Y"), None);
    }

    #[test]
    fn positional_params() {
        let mut vars = Variables::new();
        vars.positional = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(
            vars.get_special("#", ExitStatus::SUCCESS, 1, "", None),
            Some("3".into())
        );
        assert_eq!(
            vars.get_special("1", ExitStatus::SUCCESS, 1, "", None),
            Some("a".into())
        );
        assert_eq!(
            vars.get_special("3", ExitStatus::SUCCESS, 1, "", None),
            Some("c".into())
        );
        assert_eq!(
            vars.get_special("4", ExitStatus::SUCCESS, 1, "", None),
            None
        );
    }

    #[test]
    fn ifs_default() {
        let vars = Variables::new();
        assert_eq!(vars.ifs(), " \t\n");
    }

    #[test]
    fn env_for_command_os_is_exact_and_assignment_aware() {
        let mut vars = Variables::new_clean();
        vars.inherited_env = vec![(ShellBytes::from("1INHERITED"), ShellBytes::from("ambient"))];
        vars.set("KEEP", "stored").unwrap();
        vars.export("KEEP");
        vars.set("REMOVE", "gone").unwrap();
        vars.export("REMOVE");
        vars.unset("REMOVE").unwrap();

        let env = vars.env_for_command_os(&[("KEEP".into(), ShellBytes::from("prefix"))]);
        let mut entries: Vec<(String, String)> = env
            .into_iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        entries.sort();

        assert_eq!(
            entries,
            vec![
                ("1INHERITED".into(), "ambient".into()),
                ("KEEP".into(), "prefix".into()),
            ]
        );
    }
}
