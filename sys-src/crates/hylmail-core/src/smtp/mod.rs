//! SMTP submission with `lettre`.
//!
//! HylMail never sends a message straight from the compose window: the message is
//! written to the outbox first (phase 1, [`crate::models::draft`]) and this
//! module is what drains it.  That is what makes "send" survive a laptop that
//! closes its lid mid-`DATA`.
//!
//! The transport is built per submission rather than kept in a process-wide
//! pool, because a desktop client submits a handful of messages per day and a
//! stale pooled connection to an on-premises relay is a far more common source
//! of "sending failed" than a fresh TCP handshake.
//!
//! TLS is the same [`TlsPolicy`] the IMAP side uses, so a self-signed
//! on-premises relay is trusted for reading *and* writing with one setting.

use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{
    Certificate, CertificateStore, Tls, TlsParameters, TlsParametersBuilder, TlsVersion,
};
use lettre::transport::smtp::extension::ClientId;
use lettre::transport::smtp::Error as SmtpError;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use secrecy::ExposeSecret;

use crate::error::{Error, Result};
use crate::models::account::{AuthKind, Endpoint, TlsMode};
use crate::net::{Timeouts, TlsPolicy};
use crate::secrets::Credential;

/// Everything needed to submit mail through one account.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    /// Server host, port and TLS mode.
    pub endpoint: Endpoint,
    /// Login name, which for some providers is the full e-mail address.
    pub username: String,
    /// How to authenticate.
    pub auth: AuthKind,
    /// Certificate policy, usually the same value as the IMAP side.
    pub policy: TlsPolicy,
    /// Deadlines.
    pub timeouts: Timeouts,
    /// `EHLO` name; `None` lets lettre use the local host name.
    pub helo_name: Option<String>,
}

impl SmtpConfig {
    /// Builds a configuration from the parts an account supplies.
    #[must_use]
    pub fn new(
        endpoint: Endpoint,
        username: impl Into<String>,
        auth: AuthKind,
        policy: TlsPolicy,
    ) -> Self {
        Self {
            endpoint,
            username: username.into(),
            auth,
            policy,
            timeouts: Timeouts::default(),
            helo_name: None,
        }
    }

    /// Uses the short deadlines of the account dialog.
    #[must_use]
    pub fn probe(mut self) -> Self {
        self.timeouts = Timeouts::probe();
        self
    }

    /// `true` when the message will travel encrypted.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.endpoint.tls != TlsMode::None
    }
}

/// What the server said about a submitted message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    /// `true` when the server accepted the message for delivery.
    pub accepted: bool,
    /// The raw reply, already stripped of the message body.
    pub response: String,
}

impl Submission {
    /// The reply as it should appear in the outbox entry.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.accepted {
            format!("accepted: {}", self.response)
        } else {
            format!("rejected: {}", self.response)
        }
    }
}

/// Submits one message.
///
/// `raw` is the complete RFC 5322 message as it will be stored in the Sent
/// folder; `from` and `recipients` form the SMTP envelope, which deliberately
/// can differ from the headers (`Bcc:` recipients are only ever in the
/// envelope).
///
/// # Errors
///
/// The returned error is [`Error::Auth`] when the server rejected the
/// credentials, [`Error::Protocol`] with the server's reply when it rejected the
/// message, [`Error::Tls`] on a handshake failure and [`Error::ConnectionLost`]
/// when the connection died.  Callers decide whether to retry by looking at
/// [`Error::is_transient`].
pub async fn submit(
    config: &SmtpConfig,
    credential: Credential<'_>,
    from: &str,
    recipients: &[String],
    raw: &[u8],
) -> Result<Submission> {
    if recipients.is_empty() {
        return Err(Error::config(
            "a message needs at least one recipient before it can be sent",
        ));
    }
    let envelope = envelope(from, recipients)?;
    let transport = transport(config, credential)?;
    match transport.send_raw(&envelope, raw).await {
        Ok(response) => Ok(Submission {
            accepted: response.is_positive(),
            response: describe_response(&response),
        }),
        Err(error) => Err(classify(&error, config)),
    }
}

/// Opens a connection and authenticates, without sending anything.
///
/// This is what the "verify outgoing server" button in the account dialog runs.
///
/// # Errors
///
/// See [`submit`].
pub async fn verify(config: &SmtpConfig, credential: Credential<'_>) -> Result<()> {
    let transport = transport(config, credential)?;
    match transport.test_connection().await {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::ConnectionLost {
            protocol: "SMTP",
            host: config.endpoint.host.clone(),
        }),
        Err(error) => Err(classify(&error, config)),
    }
}

/// Builds the SMTP envelope.
fn envelope(from: &str, recipients: &[String]) -> Result<lettre::address::Envelope> {
    let from = parse_address(from)?;
    let mut to = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        to.push(parse_address(recipient)?);
    }
    lettre::address::Envelope::new(Some(from), to)
        .map_err(|error| Error::config(format!("invalid recipient list: {error}")))
}

/// Parses one address, reporting which one was wrong.
fn parse_address(address: &str) -> Result<lettre::address::Address> {
    address
        .parse::<lettre::address::Address>()
        .map_err(|error| Error::config(format!("invalid address `{address}`: {error}")))
}

/// Builds the transport for one submission.
fn transport(
    config: &SmtpConfig,
    credential: Credential<'_>,
) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let endpoint = &config.endpoint;
    let tls = match endpoint.tls {
        TlsMode::None => Tls::None,
        TlsMode::Implicit => Tls::Wrapper(tls_parameters(config)?),
        TlsMode::StartTls => Tls::Required(tls_parameters(config)?),
    };

    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&endpoint.host)
        .port(endpoint.port)
        .tls(tls)
        .timeout(Some(config.timeouts.command));

    if let Some(helo) = config.helo_name.as_ref() {
        builder = builder.hello_name(ClientId::Domain(helo.clone()));
    }

    match credential {
        Credential::Password(password) => {
            if config.auth == AuthKind::OAuth2 {
                return Err(Error::Unsupported(
                    "an OAuth2 account needs an access token, not a password".to_string(),
                ));
            }
            builder = builder
                // Office 365 and several on-premises relays still need `LOGIN`.
                .authentication(vec![Mechanism::Plain, Mechanism::Login])
                .credentials(Credentials::new(
                    config.username.clone(),
                    password.expose_secret().to_string(),
                ));
        }
        Credential::OAuth2 { user, token } => {
            builder = builder
                .authentication(vec![Mechanism::Xoauth2])
                .credentials(Credentials::new(user, token.expose_secret().to_string()));
        }
    }

    Ok(builder.build())
}

/// Translates the certificate policy into lettre's TLS parameters.
///
/// The extra trust anchors come from the same PEM strings the IMAP connection
/// uses, so "trust this certificate" is a single per-account setting.
fn tls_parameters(config: &SmtpConfig) -> Result<TlsParameters> {
    let policy = &config.policy;
    let mut builder = TlsParametersBuilder::new(config.endpoint.host.clone())
        .certificate_store(CertificateStore::Default);

    if policy.require_tls13 {
        builder = builder.set_min_tls_version(TlsVersion::Tlsv13);
    }
    if !policy.verify {
        // Mirrors `TlsPolicy::insecure()`: only reachable through an explicit
        // user decision or a test fixture, never through configuration parsing.
        builder = builder
            .dangerous_accept_invalid_certs(true)
            .dangerous_accept_invalid_hostnames(true);
    }
    for pem in &policy.extra_roots_pem {
        // `Certificate::from_pem` is not defensive enough for a value that came
        // out of a settings file: a malformed blob aborts the process instead
        // of returning an error.  Our own parser rejects it first, so a typo in
        // `settings.json` can only ever produce an error message.
        let _ = crate::net::pem::certificates(pem)
            .map_err(|error| Error::TlsConfig(format!("unreadable extra root: {error}")))?;
        let certificate = Certificate::from_pem(pem.as_bytes())
            .map_err(|error| Error::TlsConfig(format!("unreadable extra root: {error}")))?;
        builder = builder.add_root_certificate(certificate);
    }

    builder
        .build()
        .map_err(|error| Error::TlsConfig(error.to_string()))
}

/// Renders a server reply into one line for the log and the outbox entry.
fn describe_response(response: &lettre::transport::smtp::response::Response) -> String {
    let mut text = String::new();
    for line in response.message() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(line);
    }
    if text.is_empty() {
        format!("{}", response.code())
    } else {
        format!("{} {}", response.code(), text)
    }
}

/// Maps a lettre error onto the crate error type.
fn classify(error: &SmtpError, config: &SmtpConfig) -> Error {
    let host = config.endpoint.host.clone();
    if error.is_tls() {
        return Error::Tls {
            host,
            message: error.to_string(),
        };
    }
    if error.is_timeout() {
        return Error::timeout("SMTP submission", config.timeouts.command.as_secs());
    }
    // `535` is RFC 4954's "authentication credentials invalid".  The account
    // dialog can act on that, so it must not be buried in a protocol error.
    if error.status().is_some_and(|code| code.to_string() == "535") {
        return Error::Auth {
            account: config.username.clone(),
            message: error.to_string(),
        };
    }
    if error.is_transient() || error.is_permanent() {
        // A 4xx/5xx reply: the message itself is the problem, not the network.
        return Error::protocol("SMTP", "submission", error.to_string());
    }
    if error.is_transport_shutdown() || error.is_response() || error.is_client() {
        return Error::protocol("SMTP", "submission", error.to_string());
    }
    // What is left is a network or connection level failure.
    Error::ConnectionLost {
        protocol: "SMTP",
        host,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::models::account::TlsMode;
    use secrecy::SecretString;

    fn config(tls: TlsMode) -> SmtpConfig {
        SmtpConfig::new(
            Endpoint::new("localhost", 587, tls),
            "ada@example.org",
            AuthKind::Password,
            TlsPolicy::strict(),
        )
    }

    #[test]
    fn an_encrypted_endpoint_is_reported_as_such() {
        assert!(config(TlsMode::Implicit).is_encrypted());
        assert!(config(TlsMode::StartTls).is_encrypted());
        assert!(!config(TlsMode::None).is_encrypted());
    }

    #[test]
    fn addresses_are_validated_before_a_connection_is_opened() {
        let error = parse_address("not an address").unwrap_err();
        assert!(error.to_string().contains("not an address"));
        assert_eq!(
            parse_address("ada@example.org").unwrap().to_string(),
            "ada@example.org"
        );
    }

    #[test]
    fn an_empty_recipient_list_is_rejected() {
        assert!(envelope("ada@example.org", &[]).is_err());
        let built = envelope("ada@example.org", &["bob@example.org".to_string()]);
        assert_eq!(built.unwrap().to().len(), 1);
    }

    #[test]
    fn the_password_is_used_for_the_configured_mechanism() {
        let secret = SecretString::from("geheim");
        let transport = transport(&config(TlsMode::Implicit), Credential::Password(&secret));
        assert!(transport.is_ok());
        assert!(format!("{:?}", Credential::Password(&secret)).contains("[REDACTED]"));
    }

    #[test]
    fn an_oauth_account_refuses_a_password() {
        let secret = SecretString::from("geheim");
        let mut config = config(TlsMode::Implicit);
        config.auth = AuthKind::OAuth2;
        let error = transport(&config, Credential::Password(&secret)).unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
    }

    #[test]
    fn the_token_is_used_for_xoauth2() {
        let token = SecretString::from("token");
        let mut config = config(TlsMode::Implicit);
        config.auth = AuthKind::OAuth2;
        let credential = Credential::OAuth2 {
            user: "ada@example.org".to_string(),
            token: &token,
        };
        assert!(credential.is_token());
        assert!(transport(&config, credential).is_ok());
    }

    #[test]
    fn an_extra_root_is_added_to_the_trust_store() {
        // The same fixture the `net` tests use, so "trust this certificate"
        // follows exactly the same path for reading and for writing mail.
        let pem = include_str!("../../tests/fixtures/tls/localhost.crt.pem");
        let mut config = config(TlsMode::Implicit);
        config.policy.extra_roots_pem.push(pem.to_string());
        assert!(tls_parameters(&config).is_ok());
    }

    #[test]
    fn an_unreadable_extra_root_is_rejected_before_connecting() {
        let mut config = config(TlsMode::Implicit);
        config.policy.extra_roots_pem.push("not a pem".to_string());
        assert!(matches!(
            tls_parameters(&config),
            Err(Error::TlsConfig(_)) | Err(Error::Tls { .. })
        ));
    }

    #[test]
    fn an_accepted_submission_is_described_in_words() {
        let submission = Submission {
            accepted: true,
            response: "250 2.0.0 Ok: queued as 4242".to_string(),
        };
        assert!(submission.describe().starts_with("accepted:"));
        let submission = Submission {
            accepted: false,
            response: "550 5.1.1 No such user".to_string(),
        };
        assert!(submission.describe().starts_with("rejected:"));
    }
}
