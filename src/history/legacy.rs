use super::store::Occurrence;
use fxhash::FxHashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const TS_EPOCH_MILLIS: u64 = 883_612_800_000;
const RECORD_V1: &str = ":ish-history:v1\t";
const RECORD_V2: &str = ":ish-history:v2\t";

fn invalid(message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("legacy history migration: {message}"),
    )
}

fn read_if_present(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn cache_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bin");
    path.with_file_name(name)
}

pub(super) fn read(path: &Path) -> io::Result<Vec<Occurrence>> {
    let mut text = Vec::new();
    if let Some(bytes) = read_if_present(path)? {
        // Old shells could record arbitrary bytes. Import must not abort on
        // them, so invalid sequences become U+FFFD.
        let contents = String::from_utf8_lossy(&bytes);
        for line in contents.lines() {
            if !line.is_empty() {
                text.push(parse_line(line)?);
            }
        }
    }
    let mut cached = match read_if_present(&cache_path(path))? {
        Some(bytes) => parse_cache(&bytes).ok_or_else(|| invalid("binary cache is corrupt"))?,
        None => Vec::new(),
    };
    // A legacy flush could copy cache entries into the text file. An identical
    // command and timestamp denotes that surviving occurrence, not another use.
    let text_uses: FxHashSet<(&str, u64)> = text
        .iter()
        .map(|entry| (entry.command.as_str(), entry.timestamp))
        .collect();
    cached.retain(|entry| !text_uses.contains(&(entry.command.as_str(), entry.timestamp)));
    cached.extend(text);
    Ok(cached)
}

fn parse_line(line: &str) -> io::Result<Occurrence> {
    let mut entry = Occurrence {
        id: 0,
        command: line.to_string(),
        timestamp: 0,
        session_id: 0,
        cwd: None,
    };
    let (rest, has_cwd) = if let Some(rest) = line.strip_prefix(RECORD_V2) {
        (rest, true)
    } else if let Some(rest) = line.strip_prefix(RECORD_V1) {
        (rest, false)
    } else {
        if line.starts_with(":ish-history:") {
            return Err(invalid("unsupported or malformed text record"));
        }
        return Ok(entry);
    };
    let mut parts = rest.splitn(if has_cwd { 4 } else { 3 }, '\t');
    entry.timestamp = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid("invalid timestamp"))?;
    entry.session_id = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid("invalid session id"))?;
    if has_cwd {
        let field = parts.next().ok_or_else(|| invalid("missing directory"))?;
        let cwd = unescape(field).ok_or_else(|| invalid("invalid directory escaping"))?;
        entry.cwd = Some(PathBuf::from(cwd));
    }
    entry.command = parts
        .next()
        .filter(|command| !command.is_empty())
        .ok_or_else(|| invalid("missing command"))?
        .to_string();
    Ok(entry)
}

fn unescape(field: &str) -> Option<String> {
    let mut output = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            output.push(c);
        } else {
            output.push(match chars.next()? {
                '\\' => '\\',
                't' => '\t',
                'n' => '\n',
                'r' => '\r',
                _ => return None,
            });
        }
    }
    Some(output)
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Only the final binary cache layout is imported: a 16-byte header (magic,
/// version 5, command count, command arena size, directory arena size), one
/// 8-byte timestamp per command, then NUL-terminated commands and directories.
fn parse_cache(bytes: &[u8]) -> Option<Vec<Occurrence>> {
    const HEADER: usize = 16;
    if bytes.get(..4)? != b"ISH\x05" {
        return None;
    }
    let count = u32_at(bytes, 4)? as usize;
    let arena_size = u32_at(bytes, 8)? as usize;
    let cwd_size = u32_at(bytes, 12)? as usize;
    let arena_start = HEADER.checked_add(count.checked_mul(8)?)?;
    let cwd_start = arena_start.checked_add(arena_size)?;
    if cwd_start.checked_add(cwd_size)? != bytes.len() {
        return None;
    }
    let split = |range: std::ops::Range<usize>| -> Option<Vec<&str>> {
        let text = std::str::from_utf8(bytes.get(range)?).ok()?;
        let mut parts: Vec<&str> = text.split('\0').collect();
        (parts.pop()?.is_empty() && parts.len() == count).then_some(parts)
    };
    let commands = split(arena_start..cwd_start)?;
    let cwds = split(cwd_start..bytes.len())?;
    let mut entries = Vec::with_capacity(count);
    for (i, (command, cwd)) in commands.into_iter().zip(cwds).enumerate() {
        if command.is_empty() {
            return None;
        }
        entries.push(Occurrence {
            id: 0,
            command: command.to_string(),
            timestamp: u64_at(bytes, HEADER + i * 8)?.wrapping_add(TS_EPOCH_MILLIS),
            session_id: 0,
            cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
        });
    }
    Some(entries)
}
