use epsh::eval::Shell;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

fn capture(source: &str, cwd: PathBuf) -> (i32, Vec<u8>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut shell = Shell::builder()
        .cwd(cwd)
        .stdout_sink(output.clone())
        .build();
    let status = shell.run_script(source);
    (status, output.lock().unwrap().clone())
}

#[test]
fn literal_control_escape_survives_word_and_variable_expansion() {
    let marker = '\u{fffe}';
    let expected = format!("a{marker}b").into_bytes();
    let cwd = std::env::current_dir().unwrap();
    assert!(epsh::glob::has_glob_chars(&format!("{marker}*")));

    for source in [
        format!("printf '%s' 'a{marker}b'"),
        format!("printf '%s' a{marker}b"),
        format!("value='a{marker}b'; printf '%s' \"$value\""),
    ] {
        let (status, output) = capture(&source, cwd.clone());
        assert_eq!(status, 0, "{source}");
        assert_eq!(output, expected, "{source}");
    }
}

#[test]
fn literal_control_escape_in_glob_pattern_matches_filename_bytes() {
    let directory = tempdir().unwrap();
    let marker = '\u{fffe}';
    let names = [format!("a{marker}b"), format!("a{marker}c")];
    for name in &names {
        fs::write(directory.path().join(name), b"").unwrap();
    }

    let (status, output) = capture(
        &format!("printf '<%s>' a{marker}*"),
        directory.path().to_path_buf(),
    );

    assert_eq!(status, 0);
    assert_eq!(output, format!("<{}><{}>", names[0], names[1]).into_bytes());
}
