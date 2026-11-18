//! The TLS policy shared by IMAP, SMTP and (in phase 3) the SOAP clients.
//!
//! HylMail verifies certificates against the **operating system trust store**
//! instead of a bundled root list.  That is the only choice that works for the
//! audience this client is written for: an on-premises Exchange or GroupWise
//! server presents a certificate from the organisation's own CA, and the
//! organisation has already installed that CA in the machine's trust store.
//! Pinning Mozilla's roots would reject exactly the servers the application
//! exists to talk to.
//!
//! Extra roots can be added per account for the case where the CA is *not* in
//! the machine store.  Disabling verification entirely is possible but has to be
//! requested explicitly, is logged loudly, and is never a default.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

use crate::error::{Error, Result};
use crate::net::pem;

/// How a connection is protected.
///
/// [`Default`] is implemented as [`TlsPolicy::strict`]: a policy type whose
/// default is *less* safe than its constructor would be a trap.
#[derive(Debug, Clone)]
pub struct TlsPolicy {
    /// Verify the certificate chain and the host name.  Never disable this in
    /// production code; the field exists so tests and an explicit
    /// "trust this certificate" workflow can.
    pub verify: bool,
    /// PEM encoded extra trust anchors, appended to the platform store.
    pub extra_roots_pem: Vec<String>,
    /// Refuse anything below TLS 1.3.
    pub require_tls13: bool,
}

impl Default for TlsPolicy {
    fn default() -> Self {
        Self::strict()
    }
}

impl TlsPolicy {
    /// The policy every connection uses unless the user says otherwise.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            verify: true,
            extra_roots_pem: Vec::new(),
            require_tls13: false,
        }
    }

    /// Accepts any certificate.  Only for tests and for the explicit
    /// "trust this certificate" flow; every caller of this must log a warning.
    #[must_use]
    pub fn insecure() -> Self {
        Self {
            verify: false,
            extra_roots_pem: Vec::new(),
            require_tls13: false,
        }
    }

    /// Adds one PEM document of extra trust anchors.
    ///
    /// # Errors
    ///
    /// Returns an error when the document cannot be read, so that a mistyped CA
    /// is reported while the user is still in the account dialog instead of
    /// surfacing later as a confusing handshake failure.
    pub fn with_extra_roots_pem(mut self, pem_text: impl Into<String>) -> Result<Self> {
        let pem_text = pem_text.into();
        if !pem_text.trim().is_empty() {
            pem::certificates(&pem_text).map_err(|error| Error::TlsConfig(error.to_string()))?;
            self.extra_roots_pem.push(pem_text);
        }
        Ok(self)
    }

    /// Require TLS 1.3 or newer.
    #[must_use]
    pub fn require_tls13(mut self) -> Self {
        self.require_tls13 = true;
        self
    }

    /// `true` when the chain is verified.
    #[must_use]
    pub fn is_verifying(&self) -> bool {
        self.verify
    }

    /// Number of extra trust anchors that are configured.
    ///
    /// # Errors
    ///
    /// Returns an error when one of the PEM documents is unreadable; the
    /// documents were validated by [`Self::with_extra_roots_pem`], so this can
    /// only fail for a policy that was built by hand.
    pub fn extra_root_count(&self) -> Result<usize> {
        let mut count = 0;
        for document in &self.extra_roots_pem {
            count += pem::certificates(document)
                .map_err(|error| Error::TlsConfig(error.to_string()))?
                .len();
        }
        Ok(count)
    }

    /// A one line description for the log and the account dialog.
    ///
    /// # Errors
    ///
    /// Returns an error when the extra roots cannot be read.
    pub fn describe(&self) -> Result<String> {
        let mut parts = Vec::new();
        parts.push(if self.require_tls13 {
            "TLS 1.3".to_string()
        } else {
            "TLS 1.2+".to_string()
        });
        if self.verify {
            parts.push("certificates verified against the system trust store".to_string());
            let extra = self.extra_root_count()?;
            if extra > 0 {
                parts.push(format!("{extra} extra root(s)"));
            }
        } else {
            parts.push("CERTIFICATE VERIFICATION DISABLED".to_string());
        }
        Ok(parts.join(", "))
    }

    /// Builds the rustls configuration used by every protocol.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TlsConfig`] when an extra root is unreadable or the
    /// platform verifier cannot be created.
    pub fn client_config(&self) -> Result<Arc<ClientConfig>> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        let versions: &[&rustls::SupportedProtocolVersion] = if self.require_tls13 {
            &[&rustls::version::TLS13]
        } else {
            rustls::DEFAULT_VERSIONS
        };

        let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(versions)
            .map_err(|error| Error::TlsConfig(error.to_string()))?;

        let builder = if self.verify {
            let mut roots: Vec<CertificateDer<'static>> = Vec::new();
            for document in &self.extra_roots_pem {
                roots.extend(
                    pem::certificates(document)
                        .map_err(|error| Error::TlsConfig(error.to_string()))?,
                );
            }
            let verifier = if roots.is_empty() {
                rustls_platform_verifier::Verifier::new(Arc::clone(&provider))
            } else {
                rustls_platform_verifier::Verifier::new_with_extra_roots(
                    roots,
                    Arc::clone(&provider),
                )
            }
            .map_err(|error| Error::TlsConfig(error.to_string()))?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
        } else {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerification::new(provider)))
        };

        Ok(Arc::new(builder.with_no_client_auth()))
    }
}

/// Converts a host name into the [`ServerName`] rustls needs for SNI.
///
/// Accepts DNS names and IP literals; anything else (a bracketed IPv6 literal
/// with a zone id, an empty string) is rejected before a socket is opened.
///
/// # Errors
///
/// Returns [`Error::TlsConfig`] when the host cannot be used as a server name.
pub fn server_name(host: &str) -> Result<ServerName<'static>> {
    if let Ok(name) = ServerName::try_from(host.to_string()) {
        return Ok(name);
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return Ok(ServerName::IpAddress(address.into()));
    }
    Err(Error::TlsConfig(format!(
        "`{host}` is not a valid host name or IP address"
    )))
}

/// A verifier that accepts every certificate but still checks the handshake
/// signature, so a corrupted or truncated certificate is still detected.
#[derive(Debug)]
struct NoVerification {
    provider: Arc<CryptoProvider>,
}

impl NoVerification {
    fn new(provider: Arc<CryptoProvider>) -> Self {
        Self { provider }
    }
}

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, self-signed certificate for `localhost`, used by the tests in
    /// this module and by the integration tests.  See
    /// `tests/fixtures/tls/README.md`: it is a throwaway generated with
    /// `openssl` and must never be trusted outside this test suite.
    const FIXTURE_CERT: &str = include_str!("../../tests/fixtures/tls/localhost.crt.pem");

    #[test]
    fn the_default_policy_verifies() {
        let policy = TlsPolicy::default();
        assert!(policy.is_verifying());
        assert!(!policy.require_tls13);
        assert_eq!(policy.extra_root_count().unwrap(), 0);
        assert!(policy.describe().unwrap().contains("TLS 1.2+"));
        assert!(policy.client_config().is_ok());
    }

    #[test]
    fn a_strict_config_is_built() {
        let config = TlsPolicy::strict().client_config().unwrap();
        assert!(config.alpn_protocols.is_empty());
        assert!(config.enable_sni);
        assert!(!config.enable_early_data);
    }

    #[test]
    fn tls13_only_policy_is_built() {
        let policy = TlsPolicy::strict().require_tls13();
        assert!(policy.require_tls13);
        assert!(policy.describe().unwrap().starts_with("TLS 1.3"));
        let config = policy.client_config().unwrap();
        // ALPN is only advertised when a protocol asks for it.
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn extra_roots_are_loaded_and_counted() {
        let policy = TlsPolicy::strict()
            .with_extra_roots_pem(FIXTURE_CERT)
            .unwrap();
        assert_eq!(policy.extra_root_count().unwrap(), 1);
        let description = policy.describe().unwrap();
        assert!(description.contains("1 extra root"));
        assert!(policy.client_config().is_ok());
    }

    #[test]
    fn a_broken_extra_root_is_reported_while_it_is_configured() {
        let error = TlsPolicy::strict()
            .with_extra_roots_pem("-----BEGIN CERTIFICATE-----\n@@@\n-----END CERTIFICATE-----\n")
            .unwrap_err();
        assert!(matches!(error, Error::TlsConfig(_)));
    }

    #[test]
    fn an_empty_extra_root_is_ignored() {
        let policy = TlsPolicy::strict().with_extra_roots_pem("  \n ").unwrap();
        assert_eq!(policy.extra_root_count().unwrap(), 0);
    }

    #[test]
    fn the_insecure_policy_says_so() {
        let policy = TlsPolicy::insecure();
        assert!(!policy.is_verifying());
        assert!(policy
            .describe()
            .unwrap()
            .contains("CERTIFICATE VERIFICATION DISABLED"));
        assert!(policy.client_config().is_ok());
    }

    #[test]
    fn server_names_accept_dns_and_ip_literals() {
        assert!(matches!(
            server_name("imap.example.org").unwrap(),
            ServerName::DnsName(_)
        ));
        assert!(matches!(
            server_name("192.0.2.10").unwrap(),
            ServerName::IpAddress(_)
        ));
        assert!(matches!(
            server_name("::1").unwrap(),
            ServerName::IpAddress(_)
        ));
        assert!(server_name("").is_err());
        assert!(server_name("has space").is_err());
    }
}
