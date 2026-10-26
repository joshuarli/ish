use super::*;
use crate::input::{Key, KeyEvent};

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
