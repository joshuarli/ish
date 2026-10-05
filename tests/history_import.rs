use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn legacy_fish_import_refuses_migrated_storage_without_changing_files() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("ish_fish_import_{}_{stamp}", std::process::id()));
    let history_dir = home.join(".local/share/ish");
    let fish_dir = home.join(".local/share/fish");
    fs::create_dir_all(&history_dir).unwrap();
    fs::create_dir_all(&fish_dir).unwrap();
    fs::write(fish_dir.join("fish_history"), "- cmd: echo fish\n  when: 100\n").unwrap();
    fs::write(history_dir.join("history"), "echo original\n").unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/import-fish-history");
    for name in ["history.sqlite3", "history.log"] {
        fs::write(history_dir.join(name), b"storage marker").unwrap();
        let output = Command::new("bash").arg(&script).env("HOME", &home).output().unwrap();
        assert!(!output.status.success(), "legacy importer reported success after migration");
        assert!(String::from_utf8_lossy(&output.stderr).contains("before history migration"));
        assert_eq!(fs::read(history_dir.join(name)).unwrap(), b"storage marker");
        fs::remove_file(history_dir.join(name)).unwrap();
    }
    let legacy = fs::read_to_string(history_dir.join("history")).unwrap();
    fs::remove_dir_all(home).unwrap();
    assert_eq!(legacy, "echo original\n");
}

#[test]
fn sqlite_migration_preserves_occurrences_and_refuses_to_overwrite_log() {
    use ish::history::History;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("ish_sqlite_import_{}_{stamp}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let database = dir.join("history.sqlite3");
    let fixture = r#"
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.executescript('PRAGMA user_version=1; CREATE TABLE history_state(singleton, generation, migrated); INSERT INTO history_state VALUES(1,3,1); CREATE TABLE occurrences(id,command,timestamp,session_id,cwd);')
    for id, command, timestamp in [(1,'echo café',100),(4,'echo café',200),(5,'echo newest',300)]:
        db.execute('INSERT INTO occurrences VALUES(?,?,?,?,?)', (id,command,timestamp,(2**64-1).to_bytes(8,'little'),b'/work/\xff'))
"#;
    let output = Command::new("python3").arg("-c").arg(fixture).arg(&database).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let original = fs::read(&database).unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/migrate-sqlite-history");
    let run = || Command::new("python3").arg(&script).arg(&database).output().unwrap();
    let output = run();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read(&database).unwrap(), original);
    let mut history = History::load_from(dir.join("history")).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history.frequency(0), 2);
    assert_eq!(history.timestamp(0), 200);
    assert_eq!(history.get(1), "echo newest");
    history.add("echo after migration").unwrap();
    let log = fs::read(dir.join("history.log")).unwrap();
    let output = run();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert_eq!(fs::read(dir.join("history.log")).unwrap(), log);
    assert_eq!(fs::read(&database).unwrap(), original);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sqlite_migration_rejects_invalid_sources_without_publishing_a_log() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("ish_sqlite_invalid_{}_{stamp}", std::process::id()));
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/migrate-sqlite-history");
    for version in [1, 2] {
        let dir = root.join(version.to_string());
        fs::create_dir_all(&dir).unwrap();
        let database = dir.join("history.sqlite3");
        let fixture = r#"
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.executescript('CREATE TABLE history_state(singleton,generation,migrated); INSERT INTO history_state VALUES(1,0,1); CREATE TABLE occurrences(id,command,timestamp,session_id,cwd);')
    db.execute('PRAGMA user_version='+sys.argv[2])
    db.execute('INSERT INTO occurrences VALUES(1,?,100,?,NULL)', ('valid prefix',(7).to_bytes(8,'little')))
    db.execute('INSERT INTO occurrences VALUES(2,?,200,?,NULL)', ('',(7).to_bytes(8,'little')))
"#;
        let output = Command::new("python3").arg("-c").arg(fixture).arg(&database)
            .arg(version.to_string()).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let original = fs::read(&database).unwrap();
        let output = Command::new("python3").arg(&script).arg(&database).output().unwrap();
        assert!(!output.status.success());
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(message.contains(if version == 1 { "invalid history command" } else { "unsupported history database schema" }), "{message}");
        assert!(!dir.join("history.log").exists());
        assert_eq!(fs::read(&database).unwrap(), original);
        assert!(!fs::read_dir(&dir).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(".history.log-")));
    }
    fs::remove_dir_all(root).unwrap();
}
