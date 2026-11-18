//! The MIME pipeline: from raw bytes to a renderable message.
//!
//! [`parse`] is the single entry point every account kind goes through, whether
//! the bytes came from `UID FETCH BODY[]`, an EWS `GetItem` with
//! `IncludeMimeContent` or a GroupWise `getItemRequest`.  It produces exactly
//! what the reading pane and the content cache need:
//!
//! | Output | Source |
//! |---|---|
//! | plain text | the author's `text/plain` part, else rendered from HTML |
//! | raw HTML | the author's `text/html` part, untouched |
//! | sanitised HTML | [`crate::html::sanitize`] with the user's policy |
//! | attachments | every part that is offered as a download |
//! | calendar | the first `text/calendar` part, parsed |
//! | snippet | one line for the message list |
//!
//! Nothing is decoded by hand: `mail-parser` handles transfer encodings
//! (base64, quoted-printable), charsets and RFC 2047/2231 encoded file names,
//! which is where hand-written mail clients usually get it wrong.
//!
//! ## Guarantees
//!
//! * **A hostile message cannot hurt the process.** Parsing never panics, and
//!   a part that cannot be decoded is skipped rather than replaced with
//!   garbage.
//! * **HTML never reaches the UI unsanitised.** [`parse`] always runs the
//!   sanitiser; only the *raw* HTML is stored next to it, and that field is
//!   documented as untrusted.
//! * **Every error carries a reason, never a body.** Parse failures name the
//!   protocol ("MIME") and the problem, not the mail.

mod parse;

pub use parse::{
    extract_attachments, parse, parse_headers, store_body, AttachmentPayload, ParsedHeaders,
    ParsedMessage, CALENDAR_FALLBACK_NAME,
};

use crate::models::MessageBody;

impl crate::models::MessageBody {
    /// `true` when the body carries a calendar invitation.
    #[must_use]
    pub fn is_invitation(&self) -> bool {
        self.calendar_ics.is_some()
    }
}

/// Convenience: the plain text of a body, or an empty string.
#[must_use]
pub fn plain_text_or_empty(body: &MessageBody) -> &str {
    body.plain_text.as_deref().unwrap_or_default()
}
