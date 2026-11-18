//! Emitting a draft as RFC 5322 bytes.

use std::fmt;

use lettre::address::Envelope;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};
use lettre::Address;

use super::ComposeContext;
use crate::error::{Error, Result};
use crate::models::{Draft, MailAddress};

/// The message as it goes to the SMTP server.
#[derive(Clone, PartialEq, Eq)]
pub struct ComposedMessage {
    /// The complete message, ready for `DATA`.
    pub raw: Vec<u8>,
    /// Envelope sender.
    pub from: String,
    /// Envelope recipients: `To` + `Cc` + `Bcc`, each address once.
    pub recipients: Vec<String>,
    /// The generated `Message-ID`, so the Sent copy can carry the same one.
    pub message_id: String,
    /// The `Subject`, so the Sent copy and the outbox agree.
    pub subject: String,
}

impl ComposedMessage {
    /// The envelope as `lettre` needs it, for callers that use its transport
    /// directly.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when an address in the envelope is not a valid
    /// address, which [`build`] already rejects.
    pub fn envelope(&self) -> Result<Envelope> {
        let from = parse_address(&self.from)?;
        let mut to = Vec::with_capacity(self.recipients.len());
        for recipient in &self.recipients {
            to.push(parse_address(recipient)?);
        }
        Envelope::new(Some(from), to)
            .map_err(|error| Error::config(format!("invalid recipient list: {error}")))
    }

    /// The message as a string, for `--send --dry-run` and the tests.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when the headers are not valid UTF-8, which cannot
    /// happen for a message this module built.
    pub fn as_str(&self) -> Result<&str> {
        std::str::from_utf8(&self.raw).map_err(|_| {
            Error::protocol("MIME", "FORMAT", "the composed message is not valid UTF-8")
        })
    }

    /// The envelope as the CLI prints it, without asking for validity.
    #[must_use]
    pub fn envelope_from_draft(draft: &Draft) -> Self {
        Self {
            raw: Vec::new(),
            from: String::new(),
            recipients: envelope(&draft.to, &draft.cc, &draft.bcc),
            message_id: String::new(),
            subject: String::new(),
        }
    }
}

/// A source of `Message-ID` values.
///
/// Injected so tests get a stable identifier instead of a random one.
pub trait MessageIdGenerator {
    /// Produces a new message id for a draft sent from `from`.
    fn generate(&self, from: &str, subject: &str, nonce: u64) -> String;
}

/// The production generator: `<timestamp.nonce@domain>`.
///
/// It copies what real clients do - a timestamp, a counter and the sender's
/// domain - without pretending that a `Message-ID` has to be globally unique to
/// be useful. What matters for threading is that it is **stable**: the message
/// list, the Sent copy and the recipient's reply all carry the same one.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimestampMessageId;

impl MessageIdGenerator for TimestampMessageId {
    fn generate(&self, from: &str, subject: &str, nonce: u64) -> String {
        let domain = from
            .rsplit_once('@')
            .map(|(_, domain)| domain)
            .filter(|domain| !domain.is_empty())
            .unwrap_or("hylmail.local");
        let millis = crate::models::ids::utc_now().timestamp_millis();
        // A short digest of the subject keeps two messages sent in the same
        // millisecond distinguishable without putting the subject in the id.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in subject.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{millis}.{nonce}.{:08x}@{domain}", (hash >> 32) as u32)
    }
}

/// Builds the message a draft describes.
///
/// # Errors
///
/// * [`Error::Config`] when the draft has no recipient, no sender or an
///   address that is not an address.
/// * [`Error::Io`] when an attachment file cannot be read.
/// * [`Error::Unsupported`] when the draft references a `cid:` image whose
///   file is missing - silently dropping the image would produce a message with
///   a broken picture, which is worse than refusing to send it.
pub fn build(draft: &Draft, context: &ComposeContext) -> Result<ComposedMessage> {
    build_with(draft, context, &TimestampMessageId, 0)
}

/// [`build`] with an explicit id generator, so tests are deterministic.
///
/// # Errors
///
/// See [`build`].
pub fn build_with(
    draft: &Draft,
    context: &ComposeContext,
    generator: &dyn MessageIdGenerator,
    nonce: u64,
) -> Result<ComposedMessage> {
    let from = context
        .from
        .clone()
        .ok_or_else(|| Error::config("a message needs a sender"))?;
    let from_address = parse_address(&from.email)?;
    if draft.to.is_empty() {
        return Err(Error::config(
            "a message needs at least one recipient before it can be sent",
        ));
    }

    let recipients = envelope(&draft.to, &draft.cc, &draft.bcc);
    let message_id = context
        .message_id
        .clone()
        .unwrap_or_else(|| generator.generate(&from.email, &draft.subject, nonce));

    let body = body_of(draft, context)?;
    let mut builder = Message::builder()
        .from(mailbox(&from)?)
        .subject(if draft.subject.is_empty() {
            String::new()
        } else {
            draft.subject.clone()
        })
        .message_id(Some(message_id.clone()))
        .user_agent(format!("HylMail {}", env!("CARGO_PKG_VERSION")));

    if let Some(sender) = &context.sender {
        builder = builder.sender(mailbox(&MailAddress::new(sender.clone()))?);
    }
    for address in &draft.to {
        builder = builder.to(mailbox(address)?);
    }
    for address in &draft.cc {
        builder = builder.cc(mailbox(address)?);
    }
    // `bcc` is deliberately absent: it lives in the envelope only.
    if let Some(reply) = &context.reply {
        if let Some(parent) = &reply.in_reply_to {
            builder = builder.in_reply_to(parent.clone());
        }
        if !reply.references.is_empty() {
            // `references()` wraps the value and would replace the header on
            // every call, so the whole chain goes in at once.
            let chain: Vec<String> = reply
                .references
                .iter()
                .map(|id| format!("<{}>", id.trim().trim_matches(['<', '>'])))
                .collect();
            builder = builder.references(chain.join(" "));
        }
    }
    builder = builder.date(
        context
            .date
            .unwrap_or_else(crate::models::ids::utc_now)
            .into(),
    );

    let message = builder
        .multipart(body)
        .map_err(|error| Error::config(format!("the message could not be assembled: {error}")))?;

    let raw = message.formatted();
    Ok(ComposedMessage {
        raw,
        from: from_address.to_string(),
        recipients,
        message_id,
        subject: draft.subject.clone(),
    })
}

/// Only the headers, without reading the attachments.
///
/// Used by the "Entwurf als Datei exportieren" action, which wants something
/// readable more than it wants the exact bytes.
///
/// # Errors
///
/// See [`build`].
pub fn build_headers_only(draft: &Draft, context: &ComposeContext) -> Result<ComposedMessage> {
    let mut slim = draft.clone();
    slim.attachments.clear();
    build(&slim, context)
}

/// The body structure: alternative body, then the attachments.
fn body_of(draft: &Draft, context: &ComposeContext) -> Result<MultiPart> {
    let plain = plain_body(draft, context);
    let html = html_body(draft, context);
    if plain.is_empty() && html.is_empty() {
        return Err(Error::config("the message body is empty"));
    }
    // A single-part body is still assembled as a `multipart/mixed` when there
    // are attachments, and as a plain body when there are none. A
    // `multipart/alternative` with one part is legal but tells the recipient's
    // client nothing.
    let body = build_body(plain, html, &draft.attachments)?;
    Ok(body)
}

/// Assembles the body from text, HTML and attachments.
fn build_body(
    plain: String,
    html: String,
    attachments: &[crate::models::DraftAttachment],
) -> Result<MultiPart> {
    let attachments: Vec<SinglePart> = attachments
        .iter()
        .map(attachment_of)
        .collect::<Result<Vec<_>>>()?;
    let both = !plain.is_empty() && !html.is_empty();
    if attachments.is_empty() && !both {
        // One body form and nothing else: a plain body is enough, and it keeps
        // the message readable in clients that dislike multipart entirely.
        if html.is_empty() {
            return Ok(MultiPart::mixed().singlepart(SinglePart::plain(plain)));
        }
        return Ok(MultiPart::mixed().singlepart(SinglePart::html(html)));
    }
    let mut mixed = if both {
        MultiPart::mixed().multipart(MultiPart::alternative_plain_html(plain, html))
    } else if html.is_empty() {
        MultiPart::mixed().singlepart(SinglePart::plain(plain))
    } else {
        MultiPart::mixed().singlepart(SinglePart::html(html))
    };
    for attachment in attachments {
        mixed = mixed.singlepart(attachment);
    }
    Ok(mixed)
}

/// The plain text part, generated from the HTML when the user wrote none.
fn plain_body(draft: &Draft, context: &ComposeContext) -> String {
    let mut text = if draft.body_plain.trim().is_empty() {
        super::signature::plain_from_html(&draft.body_html)
    } else {
        draft.body_plain.clone()
    };
    if let Some(signature) = &context.signature {
        text = super::signature::insert(&text, &signature.plain, signature.position);
    }
    if let Some(reply) = &context.reply {
        text = super::reply::attach_quote(&text, reply);
    }
    text
}

/// The HTML part.
fn html_body(draft: &Draft, context: &ComposeContext) -> String {
    let mut html = if draft.body_html.trim().is_empty() {
        crate::html::plain_to_html(&draft.body_plain)
    } else {
        draft.body_html.clone()
    };
    if let Some(signature) = &context.signature {
        html = super::signature::insert(&html, &signature.html, signature.position);
    }
    if let Some(reply) = &context.reply {
        html = super::reply::attach_quote_html(&html, reply);
    }
    html
}

/// One attachment or inline part.
fn attachment_of(attachment: &crate::models::DraftAttachment) -> Result<SinglePart> {
    let data = std::fs::read(&attachment.path).map_err(|error| Error::Io {
        path: attachment.path.clone(),
        source: error,
    })?;
    let content_type = ContentType::parse(&attachment.mime_type).unwrap_or_else(|_| {
        // An unknown media type must not stop the mail: `application/octet-stream`
        // is exactly what it is.
        ContentType::parse("application/octet-stream").unwrap_or(ContentType::TEXT_PLAIN)
    });
    let part = if attachment.inline {
        let Some(content_id) = content_id_of(&attachment.filename) else {
            return Err(Error::Unsupported(format!(
                "the inline attachment `{}` cannot be referenced from the body",
                attachment.filename
            )));
        };
        Attachment::new_inline(content_id).body(data, content_type)
    } else {
        Attachment::new(attachment.filename.clone()).body(data, content_type)
    };
    Ok(part)
}

/// The `Content-ID` used for an inline image: the file name, sanitised.
///
/// The composer writes `cid:logo.png` into the HTML, so the id has to be
/// derived from the name and must be a valid `msg-id` - no spaces, no `@`, no
/// angle brackets.
fn content_id_of(filename: &str) -> Option<String> {
    let base: String = filename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let base = base.trim_matches('-');
    if base.is_empty() {
        None
    } else {
        Some(format!("{base}@hylmail"))
    }
}

/// The envelope recipients: every address of `to`, `cc` and `bcc`, deduplicated
/// case-insensitively and in that order.
///
/// Deduplication is not cosmetic: a recipient listed in `To` *and* `Cc` would
/// otherwise get the message twice, which is the kind of bug a user blames on
/// "the mail server".
#[must_use]
pub fn envelope(to: &[MailAddress], cc: &[MailAddress], bcc: &[MailAddress]) -> Vec<String> {
    let mut seen = Vec::new();
    let mut result = Vec::new();
    for address in to.iter().chain(cc.iter()).chain(bcc.iter()) {
        let email = address.email.trim();
        if email.is_empty()
            || seen
                .iter()
                .any(|known: &String| known.eq_ignore_ascii_case(email))
        {
            continue;
        }
        seen.push(email.to_string());
        result.push(email.to_string());
    }
    result
}

/// Converts an address into a `lettre` mailbox, keeping the display name.
fn mailbox(address: &MailAddress) -> Result<Mailbox> {
    let parsed = parse_address(&address.email)?;
    Ok(match &address.name {
        Some(name) if !name.trim().is_empty() => Mailbox::new(Some(name.clone()), parsed),
        _ => parsed.into(),
    })
}

/// Parses one address, naming the address that was wrong.
///
/// # Errors
///
/// [`Error::Config`] with the offending address.
pub fn parse_address(address: &str) -> Result<Address> {
    address
        .trim()
        .parse::<Address>()
        .map_err(|error| Error::config(format!("invalid address `{address}`: {error}")))
}

/// The `Content-ID` of an inline part, exposed for the composer UI so it can
/// build matching `cid:` URLs.
#[must_use]
pub fn inline_content_id(filename: &str) -> Option<String> {
    content_id_of(filename)
}

/// Details that must never reach a log, used by the outbox report.
impl fmt::Debug for ComposedMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComposedMessage")
            .field("recipients", &self.recipients.len())
            .field("bytes", &self.raw.len())
            .field("message_id", &self.message_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AccountId, DraftAttachment};

    struct Fixed;
    impl MessageIdGenerator for Fixed {
        fn generate(&self, from: &str, _subject: &str, nonce: u64) -> String {
            let domain = from.rsplit_once('@').map(|(_, d)| d).unwrap_or("x");
            format!("<fixed-{nonce}@{domain}>")
        }
    }

    fn context() -> ComposeContext {
        ComposeContext::from_address(MailAddress::with_name("ada@example.org", "Ada"))
    }

    fn draft() -> Draft {
        let mut draft = Draft::new(AccountId::new());
        draft.to = vec![MailAddress::with_name("bob@example.org", "Bob")];
        draft.cc = vec![MailAddress::new("cara@example.org")];
        draft.subject = "Grüße".to_string();
        draft.body_html = "<p>Hallo</p>".to_string();
        draft
    }

    #[test]
    fn a_simple_message_is_multipart_alternative() {
        let composed = build_with(&draft(), &context(), &Fixed, 1).unwrap();
        let raw = composed.as_str().unwrap();
        assert!(raw.contains("multipart/alternative"), "{raw}");
        assert!(raw.contains("text/plain"));
        assert!(raw.contains("text/html"));
        assert!(raw.contains("Hallo"));
        assert_eq!(
            composed.recipients,
            vec!["bob@example.org", "cara@example.org"]
        );
        assert_eq!(composed.message_id, "<fixed-1@example.org>");
        assert_eq!(composed.from, "ada@example.org");
    }

    #[test]
    fn bcc_never_appears_in_the_headers() {
        let mut draft = draft();
        draft.bcc = vec![MailAddress::new("geheim@example.org")];
        let composed = build(&draft, &context()).unwrap();
        let raw = composed.as_str().unwrap();
        assert!(!raw.to_ascii_lowercase().contains("geheim@example.org"));
        assert!(!raw.to_ascii_lowercase().contains("\nbcc:"));
        // ...but it *is* in the envelope, which is the only place it belongs.
        assert!(composed
            .recipients
            .contains(&"geheim@example.org".to_string()));
        assert_eq!(composed.recipients.len(), 3);
    }

    #[test]
    fn a_recipient_listed_twice_gets_one_copy() {
        let mut draft = draft();
        draft.cc = vec![MailAddress::new("BOB@example.org")];
        draft.bcc = vec![MailAddress::new("bob@example.org")];
        let composed = build(&draft, &context()).unwrap();
        assert_eq!(composed.recipients, vec!["bob@example.org"]);
    }

    #[test]
    fn the_subject_is_encoded_not_mangled() {
        let composed = build(&draft(), &context()).unwrap();
        let raw = composed.as_str().unwrap();
        // RFC 2047 encoded word for "Grüße".
        assert!(raw.contains("Subject: =?utf-8?"));
        assert!(raw.contains("Gr=C3=BC=C3=9Fe") || raw.contains("=?utf-8?b?"));
    }

    #[test]
    fn display_names_survive() {
        let composed = build(&draft(), &context()).unwrap();
        let raw = composed.as_str().unwrap();
        assert!(raw.contains("Bob"));
        assert!(raw.contains("<bob@example.org>"));
    }

    #[test]
    fn reply_headers_are_carried() {
        let mut draft = draft();
        draft.subject = "Re: Grüße".to_string();
        let context = context().in_reply_to(super::super::ReplyContext::new(
            Some("parent@example.org".to_string()),
            vec![
                "root@example.org".to_string(),
                "parent@example.org".to_string(),
            ],
        ));
        let composed = build(&draft, &context).unwrap();
        let raw = composed.as_str().unwrap();
        // `lettre` writes the id without the angle brackets; the RFC allows
        // either, and every client puts them back when it parses.
        assert!(raw.contains("In-Reply-To: parent@example.org"), "{raw}");
        // `lettre` prints the chain with the angle brackets it removed from
        // the id when it parsed them.
        assert!(
            raw.contains("References: <root@example.org> <parent@example.org>"),
            "{raw}"
        );
    }

    #[test]
    fn an_empty_draft_is_rejected() {
        let mut empty = draft();
        empty.to.clear();
        assert!(build(&empty, &context()).is_err());
        let mut bodyless = draft();
        bodyless.body_html.clear();
        assert!(build(&bodyless, &context()).is_err());
        assert!(build(&draft(), &ComposeContext::default()).is_err());
    }

    #[test]
    fn an_attachment_is_added_as_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bericht.pdf");
        std::fs::write(&path, b"%PDF-1.4").unwrap();
        let mut draft = draft();
        draft.attachments = vec![DraftAttachment {
            filename: "Bericht 2026.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            path,
            size_bytes: 8,
            inline: false,
        }];
        let composed = build(&draft, &context()).unwrap();
        let raw = composed.as_str().unwrap();
        assert!(raw.contains("multipart/mixed"));
        assert!(raw.contains("application/pdf"));
        assert!(raw.contains("Bericht 2026.pdf"));
        assert!(raw.contains("Content-Disposition: attachment"));
    }

    #[test]
    fn an_inline_image_gets_a_content_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("logo.png");
        std::fs::write(&path, b"\x89PNG").unwrap();
        let mut draft = draft();
        draft.body_html = "<p>x</p><img src=\"cid:logo.png@hylmail\">".to_string();
        draft.attachments = vec![DraftAttachment {
            filename: "logo.png".to_string(),
            mime_type: "image/png".to_string(),
            path,
            size_bytes: 4,
            inline: true,
        }];
        let composed = build(&draft, &context()).unwrap();
        let raw = composed.as_str().unwrap();
        assert!(raw.contains("Content-ID: <logo.png@hylmail>"));
        assert!(raw.contains("Content-Disposition: inline"));
        assert_eq!(
            inline_content_id("logo.png").as_deref(),
            Some("logo.png@hylmail")
        );
        assert_eq!(inline_content_id("!!!").as_deref(), None);
    }

    #[test]
    fn a_missing_attachment_file_is_an_error() {
        let mut draft = draft();
        draft.attachments = vec![DraftAttachment {
            filename: "weg.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            path: std::path::PathBuf::from("does/not/exist.pdf"),
            size_bytes: 0,
            inline: false,
        }];
        assert!(matches!(build(&draft, &context()), Err(Error::Io { .. })));
    }

    #[test]
    fn the_signature_lands_in_both_parts() {
        let signature = crate::models::Signature {
            id: crate::models::ids::SignatureId::new(),
            account_id: AccountId::new(),
            name: "Standard".to_string(),
            html: "<p>-- <br>Ada</p>".to_string(),
            plain: "-- \nAda".to_string(),
            position: crate::models::SignaturePosition::AboveQuote,
            is_default: true,
        };
        let composed = build(&draft(), &context().with_signature(signature)).unwrap();
        let raw = composed.as_str().unwrap();
        assert!(raw.contains("Ada"));
        assert!(raw.contains("-- "));
    }

    #[test]
    fn the_plain_text_is_generated_from_the_html() {
        let mut draft = draft();
        draft.body_plain.clear();
        let composed = build(&draft, &context()).unwrap();
        let raw = composed.as_str().unwrap();
        assert!(raw.contains("Hallo"));
        assert!(raw.contains("text/plain"));
    }

    #[test]
    fn a_plain_text_only_draft_is_fine() {
        let mut draft = draft();
        draft.body_html.clear();
        draft.body_plain = "Nur Text".to_string();
        let composed = build(&draft, &context()).unwrap();
        assert!(composed.as_str().unwrap().contains("Nur Text"));
    }

    #[test]
    fn the_generated_message_id_carries_the_sender_domain() {
        let id = TimestampMessageId.generate("ada@example.org", "Betreff", 0);
        assert!(id.contains("@example.org"), "{id}");
        let id = TimestampMessageId.generate("kaputt", "Betreff", 0);
        assert!(id.contains("@hylmail.local"), "{id}");
    }

    #[test]
    fn a_debug_dump_does_not_contain_the_message() {
        let composed = build(&draft(), &context()).unwrap();
        let debug = format!("{composed:?}");
        assert!(!debug.contains("Hallo"));
        assert!(debug.contains("bytes"));
    }

    #[test]
    fn the_envelope_helper_deduplicates() {
        let to = vec![MailAddress::new("a@x"), MailAddress::new("A@x")];
        let cc = vec![MailAddress::new("b@x")];
        let bcc = vec![MailAddress::new("")];
        assert_eq!(envelope(&to, &cc, &bcc), vec!["a@x", "b@x"]);
    }
}
