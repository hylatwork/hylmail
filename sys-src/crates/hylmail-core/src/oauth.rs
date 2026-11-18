//! OAuth2 authorisation code flow with PKCE over a loopback redirect.
//!
//! The flow is the one RFC 8252 prescribes for native applications:
//!
//! 1. [`Loopback::bind`] opens a listener on `127.0.0.1` and the port it got is
//!    the redirect URI, so no custom scheme and no second instance of the
//!    application is involved,
//! 2. [`AuthorizationRequest::new`] builds the URL with a fresh PKCE verifier
//!    (`S256`, never `plain`) and a random `state`,
//! 3. the system browser opens that URL and the provider redirects back to the
//!    loopback listener with `?code=…&state=…`,
//! 4. [`exchange_code`] swaps the code for an access and a refresh token.
//!
//! Two entry points sit on top of that: [`SignIn`] drives steps 1-4 for one
//! account and [`ensure_access_token`] is what the protocol clients call when
//! they need a bearer token, refreshing it silently when it is about to expire.
//!
//! Nothing here prints a token: [`Pkce`], [`TokenSet`] and the credentials of
//! the flow all have a `Debug` implementation that redacts them.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout as with_timeout, Instant};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::models::{Account, OAuthConfig};
use crate::net::{Timeouts, TlsPolicy};
use crate::secrets::{secret, CredentialKey, CredentialKind, SecretStore};
use crate::soap::http::HttpClient;

/// Path the provider redirects to, as registered with the provider.
pub const CALLBACK_PATH: &str = "/oauth2/callback";

/// Default deadline for the user to finish the sign-in in the browser.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// An access token is refreshed when it expires within this window.
const REFRESH_SKEW_SECONDS: i64 = 60;

/// Lifetime assumed for a token whose response carried no `expires_in`.
const DEFAULT_LIFETIME_SECONDS: i64 = 3600;

/// Upper bound on the HTTP request head the loopback listener reads.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// A PKCE verifier and the matching `S256` challenge.
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// Generates a fresh verifier (64 characters) and its challenge.
    #[must_use]
    pub fn generate() -> Self {
        Self::from_verifier(random_token(64))
    }

    /// Builds the challenge for an existing verifier.
    ///
    /// The verifier of a flow that was interrupted can be resumed this way,
    /// which is what [`CredentialKind::OAuthCodeVerifier`] is stored for.
    #[must_use]
    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        let verifier = verifier.into();
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }

    /// The verifier, sent in the token request.
    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The challenge, sent in the authorisation request.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"[REDACTED]")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// `BASE64URL(SHA256(verifier))`, RFC 7636 section 4.2.
fn challenge_for(verifier: &str) -> String {
    use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};

    let digest = Sha256::digest(verifier.as_bytes());
    BASE64_URL_SAFE_NO_PAD.encode(digest)
}

/// A random string over the PKCE/`state` alphabet (`[A-Za-z0-9-]` here).
///
/// The bytes come from the operating system random number generator through
/// `uuid`, which uses it for its v4 variant; two v4 UUIDs are 256 random bits.
fn random_token(characters: usize) -> String {
    let mut out = String::with_capacity(characters + 32);
    while out.len() < characters {
        out.push_str(&Uuid::new_v4().simple().to_string());
    }
    out.truncate(characters);
    out
}

/// The authorisation URL plus the values that have to survive until the token
/// request.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    /// URL to open in the browser.
    pub url: String,
    /// PKCE pair of this flow.
    pub pkce: Pkce,
    /// Anti-forgery value echoed back by the provider.
    pub state: String,
    /// Redirect URI the provider will send the code to.
    pub redirect_uri: String,
}

impl AuthorizationRequest {
    /// Builds the authorisation URL for `redirect_uri`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidSettings`] when the endpoint is not an absolute URL or
    /// the configuration is incomplete.
    pub fn new(config: &OAuthConfig, redirect_uri: impl Into<String>) -> Result<Self> {
        config.validate()?;
        let redirect_uri = redirect_uri.into();
        let pkce = Pkce::generate();
        let state = random_token(32);

        let mut url = url::Url::parse(config.authorization_endpoint.trim()).map_err(|error| {
            Error::InvalidSettings {
                field: "oauth.authorization_endpoint",
                message: format!("not a valid URL: {error}"),
            }
        })?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &config.client_id);
            query.append_pair("redirect_uri", &redirect_uri);
            query.append_pair("scope", &config.scope_string());
            query.append_pair("state", &state);
            query.append_pair("code_challenge", pkce.challenge());
            query.append_pair("code_challenge_method", "S256");
        }

        Ok(Self {
            url: url.to_string(),
            pkce,
            state,
            redirect_uri,
        })
    }
}

/// A listener on the loopback interface that receives the redirect.
#[derive(Debug)]
pub struct Loopback {
    listener: TcpListener,
    port: u16,
}

impl Loopback {
    /// Binds `127.0.0.1:port`; pass `0` to let the operating system choose.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the port is taken or the socket cannot be created.
    pub async fn bind(port: u16) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|source| Error::Io {
                path: PathBuf::from(format!("127.0.0.1:{port}")),
                source,
            })?;
        let port = listener
            .local_addr()
            .map_err(|source| Error::Io {
                path: PathBuf::from("127.0.0.1"),
                source,
            })?
            .port();
        Ok(Self { listener, port })
    }

    /// The bound port, the one that has to be registered as the redirect.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI derived from the bound port.
    #[must_use]
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{CALLBACK_PATH}", self.port)
    }

    /// Waits for the provider to redirect back and returns the code.
    ///
    /// Requests that are not the callback (a browser asking for
    /// `/favicon.ico`, a probe) are answered and ignored.  A callback whose
    /// `state` does not match is refused, because a code that arrives without
    /// the value this application generated belongs to somebody else's flow.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] when the deadline passes, [`Error::Auth`] when the
    /// provider reports a refusal, [`Error::Protocol`] for a malformed
    /// callback.
    pub async fn wait_for_code(self, expected_state: &str, deadline: Duration) -> Result<String> {
        let start = Instant::now();
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(Error::Timeout {
                    operation: "OAuth2 redirect".to_string(),
                    seconds: deadline.as_secs(),
                });
            }
            let accepted = match with_timeout(remaining, self.listener.accept()).await {
                Ok(Ok(accepted)) => accepted,
                Ok(Err(source)) => {
                    return Err(Error::Io {
                        path: PathBuf::from(format!("127.0.0.1:{}", self.port)),
                        source,
                    })
                }
                Err(_) => {
                    return Err(Error::Timeout {
                        operation: "OAuth2 redirect".to_string(),
                        seconds: deadline.as_secs(),
                    })
                }
            };
            let (mut stream, _peer) = accepted;
            let head = read_head(&mut stream).await;
            if let Some(head) = head.as_deref() {
                if request_path(head) != CALLBACK_PATH {
                    // Browsers probe for `/favicon.ico` while the user still
                    // has to confirm; answering and waiting keeps the flow
                    // alive instead of reporting a broken redirect.
                    let _ = respond(&mut stream, 404, "Not Found", ERROR_PAGE).await;
                    let _ = stream.shutdown().await;
                    continue;
                }
            }
            match head.as_deref().map(parse_callback) {
                Some(Ok(callback)) if callback.state == expected_state => {
                    let _ = respond(&mut stream, 200, "OK", SUCCESS_PAGE).await;
                    let _ = stream.shutdown().await;
                    return Ok(callback.code);
                }
                Some(Ok(_)) => {
                    let _ = respond(&mut stream, 400, "Bad Request", STATE_PAGE).await;
                    let _ = stream.shutdown().await;
                    return Err(Error::Auth {
                        account: "OAuth2 provider".to_string(),
                        message: "the redirect arrived with a foreign `state` value".to_string(),
                    });
                }
                Some(Err(error)) => {
                    let _ = respond(&mut stream, 400, "Bad Request", ERROR_PAGE).await;
                    let _ = stream.shutdown().await;
                    return Err(error);
                }
                None => {
                    // Not a request head we can read: answer and keep waiting.
                    let _ = respond(&mut stream, 400, "Bad Request", ERROR_PAGE).await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    }
}

/// A callback the provider redirected to the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    /// Authorisation code to exchange.
    pub code: String,
    /// `state` value that has to match the one sent in the request.
    pub state: String,
}

/// Parses the request line of a redirect (`GET /oauth2/callback?… HTTP/1.1`).
///
/// # Errors
///
/// [`Error::Auth`] when the provider sent `error=…`, [`Error::Protocol`] when
/// the request does not carry a code and a state.
pub fn parse_callback(request: &str) -> Result<Callback> {
    let request_line = request.lines().next().unwrap_or_default();
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let query = target
        .split_once('?')
        .map(|(_, query)| query.to_string())
        .unwrap_or_default();

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_description = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        let detail = error_description.unwrap_or(error);
        return Err(Error::Auth {
            account: "OAuth2 provider".to_string(),
            message: detail,
        });
    }

    match (code, state) {
        (Some(code), Some(state)) => Ok(Callback { code, state }),
        _ => Err(Error::Protocol {
            protocol: "OAuth2",
            command: "redirect".to_string(),
            message: "the redirect carried no `code` and `state` pair".to_string(),
        }),
    }
}

/// The path of a request line (`/oauth2/callback` for `/oauth2/callback?…`).
#[must_use]
pub fn request_path(request: &str) -> &str {
    let request_line = request.lines().next().unwrap_or_default();
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    target.split('?').next().unwrap_or_default()
}

/// Answer shown in the browser after a successful sign-in.
const SUCCESS_PAGE: &str = "<!DOCTYPE html><html lang=\"de\"><head><meta charset=\"utf-8\">\
<title>HylMail</title></head><body style=\"font-family:sans-serif;background:#1e1e22;color:#e6e6e6\">\
<h1>Anmeldung abgeschlossen</h1><p>Sie können dieses Fenster schließen und zu HylMail zurückkehren.</p>\
</body></html>";

/// Answer shown when the request was not the expected callback.
const ERROR_PAGE: &str = "<!DOCTYPE html><html lang=\"de\"><head><meta charset=\"utf-8\">\
<title>HylMail</title></head><body style=\"font-family:sans-serif;background:#1e1e22;color:#e6e6e6\">\
<h1>Anmeldung fehlgeschlagen</h1><p>HylMail konnte die Anmeldung nicht abschließen. Bitte versuchen \
Sie es erneut.</p></body></html>";

/// Answer shown when the `state` value does not match.
const STATE_PAGE: &str = "<!DOCTYPE html><html lang=\"de\"><head><meta charset=\"utf-8\">\
<title>HylMail</title></head><body style=\"font-family:sans-serif;background:#1e1e22;color:#e6e6e6\">\
<h1>Ungültige Antwort</h1><p>Die Antwort gehörte nicht zu dieser Anmeldung.</p></body></html>";

/// Writes a minimal HTTP answer.
async fn respond(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let answer = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(answer.as_bytes()).await
}

/// Reads a request head (up to the first empty line).
async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n")
                    || buffer.len() >= MAX_REQUEST_BYTES
                {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    String::from_utf8(buffer).ok()
}

/// One answer of the token endpoint.
#[derive(Debug, Clone, Default, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// The tokens of one account, as they are cached in the vault.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSet {
    /// Bearer token for IMAP `XOAUTH2`, SMTP and EWS.
    pub access_token: String,
    /// Token that yields the next access token; `None` when the provider
    /// returned none (a re-consent is needed to get one from Google).
    pub refresh_token: Option<String>,
    /// Moment the access token stops being valid.
    pub expires_at: Option<DateTime<Utc>>,
    /// `Bearer` in every implementation seen so far.
    pub token_type: Option<String>,
    /// Scopes the provider actually granted.
    pub scope: Option<String>,
}

impl TokenSet {
    /// Parses the JSON body of a token endpoint answer.
    ///
    /// # Errors
    ///
    /// [`Error::Auth`] when the provider answered with `error`, and
    /// [`Error::MalformedResponse`] when the body is not usable JSON.
    pub fn from_response(body: &str, now: DateTime<Utc>) -> Result<Self> {
        let response: TokenResponse =
            serde_json::from_str(body).map_err(|error| Error::MalformedResponse {
                protocol: "OAuth2",
                message: format!("token endpoint answered non-JSON: {error}"),
            })?;
        if let Some(error) = response.error {
            return Err(Error::Auth {
                account: "OAuth2 provider".to_string(),
                message: response.error_description.unwrap_or(error),
            });
        }
        let Some(access_token) = response.access_token else {
            return Err(Error::MalformedResponse {
                protocol: "OAuth2",
                message: "token endpoint answered without an access token".to_string(),
            });
        };
        let lifetime = response.expires_in.unwrap_or(DEFAULT_LIFETIME_SECONDS);
        Ok(Self {
            access_token,
            refresh_token: response.refresh_token,
            expires_at: Some(now + chrono::Duration::seconds(lifetime)),
            token_type: response.token_type,
            scope: response.scope,
        })
    }

    /// Serialises the set for the vault.
    ///
    /// # Errors
    ///
    /// [`Error::Json`] when the document cannot be written.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Reads a set back from the vault.
    ///
    /// # Errors
    ///
    /// [`Error::Json`] when the stored document is not a token set.
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// The bearer token.
    #[must_use]
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// The refresh token, when the provider issued one.
    #[must_use]
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    /// `true` when the access token is expired or expires within a minute.
    #[must_use]
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        match self.expires_at {
            Some(expires_at) => expires_at - now <= chrono::Duration::seconds(REFRESH_SKEW_SECONDS),
            None => true,
        }
    }
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Swaps an authorisation code for tokens.
///
/// # Errors
///
/// [`Error::Auth`] when the provider refuses the code (an expired code, a
/// verifier that does not match), the transport errors of [`HttpClient`] and
/// [`Error::MalformedResponse`] for an unusable answer.
pub async fn exchange_code(
    client: &HttpClient,
    config: &OAuthConfig,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenSet> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", config.client_id.as_str()),
        ("code_verifier", verifier),
    ];
    post_token(client, &config.token_endpoint, &form).await
}

/// Exchanges a refresh token for a new access token.
///
/// # Errors
///
/// See [`exchange_code`]; a rejected refresh token is [`Error::Auth`] and means
/// the user has to sign in again.
pub async fn refresh(
    client: &HttpClient,
    config: &OAuthConfig,
    refresh_token: &str,
) -> Result<TokenSet> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", config.client_id.as_str()),
    ];
    post_token(client, &config.token_endpoint, &form).await
}

/// Posts one form to the token endpoint and parses the answer.
async fn post_token(
    client: &HttpClient,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenSet> {
    let headers = [("Accept", "application/json".to_string())];
    let response = client.post_form(endpoint.trim(), form, &headers).await?;
    if !response.is_success() {
        // A refusal is still a JSON document in every implementation; report
        // the provider's wording rather than a bare status code.
        return match TokenSet::from_response(&response.body, Utc::now()) {
            Err(Error::Auth { message, .. }) => Err(Error::Auth {
                account: "OAuth2 provider".to_string(),
                message,
            }),
            Err(_) => Err(Error::Auth {
                account: "OAuth2 provider".to_string(),
                message: format!(
                    "token endpoint answered {} {}",
                    response.status,
                    truncate(&response.body)
                ),
            }),
            Ok(tokens) => Ok(tokens),
        };
    }
    TokenSet::from_response(&response.body, Utc::now())
}

/// Keeps an error message short enough for a status bar.
fn truncate(text: &str) -> String {
    let cleaned = text.trim().replace(['\r', '\n'], " ");
    if cleaned.chars().count() <= 200 {
        cleaned
    } else {
        let mut out: String = cleaned.chars().take(197).collect();
        out.push_str("...");
        out
    }
}

/// A sign-in that is waiting for the browser.
#[derive(Debug)]
pub struct SignIn {
    config: OAuthConfig,
    loopback: Loopback,
    request: AuthorizationRequest,
}

impl SignIn {
    /// Binds the loopback listener and builds the authorisation URL.
    ///
    /// # Errors
    ///
    /// The errors of [`Loopback::bind`] and [`AuthorizationRequest::new`].
    pub async fn begin(config: &OAuthConfig) -> Result<Self> {
        let loopback = Loopback::bind(config.redirect_port).await?;
        let request = AuthorizationRequest::new(config, loopback.redirect_uri())?;
        Ok(Self {
            config: config.clone(),
            loopback,
            request,
        })
    }

    /// The URL the user has to open.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.request.url
    }

    /// The port the provider redirects to.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.loopback.port()
    }

    /// The PKCE verifier, so that an interrupted flow can be resumed.
    #[must_use]
    pub fn verifier(&self) -> &str {
        self.request.pkce.verifier()
    }

    /// Waits for the redirect and exchanges the code, with [`DEFAULT_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// See [`Loopback::wait_for_code`] and [`exchange_code`].
    pub async fn complete(self, client: &HttpClient) -> Result<TokenSet> {
        self.complete_within(client, DEFAULT_TIMEOUT).await
    }

    /// [`SignIn::complete`] with an explicit deadline.
    ///
    /// # Errors
    ///
    /// See [`SignIn::complete`].
    pub async fn complete_within(
        self,
        client: &HttpClient,
        deadline: Duration,
    ) -> Result<TokenSet> {
        let code = self
            .loopback
            .wait_for_code(&self.request.state, deadline)
            .await?;
        exchange_code(
            client,
            &self.config,
            &code,
            self.request.pkce.verifier(),
            &self.request.redirect_uri,
        )
        .await
    }
}

/// Stores an access token (and the refresh token that came with it).
///
/// A provider that answers a refresh without a new refresh token keeps the old
/// one; losing it there would end the session an hour later.
///
/// # Errors
///
/// The errors of the vault and [`TokenSet::to_json`].
pub fn store_tokens(
    store: &dyn SecretStore,
    account_id: crate::models::AccountId,
    tokens: &TokenSet,
) -> Result<()> {
    store.set(
        &CredentialKey::new(account_id, CredentialKind::OAuthAccessToken),
        &secret(tokens.to_json()?),
    )?;
    let refresh = match tokens.refresh_token.clone() {
        Some(refresh) => Some(refresh),
        None => crate::secrets::load_secret(store, account_id, CredentialKind::OAuthRefreshToken)?,
    };
    if let Some(refresh) = refresh {
        store.set(
            &CredentialKey::new(account_id, CredentialKind::OAuthRefreshToken),
            &secret(refresh),
        )?;
    }
    // The verifier of the finished flow has no further use.
    store.delete(&CredentialKey::new(
        account_id,
        CredentialKind::OAuthCodeVerifier,
    ))?;
    Ok(())
}

/// The OAuth2 configuration of an account, or an error naming the account.
fn config_of(account: &Account) -> Result<&OAuthConfig> {
    account.oauth.as_ref().ok_or_else(|| {
        Error::Config(format!(
            "account `{}` uses OAuth2 but has no OAuth2 configuration",
            account.from_address()
        ))
    })
}

/// Runs the whole interactive flow and stores the tokens in the vault.
///
/// The URL is handed to the system browser, the redirect is awaited on the
/// loopback listener and the code is exchanged.  The PKCE verifier is written
/// to the vault first, so a crash in the browser leaves a resumable flow rather
/// than an orphaned verifier.
///
/// # Errors
///
/// The errors of [`SignIn::begin`], [`open_in_browser`], [`SignIn::complete`]
/// and the vault.
pub async fn sign_in(
    store: &dyn SecretStore,
    account: &Account,
    tls: &TlsPolicy,
) -> Result<TokenSet> {
    let config = config_of(account)?.clone();
    let sign_in = SignIn::begin(&config).await?;
    store.set(
        &CredentialKey::new(account.id, CredentialKind::OAuthCodeVerifier),
        &secret(sign_in.verifier().to_string()),
    )?;
    let url = sign_in.url().to_string();
    open_in_browser(&url)?;
    let client = HttpClient::new(tls, &Timeouts::default())?;
    let tokens = sign_in.complete(&client).await?;
    store_tokens(store, account.id, &tokens)?;
    Ok(tokens)
}

/// The access token of an OAuth2 account, refreshed when it is about to expire.
///
/// The protocol clients call this instead of reading the vault directly: a
/// cached token that is still valid costs one vault read, and one that is older
/// than an hour is replaced before the server sees it.
///
/// # Errors
///
/// [`Error::Auth`] when no token is stored (the user has to sign in again),
/// [`Error::Config`] when the account carries no OAuth2 configuration, and the
/// transport errors of the refresh request.
pub async fn ensure_access_token(
    store: &dyn SecretStore,
    account: &Account,
    tls: &TlsPolicy,
) -> Result<SecretString> {
    let now = Utc::now();
    if let Some(json) =
        crate::secrets::load_secret(store, account.id, CredentialKind::OAuthAccessToken)?
    {
        if let Ok(tokens) = TokenSet::from_json(&json) {
            if !tokens.needs_refresh(now) {
                return Ok(secret(tokens.access_token));
            }
        }
    }

    let config = config_of(account)?.clone();
    let stored = crate::secrets::load_secret(store, account.id, CredentialKind::OAuthRefreshToken)?;
    let Some(refresh_token) = stored else {
        return Err(Error::Auth {
            account: account.from_address().to_string(),
            message: "kein OAuth2-Token gespeichert; bitte das Konto neu anmelden".to_string(),
        });
    };

    let client = HttpClient::new(tls, &Timeouts::default())?;
    let tokens = refresh(&client, &config, &refresh_token).await?;
    store_tokens(store, account.id, &tokens)?;
    Ok(secret(tokens.access_token))
}

/// Program and arguments that open a URL in the system browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserCommand {
    /// Executable to start.
    pub program: String,
    /// Arguments, with the URL last.
    pub args: Vec<String>,
}

/// The browser command of the current platform.
///
/// Split out from [`open_in_browser`] so that the argument vector - the part
/// that is easy to get wrong on Windows - is testable without starting a
/// browser.
#[must_use]
pub fn browser_command(url: &str) -> BrowserCommand {
    if cfg!(target_os = "windows") {
        // `start` is a `cmd` builtin, and the empty argument is the window
        // title: without it a URL that contains `&` would be split into two
        // commands.
        BrowserCommand {
            program: "cmd".to_string(),
            args: vec![
                "/C".to_string(),
                "start".to_string(),
                String::new(),
                url.to_string(),
            ],
        }
    } else if cfg!(target_os = "macos") {
        BrowserCommand {
            program: "open".to_string(),
            args: vec![url.to_string()],
        }
    } else {
        BrowserCommand {
            program: "xdg-open".to_string(),
            args: vec![url.to_string()],
        }
    }
}

/// Opens a URL in the system browser, without waiting for it to exit.
///
/// # Errors
///
/// [`Error::Config`] when the helper program cannot be started (a headless
/// server, a container without `xdg-open`).
pub fn open_in_browser(url: &str) -> Result<()> {
    let command = browser_command(url);
    std::process::Command::new(&command.program)
        .args(&command.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|source| {
            Error::Config(format!(
                "could not start `{}` to open the browser: {source}",
                command.program
            ))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::models::{AccountId, AccountKind, AuthKind, Endpoint};
    use crate::secrets::InMemorySecretStore;

    fn oauth_config() -> OAuthConfig {
        OAuthConfig {
            issuer: "example".to_string(),
            client_id: "client-123".to_string(),
            scopes: vec!["mail.read".to_string(), "mail.send".to_string()],
            authorization_endpoint: "https://provider.example/authorize".to_string(),
            token_endpoint: "https://provider.example/token".to_string(),
            redirect_port: 0,
        }
    }

    /// An IMAP account with the given OAuth2 configuration, for the token helpers.
    fn oauth_account(oauth: Option<OAuthConfig>) -> Account {
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
            created_at: crate::models::utc_now(),
        }
    }

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        // Appendix B of RFC 7636: the challenge of a known verifier.
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pkce.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_generated_verifier_is_usable() {
        let pkce = Pkce::generate();
        let verifier = pkce.verifier();
        assert!(
            (43..=128).contains(&verifier.len()),
            "verifier has {} characters",
            verifier.len()
        );
        assert!(verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
        assert!(!pkce.challenge().contains(['=', '+', '/']));
        assert_ne!(pkce.verifier(), pkce.challenge());
    }

    #[test]
    fn two_flows_never_share_a_verifier_or_a_state() {
        let first =
            AuthorizationRequest::new(&oauth_config(), "http://127.0.0.1:1/oauth2/callback")
                .unwrap();
        let second =
            AuthorizationRequest::new(&oauth_config(), "http://127.0.0.1:1/oauth2/callback")
                .unwrap();
        assert_ne!(first.pkce.verifier(), second.pkce.verifier());
        assert_ne!(first.state, second.state);
    }

    #[test]
    fn debug_never_prints_the_verifier() {
        let pkce = Pkce::generate();
        let rendered = format!("{pkce:?}");
        assert!(!rendered.contains(pkce.verifier()));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn the_authorization_url_carries_every_parameter() {
        let request =
            AuthorizationRequest::new(&oauth_config(), "http://127.0.0.1:4040/oauth2/callback")
                .unwrap();
        let url = url::Url::parse(&request.url).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.path(), "/authorize");
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            pairs.get("client_id").map(String::as_str),
            Some("client-123")
        );
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:4040/oauth2/callback")
        );
        assert_eq!(
            pairs.get("scope").map(String::as_str),
            Some("mail.read mail.send")
        );
        assert_eq!(
            pairs.get("state").map(String::as_str),
            Some(request.state.as_str())
        );
        assert_eq!(
            pairs.get("code_challenge").map(String::as_str),
            Some(request.pkce.challenge())
        );
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
    }

    #[test]
    fn an_existing_query_of_the_endpoint_survives() {
        let mut config = oauth_config();
        config.authorization_endpoint = "https://provider.example/authorize?tenant=abc".to_string();
        let request =
            AuthorizationRequest::new(&config, "http://127.0.0.1:1/oauth2/callback").unwrap();
        let url = url::Url::parse(&request.url).unwrap();
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("tenant").map(String::as_str), Some("abc"));
        assert_eq!(
            pairs.get("client_id").map(String::as_str),
            Some("client-123")
        );
    }

    #[test]
    fn an_incomplete_configuration_is_refused() {
        let mut config = oauth_config();
        config.client_id.clear();
        assert!(matches!(
            AuthorizationRequest::new(&config, "http://127.0.0.1:1/oauth2/callback"),
            Err(Error::InvalidSettings { .. })
        ));
        let mut config = oauth_config();
        config.token_endpoint.clear();
        assert!(matches!(
            AuthorizationRequest::new(&config, "http://127.0.0.1:1/oauth2/callback"),
            Err(Error::InvalidSettings { .. })
        ));
        let mut config = oauth_config();
        config.authorization_endpoint = "not a url".to_string();
        assert!(matches!(
            AuthorizationRequest::new(&config, "http://127.0.0.1:1/oauth2/callback"),
            Err(Error::InvalidSettings { .. })
        ));
    }

    #[test]
    fn a_redirect_is_parsed() {
        let head =
            "GET /oauth2/callback?code=abc%2F1&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let callback = parse_callback(head).unwrap();
        assert_eq!(callback.code, "abc/1");
        assert_eq!(callback.state, "xyz");
    }

    #[test]
    fn a_provider_error_becomes_an_auth_error() {
        let head = "GET /oauth2/callback?error=access_denied&error_description=Der+Nutzer+hat+abgelehnt HTTP/1.1\r\n\r\n";
        let error = parse_callback(head).unwrap_err();
        match error {
            Error::Auth { message, .. } => assert!(message.contains("abgelehnt")),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_redirect_without_code_is_a_protocol_error() {
        let head = "GET /oauth2/callback?state=xyz HTTP/1.1\r\n\r\n";
        assert!(matches!(parse_callback(head), Err(Error::Protocol { .. })));
    }

    #[test]
    fn a_token_response_is_parsed() {
        let now = Utc::now();
        let body = r#"{"access_token":"at","refresh_token":"rt","expires_in":3600,
            "token_type":"Bearer","scope":"mail.read"}"#;
        let tokens = TokenSet::from_response(body, now).unwrap();
        assert_eq!(tokens.access_token(), "at");
        assert_eq!(tokens.refresh_token(), Some("rt"));
        assert_eq!(tokens.token_type.as_deref(), Some("Bearer"));
        assert_eq!(tokens.scope.as_deref(), Some("mail.read"));
        assert!(!tokens.needs_refresh(now));
        assert_eq!(
            tokens.expires_at,
            Some(now + chrono::Duration::seconds(3600))
        );
    }

    #[test]
    fn a_token_response_without_expiry_gets_an_hour() {
        let now = Utc::now();
        let tokens = TokenSet::from_response(r#"{"access_token":"at"}"#, now).unwrap();
        assert_eq!(
            tokens.expires_at,
            Some(now + chrono::Duration::seconds(DEFAULT_LIFETIME_SECONDS))
        );
    }

    #[test]
    fn a_token_error_is_an_auth_error() {
        let body = r#"{"error":"invalid_grant","error_description":"code expired"}"#;
        match TokenSet::from_response(body, Utc::now()) {
            Err(Error::Auth { message, .. }) => assert_eq!(message, "code expired"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn an_unusable_token_body_is_a_malformed_response() {
        assert!(matches!(
            TokenSet::from_response("not json", Utc::now()),
            Err(Error::MalformedResponse { .. })
        ));
        assert!(matches!(
            TokenSet::from_response("{}", Utc::now()),
            Err(Error::MalformedResponse { .. })
        ));
    }

    #[test]
    fn needs_refresh_respects_the_skew() {
        let now = Utc::now();
        let mut tokens = TokenSet::from_response(r#"{"access_token":"at"}"#, now).unwrap();
        tokens.expires_at = Some(now + chrono::Duration::seconds(30));
        assert!(tokens.needs_refresh(now), "30 s left is inside the skew");
        tokens.expires_at = Some(now - chrono::Duration::seconds(1));
        assert!(tokens.needs_refresh(now));
        tokens.expires_at = Some(now + chrono::Duration::seconds(3600));
        assert!(!tokens.needs_refresh(now));
        tokens.expires_at = None;
        assert!(tokens.needs_refresh(now));
    }

    #[test]
    fn a_token_set_roundtrips_through_json() {
        let tokens = TokenSet::from_response(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":60}"#,
            Utc::now(),
        )
        .unwrap();
        let restored = TokenSet::from_json(&tokens.to_json().unwrap()).unwrap();
        assert_eq!(restored, tokens);
    }

    #[test]
    fn debug_of_a_token_set_hides_the_secrets() {
        let tokens = TokenSet::from_response(
            r#"{"access_token":"secret-access","refresh_token":"secret-refresh"}"#,
            Utc::now(),
        )
        .unwrap();
        let rendered = format!("{tokens:?}");
        assert!(!rendered.contains("secret-access"));
        assert!(!rendered.contains("secret-refresh"));
    }

    #[test]
    fn the_browser_command_is_platform_specific() {
        let command = browser_command("https://provider.example/authorize?x=1");
        let url = "https://provider.example/authorize?x=1";
        match std::env::consts::OS {
            "windows" => {
                assert_eq!(command.program, "cmd");
                // The empty title argument keeps `&` from splitting the URL.
                assert_eq!(command.args, vec!["/C", "start", "", url]);
            }
            "macos" => {
                assert_eq!(command.program, "open");
                assert_eq!(command.args, vec![url]);
            }
            _ => {
                assert_eq!(command.program, "xdg-open");
                assert_eq!(command.args, vec![url]);
            }
        }
    }

    #[test]
    fn a_long_error_message_is_shortened() {
        let long = "x".repeat(500);
        assert_eq!(truncate(&long).chars().count(), 200);
        assert!(truncate("kurz").eq("kurz"));
        assert!(!truncate("a\nb").contains('\n'));
    }

    #[tokio::test]
    async fn the_loopback_listener_answers_the_redirect() {
        let loopback = Loopback::bind(0).await.unwrap();
        let port = loopback.port();
        let redirect = loopback.redirect_uri();
        assert_eq!(redirect, format!("http://127.0.0.1:{port}{CALLBACK_PATH}"));

        let browser = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(
                    b"GET /oauth2/callback?code=abc&state=state-1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                )
                .await
                .unwrap();
            let mut answer = String::new();
            // The loopback answers once and closes; on Windows that close can
            // reach the browser as a reset before it reads the page, so the read
            // is not treated as fatal (the flow outcome is asserted below).
            let _ = stream.read_to_string(&mut answer).await;
            answer
        });

        let code = loopback
            .wait_for_code("state-1", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(code, "abc");
        // The redirect code is the contract; the confirmation page is courtesy
        // UI and is only checked when the browser actually received it.
        let answer = browser.await.unwrap();
        if !answer.is_empty() {
            assert!(answer.starts_with("HTTP/1.1 200 OK"));
            assert!(answer.contains("Anmeldung abgeschlossen"));
        }
    }

    #[tokio::test]
    async fn a_foreign_state_is_refused() {
        let loopback = Loopback::bind(0).await.unwrap();
        let port = loopback.port();
        let browser = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let _ = stream
                .write_all(b"GET /oauth2/callback?code=abc&state=other HTTP/1.1\r\n\r\n")
                .await;
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer).await;
            answer
        });
        let error = loopback
            .wait_for_code("state-1", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Auth { .. }));
        // The refusal is the contract; the 400 page is only checked when the
        // browser received it (a Windows close can reset the socket first).
        let answer = browser.await.unwrap();
        if !answer.is_empty() {
            assert!(answer.starts_with("HTTP/1.1 400 Bad Request"));
        }
    }

    #[tokio::test]
    async fn an_unknown_request_is_answered_and_ignored() {
        let loopback = Loopback::bind(0).await.unwrap();
        let port = loopback.port();
        tokio::spawn(async move {
            // A browser asking for its favicon must not end the flow.
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let _ = stream.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n").await;
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer).await;
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let _ = stream
                .write_all(b"GET /oauth2/callback?code=later&state=s HTTP/1.1\r\n\r\n")
                .await;
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer).await;
        });
        let code = loopback
            .wait_for_code("s", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(code, "later");
    }

    #[tokio::test]
    async fn a_silent_provider_times_out() {
        let loopback = Loopback::bind(0).await.unwrap();
        let error = loopback
            .wait_for_code("s", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Timeout { .. }));
    }

    #[test]
    fn storing_tokens_keeps_the_old_refresh_token() {
        let store = InMemorySecretStore::new();
        let account = AccountId::new();
        let first = TokenSet::from_response(
            r#"{"access_token":"at1","refresh_token":"rt1"}"#,
            Utc::now(),
        )
        .unwrap();
        store_tokens(&store, account, &first).unwrap();
        assert_eq!(
            crate::secrets::load_secret(&store, account, CredentialKind::OAuthRefreshToken)
                .unwrap(),
            Some("rt1".to_string())
        );

        // Google answers a refresh without a new refresh token.
        let second = TokenSet::from_response(r#"{"access_token":"at2"}"#, Utc::now()).unwrap();
        store_tokens(&store, account, &second).unwrap();
        assert_eq!(
            crate::secrets::load_secret(&store, account, CredentialKind::OAuthRefreshToken)
                .unwrap(),
            Some("rt1".to_string())
        );
        let stored = crate::secrets::load_secret(&store, account, CredentialKind::OAuthAccessToken)
            .unwrap()
            .unwrap();
        assert_eq!(TokenSet::from_json(&stored).unwrap().access_token(), "at2");
    }

    #[tokio::test]
    async fn a_valid_token_needs_no_network() {
        let store = InMemorySecretStore::new();
        let account = oauth_account(Some(oauth_config()));
        let tokens = TokenSet::from_response(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#,
            Utc::now(),
        )
        .unwrap();
        crate::secrets::SecretStore::set(
            &store,
            &CredentialKey::new(account.id, CredentialKind::OAuthAccessToken),
            &secret(tokens.to_json().unwrap()),
        )
        .unwrap();
        let value = ensure_access_token(&store, &account, &TlsPolicy::strict())
            .await
            .unwrap();
        assert_eq!(secrecy::ExposeSecret::expose_secret(&value), "at");
    }

    #[tokio::test]
    async fn without_stored_tokens_the_user_is_asked_to_sign_in() {
        let store = InMemorySecretStore::new();
        let account = oauth_account(Some(oauth_config()));
        let error = ensure_access_token(&store, &account, &TlsPolicy::strict())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Auth { .. }));
    }

    #[tokio::test]
    async fn an_account_without_configuration_is_a_config_error() {
        let store = InMemorySecretStore::new();
        let account = oauth_account(None);
        let error = ensure_access_token(&store, &account, &TlsPolicy::strict())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
    }
}
