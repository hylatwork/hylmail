//! The message body cache.
//!
//! Bodies are the expensive part of a mailbox: an HTML newsletter can be a
//! megabyte.  They are therefore stored separately from the headers and evicted
//! with a least-recently-used policy once the configured limit (500 MB by
//! default) is exceeded.
//!
//! "Recently used" is tracked in `message_content.last_accessed`, which the
//! reading pane refreshes every time it renders a message.

use rusqlite::{Connection, OptionalExtension};

use crate::error::Result;
use crate::models::ids::{from_millis, to_millis};
use crate::models::{Attachment, MessageBody, MessageId};

/// Serialises the attachment list, never failing.
fn encode_attachments(attachments: &[Attachment]) -> String {
    serde_json::to_string(attachments).unwrap_or_else(|_| "[]".to_string())
}

/// Measures the in-memory footprint a body occupies in the cache.
#[must_use]
pub fn measure(body: &MessageBody) -> u64 {
    let mut total = 0u64;
    for text in [
        body.plain_text.as_deref(),
        body.html_raw.as_deref(),
        body.html_sanitized.as_deref(),
        body.calendar_ics.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        total = total.saturating_add(text.len() as u64);
    }
    total.saturating_add(encode_attachments(&body.attachments).len() as u64)
}

/// Inserts or replaces a cached body.
///
/// The owning message is marked as `body_cached` and its search index entry is
/// refreshed so that the body text becomes searchable.
pub fn put_content(connection: &Connection, body: &MessageBody) -> Result<()> {
    let attachments_json = encode_attachments(&body.attachments);
    let bytes = measure(body);
    let now = to_millis(Some(crate::models::utc_now()));
    connection.execute(
        "INSERT INTO message_content (
             message_id, account_id, plain_text, html_raw, html_sanitized, attachments_json,
             calendar_ics, external_images_blocked, image_count, cached_bytes, fetched_at,
             last_accessed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
         ON CONFLICT (message_id) DO UPDATE SET
             plain_text              = excluded.plain_text,
             html_raw                = excluded.html_raw,
             html_sanitized          = excluded.html_sanitized,
             attachments_json        = excluded.attachments_json,
             calendar_ics            = excluded.calendar_ics,
             external_images_blocked = excluded.external_images_blocked,
             image_count             = excluded.image_count,
             cached_bytes            = excluded.cached_bytes,
             fetched_at              = excluded.fetched_at,
             last_accessed           = excluded.last_accessed",
        rusqlite::params![
            body.message_id.to_string(),
            account_id_of(connection, body.message_id)?,
            body.plain_text,
            body.html_raw,
            body.html_sanitized,
            attachments_json,
            body.calendar_ics,
            i64::from(body.external_images_blocked),
            body.image_count as i64,
            bytes as i64,
            now,
        ],
    )?;
    super::messages::set_body_cached(connection, body.message_id, true)?;
    super::messages::reindex_message(connection, body.message_id)?;
    Ok(())
}

/// Looks up the account id of a message so that the cache row can carry it.
fn account_id_of(connection: &Connection, id: MessageId) -> Result<String> {
    let account: Option<String> = connection
        .query_row(
            "SELECT account_id FROM messages WHERE id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(account.unwrap_or_default())
}

/// Loads a cached body.
pub fn get_content(connection: &Connection, id: MessageId) -> Result<Option<MessageBody>> {
    let row = connection
        .query_row(
            "SELECT message_id, plain_text, html_raw, html_sanitized, attachments_json,
                    calendar_ics, external_images_blocked, image_count, cached_bytes, fetched_at
             FROM message_content WHERE message_id = ?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>("message_id")?,
                    row.get::<_, Option<String>>("plain_text")?,
                    row.get::<_, Option<String>>("html_raw")?,
                    row.get::<_, Option<String>>("html_sanitized")?,
                    row.get::<_, String>("attachments_json")?,
                    row.get::<_, Option<String>>("calendar_ics")?,
                    row.get::<_, i64>("external_images_blocked")?,
                    row.get::<_, i64>("image_count")?,
                    row.get::<_, i64>("cached_bytes")?,
                    row.get::<_, Option<i64>>("fetched_at")?,
                ))
            },
        )
        .optional()?;

    let Some((
        raw_id,
        plain_text,
        html_raw,
        html_sanitized,
        attachments_json,
        calendar_ics,
        blocked,
        image_count,
        cached_bytes,
        fetched_at,
    )) = row
    else {
        return Ok(None);
    };

    let attachments =
        serde_json::from_str::<Vec<Attachment>>(&attachments_json).unwrap_or_default();
    Ok(Some(MessageBody {
        message_id: MessageId::parse(&raw_id).unwrap_or(id),
        plain_text,
        html_raw,
        html_sanitized,
        attachments,
        calendar_ics,
        external_images_blocked: blocked != 0,
        image_count: image_count.max(0) as usize,
        cached_bytes: cached_bytes.max(0) as u64,
        fetched_at: from_millis(fetched_at),
    }))
}

/// Refreshes the "last used" marker of a cached body.
pub fn touch(connection: &Connection, id: MessageId) -> Result<()> {
    connection.execute(
        "UPDATE message_content SET last_accessed = ?2 WHERE message_id = ?1",
        rusqlite::params![id.to_string(), to_millis(Some(crate::models::utc_now()))],
    )?;
    Ok(())
}

/// Removes a cached body and clears the `body_cached` marker.
pub fn delete_content(connection: &Connection, id: MessageId) -> Result<bool> {
    let affected = connection.execute(
        "DELETE FROM message_content WHERE message_id = ?1",
        [id.to_string()],
    )?;
    if affected > 0 {
        super::messages::set_body_cached(connection, id, false)?;
        super::messages::reindex_message(connection, id)?;
    }
    Ok(affected > 0)
}

/// Removes every cached body.
pub fn clear(connection: &Connection) -> Result<u64> {
    let mut statement = connection.prepare("SELECT message_id FROM message_content")?;
    let ids: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    drop(statement);
    for raw in &ids {
        if let Ok(id) = MessageId::parse(raw) {
            super::messages::set_body_cached(connection, id, false)?;
        }
    }
    let affected = connection.execute("DELETE FROM message_content", [])?;
    Ok(affected as u64)
}

/// Number of cached bodies.
pub fn count(connection: &Connection) -> Result<u64> {
    let value: i64 =
        connection.query_row("SELECT count(*) FROM message_content", [], |row| row.get(0))?;
    Ok(value.max(0) as u64)
}

/// Total size of the cache in bytes.
pub fn cached_bytes(connection: &Connection) -> Result<u64> {
    let value: i64 = connection.query_row(
        "SELECT COALESCE(sum(cached_bytes), 0) FROM message_content",
        [],
        |row| row.get(0),
    )?;
    Ok(value.max(0) as u64)
}

/// Evicts least recently used bodies until the cache fits into `limit_bytes`.
///
/// Returns the number of bytes that were freed.
pub fn evict_lru(connection: &Connection, limit_bytes: u64) -> Result<u64> {
    let total = cached_bytes(connection)?;
    if total <= limit_bytes {
        return Ok(0);
    }

    let mut candidates: Vec<(String, i64)> = Vec::new();
    {
        let mut statement = connection.prepare(
            "SELECT message_id, cached_bytes FROM message_content
             ORDER BY last_accessed ASC, message_id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            candidates.push(row?);
        }
    }

    let mut freed = 0u64;
    let mut remaining = total;
    for (raw_id, bytes) in candidates {
        if remaining <= limit_bytes {
            break;
        }
        let Ok(id) = MessageId::parse(&raw_id) else {
            continue;
        };
        if delete_content(connection, id)? {
            let bytes = bytes.max(0) as u64;
            freed = freed.saturating_add(bytes);
            remaining = remaining.saturating_sub(bytes);
        }
    }
    tracing::debug!(freed, remaining, limit_bytes, "content cache evicted");
    Ok(freed)
}

/// Convenience wrapper: evicts down to `limit_bytes` only when necessary.
pub fn enforce_limit(connection: &Connection, limit_bytes: u64) -> Result<u64> {
    evict_lru(connection, limit_bytes)
}

/// Returns the ids of bodies that were not touched since `cutoff_millis`.
pub fn stale_before(connection: &Connection, cutoff_millis: i64) -> Result<Vec<MessageId>> {
    let mut statement = connection.prepare(
        "SELECT message_id FROM message_content WHERE last_accessed < ?1 ORDER BY last_accessed",
    )?;
    let rows = statement.query_map([cutoff_millis], |row| row.get::<_, String>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        if let Ok(id) = MessageId::parse(&row?) {
            ids.push(id);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{folders, messages, Database};
    use crate::models::{Folder, FolderKind, MailAddress, MessageHeader};

    struct Fixture {
        database: Database,
        account: crate::models::AccountId,
    }

    fn fixture() -> Fixture {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = crate::models::AccountId::new();
        database
            .write(|connection| {
                let folder = Folder::new(account, "Inbox", "INBOX", FolderKind::Inbox);
                folders::upsert_folder(connection, &folder)
            })
            .unwrap();
        Fixture { database, account }
    }

    fn store_message(fixture: &Fixture, uid: u32, subject: &str) -> MessageId {
        fixture
            .database
            .write(|connection| {
                let folder = folders::list_folders(connection, fixture.account)?[0].id;
                let mut header = MessageHeader::new(fixture.account, folder);
                header.uid = Some(uid);
                header.subject = subject.to_string();
                header.from = vec![MailAddress::new("a@example.org")];
                messages::upsert_message(connection, &mut header)?;
                Ok(header.id)
            })
            .unwrap()
    }

    fn body(id: MessageId, text: &str) -> MessageBody {
        let mut body = MessageBody::empty(id);
        body.plain_text = Some(text.to_string());
        body
    }

    #[test]
    fn bodies_round_trip() {
        let fixture = fixture();
        let id = store_message(&fixture, 1, "Hello");
        let mut original = body(id, "hello world");
        original.html_raw = Some("<p>hello world</p>".to_string());
        original.html_sanitized = Some("<p>hello world</p>".to_string());
        original.attachments.push(Attachment {
            part_id: "2".to_string(),
            filename: Some("a.txt".to_string()),
            mime_type: "text/plain".to_string(),
            size_bytes: Some(3),
            content_id: None,
            inline: false,
            is_calendar: false,
        });
        original.calendar_ics = Some("BEGIN:VCALENDAR".to_string());
        original.image_count = 2;
        original.external_images_blocked = false;

        fixture
            .database
            .write(|connection| {
                put_content(connection, &original)?;
                let loaded = get_content(connection, id)?.unwrap();
                assert_eq!(loaded.plain_text, original.plain_text);
                assert_eq!(loaded.attachments, original.attachments);
                assert_eq!(loaded.image_count, 2);
                assert!(!loaded.external_images_blocked);
                assert!(loaded.cached_bytes > 0);
                assert!(loaded.fetched_at.is_some());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn caching_a_body_marks_the_message_and_indexes_the_text() {
        let fixture = fixture();
        let id = store_message(&fixture, 1, "Hello");
        fixture
            .database
            .write(|connection| {
                put_content(connection, &body(id, "a kangaroo appeared"))?;
                let header = messages::get_message(connection, id)?.unwrap();
                assert!(header.body_cached);
                assert_eq!(
                    crate::db::fts::search(connection, None, "kangaroo", 10)?,
                    vec![id]
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn putting_the_same_body_twice_replaces_it() {
        let fixture = fixture();
        let id = store_message(&fixture, 1, "Hello");
        fixture
            .database
            .write(|connection| {
                put_content(connection, &body(id, "first"))?;
                put_content(connection, &body(id, "second"))?;
                assert_eq!(count(connection)?, 1);
                assert_eq!(
                    get_content(connection, id)?.unwrap().plain_text.as_deref(),
                    Some("second")
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn deleting_a_body_clears_the_marker() {
        let fixture = fixture();
        let id = store_message(&fixture, 1, "Hello");
        fixture
            .database
            .write(|connection| {
                put_content(connection, &body(id, "text"))?;
                assert!(delete_content(connection, id)?);
                assert!(!delete_content(connection, id)?);
                assert!(!messages::get_message(connection, id)?.unwrap().body_cached);
                assert!(get_content(connection, id)?.is_none());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn touching_updates_the_lru_marker() {
        let fixture = fixture();
        let id = store_message(&fixture, 1, "Hello");
        fixture
            .database
            .write(|connection| {
                put_content(connection, &body(id, "text"))?;
                let before: i64 = connection.query_row(
                    "SELECT last_accessed FROM message_content WHERE message_id = ?1",
                    [id.to_string()],
                    |row| row.get(0),
                )?;
                connection.execute(
                    "UPDATE message_content SET last_accessed = 0 WHERE message_id = ?1",
                    [id.to_string()],
                )?;
                touch(connection, id)?;
                let after: i64 = connection.query_row(
                    "SELECT last_accessed FROM message_content WHERE message_id = ?1",
                    [id.to_string()],
                    |row| row.get(0),
                )?;
                assert!(after > before || after > 0);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn lru_eviction_frees_the_oldest_entries_first() {
        let fixture = fixture();
        let mut ids = Vec::new();
        for uid in 1..=4 {
            let id = store_message(&fixture, uid, "subject");
            ids.push(id);
        }
        fixture
            .database
            .write(|connection| {
                for (index, id) in ids.iter().enumerate() {
                    let mut entry = body(*id, &"x".repeat(1_000));
                    entry.cached_bytes = 1_000;
                    put_content(connection, &entry)?;
                    connection.execute(
                        "UPDATE message_content SET last_accessed = ?2, cached_bytes = 1000 WHERE message_id = ?1",
                        rusqlite::params![id.to_string(), index as i64],
                    )?;
                }
                assert_eq!(cached_bytes(connection)?, 4_000);

                // Keeping 2500 bytes must evict the two oldest entries.
                let freed = evict_lru(connection, 2_500)?;
                assert_eq!(freed, 2_000);
                assert!(cached_bytes(connection)? <= 2_500);
                assert!(get_content(connection, ids[0])?.is_none());
                assert!(get_content(connection, ids[1])?.is_none());
                assert!(get_content(connection, ids[3])?.is_some());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn eviction_is_a_no_op_below_the_limit() {
        let fixture = fixture();
        let id = store_message(&fixture, 1, "Hello");
        fixture
            .database
            .write(|connection| {
                put_content(connection, &body(id, "small"))?;
                assert_eq!(evict_lru(connection, 1_000_000)?, 0);
                assert_eq!(count(connection)?, 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn clearing_removes_everything() {
        let fixture = fixture();
        for uid in 1..=3 {
            let id = store_message(&fixture, uid, "subject");
            fixture
                .database
                .write(|connection| put_content(connection, &body(id, "data")))
                .unwrap();
        }
        fixture
            .database
            .write(|connection| {
                assert_eq!(count(connection)?, 3);
                assert_eq!(clear(connection)?, 3);
                assert_eq!(count(connection)?, 0);
                assert_eq!(cached_bytes(connection)?, 0);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn deleting_the_message_removes_the_cached_body() {
        let fixture = fixture();
        let id = store_message(&fixture, 1, "Hello");
        fixture
            .database
            .write(|connection| {
                put_content(connection, &body(id, "data"))?;
                messages::delete_message(connection, id)?;
                assert_eq!(count(connection)?, 0);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn stale_entries_can_be_listed() {
        let fixture = fixture();
        let fresh = store_message(&fixture, 1, "fresh");
        let old = store_message(&fixture, 2, "old");
        fixture
            .database
            .write(|connection| {
                put_content(connection, &body(fresh, "a"))?;
                put_content(connection, &body(old, "b"))?;
                connection.execute(
                    "UPDATE message_content SET last_accessed = 1 WHERE message_id = ?1",
                    [old.to_string()],
                )?;
                let stale = stale_before(connection, 100)?;
                assert_eq!(stale, vec![old]);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn measuring_a_body_counts_every_field() {
        let id = MessageId::new();
        let mut body = MessageBody::empty(id);
        body.plain_text = Some("12345".to_string());
        body.html_raw = Some("123".to_string());
        let base = measure(&body);
        assert!(base >= 8);
        body.calendar_ics = Some("1234567".to_string());
        assert_eq!(measure(&body), base + 7);
    }
}
