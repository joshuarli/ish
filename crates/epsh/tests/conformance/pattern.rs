use regex_lite::{Regex, RegexBuilder};

#[derive(Debug)]
pub(super) enum OutputAssertion {
    Exact(Vec<u8>),
    Pattern(BytePattern),
}

#[derive(Debug)]
pub(super) enum BytePattern {
    Regex { regex: Regex, negate: bool },
    NonzeroRealTime,
}

impl OutputAssertion {
    pub fn exact_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Exact(bytes) => Some(bytes),
            Self::Pattern(_) => None,
        }
    }

    pub fn pattern(source: &[u8]) -> Result<Self, String> {
        let source = source.strip_suffix(b"\n").unwrap_or(source);
        let source = trim_ascii(source);
        let (negate, source) = match source.strip_prefix(b"!") {
            Some(source) => (true, trim_ascii(source)),
            None => (false, source),
        };
        let (expression, multiline) = delimited_expression(source)?;
        if expression == b"^real +(?![0.]*$)[0-9]+(?:\\.[0-9]+)?$" && multiline && !negate {
            return Ok(Self::Pattern(BytePattern::NonzeroRealTime));
        }
        let expression = std::str::from_utf8(expression)
            .map_err(|_| "pattern is not UTF-8".to_owned())?;
        let mut builder = RegexBuilder::new(expression);
        builder.multi_line(multiline);
        let regex = builder.build().map_err(|error| error.to_string())?;
        Ok(Self::Pattern(BytePattern::Regex { regex, negate }))
    }

    pub fn matches(&self, output: &[u8]) -> Result<bool, String> {
        match self {
            Self::Exact(expected) => Ok(output == expected),
            Self::Pattern(pattern) => pattern.matches(output),
        }
    }
}

impl BytePattern {
    fn matches(&self, output: &[u8]) -> Result<bool, String> {
        let output = std::str::from_utf8(output)
            .map_err(|_| "regex assertion cannot inspect non-UTF-8 output".to_owned())?;
        match self {
            Self::Regex { regex, negate } => Ok(regex.is_match(output) != *negate),
            Self::NonzeroRealTime => {
                let regex = Regex::new(r"(?m)^real +[0-9]+(?:\.[0-9]+)?$")
                    .map_err(|error| error.to_string())?;
                Ok(regex.find_iter(output).any(|matched| {
                    matched.as_str().bytes().any(|byte| (b'1'..=b'9').contains(&byte))
                }))
            }
        }
    }
}

fn delimited_expression(source: &[u8]) -> Result<(&[u8], bool), String> {
    if source.first() != Some(&b'/') {
        return Err("expected /pattern/ with an optional m modifier".to_owned());
    }
    let end = source[1..]
        .iter()
        .enumerate()
        .filter(|(index, byte)| **byte == b'/' && !is_escaped(&source[1..], *index))
        .map(|(index, _)| index + 1)
        .next_back()
        .ok_or_else(|| "pattern delimiter is not closed".to_owned())?;
    let expression = &source[1..end];
    let modifier = &source[end + 1..];
    let multiline = match modifier {
        b"" => false,
        b"m" => true,
        _ => return Err("unsupported pattern modifier".to_owned()),
    };
    Ok((expression, multiline))
}

fn is_escaped(source: &[u8], index: usize) -> bool {
    let mut slashes = 0;
    let mut cursor = index;
    while cursor > 0 && source[cursor - 1] == b'\\' {
        slashes += 1;
        cursor -= 1;
    }
    slashes % 2 == 1
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::OutputAssertion;

    #[test]
    fn adapts_the_legacy_elapsed_time_lookahead_equivalently() {
        let assertion = OutputAssertion::pattern(
            b"/^real +(?![0.]*$)[0-9]+(?:\\.[0-9]+)?$/m\n",
        )
        .unwrap();
        for (output, expected) in [
            (b"real 0\n".as_slice(), false),
            (b"real 0.000\n".as_slice(), false),
            (b"real 0.001\n".as_slice(), true),
            (b"real 1\n".as_slice(), true),
            (b"real 1.000\n".as_slice(), true),
            (b"user 1.0\n".as_slice(), false),
            (b"real 000.000\nuser 1.0\n".as_slice(), false),
        ] {
            assert_eq!(assertion.matches(output).unwrap(), expected, "{output:?}");
        }
    }

    #[test]
    fn rejects_unadapted_perl_regex_syntax_and_invalid_utf8_output() {
        assert!(OutputAssertion::pattern(b"/(?=foo)foo/\n").is_err());
        let assertion = OutputAssertion::pattern(b"/foo/\n").unwrap();
        assert!(assertion.matches(b"foo\xff").is_err());
    }
}
