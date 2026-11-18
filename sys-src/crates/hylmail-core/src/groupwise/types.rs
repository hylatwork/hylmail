//! The GroupWise shapes HylMail reads.
//!
//! GroupWise answers with `<status><code>` elements instead of HTTP status
//! codes, so an operation that "succeeded" can still carry an error.  The
//! parsers here return the data, and [`super::GroupWiseClient`] turns a non-zero
//! code into an [`crate::Error`] before any caller sees it.
//!
//! Two GroupWise quirks shape this module:
//!
//! * **Timestamps are local time.** `yyyyMMddHHmmss` without a zone is what the
//!   server sends when no time zone is configured; an unreadable timestamp means
//!   the message is stored without a date rather than dropped.
//! * **Read state is a number.** `<unread>0|1</unread>` is the flag, not a
//!   boolean element.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

use crate::models::message::MailAddress;
use crate::soap::Node;

/// An item identity: GroupWise item ids are opaque strings.
pub type ItemId = String;

/// One folder of the folder list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GwFolder {
    /// Server side identifier, e.g. `1` for the mailbox.
    pub id: String,
    /// Display name, e.g. `Mailbox` or `Projekte`.
    pub name: String,
    /// Server side identifier of the parent folder.
    pub parent_id: Option<String>,
    /// `folderType` number, kept because a few types are not mail folders.
    pub folder_type: Option<i64>,
    /// Number of items in the folder.
    pub item_count: u32,
    /// Number of unread items.
    pub unread_count: u32,
    /// Sequence number that changes when the folder content changes.
    pub sequence: Option<i64>,
}

impl GwFolder {
    /// Parses one `<gw:folder>` element.
    #[must_use]
    pub fn parse(node: &Node) -> Option<Self> {
        let id = node.find_text(&["id"]).unwrap_or_default().to_string();
        if id.is_empty() {
            return None;
        }
        let name = node
            .find_text(&["name"])
            .filter(|name| !name.is_empty())
            .unwrap_or(&id)
            .to_string();
        let folder_type = node
            .find_text(&["folderType"])
            .and_then(|value| value.parse().ok());
        Some(Self {
            id,
            name,
            parent_id: node
                .find_text(&["parent"])
                .filter(|parent| !parent.is_empty() && *parent != "0")
                .map(ToString::to_string),
            folder_type,
            item_count: node
                .find_u64(&["itemCount"])
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
            unread_count: node
                .find_u64(&["unread"])
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
            sequence: node
                .find_text(&["sequence"])
                .and_then(|value| value.parse().ok()),
        })
    }

    /// `true` when the folder holds mail.
    ///
    /// GroupWise folders have types such as `0` (mail), `2` (calendar),
    /// `3` (contacts) and `4` (tasks); the numeric coding differs between
    /// installations, so the type is only used as a hint and the name decides.
    #[must_use]
    pub fn is_mail_folder(&self) -> bool {
        match self.folder_type {
            Some(folder_type) => matches!(folder_type, 0 | 1 | 10 | 11 | 12 | 13 | 32),
            None => true,
        }
    }
}

/// One item of a folder listing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GwItem {
    /// Server side identifier.
    pub id: ItemId,
    /// Subject, unprefixed.
    pub subject: String,
    /// Sender, when the server sent one.
    pub from: Option<MailAddress>,
    /// `To:` recipients.
    pub to: Vec<MailAddress>,
    /// Delivery timestamp.
    pub date: Option<DateTime<Utc>>,
    /// Size in bytes.
    pub size_bytes: u64,
    /// `true` when `<unread>` is `0`.
    pub is_read: bool,
    /// `true` when the item reports an attachment.
    pub has_attachments: bool,
    /// RFC 5322 `Message-ID` when the server knows it.
    pub message_id: Option<String>,
    /// `In-Reply-To` when the server knows it.
    pub in_reply_to: Option<String>,
    /// `type` attribute of the item, e.g. `mail` or `appointment`.
    pub item_type: String,
}

impl GwItem {
    /// Parses one `<gw:item>` element of a listing.
    #[must_use]
    pub fn parse(node: &Node) -> Option<Self> {
        let id = node.find_text(&["id"]).unwrap_or_default().to_string();
        if id.is_empty() {
            return None;
        }
        let mut item = Self {
            id,
            subject: node.find_text(&["subject"]).unwrap_or_default().to_string(),
            from: mailbox(node, "from"),
            to: node
                .descendants_named("to")
                .into_iter()
                .filter_map(mailbox_entry)
                .collect(),
            date: timestamp(node.find_text(&["date"])),
            size_bytes: node.find_u64(&["size"]).unwrap_or(0),
            // A missing `<unread>` means "read": the server only sends the
            // element when the item is new.
            is_read: node
                .find_text(&["unread"])
                .map(|value| value == "0" || value.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            has_attachments: node
                .descendants_named("attachment")
                .into_iter()
                .next()
                .is_some(),
            message_id: node
                .find_text(&["messageId"])
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            in_reply_to: node
                .find_text(&["inReplyTo"])
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            item_type: node.attr("type").unwrap_or_default().to_string(),
        };
        if let Some(message_id) = item.message_id.take() {
            item.message_id = Some(crate::models::message::normalize_message_id(&message_id));
        }
        if let Some(in_reply_to) = item.in_reply_to.take() {
            item.in_reply_to = Some(crate::models::message::normalize_message_id(&in_reply_to));
        }
        Some(item)
    }

    /// `true` when the item is a mail message rather than an appointment,
    /// contact or task that shares the folder.
    #[must_use]
    pub fn is_mail(&self) -> bool {
        self.item_type.is_empty() || self.item_type.eq_ignore_ascii_case("mail")
    }
}

/// One page of a folder listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemPage {
    /// Items of this page.
    pub items: Vec<GwItem>,
    /// Number of items in the folder according to the server.
    pub total: u32,
}

impl ItemPage {
    /// Parses a `getItemsResponse`.
    #[must_use]
    pub fn parse(response: &Node) -> Self {
        let items = response
            .descendants_named("item")
            .into_iter()
            .filter_map(GwItem::parse)
            .collect();
        Self {
            items,
            // The total sits next to the items, but a caller may hand in either
            // the answer element or the whole envelope, so the tree is searched.
            total: response
                .descendants_named("total")
                .into_iter()
                .find_map(|node| node.text.trim().parse::<u64>().ok())
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
        }
    }

    /// `true` when the page was empty, which ends the paging loop.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Reads a `<gw:from>`/`<gw:to>` container.
fn mailbox(node: &Node, container: &str) -> Option<MailAddress> {
    let holder = node.child(container)?;
    mailbox_entry(holder)
}

/// Reads a `<gw:displayName>`/`<gw:email>` pair, either nested or flat.
fn mailbox_entry(node: &Node) -> Option<MailAddress> {
    let email = node
        .find_text(&["email"])
        .or_else(|| node.attr("email"))
        .unwrap_or_default()
        .to_string();
    if email.is_empty() {
        return None;
    }
    let name = node
        .find_text(&["displayName"])
        .or_else(|| node.find_text(&["name"]))
        .filter(|name| !name.is_empty())
        .map(ToString::to_string);
    Some(MailAddress { name, email })
}

/// Parses a GroupWise timestamp.
///
/// `yyyyMMddHHmmss` is local time without a zone (the proxy has no time zone
/// support), so it is read as UTC and only the ordering the client shows
/// depends on it; ISO 8601 is preferred when the server sends it.
#[must_use]
pub fn timestamp(text: Option<&str>) -> Option<DateTime<Utc>> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in ["%Y%m%d%H%M%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(text, format) {
            return Some(Utc.from_utc_datetime(&parsed));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLDER: &str = "<gw:folder xmlns:gw=\"x\"><gw:id>1</gw:id><gw:name>Mailbox</gw:name>\
        <gw:parent>0</gw:parent><gw:folderType>0</gw:folderType>\
        <gw:itemCount>120</gw:itemCount><gw:unread>4</gw:unread>\
        <gw:sequence>77</gw:sequence></gw:folder>";

    const ITEM: &str = "<gw:item xmlns:gw=\"x\" type=\"mail\"><gw:id>A1B2C3</gw:id>\
        <gw:subject>Rechnung 4711</gw:subject>\
        <gw:from><gw:displayName>Ada Lovelace</gw:displayName>\
        <gw:email>ada@example.org</gw:email></gw:from>\
        <gw:to><gw:email>bob@example.org</gw:email></gw:to>\
        <gw:date>20261112130000</gw:date><gw:size>2048</gw:size><gw:unread>1</gw:unread>\
        <gw:attachment><gw:name>rechnung.pdf</gw:name></gw:attachment>\
        <gw:messageId>&lt;rechnung@example.org&gt;</gw:messageId>\
        <gw:inReplyTo>thread@example.org</gw:inReplyTo></gw:item>";

    #[test]
    fn a_folder_is_parsed_with_its_counters() {
        let folder = GwFolder::parse(&Node::parse(FOLDER).unwrap()).unwrap();
        assert_eq!(folder.id, "1");
        assert_eq!(folder.name, "Mailbox");
        // A parent of `0` means "top level", not a folder called `0`.
        assert!(folder.parent_id.is_none());
        assert_eq!(folder.folder_type, Some(0));
        assert_eq!(folder.item_count, 120);
        assert_eq!(folder.unread_count, 4);
        assert_eq!(folder.sequence, Some(77));
        assert!(folder.is_mail_folder());
    }

    #[test]
    fn a_calendar_folder_is_not_a_mail_folder_and_a_nameless_one_uses_its_id() {
        let calendar = Node::parse(
            "<gw:folder xmlns:gw=\"x\"><gw:id>7</gw:id><gw:folderType>2</gw:folderType></gw:folder>",
        )
        .unwrap();
        let calendar = GwFolder::parse(&calendar).unwrap();
        assert_eq!(calendar.name, "7");
        assert!(!calendar.is_mail_folder());

        // Without an id the folder cannot be addressed and is dropped.
        assert!(GwFolder::parse(&Node::parse("<gw:folder xmlns:gw=\"x\"/>").unwrap()).is_none());
    }

    #[test]
    fn an_item_is_parsed_completely() {
        let item = GwItem::parse(&Node::parse(ITEM).unwrap()).unwrap();
        assert_eq!(item.id, "A1B2C3");
        assert_eq!(item.subject, "Rechnung 4711");
        assert_eq!(item.from.as_ref().unwrap().email, "ada@example.org");
        assert_eq!(
            item.from.as_ref().unwrap().name.as_deref(),
            Some("Ada Lovelace")
        );
        assert_eq!(item.to[0].email, "bob@example.org");
        assert_eq!(item.date.unwrap().to_rfc3339(), "2026-11-12T13:00:00+00:00");
        assert_eq!(item.size_bytes, 2048);
        assert!(!item.is_read, "unread 1 means unread");
        assert!(item.has_attachments);
        assert_eq!(item.message_id.as_deref(), Some("rechnung@example.org"));
        assert_eq!(item.in_reply_to.as_deref(), Some("thread@example.org"));
        assert!(item.is_mail());
    }

    #[test]
    fn a_read_item_without_the_element_is_read() {
        let item = Node::parse(
            "<gw:item xmlns:gw=\"x\"><gw:id>A1</gw:id><gw:unread>0</gw:unread></gw:item>",
        )
        .unwrap();
        assert!(GwItem::parse(&item).unwrap().is_read);
        let plain = Node::parse("<gw:item xmlns:gw=\"x\"><gw:id>A1</gw:id></gw:item>").unwrap();
        assert!(GwItem::parse(&plain).unwrap().is_read);
        let unread = Node::parse(
            "<gw:item xmlns:gw=\"x\"><gw:id>A1</gw:id><gw:unread>1</gw:unread></gw:item>",
        )
        .unwrap();
        assert!(!GwItem::parse(&unread).unwrap().is_read);
    }

    #[test]
    fn an_item_page_lists_the_items_and_the_total() {
        let response = Node::parse(
            "<gw:getItemsResponse xmlns:gw=\"x\"><gw:status><gw:code>0</gw:code></gw:status>\
             <gw:items><gw:total>2</gw:total>\
             <gw:item type=\"mail\"><gw:id>A1</gw:id><gw:subject>eins</gw:subject></gw:item>\
             <gw:item type=\"mail\"><gw:id>A2</gw:id><gw:subject>zwei</gw:subject></gw:item>\
             </gw:items></gw:getItemsResponse>",
        )
        .unwrap();
        let page = ItemPage::parse(&response);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 2);
        assert!(!page.is_empty());
        let empty = ItemPage::parse(&Node::parse("<gw:getItemsResponse xmlns:gw=\"x\"/>").unwrap());
        assert!(empty.is_empty());
        assert_eq!(empty.total, 0);
    }

    #[test]
    fn timestamps_understand_both_spellings() {
        assert!(timestamp(Some("20261112130000")).is_some());
        assert!(timestamp(Some("2026-11-12T13:00:00Z")).is_some());
        assert!(timestamp(Some("2026-11-12 13:00:00")).is_some());
        assert!(timestamp(Some("irgendwann")).is_none());
        assert!(timestamp(None).is_none());
        assert!(timestamp(Some("  ")).is_none());
    }
}
