use super::resolve_cd_for_history;

#[test]
fn tab_separated_cd_history_target_is_absolute() {
    let cwd = std::env::current_dir().unwrap();
    let expected = cwd.join("child");

    assert_eq!(
        resolve_cd_for_history("cd\tchild && pwd"),
        format!("cd {} && pwd", expected.display())
    );
    assert_eq!(
        resolve_cd_for_history("z\tchild"),
        format!("z {}", expected.display())
    );

    let unicode_expected = cwd.join("子");
    assert_eq!(
        resolve_cd_for_history("cd\t子 && pwd"),
        format!("cd {} && pwd", unicode_expected.display())
    );
}
