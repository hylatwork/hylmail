//! SQLite schema migrations.
//!
//! The database keeps its schema version in SQLite's built-in `user_version`
//! pragma.  Every [`Migration`] is applied in its own transaction together with
//! the `user_version` bump, so an interrupted migration either happened
//! completely or not at all.
//!
//! Migrations are append only: never edit a shipped migration, add a new one.

use rusqlite::Connection;

use crate::error::{Error, Result};

/// One schema upgrade step.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Target schema version; must increase monotonically.
    pub version: u32,
    /// Human readable name, used in error messages and the migration report.
    pub name: &'static str,
    /// SQL statements executed as a single transaction.
    pub statements: &'static str,
}

/// Schema version this build writes.
pub const SCHEMA_VERSION: u32 = 1;

/// Schema version 1: headers, folders, content cache, contacts, conversations,
/// full text search and a small key/value store.
const V1_INITIAL_SCHEMA: &str = r#"
-- ---------------------------------------------------------------------------
-- Folders (mailboxes)
-- ---------------------------------------------------------------------------
CREATE TABLE folders (
    id              TEXT    PRIMARY KEY NOT NULL,
    account_id      TEXT    NOT NULL,
    parent_id       TEXT    REFERENCES folders (id) ON DELETE SET NULL,
    name            TEXT    NOT NULL,
    path            TEXT    NOT NULL,
    delimiter       TEXT,
    kind            TEXT    NOT NULL,
    special_use     TEXT,
    unread_count    INTEGER NOT NULL DEFAULT 0,
    total_count     INTEGER NOT NULL DEFAULT 0,
    uid_validity    INTEGER,
    uid_next        INTEGER,
    sync_marker     TEXT,
    sort_order      INTEGER NOT NULL DEFAULT 0,
    selectable      INTEGER NOT NULL DEFAULT 1,
    UNIQUE (account_id, path)
) STRICT;

CREATE INDEX idx_folders_account ON folders (account_id);
CREATE INDEX idx_folders_parent ON folders (parent_id);

-- ---------------------------------------------------------------------------
-- Message headers (the list view never touches the bodies)
-- ---------------------------------------------------------------------------
CREATE TABLE messages (
    id                  TEXT    PRIMARY KEY NOT NULL,
    account_id          TEXT    NOT NULL,
    folder_id           TEXT    NOT NULL REFERENCES folders (id) ON DELETE CASCADE,
    uid                 INTEGER,
    server_id           TEXT,
    message_id          TEXT,
    in_reply_to         TEXT,
    thread_id           TEXT,
    subject             TEXT    NOT NULL DEFAULT '',
    subject_normalized  TEXT,
    from_json           TEXT    NOT NULL DEFAULT '[]',
    to_json             TEXT    NOT NULL DEFAULT '[]',
    cc_json             TEXT    NOT NULL DEFAULT '[]',
    bcc_json            TEXT    NOT NULL DEFAULT '[]',
    reply_to_json       TEXT    NOT NULL DEFAULT '[]',
    references_json     TEXT    NOT NULL DEFAULT '[]',
    date_utc            INTEGER,
    received_at         INTEGER,
    size_bytes          INTEGER NOT NULL DEFAULT 0,
    flags               INTEGER NOT NULL DEFAULT 0,
    local_flags         INTEGER NOT NULL DEFAULT 0,
    has_attachments     INTEGER NOT NULL DEFAULT 0,
    snippet             TEXT,
    is_calendar_invite  INTEGER NOT NULL DEFAULT 0,
    body_cached         INTEGER NOT NULL DEFAULT 0,
    updated_at          INTEGER NOT NULL DEFAULT 0,
    UNIQUE (folder_id, uid)
) STRICT;

CREATE INDEX idx_messages_folder_date ON messages (folder_id, date_utc DESC);
CREATE INDEX idx_messages_account ON messages (account_id);
CREATE INDEX idx_messages_thread ON messages (thread_id);
CREATE INDEX idx_messages_message_id ON messages (message_id);
CREATE INDEX idx_messages_server_id ON messages (folder_id, server_id);
CREATE INDEX idx_messages_flags ON messages (folder_id, flags);

-- ---------------------------------------------------------------------------
-- Content cache (LRU evicted)
-- ---------------------------------------------------------------------------
CREATE TABLE message_content (
    message_id              TEXT    PRIMARY KEY NOT NULL
                                    REFERENCES messages (id) ON DELETE CASCADE,
    account_id              TEXT    NOT NULL,
    plain_text              TEXT,
    html_raw                TEXT,
    html_sanitized          TEXT,
    attachments_json        TEXT    NOT NULL DEFAULT '[]',
    calendar_ics            TEXT,
    external_images_blocked INTEGER NOT NULL DEFAULT 1,
    image_count             INTEGER NOT NULL DEFAULT 0,
    cached_bytes            INTEGER NOT NULL DEFAULT 0,
    fetched_at              INTEGER,
    last_accessed           INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX idx_content_lru ON message_content (last_accessed);
CREATE INDEX idx_content_account ON message_content (account_id);

-- ---------------------------------------------------------------------------
-- Address book
-- ---------------------------------------------------------------------------
CREATE TABLE contacts (
    id            TEXT    PRIMARY KEY NOT NULL,
    account_id    TEXT,
    display_name  TEXT,
    family_name   TEXT,
    given_name    TEXT,
    emails_json   TEXT    NOT NULL DEFAULT '[]',
    organization  TEXT,
    vcard_raw     TEXT,
    etag          TEXT,
    href          TEXT,
    source        TEXT    NOT NULL DEFAULT 'collected',
    last_seen     INTEGER,
    UNIQUE (account_id, href)
) STRICT;

CREATE INDEX idx_contacts_name ON contacts (display_name);
CREATE INDEX idx_contacts_account ON contacts (account_id);

-- ---------------------------------------------------------------------------
-- Conversation summary (threading is computed per account)
-- ---------------------------------------------------------------------------
CREATE TABLE conversations (
    thread_id     TEXT    PRIMARY KEY NOT NULL,
    account_id    TEXT    NOT NULL,
    subject       TEXT,
    last_date     INTEGER,
    message_count INTEGER NOT NULL DEFAULT 0,
    unread_count  INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX idx_conversations_account_date
    ON conversations (account_id, last_date DESC);

-- ---------------------------------------------------------------------------
-- Small key/value store for synchronisation bookkeeping
-- ---------------------------------------------------------------------------
CREATE TABLE kv_state (
    scope      TEXT    NOT NULL,
    key        TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (scope, key)
) STRICT;

-- ---------------------------------------------------------------------------
-- Full text search over subject, participants and (lazily) the body
-- ---------------------------------------------------------------------------
CREATE VIRTUAL TABLE messages_fts USING fts5 (
    message_id UNINDEXED,
    account_id UNINDEXED,
    folder_id  UNINDEXED,
    subject,
    sender,
    recipients,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);
"#;

/// All migrations, ordered by version.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial-schema",
    statements: V1_INITIAL_SCHEMA,
}];

/// Reads the schema version stored in the database file.
pub fn read_schema_version(conn: &Connection) -> Result<u32> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(version.max(0) as u32)
}

/// Summary of a [`crate::db::Database::migrate`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Version the database was at before.
    pub from: u32,
    /// Version the database is at afterwards.
    pub to: u32,
    /// Names of the migrations that were applied.
    pub applied: Vec<&'static str>,
}

impl MigrationReport {
    /// `true` when something was applied.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.applied.is_empty()
    }
}

/// Applies every migration newer than `current` to the connection.
pub fn apply_pending(conn: &mut Connection, current: u32) -> Result<MigrationReport> {
    if current > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    let mut applied = Vec::new();
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        let transaction = conn.transaction()?;
        transaction
            .execute_batch(migration.statements)
            .map_err(|source| Error::Migration {
                version: migration.version,
                name: migration.name,
                source,
            })?;
        transaction
            .pragma_update(None, "user_version", i64::from(migration.version))
            .map_err(|source| Error::Migration {
                version: migration.version,
                name: migration.name,
                source,
            })?;
        transaction.commit()?;
        tracing::debug!(
            version = migration.version,
            name = migration.name,
            "schema migrated"
        );
        applied.push(migration.name);
    }
    Ok(MigrationReport {
        from: current,
        to: SCHEMA_VERSION,
        applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_versions_are_unique_and_sorted() {
        let mut versions: Vec<u32> = MIGRATIONS.iter().map(|m| m.version).collect();
        let before = versions.len();
        versions.sort_unstable();
        versions.dedup();
        assert_eq!(before, versions.len(), "duplicate migration version");
        assert_eq!(
            versions,
            MIGRATIONS.iter().map(|m| m.version).collect::<Vec<_>>()
        );
        assert_eq!(versions.last().copied(), Some(SCHEMA_VERSION));
        assert_eq!(versions.first().copied(), Some(1));
    }

    #[test]
    fn migrations_have_names() {
        for migration in MIGRATIONS {
            assert!(!migration.name.is_empty());
            assert!(!migration.statements.trim().is_empty());
        }
    }

    #[test]
    fn applying_pending_creates_the_schema_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        let report = apply_pending(&mut conn, 0).unwrap();
        assert_eq!(report.from, 0);
        assert_eq!(report.to, SCHEMA_VERSION);
        assert!(report.changed());
        assert_eq!(read_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        let again = {
            let version = read_schema_version(&conn).unwrap();
            apply_pending(&mut conn, version).unwrap()
        };
        assert!(!again.changed());
        assert!(again.applied.is_empty());
    }

    #[test]
    fn newer_schema_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        let error = apply_pending(&mut conn, SCHEMA_VERSION + 5).unwrap_err();
        assert!(matches!(error, Error::SchemaTooNew { .. }));
    }

    #[test]
    fn every_expected_table_exists() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pending(&mut conn, 0).unwrap();
        for table in [
            "folders",
            "messages",
            "message_content",
            "contacts",
            "conversations",
            "kv_state",
            "messages_fts",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table `{table}` is missing");
        }
    }

    #[test]
    fn fts5_is_available_and_usable() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pending(&mut conn, 0).unwrap();
        conn.execute(
            "INSERT INTO messages_fts (message_id, account_id, folder_id, subject, sender, recipients, body)
             VALUES ('m1', 'a', 'f', 'Quarterly Report', 'ada@example.org', 'bob@example.org', 'please review')",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'quarterly'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);

        // Diacritics are folded, so `uber` finds `Über`.
        conn.execute(
            "INSERT INTO messages_fts (message_id, account_id, folder_id, subject, sender, recipients, body)
             VALUES ('m2', 'a', 'f', 'Über den Wolken', '', '', '')",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'uber'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn foreign_keys_cascade_from_folders_to_messages() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        apply_pending(&mut conn, 0).unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, path, kind) VALUES ('f1','a1','Inbox','INBOX','inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, folder_id, subject) VALUES ('m1','a1','f1','Hi')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM folders WHERE id = 'f1'", [])
            .unwrap();
        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn strict_tables_reject_wrong_types() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pending(&mut conn, 0).unwrap();
        // `sort_order` is INTEGER; a STRICT table must refuse a string.
        let result = conn.execute(
            "INSERT INTO folders (id, account_id, name, path, kind, sort_order)
             VALUES ('f1','a1','Inbox','INBOX','inbox','not a number')",
            [],
        );
        assert!(result.is_err());
    }
}
