//! Quoting and prefixing a reply or a forward.
//!
//! German mail culture quotes with `>` and prefixes with `Re:` / `AW:` /
//! `WG:` / `Fwd:`. This module keeps that local convention in one place so the
//! GUI, the CLI and the composer agree.

use crate::models::{MailAddress, MessageHeader};

/// German date format, as the rest of the program uses it.
const DATE_FORMAT: &str = "%d.%m.%Y um %H:%M";

/// Prefix of a reply, as a German client would write it.
#[must_use]
pub fn reply_prefix() -> &'static str {
    "Re: "
}

/// Prefix of a forward.
#[must_use]
pub fn forward_prefix() -> &'static str {
    "WG: "
}

/// The headers that tie a reply to the message it answers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplyContext {
    /// `Message-ID` of the parent.
    pub in_reply_to: Option<String>,
    /// `References` chain, oldest first.
    pub references: Vec<String>,
    /// The quoted original message, as plain text.
    pub quoted_plain: Option<String>,
    /// The quoted original message, as HTML.
    pub quoted_html: Option<String>,
    /// Who wrote the original, for the attribution line.
    pub author: Option<MailAddress>,
    /// When the original was sent.
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    /// Subject of the original, for the forwarding header block.
    pub subject: Option<String>,
}

impl ReplyContext {
    /// The headers only, for a reply whose body the user has already written.
    #[must_use]
    pub fn new(in_reply_to: Option<String>, references: Vec<String>) -> Self {
        Self {
            in_reply_to,
            references,
            ..Self::default()
        }
    }

    /// Everything needed for a reply with a quote, taken from the original.
    #[must_use]
    pub fn from_message(header: &MessageHeader, body: Option<&str>, html: Option<&str>) -> Self {
        let mut references = header.references.clone();
        if let Some(parent) = &header.message_id {
            if !references.iter().any(|known| known == parent) {
                references.push(parent.clone());
            }
        }
        Self {
            in_reply_to: header.message_id.clone(),
            references,
            quoted_plain: body.map(str::to_string),
            quoted_html: html.map(str::to_string),
            author: header.from.first().cloned(),
            date: header.date.or(header.received_at),
            subject: Some(header.subject.clone()),
        }
    }

    /// `true` when the context has a body to quote.
    #[must_use]
    pub fn has_quote(&self) -> bool {
        self.quoted_plain.is_some() || self.quoted_html.is_some()
    }

    /// The attribution line: "Am 12.11.2026 um 13:00 schrieb Ada Lovelace:".
    #[must_use]
    pub fn attribution(&self) -> String {
        let when = self
            .date
            .map(|date| date.format(DATE_FORMAT).to_string())
            .unwrap_or_else(|| "unbekanntem Datum".to_string());
        let who = self
            .author
            .as_ref()
            .map(|address| match &address.name {
                Some(name) if !name.trim().is_empty() => {
                    format!("{name} <{}>", address.email)
                }
                _ => address.email.clone(),
            })
            .unwrap_or_else(|| "unbekanntem Absender".to_string());
        format!("Am {when} schrieb {who}:")
    }
}

/// Appends a quote to a plain text body.
#[must_use]
pub fn attach_quote(text: &str, context: &ReplyContext) -> String {
    let Some(original) = context.quoted_plain.as_deref() else {
        return text.to_string();
    };
    let quoted = quote_original(original, &context.attribution());
    if text.trim().is_empty() {
        return quoted;
    }
    format!("{}\n\n{}", text.trim_end(), quoted)
}

/// Appends a quote to an HTML body.
///
/// The original is escaped before it is quoted: a message that contains `<` or
/// `&` must not become markup just because it is being answered.
#[must_use]
pub fn attach_quote_html(html: &str, context: &ReplyContext) -> String {
    let original = context
        .quoted_html
        .as_deref()
        .map(str::to_string)
        .or_else(|| {
            context
                .quoted_plain
                .as_deref()
                .map(crate::html::plain_to_html)
        });
    let Some(original) = original else {
        return html.to_string();
    };
    let block = format!(
        "<p class=\"hylmail-quote\">{}</p>\n<blockquote type=\"cite\" class=\"hylmail-quote\">{}</blockquote>",
        crate::html::escape(&context.attribution()),
        original
    );
    if html.trim().is_empty() {
        return block;
    }
    format!("{}\n{}", html.trim_end(), block)
}

/// Quotes a plain text message the German way: `> ` on every line, with an
/// attribution line first.
///
/// The result has no trailing newline, so it can be appended to a body without
/// leaving a blank line behind.
#[must_use]
pub fn quote_original(original: &str, attribution: &str) -> String {
    let mut out = String::with_capacity(original.len() + attribution.len() + 16);
    out.push_str(attribution);
    for line in original.trim_end_matches('\n').lines() {
        // A line that is itself quoted gets one more level.
        out.push('\n');
        out.push_str("> ");
        out.push_str(line);
    }
    out
}

/// Prefixes a subject for a reply, without stacking `Re: Re:`.
#[must_use]
pub fn reply_subject(subject: &str) -> String {
    prefix_subject(subject, reply_prefix())
}

/// Prefixes a subject for a forward, without stacking `WG: WG:`.
#[must_use]
pub fn forward_subject(subject: &str) -> String {
    prefix_subject(subject, forward_prefix())
}

/// Adds `prefix` unless the subject already has it (case-insensitively).
#[must_use]
pub fn prefix_subject(subject: &str, prefix: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.is_empty() {
        return prefix.trim_end().to_string();
    }
    let base = prefix.trim_end_matches(' ');
    // Already prefixed means: the subject starts with the prefix *and* what
    // follows is not the rest of a word. "Re: Frage", "Re:Frage" and a bare
    // "Re:" are prefixed; "Rechnung" is not, even though it starts with "Re".
    let lowered = trimmed.to_ascii_lowercase();
    let base_lower = base.to_ascii_lowercase();
    if lowered.starts_with(&base_lower) {
        let rest = &trimmed[base.len()..];
        // The colon may already be inside `base` ("Re:") or come right after
        // it ("Re" + ":"); either way, the character at the prefix's last
        // position decides whether this is a prefix or the start of a word.
        let boundary = lowered.as_bytes().get(base_lower.len().saturating_sub(1)) == Some(&b':');
        if rest.is_empty() || rest.starts_with(char::is_whitespace) || boundary {
            return trimmed.to_string();
        }
    }
    format!("{prefix}{trimmed}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AccountId, FolderId};

    #[test]
    fn quoting_indents_every_line() {
        let quoted = quote_original("erste\nzweite\n> schon zitiert", "Am heute schrieb X:");
        assert_eq!(
            quoted,
            "Am heute schrieb X:\n> erste\n> zweite\n> > schon zitiert"
        );
    }

    #[test]
    fn quoting_a_trailing_newline_does_not_add_a_blank_quote() {
        let quoted = quote_original("nur eine\n", "A:");
        assert_eq!(quoted, "A:\n> nur eine");
    }

    #[test]
    fn an_empty_body_is_not_quoted() {
        let context = ReplyContext::new(Some("x".to_string()), Vec::new());
        assert_eq!(attach_quote("Mein Text", &context), "Mein Text");
        assert_eq!(attach_quote_html("<p>x</p>", &context), "<p>x</p>");
    }

    #[test]
    fn a_reply_appends_the_quote_after_the_new_text() {
        let context = ReplyContext {
            in_reply_to: Some("parent@example.org".to_string()),
            quoted_plain: Some("alter Text".to_string()),
            author: Some(MailAddress::with_name("ada@example.org", "Ada")),
            date: Some(
                chrono::DateTime::parse_from_rfc3339("2026-11-12T13:00:05+01:00")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ),
            ..ReplyContext::default()
        };
        let text = attach_quote("Meine Antwort", &context);
        assert!(text
            .starts_with("Meine Antwort\n\nAm 12.11.2026 um 12:00 schrieb Ada <ada@example.org>:"));
        assert!(text.ends_with("> alter Text"));
    }

    #[test]
    fn an_html_quote_escapes_the_original() {
        let context = ReplyContext {
            quoted_plain: Some("<script>boese()</script>".to_string()),
            ..ReplyContext::default()
        };
        let html = attach_quote_html("<p>Antwort</p>", &context);
        assert!(html.contains("<blockquote"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn the_attribution_falls_back_to_a_placeholder() {
        let context = ReplyContext::default();
        let line = context.attribution();
        assert!(line.contains("unbekanntem Datum"));
        assert!(line.contains("unbekanntem Absender"));
    }

    #[test]
    fn a_reply_context_is_built_from_the_parent() {
        let mut header = MessageHeader::new(AccountId::new(), FolderId::new());
        header.message_id = Some("parent@example.org".to_string());
        header.references = vec!["root@example.org".to_string()];
        header.from = vec![MailAddress::new("ada@example.org")];
        header.subject = "Frage".to_string();
        let context = ReplyContext::from_message(&header, Some("Text"), None);
        assert_eq!(context.in_reply_to.as_deref(), Some("parent@example.org"));
        assert_eq!(
            context.references,
            vec!["root@example.org", "parent@example.org"]
        );
        assert!(context.has_quote());
    }

    #[test]
    fn subject_prefixes_do_not_stack() {
        assert_eq!(reply_subject("Frage"), "Re: Frage");
        assert_eq!(reply_subject("Re: Frage"), "Re: Frage");
        assert_eq!(reply_subject("RE: Frage"), "RE: Frage");
        assert_eq!(reply_subject("Re:Frage"), "Re:Frage");
        assert_eq!(forward_subject("Frage"), "WG: Frage");
        assert_eq!(forward_subject("WG: Frage"), "WG: Frage");
        assert_eq!(reply_subject("  Frage  "), "Re: Frage");
        assert_eq!(reply_subject(""), "Re:");
    }
}
