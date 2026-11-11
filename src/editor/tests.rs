use super::*;
use crate::input::{Key, KeyEvent, Modifiers};

fn context<'a>(aliases: &'a AliasMap, history: &'a history::History, directories: &'a [String]) -> EditorContext<'a> {
    EditorContext { aliases, history, cwd: Path::new("/"), home: Some("/"), home_display: "/", path: b"", directories }
}

fn event(editor: &mut Editor, context: &EditorContext<'_>, input: EditorEvent<'_>) -> EditorAction {
    let action = editor.handle(input, context);
    match action {
        EditorAction::Complete => { editor.complete(context); EditorAction::Continue }
        EditorAction::SearchHistory => { editor.search_history(context); EditorAction::Continue }
        action => action,
    }
}

#[test]
fn normal_paste_continuation_cancel_and_submit_emit_memory_output() {
    let aliases = AliasMap::new();
    let history = history::History::from_entries(Vec::new());
    let context = context(&aliases, &history, &[]);
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Paste("printf 'open"));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::Continue));
    event(&mut editor, &context, EditorEvent::Paste("closed'"));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::Submit(line) if line == "printf 'open\nclosed'"));
    let mut output = Vec::new();
    editor.write_output(&mut output).unwrap();
    assert!(String::from_utf8_lossy(&output).contains("closed"));
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Paste("discard"));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::ctrl('c'))), EditorAction::Cancel));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::ctrl('p'))), EditorAction::Dump));
}

#[test]
fn suggestion_word_accept_len_splits_words_and_paths() {
    assert_eq!(suggestion_word_accept_len(""), 0);
    assert_eq!(suggestion_word_accept_len("checkout feature"), "checkout".len());
    assert_eq!(suggestion_word_accept_len(" checkout"), " checkout".len());
    assert_eq!(suggestion_word_accept_len("  --flag"), "  --flag".len());
    assert_eq!(suggestion_word_accept_len("src/main.rs"), "src/main.rs".len());
}

#[test]
fn right_accepts_whole_suggestion_and_ctrl_right_accepts_next_word() {
    let aliases = AliasMap::new();
    let history = history::History::from_entries(vec!["git checkout feature-branch".into()]);
    let ctx = context(&aliases, &history, &[]);
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &ctx);
    event(&mut editor, &ctx, EditorEvent::Paste("git ch"));
    let action = event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::with_mods(Key::Right, Modifiers { ctrl: true, alt: false, shift: false })));
    assert!(matches!(action, EditorAction::Continue));
    assert_eq!(editor.view.line.text(), "git checkout");

    let aliases = AliasMap::new();
    let history = history::History::from_entries(vec!["git checkout feature-branch".into()]);
    let ctx = context(&aliases, &history, &[]);
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &ctx);
    event(&mut editor, &ctx, EditorEvent::Paste("git ch"));
    event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::key(Key::Right)));
    assert_eq!(editor.view.line.text(), "git checkout feature-branch");
}

#[test]
fn undo_redo_restore_edits_and_paste() {
    let aliases = AliasMap::new();
    let history = history::History::from_entries(Vec::new());
    let ctx = context(&aliases, &history, &[]);
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &ctx);
    for c in "echo hi".chars() {
        event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::char(c)));
    }
    assert_eq!(editor.view.line.text(), "echo hi");
    event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::ctrl('_')));
    assert_eq!(editor.view.line.text(), "");
    event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::alt('r')));
    assert_eq!(editor.view.line.text(), "echo hi");

    event(&mut editor, &ctx, EditorEvent::Paste(" there"));
    assert_eq!(editor.view.line.text(), "echo hi there");
    event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::ctrl('_')));
    assert_eq!(editor.view.line.text(), "echo hi");
}

#[test]
fn completion_cancel_restores_prefix_and_accept_uses_selected_text_after_resize() {
    let mut aliases = AliasMap::new();
    aliases.set("zzfirst".into(), vec!["true".into()]);
    aliases.set("zzsecond".into(), vec!["false".into()]);
    let history = history::History::from_entries(Vec::new());
    let context = context(&aliases, &history, &[]);
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Paste("zz"));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Tab)));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Tab)));
    event(&mut editor, &context, EditorEvent::Resize { rows: 3, cols: 8 });
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Escape)));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::Submit(line) if line == "zz"));
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Paste("zz"));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Tab)));
    event(&mut editor, &context, EditorEvent::Paste("first"));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter)));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::Submit(line) if line.trim() == "zzfirst"));
    let mut output = Vec::new();
    editor.write_output(&mut output).unwrap();
    assert!(!output.is_empty());
}

#[test]
fn history_search_paste_accept_and_cancel_preserve_the_saved_line() {
    let aliases = AliasMap::new();
    let history = history::History::from_entries(vec!["echo first".into(), "printf chosen".into()]);
    let context = context(&aliases, &history, &[]);
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Paste("saved"));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::ctrl('r')));
    event(&mut editor, &context, EditorEvent::Paste("chosen"));
    event(&mut editor, &context, EditorEvent::Resize { rows: 2, cols: 12 });
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Escape)));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::Submit(line) if line == "saved"));
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::ctrl('r')));
    event(&mut editor, &context, EditorEvent::Paste("chosen"));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter)));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::Submit(line) if line == "printf chosen"));
    let mut output = Vec::new();
    editor.write_output(&mut output).unwrap();
    assert!(String::from_utf8_lossy(&output).contains("search:"));
}

#[test]
fn directory_picker_cancel_and_accept_return_actions_without_changing_cwd() {
    let aliases = AliasMap::new();
    let history = history::History::from_entries(Vec::new());
    let directories = ["/first".into(), "/second".into()];
    let context = context(&aliases, &history, &directories);
    let before = std::env::current_dir().unwrap();
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Paste("saved"));
    let picker = KeyEvent { key: Key::Backspace, mods: crate::input::Modifiers { ctrl: true, ..crate::input::Modifiers::NONE } };
    event(&mut editor, &context, EditorEvent::Key(picker));
    event(&mut editor, &context, EditorEvent::Paste("ignored"));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Escape)));
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::Submit(line) if line == "saved"));
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Key(picker));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Down)));
    event(&mut editor, &context, EditorEvent::Resize { rows: 1, cols: 8 });
    assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::ChangeDirectory(path) if path == "/first"));
    assert_eq!(std::env::current_dir().unwrap(), before);
    let mut output = Vec::new();
    editor.write_output(&mut output).unwrap();
    assert!(String::from_utf8_lossy(&output).contains("dirs:"));
}

#[test]
fn completion_reads_the_borrowed_cwd_without_changing_the_host_directory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp").join(format!("ish-editor-cwd-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(root.join("folder")).unwrap();
    std::fs::write(root.join("alpha"), b"").unwrap();
    std::fs::write(root.join("folder/beta"), b"").unwrap();
    let aliases = AliasMap::new();
    let history = history::History::from_entries(Vec::new());
    let mut context = context(&aliases, &history, &[]);
    context.cwd = &root;
    let before = std::env::current_dir().unwrap();
    let mut editor = Editor::new(24, 80);
    for (input, expected) in [("echo al", "echo alpha"), ("echo fo/be", "echo folder/beta")] {
        editor.begin("$ ".into(), 2, &context);
        event(&mut editor, &context, EditorEvent::Paste(input));
        event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Tab)));
        assert!(matches!(event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Enter))), EditorAction::Submit(line) if line.trim() == expected));
    }
    assert_eq!(std::env::current_dir().unwrap(), before);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn parse_completion_word_strips_tilde_quote_prefix() {
    assert_eq!(super::parse_completion_word("~/'Library/Mobile Documents/", true, false), ("~/Library/Mobile Documents/".into(), true));
    assert_eq!(super::parse_completion_word("~/\"Library/Mobile Documents/", false, false), ("~/Library/Mobile Documents/".into(), true));
    assert_eq!(super::parse_completion_word("~\"/Library/Mobile Documents/", false, true), ("~/Library/Mobile Documents/".into(), true));
    assert_eq!(super::parse_completion_word("'Library/Mobile Documents/", true, false), ("Library/Mobile Documents/".into(), true));
    assert_eq!(super::parse_completion_word("~/'Library/Mobile Documents/'", false, false), ("~/Library/Mobile Documents/".into(), true));
}

#[test]
fn quoted_tilde_path_with_space_completes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp").join(format!("ish-editor-quoted-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let docs = root.join("Library/Mobile Documents");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("target.txt"), b"").unwrap();
    let aliases = AliasMap::new();
    let history = history::History::from_entries(Vec::new());
    let home_str = root.to_str().unwrap().to_string();
    let base = context(&aliases, &history, &[]);
    let ctx = EditorContext { aliases: base.aliases, history: base.history, cwd: &root, home: Some(home_str.as_str()), home_display: "/", path: b"", directories: &[] };
    let before = std::env::current_dir().unwrap();
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &ctx);
    event(&mut editor, &ctx, EditorEvent::Paste("ln -s ~/'Library/Mobile Documents/"));
    event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::key(Key::Tab)));
    let result = event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::key(Key::Enter)));
    match result {
        EditorAction::Submit(line) => {
            assert!(line.contains("Mobile Documents/target.txt"), "unexpected completion: {line}");
            assert!(line.contains("~/'Library/Mobile Documents/target.txt'"), "expected quoted tilde form, got: {line}");
        }
        _other => panic!("expected submit, got continue/cancel"),
    }
    assert_eq!(std::env::current_dir().unwrap(), before);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn backslash_escaped_space_completes() {
    assert_eq!(
        super::parse_completion_word("Library/Mobile\\ Documents/", false, false),
        ("Library/Mobile Documents/".into(), false)
    );
    assert_eq!(
        super::shell_tokens("mv foo\\ bar baz"),
        vec!["mv".to_string(), "foo bar".to_string(), "baz".to_string()]
    );
    assert_eq!(
        super::shell_tokens("mv \"foo bar\" baz"),
        vec!["mv".to_string(), "foo bar".to_string(), "baz".to_string()]
    );
    assert_eq!(super::current_cmd_start("echo \"a|b\"; cd "), 11);
}

#[test]
fn backslash_escaped_tilde_path_with_space_completes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp").join(format!("ish-editor-backslash-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let docs = root.join("Library/Mobile Documents");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("target.txt"), b"").unwrap();
    let aliases = AliasMap::new();
    let history = history::History::from_entries(Vec::new());
    let home_str = root.to_str().unwrap().to_string();
    let base = context(&aliases, &history, &[]);
    let ctx = EditorContext { aliases: base.aliases, history: base.history, cwd: &root, home: Some(home_str.as_str()), home_display: "/", path: b"", directories: &[] };
    let before = std::env::current_dir().unwrap();
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &ctx);
    event(&mut editor, &ctx, EditorEvent::Paste("ln -s ~/Library/Mobile\\ Documents/"));
    event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::key(Key::Tab)));
    match event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::key(Key::Enter))) {
        EditorAction::Submit(line) => {
            assert!(line.contains("Mobile Documents/target.txt"), "unexpected completion: {line}");
            assert!(line.contains("~/'Library/Mobile Documents/target.txt'"), "expected quoted tilde form, got: {line}");
        }
        _other => panic!("expected submit"),
    }
    assert_eq!(std::env::current_dir().unwrap(), before);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn comments_do_not_hold_complete_commands_in_continuation() {
    let aliases = AliasMap::new();
    let history = history::History::from_entries(Vec::new());
    let ctx = context(&aliases, &history, &[]);
    for input in ["echo okay # don't wait", "echo okay # |", "echo okay # trailing \\", "echo okay;# &&", "echo a\n# 'open\necho b"] {
        assert!(crate::dispatch::plan(input).is_ok(), "dispatch must accept {input:?}");
        let mut editor = Editor::new(24, 80);
        editor.begin("$ ".into(), 2, &ctx);
        event(&mut editor, &ctx, EditorEvent::Paste(input));
        let action = event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::key(Key::Enter)));
        assert!(matches!(&action, EditorAction::Submit(line) if line == input), "{input:?} was not submitted");
    }
    for input in ["echo a#'b", "echo 'a # b", "echo a |", "echo a && # done", "echo a \\"] {
        let mut editor = Editor::new(24, 80);
        editor.begin("$ ".into(), 2, &ctx);
        event(&mut editor, &ctx, EditorEvent::Paste(input));
        let action = event(&mut editor, &ctx, EditorEvent::Key(KeyEvent::key(Key::Enter)));
        assert!(matches!(action, EditorAction::Continue), "{input:?} must continue");
    }
}

#[test]
fn partial_path_completion_through_the_editor_is_bounded_to_the_directory_budget() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work/tmp").join(format!("ish-editor-partial-budget-{}", std::process::id()));
    for branch in 0..70 {
        std::fs::create_dir_all(root.join(format!("branch{branch:02}"))).unwrap();
        std::fs::write(root.join(format!("branch{branch:02}/item{branch:02}")), b"").unwrap();
    }
    let aliases = AliasMap::new();
    let history = history::History::from_entries(Vec::new());
    let mut context = context(&aliases, &history, &[]);
    context.cwd = &root;
    let mut editor = Editor::new(24, 80);
    editor.begin("$ ".into(), 2, &context);
    event(&mut editor, &context, EditorEvent::Paste("cat br/item"));
    event(&mut editor, &context, EditorEvent::Key(KeyEvent::key(Key::Tab)));
    let Mode::Completion(CompletionMode { state, .. }) = &editor.mode else { panic!("completion did not open") };
    let mut names: Vec<&str> = (0..state.comp.len()).map(|index| state.comp.name(index)).collect();
    names.sort_unstable();
    let expected: Vec<String> = (0..64).map(|branch| format!("branch{branch:02}/item{branch:02}")).collect();
    assert_eq!(names, expected);
    std::fs::remove_dir_all(root).unwrap();
}
