//! Turning `FETCH` responses into the local message model.
//!
//! Two very different things come out of a `FETCH`:
//!
//! * the **header**: `UID`, `FLAGS`, `INTERNALDATE`, `RFC822.SIZE` and
//!   `ENVELOPE`.  This is what the message list needs, and it is what the
//!   synchroniser stores for every message.
//! * the **structure**: `BODYSTRUCTURE`, which tells the client — without
//!   downloading a single byte of body — whether a message has attachments,
//!   whether it is a calendar invitation, and which section to fetch later.
//!
//! Fetching the body itself is a separate, on-demand step (phase 3).

use std::borrow::Cow;

use async_imap::types::Fetch;
use imap_proto::{
    Address, BodyContentCommon, BodyContentSinglePart, BodyStructure, ContentDisposition,
    ContentType, Envelope,
};

use crate::models::ids::{AccountId, FolderId};
use crate::models::message::{normalize_message_id, Attachment, MailAddress, MessageHeader};

use super::flags;
use super::header;

/// The `FETCH` item list used to synchronise a folder.
///
/// `BODY.PEEK` is used instead of `BODY` so that synchronising never marks a
/// message as read as a side effect — a classic bug in naive clients.  The
/// `References` header is fetched as well because threading needs it and it is
/// a few dozen bytes.  `BODYSTRUCTURE` is included because it is the only way
/// to know whether a message has attachments without downloading its body; a
/// server that has to parse a large message to answer it pays for that once and
/// then caches the result.
pub const HEADER_ITEMS: &str = "(UID FLAGS RFC822.SIZE INTERNALDATE ENVELOPE BODYSTRUCTURE \
     BODY.PEEK[HEADER.FIELDS (REFERENCES)])";

/// The `FETCH` items needed for the flag-only reconciliation pass.
pub const FLAG_ITEMS: &str = "(UID FLAGS)";

/// The `FETCH` items needed to download a whole message body.
pub const BODY_ITEMS: &str = "(UID BODY.PEEK[])";

/// Header data that is not part of the `ENVELOPE`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtraHeaders {
    /// The `References` header, split into normalised message ids.
    pub references: Vec<String>,
}

/// Extracts `References` from a partial header fetch.
///
/// The header block arrives as raw bytes with folded lines (a leading space or
/// tab continues the previous line), so the lines are unfolded before the value
/// can be split.
#[must_use]
pub fn parse_extra_headers(raw: &[u8]) -> ExtraHeaders {
    let text = header::decode_bytes(raw);
    let mut unfolded = String::with_capacity(text.len());
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with(' ') || line.starts_with('\t') {
            unfolded.push(' ');
            unfolded.push_str(line.trim_start());
        } else {
            unfolded.push('\n');
            unfolded.push_str(line);
        }
    }

    let mut extra = ExtraHeaders::default();
    for line in unfolded.split('\n') {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("references") {
            continue;
        }
        for part in value.split_whitespace() {
            let reference = normalize_message_id(part);
            if !reference.is_empty() && !extra.references.contains(&reference) {
                extra.references.push(reference);
            }
        }
    }
    extra
}

/// Fills a [`MessageHeader`] from one `FETCH` response.
pub fn header_from_fetch(
    account_id: AccountId,
    folder_id: FolderId,
    fetch: &Fetch,
) -> MessageHeader {
    let mut message = MessageHeader::new(account_id, folder_id);
    message.uid = fetch.uid;
    message.size_bytes = u64::from(fetch.size.unwrap_or(0));
    message.received_at = fetch
        .internal_date()
        .map(|date| date.with_timezone(&chrono::Utc));

    let atoms: Vec<String> = flags::atoms_from_flags(fetch.flags());
    message.flags = flags::message_flags_from_atoms(atoms.iter().map(String::as_str));

    if let Some(envelope) = fetch.envelope() {
        apply_envelope(&mut message, envelope);
    }

    if let Some(structure) = fetch.bodystructure() {
        message.has_attachments = structure_has_attachments(structure);
        message.is_calendar_invite = structure_is_calendar(structure);
    }

    if let Some(raw) = fetch.header() {
        let extra = parse_extra_headers(raw);
        if message.in_reply_to.is_none() {
            message.in_reply_to = extra.references.last().cloned();
        }
        message.references = extra.references;
    }

    message.refresh_derived_fields();
    message
}

/// Copies the `ENVELOPE` fields into a header.
pub fn apply_envelope(message: &mut MessageHeader, envelope: &Envelope<'_>) {
    message.subject = envelope
        .subject
        .as_deref()
        .map(header::decode_header)
        .unwrap_or_default();
    message.message_id = envelope
        .message_id
        .as_deref()
        .map(header::decode_bytes)
        .map(|raw| normalize_message_id(&raw))
        .filter(|value| !value.is_empty());
    message.in_reply_to = envelope
        .in_reply_to
        .as_deref()
        .map(header::decode_bytes)
        .map(|raw| normalize_message_id(&raw))
        .filter(|value| !value.is_empty());

    message.from = addresses(envelope.from.as_ref());
    message.to = addresses(envelope.to.as_ref());
    message.cc = addresses(envelope.cc.as_ref());
    message.bcc = addresses(envelope.bcc.as_ref());
    message.reply_to = addresses(envelope.reply_to.as_ref());

    if let Some(date) = envelope.date.as_deref() {
        let raw = header::decode_bytes(date);
        message.date = header::parse_envelope_date(&raw);
    }
}

fn addresses(field: Option<&Vec<Address<'_>>>) -> Vec<MailAddress> {
    let Some(field) = field else {
        return Vec::new();
    };
    field
        .iter()
        .filter_map(|address| {
            header::address_from_envelope(
                address.name.as_deref(),
                address.mailbox.as_deref(),
                address.host.as_deref(),
            )
        })
        .collect()
}

/// Walks a `BODYSTRUCTURE` and collects every attachment-like part.
///
/// A part counts as an attachment when it has a file name, is a calendar part,
/// or is not `text/*` at all (an inline image, for example).  The section
/// identifier is the one to use in `BODY.PEEK[<id>]`.
#[must_use]
pub fn attachments(structure: &BodyStructure<'_>) -> Vec<Attachment> {
    let mut parts = Vec::new();
    let mut path = Vec::new();
    collect_attachments(structure, &mut path, &mut parts);
    parts
}

/// `true` when the structure contains at least one attachment.
#[must_use]
pub fn structure_has_attachments(structure: &BodyStructure<'_>) -> bool {
    !attachments(structure).is_empty()
}

/// `true` when the message carries a `text/calendar` part, i.e. it is an
/// invitation the user can answer.
#[must_use]
pub fn structure_is_calendar(structure: &BodyStructure<'_>) -> bool {
    let mut found = false;
    walk_types(structure, &mut |content_type| {
        if is_calendar_part(content_type) {
            found = true;
        }
    });
    found
}

/// Visits every content type of a structure, including nested ones.
fn walk_types<'a>(structure: &'a BodyStructure<'a>, visit: &mut impl FnMut(&'a ContentType<'a>)) {
    match structure {
        BodyStructure::Basic { common, .. } | BodyStructure::Text { common, .. } => {
            visit(&common.ty);
        }
        BodyStructure::Message { common, body, .. } => {
            visit(&common.ty);
            walk_types(body, visit);
        }
        BodyStructure::Multipart { bodies, .. } => {
            for body in bodies {
                walk_types(body, visit);
            }
        }
    }
}

/// `true` for `text/calendar`, `application/ics` and files named `*.ics`.
#[must_use]
pub fn is_calendar_part(content_type: &ContentType<'_>) -> bool {
    let ty = content_type.ty.to_ascii_lowercase();
    let subtype = content_type.subtype.to_ascii_lowercase();
    if (ty == "text" && subtype == "calendar")
        || (ty == "application" && matches!(subtype.as_str(), "ics" | "icalendar"))
    {
        return true;
    }
    filename_of(content_type).is_some_and(|name| {
        let name = name.to_ascii_lowercase();
        name.ends_with(".ics") || name.ends_with(".ical")
    })
}

/// The `name` parameter of a content type, i.e. the file name.
#[must_use]
pub fn filename_of(content_type: &ContentType<'_>) -> Option<String> {
    parameter_of(content_type.params.as_deref(), "name")
        .map(|name| header::decode_header(name.as_bytes()))
        .filter(|name| !name.trim().is_empty())
}

/// The `filename` parameter of a content disposition.
#[must_use]
pub fn disposition_filename(disposition: &ContentDisposition<'_>) -> Option<String> {
    parameter_of(disposition.params.as_deref(), "filename")
        .map(|name| header::decode_header(name.as_bytes()))
        .filter(|name| !name.trim().is_empty())
}

type Parameters<'a> = Option<&'a [(Cow<'a, str>, Cow<'a, str>)]>;

fn parameter_of(params: Parameters<'_>, key: &str) -> Option<String> {
    let params = params?;
    params
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.to_string())
}

fn collect_attachments<'a>(
    structure: &'a BodyStructure<'a>,
    path: &mut Vec<u32>,
    out: &mut Vec<Attachment>,
) {
    match structure {
        BodyStructure::Multipart { bodies, .. } => {
            for (index, body) in bodies.iter().enumerate() {
                let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
                path.push(number);
                collect_attachments(body, path, out);
                path.pop();
            }
        }
        BodyStructure::Message {
            common,
            body,
            other,
            ..
        } => {
            if let Some(part) = attachment_of(common, other, section_id(path)) {
                out.push(part);
            }
            // The encapsulated message is numbered below the current part: a
            // multipart body keeps its own 1..n numbering, a single part is
            // addressed as `<n>.1`.
            if matches!(**body, BodyStructure::Multipart { .. }) {
                collect_attachments(body, path, out);
            } else {
                path.push(1);
                collect_attachments(body, path, out);
                path.pop();
            }
        }
        BodyStructure::Text { common, other, .. } | BodyStructure::Basic { common, other, .. } => {
            if let Some(part) = attachment_of(common, other, section_id(path)) {
                out.push(part);
            }
        }
    }
}

/// The IMAP section identifier of a part path.
///
/// A message that is not multipart has exactly one part, and that part is
/// section `1` (`BODY[1]`), which is why an empty path becomes `"1"`.
fn section_id(path: &[u32]) -> String {
    if path.is_empty() {
        return "1".to_string();
    }
    path.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// Builds an [`Attachment`] for a part, or `None` when the part is ordinary
/// message text.
fn attachment_of(
    common: &BodyContentCommon<'_>,
    other: &BodyContentSinglePart<'_>,
    section: String,
) -> Option<Attachment> {
    let ty = common.ty.ty.to_ascii_lowercase();
    let subtype = common.ty.subtype.to_ascii_lowercase();
    let mime_type = format!("{ty}/{subtype}");

    let disposition = common.disposition.as_ref();
    let filename = disposition
        .and_then(disposition_filename)
        .or_else(|| filename_of(&common.ty));
    let is_inline = disposition
        .map(|disposition| disposition.ty.eq_ignore_ascii_case("inline"))
        .unwrap_or(false);
    let is_calendar = is_calendar_part(&common.ty);

    let attachable = filename.is_some() || is_calendar || ty != "text";
    if !attachable {
        return None;
    }

    Some(Attachment {
        part_id: section,
        filename,
        mime_type,
        size_bytes: Some(u64::from(other.octets)),
        content_id: other
            .id
            .as_deref()
            .map(|id| id.trim_matches(['<', '>']).trim().to_string())
            .filter(|id| !id.is_empty()),
        inline: is_inline,
        is_calendar,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::message::MessageFlags;
    use imap_proto::ContentEncoding;

    fn common(ty: &str, subtype: &str, name: Option<&str>) -> BodyContentCommon<'static> {
        BodyContentCommon {
            ty: ContentType {
                ty: Cow::Owned(ty.to_string()),
                subtype: Cow::Owned(subtype.to_string()),
                params: name
                    .map(|name| vec![(Cow::Borrowed("name"), Cow::Owned(name.to_string()))]),
            },
            disposition: None,
            language: None,
            location: None,
        }
    }

    fn single_part(octets: u32, id: Option<&str>) -> BodyContentSinglePart<'static> {
        BodyContentSinglePart {
            id: id.map(|id| Cow::Owned(id.to_string())),
            md5: None,
            description: None,
            transfer_encoding: ContentEncoding::SevenBit,
            octets,
        }
    }

    fn text(subtype: &str) -> BodyStructure<'static> {
        BodyStructure::Text {
            common: common("text", subtype, None),
            other: single_part(120, None),
            lines: 3,
            extension: None,
        }
    }

    fn with_disposition(
        mut base: BodyContentCommon<'static>,
        ty: &str,
        filename: Option<&str>,
    ) -> BodyContentCommon<'static> {
        base.disposition = Some(ContentDisposition {
            ty: Cow::Owned(ty.to_string()),
            params: filename.map(|filename| {
                vec![(Cow::Borrowed("filename"), Cow::Owned(filename.to_string()))]
            }),
        });
        base
    }

    #[test]
    fn references_are_unfolded_and_normalised() {
        let raw = b"Subject: Re: Test\r\nReferences: <a@example.org>\r\n <b@example.org>\r\n\t<c@example.org>\r\nX-Other: ignored\r\n";
        let extra = parse_extra_headers(raw);
        assert_eq!(
            extra.references,
            vec!["a@example.org", "b@example.org", "c@example.org"]
        );
    }

    #[test]
    fn duplicate_and_odd_references_are_handled() {
        let raw = b"References: <a@x> <a@x> not-an-id\r\n";
        let extra = parse_extra_headers(raw);
        assert_eq!(extra.references, vec!["a@x", "not-an-id"]);
        assert!(parse_extra_headers(b"Subject: hi\r\n")
            .references
            .is_empty());
    }

    #[test]
    fn a_plain_text_body_is_not_an_attachment() {
        assert!(attachments(&text("plain")).is_empty());
        assert!(attachments(&text("html")).is_empty());
    }

    #[test]
    fn a_named_text_part_is_an_attachment() {
        let structure = BodyStructure::Text {
            common: with_disposition(
                common("text", "plain", None),
                "attachment",
                Some("notes.txt"),
            ),
            other: single_part(42, None),
            lines: 1,
            extension: None,
        };
        let parts = attachments(&structure);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].filename.as_deref(), Some("notes.txt"));
        assert_eq!(parts[0].part_id, "1");
        assert_eq!(parts[0].size_bytes, Some(42));
        assert!(!parts[0].inline);
    }

    #[test]
    fn a_multipart_message_numbers_its_parts() {
        let structure = BodyStructure::Multipart {
            common: common("multipart", "mixed", None),
            bodies: vec![
                text("plain"),
                BodyStructure::Basic {
                    common: with_disposition(
                        common("application", "pdf", None),
                        "attachment",
                        Some("rechnung.pdf"),
                    ),
                    other: single_part(2048, None),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = attachments(&structure);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_id, "2");
        assert_eq!(parts[0].mime_type, "application/pdf");
        assert_eq!(parts[0].filename.as_deref(), Some("rechnung.pdf"));
        assert!(structure_has_attachments(&structure));
    }

    #[test]
    fn an_inline_image_is_reported_with_its_content_id() {
        let structure = BodyStructure::Multipart {
            common: common("multipart", "related", None),
            bodies: vec![
                text("html"),
                BodyStructure::Basic {
                    common: with_disposition(common("image", "png", None), "inline", None),
                    other: single_part(512, Some("<logo@example.org>")),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = attachments(&structure);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_id, "2");
        assert_eq!(parts[0].content_id.as_deref(), Some("logo@example.org"));
        assert!(parts[0].inline);
        assert!(parts[0].is_image());
    }

    #[test]
    fn a_nested_multipart_is_walked() {
        let inner = BodyStructure::Multipart {
            common: common("multipart", "alternative", None),
            bodies: vec![text("plain"), text("html")],
            extension: None,
        };
        let structure = BodyStructure::Multipart {
            common: common("multipart", "mixed", None),
            bodies: vec![
                inner,
                BodyStructure::Basic {
                    common: with_disposition(
                        common("application", "octet-stream", None),
                        "attachment",
                        Some("data.bin"),
                    ),
                    other: single_part(9, None),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = attachments(&structure);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_id, "2");
    }

    #[test]
    fn a_calendar_invitation_is_recognised() {
        let structure = BodyStructure::Multipart {
            common: common("multipart", "mixed", None),
            bodies: vec![
                text("plain"),
                BodyStructure::Basic {
                    common: with_disposition(
                        common("text", "calendar", Some("invite.ics")),
                        "attachment",
                        Some("invite.ics"),
                    ),
                    other: single_part(900, None),
                    extension: None,
                },
            ],
            extension: None,
        };
        assert!(structure_is_calendar(&structure));
        let parts = attachments(&structure);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].is_calendar);
    }

    #[test]
    fn calendar_detection_covers_the_usual_spellings() {
        assert!(is_calendar_part(&common("text", "calendar", None).ty));
        assert!(is_calendar_part(&common("TEXT", "CALENDAR", None).ty));
        assert!(is_calendar_part(&common("application", "ics", None).ty));
        assert!(is_calendar_part(
            &common("application", "octet-stream", Some("Termin.ICS")).ty
        ));
        assert!(!is_calendar_part(&common("text", "plain", None).ty));
        assert!(!is_calendar_part(&common("image", "png", None).ty));
    }

    #[test]
    fn file_names_are_decoded() {
        let content_type = common(
            "application",
            "pdf",
            Some("=?UTF-8?Q?Gr=C3=BC=C3=9Fe.pdf?="),
        )
        .ty;
        assert_eq!(filename_of(&content_type).as_deref(), Some("Grüße.pdf"));
        assert!(filename_of(&common("application", "pdf", None).ty).is_none());
        assert!(filename_of(&common("application", "pdf", Some("   ")).ty).is_none());
    }

    #[test]
    fn an_envelope_fills_every_header_field() {
        let mut message = MessageHeader::new(AccountId::new(), FolderId::new());
        let envelope = Envelope {
            date: Some(Cow::Borrowed(b"Thu, 12 Nov 2026 14:00:00 +0100".as_ref())),
            subject: Some(Cow::Borrowed("=?UTF-8?Q?Gr=C3=BC=C3=9Fe?=".as_ref())),
            from: Some(vec![Address {
                name: Some(Cow::Borrowed(b"Ada".as_ref())),
                adl: None,
                mailbox: Some(Cow::Borrowed(b"ada".as_ref())),
                host: Some(Cow::Borrowed(b"example.org".as_ref())),
            }]),
            sender: None,
            reply_to: None,
            to: Some(vec![Address {
                name: None,
                adl: None,
                mailbox: Some(Cow::Borrowed(b"billing".as_ref())),
                host: Some(Cow::Borrowed(b"example.org".as_ref())),
            }]),
            cc: None,
            bcc: None,
            in_reply_to: Some(Cow::Borrowed(b"<parent@example.org>".as_ref())),
            message_id: Some(Cow::Borrowed(b"<id@example.org>".as_ref())),
        };
        apply_envelope(&mut message, &envelope);

        assert_eq!(message.subject, "Grüße");
        assert_eq!(message.message_id.as_deref(), Some("id@example.org"));
        assert_eq!(message.in_reply_to.as_deref(), Some("parent@example.org"));
        assert_eq!(message.from.len(), 1);
        assert_eq!(message.from[0].email, "ada@example.org");
        assert_eq!(message.from[0].name.as_deref(), Some("Ada"));
        assert_eq!(message.to[0].email, "billing@example.org");
        assert!(message.date.is_some());
        assert!(message.cc.is_empty());
    }

    #[test]
    fn an_empty_envelope_leaves_the_defaults() {
        let mut message = MessageHeader::new(AccountId::new(), FolderId::new());
        let envelope = Envelope {
            date: None,
            subject: None,
            from: None,
            sender: None,
            reply_to: None,
            to: None,
            cc: None,
            bcc: None,
            in_reply_to: None,
            message_id: None,
        };
        apply_envelope(&mut message, &envelope);
        assert!(message.subject.is_empty());
        assert!(message.message_id.is_none());
        assert!(message.from.is_empty());
        assert!(message.date.is_none());
        assert_eq!(message.flags, MessageFlags::empty());
    }

    #[test]
    fn section_ids_follow_the_imap_numbering() {
        assert_eq!(section_id(&[]), "1");
        assert_eq!(section_id(&[1]), "1");
        assert_eq!(section_id(&[2]), "2");
        assert_eq!(section_id(&[2, 1]), "2.1");
        assert_eq!(section_id(&[3, 2, 1]), "3.2.1");
    }

    #[test]
    fn an_embedded_message_is_numbered_below_its_parent() {
        let structure = BodyStructure::Multipart {
            common: common("multipart", "mixed", None),
            bodies: vec![
                text("plain"),
                BodyStructure::Message {
                    common: with_disposition(
                        common("message", "rfc822", None),
                        "attachment",
                        Some("forwarded.eml"),
                    ),
                    other: single_part(4096, None),
                    envelope: Envelope {
                        date: None,
                        subject: None,
                        from: None,
                        sender: None,
                        reply_to: None,
                        to: None,
                        cc: None,
                        bcc: None,
                        in_reply_to: None,
                        message_id: None,
                    },
                    body: Box::new(text("plain")),
                    lines: 10,
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = attachments(&structure);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_id, "2");
        assert_eq!(parts[0].mime_type, "message/rfc822");
        assert_eq!(parts[0].filename.as_deref(), Some("forwarded.eml"));
    }
}
