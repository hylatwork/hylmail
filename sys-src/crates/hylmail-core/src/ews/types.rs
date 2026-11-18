//! The EWS shapes HylMail reads and writes.
//!
//! Only the subset of the Exchange Web Services schema the client needs is
//! modelled.  Parsing goes through the [`Node`] API, which strips namespace
//! prefixes, so `t:Message` and `Message` are the same element here.
//!
//! Two properties of EWS shape this module:
//!
//! * **An item identity is a pair.** Every message is addressed by `Id` *and*
//!   `ChangeKey`; the change key changes with every update.  Updates may omit it,
//!   so HylMail stores the `Id` as the local `server_id` and simply omits the
//!   change key when it updates an item.
//! * **Timestamps are strings.** `DateTimeReceived` is ISO 8601, sometimes
//!   without a zone when the server has a `TimeZoneContext`.  A timestamp that
//!   cannot be parsed is not an error: the header is stored without a date
//!   rather than dropping the message.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

use crate::models::message::MailAddress;
use crate::soap::Node;

/// An EWS `FolderId` or `ItemId` pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemId {
    /// Opaque, base64-like server identifier.
    pub id: String,
    /// Version marker of the item, when the server sent one.
    pub change_key: Option<String>,
}

impl ItemId {
    /// Reads an `Id`/`ChangeKey` element (folder or item, the shape is the same).
    #[must_use]
    pub fn parse(node: &Node) -> Option<Self> {
        let id = node.attr("Id")?.to_string();
        if id.is_empty() {
            return None;
        }
        Some(Self {
            id,
            change_key: node.attr("ChangeKey").map(ToString::to_string),
        })
    }
}

/// One folder as `FindFolder` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EwsFolder {
    /// Folder identity.
    pub id: ItemId,
    /// Identity of the parent folder, when the server sent one.
    pub parent_id: Option<String>,
    /// Display name, e.g. `Posteingang`.
    pub display_name: String,
    /// Folder class, e.g. `IPF.Note` for mail and `IPF.Appointment` for calendars.
    pub folder_class: String,
    /// `DistinguishedFolderId` the server used, when it is a well known folder.
    pub distinguished_id: Option<String>,
    /// Number of items in the folder.
    pub total_count: u32,
    /// Number of unread items in the folder.
    pub unread_count: u32,
    /// Number of immediate subfolders.
    pub child_folder_count: u32,
}

impl EwsFolder {
    /// Parses one `<t:Folder>` element.
    #[must_use]
    pub fn parse(node: &Node) -> Option<Self> {
        let id = ItemId::parse(node.child("FolderId")?)?;
        let distinguished_id = node
            .child("DistinguishedFolderId")
            .and_then(|node| node.attr("Id"))
            .map(ToString::to_string);
        Some(Self {
            id,
            parent_id: node
                .child("ParentFolderId")
                .and_then(|node| node.attr("Id"))
                .map(ToString::to_string),
            display_name: node
                .find_text(&["DisplayName"])
                .unwrap_or_default()
                .to_string(),
            folder_class: node
                .find_text(&["FolderClass"])
                .unwrap_or_default()
                .to_string(),
            distinguished_id,
            total_count: node
                .find_u64(&["TotalCount"])
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
            unread_count: node
                .find_u64(&["UnreadCount"])
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
            child_folder_count: node
                .find_u64(&["ChildFolderCount"])
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
        })
    }

    /// `true` for the folders HylMail shows as mail folders.
    ///
    /// A calendar, contact or task folder belongs to a different module and is
    /// stored as a non-selectable folder so the tree keeps its shape.
    #[must_use]
    pub fn is_mail_folder(&self) -> bool {
        self.folder_class.is_empty() || self.folder_class == "IPF.Note"
    }

    /// `true` for the synthetic roots Exchange adds to every mailbox.
    #[must_use]
    pub fn is_technical_root(&self) -> bool {
        matches!(
            self.distinguished_id.as_deref(),
            Some("msgfolderroot" | "root" | "searchfolders" | "freebusy" | "publicfoldersroot")
        )
    }
}

/// One message as `SyncFolderItems` or `FindItem` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EwsItem {
    /// Item identity.
    pub id: ItemId,
    /// Folder the item currently lives in.
    pub parent_folder_id: Option<String>,
    /// Subject, unprefixed.
    pub subject: String,
    /// Item class, e.g. `IPM.Note` for mail.
    pub item_class: String,
    /// Sender.
    pub from: Option<MailAddress>,
    /// `To:` recipients.
    pub to: Vec<MailAddress>,
    /// `Cc:` recipients.
    pub cc: Vec<MailAddress>,
    /// `DateTimeSent`.
    pub sent_at: Option<DateTime<Utc>>,
    /// `DateTimeReceived`.
    pub received_at: Option<DateTime<Utc>>,
    /// `Size` in bytes.
    pub size_bytes: u64,
    /// `IsRead`.
    pub is_read: bool,
    /// `Flag/FlagStatus` is `Flagged`.
    pub is_flagged: bool,
    /// `HasAttachments`.
    pub has_attachments: bool,
    /// `InternetMessageId`, the RFC 5322 `Message-ID`.
    pub internet_message_id: Option<String>,
    /// `InReplyTo`.
    pub in_reply_to: Option<String>,
    /// `ConversationId` - Exchange's own thread key.
    pub conversation_id: Option<String>,
}

impl EwsItem {
    /// Parses one `<t:Message>` (or `<t:Item>`) element.
    #[must_use]
    pub fn parse(node: &Node) -> Option<Self> {
        let id = ItemId::parse(node.child("ItemId").or_else(|| node.child("FolderId"))?)?;
        let mut item = Self {
            id,
            parent_folder_id: node
                .child("ParentFolderId")
                .and_then(|node| node.attr("Id"))
                .map(ToString::to_string),
            subject: node.find_text(&["Subject"]).unwrap_or_default().to_string(),
            item_class: node
                .find_text(&["ItemClass"])
                .unwrap_or_default()
                .to_string(),
            from: node.child("From").and_then(mailbox),
            to: recipients(node, "ToRecipients"),
            cc: recipients(node, "CcRecipients"),
            sent_at: timestamp(node, "DateTimeSent"),
            received_at: timestamp(node, "DateTimeReceived"),
            size_bytes: node.find_u64(&["Size"]).unwrap_or(0),
            is_read: node.find_bool(&["IsRead"]).unwrap_or(false),
            is_flagged: node
                .find_text(&["Flag", "FlagStatus"])
                .is_some_and(|status| status.eq_ignore_ascii_case("Flagged")),
            has_attachments: node.find_bool(&["HasAttachments"]).unwrap_or(false),
            internet_message_id: node
                .find_text(&["InternetMessageId"])
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            in_reply_to: node
                .find_text(&["InReplyTo"])
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            conversation_id: node
                .child("ConversationId")
                .and_then(|node| node.attr("Id"))
                .map(ToString::to_string),
        };
        // `Message-ID` headers arrive without the angle brackets some servers
        // add; normalising here keeps the thread builder happy.
        if let Some(message_id) = item.internet_message_id.take() {
            item.internet_message_id = Some(normalise_message_id(&message_id));
        }
        if let Some(in_reply_to) = item.in_reply_to.take() {
            item.in_reply_to = Some(normalise_message_id(&in_reply_to));
        }
        Some(item)
    }

    /// `true` when the item is a mail message rather than a meeting request,
    /// contact or task stored in a mail folder.
    #[must_use]
    pub fn is_mail(&self) -> bool {
        self.item_class.is_empty()
            || self.item_class.eq_ignore_ascii_case("IPM.Note")
            || self.item_class.to_ascii_lowercase().starts_with("ipm.note")
    }
}

/// The result of one `SyncFolderItems` call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncBatch {
    /// Items that were created or changed since the stored sync state.
    pub items: Vec<EwsItem>,
    /// Server ids of the items the server reported as deleted.
    pub deleted_ids: Vec<String>,
    /// Read-state changes, which EWS reports separately from the items.
    pub read_changes: Vec<(String, bool)>,
    /// New sync state to store; `None` when the server sent none.
    pub sync_state: Option<String>,
    /// `IncludesLastItemInRange`: `false` means another call with the returned
    /// sync state is needed to see the rest of the changes.
    pub includes_last_item: bool,
}

impl SyncBatch {
    /// Parses a `SyncFolderItems` (or `FindItem`) response message.
    #[must_use]
    pub fn parse(response: &Node) -> Self {
        let items = response
            .descendants_named("Message")
            .into_iter()
            .chain(response.descendants_named("Item"))
            .filter_map(EwsItem::parse)
            .collect::<Vec<_>>();
        let deleted_ids = response
            .descendants_named("Delete")
            .into_iter()
            .filter_map(|deletion| deletion.child("ItemId").and_then(ItemId::parse))
            .map(|id| id.id)
            .collect();
        let read_changes = response
            .descendants_named("ReadFlagChange")
            .into_iter()
            .filter_map(|change| {
                let id = change.child("ItemId").and_then(ItemId::parse)?;
                let is_read = change.find_bool(&["IsRead"])?;
                Some((id.id, is_read))
            })
            .collect();
        Self {
            items,
            deleted_ids,
            read_changes,
            sync_state: response
                .find_text(&["SyncState"])
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            // A missing flag means "everything was transferred".
            includes_last_item: response
                .find_bool(&["IncludesLastItemInRange"])
                .unwrap_or(true),
        }
    }
}

/// Reads the recipients of a `<t:ToRecipients>`/`<t:CcRecipients>` container.
fn recipients(node: &Node, container: &str) -> Vec<MailAddress> {
    node.child(container)
        .map(|container| {
            container
                .children_named("Mailbox")
                .filter_map(mailbox_entry)
                .collect()
        })
        .unwrap_or_default()
}

/// Reads a `<t:From>`/`<t:Mailbox>` pair.
fn mailbox(node: &Node) -> Option<MailAddress> {
    mailbox_entry(node.child("Mailbox").unwrap_or(node))
}

fn mailbox_entry(node: &Node) -> Option<MailAddress> {
    let email = node
        .find_text(&["EmailAddress"])
        .or_else(|| node.attr("EmailAddress"))
        .unwrap_or_default()
        .to_string();
    if email.is_empty() {
        return None;
    }
    let name = node
        .find_text(&["Name"])
        .filter(|name| !name.is_empty())
        .map(ToString::to_string);
    Some(MailAddress { name, email })
}

/// Reads one of the ISO 8601 timestamps of an item.
fn timestamp(node: &Node, name: &str) -> Option<DateTime<Utc>> {
    let text = node.find_text(&[name])?;
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&Utc));
    }
    // Without a `TimeZoneContext` the server may answer without a zone; `<` and
    // `>` protection is already gone at this point.
    for format in ["%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(text.trim_end_matches('Z'), format) {
            return Some(Utc.from_utc_datetime(&parsed));
        }
    }
    None
}

/// Strips the angle brackets and lowercases a message id, like the IMAP path.
#[must_use]
pub fn normalise_message_id(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLDER: &str = r#"<t:Folder xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types">
        <t:FolderId Id="AAMkAGYz" ChangeKey="AQAAAB"/>
        <t:ParentFolderId Id="AAMkROOT="/>
        <t:FolderClass>IPF.Note</t:FolderClass>
        <t:DisplayName>Rechnungen &amp; Angebote</t:DisplayName>
        <t:TotalCount>120</t:TotalCount>
        <t:UnreadCount>4</t:UnreadCount>
        <t:ChildFolderCount>2</t:ChildFolderCount>
    </t:Folder>"#;

    const MESSAGE: &str = r#"<t:Message xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types">
        <t:ItemId Id="AAMkADQx" ChangeKey="CQAAABY"/>
        <t:ParentFolderId Id="AAMkAGYz"/>
        <t:ItemClass>IPM.Note</t:ItemClass>
        <t:Subject>Rechnung 4711</t:Subject>
        <t:From><t:Mailbox><t:Name>Ada Lovelace</t:Name><t:EmailAddress>ada@example.org</t:EmailAddress></t:Mailbox></t:From>
        <t:ToRecipients>
          <t:Mailbox><t:EmailAddress>bob@example.org</t:EmailAddress></t:Mailbox>
          <t:Mailbox><t:Name>Carol</t:Name><t:EmailAddress>carol@example.org</t:EmailAddress></t:Mailbox>
        </t:ToRecipients>
        <t:CcRecipients><t:Mailbox><t:EmailAddress>dave@example.org</t:EmailAddress></t:Mailbox></t:CcRecipients>
        <t:DateTimeSent>2026-11-12T13:00:00Z</t:DateTimeSent>
        <t:DateTimeReceived>2026-11-12T13:00:05Z</t:DateTimeReceived>
        <t:Size>2048</t:Size>
        <t:IsRead>false</t:IsRead>
        <t:HasAttachments>true</t:HasAttachments>
        <t:InternetMessageId>&lt;rechnung@example.org&gt;</t:InternetMessageId>
        <t:InReplyTo>thread@example.org</t:InReplyTo>
        <t:ConversationId Id="AAQkAGYz"/>
        <t:Flag><t:FlagStatus>Flagged</t:FlagStatus></t:Flag>
    </t:Message>"#;

    #[test]
    fn a_folder_is_parsed_with_its_counts() {
        let node = Node::parse(FOLDER).unwrap();
        let folder = EwsFolder::parse(&node).unwrap();
        assert_eq!(folder.id.id, "AAMkAGYz");
        assert_eq!(folder.id.change_key.as_deref(), Some("AQAAAB"));
        assert_eq!(folder.parent_id.as_deref(), Some("AAMkROOT="));
        assert_eq!(folder.display_name, "Rechnungen & Angebote");
        assert_eq!(folder.total_count, 120);
        assert_eq!(folder.unread_count, 4);
        assert_eq!(folder.child_folder_count, 2);
        assert!(folder.is_mail_folder());
        assert!(!folder.is_technical_root());
    }

    #[test]
    fn a_technical_root_and_a_calendar_are_recognised() {
        let root = Node::parse(
            r#"<t:Folder xmlns:t="x"><t:FolderId Id="root"/>
                <t:DistinguishedFolderId Id="msgfolderroot"/><t:FolderClass>IPF.Note</t:FolderClass></t:Folder>"#,
        )
        .unwrap();
        let root = EwsFolder::parse(&root).unwrap();
        assert!(root.is_technical_root());

        let calendar = Node::parse(
            r#"<t:Folder xmlns:t="x"><t:FolderId Id="cal"/>
                <t:FolderClass>IPF.Appointment</t:FolderClass><t:DisplayName>Kalender</t:DisplayName></t:Folder>"#,
        )
        .unwrap();
        let calendar = EwsFolder::parse(&calendar).unwrap();
        assert!(!calendar.is_mail_folder());

        // A folder without an id cannot be addressed and is dropped.
        assert!(EwsFolder::parse(&Node::parse("<t:Folder xmlns:t=\"x\"/>").unwrap()).is_none());
    }

    #[test]
    fn a_message_is_parsed_completely() {
        let node = Node::parse(MESSAGE).unwrap();
        let item = EwsItem::parse(&node).unwrap();
        assert_eq!(item.id.id, "AAMkADQx");
        assert_eq!(item.id.change_key.as_deref(), Some("CQAAABY"));
        assert_eq!(item.parent_folder_id.as_deref(), Some("AAMkAGYz"));
        assert_eq!(item.subject, "Rechnung 4711");
        assert_eq!(item.from.as_ref().unwrap().email, "ada@example.org");
        assert_eq!(
            item.from.as_ref().unwrap().name.as_deref(),
            Some("Ada Lovelace")
        );
        assert_eq!(item.to.len(), 2);
        assert_eq!(item.to[1].name.as_deref(), Some("Carol"));
        assert_eq!(item.cc[0].email, "dave@example.org");
        assert_eq!(
            item.received_at.unwrap().to_rfc3339(),
            "2026-11-12T13:00:05+00:00"
        );
        assert_eq!(item.size_bytes, 2048);
        assert!(!item.is_read);
        assert!(item.is_flagged);
        assert!(item.has_attachments);
        assert_eq!(
            item.internet_message_id.as_deref(),
            Some("rechnung@example.org")
        );
        assert_eq!(item.in_reply_to.as_deref(), Some("thread@example.org"));
        assert_eq!(item.conversation_id.as_deref(), Some("AAQkAGYz"));
        assert!(item.is_mail());
    }

    #[test]
    fn timestamps_without_a_zone_are_still_read() {
        let node = Node::parse(
            r#"<t:Message xmlns:t="x"><t:ItemId Id="i"/>
                <t:DateTimeReceived>2026-11-12T13:00:05</t:DateTimeReceived></t:Message>"#,
        )
        .unwrap();
        let item = EwsItem::parse(&node).unwrap();
        assert_eq!(
            item.received_at.unwrap().to_rfc3339(),
            "2026-11-12T13:00:05+00:00"
        );
        assert!(item.sent_at.is_none());

        let broken = Node::parse(
            r#"<t:Message xmlns:t="x"><t:ItemId Id="i"/>
                <t:DateTimeReceived>gestern</t:DateTimeReceived></t:Message>"#,
        )
        .unwrap();
        // An unreadable date is not fatal: the message is still stored.
        assert!(EwsItem::parse(&broken).unwrap().received_at.is_none());
    }

    #[test]
    fn a_sync_batch_collects_items_and_read_changes() {
        let response = Node::parse(
            r#"<SyncFolderItemsResponseMessage xmlns:t="x" xmlns:m="y">
                <m:ResponseCode>NoError</m:ResponseCode>
                <m:SyncState>H4sIAAAA</m:SyncState>
                <m:Includes>
                  <t:Message><t:ItemId Id="a"/><t:Subject>eins</t:Subject></t:Message>
                  <t:Message><t:ItemId Id="b"/><t:Subject>zwei</t:Subject></t:Message>
                </m:Includes>
                <m:ReadFlagChange><t:ItemId Id="a"/><t:IsRead>true</t:IsRead></m:ReadFlagChange>
            </SyncFolderItemsResponseMessage>"#,
        )
        .unwrap();
        let batch = SyncBatch::parse(&response);
        assert_eq!(batch.items.len(), 2);
        assert_eq!(batch.sync_state.as_deref(), Some("H4sIAAAA"));
        assert_eq!(batch.read_changes, vec![("a".to_string(), true)]);
        // No deletion and no "more to come" flag; a missing flag means done.
        assert!(batch.deleted_ids.is_empty());
        assert!(batch.includes_last_item);
    }

    #[test]
    fn a_sync_batch_reports_deletions_and_the_more_flag() {
        let response = Node::parse(
            r#"<SyncFolderItemsResponseMessage xmlns:t="x" xmlns:m="y">
                <m:SyncState>cursor-1</m:SyncState>
                <m:IncludesLastItemInRange>false</m:IncludesLastItemInRange>
                <m:Changes>
                  <m:Create><t:Message><t:ItemId Id="a"/></t:Message></m:Create>
                  <m:Delete><t:ItemId Id="weg-1"/></m:Delete>
                  <m:Delete><t:ItemId Id="weg-2"/></m:Delete>
                </m:Changes>
            </SyncFolderItemsResponseMessage>"#,
        )
        .unwrap();
        let batch = SyncBatch::parse(&response);
        assert_eq!(batch.items.len(), 1);
        assert_eq!(batch.deleted_ids, vec!["weg-1", "weg-2"]);
        assert!(!batch.includes_last_item);
    }

    #[test]
    fn a_meeting_request_is_not_a_mail_message() {
        let node = Node::parse(
            r#"<t:Message xmlns:t="x"><t:ItemId Id="m"/>
                <t:ItemClass>IPM.Schedule.Meeting.Request</t:ItemClass></t:Message>"#,
        )
        .unwrap();
        assert!(!EwsItem::parse(&node).unwrap().is_mail());
    }
}
