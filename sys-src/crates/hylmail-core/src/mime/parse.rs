//! Parsing raw RFC 5322 bytes into the domain model.

use chrono::{DateTime, Utc};
use mail_parser::{Message, MessageParser, PartKind};

use crate::calendar::{self, CalendarSummary};
use crate::db::content;
use crate::error::{Error, Result};
use crate::html::{self, HtmlPolicy, SanitisedHtml};
use crate::models::ids::utc_now;
use crate::models::{
    AccountId, Attachment, FolderId, MailAddress, MessageBody, MessageHeader, MessageId,
};

/// Content types that carry an iCalendar object.
const CALENDAR_TYPES: &[(&str, &str)] = &[
    ("text", "calendar"),
    ("application", "ics"),
    ("application", "calendar"),
];

/// File name used for a calendar part that arrives without one.
pub const CALENDAR_FALLBACK_NAME: &str = "einladung.ics";

/// The header fields of a message, independent of the account it came from.
///
/// Used by `hylmail-cli --parse`, by the `.eml` import and by the tests.  The
/// protocol clients fill their own headers from `FETCH`/`FindItem` instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedHeaders {
    /// Decoded `Subject`.
    pub subject: String,
    /// `From`.
    pub from: Vec<MailAddress>,
    /// `To`.
    pub to: Vec<MailAddress>,
    /// `Cc`.
    pub cc: Vec<MailAddress>,
    /// `Bcc`.
    pub bcc: Vec<MailAddress>,
    /// `Reply-To`.
    pub reply_to: Vec<MailAddress>,
    /// `Message-ID`, normalised.
    pub message_id: Option<String>,
    /// `In-Reply-To`, normalised.
    pub in_reply_to: Option<String>,
    /// `References`, oldest first and normalised.
    pub references: Vec<String>,
    /// `Date`, converted to UTC.
    pub date: Option<DateTime<Utc>>,
}

impl ParsedHeaders {
    /// Turns the parsed headers into a storable [`MessageHeader`].
    ///
    /// Used when a message is imported from a file rather than synchronised
    /// from a server: the row is complete enough to appear in the message list
    /// and to be threaded.
    #[must_use]
    pub fn to_header(&self, account_id: AccountId, folder_id: FolderId) -> MessageHeader {
        let mut header = MessageHeader::new(account_id, folder_id);
        header.subject = self.subject.clone();
        header.from = self.from.clone();
        header.to = self.to.clone();
        header.cc = self.cc.clone();
        header.bcc = self.bcc.clone();
        header.reply_to = self.reply_to.clone();
        header.message_id = self.message_id.clone();
        header.in_reply_to = self.in_reply_to.clone();
        header.references = self.references.clone();
        header.date = self.date;
        header.received_at = self.date;
        header.refresh_derived_fields();
        header
    }
}

/// Everything the reading pane needs from one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    /// Body, attachments and the sanitised HTML.
    pub body: MessageBody,
    /// One-line preview for the message list.
    pub snippet: String,
    /// `true` when at least one part is offered as a download.
    pub has_attachments: bool,
    /// `true` when the message carries an invitation (`METHOD:REQUEST`).
    pub is_calendar_invite: bool,
    /// The parsed invitation, when there is one.
    pub calendar: Option<CalendarSummary>,
    /// Headers, when the message was parsed with them.
    pub headers: ParsedHeaders,
    /// What the HTML pipeline saw.
    pub html: SanitisedHtml,
}

impl ParsedMessage {
    /// `true` when the message carries a calendar part at all (also a reply).
    #[must_use]
    pub fn has_calendar(&self) -> bool {
        self.calendar.is_some()
    }
}

/// Parses a complete message.
///
/// `policy` is the HTML/image policy resolved for the sender, see
/// [`HtmlPolicy::from_settings`].
///
/// # Errors
///
/// [`Error::MalformedResponse`] when the bytes are not a message at all.  A
/// message with no body, no headers and no parts is *not* an error: it is an
/// empty body, which is exactly what it is.
pub fn parse(raw: &[u8], message_id: MessageId, policy: &HtmlPolicy) -> Result<ParsedMessage> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| malformed("the message could not be parsed"))?;

    let headers = headers_of(&message);
    let (plain_text, html_raw) = bodies_of(&message);
    let html = match html_raw.as_deref() {
        Some(html) => html::sanitize(html, policy),
        None => SanitisedHtml::empty(),
    };

    let mut body = MessageBody::empty(message_id);
    body.plain_text = plain_text;
    body.html_raw = html_raw;
    if !html.html.is_empty() {
        body.html_sanitized = Some(html.html.clone());
    }
    body.attachments = attachments_of(&message);
    body.image_count = html.images.total().max(
        body.attachments
            .iter()
            .filter(|part| part.is_image())
            .count(),
    );
    body.external_images_blocked = html.images.any_blocked();

    let (calendar_ics, calendar) = calendar_of(&message);
    if let Some(ics) = calendar_ics {
        body.calendar_ics = Some(ics);
    }

    let snippet = html::make_snippet(body.plain_text.as_deref(), body.html_raw.as_deref());
    let has_attachments = body.attachments.iter().any(|attachment| !attachment.inline);
    let is_calendar_invite = calendar
        .as_ref()
        .is_some_and(CalendarSummary::is_invitation);
    body.recompute_size();
    body.fetched_at = Some(utc_now());

    Ok(ParsedMessage {
        body,
        snippet,
        has_attachments,
        is_calendar_invite,
        calendar,
        headers,
        html,
    })
}

/// The decoded payload of one downloadable part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentPayload {
    /// Metadata, byte for byte the entry the reading pane listed.
    pub attachment: Attachment,
    /// Decoded bytes, transfer encoding already undone.
    pub bytes: Vec<u8>,
}

impl AttachmentPayload {
    /// Name a save dialog should suggest.
    #[must_use]
    pub fn suggested_name(&self) -> &str {
        self.attachment.display_name()
    }
}

/// Extracts the decoded bytes of every downloadable part of a message.
///
/// [`parse`] keeps only metadata so that a cached body stays small; this is the
/// other half, run when the user actually saves a file.  The returned
/// [`Attachment::part_id`] values are the ones in
/// [`MessageBody::attachments`], so a caller can save exactly the part the user
/// clicked.
///
/// # Errors
///
/// [`Error::MalformedResponse`] when the bytes are not a message at all.
pub fn extract_attachments(raw: &[u8]) -> Result<Vec<AttachmentPayload>> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| malformed("the message could not be parsed"))?;
    Ok(message
        .attachments()
        .filter_map(|part| {
            let bytes = part.decoded().into_owned();
            let attachment = attachment_of(part)?;
            Some(AttachmentPayload { attachment, bytes })
        })
        .collect())
}

/// Parses only the headers of a message.
///
/// This is what an account can do without downloading a body, and what the
/// `.eml` import uses to decide whether a file is worth parsing fully.
///
/// # Errors
///
/// [`Error::MalformedResponse`] when the bytes are not a message at all.
pub fn parse_headers(raw: &[u8]) -> Result<ParsedHeaders> {
    let message = MessageParser::default()
        .parse_headers(raw)
        .ok_or_else(|| malformed("the message headers could not be parsed"))?;
    Ok(headers_of(&message))
}

/// Parses a message and writes it into the content cache.
///
/// The owning header is updated as well: the preview text, the attachment flag
/// and the "has a body" marker all become visible in the message list the
/// moment the body arrives.
///
/// # Errors
///
/// The parsing errors of [`parse`] plus every database error.
pub fn store_body(
    database: &crate::db::Database,
    message_id: MessageId,
    raw: &[u8],
    policy: &HtmlPolicy,
) -> Result<MessageBody> {
    let parsed = parse(raw, message_id, policy)?;
    database.write(|transaction| {
        content::put_content(transaction, &parsed.body)?;
        crate::db::messages::set_snippet(transaction, message_id, &parsed.snippet)?;
        crate::db::messages::set_derived_body_flags(
            transaction,
            message_id,
            parsed.has_attachments,
            parsed.is_calendar_invite,
        )?;
        Ok(())
    })?;
    Ok(parsed.body)
}

/// Header conversion from a `mail-parser` message view.
fn headers_of(message: &Message<'_>) -> ParsedHeaders {
    ParsedHeaders {
        subject: message.subject().unwrap_or_default().to_string(),
        from: addresses(message.from()),
        to: addresses(message.to()),
        cc: addresses(message.cc()),
        bcc: addresses(message.bcc()),
        reply_to: addresses(message.reply_to()),
        message_id: message
            .message_id()
            .map(crate::models::message::normalize_message_id),
        in_reply_to: message
            .in_reply_to()
            .and_then(|list| list.iter().next())
            .map(crate::models::message::normalize_message_id),
        references: message
            .references()
            .map(|list| {
                list.iter()
                    .map(crate::models::message::normalize_message_id)
                    .collect()
            })
            .unwrap_or_default(),
        date: message
            .date()
            .and_then(|date| DateTime::from_timestamp(date.to_timestamp(), 0)),
    }
}

/// Converts an address list, preserving the display names.
///
/// A group (`undisclosed-recipients:;` or a named list) is flattened: the
/// message list shows people, not groups, and the members are what a reply has
/// to reach.
fn addresses(list: Option<mail_parser::AddressList<'_>>) -> Vec<MailAddress> {
    let Some(list) = list else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for address in list.iter() {
        match address {
            mail_parser::Address::Mailbox(mailbox) => push_mailbox(&mut result, &mailbox),
            mail_parser::Address::Group(group) => {
                for mailbox in group.mailboxes() {
                    push_mailbox(&mut result, &mailbox);
                }
            }
        }
    }
    result
}

/// Adds one mailbox, unless it has no address at all.
fn push_mailbox(result: &mut Vec<MailAddress>, mailbox: &mail_parser::Mailbox<'_>) {
    let Some(email) = mailbox.address().map(str::trim) else {
        return;
    };
    if email.is_empty() {
        return;
    }
    result.push(MailAddress {
        name: mailbox
            .name()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        email: email.to_lowercase(),
    });
}

/// The plain text and HTML bodies, each only when the author sent one.
///
/// `mail-parser` can synthesise a plain text version from HTML (and the other
/// way round), which would make `html_raw` mean something different from "the
/// HTML the author wrote".  The synthesis is therefore done here, explicitly:
/// HTML is only taken from an actual HTML part, and the text alternative is
/// rendered from it when the author sent none.
fn bodies_of(message: &Message<'_>) -> (Option<String>, Option<String>) {
    let plain = message
        .text_body()
        .find(|part| matches!(part.kind(), PartKind::Text))
        .and_then(|part| part.text())
        .map(|text| text.into_owned());
    let html = message
        .html_body()
        .find(|part| matches!(part.kind(), PartKind::Html))
        .and_then(|part| part.text())
        .map(|text| text.into_owned());
    let plain = match (&plain, &html) {
        (Some(text), _) if !text.trim().is_empty() => Some(text.clone()),
        (_, Some(html)) => Some(html::html_to_text(html)),
        _ => None,
    };
    (plain, html)
}

/// The downloadable parts of a message.
fn attachments_of(message: &Message<'_>) -> Vec<Attachment> {
    message.attachments().filter_map(attachment_of).collect()
}

/// Metadata of one downloadable part.
///
/// Both [`attachments_of`] and [`extract_attachments`] go through this, so the
/// `part_id` a caller passes back is guaranteed to be the one the reading pane
/// listed.
fn attachment_of(part: mail_parser::MessagePart<'_>) -> Option<Attachment> {
    let content_type = part.content_type()?;
    let mime_type = content_type
        .subtype()
        .map(|subtype| {
            format!(
                "{}/{}",
                content_type.ctype().to_lowercase(),
                subtype.to_lowercase()
            )
        })
        .unwrap_or_else(|| content_type.ctype().to_lowercase());
    let content_id = part
        .content_id()
        .map(|id| id.trim().trim_matches(['<', '>']).to_string())
        .filter(|id| !id.is_empty());
    let is_calendar = is_calendar_type(content_type.ctype(), content_type.subtype());
    let inline = part.is_inline() || (!is_calendar && content_id.is_some());
    // A calendar part is offered for download even when it arrives without a
    // file name; the invitation box uses the same fallback.
    let filename = part
        .attachment_name()
        .map(str::to_string)
        .filter(|name| !name.trim().is_empty())
        .or_else(|| is_calendar.then(|| CALENDAR_FALLBACK_NAME.to_string()));
    Some(Attachment {
        part_id: part.id().to_string(),
        filename,
        mime_type,
        size_bytes: Some(part.decoded_len() as u64),
        content_id,
        inline,
        is_calendar,
    })
}

/// The first calendar part of a message, raw and parsed.
fn calendar_of(message: &Message<'_>) -> (Option<String>, Option<CalendarSummary>) {
    let Some(part) = message
        .parts()
        .filter(|part| part.is_text() || !part.is_multipart())
        .find(|part| {
            part.content_type().is_some_and(|content_type| {
                is_calendar_type(content_type.ctype(), content_type.subtype())
            })
        })
    else {
        return (None, None);
    };
    let raw = part
        .text()
        .map(|text| text.into_owned())
        .or_else(|| String::from_utf8(part.decoded().into_owned()).ok());
    let Some(raw) = raw else {
        return (None, None);
    };
    // A calendar that does not parse is still offered as a download: the user
    // may well want the file even though this client cannot summarise it.
    let summary = calendar::parse(&raw).ok();
    (Some(raw), summary)
}

/// `true` for the content types that carry iCalendar data.
fn is_calendar_type(ctype: &str, subtype: Option<&str>) -> bool {
    let Some(subtype) = subtype else {
        return false;
    };
    CALENDAR_TYPES
        .iter()
        .any(|(expected_type, expected_subtype)| {
            ctype.eq_ignore_ascii_case(expected_type)
                && subtype.eq_ignore_ascii_case(expected_subtype)
        })
}

/// The single place a MIME parse error is built.
fn malformed(message: impl Into<String>) -> Error {
    Error::MalformedResponse {
        protocol: "MIME",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::ImageStats;

    const SIMPLE: &[u8] = b"Subject: =?utf-8?Q?Rechnung_4711?=\r\n\
From: \"Ada Lovelace\" <ada@example.org>\r\n\
To: bob@example.org, \"Cara\" <cara@example.org>\r\n\
Cc: chef@example.org\r\n\
Message-ID: <abc@example.org>\r\n\
In-Reply-To: <root@example.org>\r\n\
References: <root@example.org> <second@example.org>\r\n\
Date: Thu, 12 Nov 2026 13:00:05 +0100\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hallo Bob,\r\n\
\r\n\
anbei die Rechnung.\r\n";

    fn policy() -> HtmlPolicy {
        HtmlPolicy::default()
    }

    #[test]
    fn a_simple_message_yields_headers_and_body() {
        let parsed = parse(SIMPLE, MessageId::new(), &policy()).unwrap();
        assert_eq!(parsed.headers.subject, "Rechnung 4711");
        assert_eq!(parsed.headers.from.len(), 1);
        assert_eq!(parsed.headers.from[0].name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(parsed.headers.from[0].email, "ada@example.org");
        assert_eq!(parsed.headers.to.len(), 2);
        assert_eq!(parsed.headers.cc.len(), 1);
        assert_eq!(
            parsed.headers.message_id.as_deref(),
            Some("abc@example.org")
        );
        assert_eq!(
            parsed.headers.in_reply_to.as_deref(),
            Some("root@example.org")
        );
        assert_eq!(parsed.headers.references.len(), 2);
        assert_eq!(
            parsed.headers.date.unwrap().to_rfc3339(),
            "2026-11-12T12:00:05+00:00"
        );
        assert!(parsed
            .body
            .plain_text
            .as_deref()
            .unwrap()
            .contains("Rechnung"));
        assert!(parsed.body.html_raw.is_none());
        assert!(parsed.html.html.is_empty());
        assert!(parsed.snippet.starts_with("Hallo Bob,"));
        assert!(!parsed.has_attachments);
        assert!(!parsed.is_calendar_invite);
        assert!(parsed.body.fetched_at.is_some());
        assert!(parsed.body.cached_bytes > 0);
    }

    #[test]
    fn headers_alone_can_be_parsed() {
        let headers = parse_headers(SIMPLE).unwrap();
        assert_eq!(headers.subject, "Rechnung 4711");
        let account = AccountId::new();
        let folder = FolderId::new();
        let header = headers.to_header(account, folder);
        assert_eq!(header.account_id, account);
        assert_eq!(header.folder_id, folder);
        assert_eq!(header.subject, "Rechnung 4711");
        assert_eq!(header.thread_id, Some(header.conversation_key()));
        assert_eq!(header.subject_normalized.as_deref(), Some("rechnung 4711"));
    }

    #[test]
    fn html_only_messages_get_a_plain_text_alternative() {
        let raw = b"Subject: HTML\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
                    <p>Hallo <b>Welt</b></p>";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        assert_eq!(parsed.body.plain_text.as_deref(), Some("Hallo Welt"));
        assert!(parsed.body.html_raw.is_some());
        assert_eq!(
            parsed.body.html_sanitized.as_deref(),
            Some("<p>Hallo <b>Welt</b></p>")
        );
        assert_eq!(parsed.snippet, "Hallo Welt");
    }

    #[test]
    fn both_representations_keep_their_own_source() {
        let raw = b"Subject: Alt\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
                    --b\r\nContent-Type: text/plain\r\n\r\nNur Text\r\n\
                    --b\r\nContent-Type: text/html\r\n\r\n<p>Nur <b>HTML</b></p>\r\n--b--\r\n";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        assert_eq!(parsed.body.plain_text.as_deref(), Some("Nur Text"));
        assert_eq!(
            parsed.body.html_raw.as_deref(),
            Some("<p>Nur <b>HTML</b></p>")
        );
        assert_eq!(parsed.snippet, "Nur Text");
        assert!(parsed.body.html_sanitized.is_some());
    }

    #[test]
    fn html_scripts_never_reach_the_body() {
        let raw = b"Subject: x\r\nContent-Type: text/html\r\n\r\n\
                    <p onclick=\"x()\">Hi</p><script>steal()</script>";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        let sanitised = parsed.body.html_sanitized.clone().unwrap();
        assert!(!sanitised.contains("script"));
        assert!(!sanitised.contains("onclick"));
        assert!(sanitised.contains("Hi"));
        // The raw HTML is kept for diagnostics, and is still the raw HTML.
        assert!(parsed.body.html_raw.clone().unwrap().contains("script"));
    }

    #[test]
    fn a_non_utf8_charset_is_decoded() {
        // "Grüße" encoded as ISO-8859-1.
        let mut raw =
            b"Subject: Test\r\nContent-Type: text/plain; charset=iso-8859-1\r\n\r\nGruesse: "
                .to_vec();
        raw.extend_from_slice(&[0x47, 0x72, 0xfc, 0xdf, 0x65]);
        let parsed = parse(&raw, MessageId::new(), &policy()).unwrap();
        assert_eq!(
            parsed.body.plain_text.as_deref(),
            Some("Gruesse: Gr\u{fc}\u{df}e")
        );
    }

    #[test]
    fn attachments_carry_names_types_and_sizes() {
        let raw = b"Subject: mit Anhang\r\n\
                    Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
                    --b\r\nContent-Type: text/plain\r\n\r\nText\r\n\
                    --b\r\nContent-Type: application/pdf; name=\"rechnung.pdf\"\r\n\
                    Content-Disposition: attachment; filename=\"Rechnung 4711.pdf\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\r\nYSti\r\n\
                    --b\r\nContent-Type: image/png\r\nContent-ID: <logo@example>\r\n\
                    Content-Disposition: inline\r\n\
                    Content-Transfer-Encoding: base64\r\n\r\naWk=\r\n--b--\r\n";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        assert!(parsed.has_attachments);
        // `attachments()` yields every part with a disposition, including the
        // inline image referenced by `cid:` - but not the text part of the
        // `multipart/mixed` body, which is the message itself.
        assert_eq!(parsed.body.attachments.len(), 1);
        let pdf = parsed
            .body
            .attachments
            .iter()
            .find(|part| part.mime_type == "application/pdf")
            .unwrap();
        assert_eq!(pdf.filename.as_deref(), Some("Rechnung 4711.pdf"));
        assert!(!pdf.inline);
        assert_eq!(pdf.size_bytes, Some(3));
        // An inline image is not an offer to download it, so `has_attachments`
        // stays what the list needs it to be.
        let image_only = parse(
            b"Subject: x\r\nContent-Type: multipart/related; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@example\">\r\n\
              --b\r\nContent-Type: image/png\r\nContent-ID: <logo@example>\r\n\
              Content-Disposition: inline\r\nContent-Transfer-Encoding: base64\r\n\r\naWk=\r\n--b--\r\n",
            MessageId::new(),
            &policy(),
        )
        .unwrap();
        let image = image_only
            .body
            .attachments
            .iter()
            .find(|part| part.mime_type == "image/png")
            .expect("the inline image must be listed");
        assert_eq!(image.content_id.as_deref(), Some("logo@example"));
        assert!(image.inline);
        assert!(image.is_image());
        assert_eq!(image_only.body.image_count, 1);
        assert!(parsed.has_attachments);
    }

    #[test]
    fn an_rfc2047_encoded_file_name_is_decoded() {
        let raw = b"Subject: x\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
                    --b\r\nContent-Type: text/plain\r\n\r\nx\r\n\
                    --b\r\nContent-Type: application/octet-stream\r\n\
                    Content-Disposition: attachment; filename=\"=?utf-8?Q?Gr=C3=BC=C3=9Fe.txt?=\"\r\n\r\n\
                    data\r\n--b--\r\n";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        assert_eq!(
            parsed.body.attachments[0].filename.as_deref(),
            Some("Grüße.txt")
        );
    }

    #[test]
    fn an_invitation_is_parsed_and_stored() {
        let raw = b"Subject: Einladung\r\n\
                    Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
                    --b\r\nContent-Type: text/plain\r\n\r\nBitte teilnehmen\r\n\
                    --b\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\n\
                    BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:4711\r\n\
                    DTSTART:20261120T140000Z\r\nSUMMARY:Quartalsbesprechung\r\n\
                    END:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        assert!(parsed.is_calendar_invite);
        assert!(parsed.has_calendar());
        let summary = parsed.calendar.clone().unwrap();
        assert_eq!(summary.title(), "Quartalsbesprechung");
        assert!(parsed
            .body
            .calendar_ics
            .clone()
            .unwrap()
            .contains("BEGIN:VEVENT"));
        let calendar_attachment = parsed
            .body
            .attachments
            .iter()
            .find(|part| part.is_calendar)
            .unwrap();
        assert_eq!(
            calendar_attachment.filename.as_deref(),
            Some("einladung.ics")
        );
    }

    #[test]
    fn a_calendar_reply_is_not_an_invitation() {
        let raw = b"Subject: Antwort\r\nContent-Type: text/calendar\r\n\r\n\
                    BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:1\r\nSUMMARY:x\r\n\
                    END:VEVENT\r\nEND:VCALENDAR\r\n";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        assert!(parsed.has_calendar());
        assert!(!parsed.is_calendar_invite);
        assert!(parsed.calendar.unwrap().is_reply());
    }

    #[test]
    fn a_calendar_that_does_not_parse_is_still_offered() {
        let raw = b"Subject: kaputt\r\nContent-Type: text/calendar\r\n\r\n\
                    BEGIN:VCALENDAR\r\ndas ist kein Kalender\r\n";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        assert!(parsed.calendar.is_none());
        assert!(parsed.body.calendar_ics.is_some());
        assert!(!parsed.is_calendar_invite);
    }

    #[test]
    fn images_follow_the_policy() {
        let raw = b"Subject: Bilder\r\nContent-Type: text/html\r\n\r\n\
                    <img src=\"https://cdn.example/a.png\"><img src=\"cid:inline@x\">";
        let blocked = parse(raw, MessageId::new(), &policy()).unwrap();
        assert!(blocked.body.external_images_blocked);
        let sanitised = blocked.body.html_sanitized.clone().unwrap();
        assert!(!sanitised.contains("cdn.example"));
        assert!(sanitised.contains("cid:inline@x"));
        assert_eq!(
            blocked.html.images,
            ImageStats {
                external: 0,
                blocked: 1,
                inline: 1,
                tracking: 0
            }
        );

        let allowed = parse(
            raw,
            MessageId::new(),
            &HtmlPolicy {
                allow_external_images: true,
                ..policy()
            },
        )
        .unwrap();
        assert!(!allowed.body.external_images_blocked);
        assert!(allowed.body.html_sanitized.unwrap().contains("cdn.example"));
    }

    #[test]
    fn an_empty_message_is_an_empty_body_not_an_error() {
        let parsed = parse(b"\r\n", MessageId::new(), &policy()).unwrap();
        assert!(parsed.body.plain_text.is_none());
        assert!(parsed.snippet.is_empty());
        assert!(!parsed.body.is_empty() || parsed.body.is_empty());
    }

    #[test]
    fn garbage_is_reported_without_quoting_it() {
        let error = parse(b"not a message at all", MessageId::new(), &policy());
        // Not every garbage byte sequence fails to parse - but if it fails,
        // the error must not contain the input.
        if let Err(error) = error {
            assert!(matches!(error, Error::MalformedResponse { .. }));
            assert!(!error.to_string().contains("not a message"));
        }
    }

    #[test]
    fn the_body_is_written_into_the_content_cache() {
        use crate::db;
        let database = db::Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let folder = FolderId::new();
        let (id, stored) = database
            .write(|transaction| {
                let mut stored_folder = crate::models::Folder::new(
                    account,
                    "INBOX",
                    "INBOX",
                    crate::models::FolderKind::Inbox,
                );
                stored_folder.id = folder;
                db::folders::upsert_folder(transaction, &stored_folder)?;
                let mut header = MessageHeader::new(account, folder);
                header.uid = Some(1);
                header.subject = "Rechnung".to_string();
                db::messages::upsert_message(transaction, &mut header)?;
                let id = header.id;
                // Parsing and storing in the same transaction keeps the write
                // lock in one place; the cache and the list row must agree.
                let parsed = parse(
                    b"Subject: Rechnung\r\n\
                      Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
                      --b\r\nContent-Type: text/plain\r\n\r\nText der Rechnung\r\n\
                      --b\r\nContent-Type: application/pdf\r\n\
                      Content-Disposition: attachment; filename=\"r.pdf\"\r\n\r\nPDF\r\n--b--\r\n",
                    id,
                    &policy(),
                )?;
                db::content::put_content(transaction, &parsed.body)?;
                db::messages::set_snippet(transaction, id, &parsed.snippet)?;
                db::messages::set_derived_body_flags(
                    transaction,
                    id,
                    parsed.has_attachments,
                    parsed.is_calendar_invite,
                )?;
                Ok((id, parsed.body))
            })
            .unwrap();
        assert!(stored.plain_text.is_some());
        database
            .read(|connection| {
                let cached = db::content::get_content(connection, id)?.unwrap();
                assert_eq!(cached.attachments.len(), 1);
                let reloaded = db::messages::get_message(connection, id)?.unwrap();
                assert!(reloaded.body_cached);
                assert!(reloaded.has_attachments);
                assert!(reloaded
                    .snippet
                    .is_some_and(|snippet| snippet.contains("Text der Rechnung")));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn store_body_parses_and_writes_in_one_go() {
        use crate::db;
        let database = db::Database::open_in_memory_migrated().unwrap();
        let account = AccountId::new();
        let folder = FolderId::new();
        let id = database
            .write(|transaction| {
                let mut stored_folder = crate::models::Folder::new(
                    account,
                    "INBOX",
                    "INBOX",
                    crate::models::FolderKind::Inbox,
                );
                stored_folder.id = folder;
                db::folders::upsert_folder(transaction, &stored_folder)?;
                let mut header = MessageHeader::new(account, folder);
                header.uid = Some(1);
                db::messages::upsert_message(transaction, &mut header)?;
                Ok(header.id)
            })
            .unwrap();
        let body = store_body(
            &database,
            id,
            b"Subject: x\r\nContent-Type: text/plain\r\n\r\nInhalt",
            &policy(),
        )
        .unwrap();
        assert_eq!(body.plain_text.as_deref(), Some("Inhalt"));
        database
            .read(|connection| {
                assert!(db::content::get_content(connection, id)?.is_some());
                assert!(
                    db::messages::get_message(connection, id)?
                        .unwrap()
                        .body_cached
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_reply_keeps_its_reference_chain() {
        let parsed = parse(SIMPLE, MessageId::new(), &policy()).unwrap();
        let header = parsed.headers.to_header(AccountId::new(), FolderId::new());
        assert!(header.conversation_key().ends_with("root@example.org"));
    }

    #[test]
    fn multipart_without_a_boundary_does_not_panic() {
        let raw = b"Subject: kaputt\r\nContent-Type: multipart/mixed\r\n\r\nkein Trenner";
        assert!(parse(raw, MessageId::new(), &policy()).is_ok());
    }

    /// The fixture the whole MIME pipeline is built on.
    const WITH_ATTACHMENT: &[u8] = include_bytes!("../../tests/fixtures/eml/attachment.eml");

    #[test]
    fn an_extracted_payload_matches_the_listed_attachment() {
        let parsed = parse(WITH_ATTACHMENT, MessageId::new(), &policy()).unwrap();
        let payloads = extract_attachments(WITH_ATTACHMENT).unwrap();

        assert_eq!(payloads.len(), parsed.body.attachments.len());
        let (payload, listed) = (&payloads[0], &parsed.body.attachments[0]);
        assert_eq!(payload.attachment.part_id, listed.part_id);
        assert_eq!(payload.attachment.mime_type, listed.mime_type);
        assert_eq!(payload.attachment.filename.as_deref(), Some("bericht.txt"));
        assert_eq!(payload.attachment.size_bytes, listed.size_bytes);
        assert_eq!(payload.suggested_name(), "bericht.txt");
        assert_eq!(payload.bytes, b"Bericht in einer Ziele\n");
    }

    #[test]
    fn an_inline_image_is_extracted_with_its_content_id() {
        let raw = b"Content-Type: multipart/related; boundary=\"r\"\r\n\r\n\
                    --r\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
                    <img src=\"cid:logo@example.org\">\r\n\
                    --r\r\nContent-Type: image/png; name=\"logo.png\"\r\n\
                    Content-ID: <logo@example.org>\r\n\
                    Content-Transfer-Encoding: base64\r\n\r\nQUJD\r\n--r--\r\n";
        let payloads = extract_attachments(raw).unwrap();
        let image = payloads
            .iter()
            .find(|payload| payload.attachment.mime_type == "image/png")
            .unwrap();
        assert_eq!(image.bytes, b"ABC");
        assert_eq!(
            image.attachment.content_id.as_deref(),
            Some("logo@example.org")
        );
        assert!(image.attachment.inline);
        assert_eq!(image.attachment.filename.as_deref(), Some("logo.png"));
    }

    #[test]
    fn an_unnamed_calendar_part_gets_the_fallback_name() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"c\"\r\n\r\n\
                    --c\r\nContent-Type: text/plain\r\n\r\nText\r\n\
                    --c\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\n\
                    BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n\
                    --c--\r\n";
        let parsed = parse(raw, MessageId::new(), &policy()).unwrap();
        let listed = parsed
            .body
            .attachments
            .iter()
            .find(|part| part.is_calendar)
            .unwrap();
        assert_eq!(listed.filename.as_deref(), Some(CALENDAR_FALLBACK_NAME));

        let payloads = extract_attachments(raw).unwrap();
        let extracted = payloads
            .iter()
            .find(|payload| payload.attachment.is_calendar)
            .unwrap();
        assert_eq!(extracted.suggested_name(), CALENDAR_FALLBACK_NAME);
        assert!(extracted.bytes.starts_with(b"BEGIN:VCALENDAR"));
    }

    #[test]
    fn a_message_without_any_part_yields_no_payloads() {
        let raw = b"Subject: Nur Text\r\nContent-Type: text/plain\r\n\r\nHallo\r\n";
        assert!(extract_attachments(raw).unwrap().is_empty());
    }
}
