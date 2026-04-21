use std::collections::HashMap;

/// Maps alias names to their expansion as raw shell words.
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
            let expanded = expansion.join(" ");
            std::borrow::Cow::Owned(format!("{leading_ws}{expanded}{rest}"))
        } else {
            std::borrow::Cow::Borrowed(line)
        }
    }
}

/// Lex a shell fragment into its raw word tokens, preserving quoting and
/// substitutions in the returned strings.
pub fn lex_words(source: &str) -> Vec<String> {
    let mut lex = epsh::lexer::Lexer::new(source);
    lex.recognize_reserved = false;

    let mut words = Vec::new();
    while let Ok((tok, span)) = lex.next_token() {
        let next_offset = match lex.next_token() {
            Ok((next, next_span)) => {
                lex.push_back(next.clone(), next_span);
                next_span.offset
            }
            Err(_) => source.chars().count(),
        };

        match tok {
            epsh::lexer::Token::Word(_, _) => {
                let start = char_to_byte_offset(source, span.offset);
                let end = char_to_byte_offset(source, next_offset);
                words.push(source[start..end].trim_end().to_string());
            }
            epsh::lexer::Token::Eof => break,
            _ => break,
        }
    }

    words
}

fn char_to_byte_offset(source: &str, char_offset: usize) -> usize {
    source
        .char_indices()
        .nth(char_offset)
        .map(|(idx, _)| idx)
        .unwrap_or(source.len())
}
