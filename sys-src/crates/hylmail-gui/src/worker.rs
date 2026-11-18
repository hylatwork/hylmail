//! The background half of the interface.
//!
//! The window itself never blocks.  Everything that needs the network - a body
//! download, a flag push, a submission - is sent to this worker, which owns the
//! Tokio runtime, the synchronization engine and the outbox, and reports back
//! through a channel that the interface polls once per frame.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use hylmail_core::compose::{ComposeContext, ComposedMessage};
use hylmail_core::db::{folders as db_folders, messages as db_messages};
use hylmail_core::delete::{DeleteMode, DeleteOutcome};
use hylmail_core::html::HtmlPolicy;
use hylmail_core::models::folder::FolderKind;
use hylmail_core::models::{
    utc_now, Account, AccountId, AccountKind, AuthKind, Draft, Folder, FolderId, MailAddress,
    MessageBody, MessageHeader, MessageId, OutboxItem, OutboxState, Settings, Signature,
};
use hylmail_core::net::TlsPolicy;
use hylmail_core::outbox::{self, Delivery, Submitter};
use hylmail_core::secrets::{self, Credential, CredentialKind};
use hylmail_core::send;
use hylmail_core::store::ConfigStore;
use hylmail_core::sync::{EngineCommand, EngineConfig, EngineEvent, EngineHandle, SyncEngine};
use hylmail_core::{AppPaths, Core, Error, Result};

use crate::state::Task;

/// How often the background outbox retry runs.
///
/// The per-item backoff decides when a queued message is actually due, so the
/// tick only has to be short enough that a message becomes "soon"; a minute is
/// the pace a user expects from "wird automatisch gesendet".
const OUTBOX_RETRY_TICK: Duration = Duration::from_secs(60);

/// Something the worker wants the interface to know.
pub(crate) enum UiEvent {
    /// A body download finished, successfully or not.
    BodyLoaded {
        /// Message the body belongs to.
        message_id: MessageId,
        /// The parsed body, when the download succeeded.
        body: Option<Box<MessageBody>>,
        /// The reason the download failed.
        error: Option<String>,
    },
    /// A flag push finished.
    FlagsPushed {
        /// Message whose flags were pushed.
        message_id: MessageId,
        /// The reason the push failed.
        error: Option<String>,
    },
    /// Attachments were written to disk, or could not be.
    AttachmentsSaved {
        /// Message the attachments came from.
        message_id: MessageId,
        /// How many files were written.
        count: usize,
        /// The reason nothing was written.
        error: Option<String>,
    },
    /// A message was deleted on the server, or could not be.
    Deleted {
        /// Folder the message was in.
        folder_id: FolderId,
        /// The message that is gone.
        message_id: MessageId,
        /// It was moved into the trash rather than removed outright.
        moved: bool,
        /// The reason the delete failed.
        error: Option<String>,
    },
    /// A message was moved into another folder, or could not be.
    Moved {
        /// Folder the message was in.
        folder_id: FolderId,
        /// The message that moved.
        message_id: MessageId,
        /// Folder it is in now.
        target_folder_id: FolderId,
        /// The name of the destination, for the status bar.
        target_name: String,
        /// The reason the move failed.
        error: Option<String>,
    },
    /// A submission finished.
    MailSent {
        /// Human readable outcome for the status bar.
        detail: String,
        /// `true` when the message left the machine.
        ok: bool,
    },
    /// The OAuth2 sign-in finished (or was aborted).
    OAuthFinished {
        /// Account the tokens belong to.
        account_id: hylmail_core::models::AccountId,
        /// Address of that account, for the message.
        email: String,
        /// Human readable outcome for the status bar.
        detail: String,
        /// The reason the flow failed.
        error: Option<String>,
    },
    /// A CardDAV address book synchronisation finished.
    ContactsSynced {
        /// How many vCards were downloaded because they changed.
        fetched: usize,
        /// The reason the sync failed.
        error: Option<String>,
    },
    /// A message was written to an `.eml` file, or could not be.
    EmlSaved {
        /// Destination the file was written to.
        path: PathBuf,
        /// The reason nothing was written.
        error: Option<String>,
    },
    /// The background outbox retry delivered queued mail.
    OutboxSent {
        /// How many queued messages left the machine.
        count: usize,
    },
    /// Remote images of a message were downloaded and decoded (`FR-READ-2/3`).
    ImagesLoaded {
        /// Message the images belong to.
        message_id: MessageId,
        /// The successfully decoded images, keyed by URL.
        images: Vec<(String, hylmail_core::image::DecodedImage)>,
    },
    /// A server-side search finished (`FR-SEARCH-2`).
    SearchResults {
        /// How many messages the server reported.
        found: usize,
        /// The reason the search failed.
        error: Option<String>,
    },
    /// A failure the user should see.
    Error(String),
}

/// Everything the worker needs, as owned clones.
///
/// `Core` is not `Clone` and the window owns it, so the worker gets the four
/// pieces it actually uses.  Settings and the account list are re-read from
/// disk whenever they matter, which keeps a settings change in the dialog
/// effective on the very next submission.
struct WorkerContext {
    paths: AppPaths,
    database: Arc<hylmail_core::db::Database>,
    secrets: Arc<dyn secrets::SecretStore>,
    tls: TlsPolicy,
}

impl WorkerContext {
    fn store(&self) -> ConfigStore {
        ConfigStore::new(self.paths.clone())
    }

    fn settings(&self) -> Result<Settings> {
        self.store().load_settings()
    }

    fn account(&self, id: AccountId) -> Result<Account> {
        self.store()
            .load_accounts()?
            .accounts
            .into_iter()
            .find(|account| account.id == id)
            .ok_or_else(|| Error::config("das Konto ist nicht mehr eingerichtet"))
    }
}

/// The synchronization engine plus the network jobs of the reading pane.
pub(crate) struct Worker {
    jobs: Option<tokio::sync::mpsc::UnboundedSender<Task>>,
    events: Receiver<UiEvent>,
    runtime: Option<tokio::runtime::Runtime>,
    engine: Option<EngineHandle>,
    started: bool,
}

impl Worker {
    /// Starts the runtime and the synchronization engine.
    ///
    /// A failure here is not fatal: the window still shows the locally cached
    /// mail, and the reason is reported in the status bar.
    pub(crate) fn start(core: &Core, wake: egui::Context) -> Self {
        let (events_tx, events_rx) = channel();
        let context = WorkerContext {
            paths: core.paths().clone(),
            database: core.database_handle(),
            secrets: core.secrets_handle(),
            tls: TlsPolicy::strict(),
        };

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("hylmail-worker")
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = events_tx.send(UiEvent::Error(format!(
                    "Der Hintergrunddienst konnte nicht starten: {error}"
                )));
                return Self {
                    jobs: None,
                    events: events_rx,
                    runtime: None,
                    engine: None,
                    started: false,
                };
            }
        };

        let (jobs_tx, jobs_rx) = tokio::sync::mpsc::unbounded_channel();
        let wake = Arc::new(wake);

        let config = EngineConfig::new(
            context.paths.clone(),
            Arc::clone(&context.database),
            Arc::clone(&context.secrets),
            context.settings().unwrap_or_default(),
        );
        let (engine, _join) = runtime.block_on(async { SyncEngine::spawn(config) });

        spawn_jobs(&runtime, context, jobs_rx, events_tx, Arc::clone(&wake));

        Self {
            jobs: Some(jobs_tx),
            events: events_rx,
            runtime: Some(runtime),
            engine: Some(engine),
            started: true,
        }
    }

    /// Whether the runtime started.
    #[must_use]
    pub(crate) fn is_running(&self) -> bool {
        self.started
    }

    /// Hands a task to the worker.
    pub(crate) fn dispatch(&self, task: Task) {
        let Some(jobs) = self.jobs.as_ref() else {
            return;
        };
        if jobs.send(task).is_err() {
            tracing::warn!("the HylMail worker no longer accepts jobs");
        }
    }

    /// Tells an account to synchronise now.
    pub(crate) fn sync_account(&self, account_id: AccountId) {
        if let Some(engine) = self.engine.as_ref() {
            if let Err(error) = engine.send(EngineCommand::SyncNow { account_id }) {
                tracing::warn!(%error, "the synchronization engine refused a command");
            }
        }
    }

    /// Tells every account to synchronise now.
    pub(crate) fn sync_all(&self) {
        if let Some(engine) = self.engine.as_ref() {
            if let Err(error) = engine.sync_all() {
                tracing::warn!(%error, "the synchronization engine refused a command");
            }
        }
    }

    /// Tells the engine that the account list changed.
    ///
    /// The engine reads the account list once at start-up, so a newly created
    /// account stays unreachable until this command makes it re-read the list.
    pub(crate) fn reload_accounts(&self) {
        if let Some(engine) = self.engine.as_ref() {
            if let Err(error) = engine.accounts_changed() {
                tracing::warn!(%error, "the synchronization engine refused a command");
            }
        }
    }

    /// Tells the engine that the settings changed, so it applies them without a
    /// restart of the application.
    pub(crate) fn reload_settings(&self) {
        if let Some(engine) = self.engine.as_ref() {
            if let Err(error) = engine.settings_changed() {
                tracing::warn!(%error, "the synchronization engine refused a command");
            }
        }
    }

    /// Tells an account to re-read its folder list.
    pub(crate) fn refresh_folders(&self, account_id: AccountId) {
        if let Some(engine) = self.engine.as_ref() {
            if let Err(error) = engine.send(EngineCommand::RefreshFolders { account_id }) {
                tracing::warn!(%error, "the synchronization engine refused a command");
            }
        }
    }

    /// The next engine event, if one is waiting.
    pub(crate) fn engine_event(&mut self) -> Option<EngineEvent> {
        self.engine.as_mut()?.try_recv()
    }

    /// The next interface event, if one is waiting.
    pub(crate) fn try_event(&mut self) -> Option<UiEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.as_ref() {
            let _ = engine.shutdown();
        }
        // Dropping the runtime waits for the blocking tasks; the channel is
        // closed first so the job loop ends instead of running forever.
        self.jobs = None;
        self.runtime = None;
    }
}

/// Starts the job loop on the worker runtime.
fn spawn_jobs(
    runtime: &tokio::runtime::Runtime,
    context: WorkerContext,
    mut jobs: tokio::sync::mpsc::UnboundedReceiver<Task>,
    events: Sender<UiEvent>,
    wake: Arc<egui::Context>,
) {
    runtime.spawn(async move {
        // A background tick retries the outbox (`FR-OUTBOX-2`): a message queued
        // while offline is delivered as soon as the connection is back, without
        // the user having to send anything first.
        let mut ticker = tokio::time::interval(OUTBOX_RETRY_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let task = tokio::select! {
                task = jobs.recv() => task,
                _ = ticker.tick() => {
                    if let Some(event) = drain_outbox(&context).await {
                        let _ = events.send(event);
                    }
                    wake.request_repaint();
                    continue;
                }
            };
            let Some(task) = task else { break };
            match task {
                Task::FetchBody {
                    folder_id,
                    message_id,
                } => {
                    let outcome = download_body(&context, folder_id, message_id).await;
                    let event = match outcome {
                        Ok(body) => UiEvent::BodyLoaded {
                            message_id,
                            body: Some(Box::new(body)),
                            error: None,
                        },
                        Err(error) => UiEvent::BodyLoaded {
                            message_id,
                            body: None,
                            error: Some(error.to_string()),
                        },
                    };
                    let _ = events.send(event);
                }
                Task::SaveAttachments {
                    folder_id,
                    message_id,
                    targets,
                } => {
                    let event =
                        match write_attachments(&context, folder_id, message_id, &targets).await {
                            Ok(count) => UiEvent::AttachmentsSaved {
                                message_id,
                                count,
                                error: None,
                            },
                            Err(error) => UiEvent::AttachmentsSaved {
                                message_id,
                                count: 0,
                                error: Some(error.to_string()),
                            },
                        };
                    let _ = events.send(event);
                }
                Task::PushFlags { message_id } => {
                    let error = push_flags(&context, message_id)
                        .await
                        .err()
                        .map(|error| error.to_string());
                    let _ = events.send(UiEvent::FlagsPushed { message_id, error });
                }
                Task::DeleteMessage {
                    folder_id,
                    message_id,
                    permanent,
                } => {
                    let event =
                        match delete_message(&context, folder_id, message_id, permanent).await {
                            Ok(outcome) => UiEvent::Deleted {
                                folder_id,
                                message_id,
                                moved: outcome == DeleteOutcome::MovedToTrash,
                                error: None,
                            },
                            Err(error) => UiEvent::Deleted {
                                folder_id,
                                message_id,
                                moved: false,
                                error: Some(error.to_string()),
                            },
                        };
                    let _ = events.send(event);
                }
                Task::MoveMessage {
                    folder_id,
                    message_id,
                    target_folder_id,
                } => {
                    let event =
                        match move_message(&context, folder_id, message_id, target_folder_id).await
                        {
                            Ok(target) => UiEvent::Moved {
                                folder_id,
                                message_id,
                                target_folder_id,
                                target_name: target.name.clone(),
                                error: None,
                            },
                            Err(error) => UiEvent::Moved {
                                folder_id,
                                message_id,
                                target_folder_id,
                                target_name: String::new(),
                                error: Some(error.to_string()),
                            },
                        };
                    let _ = events.send(event);
                }
                Task::Submit {
                    account_id,
                    draft,
                    signature,
                } => {
                    let event = submit(&context, account_id, *draft, signature).await;
                    let _ = events.send(event);
                }
                Task::OAuthLogin { account } => {
                    let event = oauth_login(&context, *account).await;
                    let _ = events.send(event);
                }
                Task::SyncContacts { account_id } => {
                    let event = sync_contacts(&context, account_id).await;
                    let _ = events.send(event);
                }
                Task::SaveEml {
                    folder_id,
                    message_id,
                    path,
                } => {
                    let event = match save_eml(&context, folder_id, message_id, &path).await {
                        Ok(()) => UiEvent::EmlSaved { path, error: None },
                        Err(error) => UiEvent::EmlSaved {
                            path,
                            error: Some(error.to_string()),
                        },
                    };
                    let _ = events.send(event);
                }
                Task::Sync(_)
                | Task::ReloadFolders(_)
                | Task::ReloadAccounts
                | Task::ReloadSettings => {}
                Task::DrainOutbox => {
                    let event = drain_outbox(&context)
                        .await
                        .unwrap_or(UiEvent::OutboxSent { count: 0 });
                    let _ = events.send(event);
                }
                Task::LoadImages { message_id, urls } => {
                    let settings = context.settings().unwrap_or_default();
                    let limits = hylmail_core::image::ImageLimits {
                        timeout: Duration::from_secs(settings.image_timeout_seconds.max(1)),
                        per_image_bytes: settings.image_max_bytes().max(1),
                    };
                    let images = hylmail_core::image::fetch_all(&urls, &context.tls, limits).await;
                    let _ = events.send(UiEvent::ImagesLoaded { message_id, images });
                }
                Task::ServerSearch { query } => {
                    let event = server_search(&context, &query).await;
                    let _ = events.send(event);
                }
            }
            wake.request_repaint();
        }
    });
}

/// The message, its folder, its account and the password to log in with.
///
/// Every per-message network call needs exactly this, and resolving it means
/// four lookups; keeping it in one place is what stops the three call sites
/// from drifting apart.
struct MessageSite {
    header: MessageHeader,
    folder: Folder,
    account: Account,
    secret: secrecy::SecretString,
}

impl MessageSite {
    /// Resolves everything needed to talk to the server about one message.
    ///
    /// `folder_id` defaults to the folder the stored header belongs to.
    fn load(
        context: &WorkerContext,
        message_id: MessageId,
        folder_id: Option<FolderId>,
    ) -> Result<Self> {
        let header = context
            .database
            .read(|connection| db_messages::get_message(connection, message_id))?
            .ok_or_else(|| Error::NotFound {
                entity: "message",
                id: message_id.to_string(),
            })?;
        let folder_id = folder_id.unwrap_or(header.folder_id);
        let folder = context
            .database
            .read(|connection| db_folders::get_folder(connection, folder_id))?
            .ok_or_else(|| Error::NotFound {
                entity: "folder",
                id: folder_id.to_string(),
            })?;
        let account = context.account(header.account_id)?;
        let secret = incoming_secret(context, &account)?;
        Ok(Self {
            header,
            folder,
            account,
            secret,
        })
    }

    /// Borrows the credential in the shape the transport expects.
    fn credential(&self) -> Credential<'_> {
        credential_for(&self.account, &self.secret)
    }
}

/// Downloads and caches the body of one message.
async fn download_body(
    context: &WorkerContext,
    folder_id: FolderId,
    message_id: MessageId,
) -> Result<MessageBody> {
    let site = MessageSite::load(context, message_id, Some(folder_id))?;
    let settings = context.settings()?;
    let sender = site
        .header
        .from
        .first()
        .map(|address| address.email.as_str());
    let policy = HtmlPolicy::from_settings(&settings, sender);

    hylmail_core::body::download(
        &context.database,
        &site.account,
        &site.folder,
        &site.header,
        site.credential(),
        &context.tls,
        &policy,
    )
    .await
}

/// Runs the OAuth2 sign-in and reports the outcome to the accounts dialog.
///
/// The worker owns the wait: the loopback listener blocks until the user
/// confirms in the browser (or [`hylmail_core::oauth::DEFAULT_TIMEOUT`] expires),
/// while the interface keeps drawing.  The tokens themselves never leave the
/// vault - only their existence is reported.
async fn oauth_login(context: &WorkerContext, account: Account) -> UiEvent {
    match hylmail_core::oauth::sign_in(context.secrets.as_ref(), &account, &context.tls).await {
        Ok(tokens) => {
            tracing::debug!(account = %account.id, "stored OAuth2 tokens in the vault");
            let what = if tokens.refresh_token().is_some() {
                "Zugriffs- und Erneuerungstoken"
            } else {
                "Zugriffstoken"
            };
            UiEvent::OAuthFinished {
                account_id: account.id,
                email: account.from_address().to_string(),
                detail: format!("Anmeldung abgeschlossen; {what} im Tresor abgelegt."),
                error: None,
            }
        }
        Err(error) => {
            tracing::debug!(account = %account.id, %error, "OAuth2 sign-in failed");
            UiEvent::OAuthFinished {
                account_id: account.id,
                email: account.from_address().to_string(),
                detail: String::new(),
                error: Some(error.to_string()),
            }
        }
    }
}

async fn write_attachments(
    context: &WorkerContext,
    folder_id: FolderId,
    message_id: MessageId,
    targets: &[(String, PathBuf)],
) -> Result<usize> {
    let site = MessageSite::load(context, message_id, Some(folder_id))?;
    let raw = hylmail_core::body::fetch_raw(
        &site.account,
        &site.folder,
        &site.header,
        site.credential(),
        &context.tls,
    )
    .await?;
    hylmail_core::body::save_attachments(&raw, targets)
}

/// Synchronises the address book from the account's CardDAV collection.
///
/// The collection URL and the optional user name live in the settings; the
/// password is the account's incoming secret, which is what a corporate CardDAV
/// server shares with the mail account.
async fn sync_contacts(context: &WorkerContext, account_id: AccountId) -> UiEvent {
    let settings = match context.settings() {
        Ok(settings) => settings,
        Err(error) => {
            return UiEvent::ContactsSynced {
                fetched: 0,
                error: Some(error.to_string()),
            }
        }
    };
    if settings.carddav_url.trim().is_empty() {
        return UiEvent::ContactsSynced {
            fetched: 0,
            error: Some(
                "Es ist keine CardDAV-Adresse in den Einstellungen hinterlegt.".to_string(),
            ),
        };
    }
    let account = match context.account(account_id) {
        Ok(account) => account,
        Err(error) => {
            return UiEvent::ContactsSynced {
                fetched: 0,
                error: Some(error.to_string()),
            }
        }
    };
    let username = if settings.carddav_username.trim().is_empty() {
        account.username.clone()
    } else {
        settings.carddav_username.clone()
    };
    let secret = match incoming_secret(context, &account) {
        Ok(secret) => secret,
        Err(error) => {
            return UiEvent::ContactsSynced {
                fetched: 0,
                error: Some(error.to_string()),
            }
        }
    };
    match hylmail_core::carddav::sync(
        &context.database,
        account_id,
        &settings.carddav_url,
        &username,
        &secret,
        &context.tls,
    )
    .await
    {
        Ok(report) => {
            tracing::debug!(
                fetched = report.fetched,
                removed = report.removed,
                "CardDAV sync finished"
            );
            UiEvent::ContactsSynced {
                fetched: report.fetched,
                error: None,
            }
        }
        Err(error) => UiEvent::ContactsSynced {
            fetched: 0,
            error: Some(error.to_string()),
        },
    }
}

/// Downloads one message again and writes the raw bytes as a `.eml` file.
///
/// The bytes are the ones the server hands out, so what lands on disk is exactly
/// the message - headers, body and attachments - and can be re-opened by any
/// mail client (`FR-DND-2`).
async fn save_eml(
    context: &WorkerContext,
    folder_id: FolderId,
    message_id: MessageId,
    path: &Path,
) -> Result<()> {
    let site = MessageSite::load(context, message_id, Some(folder_id))?;
    let raw = hylmail_core::body::fetch_raw(
        &site.account,
        &site.folder,
        &site.header,
        site.credential(),
        &context.tls,
    )
    .await?;
    std::fs::write(path, &raw).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error,
    })
}

/// Runs a server-side `UID SEARCH` on the enabled IMAP accounts (`FR-SEARCH-2`).
///
/// Each account's inbox folders are searched, up to `settings.server_search_limit`
/// hits each, and the headers are stored so the list and the local index see
/// them.  A protocol that cannot search the server (Exchange, GroupWise) is
/// skipped, and the caller keeps its local results.
async fn server_search(context: &WorkerContext, query: &str) -> UiEvent {
    let store = context.store();
    let accounts = match store.load_accounts() {
        Ok(accounts) => accounts,
        Err(error) => {
            return UiEvent::SearchResults {
                found: 0,
                error: Some(error.to_string()),
            }
        }
    };
    let limit = context
        .settings()
        .map(|settings| settings.server_search_limit as usize)
        .unwrap_or(500)
        .max(1);
    let mut found = 0usize;
    for account in accounts
        .accounts
        .iter()
        .filter(|account| account.enabled && account.kind == AccountKind::Imap)
    {
        let Ok(secret) = incoming_secret(context, account) else {
            continue;
        };
        let folders = match context
            .database
            .read(|connection| db_folders::list_folders(connection, account.id))
        {
            Ok(folders) => folders,
            Err(_) => continue,
        };
        let inboxes: Vec<Folder> = folders
            .into_iter()
            .filter(|folder| folder.kind == FolderKind::Inbox)
            .collect();
        if inboxes.is_empty() {
            continue;
        }
        let config = hylmail_core::imap::connection::ImapConfig::new(
            account.incoming.clone(),
            account.username.clone(),
            account.auth,
            context.tls.clone(),
        );
        let mut session = match hylmail_core::imap::connection::ImapSession::login(
            &config,
            credential_for(account, &secret),
        )
        .await
        {
            Ok(session) => session,
            Err(_) => continue,
        };
        for folder in &inboxes {
            match hylmail_core::imap::synchronise::search_folder(&mut session, folder, query, limit)
                .await
            {
                Ok(mut headers) => {
                    found += headers.len();
                    let _ = context.database.write(|connection| {
                        for header in &mut headers {
                            db_messages::upsert_message(connection, header)?;
                        }
                        Ok(())
                    });
                }
                Err(error) => tracing::debug!(
                    account = %account.id,
                    %error,
                    "the server search failed for a folder"
                ),
            }
        }
        session.logout().await;
    }
    UiEvent::SearchResults { found, error: None }
}

/// Deletes one message on the server and drops the local copy.
///
/// The server is the source of truth: the local row only goes once the request
/// succeeded, so a failed delete cannot make a message vanish from the list
/// while it still exists in the mailbox.
async fn delete_message(
    context: &WorkerContext,
    folder_id: FolderId,
    message_id: MessageId,
    permanent: bool,
) -> Result<DeleteOutcome> {
    let site = MessageSite::load(context, message_id, Some(folder_id))?;
    let trash = account_trash(context, &site.account)?;
    if trash.is_none() {
        // No special-use trash folder is known (the server may not have one),
        // so the message is removed for good.  Worth a log line: the status bar
        // then says "gelöscht" instead of "in den Papierkorb verschoben".
        tracing::debug!(account = %site.account.id, "no trash folder known");
    }
    let mode = if permanent {
        DeleteMode::Permanently
    } else {
        DeleteMode::ToTrash
    };
    let outcome = hylmail_core::delete::delete(
        &site.account,
        &site.folder,
        trash.as_ref(),
        &site.header,
        mode,
        site.credential(),
        &context.tls,
    )
    .await?;
    context
        .database
        .write(|connection| db_messages::delete_message(connection, message_id))?;
    Ok(outcome)
}

/// The trash folder of one account, when a synchronisation has found it.
fn account_trash(context: &WorkerContext, account: &Account) -> Result<Option<Folder>> {
    let folders = context
        .database
        .read(|connection| db_folders::list_folders(connection, account.id))?;
    Ok(folders
        .into_iter()
        .find(|folder| folder.kind == FolderKind::Trash))
}

/// Moves a message into another folder and relocates the local row.
///
/// The row is only touched after the server confirmed, so a failed move never
/// makes a message disappear from the list it is still in.  Exchange hands out
/// a new item id for a moved item, so it is stored as well.
async fn move_message(
    context: &WorkerContext,
    folder_id: FolderId,
    message_id: MessageId,
    target_folder_id: FolderId,
) -> Result<Folder> {
    let site = MessageSite::load(context, message_id, Some(folder_id))?;
    let target = context
        .database
        .read(|connection| db_folders::get_folder(connection, target_folder_id))?
        .ok_or_else(|| Error::NotFound {
            entity: "folder",
            id: target_folder_id.to_string(),
        })?;
    let outcome = hylmail_core::relocate::move_to(
        &site.account,
        &site.folder,
        &target,
        &site.header,
        site.credential(),
        &context.tls,
    )
    .await?;
    context.database.write(|connection| {
        db_messages::set_folder(connection, message_id, target_folder_id)?;
        if let Some(server_id) = outcome.new_server_id.as_deref() {
            db_messages::set_server_id(connection, message_id, server_id)?;
        }
        Ok(())
    })?;
    Ok(target)
}

/// Pushes the current flag state of one message to the server.
async fn push_flags(context: &WorkerContext, message_id: MessageId) -> Result<()> {
    let site = MessageSite::load(context, message_id, None)?;
    let flags = site.header.flags;

    hylmail_core::flags::push(
        &site.account,
        &site.folder,
        &site.header,
        flags,
        site.credential(),
        &context.tls,
    )
    .await
}

/// Loads the stored secret a protocol login needs.
fn incoming_secret(context: &WorkerContext, account: &Account) -> Result<secrecy::SecretString> {
    let kind = credential_kind(account);
    secrets::load_secret(context.secrets.as_ref(), account.id, kind)?
        .map(secrets::secret)
        .ok_or_else(|| {
            Error::config(format!(
                "für dieses Konto ist kein {} gespeichert",
                kind.label()
            ))
        })
}

/// The vault slot a login reads from.
fn credential_kind(account: &Account) -> CredentialKind {
    match account.auth {
        AuthKind::OAuth2 => CredentialKind::OAuthAccessToken,
        _ if account.kind == hylmail_core::models::AccountKind::GroupWiseProxy => {
            CredentialKind::ProxyPassword
        }
        _ => CredentialKind::IncomingPassword,
    }
}

/// Borrows a credential in the shape the transport expects.
fn credential_for<'a>(account: &Account, secret: &'a secrecy::SecretString) -> Credential<'a> {
    if account.auth == AuthKind::OAuth2 {
        Credential::OAuth2 {
            user: account.username.clone(),
            token: secret,
        }
    } else {
        Credential::Password(secret)
    }
}

/// Retries the persisted outbox once, in the background (`FR-OUTBOX-2`).
///
/// Unlike [`submit`], nothing is enqueued here: the function exists so that a
/// message queued while offline is delivered on the next tick.  It returns an
/// event only when something actually left the machine, so an idle tick is
/// silent.
async fn drain_outbox(context: &WorkerContext) -> Option<UiEvent> {
    let store = context.store();
    let mut document = match store.load_outbox() {
        Ok(document) => document,
        Err(error) => {
            return Some(UiEvent::Error(format!(
                "Der Postausgang konnte nicht gelesen werden: {error}"
            )))
        }
    };
    if document.pending() == 0 {
        return None;
    }
    let settings = match context.settings() {
        Ok(settings) => settings,
        Err(error) => return Some(UiEvent::Error(error.to_string())),
    };
    let accounts = match store.load_accounts() {
        Ok(accounts) => accounts,
        Err(error) => return Some(UiEvent::Error(error.to_string())),
    };
    let submitter = LiveSubmitter {
        accounts: &accounts,
        secrets: context.secrets.as_ref(),
    };
    let report = outbox::drain(
        &mut document,
        &submitter,
        |item| match accounts
            .accounts
            .iter()
            .find(|account| account.id == item.draft.account_id)
        {
            Some(account) => context_for(item, account, &settings, None),
            None => ComposeContext::default(),
        },
        utc_now(),
        Some(8),
    )
    .await;
    if let Err(error) = store.save_outbox(&document) {
        tracing::warn!(%error, "the outbox could not be written");
    }
    if report.sent == 0 {
        return None;
    }
    Some(UiEvent::OutboxSent { count: report.sent })
}

/// Composes, queues and submits one message.
async fn submit(
    context: &WorkerContext,
    account_id: AccountId,
    draft: Draft,
    signature: Option<Signature>,
) -> UiEvent {
    let account = match context.account(account_id) {
        Ok(account) => account,
        Err(error) => {
            return UiEvent::MailSent {
                detail: error.to_string(),
                ok: false,
            }
        }
    };
    let settings = match context.settings() {
        Ok(settings) => settings,
        Err(error) => {
            return UiEvent::MailSent {
                detail: error.to_string(),
                ok: false,
            }
        }
    };
    let store = context.store();
    let mut document = match store.load_outbox() {
        Ok(document) => document,
        Err(error) => {
            return UiEvent::MailSent {
                detail: error.to_string(),
                ok: false,
            }
        }
    };
    let id = outbox::enqueue(&mut document, draft);
    if let Err(error) = store.save_outbox(&document) {
        return UiEvent::MailSent {
            detail: error.to_string(),
            ok: false,
        };
    }

    let accounts = match store.load_accounts() {
        Ok(accounts) => accounts,
        Err(error) => {
            return UiEvent::MailSent {
                detail: error.to_string(),
                ok: false,
            }
        }
    };
    let submitter = LiveSubmitter {
        accounts: &accounts,
        secrets: context.secrets.as_ref(),
    };
    let settings_for_context = settings.clone();
    let report = outbox::drain(
        &mut document,
        &submitter,
        |item| {
            // The signature the composer showed wins over the one on disk: the
            // editor may have been changed without saving the settings yet.
            let live = if item.id == id {
                signature.as_ref()
            } else {
                None
            };
            context_for(item, &account, &settings_for_context, live)
        },
        utc_now(),
        Some(8),
    )
    .await;
    if let Err(error) = store.save_outbox(&document) {
        tracing::warn!(%error, "the outbox could not be written");
    }

    let outcome = document.items.iter().find(|item| item.id == id);
    match outcome.map(|item| item.state) {
        Some(OutboxState::Sent) => UiEvent::MailSent {
            detail: format!(
                "Nachricht an {} übergeben ({} weitere versendet)",
                joined_recipients(&document, id),
                report.sent.saturating_sub(1)
            ),
            ok: true,
        },
        Some(OutboxState::Failed) => UiEvent::MailSent {
            detail: outcome
                .and_then(|item| item.last_error.clone())
                .unwrap_or_else(|| "Der Server hat die Nachricht abgelehnt".to_string()),
            ok: false,
        },
        Some(OutboxState::Deferred) => UiEvent::MailSent {
            detail: "Der Versand wurde zurückgestellt und später erneut versucht".to_string(),
            ok: false,
        },
        _ => UiEvent::MailSent {
            detail: "Der Versand ist fehlgeschlagen".to_string(),
            ok: false,
        },
    }
}

/// The recipient list of a queued item, for the status bar.
fn joined_recipients(
    document: &hylmail_core::store::OutboxDocument,
    id: hylmail_core::models::OutboxId,
) -> String {
    document
        .items
        .iter()
        .find(|item| item.id == id)
        .map(|item| {
            item.draft
                .to
                .iter()
                .map(MailAddress::to_header_value)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// The composer context for one queued item.
fn context_for(
    item: &OutboxItem,
    account: &Account,
    settings: &Settings,
    live_signature: Option<&Signature>,
) -> ComposeContext {
    let mut context = ComposeContext::from_address(MailAddress::with_name(
        account.email.clone(),
        account.display_name.clone(),
    ));
    context.date = Some(utc_now());
    context.sender = Some(account.email.clone());
    context.signature = live_signature.cloned().or_else(|| {
        item.draft
            .signature_id
            .and_then(|id| {
                settings
                    .signatures
                    .iter()
                    .find(|signature| signature.id == id)
            })
            .cloned()
    });
    context
}

/// Submits through [`hylmail_core::send`], using the stored outgoing secret.
///
/// The protocol is the account's, not the outbox's: an IMAP account goes over
/// SMTP, an Exchange account through EWS.
struct LiveSubmitter<'a> {
    accounts: &'a hylmail_core::store::AccountsDocument,
    secrets: &'a dyn secrets::SecretStore,
}

impl Submitter for LiveSubmitter<'_> {
    async fn submit(&self, item: &OutboxItem, composed: &ComposedMessage) -> Delivery {
        let Some(account) = self
            .accounts
            .accounts
            .iter()
            .find(|account| account.id == item.draft.account_id)
        else {
            return Delivery::Permanent {
                reason: "Das Konto der Nachricht ist nicht mehr eingerichtet.".to_string(),
            };
        };
        let kind = match account.auth {
            AuthKind::OAuth2 => CredentialKind::OAuthAccessToken,
            _ => CredentialKind::OutgoingPassword,
        };
        let stored = match secrets::load_secret(self.secrets, account.id, kind) {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                return Delivery::Permanent {
                    reason: format!("Für dieses Konto ist kein {} gespeichert.", kind.label()),
                };
            }
            Err(error) => return Delivery::from_error(&error),
        };

        let stored = secrets::secret(stored);
        let credential = if account.auth == AuthKind::OAuth2 {
            Credential::OAuth2 {
                user: account.username.clone(),
                token: &stored,
            }
        } else {
            Credential::Password(&stored)
        };

        let receipt = match send::submit(account, composed, credential, &TlsPolicy::strict()).await
        {
            Ok(receipt) => receipt,
            Err(error) => return Delivery::from_error(&error),
        };
        if receipt.accepted {
            Delivery::Accepted {
                response: receipt.describe(),
                sent_copy: receipt.sent_copy,
            }
        } else {
            Delivery::Permanent {
                reason: format!(
                    "Der Server hat die Nachricht abgelehnt: {}",
                    receipt.response
                ),
            }
        }
    }
}

/// Composes a draft without touching the network.
///
#[cfg(test)]
mod tests {
    use super::*;
    use hylmail_core::models::{AccountKind, Endpoint};

    fn account(kind: AccountKind, auth: AuthKind) -> Account {
        Account {
            id: AccountId::new(),
            display_name: "Test".to_string(),
            email: "anna@example.org".to_string(),
            kind,
            auth,
            username: "anna".to_string(),
            incoming: Endpoint::imaps("imap.example.org"),
            outgoing: Endpoint::smtps("smtp.example.org"),
            proxy: None,
            oauth: None,
            enabled: true,
            created_at: utc_now(),
        }
    }

    #[test]
    fn the_vault_slot_follows_the_authentication_kind() {
        assert_eq!(
            credential_kind(&account(AccountKind::Imap, AuthKind::OAuth2)),
            CredentialKind::OAuthAccessToken
        );
        assert_eq!(
            credential_kind(&account(AccountKind::GroupWiseProxy, AuthKind::Password)),
            CredentialKind::ProxyPassword
        );
        assert_eq!(
            credential_kind(&account(AccountKind::Exchange, AuthKind::Ntlm)),
            CredentialKind::IncomingPassword
        );
    }

    #[test]
    fn groupwise_accounts_do_not_share_the_incoming_slot() {
        let groupwise = account(AccountKind::GroupWiseProxy, AuthKind::Password);
        let imap = account(AccountKind::Imap, AuthKind::Password);
        assert_ne!(credential_kind(&groupwise), credential_kind(&imap));
    }

    #[test]
    fn a_composer_context_carries_the_signature_of_the_draft() {
        let account = account(AccountKind::Imap, AuthKind::Password);
        let mut settings = Settings::default();
        let mut signature = Signature::new(account.id, "Standard");
        signature.plain = "Viele Grüße".to_string();
        let signature_id = signature.id;
        settings.signatures.push(signature);

        let mut draft = Draft::new(account.id);
        draft.signature_id = Some(signature_id);
        let item = OutboxItem::new(draft);
        let context = context_for(&item, &account, &settings, None);
        assert_eq!(
            context.signature.as_ref().map(|signature| signature.id),
            Some(signature_id)
        );
        assert_eq!(context.sender.as_deref(), Some("anna@example.org"));
    }

    #[test]
    fn the_live_signature_overrides_the_stored_one() {
        let account = account(AccountKind::Imap, AuthKind::Password);
        let mut stored = Signature::new(account.id, "Gespeichert");
        stored.plain = "alt".to_string();
        let stored_id = stored.id;
        let mut settings = Settings::default();
        settings.signatures.push(stored);

        let mut draft = Draft::new(account.id);
        draft.signature_id = Some(stored_id);
        let item = OutboxItem::new(draft);

        let mut live = Signature::new(account.id, "Live");
        live.plain = "neu".to_string();
        let context = context_for(&item, &account, &settings, Some(&live));
        assert_eq!(
            context.signature.as_ref().map(|signature| signature.id),
            Some(live.id)
        );
    }

    #[test]
    fn a_message_without_a_signature_gets_none() {
        let account = account(AccountKind::Imap, AuthKind::Password);
        let item = OutboxItem::new(Draft::new(account.id));
        let context = context_for(&item, &account, &Settings::default(), None);
        assert!(context.signature.is_none());
    }

    #[test]
    fn an_unset_composer_cannot_be_sent() {
        let account = account(AccountKind::Imap, AuthKind::Password);
        let draft = Draft::new(account.id);
        assert!(!hylmail_core::compose::is_sendable(&draft));
        assert!(hylmail_core::compose::send_blocker(&draft).is_some());
    }
}
