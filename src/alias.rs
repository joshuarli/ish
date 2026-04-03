use std::collections::HashMap;

/// Maps alias names to their expansion (command + args).
pub struct AliasMap {
    map: HashMap<String, Vec<String>>,
}

impl Default for AliasMap {
    fn default() -> Self {
        Self::new()
    }
}

impl AliasMap {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn set(&mut self, name: String, expansion: Vec<String>) {
        self.map.insert(name, expansion);
    }

    pub fn get(&self, name: &str) -> Option<&[String]> {
        self.map.get(name).map(|v| v.as_slice())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.map.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// Expand aliases in the first word of a command line.
    /// Returns the expanded line, or the original if no alias matched.
    /// Non-recursive: only the first word is checked.
    pub fn expand_line<'a>(&self, line: &'a str) -> std::borrow::Cow<'a, str> {
        let trimmed = line.trim_start();
        let first_word = match trimmed.split_whitespace().next() {
            Some(w) => w,
            None => return std::borrow::Cow::Borrowed(line),
        };
        if let Some(expansion) = self.get(first_word) {
            let leading_ws = &line[..line.len() - trimmed.len()];
            let rest = &trimmed[first_word.len()..];
            let expanded = shell_quote_join(expansion);
            std::borrow::Cow::Owned(format!("{leading_ws}{expanded}{rest}"))
        } else {
            std::borrow::Cow::Borrowed(line)
        }
    }
}

/// Join words, quoting any that contain whitespace so the result
/// re-parses into the same tokens.
fn shell_quote_join(words: &[String]) -> String {
    let mut result = String::new();
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        if word.contains([' ', '\t', '\n']) {
            result.push('"');
            for c in word.chars() {
                if c == '"' || c == '\\' {
                    result.push('\\');
                }
                result.push(c);
            }
            result.push('"');
        } else {
            result.push_str(word);
        }
    }
    result
}
