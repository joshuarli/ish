use super::store::Occurrence;
use fxhash::FxHashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const TS_EPOCH_MILLIS: u64 = 883_612_800_000;
const RECORD_V1: &str = ":ish-history:v1\t";
const RECORD_V2: &str = ":ish-history:v2\t";

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("legacy history migration: {message}"))
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
        let contents = std::str::from_utf8(&bytes).map_err(|_| invalid("text is not UTF-8"))?;
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
    let text_uses: FxHashSet<(&str, u64)> = text.iter()
        .map(|entry| (entry.command.as_str(), entry.timestamp)).collect();
    cached.retain(|entry| !text_uses.contains(&(entry.command.as_str(), entry.timestamp)));
    cached.extend(text);
    Ok(cached)
}

fn parse_line(line: &str) -> io::Result<Occurrence> {
    let mut entry = Occurrence { id: 0, command: line.to_string(), timestamp: 0,
        session_id: 0, cwd: None };
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
    entry.timestamp = parts.next().and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid("invalid timestamp"))?;
    entry.session_id = parts.next().and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid("invalid session id"))?;
    if has_cwd {
        let field = parts.next().ok_or_else(|| invalid("missing directory"))?;
        let cwd = unescape(field).ok_or_else(|| invalid("invalid directory escaping"))?;
        entry.cwd = Some(PathBuf::from(cwd));
    }
    entry.command = parts.next().filter(|command| !command.is_empty())
        .ok_or_else(|| invalid("missing command"))?.to_string();
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
                '\\' => '\\', 't' => '\t', 'n' => '\n', 'r' => '\r', _ => return None,
            });
        }
    }
    Some(output)
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}

fn parse_cache(bytes: &[u8]) -> Option<Vec<Occurrence>> {
    if bytes.get(..3)? != b"ISH" {
        return None;
    }
    let version = *bytes.get(3)?;
    if !(1..=5).contains(&version) {
        return None;
    }
    let header: usize = if version <= 2 { 20 } else if version == 5 { 16 } else { 12 };
    let count = u32_at(bytes, if version <= 2 { 12 } else { 4 })? as usize;
    let arena_size = u32_at(bytes, if version <= 2 { 16 } else { 8 })? as usize;
    let cwd_size = if version == 5 { u32_at(bytes, 12)? as usize } else { 0 };
    let ts_width: usize = match version { 1 => 0, 2 | 3 => 4, _ => 8 };
    let hashes_size = if version <= 2 { count.checked_mul(8)? } else { 0 };
    let offsets_size = if version <= 2 { count.checked_mul(6)? } else { 0 };
    let ts_start = header.checked_add(hashes_size)?;
    let offsets_start = ts_start.checked_add(count.checked_mul(ts_width)?)?;
    let arena_start = offsets_start.checked_add(offsets_size)?;
    let cwd_start = arena_start.checked_add(arena_size)?;
    if cwd_start.checked_add(cwd_size)? != bytes.len() {
        return None;
    }
    let arena = std::str::from_utf8(bytes.get(arena_start..cwd_start)?).ok()?;
    let commands: Vec<&str> = if version <= 2 {
        (0..count).map(|i| {
            let offset = offsets_start + i * 6;
            let start = u32_at(bytes, offset)? as usize;
            let len = u16::from_le_bytes(bytes.get(offset+4..offset+6)?.try_into().ok()?) as usize;
            arena.get(start..start.checked_add(len)?)
        }).collect::<Option<_>>()?
    } else {
        let mut commands: Vec<&str> = arena.split('\0').collect();
        if commands.pop()? != "" || commands.len() != count {
            return None;
        }
        commands
    };
    let cwds: Vec<Option<PathBuf>> = if version == 5 {
        let cwd_arena = std::str::from_utf8(bytes.get(cwd_start..)?).ok()?;
        let mut parts: Vec<&str> = cwd_arena.split('\0').collect();
        if parts.pop()? != "" || parts.len() != count {
            return None;
        }
        parts.into_iter().map(|cwd| (!cwd.is_empty()).then(|| PathBuf::from(cwd))).collect()
    } else {
        vec![None; count]
    };
    let mut entries = Vec::with_capacity(count);
    for (i, (command, cwd)) in commands.into_iter().zip(cwds).enumerate() {
        if command.is_empty() {
            return None;
        }
        let timestamp = match version {
            1 => 0,
            2 => (u32_at(bytes, ts_start + i * 4)? as u64).checked_mul(1000)?,
            3 => (u32_at(bytes, ts_start + i * 4)? as u64).checked_mul(1000)?
                .checked_add(TS_EPOCH_MILLIS)?,
            _ => u64_at(bytes, ts_start + i * 8)?.wrapping_add(TS_EPOCH_MILLIS),
        };
        entries.push(Occurrence { id: 0, command: command.to_string(), timestamp,
            session_id: 0, cwd });
    }
    Some(entries)
}
