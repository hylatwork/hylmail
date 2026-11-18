//! Integration tests for the EWS client against a scripted SOAP server.
//!
//! The server is a hand written HTTP/1.1 endpoint: it reads a request line, the
//! headers and the body, remembers them and answers from a script.  That keeps
//! the tests offline, deterministic and — more importantly — able to assert what
//! the client *sent*, which is where most EWS bugs live (`RequestServerVersion`
//! missing, `SyncState` not sent back, `MaxChangesReturned` too small).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use hylmail_core::compose::ComposedMessage;
use hylmail_core::db::{folders, messages, Database};
use hylmail_core::delete::{self, DeleteMode, DeleteOutcome};
use hylmail_core::ews::{sync_folder, EwsClient, EwsConfig, FolderRef};
use hylmail_core::imap::synchronise::{SyncDepth, SyncOptions};
use hylmail_core::models::account::{Account, AccountKind, AuthKind, Endpoint, TlsMode};
use hylmail_core::models::folder::{Folder, FolderKind};
use hylmail_core::models::ids::AccountId;
use hylmail_core::models::message::{MessageFlags, MessageHeader, MessageQuery};
use hylmail_core::net::{Timeouts, TlsPolicy};
use hylmail_core::relocate::{self, MoveOutcome};
use hylmail_core::secrets::Credential;
use hylmail_core::send;
use hylmail_core::Error;
use parking_lot::Mutex;
use secrecy::SecretString;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// the scripted server
// ---------------------------------------------------------------------------

/// One request the client sent.
#[derive(Debug, Clone)]
struct Request {
    /// The `SOAPAction` header, with the quotes removed.
    action: String,
    /// The `Authorization` header, if the client sent one.
    authorization: Option<String>,
    /// The SOAP body.
    body: String,
}

impl Request {
    /// The last path segment of the action, e.g. `FindFolder`.
    fn operation(&self) -> &str {
        self.action.rsplit('/').next().unwrap_or(&self.action)
    }
}

/// What the server should answer.
#[derive(Debug, Clone, Default)]
struct Script {
    /// Answer `401` to every request.
    reject_login: bool,
    /// Answer with a SOAP `ErrorInvalidCredentials` fault instead of an HTTP 401.
    soap_auth_fault: bool,
    /// Answer with a generic SOAP fault.
    soap_fault: bool,
    /// Refuse every `CreateItem` with `ErrorSendAsDenied`.
    send_denied: bool,
    /// Ask for NTLM credentials: every request without a type 3 message is
    /// answered with `401` and a type 2 challenge.
    ntlm: bool,
}

/// A running fake server.
struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
    /// How many TCP connections the client opened, which is what the NTLM test
    /// asserts on.
    connections: Arc<AtomicUsize>,
}

impl Server {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(script);
        let captured = Arc::clone(&requests);
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let script = Arc::clone(&script);
                let captured = Arc::clone(&captured);
                tokio::spawn(async move {
                    let _ = serve(stream, script, captured).await;
                });
            }
        });
        Self {
            port,
            requests,
            connections,
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new("127.0.0.1", self.port, TlsMode::None)
    }

    /// How many connections the client opened.
    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().clone()
    }

    fn bodies(&self) -> String {
        self.requests()
            .iter()
            .map(|request| request.body.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Reads requests until the client closes the connection.
async fn serve(
    stream: TcpStream,
    script: Arc<Script>,
    captured: Arc<Mutex<Vec<Request>>>,
) -> std::io::Result<()> {
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
        let mut authorization = None;
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
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.parse().unwrap_or(0),
                "soapaction" => action = value.trim_matches('"').to_string(),
                "authorization" => authorization = Some(value.to_string()),
                _ => {}
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await?;
        let body = String::from_utf8_lossy(&body).to_string();
        let request = Request {
            action,
            authorization,
            body,
        };
        // The NTLM handshake, when this script asks for one, decides the answer
        // before the operation itself does.
        let (status, reason, payload, challenge) = match script.ntlm_handshake(&request) {
            Some(answer) => answer,
            None => {
                let (status, reason, payload) = script.respond(&request);
                (status, reason, payload, None)
            }
        };
        captured.lock().push(request);

        let mut head =
            format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/xml; charset=utf-8\r\n");
        if let Some(value) = challenge {
            head.push_str("WWW-Authenticate: ");
            head.push_str(&value);
            head.push_str("\r\n");
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            payload.len()
        ));
        write_half.write_all(head.as_bytes()).await?;
        write_half.write_all(payload.as_bytes()).await?;
        write_half.flush().await?;
    }
}

/// The type 2 challenge the scripted server hands out.
///
/// `NTLMSSP`, message type 2, an empty target name, the flags a Windows server
/// offers (`UNICODE | NTLM | ALWAYS_SIGN | EXTENDED_SESSIONSECURITY | 128 | 56`),
/// the server challenge of the published `MS-NLMP` example and an empty target
/// info list - the shortest challenge a server can send.
const NTLM_CHALLENGE: &str = "TlRMTVNTUAACAAAAAAAAAAAAAAABggigpPG6pnzcGhIAAAAAAAAAAAAAAAAwAAAA";

impl Script {
    /// Answers the NTLM leg of the handshake, if the script wants one.
    ///
    /// A request that already carries a type 3 message is left to
    /// [`Script::respond`]: the handshake is over and the operation itself is
    /// answered.
    fn ntlm_handshake(
        &self,
        request: &Request,
    ) -> Option<(u16, &'static str, String, Option<String>)> {
        if !self.ntlm {
            return None;
        }
        if ntlm_message_type(request.authorization.as_deref()) == Some(3) {
            return None;
        }
        Some((
            401,
            "Unauthorized",
            "<html><body>NTLM required</body></html>".to_string(),
            Some(format!("NTLM {NTLM_CHALLENGE}")),
        ))
    }
}

impl Script {
    fn respond(&self, request: &Request) -> (u16, &'static str, String) {
        if self.reject_login {
            let body = "<?xml version=\"1.0\"?><html><body>Unauthorized</body></html>".to_string();
            return (401, "Unauthorized", body);
        }
        if self.soap_fault {
            return (
                500,
                "Internal Server Error",
                fault("soap:Server", "Der Server ist beschäftigt", None),
            );
        }
        if self.soap_auth_fault {
            return (
                200,
                "OK",
                fault(
                    "soap:Sender",
                    "Zugriff verweigert",
                    Some("ErrorInvalidCredentials"),
                ),
            );
        }
        match request.operation() {
            "GetFolder" => (
                200,
                "OK",
                wrap(
                    "GetFolder",
                    &folders_message("GetFolder", &[inbox_folder()]),
                ),
            ),
            "FindFolder" => {
                let folders = if request.body.contains("msgfolderroot") {
                    vec![inbox_folder(), projects_folder()]
                } else if request.body.contains("AAMkPROJEKTE") {
                    vec![invoices_folder()]
                } else {
                    Vec::new()
                };
                (
                    200,
                    "OK",
                    wrap("FindFolder", &folders_message("FindFolder", &folders)),
                )
            }
            "SyncFolderItems" => {
                let body = if request.body.contains("<m:SyncState>cursor-1</m:SyncState>") {
                    incremental_changes()
                } else {
                    first_changes()
                };
                (200, "OK", wrap("SyncFolderItems", &body))
            }
            "GetItem" => (200, "OK", wrap("GetItem", &mime_message())),
            "UpdateItem" => (
                200,
                "OK",
                wrap(
                    "UpdateItem",
                    "<m:UpdateItemResponseMessage ResponseClass=\"Success\">\
                     <m:ResponseCode>NoError</m:ResponseCode>\
                     </m:UpdateItemResponseMessage>",
                ),
            ),
            "MoveItem" => (
                200,
                "OK",
                wrap(
                    "MoveItem",
                    "<m:MoveItemResponseMessage ResponseClass=\"Success\">\
                     <m:ResponseCode>NoError</m:ResponseCode><m:Items>\
                     <t:Message><t:ItemId Id=\"AAMkMOVED\"/></t:Message>\
                     </m:Items></m:MoveItemResponseMessage>",
                ),
            ),
            "DeleteItem" => (
                200,
                "OK",
                wrap(
                    "DeleteItem",
                    "<m:DeleteItemResponseMessage ResponseClass=\"Success\">\
                     <m:ResponseCode>NoError</m:ResponseCode>\
                     </m:DeleteItemResponseMessage>",
                ),
            ),
            "CreateItem" => {
                if self.send_denied {
                    return (
                        200,
                        "OK",
                        wrap(
                            "CreateItem",
                            "<m:CreateItemResponseMessage ResponseClass=\"Error\">\
                             <m:MessageText>Der Benutzer darf nicht senden</m:MessageText>\
                             <m:ResponseCode>ErrorSendAsDenied</m:ResponseCode>\
                             </m:CreateItemResponseMessage>",
                        ),
                    );
                }
                (
                    200,
                    "OK",
                    wrap(
                        "CreateItem",
                        "<m:CreateItemResponseMessage ResponseClass=\"Success\">\
                         <m:ResponseCode>NoError</m:ResponseCode><m:Items>\
                         <t:Message><t:ItemId Id=\"AAMkSENT\" ChangeKey=\"ck-sent\"/>\
                         </t:Message></m:Items></m:CreateItemResponseMessage>",
                    ),
                )
            }
            other => panic!("the client sent an unexpected operation: {other}"),
        }
    }
}

/// Wraps response messages into an envelope with the version header.
fn wrap(action: &str, messages: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
         xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\">\n\
         <s:Header><t:ServerVersionInfo MajorVersion=\"15\" MinorVersion=\"6\" \
         MajorBuildNumber=\"2507\" Version=\"15.01.2507.037\"/></s:Header>\n\
         <s:Body><m:{action}Response><m:ResponseMessages>{messages}\
         </m:ResponseMessages></m:{action}Response></s:Body></s:Envelope>"
    )
}

/// A SOAP fault, in the shape Exchange sends it.
fn fault(code: &str, message: &str, response_code: Option<&str>) -> String {
    let detail = response_code
        .map(|code| format!("<t:ResponseCode>{code}</t:ResponseCode>"))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
         <s:Body><s:Fault><faultcode>{code}</faultcode>\
         <faultstring>{message}</faultstring>\
         <detail><t:ResponseCode xmlns:t=\"x\">{}</t:ResponseCode></detail>\
         </s:Fault></s:Body></s:Envelope>",
        response_code.unwrap_or("")
    )
    .replace(
        "<detail><t:ResponseCode xmlns:t=\"x\"></t:ResponseCode></detail>",
        &format!("<detail>{detail}</detail>"),
    )
}

fn folders_message(action: &str, folders: &[String]) -> String {
    format!(
        "<m:{action}ResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"{}\" IncludesLastItemInRange=\"true\">\
         <t:Folders>{}</t:Folders></m:RootFolder></m:{action}ResponseMessage>",
        folders.len(),
        folders.join("")
    )
}

fn inbox_folder() -> String {
    "<t:Folder><t:FolderId Id=\"AAMkINBOX\"/>\
     <t:DistinguishedFolderId Id=\"inbox\"/><t:FolderClass>IPF.Note</t:FolderClass>\
     <t:DisplayName>Posteingang</t:DisplayName><t:TotalCount>2</t:TotalCount>\
     <t:UnreadCount>1</t:UnreadCount><t:ChildFolderCount>0</t:ChildFolderCount></t:Folder>"
        .to_string()
}

fn projects_folder() -> String {
    "<t:Folder><t:FolderId Id=\"AAMkPROJEKTE\"/><t:FolderClass>IPF.Note</t:FolderClass>\
     <t:DisplayName>Projekte</t:DisplayName><t:TotalCount>0</t:TotalCount>\
     <t:UnreadCount>0</t:UnreadCount><t:ChildFolderCount>1</t:ChildFolderCount></t:Folder>"
        .to_string()
}

fn invoices_folder() -> String {
    "<t:Folder><t:FolderId Id=\"AAMkRECHNUNGEN\"/><t:ParentFolderId Id=\"AAMkPROJEKTE\"/>\
     <t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Rechnungen</t:DisplayName>\
     <t:TotalCount>1</t:TotalCount><t:UnreadCount>1</t:UnreadCount>\
     <t:ChildFolderCount>0</t:ChildFolderCount></t:Folder>"
        .to_string()
}

/// The reply of a first `SyncFolderItems`: two messages, no cursor in, cursor out.
fn first_changes() -> String {
    format!(
        "<m:SyncFolderItemsResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:SyncState>cursor-1</m:SyncState>\
         <m:IncludesLastItemInRange>true</m:IncludesLastItemInRange>\
         <m:Changes>{}{}</m:Changes></m:SyncFolderItemsResponseMessage>",
        create("AAMkMSG1", "Rechnung 4711", true, true, "ada@example.org"),
        create(
            "AAMkMSG2",
            "Angebot 0815",
            false,
            false,
            "carol@example.org"
        ),
    )
}

/// The reply of an incremental pass: one new message, one read change, one
/// deletion.
fn incremental_changes() -> String {
    format!(
        "<m:SyncFolderItemsResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:SyncState>cursor-2</m:SyncState>\
         <m:IncludesLastItemInRange>true</m:IncludesLastItemInRange>\
         <m:Changes>{}<m:ReadFlagChange><t:ItemId Id=\"AAMkMSG2\"/>\
         <t:IsRead>true</t:IsRead></m:ReadFlagChange>\
         <m:Delete><t:ItemId Id=\"AAMkMSG1\"/></m:Delete></m:Changes>\
         </m:SyncFolderItemsResponseMessage>",
        create("AAMkMSG3", "Nachfrage", false, true, "dave@example.org"),
    )
}

/// One `<m:Create>` element.
fn create(id: &str, subject: &str, is_read: bool, flagged: bool, from: &str) -> String {
    let flag = if flagged {
        "<t:Flag><t:FlagStatus>Flagged</t:FlagStatus></t:Flag>"
    } else {
        ""
    };
    format!(
        "<m:Create><t:Message><t:ItemId Id=\"{id}\" ChangeKey=\"ck-{id}\"/>\
         <t:ParentFolderId Id=\"AAMkINBOX\"/><t:ItemClass>IPM.Note</t:ItemClass>\
         <t:Subject>{subject}</t:Subject>\
         <t:From><t:Mailbox><t:EmailAddress>{from}</t:EmailAddress></t:Mailbox></t:From>\
         <t:ToRecipients><t:Mailbox><t:EmailAddress>bob@example.org</t:EmailAddress>\
         </t:Mailbox></t:ToRecipients>\
         <t:DateTimeReceived>2026-11-12T13:00:05Z</t:DateTimeReceived>\
         <t:Size>2048</t:Size><t:IsRead>{is_read}</t:IsRead>\
         <t:HasAttachments>false</t:HasAttachments>\
         <t:InternetMessageId>&lt;{id}@example.org&gt;</t:InternetMessageId>\
         {flag}</t:Message></m:Create>"
    )
}

/// The `GetItem` reply, carrying the raw message as base64.
fn mime_message() -> String {
    use base64::Engine;
    let raw = "From: ada@example.org\r\nTo: bob@example.org\r\nSubject: Rechnung 4711\r\n\
               Content-Type: text/plain; charset=utf-8\r\n\r\nBitte zeitnah begleichen.\r\n";
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    format!(
        "<m:GetItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode><m:Items>\
         <t:Message><t:ItemId Id=\"AAMkMSG1\"/><t:MimeContent CharacterSet=\"UTF-8\">{encoded}\
         </t:MimeContent></t:Message></m:Items></m:GetItemResponseMessage>"
    )
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn timeouts() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(5),
        handshake: Duration::from_secs(5),
        command: Duration::from_secs(20),
        greeting: Duration::from_secs(5),
    }
}

fn config(server: &Server) -> EwsConfig {
    let mut config = EwsConfig::new(server.endpoint(), "example\\ada", AuthKind::Password);
    config.timeouts = timeouts();
    config.policy = TlsPolicy::insecure();
    config
}

/// The account model of the same server, as the core sees it.
fn account(server: &Server) -> Account {
    Account {
        id: AccountId::new(),
        display_name: "Ada".to_string(),
        email: "ada@example.org".to_string(),
        kind: AccountKind::Exchange,
        auth: AuthKind::Password,
        username: "example\\ada".to_string(),
        incoming: server.endpoint(),
        outgoing: server.endpoint(),
        proxy: None,
        oauth: None,
        enabled: true,
        created_at: hylmail_core::models::ids::utc_now(),
    }
}

async fn login(server: &Server, password: &str) -> EwsClient {
    let secret = SecretString::from(password.to_string());
    let credential = Credential::Password(&secret);
    tokio::time::timeout(
        Duration::from_secs(20),
        EwsClient::login(&config(server), credential),
    )
    .await
    .expect("the client hung")
    .expect("the login failed")
}

/// Logs in and synchronises the inbox once.
async fn synced(
    server: &Server,
    depth: SyncDepth,
) -> (Database, hylmail_core::models::Folder, AccountId) {
    let account_id = AccountId::new();
    let mut client = login(server, "geheim").await;
    let discovered = client.list_folders(account_id).await.unwrap();
    let database = Database::open_in_memory_migrated().unwrap();
    database
        .write(|connection| folders::upsert_tree(connection, &discovered))
        .unwrap();
    let inbox = discovered
        .iter()
        .find(|folder| folder.kind == FolderKind::Inbox)
        .expect("the inbox is missing")
        .clone();
    let options = SyncOptions {
        depth,
        batch_size: 500,
        max_new: 0,
    };
    let mut progress = |_: hylmail_core::imap::synchronise::SyncProgress| {};
    sync_folder(
        &mut client,
        &database,
        account_id,
        &inbox,
        options,
        &mut progress,
    )
    .await
    .unwrap();
    (database, inbox, account_id)
}

fn stored(
    database: &Database,
    folder: &hylmail_core::models::Folder,
) -> Vec<(String, MessageFlags)> {
    database
        .read(|connection| {
            messages::list_messages(
                connection,
                &MessageQuery {
                    folder_ids: vec![folder.id],
                    limit: None,
                    ..MessageQuery::default()
                },
            )
        })
        .unwrap()
        .into_iter()
        .map(|header| (header.server_id.unwrap_or_default(), header.flags))
        .collect()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_connection_is_verified_and_the_server_version_is_reported() {
    let server = Server::start(Script::default()).await;
    let client = login(&server, "geheim").await;

    let outcome = client.outcome();
    assert_eq!(outcome.server_version.as_deref(), Some("15.01.2507.037"));
    assert_eq!(outcome.server_minor_version.as_deref(), Some("6"));
    assert!(!outcome.encrypted, "the test endpoint is cleartext");
    assert_eq!(outcome.requested_version, "Exchange2013");
    assert!(outcome.describe().contains("15.01.2507.037"));

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].operation(), "GetFolder");
    // The request declares the schema version and addresses the inbox.
    assert!(requests[0]
        .body
        .contains("<t:RequestServerVersion Version=\"Exchange2013\"/>"));
    assert!(requests[0]
        .body
        .contains("<t:DistinguishedFolderId Id=\"inbox\"/>"));
    assert!(requests[0]
        .body
        .contains("xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\""));

    // The credential arrives as a `Basic` header, base64 of `user:password`.
    use base64::Engine;
    let header = requests[0].authorization.as_deref().unwrap();
    let encoded = header.strip_prefix("Basic ").unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    assert_eq!(
        String::from_utf8(decoded).unwrap(),
        "example\\ada:geheim",
        "the header was {header}"
    );
}

#[tokio::test]
async fn the_folder_tree_is_discovered_and_parents_are_linked() {
    let server = Server::start(Script::default()).await;
    let account_id = AccountId::new();
    let mut client = login(&server, "geheim").await;
    let discovered = client.list_folders(account_id).await.unwrap();

    assert_eq!(discovered.len(), 3);
    let name = |wanted: &str| {
        discovered
            .iter()
            .find(|folder| folder.name == wanted)
            .unwrap()
            .clone()
    };
    let inbox = name("Posteingang");
    assert_eq!(inbox.kind, FolderKind::Inbox);
    // The server id is stored as the path: it is what every EWS call needs.
    assert_eq!(inbox.path, "AAMkINBOX");
    assert_eq!(inbox.unread_count, 1);
    assert_eq!(inbox.total_count, 2);
    assert!(inbox.selectable);

    let projects = name("Projekte");
    let invoices = name("Rechnungen");
    assert_eq!(invoices.parent_id, Some(projects.id));
    assert!(projects.parent_id.is_none());
    // Special folders are ordered ahead of the custom tree.
    assert_eq!(discovered[0].kind, FolderKind::Inbox);

    // The tree was walked: the root, then the folder that reports children.
    let find_folder_bodies: Vec<String> = server
        .requests()
        .iter()
        .filter(|request| request.operation() == "FindFolder")
        .map(|request| request.body.clone())
        .collect();
    assert_eq!(find_folder_bodies.len(), 2, "{find_folder_bodies:#?}");
    assert!(find_folder_bodies[0].contains("msgfolderroot"));
    assert!(find_folder_bodies[1].contains("AAMkPROJEKTE"));
    // A shallow search is what makes the result one level.
    assert!(find_folder_bodies[0].contains("Traversal=\"Shallow\""));
}

#[tokio::test]
async fn the_first_pass_stores_the_messages_and_the_cursor() {
    let server = Server::start(Script::default()).await;
    let (database, inbox, _account) = synced(&server, SyncDepth::Incremental).await;

    let items = stored(&database, &inbox);
    assert_eq!(items.len(), 2);
    let ids: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains(&"AAMkMSG1"));
    assert!(ids.contains(&"AAMkMSG2"));

    let (_, flags) = items.iter().find(|(id, _)| id == "AAMkMSG1").unwrap();
    assert!(flags.contains(MessageFlags::SEEN));
    assert!(flags.contains(MessageFlags::FLAGGED));
    let (_, flags) = items.iter().find(|(id, _)| id == "AAMkMSG2").unwrap();
    assert!(!flags.contains(MessageFlags::SEEN), "unread stays unread");

    // The cursor is stored on the folder row; the next pass resumes from it.
    let folder = database
        .read(|connection| folders::get_folder(connection, inbox.id))
        .unwrap()
        .unwrap();
    assert_eq!(folder.sync_marker.as_deref(), Some("cursor-1"));
    assert_eq!(folder.total_count, 2);
    assert_eq!(folder.unread_count, 1);

    // A first pass asks for the whole folder: no cursor is sent.
    let bodies = server.bodies();
    assert!(!bodies.contains("<m:SyncState>"), "{bodies}");
    assert!(bodies.contains("<m:MaxChangesReturned>512</m:MaxChangesReturned>"));
    assert!(bodies.contains("<t:FolderId Id=\"AAMkINBOX\"/></m:SyncFolderId>"));
}

#[tokio::test]
async fn a_second_pass_resumes_from_the_cursor_and_applies_every_change() {
    let server = Server::start(Script::default()).await;
    let account_id = AccountId::new();
    let mut client = login(&server, "geheim").await;
    let discovered = client.list_folders(account_id).await.unwrap();
    let database = Database::open_in_memory_migrated().unwrap();
    database
        .write(|connection| folders::upsert_tree(connection, &discovered))
        .unwrap();
    let inbox = discovered
        .iter()
        .find(|folder| folder.kind == FolderKind::Inbox)
        .unwrap()
        .clone();
    let options = SyncOptions {
        depth: SyncDepth::Incremental,
        batch_size: 500,
        max_new: 0,
    };
    let mut progress = |_: hylmail_core::imap::synchronise::SyncProgress| {};

    // First pass: two messages and the cursor.
    let first = sync_folder(
        &mut client,
        &database,
        account_id,
        &inbox,
        options,
        &mut progress,
    )
    .await
    .unwrap();
    assert_eq!(first.new_messages, 2);

    // Second pass: one new message, one read change, one deletion.
    let folder = database
        .read(|connection| folders::get_folder(connection, inbox.id))
        .unwrap()
        .unwrap();
    let second = sync_folder(
        &mut client,
        &database,
        account_id,
        &folder,
        options,
        &mut progress,
    )
    .await
    .unwrap();
    assert_eq!(second.new_messages, 1);
    assert_eq!(second.updated_flags, 1);
    assert_eq!(second.removed_messages, 1);
    assert!(!second.is_noop());

    let items = stored(&database, &folder);
    assert_eq!(items.len(), 2);
    let ids: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains(&"AAMkMSG2"), "{ids:?}");
    assert!(ids.contains(&"AAMkMSG3"), "{ids:?}");
    assert!(!ids.contains(&"AAMkMSG1"), "the deletion was not applied");
    let (_, flags) = items.iter().find(|(id, _)| id == "AAMkMSG2").unwrap();
    assert!(
        flags.contains(MessageFlags::SEEN),
        "the read change was lost"
    );

    // The second request carried the cursor of the first one.
    let bodies = server.bodies();
    assert!(
        bodies.contains("<m:SyncState>cursor-1</m:SyncState>"),
        "{bodies}"
    );
    let folder = database
        .read(|connection| folders::get_folder(connection, folder.id))
        .unwrap()
        .unwrap();
    assert_eq!(folder.sync_marker.as_deref(), Some("cursor-2"));
}

#[tokio::test]
async fn a_full_pass_drops_the_messages_the_server_no_longer_reports() {
    let server = Server::start(Script::default()).await;
    let (database, inbox, account_id) = synced(&server, SyncDepth::Incremental).await;

    // A message that only exists locally: its id is not in the enumeration.
    let mut stale = hylmail_core::models::message::MessageHeader::new(account_id, inbox.id);
    stale.server_id = Some("AAMkVERSCHWUNDEN".to_string());
    stale.uid = None;
    database
        .write(|connection| messages::upsert_message(connection, &mut stale))
        .unwrap();
    assert_eq!(stored(&database, &inbox).len(), 3);

    let mut client = login(&server, "geheim").await;
    let options = SyncOptions {
        depth: SyncDepth::Full,
        batch_size: 500,
        max_new: 0,
    };
    let mut progress = |_: hylmail_core::imap::synchronise::SyncProgress| {};
    // A full pass ignores the stored cursor, so the server enumerates the
    // folder again and the stale row can be recognised.
    let folder = database
        .read(|connection| folders::get_folder(connection, inbox.id))
        .unwrap()
        .unwrap();
    let report = sync_folder(
        &mut client,
        &database,
        account_id,
        &folder,
        options,
        &mut progress,
    )
    .await
    .unwrap();
    assert_eq!(report.removed_messages, 1, "{report:?}");
    let items = stored(&database, &folder);
    assert_eq!(items.len(), 2);
    assert!(!items.iter().any(|(id, _)| id == "AAMkVERSCHWUNDEN"));
    // The pass started without a cursor even though one was stored, so the
    // server enumerated the folder instead of answering incrementally.
    let again = server.bodies();
    assert_eq!(again.matches("<m:SyncState>").count(), 0, "{again}");
}

/// The NTLM message type of an `Authorization` header, when it carries one.
fn ntlm_message_type(authorization: Option<&str>) -> Option<u32> {
    let token = authorization?.split_whitespace().last()?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(token)
        .ok()?;
    if raw.len() < 12 || &raw[..8] != b"NTLMSSP\0" {
        return None;
    }
    Some(u32::from_le_bytes(raw[8..12].try_into().ok()?))
}

/// `UTF-16LE`, the encoding of every NTLM string.
fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[tokio::test]
async fn an_ntlm_account_authenticates_on_a_single_connection() {
    let server = Server::start(Script {
        ntlm: true,
        ..Script::default()
    })
    .await;

    let mut config = config(&server);
    config.username = "Domain\\User".to_string();
    config.auth = AuthKind::Ntlm;
    let secret = SecretString::from("Password");
    let client = EwsClient::login(&config, Credential::Password(&secret))
        .await
        .expect("the handshake must end in an authenticated session");
    assert_eq!(client.url(), config.url());

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one negotiate and one authenticate leg");
    assert_eq!(
        ntlm_message_type(requests[0].authorization.as_deref()),
        Some(1),
        "the first leg offers NTLM"
    );
    assert_eq!(
        ntlm_message_type(requests[1].authorization.as_deref()),
        Some(3),
        "the second leg answers the challenge"
    );
    // The second leg repeats the request, so the operation itself succeeded.
    assert_eq!(requests[1].operation(), "GetFolder");

    let authorize = requests[1].authorization.clone().unwrap();
    let answer = base64::engine::general_purpose::STANDARD
        .decode(authorize.split_whitespace().last().unwrap())
        .unwrap();
    let domain = utf16le("Domain");
    let user = utf16le("User");
    assert!(
        answer
            .windows(domain.len())
            .any(|window| window == domain.as_slice()),
        "the domain is carried in the authenticate message"
    );
    assert!(
        answer
            .windows(user.len())
            .any(|window| window == user.as_slice()),
        "the user is carried in the authenticate message"
    );

    // The point of the whole exercise: both legs travelled over one connection,
    // because the answer to a challenge is only valid on that connection.
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn an_ntlm_account_with_an_oauth_token_is_refused() {
    let server = Server::start(Script::default()).await;
    let mut config = config(&server);
    config.auth = AuthKind::Ntlm;
    let token = SecretString::from("token");
    let credential = Credential::OAuth2 {
        user: "ada@example.org".to_string(),
        token: &token,
    };
    let error = EwsClient::login(&config, credential).await.unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
}

#[tokio::test]
async fn a_rejected_password_becomes_an_auth_error() {
    let server = Server::start(Script {
        reject_login: true,
        ..Script::default()
    })
    .await;
    let secret = SecretString::from("falsch");
    let error = EwsClient::login(&config(&server), Credential::Password(&secret))
        .await
        .unwrap_err();
    match error {
        Error::Auth { account, message } => {
            assert_eq!(account, "example\\ada");
            assert!(
                message.contains("401") || message.contains("rejected"),
                "{message}"
            );
        }
        other => panic!("expected an auth error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_soap_fault_is_reported_as_a_protocol_error() {
    let server = Server::start(Script {
        soap_auth_fault: true,
        ..Script::default()
    })
    .await;
    let secret = SecretString::from("geheim");
    // A rejected password inside an HTTP 200 answer is still an auth error.
    let error = EwsClient::login(&config(&server), Credential::Password(&secret))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Auth { .. }), "{error:?}");

    let server = Server::start(Script {
        soap_fault: true,
        ..Script::default()
    })
    .await;
    let error = EwsClient::login(&config(&server), Credential::Password(&secret))
        .await
        .unwrap_err();
    match error {
        Error::Protocol { protocol, .. } => assert_eq!(protocol, "EWS"),
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn the_mime_content_of_a_message_is_downloaded() {
    let server = Server::start(Script::default()).await;
    let mut client = login(&server, "geheim").await;
    let raw = client.fetch_mime("AAMkMSG1").await.unwrap();
    let text = String::from_utf8(raw).unwrap();
    assert!(text.starts_with("From: ada@example.org\r\n"), "{text}");
    assert!(text.contains("Subject: Rechnung 4711"), "{text}");
    assert!(text.contains("Bitte zeitnah begleichen."), "{text}");

    // The request asked for the MIME content of exactly that item.
    let request = server
        .requests()
        .into_iter()
        .find(|request| request.operation() == "GetItem")
        .unwrap();
    assert!(request
        .body
        .contains("<t:IncludeMimeContent>true</t:IncludeMimeContent>"));
    assert!(request.body.contains("<t:ItemId Id=\"AAMkMSG1\"/>"));
}

#[tokio::test]
async fn flags_and_moves_are_pushed_to_the_server() {
    let server = Server::start(Script::default()).await;
    let mut client = login(&server, "geheim").await;
    client.set_read("AAMkMSG2", true).await.unwrap();
    client.set_flag("AAMkMSG2", false).await.unwrap();
    let moved = client
        .move_item("AAMkMSG2", &FolderRef::distinguished("archive"))
        .await
        .unwrap();
    assert_eq!(moved, "AAMkMOVED");
    client.delete_item("AAMkMSG1", false).await.unwrap();

    let bodies = server.bodies();
    assert!(bodies.contains("FieldURI=\"message:IsRead\""), "{bodies}");
    assert!(bodies.contains("<t:IsRead>true</t:IsRead>"), "{bodies}");
    assert!(bodies.contains("FieldURI=\"message:Flag\""), "{bodies}");
    assert!(
        bodies.contains("<t:FlagStatus>NotFlagged</t:FlagStatus>"),
        "{bodies}"
    );
    assert!(
        bodies.contains("<t:DistinguishedFolderId Id=\"archive\"/>"),
        "{bodies}"
    );
    assert!(bodies.contains("DeleteType=\"SoftDelete\""), "{bodies}");
}

#[tokio::test]
async fn a_mismatched_credential_is_refused_early() {
    // An OAuth2 account handed a password is a configuration mistake; the client
    // says so instead of trying and failing somewhere deep inside the transport.
    // (The mirror image - an NTLM account handed a token - is covered by
    // `an_ntlm_account_with_an_oauth_token_is_refused`.)
    let server = Server::start(Script::default()).await;
    let mut config = EwsConfig::new(server.endpoint(), "example\\ada", AuthKind::OAuth2);
    config.timeouts = timeouts();
    config.policy = TlsPolicy::insecure();
    let secret = SecretString::from("geheim");
    let error = EwsClient::login(&config, Credential::Password(&secret))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    assert!(server.requests().is_empty(), "no request may be sent");
}

#[tokio::test]
async fn an_oauth2_token_becomes_a_bearer_header() {
    let server = Server::start(Script::default()).await;
    let mut config = config(&server);
    config.auth = AuthKind::OAuth2;
    let token = SecretString::from("eyJhbGciOiJIUzI1NiJ9.payload.signature");
    let _client = EwsClient::login(
        &config,
        Credential::OAuth2 {
            user: "ada@example.org".to_string(),
            token: &token,
        },
    )
    .await
    .unwrap();
    let request = server.requests().into_iter().next().unwrap();
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature")
    );
}

#[tokio::test]
async fn credentials_never_appear_in_the_debug_output() {
    let server = Server::start(Script::default()).await;
    let client = login(&server, "s3cr3t-passwort").await;
    let text = format!("{client:?}");
    assert!(!text.contains("s3cr3t-passwort"), "{text}");
    assert!(!text.contains("Basic "), "{text}");
    assert!(text.contains("Exchange2013"), "{text}");
    assert!(text.contains("example"), "{text}");
}

#[tokio::test]
async fn the_core_deletes_an_exchange_item_softly_or_hard() {
    let server = Server::start(Script::default()).await;
    let account = account(&server);
    let folder = Folder::new(account.id, "Posteingang", "inbox", FolderKind::Inbox);
    let mut header = MessageHeader::new(account.id, folder.id);
    header.server_id = Some("AAMkMSG1".to_string());
    let secret = SecretString::from("geheim");

    // A normal delete only moves the item into `Deleted Items`; Exchange does
    // that with a soft delete.
    let soft = delete::delete(
        &account,
        &folder,
        None,
        &header,
        DeleteMode::ToTrash,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap();
    assert_eq!(soft, DeleteOutcome::MovedToTrash);

    // Deleting from the trash asks for a hard delete, which is irreversible.
    let hard = delete::delete(
        &account,
        &folder,
        None,
        &header,
        DeleteMode::Permanently,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap();
    assert_eq!(hard, DeleteOutcome::Removed);

    let deletes: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.operation() == "DeleteItem")
        .collect();
    assert_eq!(deletes.len(), 2);
    assert!(
        deletes[0].body.contains("DeleteType=\"SoftDelete\""),
        "{}",
        deletes[0].body
    );
    assert!(
        deletes[1].body.contains("DeleteType=\"HardDelete\""),
        "{}",
        deletes[1].body
    );
    assert!(
        deletes[1].body.contains("<t:ItemId Id=\"AAMkMSG1\"/>"),
        "{}",
        deletes[1].body
    );
}

#[tokio::test]
async fn the_core_moves_an_exchange_item_and_keeps_the_new_id() {
    let server = Server::start(Script::default()).await;
    let account = account(&server);
    let folder = Folder::new(account.id, "Posteingang", "inbox", FolderKind::Inbox);
    let archive = Folder::new(account.id, "Archiv", "AAMkARCHIVE", FolderKind::Archive);
    let mut header = MessageHeader::new(account.id, folder.id);
    header.server_id = Some("AAMkMSG1".to_string());
    let secret = SecretString::from("geheim");

    let outcome = relocate::move_to(
        &account,
        &folder,
        &archive,
        &header,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap();

    // Exchange renumbers the item on a move; the caller has to store that.
    assert_eq!(
        outcome,
        MoveOutcome {
            new_server_id: Some("AAMkMOVED".to_string()),
        }
    );

    let moves: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.operation() == "MoveItem")
        .collect();
    assert_eq!(moves.len(), 1);
    // The destination is the server id of the folder, not a distinguished one.
    assert!(
        moves[0].body.contains("<t:FolderId Id=\"AAMkARCHIVE\"/>"),
        "{}",
        moves[0].body
    );
    assert!(
        moves[0].body.contains("<t:ItemId Id=\"AAMkMSG1\"/>"),
        "{}",
        moves[0].body
    );
}

#[tokio::test]
async fn the_core_refuses_to_move_an_exchange_item_into_its_own_folder() {
    let server = Server::start(Script::default()).await;
    let account = account(&server);
    let folder = Folder::new(account.id, "Posteingang", "inbox", FolderKind::Inbox);
    let mut header = MessageHeader::new(account.id, folder.id);
    header.server_id = Some("AAMkMSG1".to_string());
    let secret = SecretString::from("geheim");

    let error = relocate::move_to(
        &account,
        &folder,
        &folder,
        &header,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Config(_)));
    assert!(server
        .requests()
        .into_iter()
        .all(|request| request.operation() != "MoveItem"));
}

/// A composed message, as the outbox would hand it to a submitter.
fn composed_message() -> ComposedMessage {
    ComposedMessage {
        raw: b"From: ada@example.org\r\nTo: bob@example.org\r\n\
               Subject: Bericht\r\nMessage-Id: <1@example.org>\r\n\r\nText\r\n"
            .to_vec(),
        from: "ada@example.org".to_string(),
        recipients: vec!["bob@example.org".to_string()],
        message_id: "<1@example.org>".to_string(),
        subject: "Bericht".to_string(),
    }
}

#[tokio::test]
async fn the_core_submits_an_exchange_message_as_mime() {
    let server = Server::start(Script::default()).await;
    let account = account(&server);
    let composed = composed_message();
    let secret = SecretString::from("geheim");

    let receipt = send::submit(
        &account,
        &composed,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap();

    assert!(receipt.accepted, "{receipt:?}");
    // Exchange files the copy itself, so the id it reported has to travel on.
    assert_eq!(receipt.sent_copy.as_deref(), Some("AAMkSENT"));
    assert!(receipt.describe().starts_with("accepted:"), "{receipt:?}");

    let sends: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.operation() == "CreateItem")
        .collect();
    assert_eq!(sends.len(), 1);
    let body = &sends[0].body;
    assert!(
        body.contains("MessageDisposition=\"SendAndSaveCopy\""),
        "{body}"
    );
    // The bytes are the message: decoding must give back exactly what the
    // composer produced, otherwise a header could be lost on the way.
    let start = body.find("UTF-8\">").unwrap() + "UTF-8\">".len();
    let end = body[start..].find("</t:MimeContent>").unwrap() + start;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&body[start..end])
        .unwrap();
    assert_eq!(decoded, composed.raw);
}

#[tokio::test]
async fn a_refused_exchange_send_is_permanent_and_is_not_retried() {
    let script = Script {
        send_denied: true,
        ..Script::default()
    };
    let server = Server::start(script).await;
    let account = account(&server);
    let secret = SecretString::from("geheim");

    let error = send::submit(
        &account,
        &composed_message(),
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Protocol { .. }), "{error}");
    // A refusal that would be retried forever would flood the server.
    assert!(!error.is_transient(), "{error}");
    assert_eq!(
        server
            .requests()
            .into_iter()
            .filter(|request| request.operation() == "CreateItem")
            .count(),
        1
    );
}
