use std::path::Path;

use crate::arith;
use crate::ast::*;
use crate::error::{ExitStatus, ShellError};
use crate::glob;
use crate::var::Variables;

/// Trait abstracting the shell state needed during word expansion.
/// Allows expand functions to access variables, exit status, PID, and
/// command substitution without requiring a direct `&mut Shell` reference.
pub trait ShellExpand {
    fn vars(&self) -> &Variables;
    fn vars_mut(&mut self) -> &mut Variables;
    fn exit_status(&self) -> ExitStatus;
    fn pid(&self) -> u32;
    fn cwd(&self) -> &Path;
    fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String>;
    /// Active shell option flags for `$-` (e.g. "eux").
    fn shell_flags(&self) -> String;
    /// Last background PID for `$!`, if any.
    fn last_bg_pid(&self) -> Option<u32>;
    /// Whether set -u (nounset) is active.
    fn nounset(&self) -> bool;
    /// Whether set -f (noglob) is active.
    fn noglob(&self) -> bool;
}

/// An expanded word fragment with metadata for field splitting.
#[derive(Debug, Clone)]
pub struct ExpandedWord {
    pub value: String,
    /// True if this fragment came from an unquoted expansion ($var, $(cmd))
    /// and should be subject to field splitting.
    pub split_fields: bool,
    /// True if this starts a new word boundary (for "$@" expansion).
    pub word_break: bool,
}

/// Expand a Word AST node into a list of strings (after field splitting and globbing).
/// This is the full expansion pipeline for command arguments:
///   tilde → parameter → command subst → arithmetic → field split → glob → quote removal
pub fn expand_word_to_fields(
    word: &Word,
    sh: &mut dyn ShellExpand,
) -> crate::error::Result<Vec<String>> {
    // Step 1: Expand to intermediate fragments with split_fields metadata
    let fragments = expand_word_parts(&word.parts, sh, false)?;

    // Step 2: Field splitting on fragments marked split_fields=true
    let ifs = sh.vars().ifs();
    let split = field_split(&fragments, &ifs);

    // Step 3: Pathname expansion (globbing) on results
    let mut result = Vec::new();
    let cwd = sh.cwd();
    for field in split {
        if !sh.noglob() && glob::has_glob_chars(&field) {
            let matches = glob::glob(&field, cwd);
            if matches.is_empty() {
                // No matches: keep the pattern literal, with quote removal
                result.push(remove_glob_escapes(&field));
            } else {
                result.extend(matches);
            }
        } else {
            // Quote removal: strip backslash-escapes of glob chars
            result.push(remove_glob_escapes(&field));
        }
    }

    Ok(result)
}

/// Expand word parts into a fnmatch-ready pattern string.
/// Like expand_word_to_string but escapes glob metacharacters (* ? [ ])
/// within quoted regions (SingleQuoted, DoubleQuoted).
/// Used for: ${var%pattern}, ${var#pattern}, case patterns.
pub fn expand_pattern(
    parts: &[WordPart],
    sh: &mut dyn ShellExpand,
) -> crate::error::Result<String> {
    use crate::lexer::CTLESC;
    let mut result = String::new();
    for part in parts {
        match part {
            WordPart::Literal(s) => {
                // Unquoted literal — glob chars are active, CTLESC preserved
                result.push_str(&s.to_shell_string());
            }
            WordPart::SingleQuoted(s) => {
                // Single-quoted — escape glob chars for fnmatch
                for c in s.to_shell_string().chars() {
                    if matches!(c, '*' | '?' | '[' | ']' | '\\') {
                        result.push(CTLESC);
                    }
                    result.push(c);
                }
            }
            WordPart::DoubleQuoted(inner) => {
                // Double-quoted — expand inner parts but escape glob chars
                let inner_expanded = expand_word_parts_inner(inner, sh, true, false)?;
                let text: String = inner_expanded.into_iter().map(|f| f.value).collect();
                for c in text.chars() {
                    if matches!(c, '*' | '?' | '[' | ']' | '\\') {
                        result.push(CTLESC);
                    }
                    result.push(c);
                }
            }
            WordPart::Param(param) => {
                // Parameter expansion in pattern — value is literal (escape globs)
                let value = expand_param(param, sh)?;
                for c in value.chars() {
                    if matches!(c, '*' | '?' | '[' | ']' | '\\') {
                        result.push(CTLESC);
                    }
                    result.push(c);
                }
            }
            _ => {
                // Other parts (CmdSubst, Arith, etc.) — expand and escape
                let frags = expand_word_parts_inner(std::slice::from_ref(part), sh, true, false)?;
                let text: String = frags.into_iter().map(|f| f.value).collect();
                for c in text.chars() {
                    if matches!(c, '*' | '?' | '[' | ']' | '\\') {
                        result.push(CTLESC);
                    }
                    result.push(c);
                }
            }
        }
    }
    Ok(result)
}

/// Expand a Word to a single string (no field splitting or globbing).
/// Used for: here-doc bodies, assignment values, case words.
pub fn expand_word_to_string(
    word: &Word,
    sh: &mut dyn ShellExpand,
) -> crate::error::Result<String> {
    let fragments = expand_word_parts(&word.parts, sh, true)?;
    Ok(fragments.into_iter().map(|f| f.value).collect())
}

/// Expand a list of WordParts into ExpandedWord fragments.
/// `in_param_word`: true when expanding inside ${var+word} etc. — literals should
/// be marked for field splitting since they're part of an expansion result.
fn expand_word_parts(
    parts: &[WordPart],
    sh: &mut dyn ShellExpand,
    quoted_context: bool,
) -> crate::error::Result<Vec<ExpandedWord>> {
    expand_word_parts_inner(parts, sh, quoted_context, false)
}

fn expand_word_parts_inner(
    parts: &[WordPart],
    sh: &mut dyn ShellExpand,
    quoted_context: bool,
    in_param_word: bool,
) -> crate::error::Result<Vec<ExpandedWord>> {
    let mut result = Vec::new();

    for part in parts {
        match part {
            WordPart::Literal(s) => {
                result.push(ExpandedWord {
                    value: s.to_shell_string(),
                    // Literals from ${...} words are subject to field splitting
                    split_fields: in_param_word && !quoted_context,
                    word_break: false,
                });
            }
            WordPart::SingleQuoted(s) => {
                result.push(ExpandedWord {
                    value: s.to_shell_string(),
                    split_fields: false,
                    word_break: false,
                });
            }
            WordPart::DoubleQuoted(inner) => {
                // Check if inner contains a bare $@ — needs special multi-field expansion
                let has_at = inner.iter().any(|p| matches!(p, WordPart::Param(pe) if pe.name == "@" && matches!(pe.op, ParamOp::Normal)));

                if has_at && inner.len() == 1 {
                    // "$@" — expand to separate fields
                    // First element merges with any preceding content in this word
                    let positional = sh.vars().positional_shell_strings();
                    for (i, arg) in positional.iter().enumerate() {
                        result.push(ExpandedWord {
                            value: arg.clone(),
                            split_fields: false,
                            word_break: i > 0,
                        });
                    }
                } else if has_at {
                    // Mixed case: "prefix$@suffix" — expand $@ into multiple fields
                    // with prefix on first and suffix on last
                    let mut prefix = String::new();
                    let mut suffix_parts: Vec<WordPart> = Vec::new();
                    let mut found_at = false;
                    for p in inner {
                        if !found_at {
                            if matches!(p, WordPart::Param(pe) if pe.name == "@" && matches!(pe.op, ParamOp::Normal))
                            {
                                found_at = true;
                            } else {
                                let expanded =
                                    expand_word_parts(std::slice::from_ref(p), sh, true)?;
                                for f in expanded {
                                    prefix.push_str(&f.value);
                                }
                            }
                        } else {
                            suffix_parts.push(p.clone());
                        }
                    }
                    let suffix_frags = expand_word_parts(&suffix_parts, sh, true)?;
                    let suffix: String = suffix_frags.into_iter().map(|f| f.value).collect();

                    if sh.vars().positional.is_empty() {
                        // "$@" with no positional params produces nothing (not even empty string)
                    } else {
                        let positional = sh.vars().positional_shell_strings();
                        let pos_len = positional.len();
                        for (i, arg) in positional.iter().enumerate() {
                            let mut val = String::new();
                            if i == 0 {
                                val.push_str(&prefix);
                            }
                            val.push_str(arg);
                            if i == pos_len - 1 {
                                val.push_str(&suffix);
                            }
                            result.push(ExpandedWord {
                                value: val,
                                split_fields: false,
                                word_break: i > 0,
                            });
                        }
                    }
                } else {
                    // No $@ — normal double-quote handling
                    let expanded = expand_word_parts(inner, sh, true)?;
                    let value: String = expanded.into_iter().map(|f| f.value).collect();
                    result.push(ExpandedWord {
                        value,
                        split_fields: false,
                        word_break: false,
                    });
                }
            }
            WordPart::Param(param) => {
                // $@ unquoted: expand to separate fields
                if param.name == "@" && matches!(param.op, ParamOp::Normal) && !quoted_context {
                    let positional = sh.vars().positional_shell_strings();
                    for (i, arg) in positional.iter().enumerate() {
                        result.push(ExpandedWord {
                            value: arg.clone(),
                            split_fields: false,
                            word_break: i > 0,
                        });
                    }
                }
                // $* in quoted context: join with first char of IFS
                else if param.name == "*" && matches!(param.op, ParamOp::Normal) && quoted_context
                {
                    let sep = sh
                        .vars()
                        .ifs()
                        .chars()
                        .next()
                        .map_or(String::new(), |c| c.to_string());
                    let value = sh.vars().positional_join_shell(&sep);
                    result.push(ExpandedWord {
                        value,
                        split_fields: false,
                        word_break: false,
                    });
                }
                // $* unquoted: each positional param is a separate field
                // (same as $@ when unquoted — POSIX specifies both produce separate fields)
                else if param.name == "*"
                    && matches!(param.op, ParamOp::Normal)
                    && !quoted_context
                {
                    let positional = sh.vars().positional_shell_strings();
                    for arg in &positional {
                        result.push(ExpandedWord {
                            value: arg.clone(),
                            split_fields: true, // subject to further IFS splitting
                            word_break: true,
                        });
                    }
                } else {
                    let frags = expand_param_to_fragments(param, sh, quoted_context)?;
                    result.extend(frags);
                }
            }
            WordPart::Tilde(user) => {
                let expanded = expand_tilde(user, sh);
                result.push(ExpandedWord {
                    value: expanded,
                    split_fields: false,
                    word_break: false,
                });
            }
            WordPart::CmdSubst(cmd) | WordPart::Backtick(cmd) => {
                let value = sh.command_subst(cmd).unwrap_or_default();
                result.push(ExpandedWord {
                    value,
                    split_fields: !quoted_context,
                    word_break: false,
                });
            }
            WordPart::Arith(inner) => {
                // First expand any variables in the arithmetic expression
                let text: String = expand_word_parts(inner, sh, true)?
                    .into_iter()
                    .map(|f| f.value)
                    .collect();
                // Then evaluate the arithmetic
                let exit_status = sh.exit_status();
                let shell_pid = sh.pid();
                let value = arith::eval_arith(&text, sh.vars_mut(), exit_status, shell_pid)
                    .map_err(|e| ShellError::Runtime {
                        msg: format!("arithmetic error: {e}"),
                        span: crate::error::Span::default(),
                    })?
                    .to_string();
                result.push(ExpandedWord {
                    value,
                    split_fields: !quoted_context,
                    word_break: false,
                });
            }
        }
    }

    Ok(result)
}

/// Expand a parameter expression to fragments, preserving quoting info.
fn expand_param_to_fragments(
    param: &ParamExpr,
    sh: &mut dyn ShellExpand,
    quoted_context: bool,
) -> crate::error::Result<Vec<ExpandedWord>> {
    let name = &param.name;

    // $* and $@ join with first char of IFS when used as a scalar
    let raw_value = if name == "*" || name == "@" {
        let sep = sh
            .vars()
            .ifs()
            .chars()
            .next()
            .map_or(String::new(), |c| c.to_string());
        Some(sh.vars().positional_join_shell(&sep))
    } else if is_special_param(name) {
        let exit_status = sh.exit_status();
        let pid = sh.pid();
        let flags = sh.shell_flags();
        let bg_pid = sh.last_bg_pid();
        sh.vars()
            .get_special(name, exit_status, pid, &flags, bg_pid)
    } else {
        sh.vars().get_shell(name)
    };

    // set -u: error on unset variables (not special params, not ops that handle unset)
    if raw_value.is_none()
        && !is_special_param(name)
        && sh.nounset()
        && matches!(
            param.op,
            ParamOp::Normal
                | ParamOp::Length
                | ParamOp::TrimSuffixSmall(_)
                | ParamOp::TrimSuffixLarge(_)
                | ParamOp::TrimPrefixSmall(_)
                | ParamOp::TrimPrefixLarge(_)
        )
    {
        return Err(ShellError::Runtime {
            msg: format!("{name}: parameter not set"),
            span: param.span,
        });
    }

    // For operators that use a word (default, assign, alternative, error),
    // expand the word to fragments to preserve quoting
    match &param.op {
        ParamOp::BadSubst => Err(ShellError::Runtime {
            msg: format!("{}: bad substitution", param.name),
            span: param.span,
        }),
        ParamOp::Normal => {
            let value = raw_value.unwrap_or_default();
            Ok(vec![ExpandedWord {
                value,
                split_fields: !quoted_context,
                word_break: false,
            }])
        }
        ParamOp::Length => {
            let value = raw_value
                .as_ref()
                .map(|v| v.len().to_string())
                .unwrap_or_else(|| "0".to_string());
            Ok(vec![ExpandedWord {
                value,
                split_fields: !quoted_context,
                word_break: false,
            }])
        }
        ParamOp::Default { colon, word } | ParamOp::Assign { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                let frags = expand_word_parts_inner(
                    word,
                    sh,
                    quoted_context,
                    true, // in_param_word: literals subject to field splitting
                )?;
                if matches!(param.op, ParamOp::Assign { .. }) {
                    let val: String = frags.iter().map(|f| f.value.clone()).collect();
                    let _ = sh.vars_mut().set(name, &val);
                    // Return the assigned value as a single fragment for further splitting
                    return Ok(vec![ExpandedWord {
                        value: val,
                        split_fields: !quoted_context,
                        word_break: false,
                    }]);
                }
                Ok(frags)
            } else {
                let value = raw_value.unwrap_or_default();
                Ok(vec![ExpandedWord {
                    value,
                    split_fields: !quoted_context,
                    word_break: false,
                }])
            }
        }
        ParamOp::Error { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                let msg = expand_param_word(word, sh)?;
                let display_msg = if msg.is_empty() {
                    format!("{name}: parameter not set")
                } else {
                    format!("{name}: {msg}")
                };
                Err(ShellError::Runtime {
                    msg: display_msg,
                    span: param.span,
                })
            } else {
                let value = raw_value.unwrap_or_default();
                Ok(vec![ExpandedWord {
                    value,
                    split_fields: !quoted_context,
                    word_break: false,
                }])
            }
        }
        ParamOp::Alternative { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                Ok(vec![])
            } else {
                let frags = expand_word_parts_inner(
                    word,
                    sh,
                    quoted_context,
                    true, // in_param_word
                )?;
                Ok(frags)
            }
        }
        _ => {
            // Trim operations — fall back to string-based expand_param
            let value = expand_param(param, sh)?;
            Ok(vec![ExpandedWord {
                value,
                split_fields: !quoted_context,
                word_break: false,
            }])
        }
    }
}

/// Expand a parameter expression (returns flat string).
fn expand_param(param: &ParamExpr, sh: &mut dyn ShellExpand) -> crate::error::Result<String> {
    let name = &param.name;

    // Get the raw value
    // $* joins with first char of IFS (or nothing if IFS="")
    let raw_value = if name == "*" || name == "@" {
        let sep = sh
            .vars()
            .ifs()
            .chars()
            .next()
            .map_or(String::new(), |c| c.to_string());
        Some(sh.vars().positional_join_shell(&sep))
    } else if is_special_param(name) {
        let exit_status = sh.exit_status();
        let pid = sh.pid();
        let flags = sh.shell_flags();
        let bg_pid = sh.last_bg_pid();
        sh.vars()
            .get_special(name, exit_status, pid, &flags, bg_pid)
    } else {
        sh.vars().get_shell(name)
    };

    // set -u: error on unset variables (not special params, not ops that handle unset)
    if raw_value.is_none()
        && !is_special_param(name)
        && sh.nounset()
        && matches!(
            param.op,
            ParamOp::Normal
                | ParamOp::Length
                | ParamOp::TrimSuffixSmall(_)
                | ParamOp::TrimSuffixLarge(_)
                | ParamOp::TrimPrefixSmall(_)
                | ParamOp::TrimPrefixLarge(_)
        )
    {
        return Err(ShellError::Runtime {
            msg: format!("{name}: parameter not set"),
            span: param.span,
        });
    }

    match &param.op {
        ParamOp::BadSubst => Err(ShellError::Runtime {
            msg: format!("{}: bad substitution", param.name),
            span: param.span,
        }),
        ParamOp::Normal => Ok(raw_value.unwrap_or_default()),
        ParamOp::Length => Ok(raw_value
            .as_ref()
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "0".to_string())),
        ParamOp::Default { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                expand_param_word(word, sh)
            } else {
                Ok(raw_value.unwrap_or_default())
            }
        }
        ParamOp::Assign { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                let default = expand_param_word(word, sh)?;
                let _ = sh.vars_mut().set(name, &default);
                Ok(default)
            } else {
                Ok(raw_value.unwrap_or_default())
            }
        }
        ParamOp::Error { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                let msg = expand_param_word(word, sh)?;
                let display_msg = if msg.is_empty() {
                    format!("{name}: parameter not set")
                } else {
                    format!("{name}: {msg}")
                };
                Err(ShellError::Runtime {
                    msg: display_msg,
                    span: param.span,
                })
            } else {
                Ok(raw_value.unwrap_or_default())
            }
        }
        ParamOp::Alternative { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                Ok(String::new())
            } else {
                expand_param_word(word, sh)
            }
        }
        ParamOp::TrimSuffixSmall(pattern) => {
            let val = raw_value.unwrap_or_default();
            let pat = expand_pattern(pattern, sh)?;
            Ok(trim_suffix(&val, &pat, false))
        }
        ParamOp::TrimSuffixLarge(pattern) => {
            let val = raw_value.unwrap_or_default();
            let pat = expand_pattern(pattern, sh)?;
            Ok(trim_suffix(&val, &pat, true))
        }
        ParamOp::TrimPrefixSmall(pattern) => {
            let val = raw_value.unwrap_or_default();
            let pat = expand_pattern(pattern, sh)?;
            Ok(trim_prefix(&val, &pat, false))
        }
        ParamOp::TrimPrefixLarge(pattern) => {
            let val = raw_value.unwrap_or_default();
            let pat = expand_pattern(pattern, sh)?;
            Ok(trim_prefix(&val, &pat, true))
        }
    }
}

/// Expand a parameter operation's word (the part after :-, :=, etc.)
fn expand_param_word(parts: &[WordPart], sh: &mut dyn ShellExpand) -> crate::error::Result<String> {
    let fragments = expand_word_parts(parts, sh, true)?;
    Ok(fragments.into_iter().map(|f| f.value).collect())
}

/// Tilde expansion: ~ → $HOME, ~user → user's home dir
fn expand_tilde(user: &crate::shell_bytes::ShellBytes, sh: &dyn ShellExpand) -> String {
    if user.is_empty() {
        sh.vars().get_shell("HOME").unwrap_or_else(|| "~".into())
    } else {
        // ~user → look up user's home directory
        // For stdlib-only, we can't easily do this without libc getpwnam.
        // Fall back to leaving it unexpanded.
        format!("~{}", user.to_shell_string())
    }
}

/// Check if a parameter name is a special parameter.
fn is_special_param(name: &str) -> bool {
    matches!(name, "@" | "*" | "#" | "?" | "-" | "$" | "!" | "0")
        || (name.len() == 1 && name.chars().next().unwrap().is_ascii_digit())
}

/// Field splitting based on IFS.
/// Uses posh-style state machine: Init → Word / IfsWs / IfsNws
fn field_split(fragments: &[ExpandedWord], ifs: &str) -> Vec<String> {
    if fragments.is_empty() {
        return Vec::new();
    }

    // Concatenate non-splitting fragments and split where marked
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut have_field = false;

    let ifs_ws: Vec<char> = ifs.chars().filter(|c| c.is_whitespace()).collect();
    let ifs_nws: Vec<char> = ifs.chars().filter(|c| !c.is_whitespace()).collect();

    for frag in fragments {
        // word_break starts a new field (used for "$@" expansion)
        if frag.word_break && have_field {
            fields.push(std::mem::take(&mut current));
            have_field = false;
        }

        if !frag.split_fields || ifs.is_empty() {
            // No splitting: append directly
            current.push_str(&frag.value);
            have_field = true;
            continue;
        }

        // Split this fragment on IFS
        let chars: Vec<char> = frag.value.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let ch = chars[i];

            if ifs_ws.contains(&ch) {
                // IFS whitespace: skip consecutive whitespace, then emit field
                if have_field {
                    fields.push(std::mem::take(&mut current));
                    have_field = false;
                }
                while i < chars.len() && ifs_ws.contains(&chars[i]) {
                    i += 1;
                }
                // Check for non-ws IFS delimiter after whitespace
                if i < chars.len() && ifs_nws.contains(&chars[i]) {
                    i += 1;
                    // Skip trailing ws after nws delimiter
                    while i < chars.len() && ifs_ws.contains(&chars[i]) {
                        i += 1;
                    }
                }
            } else if ifs_nws.contains(&ch) {
                // IFS non-whitespace: always creates a field boundary
                if have_field {
                    fields.push(std::mem::take(&mut current));
                } else {
                    fields.push(String::new()); // empty field
                }
                have_field = false;
                i += 1;
                // Skip trailing ws after nws delimiter
                while i < chars.len() && ifs_ws.contains(&chars[i]) {
                    i += 1;
                }
            } else {
                current.push(ch);
                have_field = true;
                i += 1;
            }
        }
    }

    if have_field {
        fields.push(current);
    }

    fields
}

/// Remove smallest/largest suffix matching pattern.
/// Remove backslash escapes before glob metacharacters.
/// These were preserved by the lexer for fnmatch/glob, but need stripping
/// for normal word expansion (POSIX quote removal).
pub fn remove_glob_escapes(s: &str) -> String {
    use crate::lexer::CTLESC;
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == CTLESC && i + 1 < chars.len() {
            // Strip CTLESC, keep the escaped char
            i += 1;
            result.push(chars[i]);
        } else {
            result.push(chars[i]);
        }
        i += 1;
    }
    result
}

fn trim_suffix(value: &str, pattern: &str, greedy: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    if greedy {
        // Largest suffix: try from the beginning
        for i in 0..chars.len() {
            let suffix: String = chars[i..].iter().collect();
            if glob::fnmatch(pattern, &suffix) {
                return chars[..i].iter().collect();
            }
        }
    } else {
        // Smallest suffix: try from the end
        for i in (0..chars.len()).rev() {
            let suffix: String = chars[i..].iter().collect();
            if glob::fnmatch(pattern, &suffix) {
                return chars[..i].iter().collect();
            }
        }
    }
    value.to_string()
}

/// Remove smallest/largest prefix matching pattern.
fn trim_prefix(value: &str, pattern: &str, greedy: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    if greedy {
        // Largest prefix: try from the end
        for i in (1..=chars.len()).rev() {
            let prefix: String = chars[..i].iter().collect();
            if glob::fnmatch(pattern, &prefix) {
                return chars[i..].iter().collect();
            }
        }
    } else {
        // Smallest prefix: try from the beginning
        for i in 1..=chars.len() {
            let prefix: String = chars[..i].iter().collect();
            if glob::fnmatch(pattern, &prefix) {
                return chars[i..].iter().collect();
            }
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Span;

    struct TestShell {
        vars: Variables,
        status: ExitStatus,
    }

    impl TestShell {
        fn new(vars: Variables) -> Self {
            Self {
                vars,
                status: ExitStatus::SUCCESS,
            }
        }

        fn with_status(vars: Variables, status: ExitStatus) -> Self {
            Self { vars, status }
        }
    }

    impl ShellExpand for TestShell {
        fn vars(&self) -> &Variables {
            &self.vars
        }
        fn vars_mut(&mut self) -> &mut Variables {
            &mut self.vars
        }
        fn exit_status(&self) -> ExitStatus {
            self.status
        }
        fn pid(&self) -> u32 {
            1
        }
        fn cwd(&self) -> &Path {
            Path::new("/")
        }
        fn shell_flags(&self) -> String {
            String::new()
        }
        fn last_bg_pid(&self) -> Option<u32> {
            None
        }
        fn nounset(&self) -> bool {
            false
        }
        fn noglob(&self) -> bool {
            false
        }
        fn command_subst(&mut self, _cmd: &Command) -> crate::error::Result<String> {
            Ok(String::new())
        }
    }

    fn make_vars() -> Variables {
        let mut vars = Variables::new();
        vars.set("FOO", "hello").unwrap();
        vars.set("EMPTY", "").unwrap();
        vars.set("PATH_VAR", "/usr/local/bin:/usr/bin:/bin")
            .unwrap();
        vars.set("FILE", "archive.tar.gz").unwrap();
        vars
    }

    fn make_word(parts: Vec<WordPart>) -> Word {
        Word {
            parts,
            span: Span::default(),
        }
    }

    #[test]
    fn expand_literal() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Literal("hello".into())]);
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["hello"]
        );
    }

    #[test]
    fn expand_simple_variable() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FOO".into(),
            op: ParamOp::Normal,
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["hello"]
        );
    }

    #[test]
    fn expand_default_unset() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "UNSET".into(),
            op: ParamOp::Default {
                colon: false,
                word: vec![WordPart::Literal("fallback".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["fallback"]
        );
    }

    #[test]
    fn expand_default_colon_empty() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "EMPTY".into(),
            op: ParamOp::Default {
                colon: true,
                word: vec![WordPart::Literal("fallback".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["fallback"]
        );
    }

    #[test]
    fn expand_default_set() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FOO".into(),
            op: ParamOp::Default {
                colon: false,
                word: vec![WordPart::Literal("fallback".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["hello"]
        );
    }

    #[test]
    fn expand_assign_default() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "NEW_VAR".into(),
            op: ParamOp::Assign {
                colon: false,
                word: vec![WordPart::Literal("assigned".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["assigned"]
        );
        assert_eq!(sh.vars.get("NEW_VAR"), Some("assigned"));
    }

    #[test]
    fn expand_alternative_set() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FOO".into(),
            op: ParamOp::Alternative {
                colon: false,
                word: vec![WordPart::Literal("alt".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(expand_word_to_fields(&word, &mut sh).unwrap(), vec!["alt"]);
    }

    #[test]
    fn expand_alternative_unset() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "UNSET".into(),
            op: ParamOp::Alternative {
                colon: false,
                word: vec![WordPart::Literal("alt".into())],
            },
            span: Span::default(),
        })]);
        let result = expand_word_to_fields(&word, &mut sh).unwrap();
        assert!(result.is_empty() || result == vec![""]);
    }

    #[test]
    fn expand_length() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FOO".into(),
            op: ParamOp::Length,
            span: Span::default(),
        })]);
        assert_eq!(expand_word_to_fields(&word, &mut sh).unwrap(), vec!["5"]);
    }

    #[test]
    fn expand_trim_suffix_small() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FILE".into(),
            op: ParamOp::TrimSuffixSmall(vec![WordPart::Literal(".*".into())]),
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["archive.tar"]
        );
    }

    #[test]
    fn expand_trim_suffix_large() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FILE".into(),
            op: ParamOp::TrimSuffixLarge(vec![WordPart::Literal(".*".into())]),
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["archive"]
        );
    }

    #[test]
    fn expand_trim_prefix_small() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "PATH_VAR".into(),
            op: ParamOp::TrimPrefixSmall(vec![WordPart::Literal("*/".into())]),
            span: Span::default(),
        })]);
        // Smallest prefix matching */: removes "/" (since */ matches "/")
        let result = expand_word_to_fields(&word, &mut sh).unwrap();
        assert_eq!(result, vec!["usr/local/bin:/usr/bin:/bin"]);
    }

    #[test]
    fn expand_trim_prefix_large() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "PATH_VAR".into(),
            op: ParamOp::TrimPrefixLarge(vec![WordPart::Literal("*/".into())]),
            span: Span::default(),
        })]);
        // Largest prefix matching */: removes everything up to last /
        let result = expand_word_to_fields(&word, &mut sh).unwrap();
        assert_eq!(result, vec!["bin"]);
    }

    #[test]
    fn expand_tilde_home() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![
            WordPart::Tilde(String::new().into()),
            WordPart::Literal("/bin".into()),
        ]);
        let result = expand_word_to_string(&word, &mut sh).unwrap();
        let home = std::env::var_os("HOME")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(result, format!("{home}/bin"));
    }

    #[test]
    fn expand_single_quoted() {
        let mut sh = TestShell::new(make_vars());
        let word = make_word(vec![WordPart::SingleQuoted("$FOO".into())]);
        assert_eq!(expand_word_to_fields(&word, &mut sh).unwrap(), vec!["$FOO"]);
    }

    #[test]
    fn expand_double_quoted_no_split() {
        let mut vars = make_vars();
        vars.set("X", "a  b  c").unwrap();
        let mut sh = TestShell::new(vars);
        let word = make_word(vec![WordPart::DoubleQuoted(vec![WordPart::Param(
            ParamExpr {
                name: "X".into(),
                op: ParamOp::Normal,
                span: Span::default(),
            },
        )])]);
        // Inside double quotes, no field splitting
        assert_eq!(
            expand_word_to_fields(&word, &mut sh).unwrap(),
            vec!["a  b  c"]
        );
    }

    #[test]
    fn field_split_basic() {
        let fragments = vec![ExpandedWord {
            value: "a b c".into(),
            split_fields: true,
            word_break: false,
        }];
        let result = field_split(&fragments, " \t\n");
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn field_split_custom_ifs() {
        let fragments = vec![ExpandedWord {
            value: "a:b:c".into(),
            split_fields: true,
            word_break: false,
        }];
        let result = field_split(&fragments, ":");
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn field_split_mixed() {
        let fragments = vec![
            ExpandedWord {
                value: "prefix-".into(),
                split_fields: false,
                word_break: false,
            },
            ExpandedWord {
                value: "a b".into(),
                split_fields: true,
                word_break: false,
            },
        ];
        let result = field_split(&fragments, " \t\n");
        assert_eq!(result, vec!["prefix-a", "b"]);
    }

    #[test]
    fn expand_exit_status() {
        let mut sh = TestShell::with_status(make_vars(), ExitStatus::from(42));
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "?".into(),
            op: ParamOp::Normal,
            span: Span::default(),
        })]);
        assert_eq!(expand_word_to_fields(&word, &mut sh).unwrap(), vec!["42"]);
    }
}
