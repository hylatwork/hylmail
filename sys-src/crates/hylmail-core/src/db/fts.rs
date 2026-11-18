//! FTS5 index maintenance and query escaping.
//!
//! The index is a standalone FTS5 table that mirrors the searchable fields of a
//! message: subject, sender, recipients and - once the body has been fetched -
//! the plain text body.  It is maintained by the application instead of by
//! triggers because the body lives in a separate table and arrives later than
//! the header.

use rusqlite::{Connection, OptionalExtension};

use crate::error::Result;
use crate::models::{AccountId, MessageHeader, MessageId};

/// Turns a user typed search term into an FTS5 `MATCH` expression.
///
/// Every token is quoted (so that `AND`, `*` or `"` in the input cannot break
/// the query) and marked as a prefix query, which is what makes the search feel
/// incremental while the user is still typing.
#[must_use]
pub fn build_match_query(input: &str) -> Option<String> {
    let tokens: Vec<String> = input
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .map(|token| {
            let escaped = token.replace('"', "\"\"");
            format!("\"{escaped}\"*")
        })
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

/// Joins a list of addresses into the single text blob stored in the index.
fn join_addresses(addresses: &[crate::models::MailAddress]) -> String {
    let mut parts = Vec::with_capacity(addresses.len() * 2);
    for address in addresses {
        if let Some(name) = address.name.as_deref() {
            if !name.trim().is_empty() {
                parts.push(name.to_string());
            }
        }
        parts.push(address.email.clone());
    }
    parts.join(" ")
}

/// (Re)indexes a message.
///
/// The previous entry is removed first, so calling this repeatedly is safe and
/// is exactly what happens when the body arrives after the header.
pub fn index_message(
    connection: &Connection,
    header: &MessageHeader,
    body: Option<&str>,
) -> Result<()> {
    remove_message(connection, header.id)?;
    let mut recipients = join_addresses(&header.to);
    let cc = join_addresses(&header.cc);
    if !cc.is_empty() {
        recipients.push(' ');
        recipients.push_str(&cc);
    }
    connection.execute(
        "INSERT INTO messages_fts
             (message_id, account_id, folder_id, subject, sender, recipients, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            header.id.to_string(),
            header.account_id.to_string(),
            header.folder_id.to_string(),
            header.subject,
            join_addresses(&header.from),
            recipients,
            body.unwrap_or_default(),
        ],
    )?;
    Ok(())
}

/// Removes a message from the index.
pub fn remove_message(connection: &Connection, id: MessageId) -> Result<()> {
    connection.execute(
        "DELETE FROM messages_fts WHERE message_id = ?1",
        [id.to_string()],
    )?;
    Ok(())
}

/// Runs a full text query and returns the matching message ids.
///
/// Results are ordered by nothing in particular - callers re-fetch the headers
/// through [`super::messages::list_messages`] which applies the proper sort.
pub fn search(
    connection: &Connection,
    account_id: Option<AccountId>,
    query: &str,
    limit: u32,
) -> Result<Vec<MessageId>> {
    let Some(match_query) = build_match_query(query) else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    match account_id {
        Some(account_id) => {
            let mut statement = connection.prepare(
                "SELECT message_id FROM messages_fts
                 WHERE messages_fts MATCH ?1 AND account_id = ?2
                 LIMIT ?3",
            )?;
            let rows = statement.query_map(
                rusqlite::params![match_query, account_id.to_string(), i64::from(limit)],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                if let Ok(id) = MessageId::parse(&row?) {
                    ids.push(id);
                }
            }
        }
        None => {
            let mut statement = connection.prepare(
                "SELECT message_id FROM messages_fts
                 WHERE messages_fts MATCH ?1
                 LIMIT ?2",
            )?;
            let rows = statement
                .query_map(rusqlite::params![match_query, i64::from(limit)], |row| {
                    row.get::<_, String>(0)
                })?;
            for row in rows {
                if let Ok(id) = MessageId::parse(&row?) {
                    ids.push(id);
                }
            }
        }
    }
    Ok(ids)
}

/// Number of entries in the index.
pub fn indexed_count(connection: &Connection) -> Result<u64> {
    let value: i64 =
        connection.query_row("SELECT count(*) FROM messages_fts", [], |row| row.get(0))?;
    Ok(value.max(0) as u64)
}

/// `true` when the message is present in the index.
pub fn contains(connection: &Connection, id: MessageId) -> Result<bool> {
    let found: Option<String> = connection
        .query_row(
            "SELECT message_id FROM messages_fts WHERE message_id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{folders, Database};
    use crate::models::{Folder, FolderKind, MailAddress, MessageHeader};

    fn database_with_folder() -> (Database, AccountId, crate::models::FolderId) {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let folder = database
            .write(|connection| {
                let folder = Folder::new(account, "Inbox", "INBOX", FolderKind::Inbox);
                folders::upsert_folder(connection, &folder)?;
                Ok(folder)
            })
            .unwrap();
        (database, account, folder.id)
    }

    fn header(account: AccountId, folder: crate::models::FolderId) -> MessageHeader {
        let mut header = MessageHeader::new(account, folder);
        header.subject = "Quarterly report".to_string();
        header.from = vec![MailAddress::with_name("ada@example.org", "Ada Lovelace")];
        header.to = vec![MailAddress::new("bob@example.org")];
        header
    }

    #[test]
    fn empty_input_has_no_match_query() {
        assert!(build_match_query("").is_none());
        assert!(build_match_query("   \t ").is_none());
    }

    #[test]
    fn tokens_are_quoted_and_prefix_matched() {
        assert_eq!(build_match_query("hello"), Some("\"hello\"*".to_string()));
        assert_eq!(
            build_match_query("hello world"),
            Some("\"hello\"* \"world\"*".to_string())
        );
    }

    #[test]
    fn quotes_and_operators_cannot_break_the_query() {
        assert_eq!(
            build_match_query("say \"hi\""),
            Some("\"say\"* \"\"\"hi\"\"\"*".to_string())
        );
        // A bare `AND` would be an operator; quoting makes it a term.
        assert_eq!(build_match_query("AND"), Some("\"AND\"*".to_string()));
    }

    #[test]
    fn indexing_and_searching_round_trip() {
        let (database, account, folder) = database_with_folder();
        let header = header(account, folder);
        database
            .write(|connection| {
                index_message(connection, &header, None)?;
                assert!(contains(connection, header.id)?);
                assert_eq!(indexed_count(connection)?, 1);

                let by_subject = search(connection, None, "quarter", 10)?;
                assert_eq!(by_subject, vec![header.id]);
                let by_sender_name = search(connection, None, "lovelace", 10)?;
                assert_eq!(by_sender_name, vec![header.id]);
                let by_recipient = search(connection, None, "bob@example.org", 10)?;
                assert_eq!(by_recipient, vec![header.id]);
                let miss = search(connection, None, "nothing-matches", 10)?;
                assert!(miss.is_empty());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn body_is_searchable_once_indexed() {
        let (database, account, folder) = database_with_folder();
        let header = header(account, folder);
        database
            .write(|connection| {
                index_message(connection, &header, None)?;
                assert!(search(connection, None, "kangaroo", 10)?.is_empty());
                index_message(connection, &header, Some("a kangaroo appeared"))?;
                assert_eq!(search(connection, None, "kangaroo", 10)?.len(), 1);
                // Re-indexing must not duplicate the entry.
                assert_eq!(indexed_count(connection)?, 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn searching_can_be_restricted_to_one_account() {
        let (database, account, folder) = database_with_folder();
        let other = AccountId::new();
        let first = header(account, folder);
        let second = header(other, folder);
        database
            .write(|connection| {
                index_message(connection, &first, None)?;
                index_message(connection, &second, None)?;
                assert_eq!(search(connection, Some(account), "quarter", 10)?.len(), 1);
                assert_eq!(search(connection, None, "quarter", 10)?.len(), 2);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn limit_is_respected() {
        let (database, account, folder) = database_with_folder();
        database
            .write(|connection| {
                for index in 0..5 {
                    let mut header = header(account, folder);
                    header.subject = format!("Quarterly report {index}");
                    index_message(connection, &header, None)?;
                }
                assert_eq!(search(connection, None, "quarter", 2)?.len(), 2);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn removing_an_entry_works() {
        let (database, account, folder) = database_with_folder();
        let header = header(account, folder);
        database
            .write(|connection| {
                index_message(connection, &header, None)?;
                remove_message(connection, header.id)?;
                assert!(!contains(connection, header.id)?);
                assert_eq!(indexed_count(connection)?, 0);
                // Removing twice is harmless.
                remove_message(connection, header.id)?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn diacritics_are_folded() {
        let (database, account, folder) = database_with_folder();
        let mut header = header(account, folder);
        header.subject = "Über den Wolken".to_string();
        database
            .write(|connection| {
                index_message(connection, &header, None)?;
                assert_eq!(search(connection, None, "uber", 10)?.len(), 1);
                assert_eq!(search(connection, None, "Über", 10)?.len(), 1);
                Ok(())
            })
            .unwrap();
    }
}
