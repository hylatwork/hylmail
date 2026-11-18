//! Deleting a message on the server it lives on.
//!
//! [`crate::body`] downloads a message and [`crate::flags`] toggles its read
//! state; this module removes it.  The three backends agree on the outcome but
//! not on the route:
//!
//! * **IMAP** moves the message into the account's trash mailbox when one is
//!   known and the server advertises `MOVE`.  Without `MOVE` the message is
//!   copied and flagged `\Deleted`, and the original is then expunged; without
//!   a trash folder the message is only flagged and expunged, which is final.
//! * **Exchange** uses `DeleteItem`.  A soft delete lands in `Deleted Items`
//!   without the client naming the folder, a hard one is final.
//! * **GroupWise** mirrors IMAP: `moveItemRequest` into the trash folder, or
//!   `deleteItemRequest`.
//!
//! The local row is deliberately *not* touched here.  The caller decides what
//! the database should show, because only it knows whether the message was the
//! one on screen.

use crate::error::{Error, Result};
use crate::models::{Account, AccountKind, Folder, MessageFlags, MessageHeader};
use crate::net::TlsPolicy;
use crate::secrets::Credential;

/// Where the user wants the message to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Move it into the trash folder, like the `Delete` key of every other
    /// mail client.  Falls back to a permanent delete when the account has no
    /// trash folder.
    ToTrash,
    /// Remove it from the server without a stop in the trash.
    Permanently,
}

/// What the server did with the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The message is in the trash folder now.
    MovedToTrash,
    /// The message is gone from the mailbox it was in.
    Removed,
}

impl DeleteOutcome {
    /// The outcome for the status bar.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MovedToTrash => "in den Papierkorb verschoben",
            Self::Removed => "gelöscht",
        }
    }
}

/// Deletes `header` on the server `account` describes.
///
/// `trash` is the trash folder of the same account, when the database knows
/// one.  It is only consulted for [`DeleteMode::ToTrash`] and ignored when it
/// *is* the folder the message lives in, because deleting from the trash is
/// final everywhere.
///
/// # Errors
///
/// * [`Error::NotFound`] when the header lacks the identifier the protocol
///   needs (an IMAP `UID` or an Exchange / GroupWise item id).
/// * [`Error::Config`] when a GroupWise account has no proxy configuration.
/// * The login, protocol and transport errors of the respective client.
pub async fn delete(
    account: &Account,
    folder: &Folder,
    trash: Option<&Folder>,
    header: &MessageHeader,
    mode: DeleteMode,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<DeleteOutcome> {
    let trash = usable_trash(folder, trash);
    match account.kind {
        AccountKind::Imap => {
            delete_imap(account, folder, trash, header, mode, credential, tls).await
        }
        AccountKind::Exchange => delete_exchange(account, header, mode, credential, tls).await,
        AccountKind::GroupWiseProxy => {
            delete_groupwise(account, trash, header, mode, credential, tls).await
        }
    }
}

/// The trash folder to use, or `None` when it would not change anything.
///
/// Moving a message into the folder it is already in is not a delete, and a
/// folder the server does not accept mail in is not a destination.  The path is
/// compared as well as the id, because a re-created folder gets a new local id
/// while the server keeps the same mailbox.
fn usable_trash<'a>(folder: &Folder, trash: Option<&'a Folder>) -> Option<&'a Folder> {
    trash.filter(|trash| {
        trash.id != folder.id
            && trash.account_id == folder.account_id
            && !trash.path.eq_ignore_ascii_case(&folder.path)
            && trash.can_receive()
    })
}

/// `MOVE` into the trash, or `\Deleted` plus `EXPUNGE`.
async fn delete_imap(
    account: &Account,
    folder: &Folder,
    trash: Option<&Folder>,
    header: &MessageHeader,
    mode: DeleteMode,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<DeleteOutcome> {
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
    let result = delete_imap_session(&mut session, folder, trash, header, uid, mode).await;
    session.logout().await;
    result
}

/// The part of [`delete_imap`] that holds a logged-in session.
async fn delete_imap_session(
    session: &mut crate::imap::connection::ImapSession,
    folder: &Folder,
    trash: Option<&Folder>,
    header: &MessageHeader,
    uid: u32,
    mode: DeleteMode,
) -> Result<DeleteOutcome> {
    if session.selected() != Some(folder.path.as_str()) {
        session.select(&folder.path).await?;
    }

    if mode == DeleteMode::ToTrash {
        if let Some(trash) = trash {
            let atomic = session.move_message(uid, &trash.path).await?;
            if !atomic {
                // `COPY` plus `\Deleted`: the copy is in the trash, the
                // original still needs the expunge the move could not do.
                session.expunge(Some(uid)).await?;
            }
            return Ok(DeleteOutcome::MovedToTrash);
        }
    }

    // `desired` is the whole flag set, so the flags the message already
    // carries have to be repeated - a bare `DELETED` would clear `\Seen` and
    // `\Flagged` with the same `STORE`.
    let desired = header.flags | MessageFlags::DELETED;
    session
        .store_flags(uid, desired, header.flags, header.local_flags)
        .await?;
    session.expunge(Some(uid)).await?;
    Ok(DeleteOutcome::Removed)
}

/// `DeleteItem`, soft for the trash and hard for a final delete.
async fn delete_exchange(
    account: &Account,
    header: &MessageHeader,
    mode: DeleteMode,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<DeleteOutcome> {
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
    let hard = mode == DeleteMode::Permanently;
    client.delete_item(item_id, hard).await?;
    Ok(if hard {
        DeleteOutcome::Removed
    } else {
        DeleteOutcome::MovedToTrash
    })
}

/// `moveItemRequest` into the trash, or `deleteItemRequest`.
async fn delete_groupwise(
    account: &Account,
    trash: Option<&Folder>,
    header: &MessageHeader,
    mode: DeleteMode,
    credential: Credential<'_>,
    tls: &TlsPolicy,
) -> Result<DeleteOutcome> {
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
    let result = match (mode, trash) {
        // The server folder id is the `path` of a GroupWise folder, like it is
        // for an EWS folder.
        (DeleteMode::ToTrash, Some(trash)) => client
            .move_items(std::slice::from_ref(&item_id), &trash.path)
            .await
            .map(|()| DeleteOutcome::MovedToTrash),
        _ => client
            .delete_items(std::slice::from_ref(&item_id))
            .await
            .map(|()| DeleteOutcome::Removed),
    };
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

    fn folder(kind: FolderKind, path: &str) -> Folder {
        Folder::new(AccountId::new(), path, path, kind)
    }

    fn header(account: &Account) -> MessageHeader {
        MessageHeader::new(account.id, FolderId::new())
    }

    #[test]
    fn the_outcome_has_a_german_label() {
        assert_eq!(
            DeleteOutcome::MovedToTrash.label(),
            "in den Papierkorb verschoben"
        );
        assert_eq!(DeleteOutcome::Removed.label(), "gelöscht");
    }

    #[test]
    fn the_message_folder_is_never_its_own_trash() {
        let mut folder = folder(FolderKind::Trash, "Papierkorb");
        folder.account_id = AccountId::new();
        // The same row: deleting from the trash is permanent.
        assert!(usable_trash(&folder, Some(&folder)).is_none());
    }

    #[test]
    fn a_trash_folder_of_another_account_is_ignored() {
        let source = folder(FolderKind::Inbox, "INBOX");
        let mut other = folder(FolderKind::Trash, "Papierkorb");
        other.account_id = AccountId::new();
        assert!(usable_trash(&source, Some(&other)).is_none());
    }

    #[test]
    fn the_same_path_under_a_new_id_is_still_the_same_mailbox() {
        let source = folder(FolderKind::Inbox, "INBOX");
        // A re-created folder: the server path matches, the local row does not.
        let mut trash = folder(FolderKind::Trash, "inbox");
        trash.account_id = source.account_id;
        assert!(usable_trash(&source, Some(&trash)).is_none());
    }

    #[test]
    fn a_folder_that_takes_no_mail_is_not_a_trash() {
        let source = folder(FolderKind::Inbox, "INBOX");
        let mut trash = folder(FolderKind::Trash, "Papierkorb");
        trash.account_id = source.account_id;
        trash.selectable = false;
        assert!(usable_trash(&source, Some(&trash)).is_none());
    }

    #[test]
    fn another_folder_of_the_same_account_is_used() {
        let source = folder(FolderKind::Inbox, "INBOX");
        let mut trash = folder(FolderKind::Trash, "Papierkorb");
        trash.account_id = source.account_id;
        assert_eq!(
            usable_trash(&source, Some(&trash)).map(|trash| trash.path.as_str()),
            Some("Papierkorb")
        );
    }

    #[tokio::test]
    async fn a_message_without_an_imap_uid_is_not_found() {
        let account = account(AccountKind::Imap);
        let error = delete(
            &account,
            &folder(FolderKind::Inbox, "INBOX"),
            None,
            &header(&account),
            DeleteMode::ToTrash,
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
        let error = delete(
            &account,
            &folder(FolderKind::Inbox, "Posteingang"),
            None,
            &header(&account),
            DeleteMode::Permanently,
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
    async fn a_message_without_a_groupwise_item_id_is_not_found() {
        let account = account(AccountKind::GroupWiseProxy);
        let error = delete(
            &account,
            &folder(FolderKind::Inbox, "Mailbox"),
            None,
            &header(&account),
            DeleteMode::ToTrash,
            Credential::Password(&secret("geheim".to_string())),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            Error::NotFound {
                entity: "GroupWise item id",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_groupwise_account_without_a_proxy_is_a_config_error() {
        let account = account(AccountKind::GroupWiseProxy);
        let mut header = header(&account);
        header.server_id = Some("A1".to_string());
        let error = delete(
            &account,
            &folder(FolderKind::Inbox, "Mailbox"),
            None,
            &header,
            DeleteMode::ToTrash,
            Credential::Password(&secret("geheim".to_string())),
            &TlsPolicy::strict(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
    }
}
