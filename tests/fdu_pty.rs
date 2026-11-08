#[path = "fdu_support.rs"]
mod fdu_support;

use ptytest::Key;
use std::fs;
use std::io;
use fdu_support::{ScreenText, Session, TempDir};

#[test]
fn startup_shows_a_minimal_title_and_the_entry_count_and_restores_the_terminal() -> io::Result<()> {
    let (_temp, root) = TempDir::standard_tree()?;
    let mut session = Session::start("startup-ready", &root, &[], 100, 24);
    let screen = session.wait_ready();
    let title = screen.lines().remove(0);
    assert_eq!(title.split_whitespace().collect::<Vec<_>>(), ["fdu"]);
    let status = screen.lines().pop().unwrap();
    for noise in ["Ready", "allocated", "sort", "Scanning"] {
        assert!(!title.contains(noise), "{noise} is not in the title");
    }
    for noise in ["Ready", "allocated", "Scanning", "incomplete", "excluded"] {
        assert!(!status.contains(noise), "{noise} is not in the status bar");
    }
    for name in ["alpha/", "beta/", "top.txt"] {
        assert!(screen.shows(name), "{name} is listed");
    }
    assert!(screen.shows("root (3 shown, 6 total,"));
    let status = screen.lines().pop().unwrap();
    assert!(status.trim().starts_with("6 entries · d mark"), "{status}");
    assert!(status.contains("? help"), "{status}");
    assert!(!screen.shows("Marked"));
    session.quit();
    Ok(())
}

#[test]
fn repeated_exclusions_hide_exact_root_relative_paths_in_the_browser() -> io::Result<()> {
    let (_temp, root) = TempDir::standard_tree()?;
    let mut session = Session::start("exclude-paths", &root, &["-x", "alpha", "--exclude", "beta"], 100, 24);
    let screen = session.wait_ready();
    assert!(screen.shows("top.txt"));
    assert!(!screen.shows("alpha/"));
    assert!(!screen.shows("beta/"));
    assert!(screen.shows("root (1 shown, 1 total,"));
    session.quit();
    Ok(())
}

#[test]
fn directory_with_excluded_contents_is_deleted_around_the_excluded_entry() -> io::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.0.join("root");
    fs::create_dir_all(root.join("alpha/nested"))?;
    fs::write(root.join("alpha/nested/hidden.txt"), b"excluded")?;
    fs::write(root.join("alpha/visible.txt"), b"included")?;

    let mut session = Session::start("exclude-delete-safety", &root, &["--exclude", "alpha/nested"], 100, 24);
    session.ready_by_name();
    session.text("d");
    session.wait_for("Marked 1 item");
    session.key(Key::Tab);
    session.key(Key::Ctrl('r'));
    session.wait_for("Permanently delete");
    session.key(Key::Enter);
    session.wait_for("Deleted 1");
    session.quit();

    assert!(root.join("alpha/nested/hidden.txt").exists());
    assert!(!root.join("alpha/visible.txt").exists());
    assert!(root.join("alpha").is_dir());
    Ok(())
}

#[test]
fn delete_confirmation_shows_root_and_entry_counts_and_leaves_unconfirmed_marks() -> io::Result<()> {
    let (_temp, root) = TempDir::standard_tree()?;
    let mut session = Session::start("confirm-delete", &root, &[], 100, 24);
    session.ready_by_name();
    session.text("d");
    session.wait_for("Marked 1 item");
    session.key(Key::Tab);
    session.key(Key::Ctrl('r'));
    let screen = session.wait_for("Permanently delete");
    assert!(screen.shows("Permanently delete 1 item (3 entries,"));
    session.key(Key::Escape);
    session.wait_until_absent("Permanently delete");
    assert!(root.join("alpha/inner.txt").exists());
    session.quit();
    Ok(())
}

#[test]
fn help_screen_opens_and_closes() -> io::Result<()> {
    let (_temp, root) = TempDir::standard_tree()?;
    let mut session = Session::start("help-screen", &root, &[], 100, 24);
    session.wait_ready();
    session.text("?");
    let screen = session.wait_for("Tab switches");
    assert!(screen.shows("there is no Trash"));
    session.key(Key::Escape);
    session.wait_until_absent("Tab switches");
    session.quit();
    Ok(())
}

#[test]
fn read_only_mode_disables_deletion() -> io::Result<()> {
    let (_temp, root) = TempDir::standard_tree()?;
    let mut session = Session::start("read-only-delete", &root, &["--read-only"], 100, 24);
    session.ready_by_name();
    session.text("d");
    session.wait_for("Marked 1 item");
    session.key(Key::Tab);
    session.key(Key::Ctrl('r'));
    session.wait_for("This session is read only; deletion is disabled.");
    assert!(root.join("alpha/inner.txt").exists());
    session.quit();
    Ok(())
}

#[test]
fn minus_toggles_between_side_by_side_and_stacked_panes() -> io::Result<()> {
    let (_temp, root) = TempDir::standard_tree()?;
    let mut session = Session::start("split-toggle", &root, &[], 100, 24);
    session.ready_by_name();
    session.text("d");
    let screen = session.wait_for("Marked 1 item");
    let list_row = screen.lines().iter().position(|line| line.contains("root (")).unwrap();
    let marked_row = screen.lines().iter().position(|line| line.contains("Marked 1 item")).unwrap();
    assert_eq!(list_row, marked_row, "side by side by default");

    session.text("-");
    let screen = session.wait("stacked", |screen| {
        let lines = screen.lines();
        match (lines.iter().position(|line| line.contains("root (")), lines.iter().position(|line| line.contains("Marked 1 item"))) {
            (Some(list), Some(marked)) => list < marked,
            _ => false,
        }
    });
    assert!(screen.shows("alpha/"), "the list stays readable above the marks");

    session.text("-");
    session.wait("side by side again", |screen| {
        let lines = screen.lines();
        lines.iter().position(|line| line.contains("root (")) == lines.iter().position(|line| line.contains("Marked 1 item"))
    });
    session.quit();
    Ok(())
}

#[test]
fn s_cycles_between_size_name_and_date_order() -> io::Result<()> {
    let (_temp, root) = TempDir::standard_tree()?;
    let mut session = Session::start("sort-toggle", &root, &[], 100, 24);
    session.wait_ready();
    session.wait("size order", |screen| screen.lines()[2].contains("beta/"));
    session.text("s");
    session.wait("name order", |screen| screen.lines()[2].contains("alpha/"));
    session.text("s");
    session.wait_for("Sorted by date");
    session.text("s");
    session.wait("size order again", |screen| screen.lines()[2].contains("beta/"));
    session.quit();
    Ok(())
}

#[test]
fn unicode_filter_input_and_backspace_preserve_complete_characters() -> io::Result<()> {
    let temp = TempDir::new()?;
    let root = temp.0.join("root");
    fs::create_dir(&root)?;
    fs::write(root.join("日本-é.txt"), b"x")?;
    fs::write(root.join("other.txt"), b"x")?;
    let mut session = Session::start("unicode-filter", &root, &[], 100, 16);
    session.wait_ready();
    session.text("/");
    session.text("日本é");
    session.wait_for("/ 日本é");
    session.key(Key::Backspace);
    session.wait("unicode backspace", |screen| screen.shows("/ 日本") && !screen.shows("/ 日本é"));
    session.key(Key::Enter);
    let screen = session.wait_for("(1 shown, 2 total");
    assert!(screen.shows("日本-é.txt"));
    assert!(!screen.shows("other.txt"));
    session.quit();
    Ok(())
}

#[test]
fn profile_large_layout_fixture() -> io::Result<()> {
    let fixture = fdu_support::layout_fixture();
    let mut session = Session::start_with_env(
        "profile-large",
        fixture,
        &["--apparent"],
        100,
        24,
        &[("FDU_PROFILE", "1")],
    );
    session.wait_ready();
    session.text("q");
    let deadline = session.terminal.deadline(session.step);
    session.terminal.wait_for_screen(deadline, "prompt returns after fdu exit", |screen| {
        let cursor = screen.cursor();
        screen.row(usize::from(cursor.row)).is_some_and(|r| {
            r.get(..usize::from(cursor.column)).is_some_and(|p| p.ends_with("$ "))
        })
    }).unwrap();
    let raw = session.terminal.raw_output();
    let text = String::from_utf8_lossy(&raw);
    for line in text.lines() {
        if line.contains("fdu-profile") {
            println!("{line}");
        }
    }
    session.terminal.send_bytes(deadline, b"exit\r").unwrap();
    let status = session.terminal.wait_for_exit(deadline).unwrap();
    assert_eq!(status, ptytest::ExitStatus::Code(0));
    Ok(())
}
