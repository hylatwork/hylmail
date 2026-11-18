//! Conversation (thread) summaries.
//!
//! Threading itself is derived from the RFC 5322 headers when a message is
//! stored (see [`crate::models::MessageHeader::conversation_key`]); this module
//! only materialises the aggregate that the conversation view needs to render a
//! collapsed thread: subject, newest timestamp, total and unread counts.
//!
//! `conversations` is a pure cache.  It can be rebuilt from `messages` at any
//! time with [`rebuild_for_account`], which is what happens after a
//! synchronisation that changed thread assignments.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row};

use crate::error::Result;
use crate::models::ids::from_millis;
use crate::models::AccountId;

/// One collapsed conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    /// Conversation key, see [`crate::models::MessageHeader::conversation_key`].
    pub thread_id: String,
    /// Owning account.
    pub account_id: AccountId,
    /// Subject of the oldest message in the thread.
    pub subject: Option<String>,
    /// Timestamp of the newest message.
    pub last_date: Option<DateTime<Utc>>,
    /// Number of messages in the thread.
    pub message_count: u32,
    /// Number of unread messages in the thread.
    pub unread_count: u32,
}

impl Conversation {
    /// `true` when the thread contains at least one unread message.
    #[must_use]
    pub fn is_unread(&self) -> bool {
        self.unread_count > 0
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        thread_id: row.get("thread_id")?,
        account_id: AccountId::parse(&row.get::<_, String>("account_id")?).unwrap_or_default(),
        subject: row.get("subject")?,
        last_date: from_millis(row.get::<_, Option<i64>>("last_date")?),
        message_count: row.get::<_, i64>("message_count")?.max(0) as u32,
        unread_count: row.get::<_, i64>("unread_count")?.max(0) as u32,
    })
}

const SELECT_COLUMNS: &str =
    "thread_id, account_id, subject, last_date, message_count, unread_count";

/// Recomputes every conversation of an account.
///
/// Returns the number of conversations that were written.
pub fn rebuild_for_account(connection: &Connection, account_id: AccountId) -> Result<u64> {
    connection.execute(
        "DELETE FROM conversations WHERE account_id = ?1",
        [account_id.to_string()],
    )?;
    let inserted = connection.execute(
        "INSERT INTO conversations (thread_id, account_id, subject, last_date, message_count, unread_count)
         SELECT
             m.thread_id,
             m.account_id,
             (SELECT m2.subject FROM messages m2
               WHERE m2.thread_id = m.thread_id
               ORDER BY COALESCE(m2.date_utc, m2.received_at, 0) ASC, m2.id ASC
               LIMIT 1),
             max(COALESCE(m.date_utc, m.received_at, 0)),
             count(*),
             sum(CASE WHEN (m.flags & 1) = 0 THEN 1 ELSE 0 END)
         FROM messages m
         WHERE m.account_id = ?1 AND m.thread_id IS NOT NULL
         GROUP BY m.thread_id",
        [account_id.to_string()],
    )?;
    Ok(inserted as u64)
}

/// Recomputes the conversations of every account.
pub fn rebuild_all(connection: &Connection) -> Result<u64> {
    connection.execute("DELETE FROM conversations", [])?;
    let inserted = connection.execute(
        "INSERT INTO conversations (thread_id, account_id, subject, last_date, message_count, unread_count)
         SELECT
             m.thread_id,
             m.account_id,
             (SELECT m2.subject FROM messages m2
               WHERE m2.thread_id = m.thread_id
               ORDER BY COALESCE(m2.date_utc, m2.received_at, 0) ASC, m2.id ASC
               LIMIT 1),
             max(COALESCE(m.date_utc, m.received_at, 0)),
             count(*),
             sum(CASE WHEN (m.flags & 1) = 0 THEN 1 ELSE 0 END)
         FROM messages m
         WHERE m.thread_id IS NOT NULL
         GROUP BY m.thread_id",
        [],
    )?;
    Ok(inserted as u64)
}

/// Lists the conversations of an account, newest first.
pub fn list_conversations(
    connection: &Connection,
    account_id: AccountId,
    limit: u32,
) -> Result<Vec<Conversation>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM conversations \
         WHERE account_id = ?1 ORDER BY COALESCE(last_date, 0) DESC, thread_id LIMIT ?2"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        rusqlite::params![account_id.to_string(), i64::from(limit)],
        from_row,
    )?;
    let mut conversations = Vec::new();
    for row in rows {
        conversations.push(row?);
    }
    Ok(conversations)
}

/// Loads a single conversation.
pub fn get_conversation(connection: &Connection, thread_id: &str) -> Result<Option<Conversation>> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM conversations WHERE thread_id = ?1");
    Ok(connection
        .query_row(&sql, [thread_id], from_row)
        .optional()?)
}

/// Number of materialised conversations.
pub fn count_conversations(connection: &Connection) -> Result<u64> {
    let value: i64 =
        connection.query_row("SELECT count(*) FROM conversations", [], |row| row.get(0))?;
    Ok(value.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{folders, messages, Database};
    use crate::models::{Folder, FolderKind, MailAddress, MessageFlags, MessageHeader};
    use chrono::Duration;

    struct Fixture {
        database: Database,
        account: AccountId,
        folder: crate::models::FolderId,
    }

    fn fixture() -> Fixture {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let folder = database
            .write(|connection| {
                let folder = Folder::new(account, "Inbox", "INBOX", FolderKind::Inbox);
                folders::upsert_folder(connection, &folder)?;
                Ok(folder.id)
            })
            .unwrap();
        Fixture {
            database,
            account,
            folder,
        }
    }

    fn message(
        fixture: &Fixture,
        uid: u32,
        subject: &str,
        root: &str,
        age_days: i64,
        seen: bool,
    ) -> MessageHeader {
        let mut header = MessageHeader::new(fixture.account, fixture.folder);
        header.uid = Some(uid);
        header.subject = subject.to_string();
        header.message_id = Some(format!("<{uid}@example.org>"));
        header.references = vec![root.to_string()];
        header.from = vec![MailAddress::new("a@example.org")];
        header.date = Some(Utc::now() - Duration::days(age_days));
        if seen {
            header.flags = MessageFlags::SEEN;
        }
        header
    }

    #[test]
    fn rebuilding_collapses_a_thread() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                for (uid, age, seen) in [(1, 3, true), (2, 2, false), (3, 1, false)] {
                    let mut header =
                        message(&fixture, uid, "Thread", "<root@example.org>", age, seen);
                    messages::upsert_message(connection, &mut header)?;
                }
                assert_eq!(rebuild_for_account(connection, fixture.account)?, 1);
                let conversations = list_conversations(connection, fixture.account, 10)?;
                assert_eq!(conversations.len(), 1);
                assert_eq!(conversations[0].message_count, 3);
                assert_eq!(conversations[0].unread_count, 2);
                assert!(conversations[0].is_unread());
                assert_eq!(conversations[0].subject.as_deref(), Some("Thread"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn rebuilding_is_idempotent() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                let mut header = message(&fixture, 1, "Only", "<root@example.org>", 0, false);
                messages::upsert_message(connection, &mut header)?;
                rebuild_for_account(connection, fixture.account)?;
                rebuild_for_account(connection, fixture.account)?;
                assert_eq!(count_conversations(connection)?, 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn separate_roots_produce_separate_conversations() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                let mut first = message(&fixture, 1, "A", "<a@example.org>", 5, true);
                messages::upsert_message(connection, &mut first)?;
                let mut second = message(&fixture, 2, "B", "<b@example.org>", 1, true);
                messages::upsert_message(connection, &mut second)?;
                assert_eq!(rebuild_for_account(connection, fixture.account)?, 2);
                let conversations = list_conversations(connection, fixture.account, 10)?;
                assert_eq!(conversations[0].subject.as_deref(), Some("B"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn accounts_are_isolated() {
        let fixture = fixture();
        let other_account = AccountId::new();
        fixture
            .database
            .write(|connection| {
                let other_folder = Folder::new(other_account, "Inbox", "INBOX", FolderKind::Inbox);
                folders::upsert_folder(connection, &other_folder)?;

                let mut mine = message(&fixture, 1, "Mine", "<root@example.org>", 0, true);
                messages::upsert_message(connection, &mut mine)?;

                let mut theirs = MessageHeader::new(other_account, other_folder.id);
                theirs.uid = Some(1);
                theirs.subject = "Theirs".to_string();
                theirs.message_id = Some("<9@example.org>".to_string());
                messages::upsert_message(connection, &mut theirs)?;

                assert_eq!(rebuild_all(connection)?, 2);
                assert_eq!(
                    list_conversations(connection, fixture.account, 10)?.len(),
                    1
                );
                assert_eq!(list_conversations(connection, other_account, 10)?.len(), 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn unknown_conversation_is_none() {
        let fixture = fixture();
        assert!(fixture
            .database
            .read(|connection| get_conversation(connection, "missing"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn limit_is_respected_and_newest_comes_first() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                for index in 0..5 {
                    let mut header = message(
                        &fixture,
                        index + 1,
                        &format!("T{index}"),
                        &format!("<root{index}@example.org>"),
                        i64::from(5 - index),
                        true,
                    );
                    messages::upsert_message(connection, &mut header)?;
                }
                rebuild_for_account(connection, fixture.account)?;
                let conversations = list_conversations(connection, fixture.account, 2)?;
                assert_eq!(conversations.len(), 2);
                // `T4` has the smallest age and is therefore the newest.
                assert_eq!(conversations[0].subject.as_deref(), Some("T4"));
                assert_eq!(conversations[1].subject.as_deref(), Some("T3"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn messages_without_message_ids_still_get_a_thread() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                // No `message_id`, no `references`: `conversation_key` still
                // falls back to the local id, so a thread is always produced.
                let mut header = MessageHeader::new(fixture.account, fixture.folder);
                header.uid = Some(1);
                messages::upsert_message(connection, &mut header)?;
                assert_eq!(rebuild_for_account(connection, fixture.account)?, 1);
                Ok(())
            })
            .unwrap();
    }
}
