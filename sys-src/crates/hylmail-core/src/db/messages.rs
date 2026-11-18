//! Message header persistence and the message list query.
//!
//! The list view is virtualised, which means it asks for a page of headers and
//! fetches more while the user scrolls.  Everything it needs - flags, snippet,
//! attachment marker, sort timestamp - is part of the `messages` row, so
//! rendering a page never touches [`super::content`].

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::de::DeserializeOwned;

use crate::error::Result;
use crate::models::ids::{from_millis, to_millis};
use crate::models::{
    AccountId, FolderId, LocalFlags, MailAddress, MessageFlags, MessageHeader, MessageId,
    MessageQuery, SortOrder,
};

use super::fts;

const SELECT_COLUMNS: &str = "m.id, m.account_id, m.folder_id, m.uid, m.server_id, m.message_id, \
     m.in_reply_to, m.thread_id, m.subject, m.subject_normalized, m.from_json, m.to_json, \
     m.cc_json, m.bcc_json, m.reply_to_json, m.references_json, m.date_utc, m.received_at, \
     m.size_bytes, m.flags, m.local_flags, m.has_attachments, m.snippet, m.is_calendar_invite, \
     m.body_cached, m.updated_at";

/// Deserialises a JSON list column, falling back to an empty list.
///
/// A corrupted column must not make the whole mailbox unreadable; the warning
/// makes the damage visible in the log instead.
fn json_list<T: DeserializeOwned>(raw: &str, column: &str) -> Vec<T> {
    match serde_json::from_str::<Vec<T>>(raw) {
        Ok(values) => values,
        Err(error) => {
            tracing::warn!(column, %error, "message column contained invalid JSON, using empty list");
            Vec::new()
        }
    }
}

fn encode_list<T: serde::Serialize>(values: &[T]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<MessageHeader> {
    let from_json: String = row.get("from_json")?;
    let to_json: String = row.get("to_json")?;
    let cc_json: String = row.get("cc_json")?;
    let bcc_json: String = row.get("bcc_json")?;
    let reply_to_json: String = row.get("reply_to_json")?;
    let references_json: String = row.get("references_json")?;

    Ok(MessageHeader {
        id: MessageId::parse(&row.get::<_, String>("id")?).unwrap_or_default(),
        account_id: AccountId::parse(&row.get::<_, String>("account_id")?).unwrap_or_default(),
        folder_id: FolderId::parse(&row.get::<_, String>("folder_id")?).unwrap_or_default(),
        uid: row
            .get::<_, Option<i64>>("uid")?
            .map(|value| value.max(0) as u32),
        server_id: row.get("server_id")?,
        message_id: row.get("message_id")?,
        in_reply_to: row.get("in_reply_to")?,
        thread_id: row.get("thread_id")?,
        subject: row.get("subject")?,
        subject_normalized: row.get("subject_normalized")?,
        from: json_list::<MailAddress>(&from_json, "from_json"),
        to: json_list::<MailAddress>(&to_json, "to_json"),
        cc: json_list::<MailAddress>(&cc_json, "cc_json"),
        bcc: json_list::<MailAddress>(&bcc_json, "bcc_json"),
        reply_to: json_list::<MailAddress>(&reply_to_json, "reply_to_json"),
        references: json_list::<String>(&references_json, "references_json"),
        date: from_millis(row.get::<_, Option<i64>>("date_utc")?),
        received_at: from_millis(row.get::<_, Option<i64>>("received_at")?),
        size_bytes: row.get::<_, i64>("size_bytes")?.max(0) as u64,
        flags: MessageFlags::from_bits_truncate(row.get::<_, i64>("flags")? as u32),
        local_flags: LocalFlags::from_bits_truncate(row.get::<_, i64>("local_flags")? as u32),
        has_attachments: row.get::<_, i64>("has_attachments")? != 0,
        snippet: row.get("snippet")?,
        is_calendar_invite: row.get::<_, i64>("is_calendar_invite")? != 0,
        body_cached: row.get::<_, i64>("body_cached")? != 0,
        updated_at: from_millis(row.get::<_, Option<i64>>("updated_at")?)
            .unwrap_or_else(crate::models::utc_now),
    })
}

/// Finds the id of an already stored message.
///
/// Resolution order mirrors how servers identify messages: IMAP by
/// `(folder, UID)`, EWS / GroupWise by `(folder, server id)`, and everything
/// else by the RFC 5322 `Message-ID` inside the account.
pub fn resolve_existing_id(
    connection: &Connection,
    header: &MessageHeader,
) -> Result<Option<MessageId>> {
    let lookup = |sql: &str, parameters: &[&dyn rusqlite::ToSql]| -> Result<Option<MessageId>> {
        let raw: Option<String> = connection
            .query_row(sql, parameters, |row| row.get(0))
            .optional()?;
        Ok(raw
            .as_deref()
            .and_then(|value| MessageId::parse(value).ok()))
    };

    if let Some(uid) = header.uid {
        let folder = header.folder_id.to_string();
        let uid = i64::from(uid);
        return lookup(
            "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
            &[&folder, &uid],
        );
    }
    if let Some(server_id) = header.server_id.as_deref() {
        let folder = header.folder_id.to_string();
        return lookup(
            "SELECT id FROM messages WHERE folder_id = ?1 AND server_id = ?2",
            &[&folder, &server_id],
        );
    }
    if let Some(message_id) = header.message_id.as_deref() {
        let account = header.account_id.to_string();
        return lookup(
            "SELECT id FROM messages WHERE account_id = ?1 AND message_id = ?2 LIMIT 1",
            &[&account, &message_id],
        );
    }
    Ok(None)
}

/// Inserts or updates a header.
///
/// The function resolves the existing row (see [`resolve_existing_id`]) and
/// writes `header.id` back, so the caller always learns which row was used.
/// Returns `true` when a new row was created.
pub fn upsert_message(connection: &Connection, header: &mut MessageHeader) -> Result<bool> {
    if let Some(existing) = resolve_existing_id(connection, header)? {
        header.id = existing;
    }
    header.refresh_derived_fields();
    header.updated_at = crate::models::utc_now();

    let inserted = !exists(connection, header.id)?;
    connection.execute(
        "INSERT INTO messages (
             id, account_id, folder_id, uid, server_id, message_id, in_reply_to, thread_id,
             subject, subject_normalized, from_json, to_json, cc_json, bcc_json, reply_to_json,
             references_json, date_utc, received_at, size_bytes, flags, local_flags,
             has_attachments, snippet, is_calendar_invite, body_cached, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)
         ON CONFLICT (id) DO UPDATE SET
             folder_id           = excluded.folder_id,
             uid                 = excluded.uid,
             server_id           = excluded.server_id,
             message_id          = excluded.message_id,
             in_reply_to         = excluded.in_reply_to,
             thread_id           = excluded.thread_id,
             subject             = excluded.subject,
             subject_normalized  = excluded.subject_normalized,
             from_json           = excluded.from_json,
             to_json             = excluded.to_json,
             cc_json             = excluded.cc_json,
             bcc_json            = excluded.bcc_json,
             reply_to_json       = excluded.reply_to_json,
             references_json     = excluded.references_json,
             date_utc            = excluded.date_utc,
             received_at         = excluded.received_at,
             size_bytes          = excluded.size_bytes,
             flags               = excluded.flags,
             local_flags         = excluded.local_flags,
             has_attachments     = excluded.has_attachments,
             snippet             = excluded.snippet,
             is_calendar_invite  = excluded.is_calendar_invite,
             body_cached         = excluded.body_cached,
             updated_at          = excluded.updated_at",
        rusqlite::params![
            header.id.to_string(),
            header.account_id.to_string(),
            header.folder_id.to_string(),
            header.uid.map(i64::from),
            header.server_id,
            header.message_id,
            header.in_reply_to,
            header.thread_id,
            header.subject,
            header.subject_normalized,
            encode_list(&header.from),
            encode_list(&header.to),
            encode_list(&header.cc),
            encode_list(&header.bcc),
            encode_list(&header.reply_to),
            encode_list(&header.references),
            to_millis(header.date),
            to_millis(header.received_at),
            header.size_bytes as i64,
            i64::from(header.flags.bits()),
            i64::from(header.local_flags.bits()),
            i64::from(header.has_attachments),
            header.snippet,
            i64::from(header.is_calendar_invite),
            i64::from(header.body_cached),
            to_millis(Some(header.updated_at)),
        ],
    )?;

    if inserted {
        fts::index_message(connection, header, None)?;
    }
    Ok(inserted)
}

/// `true` when a message row exists.
pub fn exists(connection: &Connection, id: MessageId) -> Result<bool> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM messages WHERE id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Loads a single header.
pub fn get_message(connection: &Connection, id: MessageId) -> Result<Option<MessageHeader>> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM messages m WHERE m.id = ?1");
    Ok(connection
        .query_row(&sql, [id.to_string()], from_row)
        .optional()?)
}

/// A WHERE clause together with its bound values.
struct WhereClause {
    sql: String,
    values: Vec<Value>,
}

fn build_where(query: &MessageQuery) -> WhereClause {
    let (scope, mut values) = scope_clause(query, "m");
    let mut clauses: Vec<String> = scope;

    if query.unread_only {
        // MessageFlags::SEEN == 1 << 0
        clauses.push("(m.flags & 1) = 0".to_string());
    }
    if query.flagged_only {
        // MessageFlags::FLAGGED == 1 << 2, LocalFlags::FLAGGED == 1 << 0
        clauses.push("((m.flags & 4) <> 0 OR (m.local_flags & 1) <> 0)".to_string());
    }
    if query.has_attachment {
        clauses.push("m.has_attachments <> 0".to_string());
    }
    if query.done_only {
        // MessageFlags::DONE == 1 << 7, LocalFlags::DONE == 1 << 1
        clauses.push("((m.flags & 128) <> 0 OR (m.local_flags & 2) <> 0)".to_string());
    }
    if query.exclude_done {
        clauses.push("((m.flags & 128) = 0 AND (m.local_flags & 2) = 0)".to_string());
    }
    if let Some(match_query) = query.text.as_deref().and_then(fts::build_match_query) {
        clauses.push(
            "m.id IN (SELECT message_id FROM messages_fts WHERE messages_fts MATCH ?)".to_string(),
        );
        values.push(Value::Text(match_query));
    }
    if let Some(window) = query.window.filter(|window| *window > 0) {
        // Keep only the newest `window` rows of the *same* scope, so a backend
        // without server-side search bounds how far back a local search looks
        // (`BR-GW-3`).  The scope is spelled once and reused for `w`.
        let (inner_scope, inner_values) = scope_clause(query, "w");
        let inner_where = if inner_scope.is_empty() {
            "1".to_string()
        } else {
            inner_scope.join(" AND ")
        };
        clauses.push(format!(
            "m.id IN (SELECT w.id FROM messages w WHERE {inner_where} \
             ORDER BY COALESCE(w.date_utc, w.received_at, 0) DESC, w.id DESC LIMIT ?)"
        ));
        values.extend(inner_values);
        values.push(Value::Integer(i64::from(window)));
    }

    WhereClause {
        sql: if clauses.is_empty() {
            "1".to_string()
        } else {
            clauses.join(" AND ")
        },
        values,
    }
}

/// The account and folder restrictions, written against `alias`.
///
/// The newest-window clause re-runs them inside a subquery, so the scope is
/// defined once instead of being spelled twice.
fn scope_clause(query: &MessageQuery, alias: &str) -> (Vec<String>, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Value> = Vec::new();

    if let Some(account_id) = query.account_id {
        clauses.push(format!("{alias}.account_id = ?"));
        values.push(Value::Text(account_id.to_string()));
    }
    if !query.folder_ids.is_empty() {
        let placeholders = vec!["?"; query.folder_ids.len()].join(", ");
        clauses.push(format!("{alias}.folder_id IN ({placeholders})"));
        for folder_id in &query.folder_ids {
            values.push(Value::Text(folder_id.to_string()));
        }
    }

    (clauses, values)
}

/// Runs a message list query.
pub fn list_messages(connection: &Connection, query: &MessageQuery) -> Result<Vec<MessageHeader>> {
    let where_clause = build_where(query);
    let direction = match query.order {
        SortOrder::NewestFirst => "DESC",
        SortOrder::OldestFirst => "ASC",
    };
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM messages m WHERE {} \
         ORDER BY COALESCE(m.date_utc, m.received_at, 0) {direction}, m.id {direction} \
         LIMIT ? OFFSET ?",
        where_clause.sql
    );

    let mut values = where_clause.values;
    values.push(Value::Integer(query.limit.map(i64::from).unwrap_or(-1)));
    values.push(Value::Integer(i64::from(query.offset)));

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), from_row)?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row?);
    }
    Ok(messages)
}

/// Counts the messages matching a query, ignoring limit and offset.
pub fn count_messages(connection: &Connection, query: &MessageQuery) -> Result<u64> {
    let where_clause = build_where(query);
    let sql = format!("SELECT count(*) FROM messages m WHERE {}", where_clause.sql);
    let value: i64 = connection.query_row(
        &sql,
        rusqlite::params_from_iter(where_clause.values.iter()),
        |row| row.get(0),
    )?;
    Ok(value.max(0) as u64)
}

/// Loads every message of one conversation.
pub fn list_thread(
    connection: &Connection,
    account_id: AccountId,
    thread_id: &str,
) -> Result<Vec<MessageHeader>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM messages m \
         WHERE m.account_id = ?1 AND m.thread_id = ?2 \
         ORDER BY COALESCE(m.date_utc, m.received_at, 0) ASC, m.id ASC"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        rusqlite::params![account_id.to_string(), thread_id],
        from_row,
    )?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row?);
    }
    Ok(messages)
}

/// Replaces the server flags of a message.
pub fn set_flags(connection: &Connection, id: MessageId, flags: MessageFlags) -> Result<()> {
    connection.execute(
        "UPDATE messages SET flags = ?2, updated_at = ?3 WHERE id = ?1",
        rusqlite::params![
            id.to_string(),
            i64::from(flags.bits()),
            to_millis(Some(crate::models::utc_now()))
        ],
    )?;
    Ok(())
}

/// Replaces the local-only flags of a message.
pub fn set_local_flags(connection: &Connection, id: MessageId, flags: LocalFlags) -> Result<()> {
    connection.execute(
        "UPDATE messages SET local_flags = ?2, updated_at = ?3 WHERE id = ?1",
        rusqlite::params![
            id.to_string(),
            i64::from(flags.bits()),
            to_millis(Some(crate::models::utc_now()))
        ],
    )?;
    Ok(())
}

/// Moves a message into another folder, resetting its IMAP UID.
pub fn set_folder(connection: &Connection, id: MessageId, folder_id: FolderId) -> Result<()> {
    connection.execute(
        "UPDATE messages SET folder_id = ?2, uid = NULL, updated_at = ?3 WHERE id = ?1",
        rusqlite::params![
            id.to_string(),
            folder_id.to_string(),
            to_millis(Some(crate::models::utc_now()))
        ],
    )?;
    Ok(())
}

/// Stores the item id a move produced.
///
/// Exchange hands out a new id for a moved item, so the next `UpdateItem`,
/// `MoveItem` or `DeleteItem` would address a message that no longer exists
/// without this.
pub fn set_server_id(connection: &Connection, id: MessageId, server_id: &str) -> Result<()> {
    connection.execute(
        "UPDATE messages SET server_id = ?2, updated_at = ?3 WHERE id = ?1",
        rusqlite::params![
            id.to_string(),
            server_id,
            to_millis(Some(crate::models::utc_now()))
        ],
    )?;
    Ok(())
}

/// Marks whether the body of a message is present in the cache.
pub fn set_body_cached(connection: &Connection, id: MessageId, cached: bool) -> Result<()> {
    connection.execute(
        "UPDATE messages SET body_cached = ?2 WHERE id = ?1",
        rusqlite::params![id.to_string(), i64::from(cached)],
    )?;
    Ok(())
}

/// Updates the list preview text.
pub fn set_snippet(connection: &Connection, id: MessageId, snippet: &str) -> Result<()> {
    connection.execute(
        "UPDATE messages SET snippet = ?2 WHERE id = ?1",
        rusqlite::params![id.to_string(), snippet],
    )?;
    Ok(())
}

/// Updates the header flags that are only known once the body was parsed.
///
/// The message list shows a paper clip for a message with an attachment and an
/// invitation marker for a meeting request.  Both can be discovered later than
/// the header - an IMAP `BODYSTRUCTURE` does not say whether a part is an
/// invitation, and the proxy protocols report no structure at all - so the
/// body parser writes the result back here.
pub fn set_derived_body_flags(
    connection: &Connection,
    id: MessageId,
    has_attachments: bool,
    is_calendar_invite: bool,
) -> Result<()> {
    connection.execute(
        "UPDATE messages SET has_attachments = ?2, is_calendar_invite = ?3 WHERE id = ?1",
        rusqlite::params![
            id.to_string(),
            i64::from(has_attachments),
            i64::from(is_calendar_invite)
        ],
    )?;
    Ok(())
}

/// Deletes a message including its cache entry and search index row.
pub fn delete_message(connection: &Connection, id: MessageId) -> Result<bool> {
    fts::remove_message(connection, id)?;
    let affected = connection.execute("DELETE FROM messages WHERE id = ?1", [id.to_string()])?;
    Ok(affected > 0)
}

/// Deletes every message of a folder.
pub fn delete_folder_messages(connection: &Connection, folder_id: FolderId) -> Result<u64> {
    let mut statement = connection.prepare("SELECT id FROM messages WHERE folder_id = ?1")?;
    let ids: Vec<String> = statement
        .query_map([folder_id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    drop(statement);
    for raw in &ids {
        if let Ok(id) = MessageId::parse(raw) {
            fts::remove_message(connection, id)?;
        }
    }
    let affected = connection.execute(
        "DELETE FROM messages WHERE folder_id = ?1",
        [folder_id.to_string()],
    )?;
    Ok(affected as u64)
}

/// Rebuilds the search index entry of a message, including its cached body.
pub fn reindex_message(connection: &Connection, id: MessageId) -> Result<()> {
    let Some(header) = get_message(connection, id)? else {
        fts::remove_message(connection, id)?;
        return Ok(());
    };
    let body: Option<String> = connection
        .query_row(
            "SELECT COALESCE(plain_text, html_sanitized, html_raw) FROM message_content WHERE message_id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    fts::index_message(connection, &header, body.as_deref())
}

/// Recomputes the unread and total counters of a folder from the message table.
///
/// Returns the new `(unread, total)` pair.
pub fn refresh_folder_counts(connection: &Connection, folder_id: FolderId) -> Result<(u32, u32)> {
    let total: i64 = connection.query_row(
        "SELECT count(*) FROM messages WHERE folder_id = ?1",
        [folder_id.to_string()],
        |row| row.get(0),
    )?;
    let unread: i64 = connection.query_row(
        "SELECT count(*) FROM messages WHERE folder_id = ?1 AND (flags & 1) = 0",
        [folder_id.to_string()],
        |row| row.get(0),
    )?;
    let pair = (unread.max(0) as u32, total.max(0) as u32);
    super::folders::update_counts(connection, folder_id, pair.0, pair.1)?;
    Ok(pair)
}

/// Highest UID stored for a folder; used by the incremental synchronisation.
pub fn highest_uid(connection: &Connection, folder_id: FolderId) -> Result<Option<u32>> {
    let value: Option<i64> = connection.query_row(
        "SELECT max(uid) FROM messages WHERE folder_id = ?1",
        [folder_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(value.map(|value| value.max(0) as u32))
}

/// UID and flag state of one stored message, as the synchroniser needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UidState {
    /// Local identifier of the message.
    pub id: MessageId,
    /// Server side UID.
    pub uid: u32,
    /// Flags currently stored locally.
    pub flags: MessageFlags,
}

/// Lists `(UID, id, flags)` for every message of a folder, ascending by UID.
///
/// The flag reconciliation pass needs the whole mapping at once: it compares a
/// server side `SEARCH` result against the local state, so fetching the local
/// state row by row would turn one query into thousands.
pub fn list_uid_states(connection: &Connection, folder_id: FolderId) -> Result<Vec<UidState>> {
    let mut statement = connection.prepare(
        "SELECT id, uid, flags FROM messages \
         WHERE folder_id = ?1 AND uid IS NOT NULL ORDER BY uid",
    )?;
    let rows = statement.query_map([folder_id.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut states = Vec::new();
    for row in rows {
        let (id, uid, flags) = row?;
        let id = id.parse::<MessageId>().map_err(|error| {
            crate::error::Error::Config(format!("invalid message id `{id}`: {error}"))
        })?;
        states.push(UidState {
            id,
            uid: uid.max(0) as u32,
            flags: MessageFlags::from_bits_truncate(flags as u32),
        });
    }
    Ok(states)
}

/// Collects the distinct conversation keys of an account.
pub fn distinct_thread_ids(connection: &Connection, account_id: AccountId) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT thread_id FROM messages \
         WHERE account_id = ?1 AND thread_id IS NOT NULL",
    )?;
    let rows = statement.query_map([account_id.to_string()], |row| row.get::<_, String>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

/// Every stored message id.
///
/// Used at startup to seed the "already handled by the rule engine" set, so a
/// rule that arrived in an earlier session is not re-applied to the whole
/// mailbox (`FR-RULE-1`).
pub fn all_ids(connection: &Connection) -> Result<Vec<MessageId>> {
    let mut statement = connection.prepare("SELECT id FROM messages")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
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
    use crate::db::{folders, Database};
    use crate::models::{Folder, FolderKind};
    use chrono::{Duration, Utc};

    struct Fixture {
        database: Database,
        account: AccountId,
        folder: FolderId,
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

    fn header(fixture: &Fixture, subject: &str, uid: u32, age_days: i64) -> MessageHeader {
        let mut header = MessageHeader::new(fixture.account, fixture.folder);
        header.subject = subject.to_string();
        header.uid = Some(uid);
        header.message_id = Some(format!("<{uid}@example.org>"));
        header.from = vec![MailAddress::with_name("ada@example.org", "Ada")];
        header.to = vec![MailAddress::new("bob@example.org")];
        header.date = Some(Utc::now() - Duration::days(age_days));
        header.size_bytes = 1024;
        header
    }

    #[test]
    fn headers_round_trip_through_the_database() {
        let fixture = fixture();
        let mut original = header(&fixture, "Hallo", 1, 0);
        original.cc = vec![MailAddress::new("cc@example.org")];
        original.references = vec!["<root@example.org>".to_string()];
        original.flags = MessageFlags::SEEN | MessageFlags::FLAGGED;
        original.local_flags = LocalFlags::DONE;
        original.has_attachments = true;
        original.snippet = Some("preview".to_string());
        original.is_calendar_invite = true;

        fixture
            .database
            .write(|connection| {
                assert!(upsert_message(connection, &mut original)?);
                // Timestamps are persisted with millisecond precision.
                original.date = crate::models::from_millis(crate::models::to_millis(original.date));
                original.updated_at =
                    crate::models::from_millis(crate::models::to_millis(Some(original.updated_at)))
                        .unwrap_or(original.updated_at);
                let loaded = get_message(connection, original.id)?.unwrap();
                assert_eq!(loaded, original);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn upserting_the_same_uid_updates_instead_of_duplicating() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                let mut first = header(&fixture, "First", 7, 0);
                assert!(upsert_message(connection, &mut first)?);
                let id = first.id;

                let mut second = header(&fixture, "Second", 7, 0);
                assert!(!upsert_message(connection, &mut second)?);
                assert_eq!(second.id, id, "the existing row must be reused");
                let loaded = get_message(connection, id)?.unwrap();
                assert_eq!(loaded.subject, "Second");
                assert_eq!(count_messages(connection, &MessageQuery::default())?, 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn messages_can_be_resolved_by_server_id_and_message_id() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                let mut by_server = header(&fixture, "Server", 1, 0);
                by_server.uid = None;
                by_server.server_id = Some("EWS-1".to_string());
                upsert_message(connection, &mut by_server)?;

                let mut lookup = header(&fixture, "Server", 0, 0);
                lookup.uid = None;
                lookup.server_id = Some("EWS-1".to_string());
                assert_eq!(
                    resolve_existing_id(connection, &lookup)?,
                    Some(by_server.id)
                );

                let mut by_message_id = header(&fixture, "MsgId", 2, 0);
                by_message_id.uid = None;
                by_message_id.server_id = None;
                upsert_message(connection, &mut by_message_id)?;

                let mut lookup = header(&fixture, "MsgId", 0, 0);
                lookup.uid = None;
                lookup.server_id = None;
                lookup.message_id = by_message_id.message_id.clone();
                assert_eq!(
                    resolve_existing_id(connection, &lookup)?,
                    Some(by_message_id.id)
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn list_orders_newest_first_and_pages() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                for index in 0..5 {
                    let mut message = header(
                        &fixture,
                        &format!("Message {index}"),
                        index + 1,
                        i64::from(index),
                    );
                    upsert_message(connection, &mut message)?;
                }
                let query = MessageQuery::in_folder(fixture.folder, 2);
                let page_one = list_messages(connection, &query)?;
                assert_eq!(page_one.len(), 2);
                assert_eq!(page_one[0].subject, "Message 0");

                let mut query = query;
                query.offset = 2;
                let page_two = list_messages(connection, &query)?;
                assert_eq!(page_two.len(), 2);
                assert_ne!(page_one[0].id, page_two[0].id);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn oldest_first_ordering_is_supported() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                for index in 0..3 {
                    let mut message = header(
                        &fixture,
                        &format!("M{index}"),
                        index + 1,
                        3 - i64::from(index),
                    );
                    upsert_message(connection, &mut message)?;
                }
                let query = MessageQuery {
                    folder_ids: vec![fixture.folder],
                    order: SortOrder::OldestFirst,
                    ..MessageQuery::default()
                };
                let messages = list_messages(connection, &query)?;
                assert_eq!(messages[0].subject, "M0");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn filters_restrict_the_result_set() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                let mut unread = header(&fixture, "Unread", 1, 0);
                unread.has_attachments = true;
                upsert_message(connection, &mut unread)?;

                let mut read = header(&fixture, "Read", 2, 1);
                read.flags = MessageFlags::SEEN;
                upsert_message(connection, &mut read)?;

                let mut flagged = header(&fixture, "Flagged", 3, 2);
                flagged.flags = MessageFlags::SEEN | MessageFlags::FLAGGED;
                upsert_message(connection, &mut flagged)?;

                let mut locally_done = header(&fixture, "Done", 4, 3);
                locally_done.local_flags = LocalFlags::DONE;
                upsert_message(connection, &mut locally_done)?;

                let base = MessageQuery {
                    folder_ids: vec![fixture.folder],
                    ..MessageQuery::default()
                };

                let mut query = base.clone();
                query.unread_only = true;
                assert_eq!(list_messages(connection, &query)?.len(), 2);

                let mut query = base.clone();
                query.flagged_only = true;
                assert_eq!(list_messages(connection, &query)?.len(), 1);

                let mut query = base.clone();
                query.has_attachment = true;
                assert_eq!(list_messages(connection, &query)?.len(), 1);

                let mut query = base.clone();
                query.done_only = true;
                assert_eq!(list_messages(connection, &query)?.len(), 1);

                let mut query = base.clone();
                query.exclude_done = true;
                assert_eq!(list_messages(connection, &query)?.len(), 3);

                assert_eq!(count_messages(connection, &base)?, 4);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn full_text_search_filters_the_list() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                let mut invoice = header(&fixture, "Invoice 2024", 1, 0);
                upsert_message(connection, &mut invoice)?;
                let mut other = header(&fixture, "Lunch", 2, 1);
                upsert_message(connection, &mut other)?;

                let query = MessageQuery::in_folder(fixture.folder, 500).matching("invo");
                let found = list_messages(connection, &query)?;
                assert_eq!(found.len(), 1);
                assert_eq!(found[0].subject, "Invoice 2024");

                // A query without any token is ignored rather than failing.
                let query = MessageQuery::in_folder(fixture.folder, 500).matching("   ");
                assert_eq!(list_messages(connection, &query)?.len(), 2);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_window_keeps_only_the_newest_messages_of_the_scope() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                for index in 0..5 {
                    let mut message =
                        header(&fixture, &format!("A{index}"), index + 1, i64::from(index));
                    upsert_message(connection, &mut message)?;
                }

                // A second folder with newer messages that must not fill the
                // window: the restriction is per scope, not global (`BR-GW-3`).
                let other = Folder::new(fixture.account, "Archive", "Archive", FolderKind::Archive);
                folders::upsert_folder(connection, &other)?;
                for index in 0..3 {
                    let mut message = MessageHeader::new(fixture.account, other.id);
                    message.subject = format!("B{index}");
                    message.uid = Some(100 + index);
                    message.message_id = Some(format!("<b{index}@example.org>"));
                    message.date = Some(Utc::now());
                    upsert_message(connection, &mut message)?;
                }

                let query = MessageQuery::in_folder(fixture.folder, 500).within(2);
                let found = list_messages(connection, &query)?;
                assert_eq!(found.len(), 2);
                assert_eq!(found[0].subject, "A0");
                assert_eq!(found[1].subject, "A1");
                assert_eq!(count_messages(connection, &query)?, 2);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn flags_can_be_updated_individually() {
        let fixture = fixture();
        let id = fixture
            .database
            .write(|connection| {
                let mut message = header(&fixture, "Flags", 1, 0);
                upsert_message(connection, &mut message)?;
                set_flags(connection, message.id, MessageFlags::SEEN)?;
                set_local_flags(connection, message.id, LocalFlags::FLAGGED)?;
                Ok(message.id)
            })
            .unwrap();
        let loaded = fixture
            .database
            .read(|connection| get_message(connection, id))
            .unwrap()
            .unwrap();
        assert!(loaded.flags.is_seen());
        assert!(loaded.local_flags.is_flagged());
        assert!(loaded.is_flagged());
    }

    #[test]
    fn moving_a_message_clears_its_uid() {
        let fixture = fixture();
        let target = FolderId::new();
        fixture
            .database
            .write(|connection| {
                let mut folder =
                    Folder::new(fixture.account, "Archive", "Archive", FolderKind::Archive);
                folder.id = target;
                folders::upsert_folder(connection, &folder)?;

                let mut message = header(&fixture, "Move me", 1, 0);
                upsert_message(connection, &mut message)?;
                set_folder(connection, message.id, target)?;
                let loaded = get_message(connection, message.id)?.unwrap();
                assert_eq!(loaded.folder_id, target);
                assert!(loaded.uid.is_none());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn deleting_a_message_removes_its_index_entry() {
        let fixture = fixture();
        let id = fixture
            .database
            .write(|connection| {
                let mut message = header(&fixture, "Delete me", 1, 0);
                upsert_message(connection, &mut message)?;
                assert!(fts::contains(connection, message.id)?);
                assert!(delete_message(connection, message.id)?);
                assert!(!fts::contains(connection, message.id)?);
                assert!(!delete_message(connection, message.id)?);
                Ok(message.id)
            })
            .unwrap();
        assert!(fixture
            .database
            .read(|connection| get_message(connection, id))
            .unwrap()
            .is_none());
    }

    #[test]
    fn deleting_a_folder_removes_all_its_messages() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                for index in 0..3 {
                    let mut message = header(&fixture, &format!("M{index}"), index + 1, 0);
                    upsert_message(connection, &mut message)?;
                }
                assert_eq!(delete_folder_messages(connection, fixture.folder)?, 3);
                assert_eq!(count_messages(connection, &MessageQuery::default())?, 0);
                assert_eq!(fts::indexed_count(connection)?, 0);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn folder_counts_are_refreshed() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                for index in 0..4 {
                    let mut message = header(&fixture, &format!("M{index}"), index + 1, 0);
                    if index % 2 == 0 {
                        message.flags = MessageFlags::SEEN;
                    }
                    upsert_message(connection, &mut message)?;
                }
                let (unread, total) = refresh_folder_counts(connection, fixture.folder)?;
                assert_eq!(total, 4);
                assert_eq!(unread, 2);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn highest_uid_is_reported() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                assert_eq!(highest_uid(connection, fixture.folder)?, None);
                for uid in [3, 9, 5] {
                    let mut message = header(&fixture, "x", uid, 0);
                    upsert_message(connection, &mut message)?;
                }
                assert_eq!(highest_uid(connection, fixture.folder)?, Some(9));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn threading_groups_messages_of_an_account() {
        let fixture = fixture();
        fixture
            .database
            .write(|connection| {
                let mut root = header(&fixture, "Thread", 1, 3);
                root.message_id = Some("<root@example.org>".to_string());
                upsert_message(connection, &mut root)?;

                let mut reply = header(&fixture, "Re: Thread", 2, 2);
                reply.message_id = Some("<reply@example.org>".to_string());
                reply.in_reply_to = Some("<root@example.org>".to_string());
                reply.references = vec!["<root@example.org>".to_string()];
                upsert_message(connection, &mut reply)?;

                let thread_id = root.thread_id.clone().unwrap();
                assert_eq!(reply.thread_id.as_deref(), Some(thread_id.as_str()));
                let thread = list_thread(connection, fixture.account, &thread_id)?;
                assert_eq!(thread.len(), 2);
                assert_eq!(thread[0].subject, "Thread");

                let ids = distinct_thread_ids(connection, fixture.account)?;
                assert_eq!(ids.len(), 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn snippets_and_body_markers_are_updated() {
        let fixture = fixture();
        let id = fixture
            .database
            .write(|connection| {
                let mut message = header(&fixture, "Snippet", 1, 0);
                upsert_message(connection, &mut message)?;
                set_snippet(connection, message.id, "hello world")?;
                set_body_cached(connection, message.id, true)?;
                Ok(message.id)
            })
            .unwrap();
        let loaded = fixture
            .database
            .read(|connection| get_message(connection, id))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.snippet.as_deref(), Some("hello world"));
        assert!(loaded.body_cached);
    }

    #[test]
    fn unknown_message_is_none() {
        let fixture = fixture();
        assert!(fixture
            .database
            .read(|connection| get_message(connection, MessageId::new()))
            .unwrap()
            .is_none());
        assert!(!fixture
            .database
            .read(|connection| exists(connection, MessageId::new()))
            .unwrap());
    }
}
