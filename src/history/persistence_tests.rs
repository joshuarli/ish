use super::*;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("ish-history-{}-{}-{}",
            std::process::id(), now_millis(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf { self.0.join("history") }
    fn connection(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(store::database_path(&self.path())).unwrap()
    }
    fn occurrences(&self) -> i64 {
        self.connection().query_row("SELECT COUNT(*) FROM occurrences", [], |row| row.get(0)).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); }
}

fn record(timestamp: u64, command: &str) -> String {
    format!(":ish-history:v2\t{timestamp}\t7\t/work/project\t{command}\n")
}

fn cache_fixture(version: u8, commands: &[&str], timestamps: &[u64]) -> Vec<u8> {
    const EPOCH: u64 = 883_612_800_000;
    let arena: String = if version <= 2 { commands.concat() }
        else { commands.iter().map(|command| format!("{command}\0")).collect() };
    let cwd_arena: String = commands.iter().map(|_| "/cached\0").collect();
    let mut bytes = vec![b'I', b'S', b'H', version];
    if version <= 2 { bytes.extend_from_slice(&0u64.to_le_bytes()); }
    bytes.extend_from_slice(&(commands.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(arena.len() as u32).to_le_bytes());
    if version == 5 { bytes.extend_from_slice(&(cwd_arena.len() as u32).to_le_bytes()); }
    if version <= 2 { for _ in commands { bytes.extend_from_slice(&0u64.to_le_bytes()); } }
    for &timestamp in timestamps {
        match version {
            1 => {},
            2 => bytes.extend_from_slice(&((timestamp / 1000) as u32).to_le_bytes()),
            3 => bytes.extend_from_slice(&(((timestamp-EPOCH) / 1000) as u32).to_le_bytes()),
            _ => bytes.extend_from_slice(&timestamp.wrapping_sub(EPOCH).to_le_bytes()),
        }
    }
    if version <= 2 {
        let mut start = 0u32;
        for command in commands {
            bytes.extend_from_slice(&start.to_le_bytes());
            bytes.extend_from_slice(&(command.len() as u16).to_le_bytes());
            start += command.len() as u32;
        }
    }
    bytes.extend_from_slice(arena.as_bytes());
    if version == 5 { bytes.extend_from_slice(cwd_arena.as_bytes()); }
    bytes
}

#[test]
fn migrates_every_cache_version_without_changing_originals() {
    for version in 1..=5 {
        let fixture = Fixture::new();
        let timestamp = 1_700_000_000_000;
        let bytes = cache_fixture(version, &["cached", "repeated"], &[timestamp, timestamp]);
        let text = record(timestamp+1000, "repeated") + &record(timestamp+2000, "repeated");
        fs::write(legacy::cache_path(&fixture.path()), &bytes).unwrap();
        fs::write(fixture.path(), &text).unwrap();
        let history = History::load_from(fixture.path()).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(fixture.occurrences(), 4);
        let repeated = history.find_entry_index(hash_str("repeated"), "repeated").unwrap();
        assert_eq!(history.frequency(repeated), 3);
        assert_eq!(history.timestamp(repeated), timestamp+2000);
        assert_eq!(history.usages[repeated].directories[Path::new("/work/project")].count, 2);
        assert_eq!(fs::read(fixture.path()).unwrap(), text.as_bytes());
        assert_eq!(fs::read(legacy::cache_path(&fixture.path())).unwrap(), bytes);
        drop(history);
        let reopened = History::load_from(fixture.path()).unwrap();
        assert_eq!(reopened.frequency(repeated), 3);
        assert_eq!(fixture.occurrences(), 4);
    }
}

#[test]
fn migration_prefers_newest_metadata_and_does_not_double_count_flush_copy() {
    let fixture = Fixture::new();
    let timestamp = 1_700_000_000_000;
    let bytes = cache_fixture(5, &["same", "newer-cache"], &[timestamp, timestamp+2000]);
    fs::write(legacy::cache_path(&fixture.path()), bytes).unwrap();
    fs::write(fixture.path(), record(timestamp, "same") + &record(timestamp+1000, "newer-cache")).unwrap();
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(fixture.occurrences(), 3);
    let idx = history.find_entry_index(hash_str("newer-cache"), "newer-cache").unwrap();
    assert_eq!(history.timestamp(idx), timestamp+2000);
    assert_eq!(history.usages[idx].directories[Path::new("/cached")].timestamp, timestamp+2000);
}

#[test]
fn migration_corruption_rolls_back_and_retry_imports_once() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "preserve me\n").unwrap();
    fs::write(legacy::cache_path(&fixture.path()), b"ISH\x05bad").unwrap();
    assert!(History::load_from(fixture.path()).is_err());
    assert_eq!(fs::read(fixture.path()).unwrap(), b"preserve me\n");
    fs::remove_file(legacy::cache_path(&fixture.path())).unwrap();
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(history.get(0), "preserve me");
    assert_eq!(fixture.occurrences(), 1);
    fs::write(fixture.path(), "unimported legacy append\n").unwrap();
    fs::write(legacy::cache_path(&fixture.path()), b"corrupt after migration").unwrap();
    let reopened = History::load_from(fixture.path()).unwrap();
    assert_eq!(reopened.get(0), "preserve me");
    assert_eq!(reopened.len(), 1);
}

#[test]
fn malformed_structured_text_and_utf8_offsets_are_reported() {
    let fixture = Fixture::new();
    for text in [b":ish-history:v2\tbad\n".as_slice(), &[0xff, b'\n']] {
        fs::write(fixture.path(), text).unwrap();
        assert!(History::load_from(fixture.path()).is_err());
    }
    fs::write(fixture.path(), "okay\n").unwrap();
    let mut bytes = cache_fixture(1, &["é"], &[0]);
    // The cache offset starts in the middle of a UTF-8 code point.
    bytes[28..32].copy_from_slice(&1u32.to_le_bytes());
    bytes[32..34].copy_from_slice(&1u16.to_le_bytes());
    fs::write(legacy::cache_path(&fixture.path()), bytes).unwrap();
    assert!(History::load_from(fixture.path()).is_err());
}

#[test]
fn simultaneous_open_migrates_once_and_appends_without_lost_occurrences() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "seed\n").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = (0..2).map(|session| {
        let path = fixture.path();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            let mut history = History::load_from(path).unwrap();
            for i in 0..20 { history.add_in_dir(&format!("session {session} {i}"), None).unwrap(); }
        })
    }).collect();
    barrier.wait();
    for thread in threads { thread.join().unwrap(); }
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(history.len(), 41);
    assert_eq!(fixture.occurrences(), 41);
}

#[test]
fn session_recall_stays_fixed_while_global_usage_synchronizes() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "first\nsecond\n").unwrap();
    let mut local = History::load_from(fixture.path()).unwrap();
    let mut other = History::load_from(fixture.path()).unwrap();
    let first_idx = local.find_entry_index(hash_str("first"), "first").unwrap();
    other.add_in_dir("first", Some(Path::new("/elsewhere"))).unwrap();
    other.add_in_dir("foreign", None).unwrap();
    local.sync().unwrap();
    assert_eq!(local.session_get(0), Some("second"));
    assert_eq!(local.session_get(1), Some("first"));
    assert_eq!(local.session_get(2), None);
    assert_eq!(local.get(first_idx), "first");
    assert_eq!(local.frequency(first_idx), 2);
    assert_eq!(local.fuzzy_search("foreign").len(), 1);
    local.add_in_dir("first", None).unwrap();
    assert_eq!(local.session_get(0), Some("first"));
    assert_eq!(local.session_get(1), Some("second"));
    assert_eq!(local.session_prefix_search("foreign", 0), None);
    other.add_in_dir("unsynced", None).unwrap();
    local.add_in_dir("mine", None).unwrap();
    assert_eq!(local.fuzzy_search("unsynced").len(), 1);
    assert_eq!(local.session_get(0), Some("mine"));
    assert_eq!(fixture.occurrences(), 7);
}

#[test]
fn reset_invalidates_stale_sessions_before_their_next_append() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "legacy\n").unwrap();
    let mut resetting = History::load_from(fixture.path()).unwrap();
    let mut stale = History::load_from(fixture.path()).unwrap();
    resetting.add_in_dir("old", None).unwrap();
    resetting.reset().unwrap();
    stale.add_in_dir("after reset", None).unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale.session_get(0), Some("after reset"));
    resetting.sync().unwrap();
    assert_eq!(resetting.len(), 1);
    assert_eq!(resetting.session_get(0), None);
    resetting.compact().unwrap();
    assert_eq!(fixture.occurrences(), 1);
    assert_eq!(fs::read(fixture.path()).unwrap(), b"legacy\n");
    let reopened = History::load_from(fixture.path()).unwrap();
    assert_eq!(reopened.get(0), "after reset");
}

#[test]
fn append_failure_does_not_update_memory_or_usage() {
    let fixture = Fixture::new();
    let mut history = History::load_from(fixture.path()).unwrap();
    history.add_in_dir("saved", None).unwrap();
    fixture.connection().execute_batch("CREATE TRIGGER reject_append BEFORE INSERT ON occurrences
        BEGIN SELECT RAISE(ABORT, 'test write failure'); END;").unwrap();
    assert!(history.add_in_dir("unsaved", None).is_err());
    assert_eq!(history.len(), 1);
    assert_eq!(history.session_get(0), Some("saved"));
    assert_eq!(history.frequency(0), 1);
    assert_eq!(fixture.occurrences(), 1);
}

#[test]
fn cwd_bytes_and_full_unicode_commands_round_trip() {
    let fixture = Fixture::new();
    let mut history = History::load_from(fixture.path()).unwrap();
    let cwd = PathBuf::from(std::ffi::OsString::from_vec(b"/work/\xff\t\\".to_vec()));
    let command = "é".repeat(70_000) + " target";
    history.add_in_dir(&command, Some(&cwd)).unwrap();
    drop(history);
    let loaded = History::load_from(fixture.path()).unwrap();
    assert_eq!(loaded.get(0), command);
    assert_eq!(loaded.usages[0].directories.keys().next().unwrap().as_os_str().as_bytes(), cwd.as_os_str().as_bytes());
    let matches = loaded.fuzzy_search("target");
    assert_eq!(matches[0].match_positions[..6], [70_001, 70_002, 70_003, 70_004, 70_005, 70_006]);
    let mut limited = Vec::new();
    loaded.fuzzy_search_into("target", &mut limited, 1, "");
    assert_eq!(limited[0].match_positions, matches[0].match_positions);
}

#[test]
fn read_only_render_matches_actual_time_order_without_mutating_store() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), record(300, "later") + &record(100, "earlier") + &record(200, "later")).unwrap();
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(render_history_database(history.database_path().unwrap()).unwrap(), "earlier\nlater\n");
    let mut indices = Vec::new();
    history.command_indices_into(&mut indices);
    assert_eq!(indices.iter().map(|&idx| history.get(idx)).collect::<Vec<_>>(), ["earlier", "later"]);
    assert_eq!(fixture.occurrences(), 3);
}

#[test]
fn separate_legacy_names_have_independent_stores() {
    let fixture = Fixture::new();
    let a_path = fixture.0.join("a");
    let b_path = fixture.0.join("b");
    fs::write(&a_path, "only a\n").unwrap();
    fs::write(&b_path, "only b\n").unwrap();
    let mut a = History::load_from(a_path).unwrap();
    let mut b = History::load_from(b_path).unwrap();
    assert_ne!(a.database_path(), b.database_path());
    a.add_in_dir("new a", None).unwrap();
    b.sync().unwrap();
    assert_eq!(b.len(), 1);
    a.reset().unwrap();
    b.add_in_dir("new b", None).unwrap();
    a.sync().unwrap();
    assert_eq!(a.len(), 0);
    assert_eq!(b.len(), 2);
}

#[test]
fn database_permissions_and_durability_are_explicit() {
    let fixture = Fixture::new();
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(fs::metadata(history.database_path().unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
    let connection = fixture.connection();
    let journal: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
    assert_eq!(journal, "wal");
    let schema: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(schema, 1);
}

#[test]
fn future_schema_is_refused_without_import() {
    let fixture = Fixture::new();
    let connection = rusqlite::Connection::open(store::database_path(&fixture.path())).unwrap();
    connection.execute_batch("PRAGMA user_version=2;").unwrap();
    fs::write(fixture.path(), "must not import\n").unwrap();
    assert!(History::load_from(fixture.path()).is_err());
    assert_eq!(connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
}

fn use_command(history: &mut History, command: &str, timestamp: u64, cwd: Option<&Path>) {
    let id = history.last_id + 1;
    history.apply_occurrence(store::Occurrence { id, command: command.into(), timestamp,
        session_id: 0, cwd: cwd.map(Path::to_path_buf) });
}

#[test]
fn match_quality_precedes_exact_directory_and_frequency() {
    let mut history = History::from_entries(vec!["cargo build".into()]);
    for _ in 0..50 { use_command(&mut history, "echo cargo", now_millis(), Some(Path::new("/work"))); }
    let results = history.fuzzy_search_in_dir("cargo", Path::new("/work"));
    assert_eq!(history.get(results[0].entry_idx), "cargo build");
    assert_eq!(results[0].score, 3);
}

#[test]
fn directory_proximity_precedes_scoped_frequency() {
    let mut history = History::from_entries(Vec::new());
    let now = now_millis();
    use_command(&mut history, "cargo exact", now, Some(Path::new("/work/project/src")));
    use_command(&mut history, "cargo ancestor", now, Some(Path::new("/work/project")));
    use_command(&mut history, "cargo distant", now, Some(Path::new("/work")));
    for _ in 0..500 { use_command(&mut history, "cargo unrelated", now, Some(Path::new("/work/projects"))); }
    let results = history.fuzzy_search_in_dir("cargo", Path::new("/work/project/src"));
    assert_eq!(results.iter().map(|entry| history.get(entry.entry_idx)).collect::<Vec<_>>(),
        ["cargo exact", "cargo ancestor", "cargo distant", "cargo unrelated"]);
    let distant = history.find_entry_index(hash_str("cargo distant"), "cargo distant").unwrap();
    for _ in 0..100 { use_command(&mut history, "cargo distant", now, Some(Path::new("/elsewhere"))); }
    assert_eq!(history.context_usage(distant, Some(Path::new("/work/project/src"))).1.count, 1);
}

#[test]
fn recent_usage_beats_stale_popularity_but_frequency_matters_when_recent() {
    let mut history = History::from_entries(Vec::new());
    let now = now_millis();
    for _ in 0..1024 { use_command(&mut history, "cargo stale", now-90*24*3_600_000, None); }
    for _ in 0..8 { use_command(&mut history, "cargo frequent", now-1000, None); }
    use_command(&mut history, "cargo fresh", now, None);
    let matches = history.fuzzy_search("cargo");
    assert_eq!(matches.iter().map(|entry| history.get(entry.entry_idx)).collect::<Vec<_>>(),
        ["cargo frequent", "cargo fresh", "cargo stale"]);
    let empty = history.fuzzy_search("");
    assert_eq!(history.get(empty[0].entry_idx), "cargo fresh");
}

#[test]
fn subset_preserves_every_match_and_matches_full_top_k_on_refinement() {
    let mut history = History::from_entries(Vec::new());
    for _ in 0..16 { use_command(&mut history, "cargo older special", now_millis()-1000, None); }
    for i in 0..350 { use_command(&mut history, &format!("cargo newest {i}"), now_millis(), None); }
    let mut candidates = Vec::new();
    history.search_entry_indices_into(&mut candidates);
    let mut matches = Vec::new();
    let mut results = Vec::new();
    history.fuzzy_search_subset_into("cargo", &candidates, &mut matches, &mut results, 200);
    assert_eq!(matches.len(), 351);
    let full = history.fuzzy_search("cargo");
    assert_eq!(results.iter().map(|entry| entry.entry_idx).collect::<Vec<_>>(),
        full[..200].iter().map(|entry| entry.entry_idx).collect::<Vec<_>>());
    assert_eq!(history.get(results[0].entry_idx), "cargo older special");
    let mut refined = Vec::new();
    history.fuzzy_search_subset_into("cargo older", &matches, &mut refined, &mut results, 200);
    assert_eq!(refined.len(), 1);
    assert_eq!(history.get(results[0].entry_idx), "cargo older special");
}

#[test]
fn unicode_scalar_lowercasing_and_long_subsequence_queries_keep_positions() {
    let history = History::from_entries(vec!["İstanbul".into(), "é cafe".into()]);
    assert_eq!(history.get(history.fuzzy_search("İ")[0].entry_idx), "İstanbul");
    assert_eq!(history.get(history.fuzzy_search("i")[0].entry_idx), "İstanbul");
    assert_eq!(history.fuzzy_search("ca")[0].match_positions[..2], [2, 3]);
    let query = "a".repeat(40);
    let text = "a-".repeat(40);
    let matches = History::from_entries(vec![text]).fuzzy_search(&query);
    assert_eq!(matches[0].match_count, 32);
    assert_eq!(matches[0].match_positions[31], 62);
}

#[test]
fn context_lookup_does_not_scan_historical_directories() {
    let mut history = History::from_entries(Vec::new());
    for i in 0..6000 { use_command(&mut history, "cargo popular", now_millis(),
        Some(&PathBuf::from(format!("/elsewhere/{i}")))); }
    use_command(&mut history, "cargo popular", now_millis(), Some(Path::new("/work")));
    for i in 0..1000 { use_command(&mut history, &format!("cargo {i}"), now_millis(), None); }
    let started = std::time::Instant::now();
    let mut results = Vec::new();
    history.fuzzy_search_into_in_dir("cargo", &mut results, 200, Path::new("/work/src"));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(history.get(results[0].entry_idx), "cargo popular");
}

#[test]
fn startup_and_incremental_sync_merge_large_overlapping_uses_promptly() {
    let fixture = Fixture::new();
    let commands: Vec<String> = (0..6000).map(|i| format!("echo {i}")).collect();
    let refs: Vec<&str> = commands.iter().map(String::as_str).collect();
    let timestamps = vec![1_700_000_000_000; refs.len()];
    fs::write(legacy::cache_path(&fixture.path()), cache_fixture(5, &refs, &timestamps)).unwrap();
    let text: String = commands.iter().map(|command| record(1_700_000_001_000, command)).collect();
    fs::write(fixture.path(), text).unwrap();
    let started = std::time::Instant::now();
    let mut history = History::load_from(fixture.path()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(history.len(), 6000);
    assert_eq!(fixture.occurrences(), 12_000);
    let connection = fixture.connection();
    connection.execute_batch("BEGIN IMMEDIATE;
        INSERT INTO occurrences(command,timestamp,session_id,cwd)
        SELECT command,timestamp+1,session_id,cwd FROM occurrences;
        COMMIT;").unwrap();
    let started = std::time::Instant::now();
    history.sync().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(history.len(), 6000);
    for idx in 0..history.len() { assert_eq!(history.frequency(idx), 4); }
}
