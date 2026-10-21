use crate::line::LineBuffer;
use crate::mode::{CompletionMode, DirPicker, HistorySearch, Mode};
use crate::render;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::Path;

pub struct LayoutSnapshot<'a> {
    pub rows: u16,
    pub cols: u16,
    pub prompt_display_len: usize,
    pub line: &'a LineBuffer,
    pub suggestion_display_len: Option<usize>,
    pub region: render::RenderedRegion,
    pub mode: &'a Mode,
}

pub fn render_layout_dump(out: &mut String, snapshot: LayoutSnapshot<'_>) {
    let LayoutSnapshot {
        rows,
        cols,
        prompt_display_len,
        line,
        suggestion_display_len,
        region,
        mode,
    } = snapshot;
    let suggestion_len = suggestion_display_len.unwrap_or(0);

    out.clear();
    let _ = writeln!(out, "ish layout dump");
    let _ = writeln!(out, "version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(out, "terminal.rows: {}", rows);
    let _ = writeln!(out, "terminal.cols: {}", cols);
    let _ = writeln!(out, "prompt.display_len: {prompt_display_len}");
    let _ = writeln!(out, "line.bytes: {}", line.text().len());
    let _ = writeln!(out, "line.cursor_byte: {}", line.cursor());
    let _ = writeln!(
        out,
        "line.cursor_display_col: {}",
        line.display_cursor_pos()
    );
    let _ = writeln!(out, "line.display_len: {}", line.display_len());
    let _ = writeln!(out, "suggestion.present: {}", suggestion_display_len.is_some());
    let _ = writeln!(out, "suggestion.display_len: {suggestion_len}");
    let _ = writeln!(out, "line.has_newlines: {}", line.has_newlines());
    let _ = writeln!(out, "rendered_region: {region:?}");
    let _ = writeln!(out, "mode: {}", mode.name());
    render::debug_dump_prompt_layout(
        out,
        prompt_display_len,
        line,
        suggestion_len,
        cols,
    );

    match mode {
        Mode::Normal => {}
        Mode::Completion(CompletionMode { state, base_line }) => {
            let _ = writeln!(
                out,
                "completion.base_line.cursor_byte: {}",
                base_line.cursor()
            );
            let _ = writeln!(out, "completion.selected: {}", state.selected);
            let _ = writeln!(out, "completion.cols: {}", state.cols);
            let _ = writeln!(out, "completion.rows: {}", state.rows);
            let _ = writeln!(out, "completion.scroll: {}", state.scroll);
            let _ = writeln!(out, "completion.term_cols: {}", state.term_cols);
            let _ = writeln!(out, "completion.dir_prefix_bytes: {}", state.dir_prefix.len());
            let _ = writeln!(out, "completion.in_quote: {}", state.in_quote);
            let _ = writeln!(out, "completion.entries: {:?}", state.comp.entries);
            render::debug_dump_completion_layout(out, state);
        }
        Mode::HistorySearch(HistorySearch {
            query,
            matches,
            candidates,
            scratch,
            candidate_stack,
            spare_candidates: _,
            previous_query: _,
            selected,
            saved_line,
        }) => {
            let _ = writeln!(out, "history.query.cursor_byte: {}", query.cursor());
            let _ = writeln!(
                out,
                "history.query.cursor_display_col: {}",
                query.display_cursor_pos()
            );
            let _ = writeln!(out, "history.matches: {:?}", matches);
            let _ = writeln!(out, "history.candidates: {candidates:?}");
            let _ = writeln!(out, "history.scratch: {scratch:?}");
            let _ = writeln!(out, "history.candidate_stack: {candidate_stack:?}");
            let _ = writeln!(out, "history.selected: {selected}");
            let _ = writeln!(out, "history.saved_line_bytes: {}", saved_line.len());
            render::debug_dump_pager_layout(
                out,
                render::PagerInput {
                    prefix_width: crate::line::str_width("search: "),
                    query_width: crate::line::str_width(query.text()),
                    suffix_width: 0,
                    query_cursor: query.display_cursor_pos(),
                    total_entries: matches.len(),
                    selected: *selected,
                    term_rows: rows,
                    term_cols: cols,
                },
            );
        }
        Mode::DirPicker(DirPicker { entries, selected }) => {
            let _ = writeln!(out, "dir_picker.entry_count: {}", entries.len());
            let _ = writeln!(out, "dir_picker.selected: {selected}");
            render::debug_dump_pager_layout(
                out,
                render::PagerInput {
                    prefix_width: crate::line::str_width("dirs:"),
                    query_width: 0,
                    suffix_width: 0,
                    query_cursor: 0,
                    total_entries: entries.len(),
                    selected: *selected,
                    term_rows: rows,
                    term_cols: cols,
                },
            );
        }
    }
}

pub fn write_layout_dump_file(home: &OsStr, dump: &str) -> Result<String, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    if home.is_empty() {
        return Err("ish-dump: HOME is not set".to_string());
    }
    let dir = Path::new(home).join(".cache/ish");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| format!("ish-dump: could not create {}: {error}", dir.display()))?;

    for _ in 0..8 {
        let name = random_dump_hex()
            .map_err(|error| format!("ish-dump: could not read random bytes: {error}"))?;
        let path = dir.join(format!("dump-{name}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, dump.as_bytes()).map_err(|error| {
                    format!("ish-dump: could not write {}: {error}", path.display())
                })?;
                return Ok(path.display().to_string());
            }
            Err(_) => continue,
        }
    }

    Err(format!(
        "ish-dump: could not choose a unique dump filename in {}",
        dir.display()
    ))
}

fn random_dump_hex() -> rustix::io::Result<String> {
    let mut bytes = [0u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        match rustix::rand::getrandom(&mut bytes[filled..], rustix::rand::GetRandomFlags::empty()) {
            Ok(count) => filled += count,
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error),
        }
    }

    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64_encode, random_dump_hex};

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn dump_names_are_distinct_hex() {
        let first = random_dump_hex().unwrap();
        let second = random_dump_hex().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
