//! The composer: turning a [`Draft`] into the bytes that go on the wire.
//!
//! The composer owns one decision that is worth stating plainly: **the message
//! that leaves this program is a `multipart/alternative` message**, never an
//! HTML-only one. Every draft therefore has both a `text/html` part and a
//! `text/plain` part, and the plain text is generated from the HTML when the
//! user did not write one. A mail client that sends HTML only is a mail client
//! that breaks the recipients it claims to serve.
//!
//! Layout of what is emitted:
//!
//! ```text
//! multipart/mixed
//! ├── multipart/alternative
//! │   ├── text/plain
//! │   └── text/html
//! ├── application/pdf          (attachment)
//! └── image/png                (inline, Content-ID set)
//! ```
//!
//! The `multipart/alternative` wrapper is left out when there is only one body
//! form to send - a `multipart/alternative` with a single part is legal but
//! pointless.
//!
//! ## The Bcc rule
//!
//! `Bcc` is used to build the envelope but **never appears in the headers** of
//! the emitted message. See [`builder::envelope`]: the recipients of the
//! SMTP conversation are taken from `To`+`Cc`+`Bcc`, and the header block is
//! built from `To`+`Cc` only. A Bcc that leaked into a header would be visible
//! to every recipient, which is the classic way a mail client betrays its user.

mod builder;
mod reply;
mod signature;

pub use builder::{
    build, build_headers_only, build_with, envelope, inline_content_id, parse_address,
    ComposedMessage, MessageIdGenerator, TimestampMessageId,
};
pub use reply::{
    attach_quote, attach_quote_html, forward_prefix, forward_subject, prefix_subject,
    quote_original, reply_prefix, reply_subject, ReplyContext,
};
pub use signature::{contains as has_signature, insert as apply_signature, strip_signature};

use crate::models::Draft;

/// Everything the composer needs besides the draft itself.
#[derive(Debug, Clone, Default)]
pub struct ComposeContext {
    /// The address the message is sent from, with display name.
    pub from: Option<crate::models::MailAddress>,
    /// The signature to insert, if the user picked one.
    pub signature: Option<crate::models::Signature>,
    /// Overrides `from` in the `Sender:` header, for a shared mailbox.
    pub sender: Option<String>,
    /// Date to stamp into the `Date:` header; defaults to "now".
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    /// The `Message-ID` to use; a fresh one is generated when absent.
    pub message_id: Option<String>,
    /// Reply headers of the message being answered.
    pub reply: Option<ReplyContext>,
}

impl ComposeContext {
    /// Creates a context that sends from `from`.
    #[must_use]
    pub fn from_address(from: crate::models::MailAddress) -> Self {
        Self {
            from: Some(from),
            ..Self::default()
        }
    }

    /// Adds a signature.
    #[must_use]
    pub fn with_signature(mut self, signature: crate::models::Signature) -> Self {
        self.signature = Some(signature);
        self
    }

    /// Adds the reply headers.
    #[must_use]
    pub fn in_reply_to(mut self, reply: ReplyContext) -> Self {
        self.reply = Some(reply);
        self
    }
}

/// `true` when the draft is complete enough to hand to the SMTP server.
///
/// The check is deliberately local: it does not ask whether the account is
/// enabled or the server is reachable, only whether the message itself makes
/// sense. [`crate::models::Draft::validate`] covers the rest.
#[must_use]
pub fn is_sendable(draft: &Draft) -> bool {
    !draft.to.is_empty()
        && (!draft.body_html.trim().is_empty() || !draft.body_plain.trim().is_empty())
}

/// A short German description of why a draft cannot be sent.
#[must_use]
pub fn send_blocker(draft: &Draft) -> Option<&'static str> {
    if draft.to.is_empty() {
        return Some("Kein Empfänger angegeben");
    }
    if draft.body_html.trim().is_empty() && draft.body_plain.trim().is_empty() {
        return Some("Der Nachrichtentext ist leer");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AccountId, Draft, MailAddress};

    fn draft() -> Draft {
        let mut draft = Draft::new(AccountId::new());
        draft.to = vec![MailAddress::new("bob@example.org")];
        draft.subject = "Hallo".to_string();
        draft.body_html = "<p>Hallo Bob</p>".to_string();
        draft
    }

    #[test]
    fn a_complete_draft_is_sendable() {
        let draft = draft();
        assert!(is_sendable(&draft));
        assert_eq!(send_blocker(&draft), None);
    }

    #[test]
    fn an_empty_recipient_list_blocks_sending() {
        let mut draft = draft();
        draft.to.clear();
        assert!(!is_sendable(&draft));
        assert_eq!(send_blocker(&draft), Some("Kein Empfänger angegeben"));
    }

    #[test]
    fn an_empty_body_blocks_sending() {
        let mut draft = draft();
        draft.body_html.clear();
        assert!(!is_sendable(&draft));
        assert_eq!(send_blocker(&draft), Some("Der Nachrichtentext ist leer"));
        // A draft with only a plain text body is fine.
        draft.body_plain = "Nur Text".to_string();
        assert!(is_sendable(&draft));
    }
}
