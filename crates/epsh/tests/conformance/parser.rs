use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use super::pattern::OutputAssertion;

#[derive(Debug)]
pub struct Case {
    pub name: String,
    pub line: usize,
    pub description: Vec<u8>,
    pub arguments: Vec<Vec<u8>>,
    pub script: Option<Vec<u8>>,
    pub stdin: Option<Vec<u8>>,
    pub environment: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    pub files: Vec<FileSetup>,
    pub time_limit_secs: u64,
    pub expected_exit: ExpectedExit,
    pub need_pass: bool,
    pub category: Option<Vec<u8>>,
    pub info_pre: Option<Vec<u8>>,
    pub stdout: Option<OutputAssertion>,
    pub stderr: Option<OutputAssertion>,
}

#[derive(Debug)]
pub struct FileSetup {
    pub kind: FileKind,
    pub mode: u32,
    pub path: Vec<u8>,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedExit {
    Code(u8),
    IsZero,
    IsNonzero,
}

#[derive(Clone, Copy)]
enum FieldMode {
    Single,
    Multiline,
    Delimited,
    Parameterized,
}

struct FieldSpec {
    name: &'static str,
    mode: FieldMode,
    repeated: bool,
}

const FIELD_SPECS: &[FieldSpec] = &[
    FieldSpec { name: "name", mode: FieldMode::Single, repeated: false },
    FieldSpec { name: "description", mode: FieldMode::Multiline, repeated: false },
    FieldSpec { name: "arguments", mode: FieldMode::Delimited, repeated: false },
    FieldSpec { name: "script", mode: FieldMode::Multiline, repeated: false },
    FieldSpec { name: "stdin", mode: FieldMode::Multiline, repeated: false },
    FieldSpec { name: "env-setup", mode: FieldMode::Delimited, repeated: false },
    FieldSpec { name: "file-setup", mode: FieldMode::Parameterized, repeated: true },
    FieldSpec { name: "time-limit", mode: FieldMode::Single, repeated: false },
    FieldSpec { name: "expected-exit", mode: FieldMode::Single, repeated: false },
    FieldSpec { name: "expected-stdout", mode: FieldMode::Multiline, repeated: false },
    FieldSpec { name: "expected-stderr", mode: FieldMode::Multiline, repeated: false },
    FieldSpec { name: "expected-stderr-pattern", mode: FieldMode::Multiline, repeated: false },
    FieldSpec { name: "category", mode: FieldMode::Multiline, repeated: false },
    FieldSpec { name: "need-pass", mode: FieldMode::Single, repeated: false },
    FieldSpec { name: "info-pre", mode: FieldMode::Single, repeated: false },
];

pub fn parse_corpus(input: &[u8]) -> Result<Vec<Case>, String> {
    let mut lines = input.split_inclusive(|byte| *byte == b'\n').enumerate().peekable();
    let mut cases = Vec::new();
    let mut names = HashSet::new();
    let mut current = HashMap::<&'static str, Vec<Vec<u8>>>::new();
    let mut case_line = 0;
    let mut ended = true;

    while let Some((zero_line, raw_line)) = lines.next() {
        let line_number = zero_line + 1;
        let line = strip_line_feed(raw_line);
        if trim_ascii(line).is_empty() || line.iter().take_while(|byte| **byte == b' ').count() == line.len() {
            continue;
        }
        if line.iter().take_while(|byte| **byte == b' ').count() < line.len()
            && line[line.iter().take_while(|byte| **byte == b' ').count()] == b'#'
        {
            continue;
        }
        if trim_ascii(line) == b"---" {
            if current.is_empty() {
                return Err(format!("line {line_number}: empty conformance record"));
            }
            let case = build_case(current, case_line)?;
            if !names.insert(case.name.clone()) {
                return Err(format!("line {}: duplicate case ID {}", case.line, case.name));
            }
            cases.push(case);
            current = HashMap::new();
            case_line = 0;
            ended = true;
            continue;
        }

        let colon = line.iter().position(|byte| *byte == b':')
            .ok_or_else(|| format!("line {line_number}: expected field:value"))?;
        let field_bytes = &line[..colon];
        if field_bytes.is_empty()
            || !field_bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_')
        {
            return Err(format!("line {line_number}: invalid field name"));
        }
        let field_name = std::str::from_utf8(field_bytes)
            .map_err(|_| format!("line {line_number}: non-ASCII field name"))?;
        let spec = FIELD_SPECS.iter().find(|spec| spec.name == field_name)
            .ok_or_else(|| format!("line {line_number}: unknown field {field_name}"))?;
        if case_line == 0 {
            case_line = line_number;
        }
        ended = false;

        let initial = trim_ascii(&line[colon + 1..]);
        let value = match spec.mode {
            FieldMode::Single | FieldMode::Delimited => initial.to_vec(),
            FieldMode::Multiline => {
                if initial.is_empty() || initial == b"!" {
                    let strip_final_newline = initial == b"!";
                    let mut value = Vec::new();
                    consume_continuations(&mut lines, &mut value);
                    if strip_final_newline && value.last() == Some(&b'\n') {
                        value.pop();
                    }
                    value
                } else {
                    let mut value = initial.to_vec();
                    value.push(b'\n');
                    value
                }
            }
            FieldMode::Parameterized => {
                let (mut value, strip_final_newline) = if let Some(rest) = initial.strip_prefix(b"!") {
                    (rest.to_vec(), true)
                } else {
                    (initial.to_vec(), false)
                };
                consume_continuations(&mut lines, &mut value);
                if strip_final_newline && !value.is_empty() {
                    value.pop();
                }
                value
            }
        };

        let values = current.entry(spec.name).or_default();
        if !spec.repeated && !values.is_empty() {
            return Err(format!("line {line_number}: duplicate field {field_name}"));
        }
        values.push(value);
    }

    if !ended || !current.is_empty() {
        return Err(format!("line {case_line}: record is missing --- terminator"));
    }
    Ok(cases)
}

fn consume_continuations<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'a [u8])>>,
    output: &mut Vec<u8>,
) {
    while let Some((_, raw_line)) = lines.peek() {
        let line = strip_line_feed(raw_line);
        if line.first() != Some(&b'\t') {
            break;
        }
        let (_, raw_line) = lines.next().expect("peeked continuation line");
        output.extend_from_slice(&raw_line[1..]);
    }
}

fn strip_line_feed(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
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

fn build_case(mut fields: HashMap<&'static str, Vec<Vec<u8>>>, line: usize) -> Result<Case, String> {
    let name = take_one(&mut fields, "name")
        .ok_or_else(|| format!("line {line}: missing required field name"))?;
    let name = std::str::from_utf8(&name)
        .map_err(|_| format!("line {line}: case ID is not UTF-8"))?;
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
        return Err(format!("line {line}: invalid case ID {name:?}"));
    }

    let description = take_one(&mut fields, "description").unwrap_or_default();
    let arguments = decode_delimited(take_one(&mut fields, "arguments"), "arguments", line)?;
    let script = take_one(&mut fields, "script");
    let stdin = take_one(&mut fields, "stdin");
    let environment = decode_environment(take_one(&mut fields, "env-setup"), line)?;
    let files = fields.remove("file-setup").unwrap_or_default()
        .into_iter()
        .map(|value| parse_file_setup(&value, line))
        .collect::<Result<Vec<_>, _>>()?;
    let time_limit_secs = match take_one(&mut fields, "time-limit") {
        Some(value) => parse_positive_integer(&value, "time-limit", line)?,
        None => 10,
    };
    let expected_exit = parse_expected_exit(take_one(&mut fields, "expected-exit"), line)?;
    let expected_stdout = take_one(&mut fields, "expected-stdout");
    let expected_stderr = take_one(&mut fields, "expected-stderr");
    let stderr_pattern = take_one(&mut fields, "expected-stderr-pattern");
    if expected_stderr.is_some() && stderr_pattern.is_some() {
        return Err(format!("line {line}: both stderr assertions are present"));
    }
    let stdout = expected_stdout.map(OutputAssertion::Exact);
    let stderr = make_assertion(expected_stderr, stderr_pattern, line)?;
    let category = take_one(&mut fields, "category");
    let need_pass = parse_yes_no(take_one(&mut fields, "need-pass"), true, "need-pass", line)?;
    let info_pre = take_one(&mut fields, "info-pre");

    if let Some((field, _)) = fields.into_iter().next() {
        return Err(format!("line {line}: unconsumed field {field}"));
    }

    Ok(Case {
        name: name.to_owned(),
        line,
        description,
        arguments,
        script,
        stdin,
        environment,
        files,
        time_limit_secs,
        expected_exit,
        need_pass,
        category,
        info_pre,
        stdout,
        stderr,
    })
}

fn take_one(fields: &mut HashMap<&'static str, Vec<Vec<u8>>>, name: &'static str) -> Option<Vec<u8>> {
    fields.remove(name).and_then(|mut values| values.pop())
}

fn decode_delimited(value: Option<Vec<u8>>, field: &str, line: usize) -> Result<Vec<Vec<u8>>, String> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let Some((&delimiter, body)) = value.split_first() else {
        return Err(format!("line {line}: empty delimiter in {field}"));
    };
    if body.last() != Some(&delimiter) {
        return Err(format!("line {line}: {field} delimiter is not closed"));
    }
    Ok(body[..body.len() - 1]
        .split(|byte| *byte == delimiter)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn decode_environment(value: Option<Vec<u8>>, line: usize) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>, String> {
    decode_delimited(value, "env-setup", line)?
        .into_iter()
        .map(|entry| {
            let equal = entry.iter().position(|byte| *byte == b'=');
            match equal {
                Some(0) => Ok(None),
                Some(index) => {
                    let key = entry[..index].to_vec();
                    validate_environment_key(&key, line)?;
                    Ok(Some((key, Some(entry[index + 1..].to_vec()))))
                }
                None => {
                    validate_environment_key(&entry, line)?;
                    Ok(Some((entry, None)))
                }
            }
        })
        .filter_map(|result| match result {
            Ok(Some(value)) => Some(Ok(value)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn validate_environment_key(key: &[u8], line: usize) -> Result<(), String> {
    if key.is_empty() || key.contains(&0) || key.contains(&b'=') {
        return Err(format!("line {line}: invalid environment variable name"));
    }
    Ok(())
}

fn parse_file_setup(value: &[u8], line: usize) -> Result<FileSetup, String> {
    let (kind, remainder) = take_token(value)
        .ok_or_else(|| format!("line {line}: file-setup is missing a type"))?;
    let (mode, remainder) = take_token(remainder)
        .ok_or_else(|| format!("line {line}: file-setup is missing a mode"))?;
    let (path, contents) = parse_quoted_parameter(remainder)
        .ok_or_else(|| format!("line {line}: malformed file-setup path"))?;
    let kind = match kind {
        b"file" => FileKind::File,
        b"dir" => FileKind::Directory,
        b"symlink" => FileKind::Symlink,
        _ => return Err(format!("line {line}: unknown file-setup type")),
    };
    let mode = parse_octal(mode).ok_or_else(|| format!("line {line}: invalid file-setup mode"))?;
    validate_relative_path(path).map_err(|_| format!("line {line}: unsafe file-setup path"))?;
    if kind == FileKind::Symlink {
        validate_symlink_target(path, contents)
            .map_err(|_| format!("line {line}: file-setup symlink target escapes its fixture"))?;
    }
    Ok(FileSetup { kind, mode, path: path.to_vec(), contents: contents.to_vec() })
}

fn take_token(value: &[u8]) -> Option<(&[u8], &[u8])> {
    let start = value.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let value = &value[start..];
    let end = value.iter().position(u8::is_ascii_whitespace).unwrap_or(value.len());
    Some((&value[..end], &value[end..]))
}

fn parse_quoted_parameter(value: &[u8]) -> Option<(&[u8], &[u8])> {
    let value = value.iter().position(|byte| !byte.is_ascii_whitespace())
        .map(|index| &value[index..])?;
    let delimiter = *value.first()?;
    let end = value[1..].iter().position(|byte| *byte == delimiter)? + 1;
    let path = &value[1..end];
    let contents = &value[end + 1..];
    Some((path, contents))
}

fn parse_octal(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
        return None;
    }
    value.iter().try_fold(0u32, |mode, digit| {
        mode.checked_mul(8)?.checked_add(u32::from(*digit - b'0'))
    })
}

fn validate_relative_path(bytes: &[u8]) -> Result<(), ()> {
    let path = Path::new(std::ffi::OsStr::from_bytes(bytes));
    if bytes.contains(&0) || path.as_os_str().is_empty() || path.is_absolute() {
        return Err(());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return Err(()),
        }
    }
    Ok(())
}

fn validate_symlink_target(link: &[u8], target: &[u8]) -> Result<(), ()> {
    let link = Path::new(std::ffi::OsStr::from_bytes(link));
    let target = Path::new(std::ffi::OsStr::from_bytes(target));
    if target_bytes_contain_nul(target.as_os_str().as_bytes()) || target.is_absolute() {
        return Err(());
    }
    let mut depth = link.components().count().saturating_sub(1);
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return Err(()),
        }
    }
    Ok(())
}

fn target_bytes_contain_nul(value: &[u8]) -> bool {
    value.contains(&0)
}

fn parse_positive_integer(value: &[u8], field: &str, line: usize) -> Result<u64, String> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(format!("line {line}: invalid {field}"));
    }
    let number = std::str::from_utf8(value).unwrap()
        .parse::<u64>()
        .map_err(|_| format!("line {line}: invalid {field}"))?;
    if number == 0 {
        return Err(format!("line {line}: {field} must be positive"));
    }
    Ok(number)
}

fn parse_yes_no(value: Option<Vec<u8>>, default: bool, field: &str, line: usize) -> Result<bool, String> {
    match value.as_deref() {
        None => Ok(default),
        Some(b"yes") => Ok(true),
        Some(b"no") => Ok(false),
        Some(_) => Err(format!("line {line}: invalid {field}; expected yes or no")),
    }
}

fn parse_expected_exit(value: Option<Vec<u8>>, line: usize) -> Result<ExpectedExit, String> {
    let Some(value) = value else { return Ok(ExpectedExit::Code(0)) };
    match value.as_slice() {
        b"e == 0" => Ok(ExpectedExit::IsZero),
        b"e != 0" => Ok(ExpectedExit::IsNonzero),
        _ if value.iter().all(u8::is_ascii_digit) => {
            let code = parse_positive_or_zero_integer(&value)
                .filter(|code| *code <= 255)
                .ok_or_else(|| format!("line {line}: invalid expected-exit code"))?;
            Ok(ExpectedExit::Code(code as u8))
        }
        _ => Err(format!("line {line}: unsupported expected-exit expression")),
    }
}

fn parse_positive_or_zero_integer(value: &[u8]) -> Option<u64> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn make_assertion(
    exact: Option<Vec<u8>>,
    pattern: Option<Vec<u8>>,
    line: usize,
) -> Result<Option<OutputAssertion>, String> {
    match (exact, pattern) {
        (Some(bytes), None) => Ok(Some(OutputAssertion::Exact(bytes))),
        (None, Some(bytes)) => OutputAssertion::pattern(&bytes)
            .map(Some)
            .map_err(|error| format!("line {line}: invalid output pattern: {error}")),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(format!("line {line}: conflicting output assertions")),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_corpus, FileKind};

    #[test]
    fn preserves_invalid_utf8_in_values_and_paths() {
        let corpus = b"name: raw-bytes\nfile-setup: file 644 \"bad\xff\"\tbytes\xfe\nstdin:\n\tinput\xfd\nexpected-stdout:\n\toutput\xfc\n---\n";
        let cases = parse_corpus(corpus).unwrap();
        assert_eq!(cases[0].files[0].kind, FileKind::File);
        assert_eq!(cases[0].files[0].path, b"bad\xff");
        assert_eq!(cases[0].files[0].contents, b"\tbytes\xfe");
        assert_eq!(cases[0].stdin.as_deref(), Some(&b"input\xfd\n"[..]));
        assert_eq!(cases[0].stdout.as_ref().unwrap().exact_bytes(), Some(&b"output\xfc\n"[..]));
    }

    #[test]
    fn rejects_unknown_and_duplicate_fields() {
        assert!(parse_corpus(b"name: x\nunknown: value\n---\n").is_err());
        assert!(parse_corpus(b"name: x\nname: y\n---\n").is_err());
        assert!(parse_corpus(b"name: x\nexpected-fail: yes\n---\n").is_err());
        assert!(parse_corpus(b"name: x\nneed-ctty: yes\n---\n").is_err());
        assert!(parse_corpus(b"name: x\nexpected-stdout-pattern: /x/\n---\n").is_err());
        assert!(parse_corpus(b"name: x\nfile-setup: file 688 \"bad-mode\"\n---\n").is_err());
        assert!(parse_corpus(b"name: x\nfile-setup: symlink 777 \"link\"../../outside\n---\n").is_err());
    }

    #[test]
    fn applies_the_explicit_trailing_newline_marker() {
        let cases = parse_corpus(b"name: newline\nstdin:\n\tline\n---\nname: no-newline\nstdin: !\n\tline\n---\n").unwrap();
        assert_eq!(cases[0].stdin.as_deref(), Some(&b"line\n"[..]));
        assert_eq!(cases[1].stdin.as_deref(), Some(&b"line"[..]));
    }
}
