//! One authenticated IMAP session.
//!
//! The connection layer is deliberately thin: it opens the socket (through
//! [`crate::net`]), negotiates `STARTTLS`, authenticates, and then exposes the
//! handful of operations HylMail actually uses.  Everything that decides *what*
//! to synchronise lives in [`super::synchronise`], so the protocol code stays
//! readable and the algorithm stays testable.
//!
//! Three protocol details are handled here because they are easy to get wrong:
//!
//! * `STARTTLS` runs **before** the credentials are sent, and the capability
//!   list is re-read afterwards — servers routinely advertise different
//!   capabilities on the encrypted connection;
//! * `EXAMINE` is used for folders HylMail only reads, so synchronising cannot
//!   leave a `\Seen` side effect behind;
//! * credentials are never put into a log record — the redactor in
//!   [`crate::logging::redact`] covers `LOGIN`/`AUTHENTICATE`, and the
//!   `Debug` impls here expose only the user name.

use async_imap::types::Mailbox;
use async_imap::{Authenticator, Client, Session};
use futures_util::TryStreamExt;
use secrecy::ExposeSecret;

use crate::error::{Error, Result};
use crate::models::account::{AuthKind, Endpoint, TlsMode};
use crate::models::folder::Folder;
use crate::models::ids::{AccountId, FolderId};
use crate::models::message::{LocalFlags, MessageFlags, MessageHeader};
use crate::net::{self, Stream, Timeouts, TlsPolicy};

use super::capabilities::Capabilities;
use super::fetch;
use super::flags;
use super::mailbox;

/// How a session proves who it is.
///
/// Re-exported from [`crate::secrets`] because IMAP and SMTP authenticate with
/// exactly the same material, and the `Debug` implementation there is the one
/// that guarantees a secret never reaches a log record.
pub use crate::secrets::Credential;

/// Everything needed to open one IMAP connection.
#[derive(Debug, Clone)]
pub struct ImapConfig {
    /// Server host, port and TLS mode.
    pub endpoint: Endpoint,
    /// Login name, which is not always the e-mail address.
    pub username: String,
    /// How to authenticate.
    pub auth: AuthKind,
    /// Certificate policy.
    pub policy: TlsPolicy,
    /// Deadlines.
    pub timeouts: Timeouts,
}

impl ImapConfig {
    /// Builds a configuration from the parts an account supplies.
    #[must_use]
    pub fn new(
        endpoint: Endpoint,
        username: impl Into<String>,
        auth: AuthKind,
        policy: TlsPolicy,
    ) -> Self {
        Self {
            endpoint,
            username: username.into(),
            auth,
            policy,
            timeouts: Timeouts::default(),
        }
    }

    /// Uses the short deadlines of the account dialog.
    #[must_use]
    pub fn probe(mut self) -> Self {
        self.timeouts = Timeouts::probe();
        self
    }
}

/// Facts about a successful login, for the account dialog and the sync log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutcome {
    /// Host that was connected to.
    pub host: String,
    /// Capabilities of the *authenticated* connection.
    pub capabilities: Capabilities,
    /// `true` when the stream is encrypted.
    pub encrypted: bool,
    /// Negotiated TLS version, e.g. `TLSv1.3`.
    pub tls_version: Option<String>,
}

/// State that survives handing the socket to the `IDLE` layer and back.
#[derive(Debug, Clone)]
pub(crate) struct SessionMeta {
    /// Capabilities of the authenticated connection.
    pub(crate) capabilities: Capabilities,
    /// `true` when the stream is encrypted.
    pub(crate) encrypted: bool,
    /// Negotiated TLS version, e.g. `TLSv1.3`.
    pub(crate) tls_version: Option<String>,
    /// Wire path of the mailbox that is currently selected or examined.
    pub(crate) selected: Option<String>,
}

/// An authenticated IMAP session.
pub struct ImapSession {
    session: Session<Stream>,
    host: String,
    meta: SessionMeta,
    /// Number of `IDLE` rounds already completed on this connection.
    idle_rounds: u32,
}

impl std::fmt::Debug for ImapSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapSession")
            .field("host", &self.host)
            .field("capabilities", &self.meta.capabilities.len())
            .field("selected", &self.meta.selected)
            .finish()
    }
}

/// A connected but not yet authenticated client.
struct Pending {
    client: Client<Stream>,
    tls_version: Option<String>,
    encrypted: bool,
}

impl ImapSession {
    /// Rebuilds a session that came back from the `IDLE` layer.
    pub(crate) fn from_parts(
        session: Session<Stream>,
        host: String,
        meta: SessionMeta,
        idle_rounds: u32,
    ) -> Self {
        Self {
            session,
            host,
            meta,
            idle_rounds,
        }
    }

    /// Connects, optionally negotiates `STARTTLS`, and authenticates.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Connect`], [`Error::Tls`], [`Error::Auth`],
    /// [`Error::Timeout`], [`Error::MissingCapability`] or
    /// [`Error::Protocol`] — every one of which the account dialog can show to
    /// the user as is.
    pub async fn login(config: &ImapConfig, credential: Credential<'_>) -> Result<Self> {
        let pending = connect_and_upgrade(config).await?;
        let authenticated = log_in(pending, config, credential).await?;
        Ok(Self {
            session: authenticated.session,
            host: config.endpoint.host.clone(),
            meta: authenticated.meta,
            idle_rounds: 0,
        })
    }

    /// The capabilities of the authenticated connection.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        &self.meta.capabilities
    }

    /// Facts about this session, without the credentials.
    #[must_use]
    pub fn outcome(&self) -> LoginOutcome {
        LoginOutcome {
            host: self.host.clone(),
            capabilities: self.meta.capabilities.clone(),
            encrypted: self.meta.encrypted,
            tls_version: self.meta.tls_version.clone(),
        }
    }

    /// The host that was connected to.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The wire path of the selected mailbox, if any.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.meta.selected.as_deref()
    }

    /// How many times `IDLE` was re-issued on this connection.
    #[must_use]
    pub fn idle_rounds(&self) -> u32 {
        self.idle_rounds
    }

    /// Selects a mailbox read-only and returns its metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] when the server answers `NO` — typically
    /// because the mailbox does not exist or is `\Noselect`.
    pub async fn examine(&mut self, wire_path: &str) -> Result<Mailbox> {
        let mailbox = self
            .session
            .examine(wire_path)
            .await
            .map_err(|error| protocol_error("EXAMINE", &error))?;
        self.meta.selected = Some(wire_path.to_string());
        Ok(mailbox)
    }

    /// Selects a mailbox read-write.
    ///
    /// # Errors
    ///
    /// See [`Self::examine`].
    pub async fn select(&mut self, wire_path: &str) -> Result<Mailbox> {
        let mailbox = self
            .session
            .select(wire_path)
            .await
            .map_err(|error| protocol_error("SELECT", &error))?;
        self.meta.selected = Some(wire_path.to_string());
        Ok(mailbox)
    }

    /// Lists every mailbox of the account, already ordered and linked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] when the server refuses `LIST`.
    pub async fn list_folders(&mut self, account_id: AccountId) -> Result<Vec<Folder>> {
        let names: Vec<async_imap::types::Name> = self
            .session
            .list(Some(""), Some("*"))
            .await
            .map_err(|error| protocol_error("LIST", &error))?
            .try_collect()
            .await
            .map_err(|error| protocol_error("LIST", &error))?;
        Ok(mailbox::build_tree(account_id, &names))
    }

    /// Fetches the headers of the given UIDs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] on a refused `UID FETCH`.
    pub async fn fetch_headers(
        &mut self,
        account_id: AccountId,
        folder_id: FolderId,
        uid_set: &str,
    ) -> Result<Vec<MessageHeader>> {
        let fetches: Vec<async_imap::types::Fetch> = self
            .session
            .uid_fetch(uid_set, fetch::HEADER_ITEMS)
            .await
            .map_err(|error| protocol_error("UID FETCH", &error))?
            .try_collect()
            .await
            .map_err(|error| protocol_error("UID FETCH", &error))?;

        Ok(fetches
            .iter()
            .filter(|fetch| fetch.uid.is_some())
            .map(|fetch| fetch::header_from_fetch(account_id, folder_id, fetch))
            .collect())
    }

    /// Fetches only `UID`/`FLAGS` pairs, used by the flag reconciliation pass.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] on a refused `UID FETCH`.
    pub async fn fetch_flags(&mut self, uid_set: &str) -> Result<Vec<(u32, MessageFlags)>> {
        let fetches: Vec<async_imap::types::Fetch> = self
            .session
            .uid_fetch(uid_set, fetch::FLAG_ITEMS)
            .await
            .map_err(|error| protocol_error("UID FETCH", &error))?
            .try_collect()
            .await
            .map_err(|error| protocol_error("UID FETCH", &error))?;

        Ok(fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                let atoms: Vec<String> = flags::atoms_from_flags(fetch.flags());
                Some((
                    uid,
                    flags::message_flags_from_atoms(atoms.iter().map(String::as_str)),
                ))
            })
            .collect())
    }

    /// Downloads the complete raw message.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the UID is not in the selected mailbox.
    pub async fn fetch_raw(&mut self, uid: u32) -> Result<Vec<u8>> {
        let fetches: Vec<async_imap::types::Fetch> = self
            .session
            .uid_fetch(uid.to_string(), fetch::BODY_ITEMS)
            .await
            .map_err(|error| protocol_error("UID FETCH", &error))?
            .try_collect()
            .await
            .map_err(|error| protocol_error("UID FETCH", &error))?;

        fetches
            .iter()
            .find_map(|fetch| fetch.body().map(<[u8]>::to_vec))
            .ok_or_else(|| Error::NotFound {
                entity: "message",
                id: uid.to_string(),
            })
    }

    /// Pushes a flag change to the server.
    ///
    /// Returns `true` when a `STORE` was actually sent.  Only the difference
    /// between the current and the desired state is transmitted, because
    /// sending `-FLAGS` for a flag the message does not carry is a wasted round
    /// trip on some servers and an error on others.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] when the server refuses the `STORE`.
    pub async fn store_flags(
        &mut self,
        uid: u32,
        desired: MessageFlags,
        current: MessageFlags,
        local: LocalFlags,
    ) -> Result<bool> {
        let (add, remove) = flags::diff_for_store(desired, current, local);
        let Some(command) = flags::store_command(&uid.to_string(), &add, &remove) else {
            return Ok(false);
        };
        self.session
            .run_command_and_check_ok(&command)
            .await
            .map_err(|error| protocol_error("UID STORE", &error))?;
        Ok(true)
    }

    /// Moves a message into another mailbox, using `MOVE` when available.
    ///
    /// Without `MOVE` the message is copied and then flagged `\Deleted`; the
    /// expunge is left to the caller, because a plain `EXPUNGE` would also
    /// remove messages another client deleted.
    ///
    /// Returns `true` when the server did the move atomically.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] when both `MOVE` and `COPY` are refused.
    pub async fn move_message(&mut self, uid: u32, target_wire_path: &str) -> Result<bool> {
        if self.meta.capabilities.has_move() {
            self.session
                .uid_mv(uid.to_string(), target_wire_path)
                .await
                .map_err(|error| protocol_error("UID MOVE", &error))?;
            return Ok(true);
        }
        self.session
            .uid_copy(uid.to_string(), target_wire_path)
            .await
            .map_err(|error| protocol_error("UID COPY", &error))?;
        self.session
            .run_command_and_check_ok(&format!("UID STORE {uid} +FLAGS.SILENT (\\Deleted)"))
            .await
            .map_err(|error| protocol_error("UID STORE", &error))?;
        Ok(false)
    }

    /// Removes the messages flagged `\Deleted` from the selected mailbox.
    ///
    /// With `UIDPLUS` only `uid` is expunged, which is what a delete of a
    /// single message wants: a plain `EXPUNGE` would also drop the messages
    /// another client had flagged.  Without the extension there is no way to
    /// name a single message, so the whole mailbox is expunged - the caller
    /// learns this from the capability, not from a failure.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] when the server refuses the expunge, for
    /// example because no mailbox is selected.
    pub async fn expunge(&mut self, uid: Option<u32>) -> Result<()> {
        let uidplused = uid.is_some() && self.meta.capabilities.has_uidplus();
        let operation = if uidplused { "UID EXPUNGE" } else { "EXPUNGE" };
        let command = match uid {
            Some(uid) if uidplused => format!("{operation} {uid}"),
            _ => operation.to_string(),
        };
        self.session
            .run_command_and_check_ok(&command)
            .await
            .map_err(|error| protocol_error(operation, &error))?;
        Ok(())
    }

    /// Searches the selected mailbox and returns the matching UIDs, ascending.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] on a refused `UID SEARCH`.
    pub async fn search(&mut self, query: &str) -> Result<Vec<u32>> {
        let mut uids: Vec<u32> = self
            .session
            .uid_search(query)
            .await
            .map_err(|error| protocol_error("UID SEARCH", &error))?
            .into_iter()
            .collect();
        uids.sort_unstable();
        Ok(uids)
    }

    /// Uploads a message to a mailbox.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] on a refused `APPEND`.
    pub async fn append(
        &mut self,
        wire_path: &str,
        message: &[u8],
        flags: &[String],
    ) -> Result<()> {
        let flags = if flags.is_empty() {
            None
        } else {
            Some(flags.join(" "))
        };
        self.session
            .append(wire_path, flags.as_deref(), None, message)
            .await
            .map_err(|error| protocol_error("APPEND", &error))
    }

    /// Sends `NOOP`, which lets the server flush pending notifications.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] when the server refuses `NOOP`.
    pub async fn noop(&mut self) -> Result<()> {
        self.session
            .noop()
            .await
            .map_err(|error| protocol_error("NOOP", &error))
    }

    /// Logs out.  Errors are logged, not returned: the session is gone either
    /// way, and the caller is usually already shutting down.
    pub async fn logout(mut self) {
        if let Err(error) = self.session.logout().await {
            tracing::debug!(%error, "IMAP LOGOUT failed");
        }
    }

    /// Hands the session to the IDLE layer.
    ///
    /// Callers must check [`Capabilities::has_idle`] first; the conversion
    /// itself cannot fail, because it only moves the socket.  That keeps the
    /// error handling in one place: a server without `IDLE` is a *decision* of
    /// the sync loop (poll instead), not an error.
    #[must_use]
    pub fn into_idle(self) -> super::idle::IdleGuard {
        super::idle::IdleGuard::new(self.session, self.host, self.meta)
    }
}

/// The result of a login.
struct Authenticated {
    session: Session<Stream>,
    meta: SessionMeta,
}

/// Opens the connection and runs `STARTTLS` when the account asks for it.
async fn connect_and_upgrade(config: &ImapConfig) -> Result<Pending> {
    let stream = net::connect(&config.endpoint, &config.policy, &config.timeouts).await?;
    let tls_version = stream.tls_version().map(|version| format!("{version:?}"));
    let encrypted = stream.is_encrypted();
    let host = config.endpoint.host.clone();

    let mut client = Client::new(stream);
    let greeting = read_greeting(&mut client, &config.timeouts).await?;

    let mut pending = Pending {
        client,
        tls_version,
        encrypted,
    };

    if config.endpoint.tls == TlsMode::StartTls {
        // Only trust a capability list that the greeting actually carried: an
        // empty set means "the server did not say", not "not supported".  In
        // that case `STARTTLS` is attempted and the server's own `NO`/`BAD` is
        // reported, which is far more useful than a guessed refusal.
        if !greeting.is_empty() && !greeting.has_starttls() {
            return Err(Error::MissingCapability {
                protocol: "IMAP",
                host,
                capability: "STARTTLS".to_string(),
            });
        }
        pending
            .client
            .run_command_and_check_ok("STARTTLS", None)
            .await
            .map_err(|error| protocol_error("STARTTLS", &error))?;

        let stream = pending.client.into_inner();
        let stream = net::start_tls(stream, &host, &config.policy, &config.timeouts).await?;
        pending.tls_version = stream.tls_version().map(|version| format!("{version:?}"));
        pending.encrypted = stream.is_encrypted();
        pending.client = Client::new(stream);
        read_greeting(&mut pending.client, &config.timeouts).await?;
    }

    Ok(pending)
}

/// Reads the server greeting.
///
/// Most servers announce their capabilities in the greeting
/// (`* OK [CAPABILITY ...] ready`), which is the only way to learn them before
/// authenticating — `Session::capabilities()` does not exist yet.  When the
/// greeting carries no capability list the client proceeds anyway and uses the
/// list of the authenticated connection.
async fn read_greeting(client: &mut Client<Stream>, timeouts: &Timeouts) -> Result<Capabilities> {
    let operation = "the IMAP greeting";
    let greeting = net::with_deadline(timeouts.greeting, operation, async {
        client
            .read_response()
            .await
            .map_err(|error| io_error("greeting", &error))
    })
    .await?;

    let Some(response) = greeting else {
        return Err(Error::ConnectionLost {
            protocol: "IMAP",
            host: "greeting".to_string(),
        });
    };

    match response.parsed() {
        imap_proto::Response::Done {
            status: imap_proto::Status::Bye,
            outcome,
            ..
        }
        | imap_proto::Response::Data {
            status: imap_proto::Status::Bye,
            outcome,
        } => Err(Error::protocol(
            "IMAP",
            "greeting",
            outcome
                .information
                .as_ref()
                .map(|text| text.to_string())
                .unwrap_or_else(|| "the server refused the connection".to_string()),
        )),
        imap_proto::Response::Data { outcome, .. } => Ok(outcome
            .code
            .as_ref()
            .and_then(|code| match code {
                imap_proto::ResponseCode::Capabilities(capabilities) => Some(
                    Capabilities::from_names(capabilities.iter().map(capability_name)),
                ),
                _ => None,
            })
            .unwrap_or_default()),
        _ => Ok(Capabilities::default()),
    }
}

/// Renders one `imap_proto` capability the way a server would spell it.
fn capability_name(capability: &imap_proto::Capability<'_>) -> String {
    match capability {
        imap_proto::Capability::Imap4rev1 => "IMAP4REV1".to_string(),
        imap_proto::Capability::Auth(mechanism) => format!("AUTH={mechanism}"),
        imap_proto::Capability::Atom(atom) => atom.to_string(),
    }
}

/// Authenticates with the mechanism the account asked for.
async fn log_in(
    pending: Pending,
    config: &ImapConfig,
    credential: Credential<'_>,
) -> Result<Authenticated> {
    let Pending {
        client,
        tls_version,
        encrypted,
    } = pending;
    let user = config.username.clone();
    let timeouts = config.timeouts;

    let (session, capabilities) = match credential {
        Credential::Password(password) => {
            if config.auth == AuthKind::OAuth2 {
                return Err(Error::Unsupported(
                    "an OAuth2 account needs an access token, not a password".to_string(),
                ));
            }
            let password = password.expose_secret().to_string();
            let operation = format!("IMAP login for {user}");
            net::with_deadline(timeouts.command, &operation, async {
                client
                    .login_with_capabilities(&user, &password)
                    .await
                    .map_err(|(error, _)| authentication_error(&user, "LOGIN", &error))
            })
            .await?
        }
        Credential::OAuth2 {
            user: token_user,
            token,
        } => {
            let token = token.expose_secret().to_string();
            let operation = format!("IMAP XOAUTH2 login for {user}");
            let authenticator = XOAuth2 {
                response: format!("user={token_user}\x01auth=Bearer {token}\x01\x01"),
            };
            let session = net::with_deadline(timeouts.command, &operation, async {
                client
                    .authenticate("XOAUTH2", authenticator)
                    .await
                    .map_err(|(error, _)| {
                        authentication_error(&user, "AUTHENTICATE XOAUTH2", &error)
                    })
            })
            .await?;
            // `AUTHENTICATE` carries no capability list, so the authenticated
            // session is asked for one below.
            (session, None)
        }
    };

    let mut session = session;
    let capabilities = match capabilities {
        Some(capabilities) => Capabilities::from_async_imap(&capabilities),
        None => capabilities_of(&mut session).await?,
    };
    Ok(Authenticated {
        session,
        meta: SessionMeta {
            capabilities,
            encrypted,
            tls_version,
            selected: None,
        },
    })
}

/// Asks the *authenticated* server for its capabilities.
async fn capabilities_of(session: &mut Session<Stream>) -> Result<Capabilities> {
    let capabilities = session
        .capabilities()
        .await
        .map_err(|error| protocol_error("CAPABILITY", &error))?;
    Ok(Capabilities::from_async_imap(&capabilities))
}

/// A one-shot SASL authenticator: XOAUTH2 has no challenge/response dance, the
/// token is sent as the first (and only) response.
struct XOAuth2 {
    response: String,
}

impl Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        std::mem::take(&mut self.response)
    }
}

/// Maps an i/o failure of the connection onto [`Error::ConnectionLost`].
fn io_error(command: &str, error: &std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::TimedOut
        || error.kind() == std::io::ErrorKind::WouldBlock
    {
        return Error::timeout(format!("IMAP {command}"), 0);
    }
    Error::Io {
        path: std::path::PathBuf::from(format!("imap:{command}")),
        source: std::io::Error::new(error.kind(), error.to_string()),
    }
}

/// Maps a refused authentication onto the error the account dialog needs.
///
/// A `NO` reply is what a server answers for wrong credentials, so it becomes
/// [`Error::Auth`] and the dialog can point the user at the password field.  A
/// `BAD` reply is a syntax or mechanism problem and stays a protocol error.
fn authentication_error(
    account: &str,
    command: &'static str,
    error: &async_imap::error::Error,
) -> Error {
    match error {
        async_imap::error::Error::No(response) => Error::Auth {
            account: account.to_string(),
            message: response.clone(),
        },
        other => protocol_error(command, other),
    }
}

/// Maps a `NO`/`BAD` answer onto [`Error::Protocol`].
fn protocol_error(command: &'static str, error: &async_imap::error::Error) -> Error {
    match error {
        async_imap::error::Error::No(response) | async_imap::error::Error::Bad(response) => {
            Error::protocol("IMAP", command, response)
        }
        async_imap::error::Error::Parse(error) => Error::malformed("IMAP", error.to_string()),
        async_imap::error::Error::Io(source) => io_error(command, source),
        async_imap::error::Error::ConnectionLost => Error::ConnectionLost {
            protocol: "IMAP",
            host: command.to_string(),
        },
        other => Error::protocol("IMAP", command, other.to_string()),
    }
}
