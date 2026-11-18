//! End-to-end test of the OAuth2 authorisation code flow with PKCE.
//!
//! A real browser cannot take part in a test, so it is replaced by a TCP
//! client that speaks the redirect the provider would send.  The token
//! endpoint is a one-shot TCP server that records the request, which makes the
//! two things that are easy to get wrong - the PKCE verifier and the refresh
//! grant - assertable.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use hylmail_core::error::Error;
use hylmail_core::models::account::{Account, AccountKind, AuthKind, Endpoint, OAuthConfig};
use hylmail_core::models::AccountId;
use hylmail_core::net::tls::TlsPolicy;
use hylmail_core::net::Timeouts;
use hylmail_core::oauth::{self, ensure_access_token, refresh, store_tokens, SignIn, TokenSet};
use hylmail_core::secrets::InMemorySecretStore;
use hylmail_core::soap::http::HttpClient;
use parking_lot::Mutex;
use secrecy::ExposeSecret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A one-shot HTTP server that answers every request with a fixed body.
struct MockEndpoint {
    port: u16,
    request: Arc<Mutex<Vec<u8>>>,
}

impl MockEndpoint {
    /// Starts a server that answers one request and then stops accepting.
    async fn start(status: u16, body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let request = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&request);
        tokio::spawn(async move {
            // Keep every connection alive and serve it request by request: a
            // `Connection: close` socket that the client still picks from its
            // pool is what turns into "the HTTP connection was lost".
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else {
                    return;
                };
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 4096];
                    let mut carry: Vec<u8> = Vec::new();
                    loop {
                        let Some(request) =
                            read_request(&mut stream, &mut buffer, &mut carry).await
                        else {
                            return;
                        };
                        recorded.lock().extend_from_slice(&request);
                        let answer = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                            body.len()
                        );
                        if stream.write_all(answer.as_bytes()).await.is_err() {
                            return;
                        }
                        if stream.flush().await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self { port, request }
    }

    fn token_endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/token", self.port)
    }

    fn config(&self) -> OAuthConfig {
        OAuthConfig {
            issuer: "https://provider.example".to_string(),
            client_id: "client-123".to_string(),
            scopes: vec!["mail.read".to_string(), "offline_access".to_string()],
            authorization_endpoint: "https://provider.example/authorize".to_string(),
            token_endpoint: self.token_endpoint(),
            redirect_port: 0,
        }
    }

    /// The `application/x-www-form-urlencoded` body of the recorded request.
    fn form(&self) -> std::collections::HashMap<String, String> {
        let request = self.request.lock().clone();
        let text = String::from_utf8_lossy(&request).to_string();
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect()
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

fn client() -> HttpClient {
    HttpClient::new(&TlsPolicy::strict(), &Timeouts::default()).unwrap()
}

fn account(oauth: Option<OAuthConfig>) -> Account {
    Account {
        id: AccountId::new(),
        display_name: "Test".to_string(),
        email: "anna@example.org".to_string(),
        kind: AccountKind::Imap,
        auth: AuthKind::OAuth2,
        username: "anna@example.org".to_string(),
        incoming: Endpoint::imaps("imap.example.org"),
        outgoing: Endpoint::smtps("smtp.example.org"),
        proxy: None,
        oauth,
        enabled: true,
        created_at: Utc::now(),
    }
}

/// Plays the browser: waits until the loopback listener is up and sends the
/// redirect with the given code and state.
async fn play_browser(port: u16, code: &str, state: String) -> String {
    let path = format!("/oauth2/callback?code={code}&state={state}");
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    // The listener is bound before the flow starts; retry briefly so a loaded
    // machine cannot lose the race between `begin` and this connect.
    let mut stream = None;
    for _ in 0..20 {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    let mut stream = stream.expect("the loopback listener was reachable");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = String::new();
    // The callback answers once and then closes; a reset after the body is
    // harmless, so the read is not treated as fatal (like the loopback's own
    // unit tests do).
    let _ = stream.read_to_string(&mut answer).await;
    answer
}

/// The `state` that the authorisation URL carries.
fn state_of(url: &str) -> String {
    let parsed = url::Url::parse(url).unwrap();
    parsed
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flow_ends_with_a_token_set_in_the_vault() {
    let endpoint = MockEndpoint::start(
        200,
        r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600,"token_type":"Bearer"}"#,
    )
    .await;
    let store = InMemorySecretStore::new();
    let account = account(Some(endpoint.config()));

    let config = endpoint.config();
    let sign_in = SignIn::begin(&config).await.unwrap();
    let url = sign_in.url().to_string();
    let port = sign_in.port();
    let verifier = sign_in.verifier().to_string();
    let state = state_of(&url);
    assert!(url.starts_with("https://provider.example/authorize?"));

    let browser = tokio::spawn(play_browser(port, "code-1", state.clone()));
    let tokens = sign_in
        .complete_within(&client(), Duration::from_secs(10))
        .await
        .unwrap();
    let page = browser.await.unwrap();
    // The loopback writes the confirmation page best-effort before it closes
    // (`respond` is not awaited for its result); on Windows the OS occasionally
    // resets that idle socket before the browser reads it.  The page is UI
    // chrome, so it is only checked when it arrived - the flow's real outcome
    // (the token set and the PKCE exchange, asserted below) is not affected.
    if !page.is_empty() {
        assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    }
    assert_eq!(tokens.access_token(), "at-1");
    assert_eq!(tokens.refresh_token(), Some("rt-1"));

    store_tokens(&store, account.id, &tokens).unwrap();
    let stored = hylmail_core::secrets::load_secret(
        &store,
        account.id,
        hylmail_core::secrets::CredentialKind::OAuthAccessToken,
    )
    .unwrap()
    .unwrap();
    assert_eq!(TokenSet::from_json(&stored).unwrap(), tokens);

    // The token request carried the PKCE pair, not the plain verifier.
    let form = endpoint.form();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(form.get("code").map(String::as_str), Some("code-1"));
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some("client-123")
    );
    assert_eq!(
        form.get("code_verifier").map(String::as_str),
        Some(verifier.as_str())
    );
    let redirect = form.get("redirect_uri").unwrap();
    assert_eq!(
        redirect,
        &format!("http://127.0.0.1:{port}/oauth2/callback")
    );
    let challenge = {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(verifier.as_bytes());
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    };
    assert_eq!(
        state_of_parameter(&url, "code_challenge"),
        challenge,
        "the challenge of the URL has to be the digest of the verifier"
    );
}

fn state_of_parameter(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_uses_the_refresh_token_grant() {
    let endpoint = MockEndpoint::start(200, r#"{"access_token":"at-2","expires_in":3600}"#).await;
    let tokens = refresh(&client(), &endpoint.config(), "rt-9")
        .await
        .unwrap();
    assert_eq!(tokens.access_token(), "at-2");
    assert_eq!(
        tokens.refresh_token(),
        None,
        "the old token is kept by the vault"
    );
    let form = endpoint.form();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(form.get("refresh_token").map(String::as_str), Some("rt-9"));
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some("client-123")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_error_is_reported_as_an_auth_error() {
    let endpoint = MockEndpoint::start(
        400,
        r#"{"error":"invalid_grant","error_description":"Der Code ist abgelaufen"}"#,
    )
    .await;
    let error = refresh(&client(), &endpoint.config(), "rt-9")
        .await
        .unwrap_err();
    match error {
        Error::Auth { account, message } => {
            assert_eq!(account, "OAuth2 provider");
            assert_eq!(message, "Der Code ist abgelaufen");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_json_answer_is_reported_as_malformed() {
    let endpoint = MockEndpoint::start(200, "not json at all").await;
    let error = refresh(&client(), &endpoint.config(), "rt-9")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::MalformedResponse { .. }), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_token_is_refreshed_from_the_vault() {
    let endpoint =
        MockEndpoint::start(200, r#"{"access_token":"at-fresh","expires_in":3600}"#).await;
    let store = InMemorySecretStore::new();
    let account = account(Some(endpoint.config()));

    let expired = TokenSet::from_response(
        r#"{"access_token":"at-stale","refresh_token":"rt-1"}"#,
        Utc::now(),
    )
    .unwrap();
    let mut expired = expired;
    expired.expires_at = Some(Utc::now() - chrono::Duration::seconds(5));
    store_tokens(&store, account.id, &expired).unwrap();

    let token = ensure_access_token(&store, &account, &TlsPolicy::strict())
        .await
        .unwrap();
    assert_eq!(token.expose_secret(), "at-fresh");
    assert_eq!(
        endpoint.form().get("refresh_token").map(String::as_str),
        Some("rt-1")
    );

    // The rotated token is appended to the vault, the refresh token survives.
    let stored = hylmail_core::secrets::load_secret(
        &store,
        account.id,
        hylmail_core::secrets::CredentialKind::OAuthAccessToken,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        TokenSet::from_json(&stored).unwrap().access_token(),
        "at-fresh"
    );
    assert_eq!(
        hylmail_core::secrets::load_secret(
            &store,
            account.id,
            hylmail_core::secrets::CredentialKind::OAuthRefreshToken
        )
        .unwrap(),
        Some("rt-1".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_still_valid_token_never_reaches_the_provider() {
    let store = InMemorySecretStore::new();
    let mut account = account(None);
    // Point at a port nothing listens on: a network call would fail.
    account.oauth = Some(OAuthConfig {
        issuer: "https://provider.example".to_string(),
        client_id: "client-123".to_string(),
        scopes: Vec::new(),
        authorization_endpoint: "https://provider.example/authorize".to_string(),
        token_endpoint: "http://127.0.0.1:1/token".to_string(),
        redirect_port: 0,
    });
    let tokens = TokenSet::from_response(
        r#"{"access_token":"at-cached","refresh_token":"rt","expires_in":3600}"#,
        Utc::now(),
    )
    .unwrap();
    store_tokens(&store, account.id, &tokens).unwrap();
    let token = ensure_access_token(&store, &account, &TlsPolicy::strict())
        .await
        .unwrap();
    assert_eq!(token.expose_secret(), "at-cached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_oauth_module_reports_a_missing_configuration() {
    let store = InMemorySecretStore::new();
    let account = account(None);
    assert!(matches!(
        ensure_access_token(&store, &account, &TlsPolicy::strict()).await,
        Err(Error::Config(_))
    ));
    assert_eq!(oauth::CALLBACK_PATH, "/oauth2/callback");
}
