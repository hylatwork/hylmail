//! The Exchange account driver inside the sync engine.
//!
//! `tests/ews_sync.rs` proves the client; this test proves that the engine wires
//! the client up: that an `AccountKind::Exchange` account gets a task, emits the
//! same events an IMAP account does, stores its folders and messages in the
//! database it was given, and stops when it is asked to.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use hylmail_core::db::{folders, messages, Database};
use hylmail_core::models::account::{Account, AccountKind, AuthKind, Endpoint, TlsMode};
use hylmail_core::models::ids::AccountId;
use hylmail_core::models::message::MessageFlags;
use hylmail_core::models::Settings;
use hylmail_core::paths::{MapEnv, Os, PORTABLE_ENV};
use hylmail_core::sync::{EngineConfig, EngineEvent, SyncEngine};
use hylmail_core::{
    AccountsDocument, AppPaths, ConfigStore, CredentialKey, CredentialKind, InMemorySecretStore,
    SecretStore,
};
use secrecy::SecretString;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// a minimal Exchange server: enough SOAP to satisfy one engine round
// ---------------------------------------------------------------------------

/// Serves requests until the client goes away.
async fn start() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = serve(stream).await;
            });
        }
    });
    port
}

async fn serve(stream: TcpStream) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await? == 0 {
            return Ok(());
        }
        if request_line.trim().is_empty() {
            continue;
        }
        let mut content_length = 0usize;
        let mut action = String::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).await? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else {
                continue;
            };
            match name.to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "soapaction" => action = value.trim().trim_matches('"').to_string(),
                _ => {}
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await?;
        let body = String::from_utf8_lossy(&body).to_string();

        let payload = if action.ends_with("GetFolder") {
            message(
                "GetFolder",
                "<m:ResponseCode>NoError</m:ResponseCode><m:RootFolder>\
                 <t:Folders><t:Folder><t:FolderId Id=\"AAMkINBOX\"/>\
                 <t:DisplayName>Posteingang</t:DisplayName></t:Folder></t:Folders>\
                 </m:RootFolder>",
            )
        } else if action.ends_with("FindFolder") {
            // The mailbox has exactly one folder, and it is the inbox.
            message(
                "FindFolder",
                "<m:ResponseCode>NoError</m:ResponseCode><m:RootFolder \
                 IncludesLastItemInRange=\"true\"><t:Folders>\
                 <t:Folder><t:FolderId Id=\"AAMkINBOX\"/>\
                 <t:DistinguishedFolderId Id=\"inbox\"/>\
                 <t:FolderClass>IPF.Note</t:FolderClass>\
                 <t:DisplayName>Posteingang</t:DisplayName><t:TotalCount>2</t:TotalCount>\
                 <t:UnreadCount>1</t:UnreadCount><t:ChildFolderCount>0</t:ChildFolderCount>\
                 </t:Folder></t:Folders></m:RootFolder>",
            )
        } else if action.ends_with("SyncFolderItems") {
            let state = if body.contains("<m:SyncState>cursor-1</m:SyncState>") {
                "<m:SyncState>cursor-2</m:SyncState>"
            } else {
                "<m:SyncState>cursor-1</m:SyncState>"
            };
            message(
                "SyncFolderItems",
                &format!(
                    "<m:ResponseCode>NoError</m:ResponseCode>{state}\
                     <m:IncludesLastItemInRange>true</m:IncludesLastItemInRange><m:Changes>\
                     <m:Create><t:Message><t:ItemId Id=\"AAMkMSG1\"/>\
                     <t:ItemClass>IPM.Note</t:ItemClass><t:Subject>Rechnung 4711</t:Subject>\
                     <t:DateTimeReceived>2026-11-12T13:00:05Z</t:DateTimeReceived>\
                     <t:IsRead>true</t:IsRead><t:Size>2048</t:Size></t:Message></m:Create>\
                     <m:Create><t:Message><t:ItemId Id=\"AAMkMSG2\"/>\
                     <t:ItemClass>IPM.Note</t:ItemClass><t:Subject>Angebot</t:Subject>\
                     <t:DateTimeReceived>2026-11-12T13:05:00Z</t:DateTimeReceived>\
                     <t:IsRead>false</t:IsRead><t:Size>1024</t:Size></t:Message></m:Create>\
                     </m:Changes>"
                ),
            )
        } else {
            panic!("the engine sent an unexpected operation: {action}");
        };

        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=utf-8\r\nContent-Length: {}\r\n\
             Connection: keep-alive\r\n\r\n",
            payload.len()
        );
        write_half.write_all(head.as_bytes()).await?;
        write_half.write_all(payload.as_bytes()).await?;
        write_half.flush().await?;
    }
}

/// Wraps one response message into a SOAP envelope with a version header.
fn message(action: &str, content: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
         xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\">\
         <s:Header><t:ServerVersionInfo Version=\"15.01.2507.037\"/></s:Header><s:Body>\
         <m:{action}Response><m:ResponseMessages>\
         <m:{action}ResponseMessage ResponseClass=\"Success\">{content}\
         </m:{action}ResponseMessage></m:ResponseMessages></m:{action}Response>\
         </s:Body></s:Envelope>"
    )
}

// ---------------------------------------------------------------------------
// the test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_engine_synchronises_an_exchange_account() {
    let port = start().await;

    // A portable home inside a temporary directory: the engine reads its account
    // list from disk, exactly like the real application does.
    let home = tempfile::tempdir().unwrap();
    let env = MapEnv::new().with(PORTABLE_ENV, home.path().to_string_lossy().to_string());
    let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
    let store = ConfigStore::new(paths.clone());
    store.ensure_dirs().unwrap();

    let account_id = AccountId::new();
    let account = Account {
        id: account_id,
        display_name: "Ada (Exchange)".to_string(),
        email: "ada@example.org".to_string(),
        kind: AccountKind::Exchange,
        auth: AuthKind::Password,
        username: "example\\ada".to_string(),
        incoming: Endpoint::new("127.0.0.1", port, TlsMode::None),
        outgoing: Endpoint::new("127.0.0.1", port, TlsMode::None),
        proxy: None,
        oauth: None,
        enabled: true,
        created_at: hylmail_core::models::ids::utc_now(),
    };
    account.validate().unwrap();
    store
        .save_accounts(&AccountsDocument {
            accounts: vec![account.clone()],
        })
        .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    secrets
        .set(
            &CredentialKey::new(account_id, CredentialKind::IncomingPassword),
            &SecretString::from("geheim"),
        )
        .unwrap();

    let database = Arc::new(Database::open_in_memory_migrated().unwrap());
    let config = EngineConfig::new(
        paths,
        Arc::clone(&database),
        secrets,
        Settings::sanitized_default(),
    );
    let (mut handle, task) = SyncEngine::spawn(config);

    // Collect events until the folder has been synchronised; the driver then
    // waits for the poll interval, which is where the round ends.
    let mut connected = false;
    let mut discovered = false;
    let mut synced = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, handle.recv()).await {
        match event {
            EngineEvent::Connected {
                host, encrypted, ..
            } => {
                assert_eq!(host, "127.0.0.1");
                assert!(!encrypted, "the fake server speaks cleartext");
                connected = true;
            }
            EngineEvent::FoldersDiscovered { count, .. } => {
                assert_eq!(count, 1);
                discovered = true;
            }
            EngineEvent::FolderSynced { report, .. } => {
                synced += report.new_messages;
                if synced >= 2 && discovered {
                    break;
                }
            }
            EngineEvent::Disconnected { reason, .. } => {
                panic!("the engine lost the connection: {reason}")
            }
            _ => {}
        }
    }
    assert!(connected, "no `Connected` event arrived");
    assert!(discovered, "no `FoldersDiscovered` event arrived");
    assert_eq!(synced, 2, "the two Exchange items were not stored");

    // The account really is in the database the engine was given.
    let inbox = database
        .read(|connection| {
            let folders = folders::list_folders(connection, account_id)?;
            let inbox = folders
                .into_iter()
                .find(|folder| folder.name == "Posteingang")
                .expect("the Exchange inbox was stored");
            let stored = messages::list_messages(
                connection,
                &hylmail_core::models::message::MessageQuery {
                    folder_ids: vec![inbox.id],
                    limit: None,
                    ..Default::default()
                },
            )?;
            Ok((inbox, stored))
        })
        .unwrap();
    let (inbox, stored) = inbox;
    assert_eq!(inbox.kind, hylmail_core::models::FolderKind::Inbox);
    // The server folder id is the identity of the row.
    assert_eq!(inbox.path, "AAMkINBOX");
    assert_eq!(stored.len(), 2);
    let read = stored
        .iter()
        .find(|header| header.server_id.as_deref() == Some("AAMkMSG1"))
        .unwrap();
    assert!(read.flags.contains(MessageFlags::SEEN));
    let unread = stored
        .iter()
        .find(|header| header.server_id.as_deref() == Some("AAMkMSG2"))
        .unwrap();
    assert!(!unread.flags.contains(MessageFlags::SEEN));

    // Shutting down ends the driver and joins its task.
    handle.shutdown().unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the engine did not stop")
        .unwrap();
}

/// The account dialog saves an account *after* the engine started; the engine
/// must pick it up on `AccountsChanged` and synchronise it at once, without the
/// application being restarted (`AC-1`).
#[tokio::test]
async fn the_engine_picks_up_an_account_saved_after_startup() {
    // A portable home with an *empty* account list: the engine starts with no
    // account tasks at all.
    let home = tempfile::tempdir().unwrap();
    let env = MapEnv::new().with(PORTABLE_ENV, home.path().to_string_lossy().to_string());
    let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
    let store = ConfigStore::new(paths.clone());
    store.ensure_dirs().unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    let database = Arc::new(Database::open_in_memory_migrated().unwrap());
    let config = EngineConfig::new(
        paths.clone(),
        Arc::clone(&database),
        Arc::clone(&secrets),
        Settings::sanitized_default(),
    );
    let (mut handle, task) = SyncEngine::spawn(config);

    // Now the user creates the account: write it and tell the engine, exactly
    // like the account dialog does.
    let port = start().await;
    let account_id = AccountId::new();
    let account = Account {
        id: account_id,
        display_name: "Ada (Exchange)".to_string(),
        email: "ada@example.org".to_string(),
        kind: AccountKind::Exchange,
        auth: AuthKind::Password,
        username: "example\\ada".to_string(),
        incoming: Endpoint::new("127.0.0.1", port, TlsMode::None),
        outgoing: Endpoint::new("127.0.0.1", port, TlsMode::None),
        proxy: None,
        oauth: None,
        enabled: true,
        created_at: hylmail_core::models::ids::utc_now(),
    };
    account.validate().unwrap();
    store
        .save_accounts(&AccountsDocument {
            accounts: vec![account.clone()],
        })
        .unwrap();
    secrets
        .set(
            &CredentialKey::new(account_id, CredentialKind::IncomingPassword),
            &SecretString::from("geheim"),
        )
        .unwrap();
    handle.accounts_changed().unwrap();

    let mut connected = false;
    let mut discovered = false;
    let mut synced = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, handle.recv()).await {
        match event {
            EngineEvent::Connected { .. } => connected = true,
            EngineEvent::FoldersDiscovered { count, .. } => {
                assert_eq!(count, 1);
                discovered = true;
            }
            EngineEvent::FolderSynced { report, .. } => {
                synced += report.new_messages;
                if synced >= 2 && discovered {
                    break;
                }
            }
            EngineEvent::Disconnected { reason, .. } => {
                panic!("a freshly saved account was not synchronised: {reason}")
            }
            _ => {}
        }
    }
    assert!(
        connected,
        "no `Connected` event after the account was saved"
    );
    assert!(discovered, "the folders of the new account were not read");
    assert_eq!(synced, 2, "the new account's mail was not stored");

    handle.shutdown().unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the engine did not stop")
        .unwrap();
}
