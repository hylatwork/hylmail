//! Address book persistence.
//!
//! Contacts arrive from three places: a CardDAV collection (phase 2), the
//! automatic "collected addresses" feature of the composer, and the manual
//! editor.  The raw vCard text is stored verbatim so that a synchronisation
//! round trip never drops properties HylMail does not understand.

use rusqlite::{Connection, OptionalExtension, Row};

use crate::error::Result;
use crate::models::ids::to_millis;
use crate::models::{AccountId, Contact, ContactEmail, ContactId, ContactSource, MailAddress};

const SELECT_COLUMNS: &str = "c.id, c.account_id, c.display_name, c.family_name, c.given_name, \
     c.emails_json, c.organization, c.vcard_raw, c.etag, c.href, c.source, c.last_seen";

fn source_to_str(source: ContactSource) -> &'static str {
    match source {
        ContactSource::CardDav => "carddav",
        ContactSource::Collected => "collected",
        ContactSource::Manual => "manual",
    }
}

fn source_from_str(value: &str) -> ContactSource {
    match value {
        "carddav" => ContactSource::CardDav,
        "manual" => ContactSource::Manual,
        _ => ContactSource::Collected,
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Contact> {
    let emails_json: String = row.get("emails_json")?;
    Ok(Contact {
        id: ContactId::parse(&row.get::<_, String>("id")?).unwrap_or_default(),
        account_id: row
            .get::<_, Option<String>>("account_id")?
            .and_then(|value| AccountId::parse(&value).ok()),
        display_name: row.get("display_name")?,
        family_name: row.get("family_name")?,
        given_name: row.get("given_name")?,
        emails: serde_json::from_str::<Vec<ContactEmail>>(&emails_json).unwrap_or_default(),
        organization: row.get("organization")?,
        vcard_raw: row.get("vcard_raw")?,
        etag: row.get("etag")?,
        href: row.get("href")?,
        source: source_from_str(&row.get::<_, String>("source")?),
        last_seen: crate::models::from_millis(row.get::<_, Option<i64>>("last_seen")?),
    })
}

/// Inserts or replaces a contact, keyed by its local id.
///
/// Returns `true` when the row was newly created.
pub fn upsert_contact(connection: &Connection, contact: &Contact) -> Result<bool> {
    let created = !exists(connection, contact.id)?;
    connection.execute(
        "INSERT INTO contacts (
             id, account_id, display_name, family_name, given_name, emails_json, organization,
             vcard_raw, etag, href, source, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT (id) DO UPDATE SET
             account_id   = excluded.account_id,
             display_name = excluded.display_name,
             family_name  = excluded.family_name,
             given_name   = excluded.given_name,
             emails_json  = excluded.emails_json,
             organization = excluded.organization,
             vcard_raw    = excluded.vcard_raw,
             etag         = excluded.etag,
             href         = excluded.href,
             source       = excluded.source,
             last_seen    = excluded.last_seen",
        rusqlite::params![
            contact.id.to_string(),
            contact.account_id.map(|id| id.to_string()),
            contact.display_name,
            contact.family_name,
            contact.given_name,
            serde_json::to_string(&contact.emails).unwrap_or_else(|_| "[]".to_string()),
            contact.organization,
            contact.vcard_raw,
            contact.etag,
            contact.href,
            source_to_str(contact.source),
            to_millis(contact.last_seen),
        ],
    )?;
    Ok(created)
}

/// Inserts or updates a remote contact identified by its `href`.
pub fn upsert_contact_by_href(connection: &Connection, contact: &mut Contact) -> Result<ContactId> {
    if let Some(href) = contact.href.as_deref() {
        let existing: Option<String> = connection
            .query_row(
                "SELECT id FROM contacts WHERE href = ?1 LIMIT 1",
                [href],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(raw) = existing {
            if let Ok(id) = ContactId::parse(&raw) {
                contact.id = id;
            }
        }
    }
    upsert_contact(connection, contact)?;
    Ok(contact.id)
}

/// `true` when the contact exists.
pub fn exists(connection: &Connection, id: ContactId) -> Result<bool> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM contacts WHERE id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Loads a contact by id.
pub fn get_contact(connection: &Connection, id: ContactId) -> Result<Option<Contact>> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM contacts c WHERE c.id = ?1");
    Ok(connection
        .query_row(&sql, [id.to_string()], from_row)
        .optional()?)
}

/// Lists the contacts of an account, or of every account when `None`.
pub fn list_contacts(
    connection: &Connection,
    account_id: Option<AccountId>,
) -> Result<Vec<Contact>> {
    let mut contacts = Vec::new();
    match account_id {
        Some(account_id) => {
            let sql = format!(
                "SELECT {SELECT_COLUMNS} FROM contacts c \
                 WHERE c.account_id = ?1 ORDER BY lower(c.display_name), c.id"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map([account_id.to_string()], from_row)?;
            for row in rows {
                contacts.push(row?);
            }
        }
        None => {
            let sql = format!(
                "SELECT {SELECT_COLUMNS} FROM contacts c ORDER BY lower(c.display_name), c.id"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map([], from_row)?;
            for row in rows {
                contacts.push(row?);
            }
        }
    }
    Ok(contacts)
}

/// Finds contacts whose name or address matches `query`.
///
/// Used by the composer autocomplete; `limit` keeps the query cheap while the
/// user types.
pub fn search_contacts(connection: &Connection, query: &str, limit: u32) -> Result<Vec<Contact>> {
    let needle = format!("%{}%", query.trim().to_lowercase());
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM contacts c \
         WHERE lower(COALESCE(c.display_name, '')) LIKE ?1 \
            OR lower(c.emails_json) LIKE ?1 \
         ORDER BY lower(COALESCE(c.display_name, '')), c.id LIMIT ?2"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params![needle, i64::from(limit)], from_row)?;
    let mut contacts = Vec::new();
    for row in rows {
        contacts.push(row?);
    }
    Ok(contacts)
}

/// Finds a contact by one of its e-mail addresses.
///
/// The lookup uses SQLite's JSON1 functions instead of a `LIKE` on the raw JSON
/// so that `ada@example.org` cannot accidentally match `ada@example.org.uk`.
pub fn find_by_email(connection: &Connection, email: &str) -> Result<Option<Contact>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM contacts c
         WHERE EXISTS (
             SELECT 1 FROM json_each(c.emails_json)
             WHERE lower(json_extract(value, '$.address')) = lower(?1)
         )
         ORDER BY c.id LIMIT 1"
    );
    Ok(connection
        .query_row(&sql, [email.trim()], from_row)
        .optional()?)
}

/// Stores a collected address, returning the id of the contact that holds it.
///
/// An address that is already known is only marked as "seen again"; otherwise a
/// new [`ContactSource::Collected`] entry is created.
pub fn collect_address(connection: &Connection, address: &MailAddress) -> Result<ContactId> {
    if let Some(existing) = find_by_email(connection, &address.email)? {
        connection.execute(
            "UPDATE contacts SET last_seen = ?2 WHERE id = ?1",
            rusqlite::params![
                existing.id.to_string(),
                to_millis(Some(crate::models::utc_now()))
            ],
        )?;
        return Ok(existing.id);
    }
    let contact = Contact::collected(address);
    upsert_contact(connection, &contact)?;
    Ok(contact.id)
}

/// Updates the `ETag` of a synchronised contact.
pub fn update_etag(connection: &Connection, id: ContactId, etag: Option<&str>) -> Result<()> {
    connection.execute(
        "UPDATE contacts SET etag = ?2 WHERE id = ?1",
        rusqlite::params![id.to_string(), etag],
    )?;
    Ok(())
}

/// Deletes a contact.
pub fn delete_contact(connection: &Connection, id: ContactId) -> Result<bool> {
    let affected = connection.execute("DELETE FROM contacts WHERE id = ?1", [id.to_string()])?;
    Ok(affected > 0)
}

/// Deletes every contact of an account.
pub fn delete_account_contacts(connection: &Connection, account_id: AccountId) -> Result<u64> {
    let affected = connection.execute(
        "DELETE FROM contacts WHERE account_id = ?1",
        [account_id.to_string()],
    )?;
    Ok(affected as u64)
}

/// Number of stored contacts.
pub fn count_contacts(connection: &Connection) -> Result<u64> {
    let value: i64 = connection.query_row("SELECT count(*) FROM contacts", [], |row| row.get(0))?;
    Ok(value.max(0) as u64)
}

/// Removes collected addresses that have not been seen for `days` days.
pub fn prune_collected(connection: &Connection, days: i64) -> Result<u64> {
    let cutoff = to_millis(Some(
        crate::models::utc_now() - chrono::Duration::days(days),
    ))
    .unwrap_or(0);
    let affected = connection.execute(
        "DELETE FROM contacts WHERE source = 'collected' AND COALESCE(last_seen, 0) < ?1",
        [cutoff],
    )?;
    Ok(affected as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn contacts_round_trip() {
        let database = Database::open_in_memory_migrated().unwrap();
        let mut contact = Contact::collected(&MailAddress::with_name("ada@example.org", "Ada"));
        contact.family_name = Some("Lovelace".to_string());
        contact.organization = Some("Example University".to_string());
        contact.vcard_raw = Some("BEGIN:VCARD\r\nX-UNKNOWN:keep me\r\nEND:VCARD\r\n".to_string());
        // The database stores timestamps with millisecond precision.
        contact.last_seen = crate::models::from_millis(crate::models::to_millis(contact.last_seen));

        database
            .read(|connection| {
                assert!(upsert_contact(connection, &contact)?);
                assert!(!upsert_contact(connection, &contact)?);
                let loaded = get_contact(connection, contact.id)?.unwrap();
                assert_eq!(loaded, contact);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn raw_vcard_is_preserved_verbatim() {
        let database = Database::open_in_memory_migrated().unwrap();
        let mut contact = Contact::collected(&MailAddress::new("ada@example.org"));
        contact.vcard_raw =
            Some("BEGIN:VCARD\r\nX-CUSTOM;PARAM=1:value\r\nEND:VCARD\r\n".to_string());
        database
            .read(|connection| {
                upsert_contact(connection, &contact)?;
                let loaded = get_contact(connection, contact.id)?.unwrap();
                assert_eq!(loaded.vcard_raw, contact.vcard_raw);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn remote_contacts_are_keyed_by_href() {
        let database = Database::open_in_memory_migrated().unwrap();
        let first = database
            .write(|connection| {
                let mut contact = Contact::collected(&MailAddress::new("ada@example.org"));
                contact.source = ContactSource::CardDav;
                contact.href = Some("https://dav.example.org/1.vcf".to_string());
                contact.etag = Some("\"v1\"".to_string());
                upsert_contact_by_href(connection, &mut contact)
            })
            .unwrap();

        let second = database
            .write(|connection| {
                let mut contact = Contact::collected(&MailAddress::new("ada@example.org"));
                contact.source = ContactSource::CardDav;
                contact.href = Some("https://dav.example.org/1.vcf".to_string());
                contact.etag = Some("\"v2\"".to_string());
                upsert_contact_by_href(connection, &mut contact)
            })
            .unwrap();

        assert_eq!(first, second);
        let loaded = database
            .read(|connection| get_contact(connection, first))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.etag.as_deref(), Some("\"v2\""));
        assert_eq!(database.read(count_contacts).unwrap(), 1);
    }

    #[test]
    fn searching_matches_name_and_address() {
        let database = Database::open_in_memory_migrated().unwrap();
        database
            .write(|connection| {
                let mut ada =
                    Contact::collected(&MailAddress::with_name("ada@example.org", "Ada Lovelace"));
                ada.id = ContactId::new();
                upsert_contact(connection, &ada)?;
                let bob = Contact::collected(&MailAddress::with_name("bob@other.org", "Bob"));
                upsert_contact(connection, &bob)?;
                Ok(())
            })
            .unwrap();
        database
            .read(|connection| {
                assert_eq!(search_contacts(connection, "lovelace", 10)?.len(), 1);
                assert_eq!(search_contacts(connection, "bob@", 10)?.len(), 1);
                assert_eq!(search_contacts(connection, "example.org", 10)?.len(), 1);
                assert!(search_contacts(connection, "nobody", 10)?.is_empty());
                assert_eq!(search_contacts(connection, "", 10)?.len(), 2);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn email_lookup_is_exact() {
        let database = Database::open_in_memory_migrated().unwrap();
        database
            .write(|connection| {
                upsert_contact(
                    connection,
                    &Contact::collected(&MailAddress::new("ada@example.org")),
                )
            })
            .unwrap();
        database
            .read(|connection| {
                assert!(find_by_email(connection, "ADA@example.org")?.is_some());
                assert!(find_by_email(connection, "ada@example.org.uk")?.is_none());
                assert!(find_by_email(connection, "missing@example.org")?.is_none());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn collecting_the_same_address_twice_reuses_the_contact() {
        let database = Database::open_in_memory_migrated().unwrap();
        let (first, second) = database
            .write(|connection| {
                let first = collect_address(connection, &MailAddress::new("ada@example.org"))?;
                let second = collect_address(connection, &MailAddress::new("ADA@example.org"))?;
                Ok((first, second))
            })
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(database.read(count_contacts).unwrap(), 1);
    }

    #[test]
    fn listing_can_be_restricted_to_an_account() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        database
            .write(|connection| {
                let mut owned = Contact::collected(&MailAddress::new("a@example.org"));
                owned.account_id = Some(account);
                upsert_contact(connection, &owned)?;
                upsert_contact(
                    connection,
                    &Contact::collected(&MailAddress::new("b@example.org")),
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(database.read(|c| list_contacts(c, None)).unwrap().len(), 2);
        assert_eq!(
            database
                .read(|c| list_contacts(c, Some(account)))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn deleting_contacts_is_reported() {
        let database = Database::open_in_memory_migrated().unwrap();
        let id = database
            .write(|connection| {
                let contact = Contact::collected(&MailAddress::new("ada@example.org"));
                upsert_contact(connection, &contact)?;
                Ok(contact.id)
            })
            .unwrap();
        assert!(database.write(|c| delete_contact(c, id)).unwrap());
        assert!(!database.write(|c| delete_contact(c, id)).unwrap());
    }

    #[test]
    fn account_contacts_can_be_dropped() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        database
            .write(|connection| {
                let mut owned = Contact::collected(&MailAddress::new("a@example.org"));
                owned.account_id = Some(account);
                upsert_contact(connection, &owned)?;
                assert_eq!(delete_account_contacts(connection, account)?, 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(database.read(count_contacts).unwrap(), 0);
    }

    #[test]
    fn stale_collected_addresses_are_pruned() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        database
            .write(|connection| {
                let mut old = Contact::collected(&MailAddress::new("old@example.org"));
                old.last_seen = Some(crate::models::utc_now() - chrono::Duration::days(400));
                upsert_contact(connection, &old)?;

                let mut fresh = Contact::collected(&MailAddress::new("fresh@example.org"));
                fresh.last_seen = Some(crate::models::utc_now());
                upsert_contact(connection, &fresh)?;

                let mut remote = Contact::collected(&MailAddress::new("dav@example.org"));
                remote.source = ContactSource::CardDav;
                remote.href = Some("https://dav.example.org/x.vcf".to_string());
                remote.last_seen = Some(crate::models::utc_now() - chrono::Duration::days(400));
                upsert_contact(connection, &remote)?;

                assert_eq!(prune_collected(connection, 365)?, 1);
                assert_eq!(count_contacts(connection)?, 2);
                let _ = account;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn etags_can_be_updated() {
        let database = Database::open_in_memory_migrated().unwrap();
        let id = database
            .write(|connection| {
                let mut contact = Contact::collected(&MailAddress::new("ada@example.org"));
                contact.source = ContactSource::CardDav;
                contact.href = Some("https://dav.example.org/a.vcf".to_string());
                upsert_contact(connection, &contact)?;
                update_etag(connection, contact.id, Some("\"etag\""))?;
                Ok(contact.id)
            })
            .unwrap();
        let loaded = database
            .read(|connection| get_contact(connection, id))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.etag.as_deref(), Some("\"etag\""));
    }
}
