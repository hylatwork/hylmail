//! Integration tests for the GroupWise SOAP client against a scripted proxy.
//!
//! The proxy is a hand written HTTP/1.1 endpoint that remembers what the client
//! sent, so the tests can assert the parts of the protocol that a real GroupWise
//! installation is unforgiving about: the session header on every request after
//! the login, the paging window of `getItemsRequest`, and the second login after
//! a session expired.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use hylmail_core::db::{folders, messages, Database};
use hylmail_core::delete::{self, DeleteMode, DeleteOutcome};
use hylmail_core::groupwise::{parse_status_code, sync_folder, GroupWiseClient, GroupWiseConfig};
use hylmail_core::models::account::{
    Account, AccountKind, AuthKind, Endpoint, GroupWiseProxyConfig, TlsMode,
};
use hylmail_core::models::folder::{Folder, FolderKind};
use hylmail_core::models::ids::AccountId;
use hylmail_core::models::message::{MessageFlags, MessageHeader, MessageQuery};
use hylmail_core::net::{Timeouts, TlsPolicy};
use hylmail_core::relocate::{self, MoveOutcome};
use hylmail_core::secrets::Credential;
use hylmail_core::Error;
use parking_lot::Mutex;
use secrecy::SecretString;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// the scripted proxy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Request {
    action: String,
    /// `true` when the request carried a session id in the SOAP header.
    has_session: bool,
    body: String,
}

impl Request {
    fn field(&self, name: &str) -> String {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        self.body
            .split_once(&open)
            .and_then(|(_, rest)| rest.split_once(&close))
            .map(|(value, _)| value.to_string())
            .unwrap_or_default()
    }
}

/// How the proxy should behave.
#[derive(Debug, Clone, Default)]
struct Script {
    /// Reject the first login with a wrong-password status.
    reject_login: bool,
    /// Expire the session once, so the client has to log in again.
    expire_session_once: bool,
    /// Answer with a SOAP fault.
    soap_fault: bool,
    /// Answer with an HTTP redirect to the post office.
    redirect: bool,
}

struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
}

#[derive(Debug, Default)]
struct ServerState {
    /// Number of logins served so far.
    logins: u32,
    /// Number of times a listing was served, used for paging and expiry.
    listings: u32,
    expired: bool,
}

impl Server {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
        let state: Arc<Mutex<ServerState>> = Arc::new(Mutex::new(ServerState::default()));
        let script = Arc::new(script);
        let captured = Arc::clone(&requests);
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let script = Arc::clone(&script);
                let captured = Arc::clone(&captured);
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    let _ = serve(stream, script, captured, shared).await;
                });
            }
        });
        Self { port, requests }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new("127.0.0.1", self.port, TlsMode::None)
    }

    fn settings(&self) -> GroupWiseProxyConfig {
        GroupWiseProxyConfig {
            proxy: self.endpoint(),
            post_office: Some("po1".to_string()),
            user_id: Some("ada".to_string()),
            shared_mailboxes: Vec::new(),
            allow_certificate_cn_fallback: false,
        }
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

async fn serve(
    stream: TcpStream,
    script: Arc<Script>,
    captured: Arc<Mutex<Vec<Request>>>,
    state: Arc<Mutex<ServerState>>,
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
        let request = Request {
            action,
            has_session: body.contains("<log:session"),
            body,
        };
        let response = respond(&request, &script, &state);
        captured.lock().push(request);

        let (status, reason, payload, extra) = response;
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/xml; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n{extra}\r\n",
            payload.len()
        );
        write_half.write_all(head.as_bytes()).await?;
        write_half.write_all(payload.as_bytes()).await?;
        write_half.flush().await?;
    }
}

/// Builds the answer for one request.
fn respond(
    request: &Request,
    script: &Script,
    state: &Arc<Mutex<ServerState>>,
) -> (u16, &'static str, String, String) {
    let ok = |payload: String| (200, "OK", payload, String::new());
    if script.redirect {
        return (
            302,
            "Found",
            "<?xml version=\"1.0\"?><html/>".to_string(),
            "Location: https://po1.example.org:7191/soap\r\n".to_string(),
        );
    }
    if script.soap_fault {
        return ok(fault("soap:Server", "Der Proxy ist beschäftigt"));
    }
    match request.action.as_str() {
        "loginRequest" => {
            let logins = {
                let mut state = state.lock();
                state.logins += 1;
                state.logins
            };
            if script.reject_login && logins == 1 {
                return ok(status_response(
                    "loginRequest",
                    0x4205,
                    "Ungültiges Kennwort",
                    None,
                ));
            }
            let session = format!("SESSION-{logins}");
            let mut payload = status_response("loginRequest", 0, "", None);
            payload = payload.replace(
                "</log:loginResponse>",
                &format!("<log:session>{session}</log:session></log:loginResponse>"),
            );
            ok(payload)
        }
        "logoutRequest" => ok(envelope(
            "log:logoutResponse",
            "<log:status><log:code>0</log:code></log:status>",
        )),
        "getFolderListRequest" => ok(envelope(
            "gw:getFolderListResponse",
            &format!(
                "<gw:status><gw:code>0</gw:code></gw:status><gw:folders>{}{}{}</gw:folders>",
                folder("1", "Mailbox", None, 0, 2, 1),
                folder("5", "Projekte", None, 0, 0, 0),
                folder("9", "Rechnungen", Some("5"), 0, 1, 1),
            ),
        )),
        "getItemsRequest" => {
            let container = request.field("gw:container");
            let offset: u32 = request.field("gw:offset").parse().unwrap_or(0);
            let mut state = state.lock();
            state.listings += 1;
            let listings = state.listings;
            let session_expired = script.expire_session_once && listings == 1;
            state.expired |= session_expired;
            drop(state);
            if session_expired {
                return ok(status_response(
                    "getItemsRequest",
                    0x4202,
                    "Sitzung abgelaufen",
                    None,
                ));
            }
            if container != "1" {
                return ok(envelope(
                    "gw:getItemsResponse",
                    "<gw:status><gw:code>0</gw:code></gw:status>",
                ));
            }
            // The mailbox holds three items, served in two pages.
            let items = if offset == 0 {
                format!(
                    "{}{}",
                    item("A1", "Rechnung 4711", 1, 1),
                    item("A2", "Angebot", 0, 0)
                )
            } else if offset == 2 {
                item("A3", "Nachfrage", 1, 0)
            } else {
                String::new()
            };
            ok(envelope(
                "gw:getItemsResponse",
                &format!(
                    "<gw:status><gw:code>0</gw:code></gw:status>\
                     <gw:items><gw:total>3</gw:total>{items}</gw:items>"
                ),
            ))
        }
        "getItemRequest" => {
            use base64::Engine;
            let raw = "From: ada@example.org\r\nTo: bob@example.org\r\nSubject: Rechnung 4711\r\n\
                       Content-Type: text/plain; charset=utf-8\r\n\r\nBitte begleichen.\r\n";
            let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
            ok(envelope(
                "gw:getItemResponse",
                &format!(
                    "<gw:status><gw:code>0</gw:code></gw:status>\
                     <gw:item type=\"mail\"><gw:id>A1</gw:id><gw:data>{encoded}</gw:data>\
                     </gw:item>"
                ),
            ))
        }
        "markItemsReadRequest"
        | "markItemsUnreadRequest"
        | "moveItemRequest"
        | "deleteItemRequest" => ok(status_response(&request.action, 0, "", None)),
        other => panic!("the client sent an unexpected operation: {other}"),
    }
}

fn envelope(element: &str, content: &str) -> String {
    let namespace = if element.starts_with("log:") {
        hylmail_core::groupwise::LOGIN_NAMESPACE
    } else {
        hylmail_core::groupwise::METHODS_NAMESPACE
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <SOAP-ENV:Envelope xmlns:SOAP-ENV=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         xmlns:log=\"{}\" xmlns:gw=\"{}\" xmlns:types=\"{}\">\
         <SOAP-ENV:Body><{element} xmlns=\"{namespace}\">{content}</{element}></SOAP-ENV:Body>\
         </SOAP-ENV:Envelope>",
        hylmail_core::groupwise::LOGIN_NAMESPACE,
        hylmail_core::groupwise::METHODS_NAMESPACE,
        hylmail_core::groupwise::TYPES_NAMESPACE,
    )
}

/// A status-only answer, optionally with a session id.
fn status_response(
    action: &str,
    code: i64,
    description: &str,
    element: Option<(&str, &str)>,
) -> String {
    let (element, content) = match element {
        Some((element, content)) => (element, content.to_string()),
        None => {
            let response = if action.starts_with("login") {
                "log:loginResponse"
            } else {
                "gw:getItemsResponse"
            };
            (response, String::new())
        }
    };
    let description = if description.is_empty() {
        String::new()
    } else {
        format!("<gw:description>{description}</gw:description>")
    };
    envelope(
        element,
        &format!("<gw:status><gw:code>0x{code:04X}</gw:code>{description}</gw:status>{content}"),
    )
}

fn fault(code: &str, message: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <SOAP-ENV:Envelope xmlns:SOAP-ENV=\"http://schemas.xmlsoap.org/soap/envelope/\">\
         <SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>{code}</faultcode>\
         <faultstring>{message}</faultstring></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>"
    )
}

fn folder(
    id: &str,
    name: &str,
    parent: Option<&str>,
    folder_type: i32,
    count: u32,
    unread: u32,
) -> String {
    let parent = parent.unwrap_or("0");
    format!(
        "<gw:folder><gw:id>{id}</gw:id><gw:name>{name}</gw:name><gw:parent>{parent}</gw:parent>\
         <gw:folderType>{folder_type}</gw:folderType><gw:itemCount>{count}</gw:itemCount>\
         <gw:unread>{unread}</gw:unread><gw:sequence>7</gw:sequence></gw:folder>"
    )
}

fn item(id: &str, subject: &str, unread: u32, size: u32) -> String {
    format!(
        "<gw:item type=\"mail\"><gw:id>{id}</gw:id><gw:subject>{subject}</gw:subject>\
         <gw:from><gw:displayName>Ada</gw:displayName><gw:email>ada@example.org</gw:email></gw:from>\
         <gw:to><gw:email>bob@example.org</gw:email></gw:to>\
         <gw:date>20261112130000</gw:date><gw:size>{size}</gw:size><gw:unread>{unread}</gw:unread>\
         <gw:messageId>&lt;{id}@example.org&gt;</gw:messageId></gw:item>"
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

fn config(server: &Server) -> GroupWiseConfig {
    let mut config = GroupWiseConfig::new(server.endpoint(), "ada", &server.settings());
    config.timeouts = timeouts();
    config.policy = TlsPolicy::insecure();
    config
}

/// The account model of the same proxy, as the core sees it.
fn account(server: &Server) -> Account {
    Account {
        id: AccountId::new(),
        display_name: "Ada".to_string(),
        email: "ada@example.org".to_string(),
        kind: AccountKind::GroupWiseProxy,
        auth: AuthKind::Password,
        username: "ada".to_string(),
        incoming: server.endpoint(),
        outgoing: server.endpoint(),
        proxy: Some(server.settings()),
        oauth: None,
        enabled: true,
        created_at: hylmail_core::models::ids::utc_now(),
    }
}

async fn login(server: &Server, password: &str) -> GroupWiseClient {
    let secret = SecretString::from(password.to_string());
    tokio::time::timeout(
        Duration::from_secs(20),
        GroupWiseClient::login(&config(server), Credential::Password(&secret)),
    )
    .await
    .expect("the client hung")
    .expect("the login failed")
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
async fn the_login_yields_a_session_and_a_verified_connection() {
    let server = Server::start(Script::default()).await;
    let mut client = login(&server, "geheim").await;
    assert!(client.is_logged_in());
    let outcome = client.outcome();
    assert_eq!(outcome.host, "127.0.0.1");
    assert!(!outcome.encrypted);
    assert!(outcome.describe().contains("GroupWise proxy"));

    // The login carried the credentials in the `log` namespace and no session.
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].action, "loginRequest");
    assert!(!requests[0].has_session);
    assert!(requests[0]
        .body
        .contains("<log:userName>ada</log:userName>"));
    assert!(requests[0]
        .body
        .contains("<log:password>geheim</log:password>"));
    assert!(requests[0]
        .body
        .contains(hylmail_core::groupwise::LOGIN_NAMESPACE));

    // The logout ends the session.
    client.logout().await.unwrap();
    assert!(!client.is_logged_in());
    let last = server.requests().pop().unwrap();
    assert_eq!(last.action, "logoutRequest");
    assert!(last.has_session, "the logout needs the session header");
}

#[tokio::test]
async fn a_rejected_password_is_an_auth_error() {
    let server = Server::start(Script {
        reject_login: true,
        ..Script::default()
    })
    .await;
    let secret = SecretString::from("falsch");
    let error = GroupWiseClient::login(&config(&server), Credential::Password(&secret))
        .await
        .unwrap_err();
    match error {
        Error::Auth { account, message } => {
            assert_eq!(account, "ada");
            assert!(message.contains("0x4205"), "{message}");
            assert!(message.contains("Kennwort"), "{message}");
        }
        other => panic!("expected an auth error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_oauth2_account_is_refused_without_a_request() {
    let server = Server::start(Script::default()).await;
    let token = SecretString::from("token");
    let error = GroupWiseClient::login(
        &config(&server),
        Credential::OAuth2 {
            user: "ada".to_string(),
            token: &token,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_soap_fault_becomes_a_protocol_error() {
    let server = Server::start(Script {
        soap_fault: true,
        ..Script::default()
    })
    .await;
    let secret = SecretString::from("geheim");
    let error = GroupWiseClient::login(&config(&server), Credential::Password(&secret))
        .await
        .unwrap_err();
    match error {
        Error::Protocol { protocol, .. } => assert_eq!(protocol, "GroupWise"),
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_redirect_reports_the_post_office_and_the_policy_decision() {
    let server = Server::start(Script {
        redirect: true,
        ..Script::default()
    })
    .await;
    let secret = SecretString::from("geheim");
    let error = GroupWiseClient::login(&config(&server), Credential::Password(&secret))
        .await
        .unwrap_err();
    // The fallback is not enabled for this account, so the redirect is a TLS
    // policy problem rather than something to follow silently.
    match error {
        Error::Tls { host, message } => {
            assert_eq!(host, "127.0.0.1");
            assert!(message.contains("po1.example.org"), "{message}");
            assert!(message.contains("certificate fallback"), "{message}");
        }
        other => panic!("expected a TLS policy error, got {other:?}"),
    }
}

#[tokio::test]
async fn the_folder_tree_is_discovered_and_linked() {
    let server = Server::start(Script::default()).await;
    let account_id = AccountId::new();
    let mut client = login(&server, "geheim").await;
    let discovered = client.list_folders(account_id).await.unwrap();
    assert_eq!(discovered.len(), 3);

    let inbox = discovered
        .iter()
        .find(|folder| folder.kind == FolderKind::Inbox)
        .unwrap();
    // `Mailbox` is the GroupWise name of the inbox.
    assert_eq!(inbox.name, "Mailbox");
    assert_eq!(inbox.path, "1");
    assert_eq!(inbox.unread_count, 1);

    let projects = discovered.iter().find(|f| f.name == "Projekte").unwrap();
    let invoices = discovered.iter().find(|f| f.name == "Rechnungen").unwrap();
    assert_eq!(invoices.parent_id, Some(projects.id));

    // The request after the login carries the session header.
    let listing = server
        .requests()
        .into_iter()
        .find(|request| request.action == "getFolderListRequest")
        .unwrap();
    assert!(listing.has_session);
    assert!(listing.body.contains("<gw:folderType>0</gw:folderType>"));
}

#[tokio::test]
async fn the_synchronisation_pages_through_the_folder_and_stores_the_messages() {
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

    let mut progress = |_: hylmail_core::imap::synchronise::SyncProgress| {};
    let report = sync_folder(&mut client, &database, account_id, &inbox, 0, &mut progress)
        .await
        .unwrap();
    assert_eq!(report.new_messages, 3, "{report:?}");
    assert_eq!(report.removed_messages, 0);

    let items = stored(&database, &inbox);
    assert_eq!(items.len(), 3);
    let ids: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains(&"A1") && ids.contains(&"A2") && ids.contains(&"A3"));
    // `unread 1` means unread; the others are read.
    let (_, flags) = items.iter().find(|(id, _)| id == "A1").unwrap();
    assert!(!flags.contains(MessageFlags::SEEN));
    let (_, flags) = items.iter().find(|(id, _)| id == "A2").unwrap();
    assert!(flags.contains(MessageFlags::SEEN));

    // Two pages were requested: the paging stops when the window reached the
    // total the server reported, so no empty third request is made.
    let pages: Vec<Request> = server
        .requests()
        .into_iter()
        .filter(|request| request.action == "getItemsRequest")
        .collect();
    assert_eq!(pages.len(), 2, "both data pages: {pages:#?}");
    assert_eq!(pages[0].field("gw:offset"), "0");
    assert_eq!(pages[0].field("gw:count"), "250");
    assert_eq!(pages[0].field("gw:container"), "1");
    assert_eq!(pages[1].field("gw:offset"), "2");
    assert!(pages.iter().all(|page| page.has_session));

    // The folder counters are refreshed from the local store: two of the three
    // items are unread, whatever the folder listing claimed.
    let folder = database
        .read(|connection| folders::get_folder(connection, inbox.id))
        .unwrap()
        .unwrap();
    assert_eq!(folder.total_count, 3);
    assert_eq!(folder.unread_count, 2);
}

#[tokio::test]
async fn a_second_pass_removes_the_messages_the_server_no_longer_lists() {
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
    let mut progress = |_: hylmail_core::imap::synchronise::SyncProgress| {};
    sync_folder(&mut client, &database, account_id, &inbox, 0, &mut progress)
        .await
        .unwrap();

    // A message that only exists locally is dropped by the next pass, because
    // GroupWise always lists the whole folder.
    let mut stale = hylmail_core::models::message::MessageHeader::new(account_id, inbox.id);
    stale.server_id = Some("A9VERSCHWUNDEN".to_string());
    database
        .write(|connection| messages::upsert_message(connection, &mut stale))
        .unwrap();
    assert_eq!(stored(&database, &inbox).len(), 4);

    let report = sync_folder(&mut client, &database, account_id, &inbox, 0, &mut progress)
        .await
        .unwrap();
    assert_eq!(report.new_messages, 0);
    assert_eq!(report.removed_messages, 1);
    let items = stored(&database, &inbox);
    assert_eq!(items.len(), 3);
    assert!(!items.iter().any(|(id, _)| id == "A9VERSCHWUNDEN"));
}

#[tokio::test]
async fn an_expired_session_is_repaired_by_logging_in_again() {
    let server = Server::start(Script {
        expire_session_once: true,
        ..Script::default()
    })
    .await;
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

    // The first listing is answered with "session expired"; the client has to
    // log in again and replay the request without the caller noticing.
    let mut progress = |_: hylmail_core::imap::synchronise::SyncProgress| {};
    let report = sync_folder(&mut client, &database, account_id, &inbox, 0, &mut progress)
        .await
        .unwrap();
    assert_eq!(report.new_messages, 3);
    assert!(client.is_logged_in());

    let logins = server
        .requests()
        .iter()
        .filter(|request| request.action == "loginRequest")
        .count();
    assert_eq!(logins, 2, "the client must log in a second time");
}

#[tokio::test]
async fn items_are_marked_and_moved_through_the_proxy() {
    let server = Server::start(Script::default()).await;
    let mut client = login(&server, "geheim").await;
    client.set_read(&["A1".to_string()], true).await.unwrap();
    client.set_read(&["A1".to_string()], false).await.unwrap();
    client.move_items(&["A1".to_string()], "9").await.unwrap();
    client.delete_items(&["A2".to_string()]).await.unwrap();

    let bodies = server.bodies();
    assert!(bodies.contains("markItemsReadRequest"), "{bodies}");
    assert!(bodies.contains("markItemsUnreadRequest"), "{bodies}");
    assert!(bodies.contains("moveItemRequest"), "{bodies}");
    assert!(
        bodies.contains("<gw:destination>9</gw:destination>"),
        "{bodies}"
    );
    assert!(bodies.contains("deleteItemRequest"), "{bodies}");
    assert!(bodies.contains("<gw:id>A1</gw:id>"), "{bodies}");
}

#[tokio::test]
async fn the_mime_content_of_a_message_is_downloaded() {
    let server = Server::start(Script::default()).await;
    let mut client = login(&server, "geheim").await;
    let raw = client.fetch_mime("A1").await.unwrap();
    let text = String::from_utf8(raw).unwrap();
    assert!(text.starts_with("From: ada@example.org\r\n"), "{text}");
    assert!(text.contains("Bitte begleichen."), "{text}");

    let request = server
        .requests()
        .into_iter()
        .find(|request| request.action == "getItemRequest")
        .unwrap();
    assert!(request.body.contains("<gw:id>A1</gw:id>"));
    assert!(request.has_session);
}

#[tokio::test]
async fn the_session_and_the_password_never_appear_in_the_debug_output() {
    let server = Server::start(Script::default()).await;
    let client = login(&server, "s3cr3t-passwort").await;
    let text = format!("{client:?}");
    assert!(!text.contains("s3cr3t-passwort"), "{text}");
    assert!(!text.contains("SESSION-"), "{text}");
    assert!(text.contains("ada"), "{text}");
}

#[test]
fn status_codes_are_parsed_from_both_spellings() {
    assert_eq!(parse_status_code("0x4202"), Some(0x4202));
    assert_eq!(parse_status_code("0"), Some(0));
    assert_eq!(parse_status_code("nope"), None);
}

#[tokio::test]
async fn the_core_deletes_a_groupwise_item() {
    let server = Server::start(Script::default()).await;
    let account = account(&server);
    // GroupWise folder ids are opaque numbers; they are what travels on the
    // wire, so they have to be the `path` of the folder model.
    let inbox = Folder::new(account.id, "Postfach", "1", FolderKind::Inbox);
    let trash = Folder::new(account.id, "Papierkorb", "9", FolderKind::Trash);
    let mut header = MessageHeader::new(account.id, inbox.id);
    header.server_id = Some("A1".to_string());
    let secret = SecretString::from("geheim");

    // With a known trash folder the item is moved there.
    let moved = delete::delete(
        &account,
        &inbox,
        Some(&trash),
        &header,
        DeleteMode::ToTrash,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap();
    assert_eq!(moved, DeleteOutcome::MovedToTrash);
    let bodies = server.bodies();
    assert!(bodies.contains("moveItemRequest"), "{bodies}");
    assert!(
        bodies.contains("<gw:destination>9</gw:destination>"),
        "{bodies}"
    );

    // Without one — or when the user asked for a permanent delete — the item is
    // removed outright.
    let removed = delete::delete(
        &account,
        &inbox,
        None,
        &header,
        DeleteMode::Permanently,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap();
    assert_eq!(removed, DeleteOutcome::Removed);
    let bodies = server.bodies();
    assert!(bodies.contains("deleteItemRequest"), "{bodies}");
    assert!(bodies.contains("<gw:id>A1</gw:id>"), "{bodies}");
}

#[tokio::test]
async fn the_core_moves_a_groupwise_item_into_another_folder() {
    let server = Server::start(Script::default()).await;
    let account = account(&server);
    // GroupWise folder ids are opaque numbers; they are what travels on the
    // wire, so they have to be the `path` of the folder model.
    let inbox = Folder::new(account.id, "Postfach", "1", FolderKind::Inbox);
    let cabinet = Folder::new(account.id, "Archiv", "6", FolderKind::Archive);
    let mut header = MessageHeader::new(account.id, inbox.id);
    header.server_id = Some("A1".to_string());
    let secret = SecretString::from("geheim");

    let outcome = relocate::move_to(
        &account,
        &inbox,
        &cabinet,
        &header,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap();

    // No new id: GroupWise keeps addressing the item as it did.
    assert_eq!(outcome, MoveOutcome::default());
    let bodies = server.bodies();
    assert!(bodies.contains("moveItemRequest"), "{bodies}");
    assert!(
        bodies.contains("<gw:destination>6</gw:destination>"),
        "{bodies}"
    );
    assert!(!bodies.contains("deleteItemRequest"), "{bodies}");
}

#[tokio::test]
async fn the_core_refuses_to_move_a_groupwise_item_into_its_own_folder() {
    let server = Server::start(Script::default()).await;
    let account = account(&server);
    let inbox = Folder::new(account.id, "Postfach", "1", FolderKind::Inbox);
    let mut header = MessageHeader::new(account.id, inbox.id);
    header.server_id = Some("A1".to_string());
    let secret = SecretString::from("geheim");

    let error = relocate::move_to(
        &account,
        &inbox,
        &inbox,
        &header,
        Credential::Password(&secret),
        &TlsPolicy::insecure(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Config(_)));
    assert!(!server.bodies().contains("moveItemRequest"));
}
