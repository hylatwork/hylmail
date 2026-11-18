//! Synchronisation of one GroupWise folder.
//!
//! GroupWise has no delta protocol and no UID space: the proxy lists the items
//! of a folder page by page, and the sequence number of a folder is the only
//! change hint it offers.  A pass therefore
//!
//! 1. walks the pages of a folder with `getItemsRequest`,
//! 2. stores a header for every item it does not know yet,
//! 3. corrects the read state of the items that changed it elsewhere, and
//! 4. drops the local messages whose ids the server did not report.
//!
//! Step 4 is why a GroupWise pass is a *full* pass in the IMAP sense: the
//! enumeration is the only way to notice a deletion, so a folder is always
//! compared as a whole.  The comparison itself is cheap (a set of ids), and the
//! network cost is the listing, which happens anyway.

use std::collections::HashSet;

use crate::db::{folders as folder_store, messages as message_store, Database};
use crate::error::Result;
use crate::imap::synchronise::{FolderSyncReport, SyncProgress};
use crate::models::folder::Folder;
use crate::models::ids::{AccountId, MessageId};
use crate::models::message::{MessageFlags, MessageQuery};
use crate::sync::reconcile::{removed_locally, LocalItem};

use super::{item_to_header, GroupWiseClient};

/// Number of items one `getItemsRequest` asks for.
///
/// The proxy answers each request in a single SOAP document, so the window is
/// kept small enough for a slow on-premises server to answer inside its request
/// timeout while still being large enough to make a big mailbox a handful of
/// round trips.
pub const ITEM_PAGE_SIZE: u32 = 250;

/// Synchronises one folder.
///
/// # Errors
///
/// A transport or protocol failure aborts the folder; a single item that cannot
/// be stored is skipped and logged.  The caller (the sync engine) decides
/// whether to retry based on [`crate::Error::is_transient`].
pub async fn sync_folder(
    client: &mut GroupWiseClient,
    database: &Database,
    account_id: AccountId,
    folder: &Folder,
    max_new: usize,
    progress: &mut impl FnMut(SyncProgress),
) -> Result<FolderSyncReport> {
    let mut report = FolderSyncReport {
        folder_id: Some(folder.id),
        name: folder.name.clone(),
        // Every pass is a full pass: removals are always detected.
        uid_validity_reset: false,
        ..FolderSyncReport::default()
    };
    progress(SyncProgress::FolderStarted {
        name: folder.name.clone(),
        full: true,
    });

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
    let local: Vec<LocalItem> = LocalItem::all(&stored);
    let local_by_id: std::collections::HashMap<String, MessageId> = local
        .iter()
        .map(|item| (item.server_id.clone(), item.id))
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut removed: HashSet<MessageId> = HashSet::new();
    let mut offset = 0u32;
    // The total the server reports, when it reports one.
    let mut total = 0u32;
    let mut deferred = false;

    loop {
        let page = client
            .list_items(&folder.path, ITEM_PAGE_SIZE, offset)
            .await?;
        if page.is_empty() {
            break;
        }
        let count = page.items.len();
        let mut added_here = 0usize;
        for item in &page.items {
            if !item.is_mail() {
                continue;
            }
            seen.insert(item.id.clone());
            if max_new > 0 && report.new_messages >= max_new {
                deferred = true;
                continue;
            }
            match local_by_id.get(&item.id) {
                Some(existing) => {
                    // The item is known: only a read state that changed
                    // elsewhere needs writing.
                    let wanted = read_flags(item.is_read);
                    let current = stored
                        .iter()
                        .find(|header| header.id == *existing)
                        .map(|header| header.flags);
                    if let Some(current) = current {
                        if current.contains(MessageFlags::SEEN)
                            != wanted.contains(MessageFlags::SEEN)
                        {
                            match database.write(|connection| {
                                message_store::set_flags(connection, *existing, wanted)
                            }) {
                                Ok(()) => report.updated_flags += 1,
                                Err(error) => {
                                    tracing::warn!(%error, "a read state change could not be stored");
                                }
                            }
                        }
                    }
                }
                None => {
                    let mut header = item_to_header(account_id, folder.id, item);
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
                            tracing::warn!(item = %item.id, %error, "a GroupWise item could not be stored");
                        }
                    }
                }
            }
        }
        if added_here > 0 {
            // GroupWise has no UID space; the field is kept for the shared type.
            progress(SyncProgress::HeadersStored {
                added: added_here,
                highest_uid: 0,
            });
        }
        if page.total > 0 {
            total = page.total;
        }
        offset += u32::try_from(count).unwrap_or(u32::MAX);
        // The server tells how many items the folder holds, so paging ends when
        // the window passed the end; without a total, a short page is the end.
        let more = if total > 0 {
            offset < total
        } else {
            u32::try_from(count).unwrap_or(u32::MAX) == ITEM_PAGE_SIZE
        };
        if !more || deferred {
            break;
        }
    }

    if !deferred {
        for id in removed_locally(&local, &seen) {
            if removed.contains(&id) {
                continue;
            }
            match database.write(|connection| message_store::delete_message(connection, id)) {
                Ok(true) => {
                    removed.insert(id);
                    report.removed_messages += 1;
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "a stale message could not be dropped"),
            }
        }
        if report.removed_messages > 0 {
            progress(SyncProgress::MessagesRemoved {
                removed: report.removed_messages,
            });
        }
    } else {
        progress(SyncProgress::Deferred { remaining: 0 });
    }

    let folder_id = folder.id;
    database.write(|connection| {
        let (unread, total) = message_store::refresh_folder_counts(connection, folder_id)?;
        folder_store::update_counts(connection, folder_id, unread, total)?;
        Ok(())
    })?;

    progress(SyncProgress::FolderFinished {
        name: folder.name.clone(),
    });
    Ok(report)
}

/// The flags of an item with the given read state.
fn read_flags(is_read: bool) -> MessageFlags {
    let mut flags = MessageFlags::empty();
    if is_read {
        flags |= MessageFlags::SEEN;
    }
    flags
}

/// Synchronises every selectable folder of an account.
///
/// Errors of a single folder are logged and collected, so one broken folder
/// cannot stop the whole account.
pub async fn sync_folders(
    client: &mut GroupWiseClient,
    database: &Database,
    account_id: AccountId,
    folders: &[Folder],
    max_new: usize,
    progress: &mut impl FnMut(SyncProgress),
) -> Vec<Result<FolderSyncReport>> {
    let mut reports = Vec::new();
    for folder in folders {
        if !folder.is_selectable() {
            continue;
        }
        match sync_folder(client, database, account_id, folder, max_new, progress).await {
            Ok(report) => reports.push(Ok(report)),
            Err(error) if error.is_transient() => {
                tracing::warn!(folder = %folder.name, %error, "folder synchronisation failed");
                reports.push(Err(error));
                // A transient error usually means the session is gone; trying
                // the next folder would only pile up timeouts.
                break;
            }
            Err(error) => {
                tracing::warn!(folder = %folder.name, %error, "folder skipped");
                reports.push(Err(error));
            }
        }
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_state_maps_onto_the_seen_flag() {
        assert!(read_flags(true).contains(MessageFlags::SEEN));
        assert_eq!(read_flags(false), MessageFlags::empty());
        assert!(!read_flags(false).contains(MessageFlags::SEEN));
    }
}
