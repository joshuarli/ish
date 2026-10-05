use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn legacy_fish_import_refuses_an_existing_database_without_changing_files() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("ish_fish_import_{}_{stamp}", std::process::id()));
    let history_dir = home.join(".local/share/ish");
    let fish_dir = home.join(".local/share/fish");
    fs::create_dir_all(&history_dir).unwrap();
    fs::create_dir_all(&fish_dir).unwrap();
    fs::write(fish_dir.join("fish_history"), "- cmd: echo fish\n  when: 100\n").unwrap();
    fs::write(history_dir.join("history"), "echo original\n").unwrap();
    fs::write(history_dir.join("history.sqlite3"), b"database marker").unwrap();

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/import-fish-history");
    let output = Command::new("bash").arg(script).env("HOME", &home).output().unwrap();
    let legacy = fs::read_to_string(history_dir.join("history")).unwrap();
    let database = fs::read(history_dir.join("history.sqlite3")).unwrap();
    fs::remove_dir_all(home).unwrap();

    assert!(!output.status.success(), "legacy importer reported success after migration");
    assert!(String::from_utf8_lossy(&output.stderr).contains("before SQLite migration"));
    assert_eq!(legacy, "echo original\n");
    assert_eq!(database, b"database marker");
}
