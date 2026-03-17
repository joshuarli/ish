use crate::error::Error;

/// Internal literal marker: chars prefixed with this byte are not expanded.
pub const LITERAL: char = '\x00';

#[derive(Debug, Clone)]
pub struct CommandLine {
    pub segments: Vec<(Pipeline, Option<Connector>)>,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub commands: Vec<PipedCommand>,
}

#[derive(Debug, Clone)]
pub struct PipedCommand {
    pub cmd: Command,
    pub pipe_stderr: bool,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub argv: Vec<String>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone)]
pub struct Redirect {
    pub kind: RedirectKind,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    Out,    // >
    Append, // >>
    In,     // <
    Err,    // 2>
    All,    // &>
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connector {
    And,
    Or,
    Semi,
}

// -- Tokenizer --

#[derive(Debug)]
enum Token {
    Word(String),
    Pipe,
    PipeStderr,
    And,
    Or,
    Semi,
    Redirect(RedirectKind),
}

fn tokenize(input: &str) -> Result<Vec<Token>, Error> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut tokens = Vec::new();

    while i < chars.len() {
        // Skip whitespace
        if chars[i] == ' ' || chars[i] == '\t' || chars[i] == '\n' {
            i += 1;
            continue;
        }
        // Comments
        if chars[i] == '#' {
            break;
        }
        match chars[i] {
            '|' => {
                if peek(&chars, i + 1) == Some('|') {
                    tokens.push(Token::Or);
                    i += 2;
                } else {
                    tokens.push(Token::Pipe);
                    i += 1;
                }
            }
            '&' => {
                if peek(&chars, i + 1) == Some('|') {
                    tokens.push(Token::PipeStderr);
                    i += 2;
                } else if peek(&chars, i + 1) == Some('>') {
                    tokens.push(Token::Redirect(RedirectKind::All));
                    i += 2;
                } else if peek(&chars, i + 1) == Some('&') {
                    tokens.push(Token::And);
                    i += 2;
                } else {
                    return Err(Error::msg("background (&) not supported"));
                }
            }
            '>' => {
                if peek(&chars, i + 1) == Some('>') {
                    tokens.push(Token::Redirect(RedirectKind::Append));
                    i += 2;
                } else {
                    tokens.push(Token::Redirect(RedirectKind::Out));
                    i += 1;
                }
            }
            '<' => {
                tokens.push(Token::Redirect(RedirectKind::In));
                i += 1;
            }
            ';' => {
                tokens.push(Token::Semi);
                i += 1;
            }
            '2' if peek(&chars, i + 1) == Some('>') => {
                tokens.push(Token::Redirect(RedirectKind::Err));
                i += 2;
            }
            _ => {
                let (word, new_i) = scan_word(&chars, i)?;
                tokens.push(Token::Word(word));
                i = new_i;
            }
        }
    }

    Ok(tokens)
}

fn peek(chars: &[char], idx: usize) -> Option<char> {
    chars.get(idx).copied()
}

fn is_meta(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '|' | '&' | '>' | '<' | ';' | '#')
}

/// Scan a word token, handling quoting. Returns (word_string, new_index).
/// Uses LITERAL (\x00) prefix to mark chars that should not be expanded.
fn scan_word(chars: &[char], start: usize) -> Result<(String, usize), Error> {
    let mut word = String::new();
    let mut i = start;

    while i < chars.len() {
        match chars[i] {
            c if is_meta(c) => break,
            // Handle 2> specially only at start of a word-scan (handled in tokenize)
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    let c = chars[i];
                    if is_expandable(c) {
                        word.push(LITERAL);
                    }
                    word.push(c);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(Error::msg("unclosed single quote"));
                }
                i += 1;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        let next = chars[i + 1];
                        if matches!(next, '$' | '"' | '\\' | '`') {
                            word.push(LITERAL);
                            word.push(next);
                            i += 2;
                        } else {
                            word.push(LITERAL);
                            word.push('\\');
                            i += 1;
                        }
                    } else {
                        let c = chars[i];
                        // In double quotes: glob/tilde chars are literal, $ and ` are expandable
                        if is_glob_or_tilde(c) {
                            word.push(LITERAL);
                        }
                        word.push(c);
                        i += 1;
                    }
                }
                if i >= chars.len() {
                    return Err(Error::msg("unclosed double quote"));
                }
                i += 1;
            }
            '\\' if i + 1 < chars.len() => {
                i += 1;
                word.push(LITERAL);
                word.push(chars[i]);
                i += 1;
            }
            c => {
                word.push(c);
                i += 1;
            }
        }
    }

    Ok((word, i))
}

fn is_expandable(c: char) -> bool {
    matches!(c, '$' | '`' | '*' | '?' | '[' | '~')
}

fn is_glob_or_tilde(c: char) -> bool {
    matches!(c, '*' | '?' | '[' | '~')
}

// -- Parser --

pub fn parse(input: &str) -> Result<CommandLine, Error> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(Error::msg("empty command"));
    }
    parse_tokens(&tokens)
}

fn parse_tokens(tokens: &[Token]) -> Result<CommandLine, Error> {
    let mut segments = Vec::new();
    let mut i = 0;

    loop {
        let (pipeline, next_i) = parse_pipeline(tokens, i)?;
        i = next_i;

        let connector = if i < tokens.len() {
            match &tokens[i] {
                Token::And => {
                    i += 1;
                    Some(Connector::And)
                }
                Token::Or => {
                    i += 1;
                    Some(Connector::Or)
                }
                Token::Semi => {
                    i += 1;
                    Some(Connector::Semi)
                }
                other => {
                    return Err(Error::msg(format!("unexpected token: {other:?}")));
                }
            }
        } else {
            None
        };

        segments.push((pipeline, connector));

        if connector.is_none() || i >= tokens.len() {
            break;
        }
    }

    Ok(CommandLine { segments })
}

fn parse_pipeline(tokens: &[Token], start: usize) -> Result<(Pipeline, usize), Error> {
    let mut commands = Vec::new();
    let mut i = start;

    loop {
        let (cmd, next_i) = parse_command(tokens, i)?;
        i = next_i;

        let pipe_stderr = if i < tokens.len() {
            match &tokens[i] {
                Token::Pipe => {
                    i += 1;
                    commands.push(PipedCommand {
                        cmd,
                        pipe_stderr: false,
                    });
                    continue;
                }
                Token::PipeStderr => {
                    i += 1;
                    commands.push(PipedCommand {
                        cmd,
                        pipe_stderr: true,
                    });
                    continue;
                }
                _ => false,
            }
        } else {
            false
        };

        commands.push(PipedCommand { cmd, pipe_stderr });
        break;
    }

    if commands.is_empty() {
        return Err(Error::msg("expected command"));
    }

    Ok((Pipeline { commands }, i))
}

fn parse_command(tokens: &[Token], start: usize) -> Result<(Command, usize), Error> {
    let mut argv = Vec::new();
    let mut redirects = Vec::new();
    let mut i = start;

    while i < tokens.len() {
        match &tokens[i] {
            Token::Word(w) => {
                argv.push(w.clone());
                i += 1;
            }
            Token::Redirect(kind) => {
                i += 1;
                let target = match tokens.get(i) {
                    Some(Token::Word(w)) => {
                        i += 1;
                        w.clone()
                    }
                    _ => return Err(Error::msg("expected filename after redirect")),
                };
                redirects.push(Redirect {
                    kind: *kind,
                    target,
                });
            }
            // Pipeline/connector tokens end this command
            _ => break,
        }
    }

    if argv.is_empty() {
        return Err(Error::msg("expected command name"));
    }

    Ok((Command { argv, redirects }, i))
}

/// Check if input needs a continuation line.
pub fn needs_continuation(input: &str) -> bool {
    let trimmed = input.trim_end();
    if trimmed.is_empty() {
        return false;
    }

    // Check unclosed quotes
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    for c in trimmed.chars() {
        if escape {
            escape = false;
            continue;
        }
        match c {
            '\\' if !in_single => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }
    }
    if in_single || in_double {
        return true;
    }

    // Check for trailing operator
    trimmed.ends_with('|') || trimmed.ends_with("&&") || trimmed.ends_with("||")
}

/// Remove the LITERAL markers, producing a clean string.
pub fn unescape(s: &str) -> String {
    s.replace(LITERAL, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command() {
        let cmd = parse("ls -la").unwrap();
        assert_eq!(cmd.segments.len(), 1);
        assert_eq!(cmd.segments[0].0.commands[0].cmd.argv, ["ls", "-la"]);
    }

    #[test]
    fn pipeline() {
        let cmd = parse("ls | grep foo | wc -l").unwrap();
        assert_eq!(cmd.segments[0].0.commands.len(), 3);
    }

    #[test]
    fn redirects() {
        let cmd = parse("echo hi > out.txt").unwrap();
        let c = &cmd.segments[0].0.commands[0].cmd;
        assert_eq!(c.argv, ["echo", "hi"]);
        assert_eq!(c.redirects.len(), 1);
        assert_eq!(c.redirects[0].kind, RedirectKind::Out);
    }

    #[test]
    fn chaining() {
        let cmd = parse("a && b || c ; d").unwrap();
        assert_eq!(cmd.segments.len(), 4);
        assert_eq!(cmd.segments[0].1, Some(Connector::And));
        assert_eq!(cmd.segments[1].1, Some(Connector::Or));
        assert_eq!(cmd.segments[2].1, Some(Connector::Semi));
        assert_eq!(cmd.segments[3].1, None);
    }

    #[test]
    fn single_quotes_are_literal() {
        let cmd = parse("echo '$HOME'").unwrap();
        let word = &cmd.segments[0].0.commands[0].cmd.argv[1];
        // The $ should be LITERAL-prefixed
        assert!(word.starts_with(LITERAL));
    }

    #[test]
    fn continuation_detection() {
        assert!(needs_continuation("ls |"));
        assert!(needs_continuation("a &&"));
        assert!(needs_continuation("a ||"));
        assert!(needs_continuation("echo 'unclosed"));
        assert!(!needs_continuation("ls -la"));
        assert!(!needs_continuation("a && b"));
    }
}
