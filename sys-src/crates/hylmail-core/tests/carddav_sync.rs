//! `carddav::sync` against a mock CardDAV server.
//!
//! The mock answers the two requests a synchronisation makes: a `PROPFIND` with
//! a multistatus body, and a `GET` per changed resource.  What the test pins
//! down is the behaviour the requirements ask for: ETag change detection, the
//! lossless vCard round trip, and the removal of resources the server dropped.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use hylmail_core::carddav;
use hylmail_core::db::Database;
use hylmail_core::models::AccountId;
use hylmail_core::net::TlsPolicy;
use hylmail_core::secrets::secret;

/// A multistatus body listing the given `(href, etag)` resources.
fn multistatus(resources: &[(&str, &str)]) -> String {
    let mut body = String::from(
        "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">\
         <d:response><d:href>/addr/</d:href><d:propstat><d:prop>\
         <d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>",
    );
    for (href, etag) in resources {
        body.push_str(&format!(
            "<d:response><d:href>{href}</d:href><d:propstat><d:prop>\
             <d:getetag>{etag}</d:getetag></d:prop></d:propstat></d:response>"
        ));
    }
    body.push_str("</d:multistatus>");
    body
}

fn vcard(fn_name: &str, email: &str, extra: &str) -> String {
    format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{fn_name}\r\nN:{fn_name};;;;\r\n\
         EMAIL;TYPE=WORK;PREF:{email}\r\n{extra}END:VCARD\r\n"
    )
}

/// A response the mock knows how to send.
#[derive(Clone)]
struct Reply {
    status: &'static str,
    body: String,
}

/// Starts a mock server and returns its base URL plus the routing table it
/// serves.  The table is shared so a test can mutate the collection between two
/// synchronisations.
async fn base_url(routing: Arc<tokio::sync::Mutex<HashMap<String, Reply>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let routing = Arc::clone(&routing);
            tokio::spawn(serve(stream, routing));
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Serves every request of one connection.
///
/// The connection is kept alive (`Connection: keep-alive`) and reused, like the
/// EWS mock does: answering with `Connection: close` and dropping the socket
/// races the client, which may pick the closing connection out of its pool for
/// the next request and then see it "forcibly closed by the remote host".
async fn serve(mut stream: TcpStream, routing: Arc<tokio::sync::Mutex<HashMap<String, Reply>>>) {
    let mut buffer = vec![0u8; 8192];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let Some(request) = read_request(&mut stream, &mut buffer, &mut carry).await else {
            return;
        };
        let text = String::from_utf8_lossy(&request);
        let mut parts = text.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let key = format!("{method} {path}");
        let reply = routing.lock().await.get(&key).cloned().unwrap_or(Reply {
            status: "404 Not Found",
            body: String::new(),
        });
        let response = format!(
            "HTTP/1.1 {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
             Connection: keep-alive\r\n\r\n{}",
            reply.status,
            reply.body.len(),
            reply.body
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        if stream.flush().await.is_err() {
            return;
        }
    }
}

/// Reads exactly one HTTP/1.1 request, leaving any extra bytes for the next.
async fn read_request(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    carry: &mut Vec<u8>,
) -> Option<Vec<u8>> {
    let head_end = loop {
        if let Some(position) = carry.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        carry.extend_from_slice(&buffer[..read]);
    };
    let head = String::from_utf8_lossy(&carry[..head_end]).to_string();
    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while carry.len() < head_end + length {
        let read = stream.read(buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        carry.extend_from_slice(&buffer[..read]);
    }
    let end = head_end + length;
    let request = carry[..end].to_vec();
    carry.drain(..end);
    Some(request)
}

fn routing() -> Arc<tokio::sync::Mutex<HashMap<String, Reply>>> {
    Arc::new(tokio::sync::Mutex::new(HashMap::new()))
}

async fn insert(
    routing: &Arc<tokio::sync::Mutex<HashMap<String, Reply>>>,
    key: &str,
    status: &'static str,
    body: String,
) {
    routing
        .lock()
        .await
        .insert(key.to_string(), Reply { status, body });
}

#[tokio::test]
async fn a_first_sync_downloads_and_stores_the_collection() {
    let routing = routing();
    insert(
        &routing,
        "PROPFIND /addr/",
        "207 Multi-Status",
        multistatus(&[("/addr/1.vcf", "\"v1\""), ("/addr/2.vcf", "\"v1\"")]),
    )
    .await;
    insert(
        &routing,
        "GET /addr/1.vcf",
        "200 OK",
        vcard("Ada Lovelace", "ada@example.org", "X-UNKNOWN:keep me\r\n"),
    )
    .await;
    insert(
        &routing,
        "GET /addr/2.vcf",
        "200 OK",
        vcard("Bob", "bob@example.org", ""),
    )
    .await;
    let url = base_url(Arc::clone(&routing)).await;

    let database = Database::open_in_memory_migrated().unwrap();
    let account = AccountId::new();
    let report = carddav::sync(
        &database,
        account,
        &format!("{url}/addr/"),
        "ada",
        &secret("pw"),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap();

    assert_eq!(report.listed, 3, "two cards plus the collection");
    assert_eq!(report.fetched, 2);
    assert_eq!(report.removed, 0);

    let contacts = database
        .read(|connection| hylmail_core::db::contacts::list_contacts(connection, Some(account)))
        .unwrap();
    assert_eq!(contacts.len(), 2);
    let ada = contacts
        .iter()
        .find(|contact| contact.primary_email() == Some("ada@example.org"))
        .unwrap();
    assert_eq!(ada.display_name.as_deref(), Some("Ada Lovelace"));
    // The unknown property survives verbatim.
    assert!(ada
        .vcard_raw
        .as_deref()
        .unwrap()
        .contains("X-UNKNOWN:keep me"));
    assert!(ada.is_remote());
}

#[tokio::test]
async fn an_unchanged_etag_is_not_downloaded_again() {
    let routing = routing();
    insert(
        &routing,
        "PROPFIND /addr/",
        "207 Multi-Status",
        multistatus(&[("/addr/1.vcf", "\"v1\"")]),
    )
    .await;
    insert(
        &routing,
        "GET /addr/1.vcf",
        "200 OK",
        vcard("Ada", "ada@example.org", ""),
    )
    .await;
    let url = base_url(Arc::clone(&routing)).await;

    let database = Database::open_in_memory_migrated().unwrap();
    let account = AccountId::new();
    let base = format!("{url}/addr/");
    let first = carddav::sync(
        &database,
        account,
        &base,
        "ada",
        &secret("pw"),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap();
    assert_eq!(first.fetched, 1);

    // A second pass with the same ETag fetches nothing.
    let second = carddav::sync(
        &database,
        account,
        &base,
        "ada",
        &secret("pw"),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap();
    assert_eq!(second.fetched, 0);
    assert!(second.is_noop());
}

#[tokio::test]
async fn a_resource_the_server_drops_is_removed_locally() {
    let routing = routing();
    insert(
        &routing,
        "PROPFIND /addr/",
        "207 Multi-Status",
        multistatus(&[("/addr/1.vcf", "\"v1\""), ("/addr/2.vcf", "\"v1\"")]),
    )
    .await;
    insert(
        &routing,
        "GET /addr/1.vcf",
        "200 OK",
        vcard("Ada", "ada@example.org", ""),
    )
    .await;
    insert(
        &routing,
        "GET /addr/2.vcf",
        "200 OK",
        vcard("Bob", "bob@example.org", ""),
    )
    .await;
    let url = base_url(Arc::clone(&routing)).await;
    let database = Database::open_in_memory_migrated().unwrap();
    let account = AccountId::new();
    let base = format!("{url}/addr/");
    carddav::sync(
        &database,
        account,
        &base,
        "ada",
        &secret("pw"),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap();

    // The server now lists only one card.
    insert(
        &routing,
        "PROPFIND /addr/",
        "207 Multi-Status",
        multistatus(&[("/addr/1.vcf", "\"v1\"")]),
    )
    .await;
    let report = carddav::sync(
        &database,
        account,
        &base,
        "ada",
        &secret("pw"),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap();
    assert_eq!(report.removed, 1);
    let contacts = database
        .read(|connection| hylmail_core::db::contacts::list_contacts(connection, Some(account)))
        .unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].primary_email(), Some("ada@example.org"));
}

#[tokio::test]
async fn an_empty_url_is_refused_before_any_connection() {
    let database = Database::open_in_memory_migrated().unwrap();
    let error = carddav::sync(
        &database,
        AccountId::new(),
        "   ",
        "ada",
        &secret("pw"),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, hylmail_core::Error::Config { .. }),
        "{error:?}"
    );
}
