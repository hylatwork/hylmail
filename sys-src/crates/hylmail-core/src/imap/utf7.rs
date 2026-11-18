//! Modified UTF-7 for IMAP mailbox names ([RFC 3501 §5.1.3]).
//!
//! IMAP predates Unicode and therefore transports mailbox names in *modified*
//! UTF-7: printable US-ASCII represents itself, `&` is written as `&-`, and
//! everything else is written as a `&`-terminated "shifted" run of a base64
//! variant that uses `,` instead of `/` over the UTF-16BE encoding of the
//! characters.
//!
//! ```text
//! Entwürfe        <->  Entw&APw-rfe
//! Größe           <->  Gr&APY-ze
//! [Gmail]/Gesendet <-> [Gmail]/Gesendet
//! A&B             <->  A&-B
//! ```
//!
//! Getting this wrong is fatal in a subtle way: the folder is then simply not
//! found on the server, and a message that is moved into it is lost.  That is
//! why both directions are implemented here, independently of any network code,
//! and covered by the RFC examples plus the names that Exchange, Gmail, Dovecot
//! and GroupWise produce in practice.
//!
//! [RFC 3501 §5.1.3]: https://datatracker.ietf.org/doc/html/rfc3501#section-5.1.3

use std::fmt;

/// Failure to decode a modified UTF-7 mailbox name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Utf7Error {
    /// A shifted sequence contained a character outside the modified base64
    /// alphabet.
    InvalidCharacter {
        /// Byte offset inside the input.
        offset: usize,
        /// Offending character.
        character: char,
    },
    /// A shifted sequence ended with a dangling base64 character.
    TruncatedSequence {
        /// Byte offset inside the input where the shifted run started.
        offset: usize,
    },
    /// The decoded UTF-16 code units were not a valid string (unpaired
    /// surrogate).
    InvalidUtf16 {
        /// Byte offset inside the input where the shifted run started.
        offset: usize,
    },
}

impl fmt::Display for Utf7Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCharacter { offset, character } => write!(
                f,
                "invalid character `{character}` at byte {offset} of a modified UTF-7 name"
            ),
            Self::TruncatedSequence { offset } => write!(
                f,
                "truncated modified UTF-7 sequence starting at byte {offset}"
            ),
            Self::InvalidUtf16 { offset } => write!(
                f,
                "unpaired UTF-16 surrogate in the modified UTF-7 sequence at byte {offset}"
            ),
        }
    }
}

impl std::error::Error for Utf7Error {}

/// Encodes a mailbox name from UTF-8 into modified UTF-7.
///
/// The terminating `-` of a shifted sequence is always written.  The RFC allows
/// omitting it when the next character cannot be confused with base64, but
/// always writing it keeps the output unambiguous, is accepted by every server
/// and makes the result stable enough to compare in tests.
#[must_use]
pub fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending: Vec<u16> = Vec::new();

    // Flushes the accumulated UTF-16 code units as one shifted sequence.
    fn flush(out: &mut String, pending: &mut Vec<u16>) {
        if pending.is_empty() {
            return;
        }
        out.push('&');
        encode_base64_utf16(out, pending);
        out.push('-');
        pending.clear();
    }

    for character in name.chars() {
        if is_direct(character) {
            flush(&mut out, &mut pending);
            out.push(character);
        } else if character == '&' {
            flush(&mut out, &mut pending);
            out.push_str("&-");
        } else {
            let mut buffer = [0_u16; 2];
            pending.extend_from_slice(character.encode_utf16(&mut buffer));
        }
    }
    flush(&mut out, &mut pending);
    out
}

/// Decodes a modified UTF-7 mailbox name into UTF-8.
///
/// Accepts sequences whose terminating `-` was omitted whenever what follows
/// cannot be confused with base64, which several servers (notably Dovecot) emit
/// in practice.  A run that was cut in half in the middle is still rejected.
///
/// # Errors
///
/// Returns a [`Utf7Error`] when the input is not valid modified UTF-7.
pub fn decode(name: &str) -> Result<String, Utf7Error> {
    let bytes: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len());
    let mut index = 0;

    while index < bytes.len() {
        let character = bytes[index];
        if character != '&' {
            out.push(character);
            index += 1;
            continue;
        }

        let start = index;
        // `&-` is a literal ampersand.
        if bytes.get(index + 1) == Some(&'-') {
            out.push('&');
            index += 2;
            continue;
        }

        index += 1;
        let mut units: Vec<u16> = Vec::new();
        let mut accumulator: u32 = 0;
        let mut bits: u32 = 0;
        let mut terminated = false;

        while index < bytes.len() {
            let current = bytes[index];
            if current == '-' {
                terminated = true;
                index += 1;
                break;
            }
            let Some(value) = base64_value(current) else {
                // Any character outside the alphabet ends the shifted run.
                break;
            };
            accumulator = (accumulator << 6) | u32::from(value);
            bits += 6;
            if bits >= 16 {
                bits -= 16;
                units.push(((accumulator >> bits) & 0xFFFF) as u16);
            }
            index += 1;
        }

        // Without a terminator the run is only unambiguous when what is left
        // over is pure padding: at most four zero bits.  Six or more bits mean
        // a character was cut in half, and a non-zero remainder means the
        // sender wrote something we cannot interpret.
        if !terminated && (bits > 4 || (accumulator & ((1 << bits) - 1)) != 0) {
            return Err(Utf7Error::TruncatedSequence { offset: start });
        }

        match String::from_utf16(&units) {
            Ok(decoded) => out.push_str(&decoded),
            Err(_) => return Err(Utf7Error::InvalidUtf16 { offset: start }),
        }
    }

    Ok(out)
}

/// Decodes a mailbox name, falling back to the raw input when it is not valid
/// modified UTF-7.
///
/// A broken folder name must never make the whole folder list unusable, so this
/// is the function the IMAP layer uses for `LIST` results.
#[must_use]
pub fn decode_lossy(name: &str) -> String {
    decode(name).unwrap_or_else(|_| name.to_string())
}

/// `true` for the characters that represent themselves (RFC 3501 §5.1.3).
#[must_use]
pub fn is_direct(character: char) -> bool {
    matches!(character, '\u{20}'..='\u{25}' | '\u{27}'..='\u{7e}')
}

/// Splits a full mailbox path into its display name and its parent path.
///
/// `delimiter` is the hierarchy separator reported by the server (`/` or `.`),
/// or `None` for a flat namespace in which case there is no parent.
#[must_use]
pub fn split_path(path: &str, delimiter: Option<char>) -> (Option<String>, String) {
    let Some(delimiter) = delimiter else {
        return (None, path.to_string());
    };
    match path.rsplit_once(delimiter) {
        Some(("", name)) => (None, name.to_string()),
        Some((parent, name)) => (Some(parent.to_string()), name.to_string()),
        None => (None, path.to_string()),
    }
}

/// Joins a parent path and a name with the folder delimiter.
#[must_use]
pub fn join_path(parent: &str, name: &str, delimiter: Option<char>) -> String {
    match delimiter {
        Some(delimiter) if !parent.is_empty() => format!("{parent}{delimiter}{name}"),
        _ => name.to_string(),
    }
}

/// The modified base64 alphabet: `A-Z a-z 0-9 + ,`.
fn base64_value(character: char) -> Option<u8> {
    match character {
        'A'..='Z' => Some((character as u8) - b'A'),
        'a'..='z' => Some((character as u8) - b'a' + 26),
        '0'..='9' => Some((character as u8) - b'0' + 52),
        '+' => Some(62),
        ',' => Some(63),
        _ => None,
    }
}

/// The inverse of [`base64_value`].
fn base64_digit(value: u8) -> char {
    match value {
        0..=25 => (b'A' + value) as char,
        26..=51 => (b'a' + value - 26) as char,
        52..=61 => (b'0' + value - 52) as char,
        62 => '+',
        _ => ',',
    }
}

/// Writes the modified base64 of a UTF-16BE code unit sequence.
fn encode_base64_utf16(out: &mut String, units: &[u16]) {
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for unit in units {
        accumulator = (accumulator << 16) | u32::from(*unit);
        bits += 16;
        while bits >= 6 {
            bits -= 6;
            out.push(base64_digit(((accumulator >> bits) & 0x3F) as u8));
        }
    }
    if bits > 0 {
        out.push(base64_digit(((accumulator << (6 - bits)) & 0x3F) as u8));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_names_pass_through() {
        for name in [
            "INBOX",
            "[Gmail]/Gesendet",
            "Sent Items",
            "2024/Rechnungen",
            "~tilde",
            "'quoted'",
        ] {
            assert_eq!(encode(name), name, "encoding changed an ASCII name");
            assert_eq!(decode(name).unwrap(), name);
        }
    }

    #[test]
    fn ampersand_is_escaped() {
        assert_eq!(encode("A&B"), "A&-B");
        assert_eq!(decode("A&-B").unwrap(), "A&B");
        assert_eq!(encode("&"), "&-");
        assert_eq!(decode("&-").unwrap(), "&");
    }

    #[test]
    fn rfc_examples_round_trip() {
        // The examples from RFC 3501 §5.1.3.
        assert_eq!(
            encode("~peter/mail/台北/日本語"),
            "~peter/mail/&U,BTFw-/&ZeVnLIqe-"
        );
        assert_eq!(
            decode("~peter/mail/&U,BTFw-/&ZeVnLIqe-").unwrap(),
            "~peter/mail/台北/日本語"
        );
    }

    #[test]
    fn german_folder_names_round_trip() {
        let cases = [
            ("Entwürfe", "Entw&APw-rfe"),
            ("Gelöschte Objekte", "Gel&APY-schte Objekte"),
            ("Über uns", "&ANw-ber uns"),
            ("Änderungen", "&AMQ-nderungen"),
            ("Österreich", "&ANY-sterreich"),
            ("Straße", "Stra&AN8-e"),
            // Two non-ASCII characters in a row share one shifted run: the
            // UTF-16 of `öß` is `00f6 00df`, which is 32 bits = `APYA3w`.
            ("Größe", "Gr&APYA3w-e"),
        ];
        for (plain, encoded) in cases {
            assert_eq!(encode(plain), encoded, "encoding {plain}");
            assert_eq!(decode(encoded).unwrap(), plain, "decoding {encoded}");
        }
    }

    #[test]
    fn consecutive_non_ascii_characters_share_one_shifted_run() {
        // `äöü` is 48 bits of UTF-16 and therefore eight base64 characters.
        assert_eq!(encode("äöü"), "&AOQA9gD8-");
        assert_eq!(decode("&AOQA9gD8-").unwrap(), "äöü");
        // A run is only interrupted by a directly encodable character.
        assert_eq!(encode("ä-ö"), "&AOQ--&APY-");
        assert_eq!(decode("&AOQ--&APY-").unwrap(), "ä-ö");
        // A dash at the very end needs no second terminator.
        assert_eq!(encode("ä-"), "&AOQ--");
        assert_eq!(decode("&AOQ--").unwrap(), "ä-");
    }

    #[test]
    fn surrogate_pairs_round_trip() {
        // U+1F600 is outside the BMP and needs a UTF-16 surrogate pair.
        let name = "Emoji 😀 Ordner";
        let encoded = encode(name);
        assert!(encoded.contains('&'));
        assert_eq!(decode(&encoded).unwrap(), name);
    }

    #[test]
    fn a_missing_terminator_is_accepted() {
        // Dovecot drops the trailing `-` at the end of a name.
        assert_eq!(decode("Entw&APw-rfe").unwrap(), "Entwürfe");
        assert_eq!(decode("Entw&APw").unwrap(), "Entwü");
        // The `-` that ends a run is consumed, it is not a literal dash.
        assert_eq!(decode("&APw-rfe").unwrap(), "ürfe");
        // A literal dash after a run needs the run's own terminator first.
        assert_eq!(decode("&APw--rfe").unwrap(), "ü-rfe");
    }

    #[test]
    fn a_half_written_sequence_is_rejected() {
        // One or two base64 characters cannot carry a 16 bit code unit.
        assert_eq!(
            decode("&A").unwrap_err(),
            Utf7Error::TruncatedSequence { offset: 0 }
        );
        assert_eq!(
            decode("Entw&AP").unwrap_err(),
            Utf7Error::TruncatedSequence { offset: 4 }
        );
    }

    #[test]
    fn a_shifted_run_ends_at_the_first_non_base64_character() {
        // A name a server produced without the terminator before a space.
        assert_eq!(decode("&APw x").unwrap(), "ü x");
    }

    #[test]
    fn unpaired_surrogates_are_rejected() {
        // `&2AA-` is a single high surrogate.
        assert_eq!(
            decode("&2AA-").unwrap_err(),
            Utf7Error::InvalidUtf16 { offset: 0 }
        );
    }

    #[test]
    fn lossy_decoding_never_fails() {
        assert_eq!(decode_lossy("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode_lossy("broken&2AA-"), "broken&2AA-");
    }

    #[test]
    fn empty_input_is_handled() {
        assert_eq!(encode(""), "");
        assert_eq!(decode("").unwrap(), "");
        assert_eq!(decode("&").unwrap(), "");
    }

    #[test]
    fn paths_are_split_and_joined() {
        assert_eq!(
            split_path("INBOX/Projekte/HylMail", Some('/')),
            (Some("INBOX/Projekte".to_string()), "HylMail".to_string())
        );
        assert_eq!(
            split_path("HylMail", Some('/')),
            (None, "HylMail".to_string())
        );
        assert_eq!(
            split_path("/leading", Some('/')),
            (None, "leading".to_string())
        );
        assert_eq!(split_path("flat", None), (None, "flat".to_string()));
        assert_eq!(join_path("INBOX", "HylMail", Some('/')), "INBOX/HylMail");
        assert_eq!(join_path("", "INBOX", Some('/')), "INBOX");
        assert_eq!(join_path("INBOX", "HylMail", None), "HylMail");
    }

    #[test]
    fn direct_characters_are_exactly_the_printable_ascii_range() {
        assert!(is_direct(' '));
        assert!(is_direct('~'));
        assert!(!is_direct('&'));
        assert!(!is_direct('\n'));
        assert!(!is_direct('\u{7f}'));
        assert!(!is_direct('ü'));
    }
}
