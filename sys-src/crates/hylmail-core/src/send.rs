//! Submitting a composed message over the protocol the account speaks.
//!
//! Reading is spread over three clients, and so is sending.  Everything above
//! this module - the outbox state machine, its retry policy, the GUI and the
//! CLI - only ever sees a [`Receipt`]; which wire the bytes travel on is
//! decided here:
//!
//! * an **IMAP** account hands the finished RFC 5322 message to SMTP
//!   ([`crate::smtp`]), with the envelope recipients taken from the composer so
//!   that `Bcc:` never reaches a header,
//! * an **Exchange** account submits the same bytes through EWS
//!   (`CreateItem` with `SendAndSaveCopy`), which delivers the message *and*
//!   files the copy in `Sent Items`; see [`crate::ews::EwsClient::send_mime`],
//! * a **GroupWise proxy** account still submits through the SMTP server
//!   configured on the account: the proxy's own `sendItem` is not implemented,
//!   and a proxy mailbox that has an SMTP server configured keeps working.
//!
//! The bytes are never re-composed here.  [`crate::compose::ComposedMessage`]
//! is the single source of what leaves the machine, so the message that was
//! shown in the composer and the one that goes out cannot drift apart.
//!
//! Failures are values, and they are classified by [`Error::is_transient`]: a
//! refused connection is worth another attempt, a rejected message is not.

use crate::compose::ComposedMessage;
use crate::error::{Error, Result};
use crate::models::account::{Account, AccountKind};
use crate::net::TlsPolicy;
use crate::secrets::Credential;
use crate::smtp::{self, SmtpConfig};

/// What a server answered about one submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// `true` when the server took the message for delivery.
    pub accepted: bool,
    /// The server's answer, for the outbox entry and the log.
    pub response: String,
    /// The server side id of the Sent copy, when the protocol filed one.
    ///
    /// SMTP leaves this `None` - a relay keeps no copy; Exchange returns the
    /// item id of the copy it stored in `Sent Items`.
    pub sent_copy: Option<String>,
}

impl Receipt {
    /// The answer as it should appear in the outbox entry.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.accepted {
            format!("accepted: {}", self.response)
        } else {
            format!("rejected: {}", self.response)
        }
    }
}

/// Submits one composed message through the account's own protocol.
///
/// `credential` borrows from a secret the caller keeps alive for the duration
/// of the call; it is the outgoing credential (a password or an access token),
/// not the incoming one.
///
/// # Errors
///
/// * [`Error::Config`] when the message has no recipient - checked here for
///   every protocol, so a mis-composed message never opens a socket.
/// * Every login and protocol error of the client that was used; the caller
///   decides about a retry through [`Error::is_transient`].
pub async fn submit(
    account: &Account,
    composed: &ComposedMessage,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<Receipt> {
    if composed.recipients.is_empty() {
        return Err(Error::config(
            "a message needs at least one recipient before it can be sent",
        ));
    }
    match account.kind {
        AccountKind::Exchange => submit_exchange(account, composed, credential, tls).await,
        AccountKind::Imap | AccountKind::GroupWiseProxy => {
            submit_smtp(account, composed, credential, tls).await
        }
    }
}

/// The SMTP path: IMAP and, for now, GroupWise proxy accounts.
async fn submit_smtp(
    account: &Account,
    composed: &ComposedMessage,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<Receipt> {
    let config = SmtpConfig::new(
        account.outgoing.clone(),
        account.username.clone(),
        account.auth,
        tls.clone(),
    );
    let submission = smtp::submit(
        &config,
        credential,
        &composed.from,
        &composed.recipients,
        &composed.raw,
    )
    .await?;
    Ok(Receipt {
        accepted: submission.accepted,
        response: submission.response,
        sent_copy: None,
    })
}

/// The Exchange path: log in, then one `CreateItem` with the finished message.
async fn submit_exchange(
    account: &Account,
    composed: &ComposedMessage,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<Receipt> {
    let mut config = crate::ews::EwsConfig::new(
        account.incoming.clone(),
        account.username.clone(),
        account.auth,
    );
    config.policy = tls.clone();
    let mut client = crate::ews::EwsClient::login(&config, credential).await?;
    let sent_copy = client.send_mime(&composed.raw).await?;
    Ok(Receipt {
        accepted: true,
        response: match &sent_copy {
            Some(id) => format!("EWS CreateItem SendAndSaveCopy, Sent-Items-Id {id}"),
            None => "EWS CreateItem SendAndSaveCopy".to_string(),
        },
        sent_copy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::account::{AuthKind, Endpoint, TlsMode};
    use crate::models::ids::{utc_now, AccountId};
    use secrecy::SecretString;

    fn account(kind: AccountKind) -> Account {
        Account {
            id: AccountId::new(),
            display_name: "Ada".to_string(),
            email: "ada@example.org".to_string(),
            kind,
            auth: AuthKind::Password,
            username: "ada".to_string(),
            incoming: Endpoint::new("127.0.0.1", 1, TlsMode::None),
            outgoing: Endpoint::new("127.0.0.1", 1, TlsMode::None),
            proxy: None,
            oauth: None,
            enabled: true,
            created_at: utc_now(),
        }
    }

    fn composed(recipients: &[&str]) -> ComposedMessage {
        ComposedMessage {
            raw: b"From: ada@example.org\r\nSubject: Bericht\r\n\r\nText\r\n".to_vec(),
            from: "ada@example.org".to_string(),
            recipients: recipients.iter().map(|it| (*it).to_string()).collect(),
            message_id: "<1@example.org>".to_string(),
            subject: "Bericht".to_string(),
        }
    }

    /// A port that nothing is listening on.
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn a_message_without_a_recipient_never_opens_a_socket() {
        let secret = SecretString::from("geheim");
        for kind in [
            AccountKind::Imap,
            AccountKind::Exchange,
            AccountKind::GroupWiseProxy,
        ] {
            // Port 1 is reserved and closed, so a dispatch that tried to connect
            // would fail with something else than a configuration error.
            let error = submit(
                &account(kind),
                &composed(&[]),
                Credential::Password(&secret),
                &TlsPolicy::insecure(),
            )
            .await
            .expect_err("a message without a recipient was accepted");
            assert!(
                matches!(error, Error::Config { .. }),
                "{kind:?} reported {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_relay_is_a_transient_failure() {
        let port = closed_port().await;
        let mut account = account(AccountKind::Imap);
        account.outgoing = Endpoint::new("127.0.0.1", port, TlsMode::None);
        let secret = SecretString::from("geheim");
        let error = submit(
            &account,
            &composed(&["bob@example.org"]),
            Credential::Password(&secret),
            &TlsPolicy::insecure(),
        )
        .await
        .expect_err("a closed port accepted the message");
        assert!(error.is_transient(), "{error}");
    }
}
