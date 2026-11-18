//! Translates EWS answers into HylMail's domain types.
//!
//! Exchange names things differently from IMAP: folders carry a class instead
//! of attributes, the well known folders have ids like `deleteditems` instead of
//! `\Trash`, and a message has no UID but an opaque item id.  This module is the
//! single place where those vocabularies meet, so the rest of the application
//! never has to know which protocol a message came from.
//!
//! The one field that needs explaining is [`Folder::path`]: for EWS it holds the
//! **server folder id** (`AAMkAGYz…`), exactly as it holds the IMAP mailbox name
//! for an IMAP account.  It is the identity of the folder, `db::folders` keys
//! rows on it, and it is what `FindFolder`/`SyncFolderItems` are given.  The
//! human readable path exists only while the tree is being discovered and is
//! used for the folder-kind heuristics; the UI rebuilds the display path from
//! `parent_id` and `name`.

use crate::models::folder::{Folder, FolderKind, SpecialUse};
use crate::models::ids::{AccountId, FolderId};
use crate::models::message::{MessageFlags, MessageHeader};

use super::types::{EwsFolder, EwsItem};

/// The distinguished folder ids Exchange defines, with the role they play.
///
/// Anything not listed here becomes a plain custom folder; the name based
/// heuristics of the IMAP path are applied as a fallback, so an installation
/// whose folders have no distinguished ids still yields a usable tree.
pub const DISTINGUISHED_FOLDERS: &[(&str, FolderKind, SpecialUse)] = &[
    ("inbox", FolderKind::Inbox, SpecialUse::Inbox),
    ("sentitems", FolderKind::Sent, SpecialUse::Sent),
    ("drafts", FolderKind::Drafts, SpecialUse::Drafts),
    ("deleteditems", FolderKind::Trash, SpecialUse::Trash),
    ("junkemail", FolderKind::Junk, SpecialUse::Junk),
    ("archive", FolderKind::Archive, SpecialUse::Archive),
    (
        "archivemsgfolderroot",
        FolderKind::Archive,
        SpecialUse::Archive,
    ),
    ("outbox", FolderKind::Outbox, SpecialUse::Sent),
];

/// Maps a distinguished folder id, or else a display path, onto a folder kind.
#[must_use]
pub fn folder_kind_of(distinguished: Option<&str>, display_path: &str) -> FolderKind {
    if let Some((_, kind, _)) = distinguished_folder(distinguished) {
        return kind;
    }
    crate::models::folder::detect_special_use(display_path)
        .map(FolderKind::from_special_use)
        .unwrap_or(FolderKind::Custom)
}

/// Maps a distinguished folder id, or else a display path, onto a special use.
#[must_use]
pub fn special_use_of(distinguished: Option<&str>, display_path: &str) -> Option<SpecialUse> {
    if let Some((_, _, use_)) = distinguished_folder(distinguished) {
        return Some(use_);
    }
    crate::models::folder::detect_special_use(display_path)
}

fn distinguished_folder(
    distinguished: Option<&str>,
) -> Option<(&'static str, FolderKind, SpecialUse)> {
    let id = distinguished?.to_ascii_lowercase();
    DISTINGUISHED_FOLDERS
        .iter()
        .find(|(name, _, _)| *name == id)
        .copied()
}

/// Converts one EWS folder into the local model.
///
/// `display_path` (`Projekte/Rechnungen`) is only used to guess the folder kind;
/// `parent_id` is filled in later by [`to_models`], because a `FindFolder` reply
/// orders the children before the parent.
#[must_use]
pub fn folder_to_model(account_id: AccountId, folder: &EwsFolder, display_path: &str) -> Folder {
    let distinguished = folder.distinguished_id.as_deref();
    let kind = folder_kind_of(distinguished, display_path);
    let name = if folder.display_name.is_empty() {
        display_path
            .rsplit('/')
            .next()
            .unwrap_or("Ordner")
            .to_string()
    } else {
        folder.display_name.clone()
    };
    let mut model = Folder::new(account_id, name, folder.id.id.clone(), kind);
    model.special_use = special_use_of(distinguished, display_path);
    model.total_count = folder.total_count;
    model.unread_count = folder.unread_count;
    // Calendar, contact and task folders are kept so the tree keeps its shape,
    // but they are never selected: HylMail has no view for them.
    model.selectable = folder.is_mail_folder();
    // An EWS `SyncState` is a cursor, not a UID space.
    model.uid_validity = None;
    model.uid_next = None;
    // Exchange ids are opaque, so the hierarchy delimiter carries no meaning.
    model.delimiter = None;
    model
}

/// Converts a discovered folder tree into models, parents linked and ordered.
///
/// `pairs` carries the display path next to every folder; it is the only place
/// the human readable hierarchy survives.
#[must_use]
pub fn to_models(account_id: AccountId, pairs: &[(EwsFolder, String)]) -> Vec<Folder> {
    let mut models: Vec<Folder> = pairs
        .iter()
        .map(|(folder, path)| folder_to_model(account_id, folder, path))
        .collect();
    let by_server_id: std::collections::HashMap<&str, FolderId> = pairs
        .iter()
        .zip(models.iter())
        .map(|((folder, _), model)| (folder.id.id.as_str(), model.id))
        .collect();
    for (index, (folder, _)) in pairs.iter().enumerate() {
        let Some(parent) = folder.parent_id.as_deref() else {
            continue;
        };
        if let Some(parent_id) = by_server_id.get(parent) {
            models[index].parent_id = Some(*parent_id);
        }
    }
    order_folders(&mut models);
    models
}

/// Sorts folders the way a mail client should display them.
///
/// The IMAP variant orders by path, which is only meaningful for a hierarchical
/// mailbox name; Exchange ids are opaque, so the name is used instead.
pub fn order_folders(folders: &mut [Folder]) {
    folders.sort_by(|left, right| {
        let left_key = (kind_rank(left.kind), left.name.to_lowercase());
        let right_key = (kind_rank(right.kind), right.name.to_lowercase());
        left_key.cmp(&right_key)
    });
    for (index, folder) in folders.iter_mut().enumerate() {
        folder.sort_order = i32::try_from(index).unwrap_or(i32::MAX);
    }
}

/// Sort rank of a folder: lower comes first.
fn kind_rank(kind: FolderKind) -> u8 {
    match kind {
        FolderKind::Inbox => 0,
        FolderKind::Sent => 1,
        FolderKind::Drafts => 2,
        FolderKind::Archive => 3,
        FolderKind::Custom => 4,
        FolderKind::Junk => 8,
        FolderKind::Trash => 9,
        FolderKind::Outbox | FolderKind::VirtualAllInboxes => 10,
    }
}

/// Converts an EWS item into a message header.
///
/// The item id becomes the `server_id`; the `uid` stays empty, exactly like the
/// schema intends for non-IMAP accounts.
#[must_use]
pub fn item_to_header(account_id: AccountId, folder_id: FolderId, item: &EwsItem) -> MessageHeader {
    let mut header = MessageHeader::new(account_id, folder_id);
    header.server_id = Some(item.id.id.clone());
    header.message_id.clone_from(&item.internet_message_id);
    header.in_reply_to.clone_from(&item.in_reply_to);
    header.subject.clone_from(&item.subject);
    header.from = item.from.clone().into_iter().collect();
    header.to.clone_from(&item.to);
    header.cc.clone_from(&item.cc);
    header.date = item.sent_at;
    header.received_at = item.received_at.or(item.sent_at);
    header.size_bytes = item.size_bytes;
    header.has_attachments = item.has_attachments;
    header.flags = flags_of(item);
    header.refresh_derived_fields();
    header
}

/// Builds the flag set of an item.
fn flags_of(item: &EwsItem) -> MessageFlags {
    let mut flags = MessageFlags::empty();
    if item.is_read {
        flags |= MessageFlags::SEEN;
    }
    if item.is_flagged {
        flags |= MessageFlags::FLAGGED;
    }
    // A meeting request is rendered with the calendar hint of the list.
    if item.item_class.to_ascii_lowercase().contains("schedule") {
        flags |= MessageFlags::INVITE;
    }
    flags
}

/// The distinguished folder id of a well known folder, if there is one.
#[must_use]
pub fn distinguished_for(kind: FolderKind) -> Option<&'static str> {
    DISTINGUISHED_FOLDERS
        .iter()
        .find(|(_, folder_kind, _)| *folder_kind == kind)
        .map(|(name, _, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soap::Node;

    fn folder(display_name: &str, distinguished: Option<&str>, parent: Option<&str>) -> EwsFolder {
        let distinguished = distinguished
            .map(|id| format!("<t:DistinguishedFolderId Id=\"{id}\"/>"))
            .unwrap_or_default();
        let parent = parent
            .map(|id| format!("<t:ParentFolderId Id=\"{id}\"/>"))
            .unwrap_or_default();
        let xml = format!(
            "<t:Folder xmlns:t=\"x\"><t:FolderId Id=\"id-{display_name}\"/>\
             {parent}{distinguished}<t:FolderClass>IPF.Note</t:FolderClass>\
             <t:DisplayName>{display_name}</t:DisplayName>\
             <t:TotalCount>5</t:TotalCount><t:UnreadCount>2</t:UnreadCount>\
             <t:ChildFolderCount>0</t:ChildFolderCount></t:Folder>"
        );
        EwsFolder::parse(&Node::parse(&xml).unwrap()).unwrap()
    }

    #[test]
    fn distinguished_ids_become_folder_kinds() {
        assert_eq!(
            folder_kind_of(Some("inbox"), "Posteingang"),
            FolderKind::Inbox
        );
        assert_eq!(
            folder_kind_of(Some("SentItems"), "Gesendet"),
            FolderKind::Sent
        );
        assert_eq!(
            folder_kind_of(Some("deleteditems"), "Gelöscht"),
            FolderKind::Trash
        );
        assert_eq!(folder_kind_of(Some("junkemail"), "Junk"), FolderKind::Junk);
        assert_eq!(
            folder_kind_of(Some("archive"), "Archiv"),
            FolderKind::Archive
        );
        assert_eq!(
            special_use_of(Some("deleteditems"), "Gelöscht"),
            Some(SpecialUse::Trash)
        );
        assert_eq!(distinguished_for(FolderKind::Inbox), Some("inbox"));
        assert_eq!(distinguished_for(FolderKind::Junk), Some("junkemail"));
        assert_eq!(distinguished_for(FolderKind::VirtualAllInboxes), None);
    }

    #[test]
    fn unknown_folders_fall_back_to_the_name_heuristics() {
        assert_eq!(folder_kind_of(None, "Gesendete Objekte"), FolderKind::Sent);
        assert_eq!(folder_kind_of(None, "Papierkorb"), FolderKind::Trash);
        assert_eq!(
            folder_kind_of(None, "Projekte/Rechnungen"),
            FolderKind::Custom
        );
        assert!(special_use_of(None, "Projekte").is_none());
    }

    #[test]
    fn a_folder_stores_its_server_id_as_the_path_and_keeps_the_display_name() {
        let account = AccountId::new();
        let model = folder_to_model(
            account,
            &folder("Rechnungen", None, None),
            "Projekte/Rechnungen",
        );
        assert_eq!(model.name, "Rechnungen");
        // The path is the server id, which is what EWS operations need.
        assert_eq!(model.path, "id-Rechnungen");
        assert!(model.delimiter.is_none());
        assert_eq!(model.total_count, 5);
        assert_eq!(model.unread_count, 2);
        assert!(model.selectable);
        assert!(model.uid_validity.is_none());
        assert!(model.sync_marker.is_none());
        assert_eq!(model.kind, FolderKind::Custom);
    }

    #[test]
    fn a_calendar_folder_is_stored_but_not_selectable() {
        let xml = "<t:Folder xmlns:t=\"x\"><t:FolderId Id=\"cal\"/>\
                   <t:FolderClass>IPF.Appointment</t:FolderClass>\
                   <t:DisplayName>Kalender</t:DisplayName></t:Folder>";
        let calendar = EwsFolder::parse(&Node::parse(xml).unwrap()).unwrap();
        let model = folder_to_model(AccountId::new(), &calendar, "Kalender");
        assert!(!model.selectable);
        assert!(!model.is_selectable());
    }

    #[test]
    fn to_models_links_parents_through_the_server_ids() {
        let account = AccountId::new();
        let pairs = vec![
            (
                folder("Rechnungen", None, Some("id-Projekte")),
                "Projekte/Rechnungen".to_string(),
            ),
            (folder("Projekte", None, None), "Projekte".to_string()),
            (
                folder("Posteingang", Some("inbox"), None),
                "Posteingang".to_string(),
            ),
        ];
        let models = to_models(account, &pairs);
        assert_eq!(models.len(), 3);
        let projects = models.iter().find(|f| f.name == "Projekte").unwrap();
        let invoices = models.iter().find(|f| f.name == "Rechnungen").unwrap();
        assert_eq!(invoices.parent_id, Some(projects.id));
        assert!(projects.parent_id.is_none());
        // The special folders are ordered first, then the tree by name.
        assert_eq!(models[0].kind, FolderKind::Inbox);
        assert_eq!(models[1].name, "Projekte");
        assert_eq!(models[2].name, "Rechnungen");
        assert_eq!(projects.sort_order, 1);
    }

    #[test]
    fn a_missing_parent_only_leaves_the_folder_a_root() {
        let pairs = vec![(
            folder("Waise", None, Some("id-verschwunden")),
            "Waise".to_string(),
        )];
        let models = to_models(AccountId::new(), &pairs);
        assert_eq!(models.len(), 1);
        assert!(models[0].parent_id.is_none());
    }

    #[test]
    fn an_item_becomes_a_header_with_its_flags() {
        let xml = "<t:Message xmlns:t=\"x\"><t:ItemId Id=\"item-1\" ChangeKey=\"ck\"/>\
            <t:Subject>Rechnung 4711</t:Subject>\
            <t:From><t:Mailbox><t:EmailAddress>ada@example.org</t:EmailAddress></t:Mailbox></t:From>\
            <t:ToRecipients><t:Mailbox><t:EmailAddress>bob@example.org</t:EmailAddress></t:Mailbox></t:ToRecipients>\
            <t:DateTimeSent>2026-11-12T13:00:00Z</t:DateTimeSent>\
            <t:DateTimeReceived>2026-11-12T13:00:05Z</t:DateTimeReceived>\
            <t:IsRead>true</t:IsRead><t:Size>2048</t:Size><t:HasAttachments>true</t:HasAttachments>\
            <t:InternetMessageId>&lt;rechnung@example.org&gt;</t:InternetMessageId>\
            <t:Flag><t:FlagStatus>Flagged</t:FlagStatus></t:Flag></t:Message>";
        let item = EwsItem::parse(&Node::parse(xml).unwrap()).unwrap();
        let folder_id = FolderId::new();
        let header = item_to_header(AccountId::new(), folder_id, &item);
        assert_eq!(header.server_id.as_deref(), Some("item-1"));
        assert!(header.uid.is_none());
        assert_eq!(header.folder_id, folder_id);
        assert_eq!(header.subject, "Rechnung 4711");
        assert_eq!(header.from[0].email, "ada@example.org");
        assert_eq!(header.message_id.as_deref(), Some("rechnung@example.org"));
        assert!(header.flags.contains(MessageFlags::SEEN));
        assert!(header.flags.contains(MessageFlags::FLAGGED));
        assert!(header.has_attachments);
        assert!(!header.is_unread());
        assert_eq!(
            header.received_at.unwrap().to_rfc3339(),
            "2026-11-12T13:00:05+00:00"
        );
        // The derived fields of the model are computed on the way in.
        assert_eq!(header.subject_normalized.as_deref(), Some("rechnung 4711"));
    }

    #[test]
    fn a_meeting_request_is_flagged_as_an_invite() {
        let xml = "<t:Message xmlns:t=\"x\"><t:ItemId Id=\"m\"/>\
                   <t:ItemClass>IPM.Schedule.Meeting.Request</t:ItemClass></t:Message>";
        let item = EwsItem::parse(&Node::parse(xml).unwrap()).unwrap();
        let header = item_to_header(AccountId::new(), FolderId::new(), &item);
        assert!(header.flags.contains(MessageFlags::INVITE));
        assert!(!header.flags.contains(MessageFlags::SEEN));
    }
}
