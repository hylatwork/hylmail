//! Turning `ENVELOPE` data into the strings a user sees.
//!
//! IMAP delivers header material as raw bytes and *does not* decode encoded
//! words: a subject arrives either as UTF-8 (modern servers) or as
//! `=?UTF-8?Q?Rechnung_2024?=`.  Displaying the latter verbatim makes a message
//! list look broken, so the two steps every client needs are implemented here:
//!
//! 1. bytes → text, falling back to ISO-8859-1 for the parts that are not valid
//!    UTF-8, because that is what a mail server that predates UTF-8 sends;
//! 2. RFC 2047 encoded words → text, including the rule that whitespace
//!    *between two adjacent encoded words* is not part of the value.
//!
//! The full MIME pipeline (`mail-parser`, phase 3) will decode bodies; this
//! module only has to make headers readable, and it is deliberately free of
//! dependencies so that it stays that way.

use chrono::{DateTime, FixedOffset, Utc};

use crate::models::message::MailAddress;

/// Decodes raw header bytes into text.
///
/// Valid UTF-8 is used as is; anything else is interpreted as ISO-8859-1, which
/// never fails and is the historical default of the protocol.
#[must_use]
pub fn decode_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|byte| cp1252_char(*byte)).collect(),
    }
}

/// Decodes a header value that may contain RFC 2047 encoded words.
#[must_use]
pub fn decode_header(bytes: &[u8]) -> String {
    decode_rfc2047(&decode_bytes(bytes))
}

/// Decodes RFC 2047 encoded words inside an already decoded string.
#[must_use]
pub fn decode_rfc2047(input: &str) -> String {
    let tokens = tokenise(input);
    let mut out = String::with_capacity(input.len());
    let mut previous_was_encoded = false;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Text(text) => {
                // Whitespace between two encoded words is not part of the value.
                let next_is_encoded = matches!(tokens.get(index + 1), Some(Token::Encoded(_)));
                if previous_was_encoded && next_is_encoded && text.trim().is_empty() {
                    continue;
                }
                out.push_str(text);
                previous_was_encoded = false;
            }
            Token::Encoded(word) => {
                out.push_str(&word.decode());
                previous_was_encoded = true;
            }
        }
    }

    out
}

/// Parses the date of an `ENVELOPE`.
///
/// The format is the RFC 2822 date of the message.  Servers do send the
/// occasional oddity, so an unparsable date yields `None` and the caller falls
/// back to the internal date of the message.
#[must_use]
pub fn parse_envelope_date(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(date) = DateTime::parse_from_rfc2822(raw) {
        return Some(date.with_timezone(&Utc));
    }
    // Some servers leave the day-of-week out, or use the IMAP internal date
    // format (`12-Nov-2026 14:00:00 +0100`).
    for format in ["%d %b %Y %H:%M:%S %z", "%d-%b-%Y %H:%M:%S %z"] {
        if let Ok(date) = DateTime::parse_from_str(raw, format) {
            return Some(date.with_timezone(&Utc));
        }
    }
    None
}

/// Converts one `ENVELOPE` address into a [`MailAddress`].
///
/// Returns `None` for an address without a mailbox, which is how servers encode
/// an undislosable recipient.
#[must_use]
pub fn address_from_envelope(
    name: Option<&[u8]>,
    mailbox: Option<&[u8]>,
    host: Option<&[u8]>,
) -> Option<MailAddress> {
    let mailbox = mailbox.map(decode_bytes).unwrap_or_default();
    let host = host.map(decode_bytes).unwrap_or_default();
    let address = if mailbox.is_empty() {
        String::new()
    } else if host.is_empty() {
        mailbox
    } else {
        format!("{mailbox}@{host}")
    };
    if address.is_empty() {
        return None;
    }
    let display = name.map(decode_header).unwrap_or_default();
    let display = display.trim();
    if display.is_empty() {
        Some(MailAddress::new(address))
    } else {
        Some(MailAddress::with_name(address, display))
    }
}

/// A piece of a header value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// Literal text.
    Text(String),
    /// One `=?charset?encoding?text?=` word.
    Encoded(EncodedWord),
}

/// One encoded word.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EncodedWord {
    charset: String,
    encoding: char,
    text: String,
}

impl EncodedWord {
    fn decode(&self) -> String {
        let bytes = match self.encoding.to_ascii_uppercase() {
            'B' => decode_base64(&self.text),
            'Q' => Some(decode_quoted_printable(&self.text)),
            _ => None,
        };
        let Some(bytes) = bytes else {
            // Not a valid encoded word after all; show it unchanged rather
            // than dropping content.
            return format!("=?{}?{}?{}?=", self.charset, self.encoding, self.text);
        };
        decode_charset(&bytes, &self.charset)
    }
}

/// Splits a header value into literal text and encoded words.
fn tokenise(input: &str) -> Vec<Token> {
    let bytes: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == '=' && bytes.get(index + 1) == Some(&'?') {
            if let Some((word, next)) = parse_encoded_word(&bytes, index) {
                if !literal.is_empty() {
                    tokens.push(Token::Text(std::mem::take(&mut literal)));
                }
                tokens.push(Token::Encoded(word));
                index = next;
                continue;
            }
        }
        literal.push(bytes[index]);
        index += 1;
    }

    if !literal.is_empty() {
        tokens.push(Token::Text(literal));
    }
    tokens
}

/// Parses `=?charset?encoding?text?=` starting at `start`.
///
/// Returns the word and the index just after `?=`, or `None` when the sequence
/// is malformed.  RFC 2047 limits every part to 75 characters; a longer run is
/// treated as plain text so that a stray `=?` in a subject cannot swallow the
/// rest of it.
fn parse_encoded_word(chars: &[char], start: usize) -> Option<(EncodedWord, usize)> {
    const LIMIT: usize = 75;
    let mut index = start + 2;
    let charset_start = index;
    while index < chars.len() && chars[index] != '?' {
        index += 1;
    }
    if index >= chars.len() || index - charset_start > LIMIT {
        return None;
    }
    let charset: String = chars[charset_start..index].iter().collect();
    index += 1;

    let encoding = *chars.get(index)?;
    if !matches!(encoding.to_ascii_uppercase(), 'B' | 'Q') {
        return None;
    }
    index += 1;
    if chars.get(index) != Some(&'?') {
        return None;
    }
    index += 1;

    let text_start = index;
    while index + 1 < chars.len() && !(chars[index] == '?' && chars[index + 1] == '=') {
        index += 1;
    }
    if index + 1 >= chars.len() {
        return None;
    }
    if index - text_start > LIMIT {
        return None;
    }
    let text: String = chars[text_start..index].iter().collect();

    Some((
        EncodedWord {
            charset,
            encoding,
            text,
        },
        index + 2,
    ))
}

/// Decodes the `B` encoding of an encoded word.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text.as_bytes()).ok()
}

/// Decodes the `Q` encoding of an encoded word: quoted printable with `_` for a
/// space.
fn decode_quoted_printable(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'_' => {
                out.push(b' ');
                index += 1;
            }
            b'=' if index + 2 < bytes.len() => {
                match hex_value(bytes[index + 1], bytes[index + 2]) {
                    Some(value) => {
                        out.push(value);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

fn hex_value(high: u8, low: u8) -> Option<u8> {
    let high = (high as char).to_digit(16)? as u8;
    let low = (low as char).to_digit(16)? as u8;
    Some(high * 16 + low)
}

/// Interprets decoded bytes according to the declared charset.
fn decode_charset(bytes: &[u8], charset: &str) -> String {
    let normalised = charset.trim().to_ascii_lowercase();
    match normalised.as_str() {
        "utf-8" | "utf8" => String::from_utf8_lossy(bytes).into_owned(),
        "us-ascii" | "ascii" | "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "cp1252"
        | "windows-1252" => bytes.iter().map(|byte| cp1252_char(*byte)).collect(),
        // An unknown charset: prefer UTF-8 when the bytes are valid, otherwise
        // fall back to ISO-8859-1 rather than producing replacement characters.
        _ => match std::str::from_utf8(bytes) {
            Ok(text) => text.to_string(),
            Err(_) => bytes.iter().map(|byte| cp1252_char(*byte)).collect(),
        },
    }
}

/// Maps one byte to a character the way Windows-1252 does.
///
/// Windows-1252 agrees with ISO-8859-1 except for `0x80..=0x9F`, where it
/// substitutes printable characters (a curly quote, a euro sign, ...).  Mail
/// written on Windows declares `windows-1252` and uses those bytes, so mapping
/// them to the ISO-8859-1 control characters would be wrong.
fn cp1252_char(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}',
        '\u{8f}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}',
        '\u{2014}', '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{17e}',
        '\u{178}',
    ];
    match byte {
        0x00..=0x7f => byte as char,
        0x80..=0x9f => HIGH[(byte - 0x80) as usize],
        _ => byte as char,
    }
}

/// Falls back to a `FixedOffset` for servers that send a numeric zone.
#[must_use]
pub fn parse_internal_date(raw: &str) -> Option<DateTime<FixedOffset>> {
    for format in ["%d-%b-%Y %H:%M:%S %z", "%d %b %Y %H:%M:%S %z"] {
        if let Ok(date) = DateTime::parse_from_str(raw.trim(), format) {
            return Some(date);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(decode_rfc2047("Rechnung 2024"), "Rechnung 2024");
        assert_eq!(decode_rfc2047(""), "");
    }

    #[test]
    fn utf8_bytes_are_used_directly() {
        assert_eq!(decode_bytes("Grüße".as_bytes()), "Grüße");
    }

    #[test]
    fn latin1_bytes_are_recovered() {
        // `Grüße` as ISO-8859-1: ü = 0xFC, ß = 0xDF.
        let bytes = b"Gr\xfc\xdfe";
        assert_eq!(decode_bytes(bytes), "Grüße");
    }

    #[test]
    fn windows_1252_punctuation_is_mapped() {
        // 0x93/0x94 are curly quotes in Windows-1252 and control characters in
        // ISO-8859-1.
        assert_eq!(decode_charset(b"\x93Hallo\x94", "windows-1252"), "“Hallo”");
        assert_eq!(decode_charset(b"\x80", "cp1252"), "€");
    }

    #[test]
    fn base64_encoded_words_are_decoded() {
        assert_eq!(
            decode_rfc2047("=?UTF-8?B?R3LDvMOfZQ==?="),
            "Grüße",
            "B encoding"
        );
    }

    #[test]
    fn quoted_encoded_words_are_decoded() {
        assert_eq!(decode_rfc2047("=?UTF-8?Q?Rechnung_2024?="), "Rechnung 2024");
        assert_eq!(decode_rfc2047("=?iso-8859-1?Q?Gr=FC=DFe?="), "Grüße");
        assert_eq!(decode_rfc2047("=?UTF-8?Q?Gr=C3=BC=C3=9Fe?="), "Grüße");
    }

    #[test]
    fn mixed_content_keeps_the_text_around_it() {
        assert_eq!(
            decode_rfc2047("Re: =?UTF-8?Q?Gr=C3=BC=C3=9Fe?= aus Berlin"),
            "Re: Grüße aus Berlin"
        );
        assert_eq!(
            decode_rfc2047("=?UTF-8?Q?Gr=C3=BC=C3=9Fe?= (privat)"),
            "Grüße (privat)"
        );
    }

    #[test]
    fn whitespace_between_two_encoded_words_disappears() {
        // RFC 2047 §6.2: the space is a display artefact, not content.
        assert_eq!(
            decode_rfc2047("=?UTF-8?Q?Rechnung?= =?UTF-8?Q?2024?="),
            "Rechnung2024"
        );
        // But a space before literal text is kept.
        assert_eq!(decode_rfc2047("=?UTF-8?Q?Rechnung?= 2024"), "Rechnung 2024");
    }

    #[test]
    fn a_malformed_word_is_shown_verbatim() {
        assert_eq!(decode_rfc2047("=?UTF-8?X?abc?="), "=?UTF-8?X?abc?=");
        assert_eq!(decode_rfc2047("=?UTF-8?Q?abc"), "=?UTF-8?Q?abc");
        assert_eq!(decode_rfc2047("=?"), "=?");
        assert_eq!(decode_rfc2047("=?UTF-8?B?!!!?="), "=?UTF-8?B?!!!?=");
    }

    #[test]
    fn an_unknown_charset_prefers_utf8() {
        assert_eq!(decode_charset("Grüße".as_bytes(), "x-unknown"), "Grüße");
        assert_eq!(decode_charset(b"Gr\xfc\xdfe", "x-unknown"), "Grüße");
    }

    #[test]
    fn envelope_dates_are_parsed() {
        let date = parse_envelope_date("Thu, 12 Nov 2026 14:00:00 +0100").unwrap();
        assert_eq!(date.to_rfc3339(), "2026-11-12T13:00:00+00:00");

        assert_eq!(parse_envelope_date(""), None);
        assert_eq!(parse_envelope_date("not a date"), None);
        // Without a day of week, and in the internal-date format.
        assert_eq!(
            parse_envelope_date("12 Nov 2026 14:00:00 +0100")
                .unwrap()
                .to_rfc3339(),
            "2026-11-12T13:00:00+00:00"
        );
        assert!(parse_envelope_date("12-Nov-2026 14:00:00 +0100").is_some());
    }

    #[test]
    fn addresses_are_built_from_envelope_fields() {
        let address =
            address_from_envelope(Some("Grüße".as_bytes()), Some(b"ada"), Some(b"example.org"))
                .unwrap();
        assert_eq!(address.email, "ada@example.org");
        assert_eq!(address.name.as_deref(), Some("Grüße"));

        let address = address_from_envelope(None, Some(b"ada"), Some(b"example.org")).unwrap();
        assert_eq!(address.to_header_value(), "ada@example.org");

        // An encoded display name is decoded.
        let address = address_from_envelope(
            Some("=?UTF-8?Q?Ada_Lovelace?=".as_bytes()),
            Some(b"ada"),
            Some(b"example.org"),
        )
        .unwrap();
        assert_eq!(address.name.as_deref(), Some("Ada Lovelace"));

        // A mailbox without a host (a local address) is still usable.
        let address = address_from_envelope(None, Some(b"postmaster"), None).unwrap();
        assert_eq!(address.email, "postmaster");

        assert!(address_from_envelope(None, None, None).is_none());
        assert!(address_from_envelope(Some(b"Name"), None, Some(b"host")).is_none());
    }

    #[test]
    fn internal_dates_are_parsed() {
        let date = parse_internal_date("12-Nov-2026 14:00:00 +0100").unwrap();
        assert_eq!(date.offset().local_minus_utc(), 3600);
        assert!(parse_internal_date("bogus").is_none());
    }
}
