//! On-demand download of a single message body.
//!
//! Synchronisation fetches **headers only**.  That is what makes a first run
//! over a 50 000 message mailbox finish in seconds instead of hours, and it is
//! what the specification asks for.  The body of a message is therefore
//! downloaded the first time somebody actually looks at it, which is what this
//! module does.
//!
//! The module owns the three protocol specific round trips and nothing else:
//! the bytes it returns are handed to [`crate::mime::store_body`], which parses
//! them and writes the content cache.  Keeping the two apart means the network
//! part and the parsing part can be tested independently.
//!
//! # Example
//!
//! ```no_run
//! use hylmail_core::body;
//! use hylmail_core::html::HtmlPolicy;
//! use hylmail_core::models::Account;
//! use hylmail_core::net::TlsPolicy;
//! use hylmail_core::secrets::{Credential, SecretStore};
//!
//! # async fn example(
//! #     database: &hylmail_core::db::Database,
//! #     account: &Account,
//! #     folder: &hylmail_core::models::Folder,
//! #     header: &hylmail_core::models::MessageHeader,
//! #     secrets: &dyn SecretStore,
//! #     password: &secrecy::SecretString,
//! # ) -> hylmail_core::Result<()> {
//! let credential = Credential::Password(password);
//! let body = body::download(
//!     database,
//!     account,
//!     folder,
//!     header,
//!     credential,
//!     &TlsPolicy::strict(),
//!     &HtmlPolicy::default(),
//! )
//! .await?;
//! println!("{} attachment(s)", body.attachments.len());
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;

use crate::db::Database;
use crate::error::{Error, IoResultExt, Result};
use crate::html::HtmlPolicy;
use crate::models::{Account, AccountKind, Folder, MessageBody, MessageHeader};
use crate::net::TlsPolicy;
use crate::secrets::Credential;

/// Downloads the raw RFC 5322 bytes of one message.
///
/// `credential` borrows from a secret the caller keeps alive for the duration
/// of the call; a password never leaves this function.
///
/// # Errors
///
/// * [`Error::NotFound`] when the header lacks the server identifier the
///   protocol needs (an IMAP `UID` or an Exchange / GroupWise item id).
/// * [`Error::Config`] when a GroupWise account has no proxy configuration.
/// * The login and protocol errors of the respective client.
pub async fn fetch_raw(
    account: &Account,
    folder: &Folder,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<Vec<u8>> {
    match account.kind {
        AccountKind::Imap => fetch_imap(account, folder, header, credential, tls).await,
        AccountKind::Exchange => fetch_exchange(account, header, credential, tls).await,
        AccountKind::GroupWiseProxy => fetch_groupwise(account, header, credential, tls).await,
    }
}

/// Downloads a body and writes it into the content cache.
///
/// The owning header row is updated as well, so the message list shows the
/// preview text and the attachment marker as soon as this returns.
///
/// # Errors
///
/// See [`fetch_raw`] plus every parsing and database error of
/// [`crate::mime::store_body`].
pub async fn download(
    database: &Database,
    account: &Account,
    folder: &Folder,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
    policy: &HtmlPolicy,
) -> Result<MessageBody> {
    let raw = fetch_raw(account, folder, header, credential, tls).await?;
    crate::mime::store_body(database, header.id, &raw, policy)
}

/// Writes the requested parts of an already downloaded message to disk.
///
/// `targets` are `(part_id, destination)` pairs taken from
/// [`MessageBody::attachments`] - the identifiers the reading pane showed.  The
/// raw bytes are passed in rather than fetched again: the caller has just
/// downloaded the message, and re-fetching a 20 MB mail because somebody
/// clicked "save" would be rude.
///
/// A message without the named part is an error rather than a silent no-op, so
/// a stale click cannot pretend to have saved something.
///
/// The write itself is synchronous and blocking, which is fine on the worker
/// thread and must not be done by whoever draws the window; a few megabytes to
/// a local disk are not worth a `tokio::fs` round trip.
///
/// # Errors
///
/// * [`Error::NotFound`] when `targets` names a part the message does not have.
/// * [`Error::Io`] when a file cannot be written; the path is preserved.
/// * [`Error::MalformedResponse`] when the bytes are not a message at all.
pub fn save_attachments(raw: &[u8], targets: &[(String, PathBuf)]) -> Result<usize> {
    if targets.is_empty() {
        return Ok(0);
    }
    let payloads = crate::mime::extract_attachments(raw)?;
    let mut written = 0;
    for (part_id, path) in targets {
        let payload = payloads
            .iter()
            .find(|payload| payload.attachment.part_id == *part_id)
            .ok_or_else(|| Error::NotFound {
                entity: "attachment",
                id: part_id.clone(),
            })?;
        std::fs::write(path, &payload.bytes).with_path(path.clone())?;
        written += 1;
    }
    Ok(written)
}

/// The IMAP path: select the folder and fetch `BODY.PEEK[]` for the `UID`.
async fn fetch_imap(
    account: &Account,
    folder: &Folder,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<Vec<u8>> {
    let uid = header.uid.ok_or_else(|| Error::NotFound {
        entity: "IMAP uid",
        id: header.id.to_string(),
    })?;
    let config = crate::imap::connection::ImapConfig::new(
        account.incoming.clone(),
        account.username.clone(),
        account.auth,
        tls.clone(),
    );
    let mut session = crate::imap::connection::ImapSession::login(&config, credential).await?;
    let result = crate::imap::synchronise::fetch_body(&mut session, folder, uid).await;
    session.logout().await;
    result
}

/// The Exchange path: `GetItem` with a MIME content projection.
async fn fetch_exchange(
    account: &Account,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<Vec<u8>> {
    let item_id = header.server_id.as_deref().ok_or_else(|| Error::NotFound {
        entity: "Exchange item id",
        id: header.id.to_string(),
    })?;
    let mut config = crate::ews::EwsConfig::new(
        account.incoming.clone(),
        account.username.clone(),
        account.auth,
    );
    config.policy = tls.clone();
    let mut client = crate::ews::EwsClient::login(&config, credential).await?;
    client.fetch_mime(item_id).await
}

/// The GroupWise path: one proxy session, one `GetItem`, then logout.
async fn fetch_groupwise(
    account: &Account,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<Vec<u8>> {
    let item_id = header.server_id.as_deref().ok_or_else(|| Error::NotFound {
        entity: "GroupWise item id",
        id: header.id.to_string(),
    })?;
    let proxy = account
        .proxy
        .as_ref()
        .ok_or_else(|| Error::config("a GroupWise account needs a proxy configuration"))?;
    let mut config = crate::groupwise::GroupWiseConfig::new(
        proxy.proxy.clone(),
        account.username.clone(),
        proxy,
    );
    config.policy = tls.clone();
    let mut client = crate::groupwise::GroupWiseClient::login(&config, credential).await?;
    let result = client.fetch_mime(item_id).await;
    let _ = client.logout().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        utc_now, AccountId, AccountKind, AuthKind, Endpoint, FolderId, FolderKind, MailAddress,
    };

    fn account(kind: AccountKind) -> Account {
        Account {
            id: AccountId::new(),
            display_name: "Test".to_string(),
            email: "anna@example.org".to_string(),
            kind,
            auth: AuthKind::Password,
            username: "anna".to_string(),
            incoming: Endpoint::imaps("imap.example.org"),
            outgoing: Endpoint::smtps("smtp.example.org"),
            proxy: None,
            oauth: None,
            enabled: true,
            created_at: utc_now(),
        }
    }

    fn header() -> MessageHeader {
        let account_id = AccountId::new();
        let mut header = MessageHeader::new(account_id, FolderId::new());
        header.subject = "Betreff".to_string();
        header.from = vec![MailAddress::new("bob@example.org")];
        header
    }

    fn folder() -> Folder {
        Folder::new(AccountId::new(), "Posteingang", "INBOX", FolderKind::Inbox)
    }

    #[tokio::test]
    async fn an_imap_message_without_a_uid_is_reported_as_missing() {
        let secret = crate::secrets::secret("geheim".to_string());
        let error = fetch_raw(
            &account(AccountKind::Imap),
            &folder(),
            &header(),
            Credential::Password(&secret),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            Error::NotFound {
                entity: "IMAP uid",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn an_exchange_message_without_an_item_id_is_reported_as_missing() {
        let secret = crate::secrets::secret("geheim".to_string());
        let error = fetch_raw(
            &account(AccountKind::Exchange),
            &folder(),
            &header(),
            Credential::Password(&secret),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            Error::NotFound {
                entity: "Exchange item id",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_groupwise_account_without_a_proxy_configuration_is_reported() {
        let secret = crate::secrets::secret("geheim".to_string());
        let mut header = header();
        header.server_id = Some("item-1".to_string());
        let error = fetch_raw(
            &account(AccountKind::GroupWiseProxy),
            &folder(),
            &header,
            Credential::Password(&secret),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
    }

    /// The fixture the whole MIME pipeline is built on.
    const WITH_ATTACHMENT: &[u8] = include_bytes!("../tests/fixtures/eml/attachment.eml");

    fn part_id_of(raw: &[u8], index: usize) -> String {
        crate::mime::parse(raw, crate::models::MessageId::new(), &HtmlPolicy::default())
            .unwrap()
            .body
            .attachments
            .remove(index)
            .part_id
    }

    #[test]
    fn a_saved_attachment_lands_on_disk_byte_for_byte() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("bericht.txt");
        let part_id = part_id_of(WITH_ATTACHMENT, 0);

        let written = save_attachments(WITH_ATTACHMENT, &[(part_id, destination.clone())]).unwrap();

        assert_eq!(written, 1);
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "Bericht in einer Ziele\n"
        );
    }

    #[test]
    fn several_attachments_can_be_saved_in_one_pass() {
        let directory = tempfile::tempdir().unwrap();
        let part_id = part_id_of(WITH_ATTACHMENT, 0);
        let targets = vec![
            (part_id.clone(), directory.path().join("a.txt")),
            (part_id, directory.path().join("b.txt")),
        ];
        assert_eq!(save_attachments(WITH_ATTACHMENT, &targets).unwrap(), 2);
        assert!(directory.path().join("a.txt").exists());
        assert!(directory.path().join("b.txt").exists());
    }

    #[test]
    fn an_unknown_part_is_reported_as_missing() {
        let directory = tempfile::tempdir().unwrap();
        let error = save_attachments(
            WITH_ATTACHMENT,
            &[("99.99".to_string(), directory.path().join("nope.txt"))],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            Error::NotFound {
                entity: "attachment",
                ..
            }
        ));
        assert!(!directory.path().join("nope.txt").exists());
    }

    #[test]
    fn saving_nothing_never_touches_the_parser() {
        assert_eq!(save_attachments(b"not a message", &[]).unwrap(), 0);
    }

    #[test]
    fn a_write_failure_keeps_the_path() {
        let directory = tempfile::tempdir().unwrap();
        // A directory can never be replaced by `std::fs::write`.
        let destination = directory.path().to_path_buf();
        let part_id = part_id_of(WITH_ATTACHMENT, 0);
        let error =
            save_attachments(WITH_ATTACHMENT, &[(part_id, destination.clone())]).unwrap_err();
        match error {
            Error::Io { path, .. } => assert_eq!(path, destination),
            other => panic!("expected an i/o error, got {other:?}"),
        }
    }
}
