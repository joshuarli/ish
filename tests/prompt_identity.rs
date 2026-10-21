use ish::prompt::Prompt;

#[test]
fn prompt_identity_replaces_controls_on_construction() {
    let mut prompt = Prompt::with_identity("Zoë\n\r\t\x1b]52;c;x\x07\x7f\u{0085}", "");
    let mut rendered = String::new();
    prompt.render_into(&mut rendered, 0, "/", false);
    assert!(rendered.starts_with("Zoë����]52;c;x���@"), "{rendered:?}");
    let identity = rendered.split("\x1b[38;5;10m").next().unwrap();
    assert!(!identity.chars().any(char::is_control), "{identity:?}");
}

#[test]
fn prompt_identity_replaces_controls_after_update() {
    let mut prompt = Prompt::with_identity("initial", "");
    prompt.set_identity("更新\n\x1b[2J\u{009b}", "");
    let mut rendered = String::new();
    prompt.render_into(&mut rendered, 0, "/", false);
    assert!(rendered.starts_with("更新��[2J�@"), "{rendered:?}");
    let identity = rendered.split("\x1b[38;5;10m").next().unwrap();
    assert!(!identity.chars().any(char::is_control), "{identity:?}");
    prompt.set_identity("plain", "");
    prompt.render_into(&mut rendered, 0, "/", false);
    assert!(rendered.starts_with("plain@"));
}
