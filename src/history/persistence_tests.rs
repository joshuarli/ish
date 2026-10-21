use super::*;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ish-history-{}-{}-{}",
            std::process::id(),
            now_millis(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("history")
    }
    fn occurrences(&self) -> usize {
        store::Store::open(&self.path())
            .unwrap()
            .snapshot(-1, 0)
            .unwrap()
            .occurrences
            .len()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn record(timestamp: u64, command: &str) -> String {
    format!(":ish-history:v2\t{timestamp}\t7\t/work/project\t{command}\n")
}

fn cache_fixture(commands: &[&str], timestamps: &[u64]) -> Vec<u8> {
    const EPOCH: u64 = 883_612_800_000;
    let arena: String = commands
        .iter()
        .map(|command| format!("{command}\0"))
        .collect();
    let cwd_arena: String = commands.iter().map(|_| "/cached\0").collect();
    let mut bytes = b"ISH\x05".to_vec();
    bytes.extend_from_slice(&(commands.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(arena.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(cwd_arena.len() as u32).to_le_bytes());
    for &timestamp in timestamps {
        bytes.extend_from_slice(&timestamp.wrapping_sub(EPOCH).to_le_bytes());
    }
    bytes.extend_from_slice(arena.as_bytes());
    bytes.extend_from_slice(cwd_arena.as_bytes());
    bytes
}

#[test]
fn deleting_the_log_after_migration_does_not_resurrect_legacy_history() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), record(1_700_000_000_000, "secret text")).unwrap();
    fs::write(
        legacy::cache_path(&fixture.path()),
        cache_fixture(&["secret cache"], &[1_700_000_000_000]),
    )
    .unwrap();
    assert_eq!(fixture.occurrences(), 2);
    fs::remove_file(store::storage_path(&fixture.path())).unwrap();
    assert_eq!(fixture.occurrences(), 0);
    assert!(!fixture.path().exists());
    assert!(!legacy::cache_path(&fixture.path()).exists());
}

#[test]
fn migrates_cache_and_retires_originals() {
    {
        let fixture = Fixture::new();
        let timestamp = 1_700_000_000_000;
        let bytes = cache_fixture(&["cached", "repeated"], &[timestamp, timestamp]);
        let text = record(timestamp + 1000, "repeated") + &record(timestamp + 2000, "repeated");
        fs::write(legacy::cache_path(&fixture.path()), &bytes).unwrap();
        fs::write(fixture.path(), &text).unwrap();
        let history = History::load_from(fixture.path()).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(fixture.occurrences(), 4);
        let repeated = history
            .find_entry_index(hash_str("repeated"), "repeated")
            .unwrap();
        assert_eq!(history.frequency(repeated), 3);
        assert_eq!(history.timestamp(repeated), timestamp + 2000);
        assert_eq!(
            history.usages[repeated].directories[Path::new("/work/project")].count,
            2
        );
        assert!(!fixture.path().exists());
        assert!(!legacy::cache_path(&fixture.path()).exists());
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
    let bytes = cache_fixture(&["same", "newer-cache"], &[timestamp, timestamp + 2000]);
    fs::write(legacy::cache_path(&fixture.path()), bytes).unwrap();
    fs::write(
        fixture.path(),
        record(timestamp, "same") + &record(timestamp + 1000, "newer-cache"),
    )
    .unwrap();
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(fixture.occurrences(), 3);
    let idx = history
        .find_entry_index(hash_str("newer-cache"), "newer-cache")
        .unwrap();
    assert_eq!(history.timestamp(idx), timestamp + 2000);
    assert_eq!(
        history.usages[idx].directories[Path::new("/cached")].timestamp,
        timestamp + 2000
    );
}

#[test]
fn migration_corruption_rolls_back_and_retry_imports_once() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "preserve me\n").unwrap();
    fs::write(legacy::cache_path(&fixture.path()), b"ISH\x05bad").unwrap();
    assert!(History::load_from(fixture.path()).is_err());
    assert_eq!(fs::read(fixture.path()).unwrap(), b"preserve me\n");
    assert!(!store::storage_path(&fixture.path()).exists());
    fs::remove_file(legacy::cache_path(&fixture.path())).unwrap();
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(history.get(0), "preserve me");
    assert_eq!(fixture.occurrences(), 1);
    fs::write(fixture.path(), "unimported legacy append\n").unwrap();
    fs::write(
        legacy::cache_path(&fixture.path()),
        b"corrupt after migration",
    )
    .unwrap();
    let reopened = History::load_from(fixture.path()).unwrap();
    assert_eq!(reopened.get(0), "preserve me");
    assert_eq!(reopened.len(), 1);
}

#[test]
fn malformed_structured_text_and_cache_are_reported() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), b":ish-history:v2\tbad\n").unwrap();
    assert!(History::load_from(fixture.path()).is_err());
    fs::write(fixture.path(), "okay\n").unwrap();
    let mut bytes = cache_fixture(&["cached"], &[0]);
    // The command arena is not valid UTF-8.
    let arena = bytes.len() - "cached\0/cached\0".len();
    bytes[arena] = 0xff;
    fs::write(legacy::cache_path(&fixture.path()), bytes).unwrap();
    assert!(History::load_from(fixture.path()).is_err());
}

#[test]
fn non_utf8_legacy_text_does_not_abort_import() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), b"echo ok\necho \xff\n").unwrap();
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history.get(1), "echo \u{fffd}");
}

#[test]
fn simultaneous_open_migrates_once_and_appends_without_lost_occurrences() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "seed\n").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|session| {
            let path = fixture.path();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut history = History::load_from(path).unwrap();
                for i in 0..20 {
                    history
                        .add_in_dir(&format!("session {session} {i}"), None)
                        .unwrap();
                }
            })
        })
        .collect();
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }
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
    other
        .add_in_dir("first", Some(Path::new("/elsewhere")))
        .unwrap();
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
    assert_eq!(fixture.occurrences(), 1);
    assert!(!fixture.path().exists());
    let reopened = History::load_from(fixture.path()).unwrap();
    assert_eq!(reopened.get(0), "after reset");
}

#[test]
fn append_failure_does_not_update_memory_or_usage() {
    let fixture = Fixture::new();
    let mut history = History::load_from(fixture.path()).unwrap();
    history.add_in_dir("saved", None).unwrap();
    let path = store::storage_path(&fixture.path());
    let saved = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(history.add_in_dir("unsaved", None).is_err());
    assert_eq!(history.len(), 1);
    assert_eq!(history.session_get(0), Some("saved"));
    assert_eq!(history.frequency(0), 1);
    fs::remove_dir(&path).unwrap();
    fs::write(&path, saved).unwrap();
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
    assert_eq!(
        loaded.usages[0]
            .directories
            .keys()
            .next()
            .unwrap()
            .as_os_str()
            .as_bytes(),
        cwd.as_os_str().as_bytes()
    );
    let matches = loaded.fuzzy_search("target");
    assert_eq!(
        matches[0].match_positions[..6],
        [70_001, 70_002, 70_003, 70_004, 70_005, 70_006]
    );
    let mut limited = Vec::new();
    let mut candidates = Vec::new();
    loaded.search_entry_indices_into(&mut candidates);
    loaded.fuzzy_search_subset_into_in_dir(
        "target",
        &candidates,
        &mut Vec::new(),
        &mut limited,
        1,
        Path::new("/"),
    );
    assert_eq!(limited[0].match_positions, matches[0].match_positions);
}

#[test]
fn read_only_render_matches_actual_time_order_without_mutating_store() {
    let fixture = Fixture::new();
    fs::write(
        fixture.path(),
        record(300, "later") + &record(100, "earlier") + &record(200, "later"),
    )
    .unwrap();
    let history = History::load_from(fixture.path()).unwrap();
    assert_eq!(
        render_history_log(history.storage_path().unwrap()).unwrap(),
        "earlier\nlater\n"
    );
    let mut indices = Vec::new();
    history.command_indices_into(&mut indices);
    assert_eq!(
        indices
            .iter()
            .map(|&idx| history.get(idx))
            .collect::<Vec<_>>(),
        ["earlier", "later"]
    );
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
    assert_ne!(a.storage_path(), b.storage_path());
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
fn log_and_lock_permissions_are_private() {
    let fixture = Fixture::new();
    let history = History::load_from(fixture.path()).unwrap();
    for path in [
        history.storage_path().unwrap().to_path_buf(),
        fixture.0.join("history.log.lock"),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        &fs::read(history.storage_path().unwrap()).unwrap()[..8],
        b"ISHLOG\0\x01"
    );
}

#[test]
fn unsupported_or_corrupt_log_headers_are_preserved() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "must not import\n").unwrap();
    for bytes in [
        [b"ISHLOG\0\x02".as_slice(), &0i64.to_le_bytes()].concat(),
        [b"ISHLOG\0\x00".as_slice(), &0i64.to_le_bytes()].concat(),
        b"ISHLOG\0\x01".to_vec(),
        [b"ISHLOG\0\x01".as_slice(), &(-1i64).to_le_bytes()].concat(),
    ] {
        fs::write(store::storage_path(&fixture.path()), &bytes).unwrap();
        assert!(History::load_from(fixture.path()).is_err());
        assert_eq!(
            fs::read(store::storage_path(&fixture.path())).unwrap(),
            bytes
        );
    }
}

#[test]
fn incomplete_final_frames_recover_the_valid_prefix() {
    for tail in [vec![1, 2, 3], {
        let length = 100u64.to_le_bytes();
        let mut tail = length.to_vec();
        tail.extend_from_slice(&store::crc32(&length).to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes());
        tail.extend_from_slice(b"partial");
        tail
    }] {
        let fixture = Fixture::new();
        let mut history = History::load_from(fixture.path()).unwrap();
        history.add_in_dir("saved", None).unwrap();
        let path = store::storage_path(&fixture.path());
        let original = fs::read(&path).unwrap();
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&tail)
            .unwrap();
        history.sync().unwrap();
        assert_eq!(fs::read(&path).unwrap()[16..], original[16..]);
        history.add_in_dir("next", None).unwrap();
        assert_eq!(fixture.occurrences(), 2);
    }
}

fn two_frame_log(fixture: &Fixture) -> (PathBuf, Vec<u8>, usize) {
    let mut history = History::load_from(fixture.path()).unwrap();
    history.add_in_dir("first", None).unwrap();
    let path = store::storage_path(&fixture.path());
    let first_end = fs::read(&path).unwrap().len();
    history.add_in_dir("second", None).unwrap();
    let bytes = fs::read(&path).unwrap();
    (path, bytes, first_end)
}

#[test]
fn damaged_final_frame_is_a_torn_tail() {
    // Payload bytes, then the stored checksum: all mismatch at EOF.
    for offset in [12, 16, 24, 28, 32, 40] {
        let fixture = Fixture::new();
        let (path, mut bytes, first_end) = two_frame_log(&fixture);
        bytes[first_end + offset] ^= 0x80;
        fs::write(&path, &bytes).unwrap();
        let history = History::load_from(fixture.path()).unwrap();
        assert_eq!(history.len(), 1, "offset {offset}");
        assert_eq!(history.get(0), "first");
        assert_eq!(fs::read(&path).unwrap().len(), first_end);
    }
}

#[test]
fn recovering_an_observed_final_frame_invalidates_existing_readers() {
    let fixture = Fixture::new();
    let (path, mut bytes, first_end) = two_frame_log(&fixture);
    let mut observer = History::load_from(fixture.path()).unwrap();
    assert_eq!(observer.len(), 2);
    bytes[first_end + 40] ^= 0x80;
    fs::write(&path, &bytes).unwrap();

    let mut recovered = History::load_from(fixture.path()).unwrap();
    assert_eq!(recovered.len(), 1);
    recovered.add_in_dir("latest", None).unwrap();
    assert_eq!(fs::read(&path).unwrap().len(), bytes.len());

    observer.sync().unwrap();
    assert_eq!(observer.len(), 2);
    assert_eq!(observer.get(0), "first");
    assert_eq!(observer.get(1), "latest");
    assert_eq!(observer.prefix_search("second", 0), None);
    assert_eq!(observer.prefix_search("latest", 0), Some("latest"));
    assert_eq!(observer.session_get(0), None);
}

fn observed_final_frame_damage_invalidates_existing_readers(damage: impl Fn(&mut Vec<u8>, usize)) {
    let fixture = Fixture::new();
    let (path, mut bytes, first_end) = two_frame_log(&fixture);
    let mut observer = History::load_from(fixture.path()).unwrap();
    assert_eq!(observer.len(), 2);
    damage(&mut bytes, first_end);
    fs::write(&path, &bytes).unwrap();

    let mut recovered = History::load_from(fixture.path()).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered.generation, 1);
    recovered.add_in_dir("latest", None).unwrap();

    observer.sync().unwrap();
    assert_eq!(observer.len(), 2);
    assert_eq!(observer.get(0), "first");
    assert_eq!(observer.get(1), "latest");
    assert_eq!(observer.prefix_search("second", 0), None);
    assert_eq!(observer.prefix_search("latest", 0), Some("latest"));
}

#[test]
fn zeroed_final_frame_invalidates_existing_readers() {
    observed_final_frame_damage_invalidates_existing_readers(|bytes, first_end| {
        bytes[first_end..].fill(0);
    });
}

#[test]
fn overlong_final_frame_truncation_invalidates_existing_readers() {
    observed_final_frame_damage_invalidates_existing_readers(|bytes, first_end| {
        bytes.truncate(first_end + 24);
    });
}

#[test]
fn short_final_frame_truncation_invalidates_existing_readers() {
    observed_final_frame_damage_invalidates_existing_readers(|bytes, first_end| {
        bytes.truncate(first_end + 5);
    });
}

#[test]
fn recovering_an_observed_final_frame_publishes_a_new_generation() {
    let fixture = Fixture::new();
    let (path, mut bytes, first_end) = two_frame_log(&fixture);
    bytes[first_end + 40] ^= 0x80;
    fs::write(&path, &bytes).unwrap();
    let before = fs::read(&path).unwrap();
    let recovered = History::load_from(fixture.path()).unwrap();
    assert_eq!(recovered.generation, 1);
    let after = fs::read(&path).unwrap();
    assert_eq!(after[..8], before[..8]);
    assert_eq!(after[8..16], 1i64.to_le_bytes());
    assert_eq!(after[16..], before[16..first_end]);
}

#[test]
fn zero_fill_after_the_last_frame_is_a_torn_tail() {
    for fill in [1usize, 15, 16, 64, 20_000] {
        let fixture = Fixture::new();
        let (path, mut bytes, _) = two_frame_log(&fixture);
        let valid = bytes.len();
        bytes.resize(valid + fill, 0);
        fs::write(&path, &bytes).unwrap();
        let mut history = History::load_from(fixture.path()).unwrap();
        assert_eq!(history.len(), 2, "fill {fill}");
        assert_eq!(fs::read(&path).unwrap().len(), valid);
        history.add_in_dir("third", None).unwrap();
        assert_eq!(fixture.occurrences(), 3);
    }
}

#[test]
fn damage_before_the_last_frame_is_salvaged_loudly() {
    for offset in [12, 16, 24, 28, 32, 40] {
        let fixture = Fixture::new();
        let mut history = History::load_from(fixture.path()).unwrap();
        history.add_in_dir("first", None).unwrap();
        let first_end = fs::read(store::storage_path(&fixture.path())).unwrap().len();
        history.add_in_dir("second", None).unwrap();
        history.add_in_dir("third", None).unwrap();
        let path = store::storage_path(&fixture.path());
        let mut bytes = fs::read(&path).unwrap();
        bytes[first_end + offset] ^= 0x80;
        fs::write(&path, &bytes).unwrap();
        let reopened = History::load_from(fixture.path()).unwrap();
        assert_eq!(reopened.len(), 1, "offset {offset}");
        assert_eq!(reopened.get(0), "first");
        assert_eq!(fs::read(&path).unwrap().len(), first_end);
        let moved: Vec<_> = fs::read_dir(&fixture.0)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("history.log.corrupt-")
            })
            .collect();
        assert_eq!(moved.len(), 1);
        assert_eq!(
            fs::read(moved[0].path()).unwrap(),
            bytes[first_end..],
            "the discarded remainder is preserved"
        );
    }
}

#[test]
fn salvage_invalidates_existing_readers_and_reused_occurrence_cutoffs() {
    for keep_first in [false, true] {
        let fixture = Fixture::new();
        let mut writer = History::load_from(fixture.path()).unwrap();
        writer.add_in_dir("first", None).unwrap();
        let path = store::storage_path(&fixture.path());
        let first_end = fs::read(&path).unwrap().len();
        writer.add_in_dir("second", None).unwrap();
        writer.add_in_dir("third", None).unwrap();
        let mut syncing = History::load_from(fixture.path()).unwrap();
        let mut appending = History::load_from(fixture.path()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let retained_end = if keep_first { first_end } else { 16 };
        bytes[retained_end + 12] ^= 0x80;
        fs::write(&path, bytes).unwrap();

        let mut recovered = History::load_from(fixture.path()).unwrap();
        let retained_count = usize::from(keep_first);
        assert_eq!(recovered.len(), retained_count);
        syncing.sync().unwrap();
        assert_eq!(syncing.len(), retained_count);
        assert_eq!(syncing.prefix_search("second", 0), None);
        assert_eq!(syncing.prefix_search("third", 0), None);
        assert_eq!(syncing.session_get(0), None);
        if keep_first {
            assert_eq!(syncing.get(0), "first");
            assert_eq!(syncing.frequency(0), 1);
        }

        appending.add_in_dir("mine", None).unwrap();
        assert_eq!(appending.len(), retained_count + 1);
        assert_eq!(appending.session_get(0), Some("mine"));
        assert_eq!(appending.session_get(1), None);
        syncing.sync().unwrap();
        assert_eq!(syncing.prefix_search("mine", 0), Some("mine"));

        recovered.add_in_dir("external", None).unwrap();
        syncing.sync().unwrap();
        assert_eq!(syncing.prefix_search("external", 0), Some("external"));
        assert_eq!(syncing.len(), retained_count + 2);
        syncing.sync().unwrap();
        for idx in 0..syncing.len() {
            assert_eq!(syncing.frequency(idx), 1);
        }
        assert_eq!(fixture.occurrences(), retained_count + 2);
    }
}

#[test]
fn reset_works_on_a_log_with_damaged_records_or_header() {
    let fixture = Fixture::new();
    let (path, mut bytes, _) = two_frame_log(&fixture);
    let mut history = History::load_from(fixture.path()).unwrap();
    bytes[16 + 12] ^= 0x80;
    fs::write(&path, &bytes).unwrap();
    history.reset().unwrap();
    assert_eq!(fixture.occurrences(), 0);
    assert_eq!(History::load_from(fixture.path()).unwrap().len(), 0);

    fs::write(&path, b"not a history log at all").unwrap();
    history.reset().unwrap();
    assert_eq!(fixture.occurrences(), 0);
    let kept = fs::read_dir(&fixture.0)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("history.log.corrupt-")
        })
        .count();
    assert_eq!(kept, 1);
}

#[test]
fn commands_are_stored_verbatim_and_display_on_one_row() {
    let fixture = Fixture::new();
    let mut history = History::load_from(fixture.path()).unwrap();
    history.add_in_dir("rm -rf build\nls", None).unwrap();
    history.add_in_dir("echo a \\\nb", None).unwrap();
    drop(history);
    let reopened = History::load_from(fixture.path()).unwrap();
    assert_eq!(reopened.get(0), "rm -rf build\nls");
    assert_eq!(reopened.get(1), "echo a \\\nb");
    assert_eq!(reopened.display(0), "rm -rf build\u{21b5}ls");
    assert_eq!(display_line("plain"), "plain");
}

#[test]
fn history_path_requires_a_home() {
    assert!(history_path_for_home(None).is_err());
    assert!(history_path_for_home(Some(std::ffi::OsStr::new(""))).is_err());
}

#[test]
fn failed_append_retains_unseen_records_for_retry() {
    let fixture = Fixture::new();
    let mut local = store::Store::open(&fixture.path()).unwrap();
    let mut other = History::load_from(fixture.path()).unwrap();
    other.add_in_dir("foreign", None).unwrap();
    let occurrence = store::Occurrence {
        id: 0,
        command: String::new(),
        timestamp: 0,
        session_id: 0,
        cwd: None,
    };
    assert!(local.append(0, 0, occurrence).is_err());
    let snapshot = local.snapshot(0, 0).unwrap();
    assert_eq!(snapshot.occurrences.len(), 1);
    assert_eq!(snapshot.occurrences[0].command, "foreign");
}

#[test]
fn history_writer_process() {
    let Some(path) = std::env::var_os("ISH_HISTORY_TEST_WRITER") else {
        return;
    };
    let mut history = History::load_from(PathBuf::from(path)).unwrap();
    for i in 0..20 {
        history
            .add_in_dir(&format!("process {} {i}", std::process::id()), None)
            .unwrap();
    }
}

#[test]
fn independent_processes_append_without_losing_occurrences() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "seed\n").unwrap();
    let mut children: Vec<_> = (0..2)
        .map(|_| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "history::persistence_tests::history_writer_process",
                ])
                .env("ISH_HISTORY_TEST_WRITER", fixture.path())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(fixture.occurrences(), 41);
}

#[test]
fn partial_write_failure_process() {
    let Some(path) = std::env::var_os("ISH_HISTORY_TEST_PARTIAL_WRITE") else {
        return;
    };
    let path = PathBuf::from(path);
    let mut local = History::load_from(path.clone()).unwrap();
    local.add_in_dir("saved", None).unwrap();
    let mut other = History::load_from(path.clone()).unwrap();
    other.add_in_dir("foreign", None).unwrap();
    let log = store::storage_path(&path);
    let before = fs::read(&log).unwrap();
    let resource = rustix::process::Resource::Fsize;
    let original_limit = rustix::process::getrlimit(resource);
    rustix::process::setrlimit(
        resource,
        rustix::process::Rlimit {
            current: Some(before.len() as u64 + 20),
            ..original_limit
        },
    )
    .unwrap();
    let result = local.add_in_dir("must not survive a partial write", None);
    rustix::process::setrlimit(resource, original_limit).unwrap();
    assert!(result.is_err());
    assert_eq!(fs::read(&log).unwrap(), before);
    assert_eq!(local.len(), 1);
    assert_eq!(local.session_get(0), Some("saved"));
    assert_eq!(local.frequency(0), 1);
    local.add_in_dir("retry", None).unwrap();
    assert_eq!(local.len(), 3);
    assert_eq!(local.session_get(0), Some("retry"));
    assert_eq!(local.session_get(1), Some("saved"));
    assert_eq!(local.session_get(2), None);
    for idx in 0..local.len() {
        assert_eq!(local.frequency(idx), 1);
    }
}

#[test]
fn partial_append_rolls_back_and_retry_retains_unseen_occurrences() {
    let fixture = Fixture::new();
    // Ignoring the file-size signal in the isolated child turns a real partial
    // filesystem write into an error without changing the test runner's limits.
    let status = std::process::Command::new("/bin/sh")
        .args(["-c", "trap '' XFSZ; exec \"$ISH_HISTORY_TEST_EXECUTABLE\" --exact history::persistence_tests::partial_write_failure_process --test-threads=1"])
        .env("ISH_HISTORY_TEST_EXECUTABLE", std::env::current_exe().unwrap())
        .env("ISH_HISTORY_TEST_PARTIAL_WRITE", fixture.path()).status().unwrap();
    assert!(status.success());
    assert_eq!(fixture.occurrences(), 3);
}

fn use_command(history: &mut History, command: &str, timestamp: u64, cwd: Option<&Path>) {
    let id = history.last_id + 1;
    history.apply_occurrence(store::Occurrence {
        id,
        command: command.into(),
        timestamp,
        session_id: 0,
        cwd: cwd.map(Path::to_path_buf),
    });
}

#[test]
fn match_quality_precedes_exact_directory_and_frequency() {
    let mut history = History::from_entries(vec!["cargo build".into()]);
    for _ in 0..50 {
        use_command(
            &mut history,
            "echo cargo",
            now_millis(),
            Some(Path::new("/work")),
        );
    }
    let results = history.fuzzy_search_in_dir("cargo", Path::new("/work"));
    assert_eq!(history.get(results[0].entry_idx), "cargo build");
    assert_eq!(results[0].score, 3);
}

#[test]
fn directory_proximity_precedes_scoped_frequency() {
    let mut history = History::from_entries(Vec::new());
    let now = now_millis();
    use_command(
        &mut history,
        "cargo exact",
        now,
        Some(Path::new("/work/project/src")),
    );
    use_command(
        &mut history,
        "cargo ancestor",
        now,
        Some(Path::new("/work/project")),
    );
    use_command(&mut history, "cargo distant", now, Some(Path::new("/work")));
    for _ in 0..500 {
        use_command(
            &mut history,
            "cargo unrelated",
            now,
            Some(Path::new("/work/projects")),
        );
    }
    let results = history.fuzzy_search_in_dir("cargo", Path::new("/work/project/src"));
    assert_eq!(
        results
            .iter()
            .map(|entry| history.get(entry.entry_idx))
            .collect::<Vec<_>>(),
        [
            "cargo exact",
            "cargo ancestor",
            "cargo distant",
            "cargo unrelated"
        ]
    );
    let distant = history
        .find_entry_index(hash_str("cargo distant"), "cargo distant")
        .unwrap();
    for _ in 0..100 {
        use_command(
            &mut history,
            "cargo distant",
            now,
            Some(Path::new("/elsewhere")),
        );
    }
    assert_eq!(
        history
            .context_usage(distant, Some(Path::new("/work/project/src")))
            .1
            .count,
        1
    );
}

#[test]
fn recent_usage_beats_stale_popularity_but_frequency_matters_when_recent() {
    let mut history = History::from_entries(Vec::new());
    let now = now_millis();
    for _ in 0..1024 {
        use_command(&mut history, "cargo stale", now - 90 * 24 * 3_600_000, None);
    }
    for _ in 0..8 {
        use_command(&mut history, "cargo frequent", now - 1000, None);
    }
    use_command(&mut history, "cargo fresh", now, None);
    let matches = history.fuzzy_search("cargo");
    assert_eq!(
        matches
            .iter()
            .map(|entry| history.get(entry.entry_idx))
            .collect::<Vec<_>>(),
        ["cargo frequent", "cargo fresh", "cargo stale"]
    );
    let empty = history.fuzzy_search("");
    assert_eq!(history.get(empty[0].entry_idx), "cargo fresh");
}

#[test]
fn subset_preserves_every_match_and_matches_full_top_k_on_refinement() {
    let mut history = History::from_entries(Vec::new());
    for _ in 0..16 {
        use_command(
            &mut history,
            "cargo older special",
            now_millis() - 1000,
            None,
        );
    }
    for i in 0..350 {
        use_command(
            &mut history,
            &format!("cargo newest {i}"),
            now_millis(),
            None,
        );
    }
    let mut candidates = Vec::new();
    history.search_entry_indices_into(&mut candidates);
    let mut matches = Vec::new();
    let mut results = Vec::new();
    let cwd = Path::new("/");
    history.fuzzy_search_subset_into_in_dir(
        "cargo",
        &candidates,
        &mut matches,
        &mut results,
        200,
        cwd,
    );
    assert_eq!(matches.len(), 351);
    let full = history.fuzzy_search("cargo");
    assert_eq!(
        results
            .iter()
            .map(|entry| entry.entry_idx)
            .collect::<Vec<_>>(),
        full[..200]
            .iter()
            .map(|entry| entry.entry_idx)
            .collect::<Vec<_>>()
    );
    assert_eq!(history.get(results[0].entry_idx), "cargo older special");
    let mut refined = Vec::new();
    history.fuzzy_search_subset_into_in_dir(
        "cargo older",
        &matches,
        &mut refined,
        &mut results,
        200,
        cwd,
    );
    assert_eq!(refined.len(), 1);
    assert_eq!(history.get(results[0].entry_idx), "cargo older special");
}

#[test]
fn unicode_scalar_lowercasing_and_long_subsequence_queries_keep_positions() {
    let history = History::from_entries(vec!["İstanbul".into(), "é cafe".into()]);
    assert_eq!(
        history.get(history.fuzzy_search("İ")[0].entry_idx),
        "İstanbul"
    );
    assert_eq!(
        history.get(history.fuzzy_search("i")[0].entry_idx),
        "İstanbul"
    );
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
    for i in 0..6000 {
        use_command(
            &mut history,
            "cargo popular",
            now_millis(),
            Some(&PathBuf::from(format!("/elsewhere/{i}"))),
        );
    }
    use_command(
        &mut history,
        "cargo popular",
        now_millis(),
        Some(Path::new("/work")),
    );
    for i in 0..1000 {
        use_command(&mut history, &format!("cargo {i}"), now_millis(), None);
    }
    let started = std::time::Instant::now();
    let mut results = Vec::new();
    let mut candidates = Vec::new();
    history.search_entry_indices_into(&mut candidates);
    history.fuzzy_search_subset_into_in_dir(
        "cargo",
        &candidates,
        &mut Vec::new(),
        &mut results,
        200,
        Path::new("/work/src"),
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(history.get(results[0].entry_idx), "cargo popular");
}

#[test]
fn startup_and_incremental_sync_merge_large_overlapping_uses_promptly() {
    let fixture = Fixture::new();
    let commands: Vec<String> = (0..6000).map(|i| format!("echo {i}")).collect();
    let refs: Vec<&str> = commands.iter().map(String::as_str).collect();
    let timestamps = vec![1_700_000_000_000; refs.len()];
    fs::write(
        legacy::cache_path(&fixture.path()),
        cache_fixture(&refs, &timestamps),
    )
    .unwrap();
    let text: String = commands
        .iter()
        .map(|command| record(1_700_000_001_000, command))
        .collect();
    fs::write(fixture.path(), text).unwrap();
    let started = std::time::Instant::now();
    let mut history = History::load_from(fixture.path()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(history.len(), 6000);
    assert_eq!(fixture.occurrences(), 12_000);
    let path = store::storage_path(&fixture.path());
    let bytes = fs::read(&path).unwrap();
    let mut additional = Vec::new();
    let mut offset = 16;
    let mut next_id = 12_001i64;
    while offset < bytes.len() {
        let length = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize;
        let mut frame = bytes[offset..offset + 16 + length].to_vec();
        frame[16..24].copy_from_slice(&next_id.to_le_bytes());
        let timestamp = u64::from_le_bytes(frame[24..32].try_into().unwrap());
        frame[24..32].copy_from_slice(&(timestamp + 1).to_le_bytes());
        let checksum = store::crc32(&frame[16..]);
        frame[12..16].copy_from_slice(&checksum.to_le_bytes());
        additional.extend_from_slice(&frame);
        offset += 16 + length;
        next_id += 1;
    }
    use std::io::Write;
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&additional).unwrap();
    file.sync_all().unwrap();
    let started = std::time::Instant::now();
    history.sync().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(history.len(), 6000);
    for idx in 0..history.len() {
        assert_eq!(history.frequency(idx), 4);
    }
}
