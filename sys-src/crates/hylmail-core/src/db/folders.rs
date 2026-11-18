//! Persistence of the folder tree.

use rusqlite::{Connection, OptionalExtension, Row};

use crate::error::Result;
use crate::models::{AccountId, Folder, FolderId, FolderKind, SpecialUse};

/// Stable textual representation of a [`FolderKind`] for SQLite.
#[must_use]
pub fn kind_to_str(kind: FolderKind) -> &'static str {
    match kind {
        FolderKind::Inbox => "inbox",
        FolderKind::Sent => "sent",
        FolderKind::Drafts => "drafts",
        FolderKind::Trash => "trash",
        FolderKind::Junk => "junk",
        FolderKind::Archive => "archive",
        FolderKind::Outbox => "outbox",
        FolderKind::VirtualAllInboxes => "virtual_all_inboxes",
        FolderKind::Custom => "custom",
    }
}

/// Parses a [`FolderKind`] written by [`kind_to_str`].
#[must_use]
pub fn kind_from_str(value: &str) -> FolderKind {
    match value {
        "inbox" => FolderKind::Inbox,
        "sent" => FolderKind::Sent,
        "drafts" => FolderKind::Drafts,
        "trash" => FolderKind::Trash,
        "junk" => FolderKind::Junk,
        "archive" => FolderKind::Archive,
        "outbox" => FolderKind::Outbox,
        "virtual_all_inboxes" => FolderKind::VirtualAllInboxes,
        _ => FolderKind::Custom,
    }
}

/// Applies the manual special-folder overrides (`FR-FOLDER-3`).
///
/// For each override the chosen folder takes the role and every other folder of
/// that role in the account is demoted to `Custom`, so a role can never be
/// claimed twice.  Returns how many rows changed.
pub fn apply_overrides(
    connection: &Connection,
    overrides: &[crate::models::FolderOverride],
) -> Result<u64> {
    let mut changed = 0u64;
    for entry in overrides {
        changed += connection.execute(
            "UPDATE folders SET kind = 'custom' \
             WHERE account_id = ?1 AND kind = ?2 AND id <> ?3",
            rusqlite::params![
                entry.account_id.to_string(),
                kind_to_str(entry.kind),
                entry.folder_id.to_string()
            ],
        )? as u64;
        changed += connection.execute(
            "UPDATE folders SET kind = ?2 WHERE id = ?1 AND account_id = ?3",
            rusqlite::params![
                entry.folder_id.to_string(),
                kind_to_str(entry.kind),
                entry.account_id.to_string()
            ],
        )? as u64;
    }
    Ok(changed)
}

/// Stable textual representation of a [`SpecialUse`].
#[must_use]
pub fn special_use_to_str(value: SpecialUse) -> &'static str {
    match value {
        SpecialUse::Inbox => "inbox",
        SpecialUse::All => "all",
        SpecialUse::Archive => "archive",
        SpecialUse::Drafts => "drafts",
        SpecialUse::Flagged => "flagged",
        SpecialUse::Junk => "junk",
        SpecialUse::Sent => "sent",
        SpecialUse::Trash => "trash",
    }
}

/// Parses a [`SpecialUse`] written by [`special_use_to_str`].
#[must_use]
pub fn special_use_from_str(value: &str) -> Option<SpecialUse> {
    match value {
        "inbox" => Some(SpecialUse::Inbox),
        "all" => Some(SpecialUse::All),
        "archive" => Some(SpecialUse::Archive),
        "drafts" => Some(SpecialUse::Drafts),
        "flagged" => Some(SpecialUse::Flagged),
        "junk" => Some(SpecialUse::Junk),
        "sent" => Some(SpecialUse::Sent),
        "trash" => Some(SpecialUse::Trash),
        _ => None,
    }
}

/// Maps a database row onto a [`Folder`].
fn from_row(row: &Row<'_>) -> rusqlite::Result<Folder> {
    let delimiter: Option<String> = row.get("delimiter")?;
    let special_use: Option<String> = row.get("special_use")?;
    Ok(Folder {
        id: FolderId::parse(&row.get::<_, String>("id")?).unwrap_or_default(),
        account_id: AccountId::parse(&row.get::<_, String>("account_id")?).unwrap_or_default(),
        parent_id: row
            .get::<_, Option<String>>("parent_id")?
            .and_then(|value| FolderId::parse(&value).ok()),
        name: row.get("name")?,
        path: row.get("path")?,
        delimiter: delimiter.and_then(|value| value.chars().next()),
        kind: kind_from_str(&row.get::<_, String>("kind")?),
        special_use: special_use.as_deref().and_then(special_use_from_str),
        unread_count: row.get::<_, i64>("unread_count")?.max(0) as u32,
        total_count: row.get::<_, i64>("total_count")?.max(0) as u32,
        uid_validity: row
            .get::<_, Option<i64>>("uid_validity")?
            .map(|value| value.max(0) as u32),
        uid_next: row
            .get::<_, Option<i64>>("uid_next")?
            .map(|value| value.max(0) as u32),
        sync_marker: row.get("sync_marker")?,
        sort_order: row.get::<_, i64>("sort_order")? as i32,
        selectable: row.get::<_, i64>("selectable")? != 0,
    })
}

const SELECT_COLUMNS: &str = "id, account_id, parent_id, name, path, delimiter, kind, \
     special_use, unread_count, total_count, uid_validity, uid_next, sync_marker, \
     sort_order, selectable";

/// Stores a whole folder list, parents before their children.
///
/// Two things make this more than a loop over [`upsert_folder`]:
///
/// * **Row ids are stable.** A folder is identified by its `(account_id, path)`
///   pair, so a folder that is discovered again keeps the id it already has in
///   the database.  Children refer to their parent by *model* id, so those
///   references are rewritten through the resolved ids before anything is
///   written; without that, a freshly built tree would point at ids that do not
///   exist and the `parent_id` foreign key would reject every child.
/// * **Order matters.** The foreign key is checked by every single statement, so
///   a parent has to be written before its children.  The list is therefore
///   emitted in topological order, which works for IMAP paths as well as for
///   Exchange folder ids where the path carries no hierarchy.
///
/// A cycle (a folder that is its own ancestor) cannot be stored without
/// violating the foreign key; the remaining rows are written in their original
/// order so the caller sees the database error rather than a silent drop.
///
/// # Errors
///
/// See [`upsert_folder`].
pub fn upsert_tree(connection: &Connection, folders: &[Folder]) -> Result<()> {
    let mut resolved: Vec<Folder> = folders.to_vec();
    let mut id_map: std::collections::HashMap<FolderId, FolderId> =
        std::collections::HashMap::new();
    for folder in &mut resolved {
        if let Some(existing) = find_folder_by_path(connection, folder.account_id, &folder.path)? {
            id_map.insert(folder.id, existing.id);
            folder.id = existing.id;
        }
    }
    for folder in &mut resolved {
        if let Some(parent) = folder.parent_id {
            folder.parent_id = Some(*id_map.get(&parent).unwrap_or(&parent));
        }
    }

    let mut written: std::collections::HashSet<FolderId> = std::collections::HashSet::new();
    let mut ordered: Vec<Folder> = Vec::with_capacity(resolved.len());
    let mut pending = resolved;
    while !pending.is_empty() {
        let mut deferred: Vec<Folder> = Vec::new();
        let mut progressed = false;
        for folder in pending {
            let ready = folder
                .parent_id
                .is_none_or(|parent| written.contains(&parent));
            if ready {
                written.insert(folder.id);
                ordered.push(folder);
                progressed = true;
            } else {
                deferred.push(folder);
            }
        }
        pending = deferred;
        if !progressed {
            // A cycle.  Writing the rest makes the foreign key fail loudly.
            ordered.append(&mut pending);
            break;
        }
    }

    for folder in &ordered {
        upsert_folder(connection, folder)?;
    }
    Ok(())
}

/// Inserts a folder, replacing an existing row with the same id.
pub fn upsert_folder(connection: &Connection, folder: &Folder) -> Result<()> {
    connection.execute(
        "INSERT INTO folders (
             id, account_id, parent_id, name, path, delimiter, kind, special_use,
             unread_count, total_count, uid_validity, uid_next, sync_marker,
             sort_order, selectable)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT (id) DO UPDATE SET
             parent_id      = excluded.parent_id,
             name           = excluded.name,
             path           = excluded.path,
             delimiter      = excluded.delimiter,
             kind           = excluded.kind,
             special_use    = excluded.special_use,
             unread_count   = excluded.unread_count,
             total_count    = excluded.total_count,
             uid_validity   = excluded.uid_validity,
             uid_next       = excluded.uid_next,
             sync_marker    = excluded.sync_marker,
             sort_order     = excluded.sort_order,
             selectable     = excluded.selectable",
        rusqlite::params![
            folder.id.to_string(),
            folder.account_id.to_string(),
            folder.parent_id.map(|id| id.to_string()),
            folder.name,
            folder.path,
            folder.delimiter.map(|c| c.to_string()),
            kind_to_str(folder.kind),
            folder.special_use.map(special_use_to_str),
            i64::from(folder.unread_count),
            i64::from(folder.total_count),
            folder.uid_validity.map(i64::from),
            folder.uid_next.map(i64::from),
            folder.sync_marker,
            i64::from(folder.sort_order),
            i64::from(folder.selectable),
        ],
    )?;
    Ok(())
}

/// Inserts or updates a folder identified by its `(account_id, path)` pair and
/// returns the row id that was used.
///
/// Folder ids are therefore stable across restarts even when the server
/// reports the same mailbox again.
pub fn upsert_folder_by_path(connection: &Connection, folder: &mut Folder) -> Result<FolderId> {
    if let Some(existing) = find_folder_by_path(connection, folder.account_id, &folder.path)? {
        folder.id = existing.id;
    }
    upsert_folder(connection, folder)?;
    Ok(folder.id)
}

/// Loads a folder by id.
pub fn get_folder(connection: &Connection, id: FolderId) -> Result<Option<Folder>> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM folders WHERE id = ?1");
    Ok(connection
        .query_row(&sql, [id.to_string()], from_row)
        .optional()?)
}

/// Loads a folder by its path inside an account.
pub fn find_folder_by_path(
    connection: &Connection,
    account_id: AccountId,
    path: &str,
) -> Result<Option<Folder>> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM folders WHERE account_id = ?1 AND path = ?2");
    Ok(connection
        .query_row(
            &sql,
            rusqlite::params![account_id.to_string(), path],
            from_row,
        )
        .optional()?)
}

/// Lists the folders of one account, ordered by path.
pub fn list_folders(connection: &Connection, account_id: AccountId) -> Result<Vec<Folder>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM folders WHERE account_id = ?1 \
         ORDER BY sort_order, path"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([account_id.to_string()], from_row)?;
    let mut folders = Vec::new();
    for row in rows {
        folders.push(row?);
    }
    Ok(folders)
}

/// Lists the folders of every account.
pub fn list_all_folders(connection: &Connection) -> Result<Vec<Folder>> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM folders ORDER BY account_id, sort_order, path");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], from_row)?;
    let mut folders = Vec::new();
    for row in rows {
        folders.push(row?);
    }
    Ok(folders)
}

/// Lists the direct children of a folder.
pub fn list_children(connection: &Connection, parent_id: FolderId) -> Result<Vec<Folder>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM folders WHERE parent_id = ?1 ORDER BY sort_order, name"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([parent_id.to_string()], from_row)?;
    let mut folders = Vec::new();
    for row in rows {
        folders.push(row?);
    }
    Ok(folders)
}

/// Deletes a folder; its messages are removed by the foreign key cascade.
pub fn delete_folder(connection: &Connection, id: FolderId) -> Result<bool> {
    let affected = connection.execute("DELETE FROM folders WHERE id = ?1", [id.to_string()])?;
    Ok(affected > 0)
}

/// Updates only the unread and total counters of a folder.
pub fn update_counts(connection: &Connection, id: FolderId, unread: u32, total: u32) -> Result<()> {
    connection.execute(
        "UPDATE folders SET unread_count = ?2, total_count = ?3 WHERE id = ?1",
        rusqlite::params![id.to_string(), i64::from(unread), i64::from(total)],
    )?;
    Ok(())
}

/// Updates the IMAP synchronisation state of a folder.
pub fn update_uid_state(
    connection: &Connection,
    id: FolderId,
    uid_validity: Option<u32>,
    uid_next: Option<u32>,
) -> Result<()> {
    connection.execute(
        "UPDATE folders SET uid_validity = ?2, uid_next = ?3 WHERE id = ?1",
        rusqlite::params![
            id.to_string(),
            uid_validity.map(i64::from),
            uid_next.map(i64::from)
        ],
    )?;
    Ok(())
}

/// Updates the opaque synchronisation marker of a folder.
pub fn update_sync_marker(
    connection: &Connection,
    id: FolderId,
    marker: Option<&str>,
) -> Result<()> {
    connection.execute(
        "UPDATE folders SET sync_marker = ?2 WHERE id = ?1",
        rusqlite::params![id.to_string(), marker],
    )?;
    Ok(())
}

/// Number of folders of an account.
pub fn count_folders(connection: &Connection, account_id: AccountId) -> Result<u64> {
    let value: i64 = connection.query_row(
        "SELECT count(*) FROM folders WHERE account_id = ?1",
        [account_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(value.max(0) as u64)
}

/// Deletes every folder of an account.
pub fn delete_account_folders(connection: &Connection, account_id: AccountId) -> Result<u64> {
    let affected = connection.execute(
        "DELETE FROM folders WHERE account_id = ?1",
        [account_id.to_string()],
    )?;
    Ok(affected as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn folder(account: AccountId, name: &str, path: &str, kind: FolderKind) -> Folder {
        Folder::new(account, name, path, kind)
    }

    #[test]
    fn kind_and_special_use_round_trip() {
        for kind in [
            FolderKind::Inbox,
            FolderKind::Sent,
            FolderKind::Drafts,
            FolderKind::Trash,
            FolderKind::Junk,
            FolderKind::Archive,
            FolderKind::Outbox,
            FolderKind::VirtualAllInboxes,
            FolderKind::Custom,
        ] {
            assert_eq!(kind_from_str(kind_to_str(kind)), kind);
        }
        for use_ in [
            SpecialUse::Inbox,
            SpecialUse::All,
            SpecialUse::Archive,
            SpecialUse::Drafts,
            SpecialUse::Flagged,
            SpecialUse::Junk,
            SpecialUse::Sent,
            SpecialUse::Trash,
        ] {
            assert_eq!(special_use_from_str(special_use_to_str(use_)), Some(use_));
        }
        assert!(special_use_from_str("nonsense").is_none());
        assert_eq!(kind_from_str("nonsense"), FolderKind::Custom);
    }

    #[test]
    fn folders_round_trip_through_the_database() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let mut folder = folder(account, "Archive", "Archiv", FolderKind::Archive);
        folder.delimiter = Some('/');
        folder.special_use = Some(SpecialUse::Archive);
        folder.unread_count = 3;
        folder.total_count = 42;
        folder.uid_validity = Some(7);
        folder.uid_next = Some(100);
        folder.sync_marker = Some("marker".to_string());
        folder.sort_order = 5;
        folder.selectable = false;
        folder.parent_id = None;

        database
            .read(|connection| {
                upsert_folder(connection, &folder)?;
                let loaded = get_folder(connection, folder.id)?.unwrap();
                assert_eq!(loaded, folder);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_child_listed_before_its_parent_is_still_stored() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        // The display order `imap::mailbox::build_tree` produces: the special
        // use folder `Sent` outranks its plain parent, so the child comes first.
        let mut inbox = folder(account, "Inbox", "INBOX", FolderKind::Inbox);
        inbox.delimiter = Some('/');
        let mut parent = folder(account, "Projekte", "Projekte", FolderKind::Custom);
        parent.delimiter = Some('/');
        let parent_id = parent.id;
        let mut child = folder(account, "Gesendet", "Projekte/Gesendet", FolderKind::Sent);
        child.delimiter = Some('/');
        child.parent_id = Some(parent_id);

        database
            .write(|connection| {
                upsert_tree(connection, &[inbox, child, parent])?;
                let stored = get_folder(connection, parent_id)?.unwrap();
                assert_eq!(stored.path, "Projekte");
                let stored_child =
                    find_folder_by_path(connection, account, "Projekte/Gesendet")?.unwrap();
                assert_eq!(stored_child.parent_id, Some(parent_id));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn upserting_the_same_path_keeps_the_id() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let first = database
            .write(|connection| {
                let mut folder = folder(account, "Inbox", "INBOX", FolderKind::Inbox);
                let id = upsert_folder_by_path(connection, &mut folder)?;
                Ok(id)
            })
            .unwrap();

        let second = database
            .write(|connection| {
                let mut folder = folder(account, "Inbox", "INBOX", FolderKind::Inbox);
                folder.unread_count = 9;
                let id = upsert_folder_by_path(connection, &mut folder)?;
                Ok(id)
            })
            .unwrap();

        assert_eq!(first, second);
        let loaded = database
            .read(|connection| get_folder(connection, first))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.unread_count, 9);
        assert_eq!(
            database
                .read(|connection| count_folders(connection, account))
                .unwrap(),
            1
        );
    }

    #[test]
    fn listing_orders_by_sort_order_then_path() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        database
            .write(|connection| {
                let mut late = folder(account, "Zeta", "Zeta", FolderKind::Custom);
                late.sort_order = 10;
                upsert_folder(connection, &late)?;
                let mut early = folder(account, "Alpha", "Alpha", FolderKind::Custom);
                early.sort_order = 1;
                upsert_folder(connection, &early)?;
                Ok(())
            })
            .unwrap();
        let folders = database
            .read(|connection| list_folders(connection, account))
            .unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].name, "Alpha");
        assert_eq!(folders[1].name, "Zeta");
    }

    #[test]
    fn children_are_listed_by_parent() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let parent = FolderId::new();
        database
            .write(|connection| {
                let mut root = folder(account, "Projects", "Projects", FolderKind::Custom);
                root.id = parent;
                upsert_folder(connection, &root)?;
                let mut child = folder(account, "2024", "Projects/2024", FolderKind::Custom);
                child.parent_id = Some(parent);
                upsert_folder(connection, &child)?;
                Ok(())
            })
            .unwrap();
        let children = database
            .read(|connection| list_children(connection, parent))
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "2024");
    }

    #[test]
    fn counters_and_sync_state_are_updated_in_place() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let id = database
            .write(|connection| {
                let mut folder = folder(account, "Inbox", "INBOX", FolderKind::Inbox);
                let id = upsert_folder_by_path(connection, &mut folder)?;
                update_counts(connection, id, 12, 300)?;
                update_uid_state(connection, id, Some(1), Some(400))?;
                update_sync_marker(connection, id, Some("abc"))?;
                Ok(id)
            })
            .unwrap();
        let loaded = database
            .read(|connection| get_folder(connection, id))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.unread_count, 12);
        assert_eq!(loaded.total_count, 300);
        assert_eq!(loaded.uid_validity, Some(1));
        assert_eq!(loaded.uid_next, Some(400));
        assert_eq!(loaded.sync_marker.as_deref(), Some("abc"));
    }

    #[test]
    fn deleting_a_folder_reports_whether_it_existed() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let id = database
            .write(|connection| {
                let mut folder = folder(account, "Inbox", "INBOX", FolderKind::Inbox);
                upsert_folder_by_path(connection, &mut folder)
            })
            .unwrap();
        assert!(database.write(|c| delete_folder(c, id)).unwrap());
        assert!(!database.write(|c| delete_folder(c, id)).unwrap());
        assert_eq!(
            database
                .read(|connection| count_folders(connection, account))
                .unwrap(),
            0
        );
    }

    #[test]
    fn account_folders_can_be_dropped_in_one_go() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let other = AccountId::new();
        database
            .write(|connection| {
                upsert_folder(
                    connection,
                    &folder(account, "Inbox", "INBOX", FolderKind::Inbox),
                )?;
                upsert_folder(
                    connection,
                    &folder(other, "Inbox", "INBOX", FolderKind::Inbox),
                )?;
                assert_eq!(delete_account_folders(connection, account)?, 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            database
                .read(|connection| count_folders(connection, other))
                .unwrap(),
            1
        );
    }

    #[test]
    fn listing_all_folders_covers_every_account() {
        let database = Database::open_in_memory_migrated().unwrap();
        database
            .write(|connection| {
                upsert_folder(
                    connection,
                    &folder(AccountId::new(), "A", "A", FolderKind::Custom),
                )?;
                upsert_folder(
                    connection,
                    &folder(AccountId::new(), "B", "B", FolderKind::Custom),
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(database.read(list_all_folders).unwrap().len(), 2);
    }

    #[test]
    fn unknown_folder_is_none() {
        let database = Database::open_in_memory_migrated().unwrap();
        assert!(database
            .read(|connection| get_folder(connection, FolderId::new()))
            .unwrap()
            .is_none());
        assert!(database
            .read(|connection| find_folder_by_path(connection, AccountId::new(), "INBOX"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn overrides_reassign_a_role_without_leaving_two_claims() {
        let database = Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let detected = Folder::new(account, "Spamverdacht", "Junk", FolderKind::Junk);
        let chosen = Folder::new(account, "Werbung", "Werbung", FolderKind::Custom);
        database
            .write(|connection| {
                upsert_folder(connection, &detected)?;
                upsert_folder(connection, &chosen)?;
                Ok(())
            })
            .unwrap();

        let overrides = vec![crate::models::FolderOverride {
            account_id: account,
            kind: FolderKind::Junk,
            folder_id: chosen.id,
        }];
        database
            .write(|connection| apply_overrides(connection, &overrides))
            .unwrap();

        database
            .read(|connection| {
                assert_eq!(
                    get_folder(connection, chosen.id)?.unwrap().kind,
                    FolderKind::Junk
                );
                // The previously detected junk folder stepped aside.
                assert_eq!(
                    get_folder(connection, detected.id)?.unwrap().kind,
                    FolderKind::Custom
                );
                Ok(())
            })
            .unwrap();
    }
}
