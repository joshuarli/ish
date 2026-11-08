#[path = "fdu_support.rs"]
mod fdu_support;

use ptytest::{Key, SnapshotOptions};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use fdu_support::{layout_fixture, ScreenText, Session};

const SCAN_STEP: Duration = Duration::from_secs(120);

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots").join(format!("{name}.ptytest"))
}

fn open(label: &str, columns: u16, rows: u16) -> Session {
    let mut session = Session::start(label, layout_fixture(), &["--apparent", "--read-only"], columns, rows);
    session.step = SCAN_STEP;
    session.wait_ready();
    session
}

fn freeze(session: &mut Session, name: &str) {
    let snapshot = normalize_age_values(
        &session
            .terminal
            .screen()
            .to_text(SnapshotOptions::default().with_attributes()),
    );
    let path = golden(name);
    if std::env::var("PTYTEST_UPDATE_SNAPSHOTS").is_ok_and(|value| value == "1") {
        fs::write(path, snapshot).unwrap();
    } else {
        assert_eq!(fs::read_to_string(&path).unwrap(), snapshot, "snapshot {name} differs");
    }
}

fn normalize_age_values(snapshot: &str) -> String {
    let mut normalized = String::with_capacity(snapshot.len());
    for line in snapshot.lines() {
        let bytes = line.as_bytes();
        let is_screen_row = bytes.get(0..2).is_some_and(|prefix| prefix.iter().all(u8::is_ascii_digit))
            && line.contains('│');
        if is_screen_row {
            normalized.push_str(&normalize_age_row(line));
        } else {
            normalized.push_str(line);
        }
        normalized.push('\n');
    }
    normalized
}

fn normalize_age_row(line: &str) -> String {
    let characters: Vec<char> = line.chars().collect();
    let mut normalized = String::with_capacity(line.len());
    let mut copied_through = 0;
    let mut index = 0;
    while index < characters.len() {
        let Some(end) = age_value_end(&characters, index) else {
            index += 1;
            continue;
        };
        let age_width = end - index;
        let padding = AGE_COLUMN_WIDTH.saturating_sub(age_width);
        let mut start = index;
        for _ in 0..padding {
            if start == 0 || characters[start - 1] != '·' {
                break;
            }
            start -= 1;
        }
        if start > 0 && characters[start - 1] == '·' && end < characters.len() && characters[end] == '·' {
            normalized.extend(characters[copied_through..start].iter());
            normalized.push_str("<age>");
            copied_through = end;
            index = end;
        } else {
            index = end;
        }
    }
    normalized.extend(characters[copied_through..].iter());
    normalized
}

fn age_value_end(characters: &[char], start: usize) -> Option<usize> {
    if start > 0 && characters[start - 1].is_ascii_alphanumeric() {
        return None;
    }
    if characters.get(start) == Some(&'—') {
        return Some(start + 1);
    }
    let mut index = start;
    let mut units = 0;
    loop {
        let digits_start = index;
        while characters.get(index).is_some_and(char::is_ascii_digit) {
            index += 1;
        }
        if digits_start == index || !characters.get(index).is_some_and(|unit| "ywdhms".contains(*unit)) {
            return None;
        }
        units += 1;
        index += 1;
        if characters.get(index) == Some(&'o') && characters.get(index.saturating_sub(1)) == Some(&'m') {
            index += 1;
        }
        if units == 2 || !characters.get(index).is_some_and(char::is_ascii_digit) {
            break;
        }
    }
    if characters.get(index) == Some(&'+') {
        index += 1;
    }
    Some(index)
}

const AGE_COLUMN_WIDTH: usize = 5;

#[test]
fn ready_listing() {
    let mut session = open("snapshot-ready", 100, 24);
    freeze(&mut session, "ready-100x24");
    session.quit();
}

#[test]
fn marked_pane_beside_the_list() {
    let mut session = open("snapshot-marked", 100, 24);
    session.text("d");
    session.text("d");
    session.wait_for("Marked 2 items");
    freeze(&mut session, "marked-100x24");
    session.key(Key::Tab);
    session.wait("marked focus", fdu_support::list_is_dimmed);
    freeze(&mut session, "marked-focus-100x24");
    session.quit();
}

#[test]
fn stacked_panes() {
    let mut session = open("snapshot-stacked", 100, 24);
    session.text("d");
    session.wait_for("Marked 1 item");
    session.text("-");
    session.wait("stacked", |screen| {
        let row = |needle: &str| (0..screen.row_count()).find(|row| screen.row(*row).is_some_and(|text| text.contains(needle)));
        matches!((row("directory-"), row("Marked 1 item")), (Some(list), Some(marked)) if list < marked)
    });
    freeze(&mut session, "stacked-100x24");
    session.quit();
}

#[test]
fn narrow_terminal_shows_one_pane() {
    let mut session = open("snapshot-narrow", 50, 24);
    session.text("d");
    session.text("d");
    session.key(Key::Tab);
    session.wait_for("Marked 2 items");
    freeze(&mut session, "narrow-marked-50x24");
    session.quit();
}

#[test]
fn inside_a_marked_directory() {
    let mut session = open("snapshot-covered", 100, 24);
    session.text("d");
    session.key(Key::Home);
    session.key(Key::Enter);
    session.wait_for("Inside marked directory");
    freeze(&mut session, "covered-100x24");
    session.quit();
}

#[test]
fn help_overlay() {
    let mut session = open("snapshot-help", 100, 24);
    session.text("?");
    session.wait_for("closes help");
    freeze(&mut session, "help-100x24");
    session.key(Key::Escape);
    session.wait_until_absent("closes help");
    session.quit();
}

#[test]
fn filter_prompt_and_applied_filter() {
    let mut session = open("snapshot-filter", 100, 24);
    session.text("/");
    session.text("directory-00001");
    session.wait_for("/ directory-00001");
    freeze(&mut session, "filter-prompt-100x24");
    session.key(Key::Enter);
    session.wait("applied filter", |screen| {
        screen.shows("filter \"directory-00001\"") && !screen.shows("Type a name fragment")
    });
    freeze(&mut session, "filtered-100x24");
    session.quit();
}
