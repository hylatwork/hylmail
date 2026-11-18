//! Placing a signature in a body.
//!
//! The signature is a per-account document (see
//! [`crate::models::Signature`]) with a position: above the quote for top
//! posting, below it for the classic German reply style. "Above the quote"
//! means: directly after the user's text and *before* the quoted original -
//! which is exactly where a signature belongs, and the opposite of what happens
//! when a client simply appends it to the end of the buffer.

use crate::models::Signature;
use crate::models::SignaturePosition;

/// The conventional signature separator: dash, dash, space.
pub const SEPARATOR: &str = "-- ";

/// Inserts a signature at the requested position.
///
/// * [`SignaturePosition::AboveQuote`]: between the new text and the quote.
/// * [`SignaturePosition::BelowQuote`]: at the very end.
///
/// The quote is recognised by the markers this module's pager writes:
/// `"Am … schrieb …:"` in plain text and `class="hylmail-quote"` in HTML. That
/// is intentionally the *only* thing that counts as a quote, so a body that
/// merely mentions the word "Am" is not cut in half.
#[must_use]
pub fn insert(body: &str, signature: &str, position: SignaturePosition) -> String {
    let signature = signature.trim();
    if signature.is_empty() {
        return body.to_string();
    }
    match position {
        SignaturePosition::BelowQuote => {
            if body.trim().is_empty() {
                return signature.to_string();
            }
            format!("{}\n{}", body.trim_end(), signature)
        }
        SignaturePosition::AboveQuote => {
            let Some(index) = quote_start(body) else {
                if body.trim().is_empty() {
                    return signature.to_string();
                }
                return format!("{}\n{}", body.trim_end(), signature);
            };
            let (before, after) = body.split_at(index);
            format!(
                "{}\n{}\n\n{}",
                before.trim_end(),
                signature,
                after.trim_start()
            )
        }
    }
}

/// `true` when the body already carries this signature.
#[must_use]
pub fn contains(body: &str, signature: &Signature) -> bool {
    let marker = signature.plain.trim();
    !marker.is_empty() && body.contains(marker)
}

/// Removes a trailing signature and its separator.
#[must_use]
pub fn strip_signature(body: &str) -> String {
    let mut lines: Vec<&str> = body.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() >= 2 && lines[lines.len() - 2].trim_end() == SEPARATOR.trim_end() {
        lines.truncate(lines.len() - 2);
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        return lines.join("\n");
    }
    body.trim_end().to_string()
}

/// Renders a plain text body from HTML, without a signature.
///
/// A thin wrapper around [`crate::html::html_to_text`] so the composer has one
/// place to call.
#[must_use]
pub fn plain_from_html(html: &str) -> String {
    let text = crate::html::html_to_text(html);
    if !text.trim().is_empty() {
        return text;
    }
    // The HTML had no text at all (an image-only newsletter, say). A message
    // without any text part is unpleasant, and an empty one looks like a bug,
    // so the picture and link inventory stands in for the missing text.
    let links = crate::html::link_count(html);
    let images = crate::html::image_count(html);
    match (links, images) {
        (0, 0) => String::new(),
        (0, images) => format!("Diese Nachricht enthält {images} Bild(er) und sonst keinen Text."),
        (links, 0) => {
            format!("Diese Nachricht enthält {links} Verweis(e) und sonst keinen Text.")
        }
        (links, images) => format!(
            "Diese Nachricht enthält {links} Verweis(e) und {images} Bild(er) und sonst keinen Text."
        ),
    }
}

/// The byte offset where the quoted original begins, if there is one.
fn quote_start(body: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        if is_quote_line(line) {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// `true` when a line starts a quote, in either body form.
fn is_quote_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if is_attribution(trimmed.trim_end()) {
        return true;
    }
    // The HTML block is written by this crate, but a body may have been
    // edited by a user or come from a template, so the class name is matched
    // case-insensitively and the tag may carry other attributes.
    let lowered = trimmed.to_ascii_lowercase();
    lowered.starts_with("<blockquote")
        || lowered.contains("class=\"hylmail-quote\"")
        || lowered.contains("class='hylmail-quote'")
        || lowered.contains("class=hylmail-quote")
}

/// `true` for an attribution line: "Am … schrieb …:".
fn is_attribution(line: &str) -> bool {
    let line = line.trim_end();
    let Some(rest) = line.strip_prefix("Am ") else {
        return false;
    };
    rest.contains(" schrieb ") && line.ends_with(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ids::{AccountId, SignatureId};

    fn signature(plain: &str, html: &str) -> Signature {
        Signature {
            id: SignatureId::new(),
            account_id: AccountId::new(),
            name: "Standard".to_string(),
            html: html.to_string(),
            plain: plain.to_string(),
            position: SignaturePosition::AboveQuote,
            is_default: true,
        }
    }

    const BODY: &str =
        "Meine Antwort\n\nAm 12.11.2026 um 12:00 schrieb Ada <ada@example.org>:\n> Frage";

    #[test]
    fn above_the_quote_lands_between_text_and_original() {
        let result = insert(BODY, "-- \nAda", SignaturePosition::AboveQuote);
        let signature_at = result.find("Ada").unwrap();
        let quote_at = result.find("> Frage").unwrap();
        let answer_at = result.find("Meine Antwort").unwrap();
        assert!(answer_at < signature_at);
        assert!(signature_at < quote_at);
    }

    #[test]
    fn below_the_quote_lands_at_the_end() {
        let result = insert(BODY, "-- \nAda", SignaturePosition::BelowQuote);
        assert!(result.ends_with("-- \nAda"));
    }

    #[test]
    fn a_body_without_a_quote_gets_the_signature_at_the_end() {
        let result = insert("Nur Text", "-- \nAda", SignaturePosition::AboveQuote);
        assert_eq!(result, "Nur Text\n-- \nAda");
    }

    #[test]
    fn an_empty_signature_changes_nothing() {
        assert_eq!(insert(BODY, "   ", SignaturePosition::AboveQuote), BODY);
    }

    #[test]
    fn an_empty_body_becomes_the_signature() {
        assert_eq!(
            insert("", "-- \nAda", SignaturePosition::AboveQuote),
            "-- \nAda"
        );
    }

    #[test]
    fn a_mention_of_am_does_not_count_as_a_quote() {
        let body = "Am Montag habe ich Zeit\nWeiterer Text";
        let result = insert(body, "SIG", SignaturePosition::AboveQuote);
        assert!(result.ends_with("SIG"));
        assert!(result.contains("Weiterer Text"));
    }

    #[test]
    fn a_recognised_signature_is_detected() {
        let sig = signature("-- \nAda", "<p>Ada</p>");
        assert!(contains("Text\n-- \nAda", &sig));
        assert!(!contains("Text", &sig));
        let empty = signature("", "");
        assert!(!contains("Text", &empty));
    }

    #[test]
    fn a_trailing_signature_can_be_removed() {
        assert_eq!(strip_signature("Text\n\n-- \nAda"), "Text");
        assert_eq!(strip_signature("Text"), "Text");
        assert_eq!(strip_signature("-- \nAda"), "");
    }

    #[test]
    fn an_html_quote_is_recognised_for_placement() {
        let body = "<p>Antwort</p>\n<p class=\"hylmail-quote\">Zitat</p>";
        let result = insert(body, "SIG", SignaturePosition::AboveQuote);
        assert!(result.find("SIG").unwrap() < result.find("Zitat").unwrap());
    }

    #[test]
    fn plain_text_is_generated_from_html() {
        assert_eq!(plain_from_html("<p>Hallo <b>Welt</b></p>"), "Hallo Welt");
        assert!(plain_from_html("<img src=\"x.png\">").contains("Bild"));
        assert_eq!(plain_from_html("<p>&nbsp;</p>"), "");
        assert_eq!(plain_from_html(""), "");
    }
}
