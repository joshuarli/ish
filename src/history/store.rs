use super::legacy;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use std::fs::{DirBuilder, OpenOptions};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) struct Occurrence {
    pub id: i64,
    pub command: String,
    pub timestamp: u64,
    pub session_id: u64,
    pub cwd: Option<PathBuf>,
}

pub(super) struct Snapshot {
    pub generation: i64,
    pub occurrences: Vec<Occurrence>,
}

pub(super) struct Store {
    connection: Connection,
    pub path: PathBuf,
}

pub(super) fn database_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".sqlite3");
    path.with_file_name(name)
}

fn database_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("history database: {error}"))
}

impl Store {
    pub fn open(legacy_path: &Path) -> io::Result<Self> {
        let path = database_path(legacy_path);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        }
        // Create the file ourselves so SQLite never briefly exposes history
        // under permissions inherited from a permissive process umask.
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error),
        }
        let mut connection = Connection::open_with_flags(&path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(database_error)?;
        connection.busy_timeout(Duration::from_secs(5)).map_err(database_error)?;
        let journal: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(database_error)?;
        if journal != "wal" {
            return Err(io::Error::other("history database refused WAL journal mode"));
        }
        connection.pragma_update(None, "synchronous", "FULL").map_err(database_error)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(database_error)?;
        if version > 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("unsupported history database schema {version}")));
        }
        transaction.execute_batch("CREATE TABLE IF NOT EXISTS history_state (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                generation INTEGER NOT NULL, migrated INTEGER NOT NULL);
            INSERT OR IGNORE INTO history_state VALUES(1, 0, 0);
            CREATE TABLE IF NOT EXISTS occurrences (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                command TEXT NOT NULL CHECK(length(command) > 0),
                timestamp INTEGER NOT NULL CHECK(timestamp >= 0),
                session_id BLOB NOT NULL CHECK(length(session_id) = 8),
                cwd BLOB);").map_err(database_error)?;
        let migrated: bool = transaction.query_row(
            "SELECT migrated FROM history_state WHERE singleton=1", [], |row| row.get(0))
            .map_err(database_error)?;
        if !migrated {
            // Migration and its marker share the writer transaction. Concurrent
            // first opens can neither import twice nor see a partially imported log.
            let occurrences = legacy::read(legacy_path)?;
            let mut insert = transaction.prepare_cached(
                "INSERT INTO occurrences(command,timestamp,session_id,cwd) VALUES(?1,?2,?3,?4)")
                .map_err(database_error)?;
            for occurrence in occurrences {
                insert_occurrence(&mut insert, &occurrence)?;
            }
            drop(insert);
            transaction.execute("UPDATE history_state SET migrated=1 WHERE singleton=1", [])
                .map_err(database_error)?;
        }
        transaction.pragma_update(None, "user_version", 1).map_err(database_error)?;
        transaction.commit().map_err(database_error)?;
        Ok(Self { connection, path })
    }

    pub fn snapshot(&mut self, generation: i64, last_id: i64) -> io::Result<Snapshot> {
        let transaction = self.connection.transaction().map_err(database_error)?;
        let snapshot = read_snapshot(&transaction, generation, last_id)?;
        transaction.commit().map_err(database_error)?;
        Ok(snapshot)
    }

    pub fn append(&mut self, generation: i64, last_id: i64, mut occurrence: Occurrence)
        -> io::Result<(Snapshot, i64)> {
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let mut snapshot = read_snapshot(&transaction, generation, last_id)?;
        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO occurrences(command,timestamp,session_id,cwd) VALUES(?1,?2,?3,?4)")
                .map_err(database_error)?;
            insert_occurrence(&mut insert, &occurrence)?;
        }
        let id = transaction.last_insert_rowid();
        occurrence.id = id;
        snapshot.occurrences.push(occurrence);
        transaction.commit().map_err(database_error)?;
        Ok((snapshot, id))
    }

    pub fn reset(&mut self) -> io::Result<i64> {
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        transaction.execute("DELETE FROM occurrences", []).map_err(database_error)?;
        transaction.execute("UPDATE history_state SET generation=generation+1 WHERE singleton=1", [])
            .map_err(database_error)?;
        let generation = transaction.query_row(
            "SELECT generation FROM history_state WHERE singleton=1", [], |row| row.get(0))
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)?;
        Ok(generation)
    }

    pub fn compact(&mut self) -> io::Result<()> {
        // SQLite serializes VACUUM with writers and preserves every occurrence.
        // A checkpoint that cannot complete must be reported to the caller.
        self.connection.execute_batch("VACUUM").map_err(database_error)?;
        let busy: i64 = self.connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .map_err(database_error)?;
        if busy != 0 {
            return Err(io::Error::new(io::ErrorKind::WouldBlock,
                "history checkpoint blocked by another database reader"));
        }
        Ok(())
    }
}

fn insert_occurrence(insert: &mut rusqlite::CachedStatement<'_>, occurrence: &Occurrence)
    -> io::Result<()> {
    let timestamp = i64::try_from(occurrence.timestamp).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "history timestamp exceeds database range")
    })?;
    let session = occurrence.session_id.to_le_bytes();
    let cwd = occurrence.cwd.as_ref().map(|path| path.as_os_str().as_bytes());
    insert.execute(params![occurrence.command, timestamp, session.as_slice(), cwd])
        .map_err(database_error)?;
    Ok(())
}

fn read_snapshot(transaction: &rusqlite::Transaction<'_>, generation: i64, last_id: i64)
    -> io::Result<Snapshot> {
    let current = transaction.query_row(
        "SELECT generation FROM history_state WHERE singleton=1", [], |row| row.get(0))
        .map_err(database_error)?;
    let cutoff = if current == generation { last_id } else { 0 };
    let mut query = transaction.prepare_cached(
        "SELECT id,command,timestamp,session_id,cwd FROM occurrences WHERE id>?1 ORDER BY id")
        .map_err(database_error)?;
    let rows = query.query_map([cutoff], |row| {
        let session: Vec<u8> = row.get(3)?;
        let session_id = session.as_slice().try_into().map(u64::from_le_bytes)
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let timestamp: i64 = row.get(2)?;
        let timestamp = u64::try_from(timestamp).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let cwd: Option<Vec<u8>> = row.get(4)?;
        Ok(Occurrence { id: row.get(0)?, command: row.get(1)?, timestamp, session_id,
            cwd: cwd.map(|bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes))) })
    }).map_err(database_error)?;
    let occurrences = rows.collect::<Result<Vec<_>, _>>().map_err(database_error)?;
    Ok(Snapshot { generation: current, occurrences })
}

pub(super) fn render(path: &Path) -> io::Result<String> {
    let connection = Connection::open_with_flags(path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(database_error)?;
    connection.busy_timeout(Duration::from_secs(5)).map_err(database_error)?;
    let mut query = connection.prepare(
        "SELECT command FROM occurrences GROUP BY command ORDER BY MAX(timestamp), MAX(id)")
        .map_err(database_error)?;
    let mut rows = query.query([]).map_err(database_error)?;
    let mut output = String::new();
    while let Some(row) = rows.next().map_err(database_error)? {
        output.push_str(&row.get::<_, String>(0).map_err(database_error)?);
        output.push('\n');
    }
    Ok(output)
}
