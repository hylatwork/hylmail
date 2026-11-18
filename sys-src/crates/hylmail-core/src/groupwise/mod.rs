//! GroupWise SOAP client for Novell / Micro Focus GroupWise behind a SOAP proxy.
//!
//! GroupWise does not speak IMAP; the supported way in is the SOAP proxy that
//! ships with the GroupWise Internet Agent.  The protocol is session based:
//!
//! 1. `loginRequest` exchanges the user name and password for a session id.
//! 2. Every other operation sends that session in a SOAP header.
//! 3. `logoutRequest` ends it.
//!
//! A session expires when the server decides so (the proxy closes idle sessions
//! after roughly 30 minutes).  Any answer with a session error therefore makes
//! the client log in again and replay the request once — the caller only sees a
//! failure when the second attempt fails too.
//!
//! ## What this client cannot do
//!
//! * GroupWise has no OAuth2 and no bearer tokens, so an account configured for
//!   OAuth2 is refused instead of being attempted.
//! * There is no delta protocol.  A synchronisation lists the items of a folder
//!   page by page and compares the ids with the local database, which is why
//!   [`crate::groupwise::sync_folder`] reports removals for every pass.
//! * Flag changes other than "read" are not pushed to the server; the proxy does
//!   not expose categories, which is why the model marks those accounts as
//!   [`AccountKind::uses_local_only_flags`](crate::models::AccountKind::uses_local_only_flags).

mod request;
mod synchronise;
mod types;

use secrecy::ExposeSecret;

use crate::error::{Error, Result};
use crate::models::account::{Endpoint, GroupWiseProxyConfig, TlsMode};
use crate::models::folder::{Folder, FolderKind, SpecialUse};
use crate::models::ids::{AccountId, FolderId};
use crate::models::message::{MessageFlags, MessageHeader};
use crate::net::{Timeouts, TlsPolicy};
use crate::secrets::Credential;
use crate::soap::http::HttpResponse;
use crate::soap::{fault_message, Envelope, HttpClient, Node};

pub use request::{LOGIN_NAMESPACE, METHODS_NAMESPACE, TYPES_NAMESPACE};
pub use synchronise::{sync_folder, sync_folders, ITEM_PAGE_SIZE};
pub use types::{GwFolder, GwItem, ItemPage};

/// Status code GroupWise returns for "everything worked".
const STATUS_OK: i64 = 0;

/// The status codes that mean "log in again".
///
/// The proxy reports an expired or unknown session as `0x4200`-style codes (and
/// the older `0x4400`), so a second login is tried before the caller sees an
/// error.
const SESSION_ERRORS: &[i64] = &[0x4200, 0x4201, 0x4202, 0x4203, 0x4204, 0x4400, 0x4401];

/// The status codes that mean "the user name or password is wrong".
const AUTH_ERRORS: &[i64] = &[
    0x4205, 0x4206, 0x4207, 0x4208, 0x4209, 0x420a, 0x420b, 0x420c,
];

/// Everything needed to talk to one GroupWise mailbox.
#[derive(Debug, Clone)]
pub struct GroupWiseConfig {
    /// SOAP endpoint of the proxy.
    pub proxy: Endpoint,
    /// Login name, usually the GroupWise user id.
    pub username: String,
    /// Post office the mailbox lives in, when the proxy needs it.
    pub post_office: Option<String>,
    /// Additional mailboxes the user may access.
    pub shared_mailboxes: Vec<String>,
    /// Certificate policy.
    pub policy: TlsPolicy,
    /// Deadlines.
    pub timeouts: Timeouts,
    /// Follow a redirect to the post office even when the certificate does not
    /// match the proxy's name.
    pub allow_certificate_cn_fallback: bool,
    /// Path of the SOAP service on the proxy.
    pub service_path: String,
}

impl GroupWiseConfig {
    /// Builds a configuration from the account settings.
    #[must_use]
    pub fn new(
        proxy: Endpoint,
        username: impl Into<String>,
        settings: &GroupWiseProxyConfig,
    ) -> Self {
        Self {
            proxy,
            username: username.into(),
            post_office: settings
                .post_office
                .clone()
                .filter(|value| !value.is_empty()),
            shared_mailboxes: settings.shared_mailboxes.clone(),
            policy: TlsPolicy::strict(),
            timeouts: Timeouts::default(),
            allow_certificate_cn_fallback: settings.allow_certificate_cn_fallback,
            service_path: "/soap".to_string(),
        }
    }

    /// The URL every request goes to.
    #[must_use]
    pub fn url(&self) -> String {
        let scheme = match self.proxy.tls {
            TlsMode::None => "http",
            TlsMode::StartTls | TlsMode::Implicit => "https",
        };
        let path = if self.service_path.starts_with('/') {
            self.service_path.clone()
        } else {
            format!("/{}", self.service_path)
        };
        format!("{scheme}://{}:{}{path}", self.proxy.host, self.proxy.port)
    }

    /// `true` when the connection is encrypted from the first byte.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.proxy.tls == TlsMode::Implicit
    }
}

/// What a connection reported about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupWiseOutcome {
    /// Proxy that was contacted.
    pub host: String,
    /// URL that was used (it changes when a redirect was followed).
    pub url: String,
    /// `true` when the connection is encrypted.
    pub encrypted: bool,
    /// Round trip of the last request.
    pub round_trip_ms: u64,
}

impl GroupWiseOutcome {
    /// A one line description for the account dialog.
    #[must_use]
    pub fn describe(&self) -> String {
        let encryption = if self.encrypted {
            "encrypted"
        } else {
            "cleartext"
        };
        format!(
            "GroupWise proxy at {} ({encryption}, {} ms)",
            self.host, self.round_trip_ms
        )
    }
}

/// One logged in GroupWise session.
pub struct GroupWiseClient {
    http: HttpClient,
    config: GroupWiseConfig,
    password: String,
    session: String,
    outcome: GroupWiseOutcome,
}

impl std::fmt::Debug for GroupWiseClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The session id and the password are credentials and never printed.
        f.debug_struct("GroupWiseClient")
            .field("url", &self.outcome.url)
            .field("username", &self.config.username)
            .field("session", &"resolved")
            .finish_non_exhaustive()
    }
}

impl GroupWiseClient {
    /// Logs in and returns the session.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for an OAuth2 credential,
    /// [`Error::Auth`] for a rejected user name or password,
    /// [`Error::Protocol`] for a SOAP fault, and the transport errors of
    /// [`HttpClient`].
    pub async fn login(config: &GroupWiseConfig, credential: Credential<'_>) -> Result<Self> {
        let password =
            match credential {
                Credential::Password(password) => password.expose_secret().to_string(),
                Credential::OAuth2 { .. } => return Err(Error::Unsupported(
                    "GroupWise has no OAuth2 support; use a password or an application specific \
                     password"
                        .to_string(),
                )),
            };
        let http = HttpClient::new(&config.policy, &config.timeouts)?;
        let mut client = Self {
            http,
            config: config.clone(),
            password,
            session: String::new(),
            outcome: GroupWiseOutcome {
                host: config.proxy.host.clone(),
                url: config.url(),
                encrypted: config.is_encrypted(),
                round_trip_ms: 0,
            },
        };
        client.start_session().await?;
        Ok(client)
    }

    /// What the connection reported about itself.
    #[must_use]
    pub fn outcome(&self) -> &GroupWiseOutcome {
        &self.outcome
    }

    /// The endpoint URL in use.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.outcome.url
    }

    /// `true` when a session id is held.
    #[must_use]
    pub fn is_logged_in(&self) -> bool {
        !self.session.is_empty()
    }

    /// Ends the session on the server.
    ///
    /// # Errors
    ///
    /// A failing logout is reported; the local session is dropped regardless, so
    /// the caller can treat the error as informational.
    pub async fn logout(&mut self) -> Result<()> {
        if self.session.is_empty() {
            return Ok(());
        }
        let result = self.post(&request::logout(), None).await;
        self.session.clear();
        result.map(|_| ())
    }

    /// Runs `loginRequest` and stores the session id.
    async fn start_session(&mut self) -> Result<()> {
        let body = request::login(&self.config.username, &self.password);
        let document = self.post(&body, None).await?;
        let session = document
            .find_text(&["Body", "loginResponse", "session"])
            .or_else(|| document.find_text(&["session"]))
            .unwrap_or_default()
            .to_string();
        if session.is_empty() {
            return Err(Error::malformed(
                "GroupWise",
                "the login answer carried no session id",
            ));
        }
        self.session = session;
        Ok(())
    }

    /// Sends one request, logging in again when the session expired.
    async fn call(&mut self, body: &str) -> Result<Node> {
        let action = request::action_of(body);
        match self.post(body, Some(&action)).await {
            Ok(document) => Ok(document),
            Err(error) if is_session_error(&error) => {
                tracing::info!("the GroupWise session expired; logging in again");
                self.session.clear();
                self.start_session().await?;
                self.post(body, Some(&action)).await
            }
            Err(error) => Err(error),
        }
    }

    /// Sends one request without the retry logic.
    async fn post(&mut self, body: &str, action: Option<&str>) -> Result<Node> {
        let action = match action {
            Some(action) => action.to_string(),
            None => request::action_of(body),
        };
        let mut envelope = Envelope::bare(action.clone(), body);
        if !self.session.is_empty() {
            envelope = envelope.header(request::session_header(&self.session));
        }
        let started = std::time::Instant::now();
        let response = self
            .http
            .post_soap(&self.outcome.url, &action, envelope.render(), None, &[])
            .await?;
        self.outcome.round_trip_ms =
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.interpret(&action, response)
    }

    /// Turns an HTTP answer into a document or a structured error.
    fn interpret(&mut self, action: &str, response: HttpResponse) -> Result<Node> {
        let status = response.status;
        if (300..400).contains(&status) {
            return Err(self.redirect_error(&response));
        }
        if status == 401 || status == 403 {
            return Err(Error::Auth {
                account: self.config.username.clone(),
                message: "the proxy rejected the credentials".to_string(),
            });
        }
        if !response.is_success() {
            return Err(Error::protocol(
                "GroupWise",
                action,
                format!("HTTP {status}: {}", first_line(&response.body)),
            ));
        }
        let document = Node::parse(&response.body)?;
        if let Some(message) = fault_message(&document) {
            return Err(Error::protocol("GroupWise", action, message));
        }
        self.check_status(action, &document)?;
        Ok(document)
    }

    /// Reads the `<status><code>` element every answer carries.
    ///
    /// The first `<code>` of the document is the status of the operation; the
    /// per-item codes of a listing come later.
    fn check_status(&self, action: &str, document: &Node) -> Result<()> {
        let Some(code_node) = document.descendants_named("code").into_iter().next() else {
            // A few operations answer without a status; that counts as success.
            return Ok(());
        };
        let Some(code) = parse_status_code(code_node.text.trim()) else {
            return Ok(());
        };
        if code == STATUS_OK {
            return Ok(());
        }
        let description = document
            .descendants_named("description")
            .into_iter()
            .next()
            .map(|node| node.text.trim().to_string())
            .unwrap_or_default();
        let detail = if description.is_empty() {
            format!("status 0x{code:04X}")
        } else {
            format!("status 0x{code:04X}: {description}")
        };
        if AUTH_ERRORS.contains(&code) || SESSION_ERRORS.contains(&code) {
            return Err(Error::Auth {
                account: self.config.username.clone(),
                message: detail,
            });
        }
        Err(Error::protocol("GroupWise", action, detail))
    }

    /// Builds the error for a redirect the policy does not allow.
    fn redirect_error(&self, response: &HttpResponse) -> Error {
        let target = response
            .location
            .as_deref()
            .unwrap_or("(no Location header)");
        let host = crate::soap::http::host_of(target);
        let same_host = host == self.config.proxy.host
            || host.starts_with(&format!("{}:", self.config.proxy.host));
        if same_host || self.config.allow_certificate_cn_fallback {
            // The caller has to retry against the new URL; the client does not
            // follow automatically because the post office resolution is a
            // one-off decision that belongs to the caller.
            crate::Error::Config(format!(
                "the proxy redirected to {target}; retry against that address"
            ))
        } else {
            Error::Tls {
                host: self.config.proxy.host.clone(),
                message: format!(
                    "the proxy redirected to {target}, whose certificate does not match \
                     `{}`; enable the certificate fallback for this account to follow it",
                    self.config.proxy.host
                ),
            }
        }
    }

    /// Lists every folder of the mailbox, linked and ordered.
    ///
    /// # Errors
    ///
    /// See [`GroupWiseClient::login`].
    pub async fn list_folders(&mut self, account_id: AccountId) -> Result<Vec<Folder>> {
        let document = self.call(&request::get_folder_list(0)).await?;
        let folders: Vec<GwFolder> = document
            .descendants_named("folder")
            .into_iter()
            .filter_map(GwFolder::parse)
            .collect();
        Ok(to_models(account_id, &folders))
    }

    /// Reads one page of the items of a folder.
    ///
    /// # Errors
    ///
    /// See [`GroupWiseClient::login`].
    pub async fn list_items(
        &mut self,
        folder_id: &str,
        count: u32,
        offset: u32,
    ) -> Result<ItemPage> {
        let document = self
            .call(&request::get_items(folder_id, count, offset))
            .await?;
        Ok(ItemPage::parse(&document))
    }

    /// Downloads the raw MIME content of one message.
    ///
    /// # Errors
    ///
    /// See [`GroupWiseClient::login`].
    pub async fn fetch_mime(&mut self, item_id: &str) -> Result<Vec<u8>> {
        let document = self.call(&request::get_item(item_id)).await?;
        let data = document
            .descendants_named("data")
            .into_iter()
            .next()
            .map(|node| node.text.clone())
            .unwrap_or_default();
        decode_mime(&data)
    }

    /// Marks items as read or unread.
    ///
    /// # Errors
    ///
    /// See [`GroupWiseClient::login`].
    pub async fn set_read(&mut self, item_ids: &[String], read: bool) -> Result<()> {
        self.call(&request::mark_items(item_ids, read)).await?;
        Ok(())
    }

    /// Moves items into another folder.
    ///
    /// # Errors
    ///
    /// See [`GroupWiseClient::login`].
    pub async fn move_items(&mut self, item_ids: &[String], destination: &str) -> Result<()> {
        self.call(&request::move_items(item_ids, destination))
            .await?;
        Ok(())
    }

    /// Deletes items.
    ///
    /// # Errors
    ///
    /// See [`GroupWiseClient::login`].
    pub async fn delete_items(&mut self, item_ids: &[String]) -> Result<()> {
        self.call(&request::delete_items(item_ids)).await?;
        Ok(())
    }
}

/// `true` when the error means "the session is gone".
fn is_session_error(error: &Error) -> bool {
    match error {
        Error::Auth { message, .. } => message
            .split_whitespace()
            .filter_map(|token| parse_status_code(token.trim_end_matches(':')))
            .any(|code| SESSION_ERRORS.contains(&code)),
        _ => false,
    }
}

/// Reads a status code, which GroupWise sends as decimal or as `0x…`.
#[must_use]
pub fn parse_status_code(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok();
    }
    text.parse().ok()
}

/// Decodes the base64 `<data>` of a `getItem` answer.
///
/// GroupWise sends the MIME content as base64, sometimes with line breaks.
fn decode_mime(data: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let compact: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return Ok(Vec::new());
    }
    base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .map_err(|error| Error::malformed("GroupWise", format!("<data> is not base64: {error}")))
}

/// First line of a body, used to keep an HTTP error short.
fn first_line(body: &str) -> String {
    body.lines().next().unwrap_or_default().trim().to_string()
}

/// Converts the folders of a `getFolderList` answer into local models.
///
/// GroupWise names its system folders differently from IMAP and Exchange:
/// `Mailbox` is the inbox, `Work In Progress` the drafts folder and `Cabinet`
/// the archive.
#[must_use]
pub fn to_models(account_id: AccountId, folders: &[GwFolder]) -> Vec<Folder> {
    let mut models: Vec<Folder> = folders
        .iter()
        .map(|folder| folder_to_model(account_id, folder))
        .collect();
    let by_server_id: std::collections::HashMap<&str, FolderId> = folders
        .iter()
        .zip(models.iter())
        .map(|(folder, model)| (folder.id.as_str(), model.id))
        .collect();
    for (index, folder) in folders.iter().enumerate() {
        if let Some(parent) = folder.parent_id.as_deref() {
            if let Some(parent_id) = by_server_id.get(parent) {
                models[index].parent_id = Some(*parent_id);
            }
        }
    }
    order_folders(&mut models);
    models
}

/// Converts one GroupWise folder into the local model.
#[must_use]
pub fn folder_to_model(account_id: AccountId, folder: &GwFolder) -> Folder {
    let kind = folder_kind_of(&folder.name);
    // The server folder id is the identity of the row, like the IMAP path.
    let mut model = Folder::new(account_id, folder.name.clone(), folder.id.clone(), kind);
    model.special_use = special_use_of(&folder.name);
    model.total_count = folder.item_count;
    model.unread_count = folder.unread_count;
    model.selectable = folder.is_mail_folder();
    model.delimiter = None;
    model
}

/// Maps a GroupWise folder name onto a folder kind.
#[must_use]
pub fn folder_kind_of(name: &str) -> FolderKind {
    let normalised = name.trim().to_ascii_lowercase();
    match normalised.as_str() {
        // The names of the GroupWise system folders.
        "mailbox" | "inbox" | "posteingang" => FolderKind::Inbox,
        "sent items" | "sent" | "gesendete objekte" => FolderKind::Sent,
        "work in progress" | "drafts" | "entwürfe" => FolderKind::Drafts,
        "trash" | "papierkorb" => FolderKind::Trash,
        "junk mail" | "spam" | "unerwünschte werbung" => FolderKind::Junk,
        "cabinet" | "archive" | "archiv" => FolderKind::Archive,
        _ => crate::models::folder::detect_special_use(name)
            .map(FolderKind::from_special_use)
            .unwrap_or(FolderKind::Custom),
    }
}

/// The special-use attribute of a GroupWise folder.
#[must_use]
pub fn special_use_of(name: &str) -> Option<SpecialUse> {
    match folder_kind_of(name) {
        FolderKind::Custom => crate::models::folder::detect_special_use(name),
        kind => crate::models::folder::detect_special_use(match kind {
            FolderKind::Inbox => "Inbox",
            FolderKind::Sent => "Sent",
            FolderKind::Drafts => "Drafts",
            FolderKind::Trash => "Trash",
            FolderKind::Junk => "Junk",
            FolderKind::Archive => "Archive",
            _ => return None,
        }),
    }
}

/// Sorts folders the way a mail client should display them.
///
/// GroupWise ids are opaque, so the name decides the order inside a rank.
pub fn order_folders(folders: &mut [Folder]) {
    folders.sort_by(|left, right| {
        let left_key = (kind_rank(left.kind), left.name.to_lowercase());
        let right_key = (kind_rank(right.kind), right.name.to_lowercase());
        left_key.cmp(&right_key)
    });
    for (index, folder) in folders.iter_mut().enumerate() {
        folder.sort_order = i32::try_from(index).unwrap_or(i32::MAX);
    }
}

/// Sort rank of a folder: lower comes first.
fn kind_rank(kind: FolderKind) -> u8 {
    match kind {
        FolderKind::Inbox => 0,
        FolderKind::Sent => 1,
        FolderKind::Drafts => 2,
        FolderKind::Archive => 3,
        FolderKind::Custom => 4,
        FolderKind::Junk => 8,
        FolderKind::Trash => 9,
        FolderKind::Outbox | FolderKind::VirtualAllInboxes => 10,
    }
}

/// Converts a GroupWise item into a message header.
#[must_use]
pub fn item_to_header(account_id: AccountId, folder_id: FolderId, item: &GwItem) -> MessageHeader {
    let mut header = MessageHeader::new(account_id, folder_id);
    header.server_id = Some(item.id.clone());
    header.subject.clone_from(&item.subject);
    header.message_id.clone_from(&item.message_id);
    header.in_reply_to.clone_from(&item.in_reply_to);
    header.from = item.from.clone().into_iter().collect();
    header.to.clone_from(&item.to);
    header.date = item.date;
    header.received_at = item.date;
    header.size_bytes = item.size_bytes;
    header.has_attachments = item.has_attachments;
    if item.is_read {
        header.flags |= MessageFlags::SEEN;
    }
    header.refresh_derived_fields();
    header
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::account::GroupWiseProxyConfig;

    fn proxy_settings() -> GroupWiseProxyConfig {
        GroupWiseProxyConfig {
            proxy: Endpoint::groupwise_proxy("gw.example.org"),
            post_office: Some("po1".to_string()),
            user_id: Some("ada".to_string()),
            shared_mailboxes: Vec::new(),
            allow_certificate_cn_fallback: false,
        }
    }

    #[test]
    fn the_configuration_comes_from_the_account_settings() {
        let config = GroupWiseConfig::new(
            Endpoint::groupwise_proxy("gw.example.org"),
            "ada",
            &proxy_settings(),
        );
        assert_eq!(config.url(), "https://gw.example.org:7191/soap");
        assert!(config.is_encrypted());
        assert_eq!(config.post_office.as_deref(), Some("po1"));
        let mut cleartext = config.clone();
        cleartext.proxy.tls = TlsMode::None;
        assert_eq!(cleartext.url(), "http://gw.example.org:7191/soap");
        assert!(!cleartext.is_encrypted());
        let mut custom = config.clone();
        custom.service_path = "soap".to_string();
        assert_eq!(custom.url(), "https://gw.example.org:7191/soap");
    }

    #[test]
    fn status_codes_are_read_in_both_spellings() {
        assert_eq!(parse_status_code("0"), Some(0));
        assert_eq!(parse_status_code("0x4200"), Some(0x4200));
        assert_eq!(parse_status_code("0X4200"), Some(0x4200));
        assert_eq!(parse_status_code("  42  "), Some(42));
        assert_eq!(parse_status_code("keine Zahl"), None);
        assert_eq!(parse_status_code(""), None);
    }

    #[test]
    fn the_groupwise_system_folders_are_recognised() {
        assert_eq!(folder_kind_of("Mailbox"), FolderKind::Inbox);
        assert_eq!(folder_kind_of("Sent Items"), FolderKind::Sent);
        assert_eq!(folder_kind_of("Work In Progress"), FolderKind::Drafts);
        assert_eq!(folder_kind_of("Trash"), FolderKind::Trash);
        assert_eq!(folder_kind_of("Junk Mail"), FolderKind::Junk);
        assert_eq!(folder_kind_of("Cabinet"), FolderKind::Archive);
        assert_eq!(folder_kind_of("Projekte"), FolderKind::Custom);
        assert_eq!(special_use_of("Mailbox"), Some(SpecialUse::Inbox));
        assert_eq!(special_use_of("Cabinet"), Some(SpecialUse::Archive));
        assert!(special_use_of("Projekte").is_none());
    }

    #[test]
    fn a_folder_tree_is_linked_and_ordered() {
        let account = AccountId::new();
        let folders = vec![
            GwFolder {
                id: "9".to_string(),
                name: "Rechnungen".to_string(),
                parent_id: Some("5".to_string()),
                ..GwFolder::default()
            },
            GwFolder {
                id: "5".to_string(),
                name: "Projekte".to_string(),
                item_count: 3,
                unread_count: 1,
                ..GwFolder::default()
            },
            GwFolder {
                id: "1".to_string(),
                name: "Mailbox".to_string(),
                item_count: 10,
                unread_count: 2,
                ..GwFolder::default()
            },
        ];
        let models = to_models(account, &folders);
        assert_eq!(models[0].name, "Mailbox");
        assert_eq!(models[0].kind, FolderKind::Inbox);
        // The server id is the identity, the name is only for display.
        assert_eq!(models[0].path, "1");
        assert_eq!(models[0].unread_count, 2);
        let projects = models.iter().find(|f| f.name == "Projekte").unwrap();
        let invoices = models.iter().find(|f| f.name == "Rechnungen").unwrap();
        assert_eq!(invoices.parent_id, Some(projects.id));
        assert!(projects.parent_id.is_none());
        assert!(models[0].uid_validity.is_none());
    }

    #[test]
    fn an_item_becomes_a_header() {
        let item = GwItem {
            id: "A1".to_string(),
            subject: "Rechnung 4711".to_string(),
            from: Some(crate::models::message::MailAddress::with_name(
                "ada@example.org",
                "Ada",
            )),
            date: types::timestamp(Some("20261112130000")),
            size_bytes: 2048,
            is_read: false,
            has_attachments: true,
            message_id: Some("rechnung@example.org".to_string()),
            item_type: "mail".to_string(),
            ..GwItem::default()
        };
        let folder_id = FolderId::new();
        let header = item_to_header(AccountId::new(), folder_id, &item);
        assert_eq!(header.server_id.as_deref(), Some("A1"));
        assert!(header.uid.is_none());
        assert_eq!(header.folder_id, folder_id);
        assert_eq!(header.subject, "Rechnung 4711");
        assert!(header.is_unread());
        assert!(header.has_attachments);
        assert_eq!(header.size_bytes, 2048);
        assert!(header.date.is_some());
    }

    #[test]
    fn a_session_error_is_recognised_but_other_errors_are_not_retried() {
        assert!(is_session_error(&Error::Auth {
            account: "ada".to_string(),
            message: "status 0x4202: session expired".to_string(),
        }));
        assert!(is_session_error(&Error::Auth {
            account: "ada".to_string(),
            message: "status 0x4400".to_string(),
        }));
        assert!(!is_session_error(&Error::Auth {
            account: "ada".to_string(),
            message: "status 0x4205: wrong password".to_string(),
        }));
        assert!(!is_session_error(&Error::timeout("GroupWise", 10)));
    }

    #[test]
    fn mime_data_is_decoded_and_broken_input_is_an_error() {
        use base64::Engine;
        let raw = b"Subject: Test\r\n\r\nHallo\r\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        assert_eq!(decode_mime(&encoded).unwrap(), raw);
        assert!(decode_mime("").unwrap().is_empty());
        assert!(matches!(
            decode_mime("nicht base64!").unwrap_err(),
            Error::MalformedResponse { .. }
        ));
    }
}
