//! Reconciling a server side item list with what is stored locally.
//!
//! IMAP has UIDs and `UIDVALIDITY`, Exchange has a `SyncState` cursor and
//! GroupWise has neither: it lists the items of a folder page by page.  All
//! three end up asking the same two questions — "which messages are new?" and
//! "which stored messages are gone?" — and the second one is answered by
//! comparing the ids the server reported with the ids in the database.
//!
//! This module holds that comparison for the protocols that identify a message
//! by an opaque server id instead of a UID.

use std::collections::HashSet;

use crate::models::ids::MessageId;
use crate::models::message::{MessageFlags, MessageHeader};

/// A stored message and the server id it is filed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalItem {
    /// Local row id.
    pub id: MessageId,
    /// Server id, e.g. an EWS item id or a GroupWise item id.
    pub server_id: String,
    /// Flags as they are stored locally.
    pub flags: MessageFlags,
}

impl LocalItem {
    /// Reads the server id of a stored header.
    ///
    /// A message without a server id belongs to an IMAP account and is not part
    /// of this comparison.
    #[must_use]
    pub fn of(header: &MessageHeader) -> Option<Self> {
        Some(Self {
            id: header.id,
            server_id: header.server_id.clone()?,
            flags: header.flags,
        })
    }

    /// Reads the server ids of a whole folder.
    #[must_use]
    pub fn all(headers: &[MessageHeader]) -> Vec<Self> {
        headers.iter().filter_map(Self::of).collect()
    }
}

/// Stored messages the server did not report.
///
/// Only meaningful for a pass that enumerated the whole folder: when a reply was
/// incremental, `seen` holds just the changed items and the difference would
/// delete everything else.
#[must_use]
pub fn removed_locally(local: &[LocalItem], seen: &HashSet<String>) -> Vec<MessageId> {
    local
        .iter()
        .filter(|item| !seen.contains(&item.server_id))
        .map(|item| item.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ids::{AccountId, FolderId};

    fn local(server_id: &str) -> LocalItem {
        LocalItem {
            id: MessageId::new(),
            server_id: server_id.to_string(),
            flags: MessageFlags::SEEN,
        }
    }

    #[test]
    fn only_the_messages_the_server_still_reports_are_kept() {
        let items = vec![local("a"), local("b"), local("c")];
        let seen: HashSet<String> = ["a".to_string(), "c".to_string()].into_iter().collect();
        assert_eq!(removed_locally(&items, &seen), vec![items[1].id]);

        let everything: HashSet<String> = items.iter().map(|item| item.server_id.clone()).collect();
        assert!(removed_locally(&items, &everything).is_empty());

        let nothing = HashSet::new();
        assert_eq!(removed_locally(&items, &nothing).len(), 3);
        assert!(removed_locally(&[], &nothing).is_empty());
    }

    #[test]
    fn a_message_without_a_server_id_is_not_part_of_the_comparison() {
        let header = MessageHeader::new(AccountId::new(), FolderId::new());
        assert!(LocalItem::of(&header).is_none());
        let mut with_id = header;
        with_id.server_id = Some("item-1".to_string());
        let item = LocalItem::of(&with_id).unwrap();
        assert_eq!(item.server_id, "item-1");
        assert_eq!(item.id, with_id.id);
        assert_eq!(LocalItem::all(&[with_id]).len(), 1);
    }
}
