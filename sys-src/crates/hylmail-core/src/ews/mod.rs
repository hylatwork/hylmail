//! Exchange Web Services (EWS) client for on-premises Exchange.
//!
//! HylMail talks to EWS over SOAP/HTTP; there is no `MAPI` and no autodiscover.
//! The client authenticates, walks the folder tree, synchronises items
//! incrementally through `SyncFolderItems`, downloads the raw MIME content of a
//! message on demand, and changes read state, flags, folders and deletion.
//!
//! ## Authentication
//!
//! * [`AuthKind::Password`] sends `Authorization: Basic …`; the login name may
//!   be `domain\user` for older installations.
//! * [`AuthKind::OAuth2`] sends `Authorization: Bearer …`, which is what modern
//!   and hybrid installations expect.
//! * [`AuthKind::Ntlm`] performs the `MS-NLMP` handshake, which Exchange on
//!   premises is the one mechanism it is guaranteed to offer.  NTLM binds its
//!   challenge to a single TCP connection, so this client is built on a
//!   connection that is kept for it alone (see [`HttpClient::ntlm`]); the login
//!   name may be `domain\user` or a UPN, exactly as for [`AuthKind::Password`].
//!
//! ## Versioning
//!
//! Every request sends a `RequestServerVersion` header; the default is
//! [`EwsVersion::Exchange2013`], which every maintained on-premises
//! installation supports.  The `ServerVersionInfo` of the response header
//! reports what the server actually is and is exposed through
//! [`EwsOutcome::server_version`] for the account dialog.

mod convert;
mod request;
mod synchronise;
mod types;

use std::collections::HashSet;

use secrecy::ExposeSecret;

use crate::error::{Error, Result};
use crate::models::account::{AuthKind, Endpoint, TlsMode};
use crate::models::folder::Folder;
use crate::models::ids::AccountId;
use crate::net::ntlm::Ntlm;
use crate::net::{Timeouts, TlsPolicy};
use crate::secrets::Credential;
use crate::soap::http::HttpResponse;
use crate::soap::{fault_message, Envelope, HttpClient, Node};

pub use convert::{
    distinguished_for, folder_kind_of, folder_to_model, item_to_header, to_models,
    DISTINGUISHED_FOLDERS,
};
pub use request::FolderRef;
pub use synchronise::{sync_folder, sync_folders, LocalItem, MAX_CHANGES_RETURNED};
pub use types::{EwsFolder, EwsItem, ItemId, SyncBatch};

/// How deep the folder tree is walked, and how many folders are accepted.
///
/// Exchange has no "list everything" operation, so `FindFolder` is called once
/// per folder.  Both limits keep a broken server that reports a folder as its
/// own child from making the client loop forever.
const MAX_FOLDER_DEPTH: usize = 8;
const MAX_FOLDERS: usize = 4_000;

/// `ExchangeImpersonation` for a shared mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impersonation {
    /// SMTP address or UPN of the mailbox to act for.
    pub smtp_address: String,
}

/// The EWS schema version HylMail asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EwsVersion {
    /// Exchange 2007 SP1.
    Exchange2007Sp1,
    /// Exchange 2010.
    Exchange2010,
    /// Exchange 2010 SP1.
    Exchange2010Sp1,
    /// Exchange 2010 SP2.
    Exchange2010Sp2,
    /// Exchange 2013, the default.
    #[default]
    Exchange2013,
    /// Exchange 2013 SP1.
    Exchange2013Sp1,
    /// Exchange 2016 and later.
    Exchange2016,
}

impl EwsVersion {
    /// The string the `RequestServerVersion` header carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exchange2007Sp1 => "Exchange2007_SP1",
            Self::Exchange2010 => "Exchange2010",
            Self::Exchange2010Sp1 => "Exchange2010_SP1",
            Self::Exchange2010Sp2 => "Exchange2010_SP2",
            Self::Exchange2013 => "Exchange2013",
            Self::Exchange2013Sp1 => "Exchange2013_SP1",
            Self::Exchange2016 => "Exchange2016",
        }
    }
}

/// Everything needed to talk to one Exchange mailbox.
#[derive(Debug, Clone)]
pub struct EwsConfig {
    /// Endpoint of the web service (host, port and TLS mode).
    pub endpoint: Endpoint,
    /// Login name; `domain\user` or a UPN.
    pub username: String,
    /// How to authenticate.
    pub auth: AuthKind,
    /// Certificate policy.
    pub policy: TlsPolicy,
    /// Deadlines.
    pub timeouts: Timeouts,
    /// Schema version to request.
    pub version: EwsVersion,
    /// Act for another mailbox (shared mailbox support).
    pub impersonation: Option<Impersonation>,
    /// EWS time zone id, e.g. `W. Europe Standard Time`.
    pub time_zone: Option<String>,
    /// Path of the web service on the host.
    pub service_path: String,
}

impl EwsConfig {
    /// Builds a configuration with the defaults HylMail uses.
    #[must_use]
    pub fn new(endpoint: Endpoint, username: impl Into<String>, auth: AuthKind) -> Self {
        Self {
            endpoint,
            username: username.into(),
            auth,
            policy: TlsPolicy::strict(),
            timeouts: Timeouts::default(),
            version: EwsVersion::default(),
            impersonation: None,
            time_zone: None,
            service_path: "/EWS/Exchange.asmx".to_string(),
        }
    }

    /// The URL every request goes to.
    #[must_use]
    pub fn url(&self) -> String {
        let scheme = match self.endpoint.tls {
            TlsMode::None => "http",
            TlsMode::StartTls | TlsMode::Implicit => "https",
        };
        let path = if self.service_path.starts_with('/') {
            self.service_path.clone()
        } else {
            format!("/{}", self.service_path)
        };
        format!(
            "{scheme}://{}:{}{path}",
            self.endpoint.host, self.endpoint.port
        )
    }

    /// `true` when the connection is encrypted from the first byte.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.endpoint.tls == TlsMode::Implicit
    }
}

/// What a connection reported about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EwsOutcome {
    /// Host that was contacted.
    pub host: String,
    /// URL that was used.
    pub url: String,
    /// `ServerVersionInfo/@Version` of the response header, when present.
    pub server_version: Option<String>,
    /// `ServerVersionInfo/@MinorVersion`.
    pub server_minor_version: Option<String>,
    /// Scheme version that was requested.
    pub requested_version: String,
    /// `true` when the connection is encrypted.
    pub encrypted: bool,
    /// Network round trip of the last request.
    pub round_trip_ms: u64,
}

impl EwsOutcome {
    /// A one line description for the account dialog.
    #[must_use]
    pub fn describe(&self) -> String {
        let version = self.server_version.as_deref().unwrap_or("unknown version");
        let encryption = if self.encrypted {
            "encrypted"
        } else {
            "cleartext"
        };
        format!(
            "Exchange {} at {} ({encryption}, {} ms)",
            version, self.host, self.round_trip_ms
        )
    }
}

/// The resolved `Authorization` header value.
///
/// The credential is turned into the header value once, when the client is
/// built; the password itself is not kept alive afterwards.
#[derive(Debug, Clone)]
struct AuthHeader(String);

impl AuthHeader {
    fn value(&self) -> &str {
        &self.0
    }
}

/// How every request of an [`EwsClient`] authenticates.
enum Authentication {
    /// A header that is computed once and sent with every request (`Basic`,
    /// `Bearer`).
    Header(AuthHeader),
    /// NTLM, where the transport performs a handshake per request because the
    /// answer to a challenge is bound to the challenge's connection.
    Ntlm(Ntlm),
}

impl std::fmt::Debug for Authentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Both variants carry a credential; neither one is ever printed.
        match self {
            Self::Header(_) => f.write_str("Authentication::Header([REDACTED])"),
            Self::Ntlm(_) => f.write_str("Authentication::Ntlm([REDACTED])"),
        }
    }
}

/// One authenticated EWS client.
pub struct EwsClient {
    http: HttpClient,
    config: EwsConfig,
    auth: Authentication,
    outcome: EwsOutcome,
}

impl std::fmt::Debug for EwsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The authentication header is a credential and is never printed.
        f.debug_struct("EwsClient")
            .field("url", &self.config.url())
            .field("username", &self.config.username)
            .field("version", &self.config.version.as_str())
            .finish_non_exhaustive()
    }
}

impl EwsClient {
    /// Builds the client and verifies that the server answers.
    ///
    /// `GetFolder` on the inbox is the cheapest authenticated request: it fails
    /// for a wrong password and carries the `ServerVersionInfo` header.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when the account kind and the credential do
    /// not match, [`Error::Auth`] when the credentials are rejected (including a
    /// malformed NTLM challenge), [`Error::Tls`]/[`Error::Connect`] for transport
    /// failures and [`Error::Protocol`] for a SOAP fault.
    pub async fn login(config: &EwsConfig, credential: Credential<'_>) -> Result<Self> {
        let auth = match credential {
            Credential::Password(password) => {
                if config.auth == AuthKind::OAuth2 {
                    return Err(Error::Unsupported(
                        "an OAuth2 account needs an access token, not a password".to_string(),
                    ));
                }
                if config.auth == AuthKind::Ntlm {
                    Authentication::Ntlm(Ntlm::new(&config.username, password))
                } else {
                    Authentication::Header(AuthHeader(basic_header(
                        &config.username,
                        password.expose_secret(),
                    )))
                }
            }
            Credential::OAuth2 { token, .. } => {
                if config.auth == AuthKind::Ntlm {
                    return Err(Error::Unsupported(
                        "an NTLM account needs a password, not an OAuth2 token".to_string(),
                    ));
                }
                Authentication::Header(AuthHeader(format!("Bearer {}", token.expose_secret())))
            }
        };
        let http = match &auth {
            Authentication::Header(_) => HttpClient::new(&config.policy, &config.timeouts)?,
            Authentication::Ntlm(_) => HttpClient::ntlm(&config.policy, &config.timeouts)?,
        };
        let mut client = Self {
            http,
            config: config.clone(),
            auth,
            outcome: EwsOutcome {
                host: config.endpoint.host.clone(),
                url: config.url(),
                requested_version: config.version.as_str().to_string(),
                encrypted: config.is_encrypted(),
                ..EwsOutcome::default()
            },
        };
        let document = client
            .call(
                "GetFolder",
                request::get_folder(&FolderRef::distinguished("inbox")),
            )
            .await?;
        client.check_response(&document, "GetFolder", "GetFolder")?;
        Ok(client)
    }

    /// What the connection reported about itself.
    #[must_use]
    pub fn outcome(&self) -> &EwsOutcome {
        &self.outcome
    }

    /// The endpoint URL in use.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.outcome.url
    }

    /// Sends one operation and returns the parsed response envelope.
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`].
    pub async fn call(&mut self, action: &str, body: String) -> Result<Node> {
        let envelope = self.envelope(action, body);
        let url = self.config.url();
        let action_uri = envelope.action_uri();
        let started = std::time::Instant::now();
        let response = match &self.auth {
            Authentication::Header(header) => {
                self.http
                    .post_soap(
                        &url,
                        &action_uri,
                        envelope.render(),
                        Some(header.value()),
                        &[],
                    )
                    .await?
            }
            Authentication::Ntlm(identity) => {
                self.http
                    .post_soap_ntlm(&url, &action_uri, envelope.render(), identity, &[])
                    .await?
            }
        };
        self.outcome.round_trip_ms =
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.parse_response(action, response)
    }

    /// Builds the envelope of one operation, headers included.
    fn envelope(&self, action: &str, body: String) -> Envelope {
        let mut envelope = Envelope::new(action, body).header(request::request_server_version(
            self.config.version.as_str(),
        ));
        if let Some(impersonation) = &self.config.impersonation {
            envelope = envelope.header(request::impersonation(&impersonation.smtp_address));
        }
        if let Some(zone) = &self.config.time_zone {
            envelope = envelope.header(request::time_zone_context(zone));
        }
        envelope
    }

    /// Turns an HTTP answer into a parsed document or a structured error.
    fn parse_response(&mut self, action: &str, response: HttpResponse) -> Result<Node> {
        let status = response.status;
        if status == 401 || status == 403 {
            return Err(Error::Auth {
                account: self.config.username.clone(),
                message: auth_failure_message(&response),
            });
        }
        if !response.is_success() {
            return Err(Error::protocol(
                "EWS",
                action,
                format!("HTTP {status}: {}", first_line(&response.body)),
            ));
        }
        let document = Node::parse(&response.body)?;
        // The response header is the only place a server reveals its version.
        if let Some(info) = document.find(&["Header", "ServerVersionInfo"]) {
            self.outcome.server_version = info.attr("Version").map(ToString::to_string);
            self.outcome.server_minor_version = info.attr("MinorVersion").map(ToString::to_string);
        }
        if let Some(message) = fault_message(&document) {
            // EWS answers with HTTP 200 even for a rejected password.
            return Err(if is_auth_fault(&document) {
                Error::Auth {
                    account: self.config.username.clone(),
                    message,
                }
            } else {
                Error::protocol("EWS", action, message)
            });
        }
        Ok(document)
    }

    /// Reads the response messages of an operation.
    ///
    /// `action` is both the name of the wrapper element (`FindFolder`) and of
    /// the message element (`FindFolderResponseMessage`).  Every message must
    /// report success, because an error in one of them means part of the answer
    /// is missing.
    fn response_messages<'a>(&self, document: &'a Node, action: &str) -> Result<Vec<&'a Node>> {
        let name = format!("{action}ResponseMessage");
        // The response messages sit at `Body/<Action>Response/ResponseMessages/…`,
        // and a few servers add another wrapper level, so a descendant search is
        // shorter and more robust than modelling the exact nesting.
        let messages: Vec<&Node> = document.descendants_named(&name);
        if messages.is_empty() {
            return Err(Error::malformed(
                "EWS",
                format!("the {action} response contains no response message"),
            ));
        }
        for message in &messages {
            let class = message.attr("ResponseClass").unwrap_or("Success");
            if class.eq_ignore_ascii_case("Success") || class.eq_ignore_ascii_case("Warning") {
                continue;
            }
            let code = message.find_text(&["ResponseCode"]).unwrap_or("unknown");
            let text = message.find_text(&["MessageText"]).unwrap_or_default();
            let detail = if text.is_empty() {
                code.to_string()
            } else {
                format!("{code}: {text}")
            };
            return Err(
                if code.to_ascii_lowercase().contains("credential")
                    || code.to_ascii_lowercase().contains("unauthorized")
                {
                    Error::Auth {
                        account: self.config.username.clone(),
                        message: detail,
                    }
                } else {
                    Error::protocol("EWS", action, detail)
                },
            );
        }
        Ok(messages)
    }

    /// Reads a single message response of an operation.
    fn check_response<'a>(
        &self,
        document: &'a Node,
        action: &str,
        label: &str,
    ) -> Result<&'a Node> {
        self.response_messages(document, action)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::malformed("EWS", format!("{label} returned no message")))
    }

    /// Walks the folder tree below `msgfolderroot`.
    ///
    /// `FindFolder` is shallow, so the tree is discovered breadth first.  The
    /// returned folders store their server id in [`Folder::path`] and their
    /// parent link in [`Folder::parent_id`].
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`].
    pub async fn list_folders(&mut self, account_id: AccountId) -> Result<Vec<Folder>> {
        let mut pairs: Vec<(EwsFolder, String)> = Vec::new();
        let mut queue: Vec<(FolderRef, String, usize)> =
            vec![(FolderRef::distinguished("msgfolderroot"), String::new(), 0)];
        let mut visited: HashSet<String> = HashSet::new();

        while let Some((reference, prefix, depth)) = queue.pop() {
            if depth > MAX_FOLDER_DEPTH || pairs.len() >= MAX_FOLDERS {
                break;
            }
            if !visited.insert(reference.as_str().to_string()) {
                continue;
            }
            let document = self
                .call("FindFolder", request::find_folder(&reference))
                .await?;
            let messages = self.response_messages(&document, "FindFolder")?;
            let mut children: Vec<(EwsFolder, String)> = Vec::new();
            for message in messages {
                for node in message.descendants_named("Folder") {
                    let Some(folder) = EwsFolder::parse(node) else {
                        continue;
                    };
                    let path = if prefix.is_empty() {
                        folder.display_name.clone()
                    } else {
                        format!("{prefix}/{}", folder.display_name)
                    };
                    children.push((folder, path));
                }
            }
            // Sorting here only makes the traversal deterministic; the final
            // order is decided by `convert::order_folders`.
            children.sort_by(|left, right| left.1.cmp(&right.1));
            for (folder, path) in children {
                if folder.is_technical_root() {
                    continue;
                }
                if folder.child_folder_count > 0 && depth < MAX_FOLDER_DEPTH {
                    queue.push((FolderRef::id(folder.id.id.clone()), path.clone(), depth + 1));
                }
                pairs.push((folder, path));
            }
        }

        Ok(to_models(account_id, &pairs))
    }

    /// Downloads the raw MIME content of one message.
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`]; a message that is gone is an
    /// [`Error::Protocol`] with `ErrorItemNotFound`.
    pub async fn fetch_mime(&mut self, item_id: &str) -> Result<Vec<u8>> {
        let document = self
            .call("GetItem", request::get_item(item_id, None))
            .await?;
        let messages = self.response_messages(&document, "GetItem")?;
        for message in messages {
            let content = message
                .descendants_named("MimeContent")
                .into_iter()
                .next()
                .map(|node| node.text.clone())
                .unwrap_or_default();
            let decoded = decode_mime(&content)?;
            if !decoded.is_empty() {
                return Ok(decoded);
            }
        }
        Err(Error::protocol(
            "EWS",
            "GetItem",
            "the server sent no MIME content for this message".to_string(),
        ))
    }

    /// Submits a complete message and files the copy in `Sent Items`.
    ///
    /// The message travels as MIME, so the wire form is byte for byte the one
    /// [`crate::compose`] produced - signature, `Bcc:` stripping and all.  The
    /// return value is the server side id of the Sent copy when the server
    /// reported one.  A server that omits it has still accepted the message,
    /// so a missing id is not an error: treating it as one would make the
    /// outbox submit the same mail a second time.
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`]; a submission the server refuses
    /// (`ErrorSendAsDenied` and friends) is an [`Error::Protocol`], never a
    /// retryable failure.
    pub async fn send_mime(&mut self, raw: &[u8]) -> Result<Option<String>> {
        let document = self
            .call("CreateItem", request::create_item_mime(raw))
            .await?;
        let messages = self.response_messages(&document, "CreateItem")?;
        Ok(messages
            .iter()
            .flat_map(|message| message.descendants_named("ItemId"))
            .find_map(ItemId::parse)
            .map(|id| id.id))
    }

    /// Marks a message as read or unread.
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`].
    pub async fn set_read(&mut self, item_id: &str, is_read: bool) -> Result<()> {
        let document = self
            .call("UpdateItem", request::set_read_state(item_id, is_read))
            .await?;
        self.check_response(&document, "UpdateItem", "UpdateItem")?;
        Ok(())
    }

    /// Sets or clears the follow-up flag of a message.
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`].
    pub async fn set_flag(&mut self, item_id: &str, flagged: bool) -> Result<()> {
        let document = self
            .call("UpdateItem", request::set_flag(item_id, flagged))
            .await?;
        self.check_response(&document, "UpdateItem", "UpdateItem")?;
        Ok(())
    }

    /// Sets the "done" state of a message as its Exchange item status.
    ///
    /// EWS expresses "done" as a completed follow-up flag; clearing it returns
    /// the item to a plain follow-up flag, which is the closest the protocol
    /// offers (`FR-FLAG-3`).
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`].
    pub async fn set_done(&mut self, item_id: &str, done: bool) -> Result<()> {
        let document = self
            .call("UpdateItem", request::set_done(item_id, done))
            .await?;
        self.check_response(&document, "UpdateItem", "UpdateItem")?;
        Ok(())
    }

    /// Moves a message into another folder and returns its new item id.
    ///
    /// EWS changes the item id on a move, so the caller has to store the new one.
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`].
    pub async fn move_item(&mut self, item_id: &str, destination: &FolderRef) -> Result<String> {
        let document = self
            .call("MoveItem", request::move_item(item_id, destination))
            .await?;
        let messages = self.response_messages(&document, "MoveItem")?;
        messages
            .iter()
            .flat_map(|message| message.descendants_named("ItemId"))
            .find_map(ItemId::parse)
            .map(|id| id.id)
            .ok_or_else(|| Error::malformed("EWS", "MoveItem returned no item id"))
    }

    /// Deletes a message, softly (into the deleted items folder) or hard.
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`].
    pub async fn delete_item(&mut self, item_id: &str, hard: bool) -> Result<()> {
        let document = self
            .call("DeleteItem", request::delete_item(item_id, hard))
            .await?;
        self.check_response(&document, "DeleteItem", "DeleteItem")?;
        Ok(())
    }

    /// One `SyncFolderItems` call.
    ///
    /// `sync_state` is the cursor of the previous pass; `None` asks for the whole
    /// folder content.
    ///
    /// # Errors
    ///
    /// See [`EwsClient::login`].
    pub(crate) async fn sync_items(
        &mut self,
        folder_id: &str,
        sync_state: Option<&str>,
        max_changes: u32,
    ) -> Result<SyncBatch> {
        let document = self
            .call(
                "SyncFolderItems",
                request::sync_folder_items(folder_id, sync_state, max_changes),
            )
            .await?;
        let message = self.check_response(&document, "SyncFolderItems", "SyncFolderItems")?;
        Ok(SyncBatch::parse(message))
    }
}

/// Builds the `Basic` header value without ever formatting the password into a
/// log line.
fn basic_header(username: &str, password: &str) -> String {
    use base64::Engine;
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    format!("Basic {encoded}")
}

/// The message of a `401`/`403` answer.
fn auth_failure_message(response: &HttpResponse) -> String {
    match &response.www_authenticate {
        Some(scheme) => format!("the server rejected the credentials (offered: {scheme})"),
        None => "the server rejected the credentials".to_string(),
    }
}

/// `true` when a SOAP fault is about authentication rather than the request.
fn is_auth_fault(document: &Node) -> bool {
    let code = document
        .find_text(&["Body", "Fault", "detail", "ResponseCode"])
        .or_else(|| document.find_text(&["Body", "Fault", "faultcode"]))
        .unwrap_or_default()
        .to_ascii_lowercase();
    code.contains("credential") || code.contains("unauthorized") || code.contains("access")
}

/// Decodes the base64 `MimeContent` of a `GetItem` answer.
fn decode_mime(content: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let compact: String = content.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return Ok(Vec::new());
    }
    base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .map_err(|error| Error::malformed("EWS", format!("MimeContent is not base64: {error}")))
}

/// First line of a body, used to keep an HTTP error short.
fn first_line(body: &str) -> String {
    body.lines().next().unwrap_or_default().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::account::{AuthKind, Endpoint, TlsMode};

    fn config(tls: TlsMode) -> EwsConfig {
        EwsConfig::new(
            Endpoint::new("mail.example.org", 443, tls),
            "example\\ada",
            AuthKind::Password,
        )
    }

    #[test]
    fn urls_follow_the_tls_mode_and_the_service_path() {
        assert_eq!(
            config(TlsMode::Implicit).url(),
            "https://mail.example.org:443/EWS/Exchange.asmx"
        );
        assert_eq!(
            config(TlsMode::StartTls).url(),
            "https://mail.example.org:443/EWS/Exchange.asmx"
        );
        assert_eq!(
            config(TlsMode::None).url(),
            "http://mail.example.org:443/EWS/Exchange.asmx"
        );
        let mut custom = config(TlsMode::Implicit);
        custom.service_path = "EWS/Exchange.asmx".to_string();
        assert_eq!(
            custom.url(),
            "https://mail.example.org:443/EWS/Exchange.asmx"
        );
        assert!(custom.is_encrypted());
        assert!(!config(TlsMode::None).is_encrypted());
    }

    #[test]
    fn the_version_strings_are_the_ones_exchange_expects() {
        assert_eq!(EwsVersion::default().as_str(), "Exchange2013");
        assert_eq!(EwsVersion::Exchange2013Sp1.as_str(), "Exchange2013_SP1");
        assert_eq!(EwsVersion::Exchange2007Sp1.as_str(), "Exchange2007_SP1");
        assert_eq!(EwsVersion::Exchange2016.as_str(), "Exchange2016");
        assert_eq!(EwsVersion::Exchange2010Sp2.as_str(), "Exchange2010_SP2");
        assert_eq!(EwsVersion::Exchange2010.as_str(), "Exchange2010");
    }

    #[test]
    fn the_basic_header_needs_no_password_formatting() {
        use base64::Engine;
        let header = basic_header("example\\ada", "geheim");
        let encoded = header.strip_prefix("Basic ").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "example\\ada:geheim");
    }

    #[test]
    fn mime_content_is_decoded_and_broken_input_is_an_error() {
        use base64::Engine;
        let raw = b"From: ada@example.org\r\nSubject: Test\r\n\r\nHallo\r\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        // EWS wraps the base64 into lines.
        let wrapped = encoded
            .as_bytes()
            .chunks(20)
            .map(|chunk| String::from_utf8_lossy(chunk).to_string())
            .collect::<Vec<_>>()
            .join("\r\n");
        assert_eq!(decode_mime(&wrapped).unwrap(), raw);
        assert!(decode_mime("").unwrap().is_empty());
        let error = decode_mime("not base64!").unwrap_err();
        assert!(
            matches!(error, Error::MalformedResponse { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_outcome_is_described_for_the_dialog() {
        let outcome = EwsOutcome {
            host: "mail.example.org".to_string(),
            url: "https://mail.example.org:443/EWS/Exchange.asmx".to_string(),
            server_version: Some("15.01.2507.037".to_string()),
            server_minor_version: Some("6".to_string()),
            requested_version: "Exchange2013".to_string(),
            encrypted: true,
            round_trip_ms: 42,
        };
        let text = outcome.describe();
        assert!(text.contains("15.01.2507.037"), "{text}");
        assert!(text.contains("mail.example.org"), "{text}");
        assert!(text.contains("42 ms"), "{text}");
        assert!(text.contains("encrypted"), "{text}");
    }
}
