//! German formatting helpers shared by every pane.
//!
//! All user visible text of the interface is German (see the project
//! specification), so dates, sizes and address lists have exactly one
//! implementation instead of one per widget.

use chrono::{DateTime, Datelike, Local, Utc};

use hylmail_core::models::MailAddress;

/// Splits a free text address field into [`MailAddress`] values.
///
/// Accepts the forms a user actually types:
///
/// * `anna@example.org`
/// * `Anna Beispiel <anna@example.org>`
/// * `"Beispiel, Anna" <anna@example.org>, bob@example.org`
///
/// Splits a free text address field into the tokens a user typed.
///
/// Accepts the forms a user actually types:
///
/// * `anna@example.org`
/// * `Anna Beispiel <anna@example.org>`
/// * `"Beispiel, Anna" <anna@example.org>, bob@example.org`
///
/// A comma or semicolon inside quotes or angle brackets does not split, so a
/// display name that contains one survives.  Parsing the tokens into
/// addresses is [`crate::state::parse_recipient`]'s job.
#[must_use]
pub(crate) fn split_address_tokens(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_angles = false;

    for character in raw.chars() {
        match character {
            '"' => {
                in_quotes = !in_quotes;
                current.push(character);
            }
            '<' if !in_quotes => {
                in_angles = true;
                current.push(character);
            }
            '>' if !in_quotes => {
                in_angles = false;
                current.push(character);
            }
            ',' | ';' if !in_quotes && !in_angles => {
                push_token(&mut tokens, &current);
                current.clear();
            }
            _ => current.push(character),
        }
    }
    push_token(&mut tokens, &current);
    tokens
}

/// Appends one trimmed, non-empty token.
fn push_token(tokens: &mut Vec<String>, token: &str) {
    let token = token.trim();
    if !token.is_empty() {
        tokens.push(token.to_string());
    }
}

/// Renders the display name of an address, falling back to the email.
#[must_use]
pub(crate) fn address_label(address: &MailAddress) -> String {
    match &address.name {
        Some(name) if !name.trim().is_empty() => name.clone(),
        _ => address.email.clone(),
    }
}

/// Joins an address list for a list column, using display names.
#[must_use]
pub(crate) fn addresses_label(addresses: &[MailAddress]) -> String {
    addresses
        .iter()
        .map(address_label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders the send timestamp the way a German mail client does.
#[must_use]
pub(crate) fn format_datetime(when: Option<DateTime<Utc>>) -> String {
    let Some(when) = when else {
        return "(kein Datum)".to_string();
    };
    let local = when.with_timezone(&Local);
    let today = Local::now().date_naive();
    let day = local.date_naive();
    if day == today {
        format!("Heute {}", local.format("%H:%M"))
    } else if Some(day) == today.pred_opt() {
        format!("Gestern {}", local.format("%H:%M"))
    } else if day.year() == today.year() {
        local.format("%d.%m. %H:%M").to_string()
    } else {
        local.format("%d.%m.%Y %H:%M").to_string()
    }
}

/// Renders a byte count with a German decimal comma.
#[must_use]
pub(crate) fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value = bytes as f64;
    let (scaled, unit) = if value < KIB {
        return format!("{bytes} B");
    } else if value < MIB {
        (value / KIB, "KB")
    } else if value < GIB {
        (value / MIB, "MB")
    } else {
        (value / GIB, "GB")
    };
    let text = format!("{scaled:.1}");
    format!("{} {unit}", text.replace('.', ","))
}

/// Shortens `value` to at most `max` characters, appending an ellipsis.
#[must_use]
pub(crate) fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `"3 Nachrichten"` / `"1 Nachricht"`.
#[must_use]
pub(crate) fn plural(count: u64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_address_is_one_token() {
        let tokens = split_address_tokens("anna@example.org");
        assert_eq!(tokens, vec!["anna@example.org"]);
    }

    #[test]
    fn a_named_address_stays_in_one_piece() {
        let tokens = split_address_tokens("Anna Beispiel <anna@example.org>");
        assert_eq!(tokens, vec!["Anna Beispiel <anna@example.org>"]);
    }

    #[test]
    fn a_quoted_comma_does_not_split_the_list() {
        let tokens =
            split_address_tokens(r#""Beispiel, Anna" <anna@example.org>, bob@example.org"#);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], r#""Beispiel, Anna" <anna@example.org>"#);
        assert_eq!(tokens[1], "bob@example.org");
    }

    #[test]
    fn semicolons_separate_too() {
        assert_eq!(split_address_tokens("a@x.org; b@y.org").len(), 2);
    }

    #[test]
    fn an_empty_field_yields_no_tokens() {
        assert!(split_address_tokens("   ").is_empty());
        assert!(split_address_tokens("").is_empty());
        assert!(split_address_tokens(", ,").is_empty());
    }

    #[test]
    fn a_label_prefers_the_display_name() {
        let named = MailAddress::with_name("a@b.org", "Anna");
        assert_eq!(address_label(&named), "Anna");
        assert_eq!(address_label(&MailAddress::new("a@b.org")), "a@b.org");
    }

    #[test]
    fn sizes_use_binary_units_and_a_comma() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2,0 KB");
        assert_eq!(format_size(1024 * 1024 * 3 / 2), "1,5 MB");
        assert_eq!(format_size(1024 * 1024 * 1024 * 2), "2,0 GB");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("Grüße", 10), "Grüße");
        assert_eq!(truncate("Grüße", 3), "Gr…");
        assert_eq!(truncate("abc", 1), "…");
    }

    #[test]
    fn plurals_are_german() {
        assert_eq!(plural(1, "Nachricht", "Nachrichten"), "1 Nachricht");
        assert_eq!(plural(0, "Nachricht", "Nachrichten"), "0 Nachrichten");
        assert_eq!(plural(7, "Nachricht", "Nachrichten"), "7 Nachrichten");
    }

    #[test]
    fn a_missing_date_is_spelled_out() {
        assert_eq!(format_datetime(None), "(kein Datum)");
    }
}
