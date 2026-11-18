//! Pushing a flag change to the server the message lives on.
//!
//! [`crate::body`] downloads a body on demand; this is its mirror image.  When
//! the reading pane marks a message read, or the list toggles a flag, the
//! change must reach the server too, otherwise the next synchronisation
//! overwrites it.
//!
//! Only flags a backend can actually express are pushed.  `\Seen`, `\Flagged`,
//! `\Answered` and `$Done` are pushed for IMAP; Exchange carries read, flagged
//! and done as item state; the GroupWise proxy keeps its marks local on purpose
//! (`BR-GW-1`).

use crate::error::{Error, Result};
use crate::models::{Account, AccountKind, Folder, MessageFlags, MessageHeader};
use crate::net::TlsPolicy;
use crate::secrets::Credential;

/// The subset of [`MessageFlags`] a server can be told about.
#[must_use]
pub fn remotely_expressible(flags: MessageFlags) -> MessageFlags {
    let mut wanted = MessageFlags::empty();
    for flag in [
        MessageFlags::SEEN,
        MessageFlags::FLAGGED,
        MessageFlags::ANSWERED,
        MessageFlags::DONE,
    ] {
        if flags.contains(flag) {
            wanted.insert(flag);
        }
    }
    wanted
}

/// Pushes `desired` to the server for one message.
///
/// Flags outside [`remotely_expressible`] are ignored rather than rejected, so
/// the caller can pass a whole [`MessageFlags`] value.
///
/// # Errors
///
/// * [`Error::NotFound`] when the header lacks the identifier the protocol
///   needs (an IMAP `UID` or an Exchange / GroupWise item id).
/// * [`Error::Config`] when a GroupWise account has no proxy configuration.
/// * The login and protocol errors of the respective client.
pub async fn push(
    account: &Account,
    folder: &Folder,
    header: &MessageHeader,
    desired: MessageFlags,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<()> {
    let wanted = remotely_expressible(desired);
    if wanted.is_empty() {
        return Ok(());
    }
    match account.kind {
        AccountKind::Imap => push_imap(account, folder, header, wanted, credential, tls).await,
        AccountKind::Exchange => push_exchange(account, header, wanted, credential, tls).await,
        AccountKind::GroupWiseProxy => {
            push_groupwise(account, header, wanted, credential, tls).await
        }
    }
}

/// `UID STORE` on the folder that holds the message.
async fn push_imap(
    account: &Account,
    folder: &Folder,
    header: &MessageHeader,
    wanted: MessageFlags,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<()> {
    if header.uid.is_none() {
        return Err(Error::NotFound {
            entity: "IMAP uid",
            id: header.id.to_string(),
        });
    }
    let config = crate::imap::connection::ImapConfig::new(
        account.incoming.clone(),
        account.username.clone(),
        account.auth,
        tls.clone(),
    );
    let mut session = crate::imap::connection::ImapSession::login(&config, credential).await?;
    let result =
        crate::imap::synchronise::apply_local_flags(&mut session, folder, header, wanted).await;
    session.logout().await;
    result.map(|_changed| ())
}

/// `UpdateItem` with the read and flagged extended properties.
async fn push_exchange(
    account: &Account,
    header: &MessageHeader,
    wanted: MessageFlags,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<()> {
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
    if wanted.contains(MessageFlags::SEEN) {
        client.set_read(item_id, true).await?;
    }
    if wanted.contains(MessageFlags::DONE) {
        // "Done" is the Exchange item status; it subsumes a plain flag, so it is
        // set last and the flag is not sent separately.
        client.set_done(item_id, true).await?;
    } else if wanted.contains(MessageFlags::FLAGGED) {
        client.set_flag(item_id, true).await?;
    }
    Ok(())
}

/// One proxy session, one `MarkItems` call, then logout.
async fn push_groupwise(
    account: &Account,
    header: &MessageHeader,
    wanted: MessageFlags,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<()> {
    let item_id = header.server_id.clone().ok_or_else(|| Error::NotFound {
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
    let result = if wanted.contains(MessageFlags::SEEN) {
        client.set_read(&[item_id], true).await
    } else {
        Ok(())
    };
    let _ = client.logout().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{utc_now, AccountId, AuthKind, Endpoint, FolderId, FolderKind};

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
        MessageHeader::new(AccountId::new(), FolderId::new())
    }

    fn folder() -> Folder {
        Folder::new(AccountId::new(), "Posteingang", "INBOX", FolderKind::Inbox)
    }

    #[test]
    fn remote_flags_are_kept_and_the_thread_marker_is_dropped() {
        let wanted = remotely_expressible(
            MessageFlags::SEEN
                | MessageFlags::FLAGGED
                | MessageFlags::ANSWERED
                | MessageFlags::DONE
                | MessageFlags::THREAD_UNREAD,
        );
        assert_eq!(
            wanted,
            MessageFlags::SEEN
                | MessageFlags::FLAGGED
                | MessageFlags::ANSWERED
                | MessageFlags::DONE
        );
    }

    #[test]
    fn a_note_without_remote_flags_needs_no_connection() {
        let flags = MessageFlags::THREAD_UNREAD;
        assert!(remotely_expressible(flags).is_empty());
    }

    #[tokio::test]
    async fn nothing_is_pushed_when_only_local_flags_change() {
        let secret = crate::secrets::secret("geheim".to_string());
        // No server is reachable; the call must return before dialling.
        push(
            &account(AccountKind::Imap),
            &folder(),
            &header(),
            MessageFlags::empty(),
            Credential::Password(&secret),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_imap_message_without_a_uid_is_reported_as_missing() {
        let secret = crate::secrets::secret("geheim".to_string());
        let error = push(
            &account(AccountKind::Imap),
            &folder(),
            &header(),
            MessageFlags::SEEN,
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
        let error = push(
            &account(AccountKind::Exchange),
            &folder(),
            &header(),
            MessageFlags::SEEN,
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
        let error = push(
            &account(AccountKind::GroupWiseProxy),
            &folder(),
            &header,
            MessageFlags::SEEN,
            Credential::Password(&secret),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
    }
}
