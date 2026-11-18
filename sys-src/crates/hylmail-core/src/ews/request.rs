//! Request bodies of the EWS operations HylMail uses.
//!
//! Every function returns the `<m:…>` fragment that goes into the SOAP body; the
//! envelope, the `SOAPAction` header and the auth header are added by
//! [`super::EwsClient`].  Keeping the bodies here makes them unit testable
//! without a socket, and the tests parse the result back with [`Node`] so a
//! malformed request fails the build instead of the server.
//!
//! The shapes are deliberately explicit: `BaseShape="IdOnly"` plus a list of
//! `AdditionalProperties` transfers exactly the fields the client stores and
//! nothing else, which keeps a mailbox with 100 000 messages from transferring
//! every body preview on every pass.

use crate::soap::envelope::{escape_attribute, escape_text};

/// A folder reference used by `FindFolder` and `MoveItem`.
///
/// It owns its identifier so a folder queue can be built while a tree is walked
/// without borrowing from the folders that were already discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderRef {
    /// A well known folder such as `inbox` or `msgfolderroot`.
    Distinguished(String),
    /// A folder addressed by its server id.
    Id(String),
}

impl FolderRef {
    /// A well known folder.
    #[must_use]
    pub fn distinguished(id: impl Into<String>) -> Self {
        Self::Distinguished(id.into())
    }

    /// A folder addressed by its server id.
    #[must_use]
    pub fn id(id: impl Into<String>) -> Self {
        Self::Id(id.into())
    }

    /// The identifier this reference carries.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Distinguished(id) | Self::Id(id) => id,
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Distinguished(id) => {
                format!("<t:DistinguishedFolderId Id=\"{}\"/>", escape_attribute(id))
            }
            Self::Id(id) => format!("<t:FolderId Id=\"{}\"/>", escape_attribute(id)),
        }
    }
}

/// The properties of an item the header list needs.
///
/// `item:Flag` and `item:InternetMessageId` are not part of `Default`, and
/// `item:ConversationId` is what Exchange uses for threading.
const ITEM_PROPERTIES: &[&str] = &[
    "item:ParentFolderId",
    "item:ItemClass",
    "item:Subject",
    "item:From",
    "item:ToRecipients",
    "item:CcRecipients",
    "item:DateTimeSent",
    "item:DateTimeReceived",
    "item:Size",
    "item:IsRead",
    "item:HasAttachments",
    "item:InternetMessageId",
    "item:InReplyTo",
    "item:ConversationId",
    "item:Flag",
];

/// Renders the `<m:ItemShape>` used by every item operation.
#[must_use]
pub fn item_shape() -> String {
    format!("<m:ItemShape>{}</m:ItemShape>", item_shape_body())
}

/// The content of `<m:ItemShape>`, for the operations that need the shape
/// nested inside another element.
fn item_shape_body() -> String {
    let properties: String = ITEM_PROPERTIES
        .iter()
        .map(|uri| format!("<t:FieldURI FieldURI=\"{uri}\"/>"))
        .collect();
    format!(
        "<t:BaseShape>IdOnly</t:BaseShape>\
         <t:AdditionalProperties>{properties}</t:AdditionalProperties>"
    )
}

/// Renders the `<m:FolderShape>` used by every folder operation.
#[must_use]
pub fn folder_shape() -> String {
    "<m:FolderShape><t:BaseShape>Default</t:BaseShape>\
     <t:AdditionalProperties><t:FieldURI FieldURI=\"folder:UnreadCount\"/>\
     </t:AdditionalProperties></m:FolderShape>"
        .to_string()
}

/// `GetFolder` for one folder, used to verify a connection and to read the
/// state of a single folder without walking the whole tree.
#[must_use]
pub fn get_folder(folder: &FolderRef) -> String {
    format!(
        "<m:GetFolder>{shape}<m:FolderIds>{folder}</m:FolderIds></m:GetFolder>",
        shape = folder_shape(),
        folder = folder.render()
    )
}

/// `FindFolder` for the direct children of a folder.
///
/// `Traversal="Shallow"` is what makes the result one level; the client walks
/// the tree by calling this once per discovered folder.
#[must_use]
pub fn find_folder(parent: &FolderRef) -> String {
    format!(
        "<m:FindFolder Traversal=\"Shallow\">{shape}\
         <m:ParentFolderIds>{parent}</m:ParentFolderIds></m:FindFolder>",
        shape = folder_shape(),
        parent = parent.render()
    )
}

/// `SyncFolderItems`, the incremental operation.
///
/// Without a `SyncState` the server answers with the current content of the
/// folder, which is exactly the "first synchronisation" case.
#[must_use]
pub fn sync_folder_items(folder_id: &str, sync_state: Option<&str>, max_changes: u32) -> String {
    let state = match sync_state {
        Some(state) if !state.is_empty() => {
            format!("<m:SyncState>{}</m:SyncState>", escape_text(state))
        }
        _ => String::new(),
    };
    format!(
        "<m:SyncFolderItems>{shape}<m:SyncFolderId><t:FolderId Id=\"{id}\"/></m:SyncFolderId>\
         {state}<m:MaxChangesReturned>{max_changes}</m:MaxChangesReturned>\
         </m:SyncFolderItems>",
        shape = item_shape(),
        id = escape_attribute(folder_id),
    )
}

/// `GetItem` including the raw MIME content.
///
/// The raw message is what the MIME pipeline of phase 4 parses, so no server
/// side body conversion is requested.
#[must_use]
pub fn get_item(item_id: &str, change_key: Option<&str>) -> String {
    let key = change_key
        .filter(|key| !key.is_empty())
        .map(|key| format!(" ChangeKey=\"{}\"", escape_attribute(key)))
        .unwrap_or_default();
    format!(
        "<m:GetItem>\
         <m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>\
         <t:IncludeMimeContent>true</t:IncludeMimeContent></m:ItemShape>\
         <m:ItemIds><t:ItemId Id=\"{id}\"{key}/></m:ItemIds></m:GetItem>",
        id = escape_attribute(item_id),
    )
}

/// `UpdateItem` that only changes the read state.
#[must_use]
pub fn set_read_state(item_id: &str, is_read: bool) -> String {
    update_item(
        item_id,
        "message:IsRead",
        &format!("<t:Message><t:IsRead>{is_read}</t:IsRead></t:Message>"),
    )
}

/// `UpdateItem` that only changes the follow-up flag.
#[must_use]
pub fn set_flag(item_id: &str, flagged: bool) -> String {
    let status = if flagged { "Flagged" } else { "NotFlagged" };
    set_flag_status(item_id, status)
}

/// Sets the "done" state of a message as its follow-up flag status (`FR-FLAG-3`).
///
/// EWS has no dedicated "done": a completed follow-up flag is the item status a
/// user sees as done.  Clearing it falls back to a plain flagged item, so a
/// message that was merely flagged is not silently unflagged.
#[must_use]
pub fn set_done(item_id: &str, done: bool) -> String {
    let status = if done { "Complete" } else { "Flagged" };
    set_flag_status(item_id, status)
}

/// `UpdateItem` that rewrites the follow-up `FlagStatus` of a message.
#[must_use]
pub fn set_flag_status(item_id: &str, status: &str) -> String {
    update_item(
        item_id,
        "message:Flag",
        &format!("<t:Message><t:Flag><t:FlagStatus>{status}</t:FlagStatus></t:Flag></t:Message>"),
    )
}

/// The common part of every `UpdateItem` request.
fn update_item(item_id: &str, field_uri: &str, update: &str) -> String {
    format!(
        "<m:UpdateItem ConflictResolution=\"AlwaysOverwrite\"\
         MessageDisposition=\"SaveOnly\">\
         <m:ItemChanges><t:ItemChange><t:ItemId Id=\"{id}\"/><t:Updates>\
         <t:SetItemField><t:FieldURI FieldURI=\"{field_uri}\"/>{update}</t:SetItemField>\
         </t:Updates></t:ItemChange></m:ItemChanges></m:UpdateItem>",
        id = escape_attribute(item_id),
    )
}

/// `MoveItem` into another folder.
#[must_use]
pub fn move_item(item_id: &str, destination: &FolderRef) -> String {
    format!(
        "<m:MoveItem><m:ToFolderId>{destination}</m:ToFolderId>\
         <m:ItemIds><t:ItemId Id=\"{id}\"/></m:ItemIds></m:MoveItem>",
        destination = destination.render(),
        id = escape_attribute(item_id),
    )
}

/// `DeleteItem`; `hard` removes the item instead of moving it to the deleted
/// items folder.
#[must_use]
pub fn delete_item(item_id: &str, hard: bool) -> String {
    let delete_type = if hard { "HardDelete" } else { "SoftDelete" };
    format!(
        "<m:DeleteItem DeleteType=\"{delete_type}\"\
         SendMeetingCancellations=\"SendToNone\"\
         AffectedTaskOccurrences=\"AllOccurrences\">\
         <m:ItemIds><t:ItemId Id=\"{id}\"/></m:ItemIds></m:DeleteItem>",
        id = escape_attribute(item_id),
    )
}

/// `CreateItem` that submits a complete message, MIME encoded.
///
/// `MessageDisposition="SendAndSaveCopy"` is the point of this request:
/// Exchange delivers the message *and* files the copy in the sender's
/// `Sent Items` folder, which a bare SMTP submission does not do.  Because the
/// item travels as `MimeContent`, nothing but the bytes is transferred - no
/// per-property mapping that could silently drop a header.  The recipients are
/// taken from the message itself, exactly as they are over SMTP.
#[must_use]
pub fn create_item_mime(raw: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    format!(
        "<m:CreateItem MessageDisposition=\"SendAndSaveCopy\"><m:Items><t:Message>\
         <t:MimeContent CharacterSet=\"UTF-8\">{encoded}</t:MimeContent>\
         </t:Message></m:Items></m:CreateItem>"
    )
}

/// The `RequestServerVersion` header fragment.
#[must_use]
pub fn request_server_version(version: &str) -> String {
    format!(
        "<t:RequestServerVersion Version=\"{}\"/>",
        escape_attribute(version)
    )
}

/// The `ExchangeImpersonation` header fragment used for a shared mailbox.
#[must_use]
pub fn impersonation(smtp_address: &str) -> String {
    format!(
        "<t:ExchangeImpersonation><t:ConnectingSID>\
         <t:PrimarySmtpAddress>{}</t:PrimarySmtpAddress>\
         </t:ConnectingSID></t:ExchangeImpersonation>",
        escape_text(smtp_address)
    )
}

/// The `TimeZoneContext` header fragment.
#[must_use]
pub fn time_zone_context(id: &str) -> String {
    format!(
        "<t:TimeZoneContext><t:TimeZoneDefinition Id=\"{}\"/></t:TimeZoneContext>",
        escape_attribute(id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soap::Node;

    fn assert_parses(xml: &str) -> Node {
        match Node::parse(xml) {
            Ok(node) => node,
            Err(error) => panic!("the request is not valid XML: {error}\n{xml}"),
        }
    }

    #[test]
    fn the_item_shape_asks_only_for_the_stored_fields() {
        let shape = assert_parses(&item_shape());
        assert_eq!(shape.find_text(&["BaseShape"]), Some("IdOnly"));
        let fields: Vec<&str> = shape
            .children
            .iter()
            .flat_map(|node| node.children_named("FieldURI"))
            .filter_map(|node| node.attr("FieldURI"))
            .collect();
        for expected in [
            "item:Subject",
            "item:From",
            "item:IsRead",
            "item:Flag",
            "item:InternetMessageId",
        ] {
            assert!(
                fields.contains(&expected),
                "{expected} missing in {fields:?}"
            );
        }
        // No body preview is ever requested during a header pass.
        assert!(!fields.iter().any(|field| field.contains("BodyPreview")));
    }

    #[test]
    fn a_distinguished_and_an_addressed_folder_render_differently() {
        let distinguished = assert_parses(&find_folder(&FolderRef::distinguished("msgfolderroot")));
        assert_eq!(
            distinguished
                .find(&["ParentFolderIds", "DistinguishedFolderId"])
                .unwrap()
                .attr("Id"),
            Some("msgfolderroot")
        );
        assert_eq!(distinguished.attr("Traversal"), Some("Shallow"));

        let addressed = assert_parses(&find_folder(&FolderRef::id("AAMkAGYz")));
        assert_eq!(
            addressed
                .find(&["ParentFolderIds", "FolderId"])
                .unwrap()
                .attr("Id"),
            Some("AAMkAGYz")
        );

        let inbox = assert_parses(&get_folder(&FolderRef::distinguished("inbox")));
        assert!(inbox.child("FolderShape").is_some());
        assert_eq!(FolderRef::id("x").as_str(), "x");
        assert_eq!(FolderRef::distinguished("inbox").as_str(), "inbox");
    }

    #[test]
    fn a_first_sync_has_no_sync_state_and_an_incremental_one_has_it() {
        let first = assert_parses(&sync_folder_items("AAMkAGYz", None, 512));
        assert!(first.find(&["SyncState"]).is_none());
        assert_eq!(
            first
                .find(&["SyncFolderId", "FolderId"])
                .unwrap()
                .attr("Id"),
            Some("AAMkAGYz")
        );
        assert_eq!(first.find_text(&["MaxChangesReturned"]), Some("512"));

        let incremental = assert_parses(&sync_folder_items("AAMkAGYz", Some("H4sIAA&=="), 512));
        assert_eq!(incremental.find_text(&["SyncState"]), Some("H4sIAA&=="));
        // An empty state is treated like a missing one.
        let empty = assert_parses(&sync_folder_items("AAMkAGYz", Some(""), 512));
        assert!(empty.find(&["SyncState"]).is_none());
    }

    #[test]
    fn mime_content_is_requested_with_the_item_id() {
        let item = assert_parses(&get_item("AAMkADQx", Some("CQAAABY")));
        assert_eq!(
            item.find(&["ItemShape", "IncludeMimeContent"])
                .unwrap()
                .text
                .trim(),
            "true"
        );
        assert_eq!(
            item.find(&["ItemIds", "ItemId"]).unwrap().attr("Id"),
            Some("AAMkADQx")
        );
        assert_eq!(
            item.find(&["ItemIds", "ItemId"]).unwrap().attr("ChangeKey"),
            Some("CQAAABY")
        );
    }

    #[test]
    fn updates_move_and_delete_address_the_item() {
        let read = assert_parses(&set_read_state("AAMkADQx", true));
        assert_eq!(
            read.find(&["ItemChanges", "ItemChange", "ItemId"])
                .unwrap()
                .attr("Id"),
            Some("AAMkADQx")
        );
        assert_eq!(
            read.find(&[
                "ItemChanges",
                "ItemChange",
                "Updates",
                "SetItemField",
                "FieldURI"
            ])
            .unwrap()
            .attr("FieldURI"),
            Some("message:IsRead")
        );
        assert_eq!(
            read.find(&[
                "ItemChanges",
                "ItemChange",
                "Updates",
                "SetItemField",
                "Message",
                "IsRead"
            ])
            .unwrap()
            .text
            .trim(),
            "true"
        );

        let flagged = assert_parses(&set_flag("AAMkADQx", true));
        assert_eq!(
            flagged
                .find(&[
                    "ItemChanges",
                    "ItemChange",
                    "Updates",
                    "SetItemField",
                    "Message",
                    "Flag",
                    "FlagStatus"
                ])
                .unwrap()
                .text
                .trim(),
            "Flagged"
        );
        let unflagged = assert_parses(&set_flag("AAMkADQx", false));
        assert_eq!(
            unflagged
                .find(&[
                    "ItemChanges",
                    "ItemChange",
                    "Updates",
                    "SetItemField",
                    "Message",
                    "Flag",
                    "FlagStatus"
                ])
                .unwrap()
                .text
                .trim(),
            "NotFlagged"
        );

        let moved = assert_parses(&move_item(
            "AAMkADQx",
            &FolderRef::distinguished("deleteditems"),
        ));
        assert_eq!(
            moved
                .find(&["ToFolderId", "DistinguishedFolderId"])
                .unwrap()
                .attr("Id"),
            Some("deleteditems")
        );

        let deleted = assert_parses(&delete_item("AAMkADQx", false));
        assert_eq!(deleted.attr("DeleteType"), Some("SoftDelete"));
        let hard = assert_parses(&delete_item("AAMkADQx", true));
        assert_eq!(hard.attr("DeleteType"), Some("HardDelete"));
    }

    #[test]
    fn a_mime_submission_asks_for_send_and_save_copy() {
        use base64::Engine;
        let raw =
            b"From: ada@example.org\r\nTo: bob@example.org\r\nSubject: Bericht\r\n\r\nText\r\n";
        let request = assert_parses(&create_item_mime(raw));
        assert_eq!(request.attr("MessageDisposition"), Some("SendAndSaveCopy"));
        // The copy is filed by the server, so no destination folder is named.
        assert_eq!(request.children_named("SavedItemFolderId").count(), 0);
        let content = request.find(&["Items", "Message", "MimeContent"]).unwrap();
        assert_eq!(content.attr("CharacterSet"), Some("UTF-8"));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(content.text.trim())
            .unwrap();
        assert_eq!(decoded, raw);
    }

    #[test]
    fn ids_are_escaped_and_the_headers_are_well_formed() {
        // A crafted id must not be able to end the attribute or the element.
        let injected = assert_parses(&get_item("x\"/><evil/>", None));
        assert_eq!(
            injected.find(&["ItemIds", "ItemId"]).unwrap().attr("Id"),
            Some("x\"/><evil/>")
        );
        assert!(injected.descendants_named("evil").is_empty());

        let version = assert_parses(&request_server_version("Exchange2013_SP1"));
        assert_eq!(version.attr("Version"), Some("Exchange2013_SP1"));

        let impersonation = assert_parses(&impersonation("shared@example.org"));
        assert_eq!(
            impersonation
                .find(&["ConnectingSID", "PrimarySmtpAddress"])
                .unwrap()
                .text
                .trim(),
            "shared@example.org"
        );

        let zone = assert_parses(&time_zone_context("W. Europe Standard Time"));
        assert_eq!(
            zone.find(&["TimeZoneDefinition"]).unwrap().attr("Id"),
            Some("W. Europe Standard Time")
        );
    }
}
