//! Minimal PEM decoder used for the *extra* trust anchors of an account.
//!
//! HylMail talks to on-premises Exchange and GroupWise servers whose TLS
//! certificates are issued by the organisation's own CA.  Such a CA is not in
//! the operating system trust store, so the user has to point HylMail at it —
//! and what they can copy out of a wiki page is PEM text.
//!
//! Only the two things HylMail needs are implemented: turning PEM into DER
//! blocks, and turning those into certificates.  The decoder deliberately
//! tolerates the things real-world files contain (CRLF line endings, comments
//! before and after a block, several certificates in one file, missing trailing
//! newline) and rejects everything else, instead of silently ignoring a
//! certificate the user believed they had installed.

use base64::Engine as _;
use rustls::pki_types::CertificateDer;

/// One `-----BEGIN …-----` block of a PEM file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    /// The label between `BEGIN` and the five dashes, e.g. `CERTIFICATE`.
    pub label: String,
    /// The decoded DER payload.
    pub der: Vec<u8>,
}

impl PemBlock {
    /// `true` for the block kinds that carry a certificate.
    #[must_use]
    pub fn is_certificate(&self) -> bool {
        self.label.eq_ignore_ascii_case("CERTIFICATE")
            || self.label.eq_ignore_ascii_case("X509 CERTIFICATE")
    }
}

/// Why a PEM document could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PemError {
    /// A `BEGIN` line had no matching `END` line.
    UnterminatedBlock {
        /// The label of the block that was never closed.
        label: String,
    },
    /// The payload between the markers was not valid base64.
    InvalidBase64 {
        /// The label of the block that failed.
        label: String,
        /// Why base64 rejected the payload.
        message: String,
    },
    /// The document contained no certificate at all.
    NoCertificate,
}

impl std::fmt::Display for PemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnterminatedBlock { label } => {
                write!(f, "PEM block `{label}` is missing its END line")
            }
            Self::InvalidBase64 { label, message } => {
                write!(f, "PEM block `{label}` is not valid base64: {message}")
            }
            Self::NoCertificate => write!(f, "the PEM document contains no certificate"),
        }
    }
}

impl std::error::Error for PemError {}

/// Parses every PEM block of a document.
///
/// # Errors
///
/// Returns a [`PemError`] when a block is unterminated or its payload is not
/// base64.  Text outside of the blocks is ignored.
pub fn decode_blocks(pem: &str) -> Result<Vec<PemBlock>, PemError> {
    let mut blocks = Vec::new();
    let mut current: Option<(String, String)> = None;

    for line in pem.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("-----BEGIN ") {
            let label = rest.trim_end_matches('-').trim().to_string();
            current = Some((label, String::new()));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("-----END ") {
            let label = rest.trim_end_matches('-').trim().to_string();
            let Some((open_label, body)) = current.take() else {
                // An END without a BEGIN is junk we can safely skip.
                continue;
            };
            if !open_label.eq_ignore_ascii_case(&label) {
                return Err(PemError::UnterminatedBlock { label: open_label });
            }
            let der = base64::engine::general_purpose::STANDARD
                .decode(body.trim())
                .map_err(|error| PemError::InvalidBase64 {
                    label: open_label.clone(),
                    message: error.to_string(),
                })?;
            blocks.push(PemBlock {
                label: open_label,
                der,
            });
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(trimmed);
        }
    }

    if let Some((label, _)) = current {
        return Err(PemError::UnterminatedBlock { label });
    }
    Ok(blocks)
}

/// Extracts every certificate of a document as DER.
///
/// # Errors
///
/// Returns a [`PemError`] for an unreadable document, and
/// [`PemError::NoCertificate`] when the document contains only keys or other
/// unrelated blocks — the caller can then tell the user exactly that.
pub fn certificates(pem: &str) -> Result<Vec<CertificateDer<'static>>, PemError> {
    let blocks = decode_blocks(pem)?;
    let certificates: Vec<CertificateDer<'static>> = blocks
        .into_iter()
        .filter(PemBlock::is_certificate)
        .map(|block| CertificateDer::from(block.der))
        .collect();
    if certificates.is_empty() {
        return Err(PemError::NoCertificate);
    }
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny but structurally valid DER blob, wrapped in PEM below.
    const DER: &[u8] = b"\x30\x03\x02\x01\x2a";

    fn pem_of(der: &[u8], label: &str) -> String {
        let body = base64::engine::general_purpose::STANDARD.encode(der);
        format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
    }

    #[test]
    fn a_single_certificate_is_decoded() {
        let pem = pem_of(DER, "CERTIFICATE");
        let blocks = decode_blocks(&pem).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].label, "CERTIFICATE");
        assert_eq!(blocks[0].der, DER);
        assert!(blocks[0].is_certificate());
    }

    #[test]
    fn crlf_and_surrounding_prose_are_tolerated() {
        let pem = format!(
            "# Organisation CA\r\n# generated 2024-01-01\r\n{}\r\nsee https://ca.example.org\r\n",
            pem_of(DER, "CERTIFICATE").replace('\n', "\r\n")
        );
        let certificates = certificates(&pem).unwrap();
        assert_eq!(certificates.len(), 1);
        assert_eq!(certificates[0].as_ref(), DER);
    }

    #[test]
    fn several_certificates_are_returned_in_order() {
        let mut pem = pem_of(DER, "CERTIFICATE");
        pem.push_str(&pem_of(b"\x30\x03\x02\x01\x2b", "X509 CERTIFICATE"));
        let certificates = certificates(&pem).unwrap();
        assert_eq!(certificates.len(), 2);
        assert_eq!(certificates[0].as_ref(), DER);
        assert_eq!(certificates[1].as_ref(), b"\x30\x03\x02\x01\x2b");
    }

    #[test]
    fn a_key_only_document_reports_the_reason() {
        let pem = pem_of(DER, "PRIVATE KEY");
        assert_eq!(certificates(&pem).unwrap_err(), PemError::NoCertificate);
        // ... but the block itself still decodes.
        assert_eq!(decode_blocks(&pem).unwrap()[0].label, "PRIVATE KEY");
    }

    #[test]
    fn an_unterminated_block_is_rejected() {
        let pem = "-----BEGIN CERTIFICATE-----\nMAMCAQ==\n";
        assert_eq!(
            decode_blocks(pem).unwrap_err(),
            PemError::UnterminatedBlock {
                label: "CERTIFICATE".to_string()
            }
        );
    }

    #[test]
    fn invalid_base64_is_rejected_with_the_label() {
        let pem = "-----BEGIN CERTIFICATE-----\nnot base64 !!\n-----END CERTIFICATE-----\n";
        match decode_blocks(pem).unwrap_err() {
            PemError::InvalidBase64 { label, .. } => assert_eq!(label, "CERTIFICATE"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn an_empty_document_has_no_blocks() {
        assert!(decode_blocks("").unwrap().is_empty());
        assert!(decode_blocks("no pem here\n").unwrap().is_empty());
        assert_eq!(certificates("").unwrap_err(), PemError::NoCertificate);
    }

    #[test]
    fn an_end_without_a_begin_is_skipped() {
        let pem = format!("-----END CERTIFICATE-----\n{}", pem_of(DER, "CERTIFICATE"));
        assert_eq!(certificates(&pem).unwrap().len(), 1);
    }
}
