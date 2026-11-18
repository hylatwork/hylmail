//! Moving a message into another folder on the server it lives on.
//!
//! [`crate::delete`] takes a message out of a mailbox and [`crate::flags`]
//! toggles its read state; this module relocates it.  The backends agree on the
//! outcome, not on the route:
//!
//! * **IMAP** uses `UID MOVE` when the server advertises it.  A server without
//!   `MOVE` gets `UID COPY` plus `\Deleted` and an expunge of the original,
//!   exactly like the delete fallback.
//! * **Exchange** answers `MoveItem` with a *new* item id, so the caller has to
//!   store it; [`MoveOutcome::new_server_id`] carries it.
//! * **GroupWise** moves by folder id through the proxy.
//!
//! Like [`crate::delete`], this module never touches the local row: only the
//! caller knows whether the message was the one on screen.

use crate::error::{Error, Result};
use crate::models::{Account, AccountKind, Folder, MessageHeader};
use crate::net::TlsPolicy;
use crate::secrets::Credential;

/// What the server reported after the move.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveOutcome {
    /// The item id after the move, when the protocol changed it.
    ///
    /// Exchange hands out a new id for the moved item; IMAP and GroupWise keep
    /// addressing it the way they did before.
    pub new_server_id: Option<String>,
}

/// `true` when a message in `folder` can be moved into `target`.
///
/// The folder picker uses this, and [`move_to`] refuses anything else: the same
/// folder (by id *or* by server path, because a re-created row gets a new local
/// id for an unchanged mailbox), another account's folder, and folders the
/// server does not accept mail in.
#[must_use]
pub fn usable_target(folder: &Folder, target: &Folder) -> bool {
    target.id != folder.id
        && target.account_id == folder.account_id
        && !target.path.eq_ignore_ascii_case(&folder.path)
        && target.can_receive()
}

/// Moves `header` from `folder` into `target`.
///
/// # Errors
///
/// * [`Error::Config`] when `target` is not a folder the message can move to.
/// * [`Error::NotFound`] when the header lacks the identifier the protocol
///   needs (an IMAP `UID` or an Exchange / GroupWise item id).
/// * [`Error::Config`] when a GroupWise account has no proxy configuration.
/// * The login, protocol and transport errors of the respective client.
pub async fn move_to(
    account: &Account,
    folder: &Folder,
    target: &Folder,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<MoveOutcome> {
    if !usable_target(folder, target) {
        return Err(Error::config(format!(
            "`{}` is not a destination for a message from `{}`",
            target.path, folder.path
        )));
    }
    match account.kind {
        AccountKind::Imap => move_imap(account, folder, target, header, credential, tls).await,
        AccountKind::Exchange => move_exchange(account, target, header, credential, tls).await,
        AccountKind::GroupWiseProxy => {
            move_groupwise(account, target, header, credential, tls).await
        }
    }
}

/// `UID MOVE`, or `COPY` + `\Deleted` + expunge.
async fn move_imap(
    account: &Account,
    folder: &Folder,
    target: &Folder,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<MoveOutcome> {
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
    let result = async {
        if session.selected() != Some(folder.path.as_str()) {
            session.select(&folder.path).await?;
        }
        let atomic = session.move_message(uid, &target.path).await?;
        if !atomic {
            // The copy is in place, the original is flagged `\Deleted`; the
            // expunge is the caller's job precisely because a plain `EXPUNGE`
            // would also drop what another client flagged.
            session.expunge(Some(uid)).await?;
        }
        Ok(MoveOutcome::default())
    }
    .await;
    session.logout().await;
    result
}

/// `MoveItem`; Exchange renumbers the item.
async fn move_exchange(
    account: &Account,
    target: &Folder,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<MoveOutcome> {
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
    let new_id = client
        .move_item(item_id, &crate::ews::FolderRef::id(&target.path))
        .await?;
    Ok(MoveOutcome {
        new_server_id: Some(new_id),
    })
}

/// `moveItemRequest` through the proxy.
async fn move_groupwise(
    account: &Account,
    target: &Folder,
    header: &MessageHeader,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<MoveOutcome> {
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
    // The server folder id is the `path` of a GroupWise folder, like it is for
    // an EWS folder.
    let result = client
        .move_items(std::slice::from_ref(&item_id), &target.path)
        .await
        .map(|()| MoveOutcome::default());
    let _ = client.logout().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{utc_now, AccountId, AuthKind, Endpoint, FolderId, FolderKind};
    use crate::secrets::secret;

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

    fn folder(name: &str) -> Folder {
        Folder::new(AccountId::new(), name, name, FolderKind::Custom)
    }

    fn header(account: &Account) -> MessageHeader {
        MessageHeader::new(account.id, FolderId::new())
    }

    #[test]
    fn the_same_mailbox_is_never_a_destination() {
        let source = folder("INBOX");
        assert!(!usable_target(&source, &source));
        // A re-created row: the server path matches, the local id does not.
        let mut again = folder("inbox");
        again.account_id = source.account_id;
        assert!(!usable_target(&source, &again));
    }

    #[test]
    fn another_accounts_folder_is_not_a_destination() {
        let source = folder("INBOX");
        let mut other = folder("Archiv");
        other.account_id = AccountId::new();
        assert!(!usable_target(&source, &other));
    }

    #[test]
    fn a_folder_that_takes_no_mail_is_not_a_destination() {
        let source = folder("INBOX");
        let mut target = folder("Kalender");
        target.account_id = source.account_id;
        target.selectable = false;
        assert!(!usable_target(&source, &target));
    }

    #[test]
    fn a_folder_of_the_same_account_is_a_destination() {
        let source = folder("INBOX");
        let mut target = folder("Archiv");
        target.account_id = source.account_id;
        assert!(usable_target(&source, &target));
    }

    #[tokio::test]
    async fn a_folder_of_another_account_is_refused() {
        let account = account(AccountKind::Imap);
        let source = Folder::new(account.id, "INBOX", "INBOX", FolderKind::Inbox);
        let target = folder("Archiv");
        let error = move_to(
            &account,
            &source,
            &target,
            &header(&account),
            Credential::Password(&secret("geheim".to_string())),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
    }

    #[tokio::test]
    async fn a_message_without_an_imap_uid_is_not_found() {
        let account = account(AccountKind::Imap);
        let source = Folder::new(account.id, "INBOX", "INBOX", FolderKind::Inbox);
        let target = Folder::new(account.id, "Archiv", "Archiv", FolderKind::Archive);
        let error = move_to(
            &account,
            &source,
            &target,
            &header(&account),
            Credential::Password(&secret("geheim".to_string())),
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
    async fn a_message_without_an_exchange_item_id_is_not_found() {
        let account = account(AccountKind::Exchange);
        let source = folder("Posteingang");
        let mut target = folder("Archiv");
        target.account_id = source.account_id;
        let error = move_to(
            &account,
            &source,
            &target,
            &header(&account),
            Credential::Password(&secret("geheim".to_string())),
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
    async fn a_groupwise_account_without_a_proxy_is_a_config_error() {
        let account = account(AccountKind::GroupWiseProxy);
        let source = Folder::new(account.id, "Mailbox", "Mailbox", FolderKind::Inbox);
        let target = Folder::new(account.id, "Archive", "Archive", FolderKind::Archive);
        let mut header = header(&account);
        header.server_id = Some("A1".to_string());
        let error = move_to(
            &account,
            &source,
            &target,
            &header,
            Credential::Password(&secret("geheim".to_string())),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
    }
}
