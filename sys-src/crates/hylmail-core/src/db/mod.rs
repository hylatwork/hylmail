//! SQLite persistence layer.
//!
//! # Design
//!
//! * One [`Database`] owns exactly one SQLite connection behind a
//!   `parking_lot::Mutex`.  SQLite is fast enough for a single writer and the
//!   mutex avoids the `SQLITE_BUSY` retry dance; long running network work
//!   never happens while the lock is held.
//! * The database runs in `WAL` mode with `synchronous = NORMAL`, which is the
//!   recommended combination for a desktop application: durable across process
//!   crashes, fast, and readable by a second process (the CLI diagnostics).
//! * Foreign keys are enabled so that deleting a folder also drops its cached
//!   headers and bodies.
//! * Full text search uses FTS5 with `unicode61 remove_diacritics 2`, so
//!   searching for `uber` finds `Über`.
//!
//! # Layout
//!
//! | Module              | Responsibility                                |
//! |---------------------|-----------------------------------------------|
//! | [`migrations`]      | schema, `user_version` handling               |
//! | [`folders`]         | folder tree persistence                       |
//! | [`messages`]        | header CRUD and the message list query         |
//! | [`fts`]             | FTS5 index maintenance and query escaping      |
//! | [`content`]         | body cache with LRU eviction                    |
//! | [`contacts`]        | address book                                    |
//! | [`conversations`]   | thread summaries                                |

pub mod contacts;
pub mod content;
pub mod conversations;
pub mod folders;
pub mod fts;
pub mod messages;
pub mod migrations;

use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::error::{IoResultExt, Result};

pub use migrations::{MigrationReport, SCHEMA_VERSION};

/// A locked handle onto the SQLite database.
#[derive(Debug)]
pub struct Database {
    connection: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl Database {
    /// Opens (or creates) the database at `path` and configures it.
    ///
    /// The schema is **not** migrated here; call [`Database::migrate`]
    /// explicitly so that the caller can report progress to the user.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_path(parent)?;
        }
        let connection = Connection::open(path)?;
        configure(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: Some(path.to_path_buf()),
        })
    }

    /// Opens a temporary in-memory database, used by the test suite.
    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: None,
        })
    }

    /// Opens an in-memory database that already carries the current schema.
    pub fn open_in_memory_migrated() -> Result<Self> {
        let database = Self::open_in_memory()?;
        database.migrate()?;
        Ok(database)
    }

    /// Path of the database file; `None` for an in-memory database.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Current schema version stored in the file.
    pub fn schema_version(&self) -> Result<u32> {
        let connection = self.connection.lock();
        migrations::read_schema_version(&connection)
    }

    /// Applies all pending migrations.
    pub fn migrate(&self) -> Result<MigrationReport> {
        let mut connection = self.connection.lock();
        let current = migrations::read_schema_version(&connection)?;
        migrations::apply_pending(&mut connection, current)
    }

    /// Runs a closure with read access to the connection.
    pub fn read<R>(&self, action: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let connection = self.connection.lock();
        action(&connection)
    }

    /// Runs a closure inside a transaction, committing on `Ok`.
    ///
    /// Rolling back happens automatically when the closure returns an error or
    /// panics.
    pub fn write<R>(
        &self,
        action: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<R>,
    ) -> Result<R> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let value = action(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    /// Runs `PRAGMA wal_checkpoint(TRUNCATE)` and `VACUUM`.
    ///
    /// Called when the user removes the content cache or before the database is
    /// copied to a backup location.
    pub fn compact(&self) -> Result<()> {
        let connection = self.connection.lock();
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(())
    }

    /// Rough numbers used by the settings dialog.
    pub fn statistics(&self) -> Result<DatabaseStatistics> {
        self.read(|connection| {
            let count = |table: &str| -> Result<u64> {
                let sql = format!("SELECT count(*) FROM {table}");
                let value: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
                Ok(value.max(0) as u64)
            };
            Ok(DatabaseStatistics {
                accounts: distinct(connection, "SELECT count(DISTINCT account_id) FROM folders")?,
                folders: count("folders")?,
                messages: count("messages")?,
                cached_bodies: count("message_content")?,
                contacts: count("contacts")?,
                conversations: count("conversations")?,
                content_bytes: connection.query_row(
                    "SELECT COALESCE(sum(cached_bytes), 0) FROM message_content",
                    [],
                    |row| row.get::<_, i64>(0),
                )? as u64,
            })
        })
    }
}

fn distinct(connection: &Connection, sql: &str) -> Result<u64> {
    let value: i64 = connection.query_row(sql, [], |row| row.get(0))?;
    Ok(value.max(0) as u64)
}

/// Aggregated row counts for the settings dialog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DatabaseStatistics {
    /// Distinct accounts that own at least one folder.
    pub accounts: u64,
    /// Number of folders.
    pub folders: u64,
    /// Number of cached message headers.
    pub messages: u64,
    /// Number of cached message bodies.
    pub cached_bodies: u64,
    /// Number of contacts.
    pub contacts: u64,
    /// Number of conversation summaries.
    pub conversations: u64,
    /// Bytes held by the body cache.
    pub content_bytes: u64,
}

/// Applies the connection level pragmas.
fn configure(connection: &Connection) -> Result<()> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "recursive_triggers", "ON")?;
    // `journal_mode` is a query pragma; using `pragma_update` would fail with
    // "Execute returned results".  An in-memory database answers `memory`,
    // which is fine.
    let mode: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    tracing::trace!(journal_mode = %mode, "sqlite configured");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn in_memory_database_is_configured_correctly() {
        let database = Database::open_in_memory().unwrap();
        database
            .read(|connection| {
                let foreign_keys: i64 =
                    connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
                assert_eq!(foreign_keys, 1);
                let synchronous: i64 =
                    connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
                assert_eq!(synchronous, 1, "NORMAL is 1");
                Ok(())
            })
            .unwrap();
        assert!(database.path().is_none());
    }

    #[test]
    fn migrate_creates_the_schema_and_can_be_repeated() {
        let database = Database::open_in_memory().unwrap();
        assert_eq!(database.schema_version().unwrap(), 0);
        let report = database.migrate().unwrap();
        assert!(report.changed());
        assert_eq!(database.schema_version().unwrap(), SCHEMA_VERSION);
        let again = database.migrate().unwrap();
        assert!(!again.changed());
    }

    #[test]
    fn file_database_persists_between_handles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("hylmail.db");
        {
            let database = Database::open(&path).unwrap();
            database.migrate().unwrap();
            database
                .read(|connection| {
                    connection.execute(
                        "INSERT INTO folders (id, account_id, name, path, kind)
                         VALUES ('f1','a1','Inbox','INBOX','inbox')",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        assert!(path.is_file());
        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
        let folders: i64 = reopened
            .read(|connection| {
                Ok(connection.query_row("SELECT count(*) FROM folders", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(folders, 1);
    }

    #[test]
    fn transactions_roll_back_on_error() {
        let database = Database::open_in_memory_migrated().unwrap();
        let result: Result<()> = database.write(|transaction| {
            transaction.execute(
                "INSERT INTO folders (id, account_id, name, path, kind)
                 VALUES ('f1','a1','Inbox','INBOX','inbox')",
                [],
            )?;
            Err(Error::config("deliberate failure"))
        });
        assert!(result.is_err());
        let count: i64 = database
            .read(|connection| {
                Ok(connection.query_row("SELECT count(*) FROM folders", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(count, 0, "the transaction must have been rolled back");
    }

    #[test]
    fn transactions_commit_on_success() {
        let database = Database::open_in_memory_migrated().unwrap();
        database
            .write(|transaction| {
                transaction.execute(
                    "INSERT INTO folders (id, account_id, name, path, kind)
                     VALUES ('f1','a1','Inbox','INBOX','inbox')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let count: i64 = database
            .read(|connection| {
                Ok(connection.query_row("SELECT count(*) FROM folders", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn statistics_report_row_counts() {
        let database = Database::open_in_memory_migrated().unwrap();
        let stats = database.statistics().unwrap();
        assert_eq!(stats, DatabaseStatistics::default());
        database
            .read(|connection| {
                connection.execute(
                    "INSERT INTO folders (id, account_id, name, path, kind)
                     VALUES ('f1','a1','Inbox','INBOX','inbox')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let stats = database.statistics().unwrap();
        assert_eq!(stats.folders, 1);
        assert_eq!(stats.accounts, 1);
    }

    #[test]
    fn compaction_succeeds() {
        let database = Database::open_in_memory_migrated().unwrap();
        database.compact().unwrap();
    }
}
