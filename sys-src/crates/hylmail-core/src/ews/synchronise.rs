//! Synchronisation of one Exchange folder.
//!
//! EWS synchronisation is cursor based rather than UID based, which makes it
//! both simpler and stricter than the IMAP path:
//!
//! * The server keeps a `SyncState` blob per folder.  HylMail stores it in
//!   [`Folder::sync_marker`] and hands it back on the next pass, so only the
//!   changes since the last pass are transferred — the equivalent of IMAP's
//!   `UID FETCH highest+1:*`.
//! * A pass **without** a cursor returns the whole folder content, which is what
//!   a first synchronisation needs.  Because that reply lists every item that
//!   exists, the same pass can also drop the local messages the server no longer
//!   has, so the "detect deletions" work of [`SyncDepth::Full`] comes for free.
//! * A pass **with** a cursor reports deletions explicitly as `<t:Delete>`
//!   elements, so removals are exact instead of being guessed from a UID set.
//! * `IncludesLastItemInRange` says whether the reply was truncated.  The loop
//!   keeps calling with the returned cursor until the server has sent everything
//!   or [`SyncOptions::max_new`] has been reached; the next pass continues from
//!   the stored cursor, so nothing is lost.
//!
//! Flag changes are the one place where EWS is awkward: a read-state change is
//! reported as a `<t:ReadFlagChange>` next to the items, so it is applied to the
//! already stored header instead of being folded into an item update.

use std::collections::{HashMap, HashSet};

use crate::db::{folders as folder_store, messages as message_store, Database};
use crate::error::Result;
use crate::imap::synchronise::{FolderSyncReport, SyncDepth, SyncOptions, SyncProgress};
use crate::models::folder::Folder;
use crate::models::ids::{AccountId, MessageId};
use crate::models::message::{MessageFlags, MessageQuery};

use super::convert::item_to_header;
use super::types::EwsItem;
use super::EwsClient;

/// Number of changes one `SyncFolderItems` call may return.
///
/// The server is free to return fewer; `IncludesLastItemInRange` tells the
/// client whether another call is needed.  512 keeps a single SOAP answer in the
/// range a slow on-premises server answers within its request timeout.
pub const MAX_CHANGES_RETURNED: u32 = 512;

// The comparison between the stored items and the ones the server reported is
// shared with the GroupWise path.
pub use crate::sync::reconcile::{removed_locally, LocalItem};

/// The read-state corrections a batch asks for.
///
/// Returns `(local id, flags with the new read state)` pairs, skipping the ones
/// that already agree so no statement is written for nothing.
#[must_use]
pub fn read_updates(
    local: &HashMap<String, LocalItem>,
    changes: &[(String, bool)],
) -> Vec<(MessageId, MessageFlags)> {
    let mut updates = Vec::new();
    for (server_id, is_read) in changes {
        let Some(item) = local.get(server_id) else {
            continue;
        };
        let mut flags = item.flags;
        if *is_read {
            flags.insert(MessageFlags::SEEN);
        } else {
            flags.remove(MessageFlags::SEEN);
        }
        if flags != item.flags && !updates.iter().any(|(id, _)| *id == item.id) {
            updates.push((item.id, flags));
        }
    }
    updates
}

/// `true` for the items that belong in the message list.
///
/// A mail folder can hold contacts, appointments and tasks as well; they are
/// left to the modules that handle them, but a meeting *request* is a real mail
/// message and is kept.
#[must_use]
pub fn is_storable(item: &EwsItem) -> bool {
    let class = item.item_class.to_ascii_lowercase();
    !(class.starts_with("ipm.contact")
        || class.starts_with("ipm.appointment")
        || class.starts_with("ipm.task")
        || class.starts_with("ipm.configuration"))
}

/// Synchronises one folder.
///
/// # Errors
///
/// A transport or protocol failure aborts the folder; a single message that
/// cannot be stored is skipped and logged, exactly like the IMAP path.  The
/// caller (the sync engine) decides whether to retry based on
/// [`crate::Error::is_transient`].
pub async fn sync_folder(
    client: &mut EwsClient,
    database: &Database,
    account_id: AccountId,
    folder: &Folder,
    options: SyncOptions,
    progress: &mut impl FnMut(SyncProgress),
) -> Result<FolderSyncReport> {
    let mut report = FolderSyncReport {
        folder_id: Some(folder.id),
        name: folder.name.clone(),
        ..FolderSyncReport::default()
    };
    let full = options.depth == SyncDepth::Full;
    progress(SyncProgress::FolderStarted {
        name: folder.name.clone(),
        full,
    });

    // The local state: what is already stored, and under which server id.
    let stored = database.read(|connection| {
        message_store::list_messages(
            connection,
            &MessageQuery {
                folder_ids: vec![folder.id],
                limit: None,
                ..MessageQuery::default()
            },
        )
    })?;
    let local: Vec<LocalItem> = stored.iter().filter_map(LocalItem::of).collect();
    let by_server_id: HashMap<String, LocalItem> = local
        .iter()
        .map(|item| (item.server_id.clone(), item.clone()))
        .collect();

    // A full pass pretends the folder was never synchronised, so the server
    // enumerates everything and the removals can be computed.
    let mut cursor: Option<String> = if full {
        None
    } else {
        folder
            .sync_marker
            .clone()
            .filter(|marker| !marker.is_empty())
    };
    let first_pass = cursor.is_none();

    let mut seen: HashSet<String> = HashSet::new();
    let mut counted: HashSet<MessageId> = HashSet::new();
    let mut deferred = false;

    loop {
        let batch = client
            .sync_items(&folder.path, cursor.as_deref(), MAX_CHANGES_RETURNED)
            .await?;

        let mut added_here = 0usize;
        for item in &batch.items {
            if !is_storable(item) {
                continue;
            }
            if options.max_new > 0 && report.new_messages >= options.max_new {
                deferred = true;
                break;
            }
            seen.insert(item.id.id.clone());
            let mut header = item_to_header(account_id, folder.id, item);
            if let Some(existing) = by_server_id.get(&item.id.id) {
                // Keep the row id stable so local-only flags survive.
                header.id = existing.id;
            }
            match database
                .write(|connection| message_store::upsert_message(connection, &mut header))
            {
                Ok(inserted) => {
                    if inserted {
                        report.new_messages += 1;
                        added_here += 1;
                    }
                }
                Err(error) => {
                    tracing::warn!(item = %item.id.id, %error, "an Exchange item could not be stored");
                }
            }
        }
        if added_here > 0 {
            // EWS has no UID space; the field is kept for the shared progress type.
            progress(SyncProgress::HeadersStored {
                added: added_here,
                highest_uid: 0,
            });
        }

        // Read-state changes are reported next to the items.
        let updates = read_updates(&by_server_id, &batch.read_changes);
        for (id, flags) in updates {
            if let Err(error) =
                database.write(|connection| message_store::set_flags(connection, id, flags))
            {
                tracing::warn!(%error, "a read state change could not be stored");
                continue;
            }
            report.updated_flags += 1;
        }
        if report.updated_flags > 0 {
            progress(SyncProgress::FlagsUpdated {
                updated: report.updated_flags,
            });
        }

        // Deletions the server reported for an incremental cursor.
        let mut removed_here = 0usize;
        for server_id in &batch.deleted_ids {
            if let Some(item) = by_server_id.get(server_id) {
                match database
                    .write(|connection| message_store::delete_message(connection, item.id))
                {
                    Ok(true) => {
                        counted.insert(item.id);
                        removed_here += 1;
                    }
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "a deleted item could not be dropped"),
                }
            }
            seen.insert(server_id.clone());
        }
        if removed_here > 0 {
            report.removed_messages += removed_here;
            progress(SyncProgress::MessagesRemoved {
                removed: removed_here,
            });
        }

        let previous = cursor.clone();
        if let Some(state) = batch.sync_state {
            cursor = Some(state);
        }
        if deferred || batch.includes_last_item {
            break;
        }
        // A server that answers with the same cursor without finishing would
        // make this loop spin forever.
        if cursor.is_some() && cursor == previous {
            tracing::warn!(
                folder = %folder.path,
                "Exchange stopped making progress; the folder is resumed next pass"
            );
            break;
        }
    }

    // Without a cursor the reply held every item of the folder, so whatever is
    // stored but was not reported is gone on the server.
    if first_pass && !deferred {
        let missing = removed_locally(&local, &seen);
        let mut removed = 0usize;
        for id in missing {
            if counted.contains(&id) {
                continue;
            }
            match database.write(|connection| message_store::delete_message(connection, id)) {
                Ok(true) => removed += 1,
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "a stale message could not be dropped"),
            }
        }
        if removed > 0 {
            report.removed_messages += removed;
            progress(SyncProgress::MessagesRemoved { removed });
        }
    }
    if deferred {
        progress(SyncProgress::Deferred { remaining: 0 });
    }

    // The cursor and the counts belong to the folder row.
    let folder_id = folder.id;
    let marker = cursor.clone();
    database.write(|connection| {
        folder_store::update_sync_marker(connection, folder_id, marker.as_deref())?;
        let (unread, total) = message_store::refresh_folder_counts(connection, folder_id)?;
        folder_store::update_counts(connection, folder_id, unread, total)?;
        Ok(())
    })?;

    progress(SyncProgress::FolderFinished {
        name: folder.name.clone(),
    });
    Ok(report)
}

/// Synchronises every selectable folder of an account.
///
/// Errors of a single folder are logged and collected, so one broken mailbox
/// cannot stop the whole account.
pub async fn sync_folders(
    client: &mut EwsClient,
    database: &Database,
    account_id: AccountId,
    folders: &[Folder],
    options: SyncOptions,
    progress: &mut impl FnMut(SyncProgress),
) -> Vec<Result<FolderSyncReport>> {
    let mut reports = Vec::new();
    for folder in folders {
        if !folder.is_selectable() {
            continue;
        }
        match sync_folder(client, database, account_id, folder, options, progress).await {
            Ok(report) => reports.push(Ok(report)),
            Err(error) if error.is_transient() => {
                tracing::warn!(folder = %folder.path, %error, "folder synchronisation failed");
                reports.push(Err(error));
                // A transient error usually means the session is gone; trying
                // the next folder would only pile up timeouts.
                break;
            }
            Err(error) => {
                tracing::warn!(folder = %folder.path, %error, "folder skipped");
                reports.push(Err(error));
            }
        }
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ids::{FolderId, MessageId};

    fn local(server_id: &str, flags: MessageFlags) -> LocalItem {
        LocalItem {
            id: MessageId::new(),
            server_id: server_id.to_string(),
            flags,
        }
    }

    #[test]
    fn a_full_pass_drops_the_messages_the_server_no_longer_has() {
        let items = vec![
            local("a", MessageFlags::empty()),
            local("b", MessageFlags::SEEN),
            local("c", MessageFlags::empty()),
        ];
        let seen: HashSet<String> = ["a".to_string(), "c".to_string()].into_iter().collect();
        let removed = removed_locally(&items, &seen);
        assert_eq!(removed, vec![items[1].id]);
        // Nothing is removed when the server reported everything.
        let all: HashSet<String> = items.iter().map(|i| i.server_id.clone()).collect();
        assert!(removed_locally(&items, &all).is_empty());
    }

    #[test]
    fn read_changes_only_touch_messages_that_actually_differ() {
        let one = local("a", MessageFlags::empty());
        let two = local("b", MessageFlags::SEEN);
        let index: HashMap<String, LocalItem> = [one.clone(), two.clone()]
            .into_iter()
            .map(|item| (item.server_id.clone(), item))
            .collect();

        let updates = read_updates(
            &index,
            &[
                ("a".to_string(), true),
                ("b".to_string(), true),
                ("unbekannt".to_string(), true),
            ],
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, one.id);
        assert!(updates[0].1.contains(MessageFlags::SEEN));

        // Marking unread removes the flag again.
        let updates = read_updates(&index, &[("b".to_string(), false)]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, two.id);
        assert!(!updates[0].1.contains(MessageFlags::SEEN));

        // A repeated report of the same state must not write twice.
        let updates = read_updates(&index, &[("a".to_string(), true), ("a".to_string(), true)]);
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn contacts_and_appointments_are_not_stored_as_messages() {
        fn item(class: &str) -> EwsItem {
            let xml = format!(
                "<t:Message xmlns:t=\"x\"><t:ItemId Id=\"i\"/>\
                 <t:ItemClass>{class}</t:ItemClass></t:Message>"
            );
            EwsItem::parse(&crate::soap::Node::parse(&xml).unwrap()).unwrap()
        }
        assert!(is_storable(&item("IPM.Note")));
        // A meeting request is a mail message.
        assert!(is_storable(&item("IPM.Schedule.Meeting.Request")));
        assert!(!is_storable(&item("IPM.Contact")));
        assert!(!is_storable(&item("IPM.Appointment")));
        assert!(!is_storable(&item("IPM.Task")));
        assert!(!is_storable(&item("ipm.configuration")));
    }

    #[test]
    fn a_header_without_a_server_id_is_not_a_local_item() {
        let header = crate::models::message::MessageHeader::new(AccountId::new(), FolderId::new());
        assert!(LocalItem::of(&header).is_none());
        let mut header = header;
        header.server_id = Some("item-1".to_string());
        let item = LocalItem::of(&header).unwrap();
        assert_eq!(item.server_id, "item-1");
        assert_eq!(item.id, header.id);
    }
}
