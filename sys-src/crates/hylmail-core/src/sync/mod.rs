//! The background synchronisation engine.
//!
//! One [`SyncEngine`] drives every configured account.  Each account gets its
//! own tokio task, its own IMAP connection and its own retry schedule, so a
//! server that is down cannot delay the accounts that are up.
//!
//! ```text
//!   UI / CLI ──EngineCommand──▶ supervisor ──AccountCommand──▶ account task
//!   UI / CLI ◀──EngineEvent───  events  ◀──── SyncProgress ────┘
//! ```
//!
//! The account task is a plain loop:
//!
//! 1. wait for the retry delay,
//! 2. connect and authenticate,
//! 3. discover the folders ([`crate::imap::mailbox`]) and store them,
//! 4. synchronise the folders that need it,
//! 5. `IDLE` when the server supports it, otherwise poll with `NOOP`,
//! 6. on failure: emit an event and go back to step 1 with a longer delay.
//!
//! Nothing in here touches the UI, and every decision the loop makes is a pure
//! function ([`AccountState`], [`next_poll_interval`]) so the interesting parts
//! can be tested without a server.

pub mod backoff;
pub mod reconcile;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::db::{folders, Database};
use crate::error::{Error, Result};
use crate::imap::connection::{Credential, ImapConfig, ImapSession};
use crate::imap::synchronise::{FolderSyncReport, SyncOptions, SyncProgress};
use crate::models::account::{Account, AccountKind, AuthKind};
use crate::models::folder::Folder;
use crate::models::ids::AccountId;
use crate::models::settings::Settings;
use crate::net::TlsPolicy;
use crate::oauth;
use crate::paths::AppPaths;
use crate::secrets::{CredentialKey, CredentialKind, SecretStore};
use crate::store::ConfigStore;

pub use backoff::Backoff;

/// Default polling interval for servers without `IDLE`.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3 * 60);

/// How long a folder synchronisation may take before the engine gives up on
/// that folder for this round.
pub const FOLDER_BUDGET: Duration = Duration::from_secs(10 * 60);

/// A command sent *to* the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCommand {
    /// Synchronise one account now, ignoring the retry delay.
    SyncNow {
        /// Account to synchronise.
        account_id: AccountId,
    },
    /// Synchronise every enabled account now.
    SyncAll,
    /// Re-read the folder list of an account (after creating a folder).
    RefreshFolders {
        /// Account whose folders changed.
        account_id: AccountId,
    },
    /// Re-read the account list (an account was added, removed or edited).
    ///
    /// The engine keeps one task per enabled account and reads that list only
    /// when it starts, so a freshly created account would never be contacted
    /// without this command.
    AccountsChanged,
    /// Re-read the settings (the user saved them).
    ///
    /// The account loops clone the settings at start-up, so this restarts them
    /// with the new interval and caps.
    SettingsChanged,
    /// Stop working on an account but keep the engine running.
    StopAccount {
        /// Account to stop.
        account_id: AccountId,
    },
    /// The machine lost or regained its network connection.
    NetworkAvailable(bool),
    /// Stop everything and wait for the tasks to finish.
    Shutdown,
}

/// Something the engine reports *to* the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// A connection was established and authenticated.
    Connected {
        /// Account that connected.
        account_id: AccountId,
        /// Host that was connected to.
        host: String,
        /// `true` when the connection is encrypted.
        encrypted: bool,
        /// Negotiated TLS version, e.g. `TLSv1.3`.
        tls_version: Option<String>,
    },
    /// The folder list was refreshed.
    FoldersDiscovered {
        /// Account whose folders were read.
        account_id: AccountId,
        /// Number of selectable folders.
        count: usize,
    },
    /// A folder was synchronised.
    FolderSynced {
        /// Account the folder belongs to.
        account_id: AccountId,
        /// What the pass achieved.
        report: Box<FolderSyncReport>,
    },
    /// New messages arrived in a folder.
    NewMail {
        /// Account that received mail.
        account_id: AccountId,
        /// Number of new messages in this batch.
        count: usize,
    },
    /// The account is idling, waiting to be told about new mail.
    Idling {
        /// Account that is idling.
        account_id: AccountId,
    },
    /// The connection went away.
    Disconnected {
        /// Account that lost the connection.
        account_id: AccountId,
        /// Human readable reason, already redacted.
        reason: String,
        /// `true` when a retry is scheduled.
        retrying: bool,
    },
    /// The account was stopped (by the user or because it is disabled).
    Stopped {
        /// Account that stopped.
        account_id: AccountId,
    },
    /// The engine could not carry out a request, for example read the account
    /// list that [`EngineCommand::AccountsChanged`] asked it to re-read.
    Error {
        /// Human readable reason, already redacted.
        message: String,
    },
    /// The engine finished; no further events will follow.
    Finished,
}

/// One running account task: its id, the configuration it was started with, the
/// gate of its event sink, the channel that reaches it and its handle.
///
/// The configuration is kept so [`reconcile_account_tasks`] can tell an edited
/// account from an unchanged one and restart only what actually changed.
type AccountTask = (
    AccountId,
    Account,
    EventSink,
    mpsc::UnboundedSender<AccountCommand>,
    JoinHandle<()>,
);

/// The channel one account task reports through, with a gate in front of it.
///
/// When an account is replaced (it was edited and restarted, still under the
/// same id) the outgoing task's sink is closed before it is asked to stop, so a
/// late `Stopped` from it cannot overtake the `Connected` of its replacement and
/// blank the connection indicator.
#[derive(Clone)]
struct EventSink {
    sender: mpsc::UnboundedSender<EngineEvent>,
    active: Arc<AtomicBool>,
}

impl EventSink {
    /// A sink that forwards every event.
    fn new(sender: mpsc::UnboundedSender<EngineEvent>) -> Self {
        Self {
            sender,
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Sends the event unless the task was superseded.
    fn send(&self, event: EngineEvent) {
        if self.active.load(Ordering::Relaxed) {
            let _ = self.sender.send(event);
        }
    }

    /// Closes the sink; further events from this task are dropped.
    fn close(&self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

/// A command sent to one account task.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AccountCommand {
    /// Synchronise immediately.
    SyncNow,
    /// Re-read the folder list and synchronise immediately.
    RefreshFolders,
    /// Stop this account but let the task end cleanly.
    Stop,
}

/// Where an account is in its lifecycle.
///
/// The states exist mostly to make the loop's decisions reviewable: the
/// transitions are tested, the i/o around them is not interesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountState {
    /// Not started yet, or stopped by the user.
    Idle,
    /// Waiting out a retry delay.
    Waiting,
    /// Connected, folders being discovered or synchronised.
    Syncing,
    /// Connected and idling (or polling).
    Watching,
    /// Stopped for good: disabled account, or the engine is shutting down.
    Stopped,
}

impl AccountState {
    /// The state after a successful connection.
    #[must_use]
    pub const fn after_connect(self) -> Self {
        Self::Syncing
    }

    /// The state after a synchronisation round finished.
    #[must_use]
    pub const fn after_sync(self) -> Self {
        Self::Watching
    }

    /// The state after a failure.
    #[must_use]
    pub const fn after_failure(self) -> Self {
        Self::Waiting
    }

    /// `true` when the state permits a new connection attempt.
    #[must_use]
    pub const fn may_connect(self) -> bool {
        matches!(self, Self::Idle | Self::Waiting)
    }
}

/// How long to wait between two polls of a folder.
///
/// `IDLE` capable servers are not polled at all; the interval from the settings
/// is the *user visible* "check for new mail every N minutes" and `0` disables
/// the timer, which is the documented behaviour of
/// [`Settings::sync_interval_minutes`].
#[must_use]
pub fn next_poll_interval(settings: &Settings, idle_supported: bool) -> Option<Duration> {
    if idle_supported {
        // `IDLE` already delivers notifications; the timer only exists as a
        // safety net for servers that lie about it.
        return Some(Duration::from_secs(30 * 60));
    }
    if settings.sync_interval_minutes == 0 {
        return None;
    }
    Some(Duration::from_secs(
        u64::from(settings.sync_interval_minutes) * 60,
    ))
}

/// Everything the engine needs from the surrounding application.
pub struct EngineConfig {
    /// Paths, used to re-read the account list from disk.
    pub paths: AppPaths,
    /// The open database.
    pub database: Arc<Database>,
    /// The credential vault.
    pub secrets: Arc<dyn SecretStore>,
    /// Settings snapshot taken at startup.
    pub settings: Settings,
    /// Certificate policy for every account.
    pub tls: TlsPolicy,
    /// How much to fetch per pass.
    pub sync: SyncOptions,
    /// Retry schedule.
    pub backoff: Backoff,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("database", &self.database.path())
            .field("sync", &self.sync)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl EngineConfig {
    /// Builds a configuration with the defaults the application uses.
    #[must_use]
    pub fn new(
        paths: AppPaths,
        database: Arc<Database>,
        secrets: Arc<dyn SecretStore>,
        settings: Settings,
    ) -> Self {
        Self {
            paths,
            database,
            secrets,
            settings,
            tls: TlsPolicy::strict(),
            sync: SyncOptions::default(),
            backoff: Backoff::default(),
        }
    }
}

/// The handle the UI uses to talk to the engine.
pub struct EngineHandle {
    commands: mpsc::UnboundedSender<EngineCommand>,
    events: mpsc::UnboundedReceiver<EngineEvent>,
    shutdown: watch::Sender<bool>,
}

impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandle").finish_non_exhaustive()
    }
}

impl EngineHandle {
    /// Queues a command.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when the engine already stopped.
    pub fn send(&self, command: EngineCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| Error::Unsupported("the synchronisation engine has stopped".to_string()))
    }

    /// Asks every enabled account to synchronise now.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when the engine already stopped.
    pub fn sync_all(&self) -> Result<()> {
        self.send(EngineCommand::SyncAll)
    }

    /// Tells the engine that the account list changed.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when the engine already stopped.
    pub fn accounts_changed(&self) -> Result<()> {
        self.send(EngineCommand::AccountsChanged)
    }

    /// Tells the engine that the settings were saved.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when the engine already stopped.
    pub fn settings_changed(&self) -> Result<()> {
        self.send(EngineCommand::SettingsChanged)
    }

    /// Receives the next event, or `None` when the engine finished.
    pub async fn recv(&mut self) -> Option<EngineEvent> {
        self.events.recv().await
    }

    /// Receives the next event without waiting.
    pub fn try_recv(&mut self) -> Option<EngineEvent> {
        self.events.try_recv().ok()
    }

    /// Asks the engine to stop and returns immediately.
    ///
    /// # Errors
    ///
    /// Never fails; the shutdown signal is a `watch` channel that keeps its
    /// value even after every receiver is gone.
    pub fn shutdown(&self) -> Result<()> {
        let _ = self.commands.send(EngineCommand::Shutdown);
        let _ = self.shutdown.send(true);
        Ok(())
    }
}

/// The background engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncEngine;

impl SyncEngine {
    /// Starts one task per enabled account.
    ///
    /// The returned [`JoinHandle`] resolves when every account task has ended.
    #[must_use]
    pub fn spawn(config: EngineConfig) -> (EngineHandle, JoinHandle<()>) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = EngineHandle {
            commands: command_tx,
            events: event_rx,
            shutdown: shutdown_tx,
        };

        let task = tokio::spawn(supervise(config, command_rx, event_tx, shutdown_rx));
        (handle, task)
    }
}

/// Reads the account list and keeps the per-account tasks alive.
async fn supervise(
    mut config: EngineConfig,
    mut commands: mpsc::UnboundedReceiver<EngineCommand>,
    events: mpsc::UnboundedSender<EngineEvent>,
    shutdown: watch::Receiver<bool>,
) {
    let mut tasks = match spawn_account_tasks(&config, &events, &shutdown) {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::error!(%error, "cannot read the account list");
            let _ = events.send(EngineEvent::Finished);
            return;
        }
    };

    while let Some(command) = commands.recv().await {
        match command {
            EngineCommand::Shutdown => break,
            EngineCommand::NetworkAvailable(available) => {
                if !available {
                    tracing::info!("the network is gone; the engine keeps retrying slowly");
                }
            }
            EngineCommand::SyncNow { account_id } => {
                send_to(&tasks, account_id, AccountCommand::SyncNow);
            }
            EngineCommand::SyncAll => {
                for (_, _, _, tx, _) in &tasks {
                    let _ = tx.send(AccountCommand::SyncNow);
                }
            }
            EngineCommand::RefreshFolders { account_id } => {
                send_to(&tasks, account_id, AccountCommand::RefreshFolders);
            }
            EngineCommand::AccountsChanged => match read_accounts(&config) {
                Ok(fresh) => {
                    reconcile_account_tasks(&config, &events, &shutdown, &mut tasks, &fresh);
                }
                Err(error) => {
                    tracing::error!(%error, "cannot read the account list");
                    let _ = events.send(EngineEvent::Error {
                        message: format!("Die Kontenliste konnte nicht gelesen werden: {error}"),
                    });
                }
            },
            EngineCommand::SettingsChanged => match load_settings(&config) {
                Ok(settings) => {
                    config.settings = settings;
                    // The account loops clone the settings at start-up, so only a
                    // restart makes a new interval or cap take effect.
                    restart_all_tasks(&config, &events, &shutdown, &mut tasks);
                }
                Err(error) => {
                    tracing::error!(%error, "cannot read the settings");
                    let _ = events.send(EngineEvent::Error {
                        message: format!("Die Einstellungen konnten nicht gelesen werden: {error}"),
                    });
                }
            },
            EngineCommand::StopAccount { account_id } => {
                send_to(&tasks, account_id, AccountCommand::Stop);
            }
        }
    }

    for (_, _, _, tx, task) in tasks {
        let _ = tx.send(AccountCommand::Stop);
        let _ = task.await;
    }
    let _ = events.send(EngineEvent::Finished);
}

/// Reads the stored settings.
fn load_settings(config: &EngineConfig) -> Result<Settings> {
    ConfigStore::new(config.paths.clone()).load_settings()
}

/// Stops every task and starts it again from the (possibly new) configuration.
///
/// Used when the settings changed: the account loops hold a copy of the settings
/// they were started with, so a restart is what makes a new interval or cap
/// apply without restarting the whole application.
fn restart_all_tasks(
    config: &EngineConfig,
    events: &mpsc::UnboundedSender<EngineEvent>,
    shutdown: &watch::Receiver<bool>,
    tasks: &mut Vec<AccountTask>,
) {
    for (_, _, sink, tx, _) in tasks.drain(..) {
        // The same ids come straight back: drop the outgoing tasks' events so
        // they cannot race the replacements.
        sink.close();
        let _ = tx.send(AccountCommand::Stop);
    }
    match spawn_account_tasks(config, events, shutdown) {
        Ok(fresh) => *tasks = fresh,
        Err(error) => tracing::error!(%error, "cannot read the account list"),
    }
}

/// Reads the stored account list.
fn read_accounts(config: &EngineConfig) -> Result<Vec<Account>> {
    Ok(ConfigStore::new(config.paths.clone())
        .load_accounts()?
        .accounts)
}

/// Spawns one task per enabled account of the stored account list.
///
/// The account list is read here so the engine can pick up accounts that were
/// added after start-up ([`EngineCommand::AccountsChanged`]).
fn spawn_account_tasks(
    config: &EngineConfig,
    events: &mpsc::UnboundedSender<EngineEvent>,
    shutdown: &watch::Receiver<bool>,
) -> Result<Vec<AccountTask>> {
    let accounts = read_accounts(config)?;
    Ok(accounts
        .iter()
        .filter(|account| account.enabled)
        .map(|account| spawn_account_task(config, account, events, shutdown))
        .collect())
}

/// Brings the running tasks in line with a freshly read account list.
///
/// A newly created account is started here, which is what makes it synchronise
/// right after it was saved instead of after the next restart.  Accounts that
/// were removed, disabled or edited are stopped and, when they still exist,
/// started again from their new configuration; untouched accounts keep running.
fn reconcile_account_tasks(
    config: &EngineConfig,
    events: &mpsc::UnboundedSender<EngineEvent>,
    shutdown: &watch::Receiver<bool>,
    tasks: &mut Vec<AccountTask>,
    accounts: &[Account],
) {
    tasks.retain(|(_, account, sink, tx, _)| {
        if task_still_current(account, accounts) {
            return true;
        }
        // A still-enabled account that changed is replaced by a fresh task under
        // the same id: close its sink so its late `Stopped` is dropped.  A
        // removed or disabled account keeps its sink, so its `Stopped` still
        // clears the connection state.
        if accounts
            .iter()
            .any(|fresh| fresh.id == account.id && fresh.enabled)
        {
            sink.close();
        }
        // `Stop` ends the task; dropping the handle detaches it.
        let _ = tx.send(AccountCommand::Stop);
        false
    });

    for account in accounts.iter().filter(|account| account.enabled) {
        if tasks.iter().all(|(id, _, _, _, _)| *id != account.id) {
            tasks.push(spawn_account_task(config, account, events, shutdown));
        }
    }
}

/// `true` when a running task may keep running against the fresh account list.
///
/// A task survives only while its account still exists, is still enabled and
/// was not edited; anything else is restarted from the new configuration.
fn task_still_current(running: &Account, accounts: &[Account]) -> bool {
    accounts
        .iter()
        .any(|account| account.id == running.id && account.enabled && account == running)
}

/// Spawns the task that owns one account.
fn spawn_account_task(
    config: &EngineConfig,
    account: &Account,
    events: &mpsc::UnboundedSender<EngineEvent>,
    shutdown: &watch::Receiver<bool>,
) -> AccountTask {
    let (tx, rx) = mpsc::unbounded_channel();
    let database = Arc::clone(&config.database);
    let secrets = Arc::clone(&config.secrets);
    let settings = config.settings.clone();
    let tls = config.tls.clone();
    let sync_options = config.sync;
    let backoff = config.backoff;
    let worker = account.clone();
    let sink = EventSink::new(events.clone());
    let stop = shutdown.clone();
    // Only the protocols HylMail can drive get a task; anything else is
    // reported instead of silently staying empty.
    let task = match account.kind {
        AccountKind::Imap => tokio::spawn(run_imap_account(
            database,
            secrets,
            settings,
            tls,
            sync_options,
            backoff,
            worker,
            rx,
            sink.clone(),
            stop,
        )),
        AccountKind::Exchange => tokio::spawn(run_exchange_account(
            database,
            secrets,
            settings,
            tls,
            sync_options,
            backoff,
            worker,
            rx,
            sink.clone(),
            stop,
        )),
        AccountKind::GroupWiseProxy => tokio::spawn(run_groupwise_account(
            database,
            secrets,
            settings,
            tls,
            sync_options,
            backoff,
            worker,
            rx,
            sink.clone(),
            stop,
        )),
    };
    (account.id, account.clone(), sink, tx, task)
}

/// Routes a command to one account task.
fn send_to(tasks: &[AccountTask], account_id: AccountId, command: AccountCommand) {
    for (id, _, _, tx, _) in tasks {
        if *id == account_id {
            let _ = tx.send(command.clone());
            return;
        }
    }
    tracing::debug!(%account_id, "no account task for this command");
}

/// The loop that owns one IMAP account.
#[allow(clippy::too_many_arguments)]
async fn run_imap_account(
    database: Arc<Database>,
    secrets: Arc<dyn SecretStore>,
    settings: Settings,
    tls: TlsPolicy,
    sync_options: SyncOptions,
    backoff: Backoff,
    account: Account,
    mut commands: mpsc::UnboundedReceiver<AccountCommand>,
    events: EventSink,
    mut shutdown: watch::Receiver<bool>,
) {
    let account_id = account.id;
    let mut state = AccountState::Idle;
    let mut attempt: u32 = 0;
    let mut force_sync = false;

    // The state machine drives the loop: `Waiting` means the previous round
    // failed, so the retry delay applies; every other state continues straight
    // away.  Nothing else in the engine decides when to back off.
    'outer: while state != AccountState::Stopped {
        if *shutdown.borrow() {
            break;
        }

        // -- wait out the retry delay, but stay responsive -------------------
        if state == AccountState::Waiting {
            let delay = backoff.jittered(attempt, backoff::entropy());
            let interrupted = tokio::select! {
                () = tokio::time::sleep(delay) => false,
                changed = shutdown.changed() => { let _ = changed; true }
                command = commands.recv() => match command {
                    Some(AccountCommand::Stop) | None => true,
                    Some(AccountCommand::SyncNow | AccountCommand::RefreshFolders) => false,
                },
            };
            if interrupted {
                break;
            }
            // The user asked for a round, so the schedule is satisfied.
            force_sync = true;
        }
        state = AccountState::Idle;

        // -- connect --------------------------------------------------------
        let secret = match account_secret(&*secrets, &account, &tls).await {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: format!(
                            "no password or token stored for {}; open the account settings",
                            account.from_address()
                        ),
                        retrying: false,
                    },
                );
                events.send(EngineEvent::Stopped { account_id });
                break;
            }
            Err(error) => {
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: error.to_string(),
                        retrying: true,
                    },
                );
                state = AccountState::Waiting;
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        let credential = match account.auth {
            AuthKind::OAuth2 => Credential::OAuth2 {
                user: account.username.clone(),
                token: &secret,
            },
            _ => Credential::Password(&secret),
        };

        let config = ImapConfig::new(
            account.incoming.clone(),
            account.username.clone(),
            account.auth,
            tls.clone(),
        );
        let session = tokio::select! {
            result = ImapSession::login(&config, credential) => result,
            changed = shutdown.changed() => { let _ = changed; break 'outer; }
        };

        let mut session = match session {
            Ok(session) => session,
            Err(error) => {
                state = state.after_failure();
                let transient = error.is_transient();
                attempt = attempt.saturating_add(1);
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: error.to_string(),
                        retrying: transient,
                    },
                );
                if !transient {
                    // A rejected password or a missing capability does not get
                    // better by waiting; the user has to fix the account.
                    break;
                }
                continue;
            }
        };

        attempt = 0;
        state = state.after_connect();
        let outcome = session.outcome();
        events_send(
            &events,
            EngineEvent::Connected {
                account_id,
                host: outcome.host.clone(),
                encrypted: outcome.encrypted,
                tls_version: outcome.tls_version.clone(),
            },
        );

        // -- discover folders ----------------------------------------------
        match session.list_folders(account_id).await {
            Ok(discovered) => {
                let count = discovered
                    .iter()
                    .filter(|folder| folder.is_selectable())
                    .count();
                if let Err(error) = store_folders(&database, &discovered) {
                    tracing::warn!(account = %account.email, %error, "cannot store the folder list");
                }
                events_send(
                    &events,
                    EngineEvent::FoldersDiscovered { account_id, count },
                );
            }
            Err(error) => {
                tracing::warn!(account = %account.email, %error, "LIST failed");
            }
        }

        // -- synchronise ----------------------------------------------------
        let local = match database.read(|connection| folders::list_folders(connection, account_id))
        {
            Ok(local) => local,
            Err(error) => {
                tracing::warn!(%error, "cannot read the folder list");
                Vec::new()
            }
        };
        let mut progress = progress_events(events.clone(), account_id);
        let reports = crate::imap::synchronise::sync_folders(
            &mut session,
            &database,
            account_id,
            &local,
            sync_options,
            &mut progress,
        )
        .await;
        for result in reports {
            match result {
                Ok(report) if !report.is_noop() => events_send(
                    &events,
                    EngineEvent::FolderSynced {
                        account_id,
                        report: Box::new(report),
                    },
                ),
                Ok(_) => {}
                // One broken folder must not stop the account, but it must not
                // disappear either: the reason belongs in the log.
                Err(error) => tracing::warn!(
                    account = %account.email,
                    %error,
                    "a folder could not be synchronised"
                ),
            }
        }

        // -- watch for new mail ---------------------------------------------
        let idle_supported = session.capabilities().has_idle();
        state = state.after_sync();
        tracing::debug!(
            account = %account.from_address(),
            state = ?state,
            idle = idle_supported,
            "the account is up to date"
        );
        if idle_supported {
            events_send(&events, EngineEvent::Idling { account_id });
        }
        let (outcome, returned) = watch_loop(
            session,
            &mut commands,
            &mut shutdown,
            next_poll_interval(&settings, idle_supported),
            &mut force_sync,
        )
        .await;
        let Some(session) = returned else {
            // The connection is unusable; reconnect from scratch.
            state = AccountState::Idle;
            continue 'outer;
        };
        session.logout().await;
        if outcome == WatchOutcome::Stop || *shutdown.borrow() {
            break;
        }
        // A `Refresh` decision: the folder list may have changed too, and a
        // fresh connection is the cheapest way to a clean state.
        state = AccountState::Idle;
    }

    events.send(EngineEvent::Stopped { account_id });
}

/// What the watch loop decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchOutcome {
    /// Something asked for a new synchronisation round.
    Refresh,
    /// The engine is shutting down.
    Stop,
}

/// Maps synchronisation progress onto engine events.
///
/// Both protocol drivers use it, so a new mail event means the same thing no
/// matter which server it came from.
fn progress_events(events: EventSink, account_id: AccountId) -> impl FnMut(SyncProgress) {
    move |update: SyncProgress| {
        if let SyncProgress::HeadersStored { added, .. } = update {
            if added > 0 {
                events.send(EngineEvent::NewMail {
                    account_id,
                    count: added,
                });
            }
        }
    }
}

/// Waits for the poll interval, a command or the shutdown signal.
///
/// `None` means "no automatic polling": only a command or the shutdown ends the
/// wait.
async fn wait_for_poll(
    poll: Option<Duration>,
    commands: &mut mpsc::UnboundedReceiver<AccountCommand>,
    shutdown: &mut watch::Receiver<bool>,
) -> WatchOutcome {
    let sleep = async {
        match poll {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        changed = shutdown.changed() => { let _ = changed; WatchOutcome::Stop }
        command = commands.recv() => match command {
            Some(AccountCommand::Stop) | None => WatchOutcome::Stop,
            Some(AccountCommand::SyncNow | AccountCommand::RefreshFolders) => WatchOutcome::Refresh,
        },
        () = sleep => WatchOutcome::Refresh,
    }
}

/// The loop that owns one Exchange (EWS) account.
///
/// Exchange has no `IDLE`, so this driver is a plain poll: connect, walk the
/// folder tree, run one synchronisation pass, then wait for the poll interval or
/// a command.  Backoff, events and shutdown behave exactly like the IMAP driver,
/// which is what lets the UI treat both kinds of account the same way.
#[allow(clippy::too_many_arguments)]
async fn run_exchange_account(
    database: Arc<Database>,
    secrets: Arc<dyn SecretStore>,
    settings: Settings,
    tls: TlsPolicy,
    sync_options: SyncOptions,
    backoff: Backoff,
    account: Account,
    mut commands: mpsc::UnboundedReceiver<AccountCommand>,
    events: EventSink,
    mut shutdown: watch::Receiver<bool>,
) {
    let account_id = account.id;
    let mut attempt: u32 = 0;
    let mut first_round = true;

    while !*shutdown.borrow() {
        if !first_round {
            // A failed round waits out the backoff, a successful one waits for
            // the poll interval; either way a command and the shutdown signal
            // interrupt the wait.
            let wait = if attempt > 0 {
                Some(backoff.jittered(attempt.saturating_sub(1), backoff::entropy()))
            } else {
                next_poll_interval(&settings, false)
            };
            if wait_for_poll(wait, &mut commands, &mut shutdown).await == WatchOutcome::Stop {
                break;
            }
        }
        first_round = false;
        if *shutdown.borrow() {
            break;
        }

        let secret = match account_secret(&*secrets, &account, &tls).await {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: format!(
                            "no password or token stored for {}; open the account settings",
                            account.from_address()
                        ),
                        retrying: false,
                    },
                );
                events.send(EngineEvent::Stopped { account_id });
                return;
            }
            Err(error) => {
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: error.to_string(),
                        retrying: true,
                    },
                );
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        let credential = match account.auth {
            AuthKind::OAuth2 => Credential::OAuth2 {
                user: account.username.clone(),
                token: &secret,
            },
            _ => Credential::Password(&secret),
        };
        let mut config = crate::ews::EwsConfig::new(
            account.incoming.clone(),
            account.username.clone(),
            account.auth,
        );
        config.policy = tls.clone();

        let login = tokio::select! {
            result = crate::ews::EwsClient::login(&config, credential) => result,
            changed = shutdown.changed() => { let _ = changed; break; }
        };
        let mut client = match login {
            Ok(client) => client,
            Err(error) => {
                // A rejected password does not get better by waiting; the user
                // has to fix the account.
                let transient = error.is_transient();
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: error.to_string(),
                        retrying: transient,
                    },
                );
                attempt = attempt.saturating_add(1);
                if !transient {
                    break;
                }
                continue;
            }
        };
        attempt = 0;
        let outcome = client.outcome();
        events_send(
            &events,
            EngineEvent::Connected {
                account_id,
                host: outcome.host.clone(),
                encrypted: outcome.encrypted,
                // EWS has no negotiated version to report; the SOAP schema
                // version travels in the outcome instead.
                tls_version: None,
            },
        );

        // -- discover the folder tree ---------------------------------------
        match client.list_folders(account_id).await {
            Ok(discovered) => {
                let count = discovered
                    .iter()
                    .filter(|folder| folder.is_selectable())
                    .count();
                if let Err(error) = store_folders(&database, &discovered) {
                    tracing::warn!(account = %account.email, %error, "cannot store the folder list");
                }
                events_send(
                    &events,
                    EngineEvent::FoldersDiscovered { account_id, count },
                );
            }
            Err(error) => {
                tracing::warn!(account = %account.email, %error, "FindFolder failed");
            }
        }

        // -- one synchronisation pass ---------------------------------------
        let local = match database.read(|connection| folders::list_folders(connection, account_id))
        {
            Ok(local) => local,
            Err(error) => {
                tracing::warn!(%error, "cannot read the folder list");
                Vec::new()
            }
        };
        let mut progress = progress_events(events.clone(), account_id);
        let reports = crate::ews::sync_folders(
            &mut client,
            &database,
            account_id,
            &local,
            sync_options,
            &mut progress,
        )
        .await;
        for result in reports {
            match result {
                Ok(report) if !report.is_noop() => events_send(
                    &events,
                    EngineEvent::FolderSynced {
                        account_id,
                        report: Box::new(report),
                    },
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    account = %account.email,
                    %error,
                    "a folder could not be synchronised"
                ),
            }
        }
        tracing::debug!(
            account = %account.from_address(),
            "the Exchange account is up to date"
        );
    }

    events.send(EngineEvent::Stopped { account_id });
}

/// The loop that owns one GroupWise account.
///
/// The proxy has no `IDLE` and closes idle sessions, so this driver polls like
/// the Exchange one; the difference is the protocol, not the schedule.
#[allow(clippy::too_many_arguments)]
async fn run_groupwise_account(
    database: Arc<Database>,
    secrets: Arc<dyn SecretStore>,
    settings: Settings,
    tls: TlsPolicy,
    sync_options: SyncOptions,
    backoff: Backoff,
    account: Account,
    mut commands: mpsc::UnboundedReceiver<AccountCommand>,
    events: EventSink,
    mut shutdown: watch::Receiver<bool>,
) {
    let account_id = account.id;
    let mut attempt: u32 = 0;
    let mut first_round = true;

    while !*shutdown.borrow() {
        if !first_round {
            let wait = if attempt > 0 {
                Some(backoff.jittered(attempt.saturating_sub(1), backoff::entropy()))
            } else {
                next_poll_interval(&settings, false)
            };
            if wait_for_poll(wait, &mut commands, &mut shutdown).await == WatchOutcome::Stop {
                break;
            }
        }
        first_round = false;
        if *shutdown.borrow() {
            break;
        }

        let Some(proxy) = account.proxy.clone() else {
            // `Account::validate` refuses this, so it can only come from a hand
            // edited account file.
            events_send(
                &events,
                EngineEvent::Disconnected {
                    account_id,
                    reason: "the account has no GroupWise proxy configuration".to_string(),
                    retrying: false,
                },
            );
            break;
        };
        let secret = match account_secret(&*secrets, &account, &tls).await {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: format!(
                            "no password or token stored for {}; open the account settings",
                            account.from_address()
                        ),
                        retrying: false,
                    },
                );
                events.send(EngineEvent::Stopped { account_id });
                return;
            }
            Err(error) => {
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: error.to_string(),
                        retrying: true,
                    },
                );
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        let mut config = crate::groupwise::GroupWiseConfig::new(
            proxy.proxy.clone(),
            account.username.clone(),
            &proxy,
        );
        config.policy = tls.clone();
        let credential = Credential::Password(&secret);

        let login = tokio::select! {
            result = crate::groupwise::GroupWiseClient::login(&config, credential) => result,
            changed = shutdown.changed() => { let _ = changed; break; }
        };
        let mut client = match login {
            Ok(client) => client,
            Err(error) => {
                let transient = error.is_transient();
                events_send(
                    &events,
                    EngineEvent::Disconnected {
                        account_id,
                        reason: error.to_string(),
                        retrying: transient,
                    },
                );
                attempt = attempt.saturating_add(1);
                if !transient {
                    break;
                }
                continue;
            }
        };
        attempt = 0;
        let outcome = client.outcome();
        events_send(
            &events,
            EngineEvent::Connected {
                account_id,
                host: outcome.host.clone(),
                encrypted: outcome.encrypted,
                tls_version: None,
            },
        );

        // -- discover the folder tree ---------------------------------------
        match client.list_folders(account_id).await {
            Ok(discovered) => {
                let count = discovered
                    .iter()
                    .filter(|folder| folder.is_selectable())
                    .count();
                if let Err(error) = store_folders(&database, &discovered) {
                    tracing::warn!(account = %account.email, %error, "cannot store the folder list");
                }
                events_send(
                    &events,
                    EngineEvent::FoldersDiscovered { account_id, count },
                );
            }
            Err(error) => {
                tracing::warn!(account = %account.email, %error, "the folder list could not be read");
            }
        }

        // -- one synchronisation pass ---------------------------------------
        let local = match database.read(|connection| folders::list_folders(connection, account_id))
        {
            Ok(local) => local,
            Err(error) => {
                tracing::warn!(%error, "cannot read the folder list");
                Vec::new()
            }
        };
        let mut progress = progress_events(events.clone(), account_id);
        let reports = crate::groupwise::sync_folders(
            &mut client,
            &database,
            account_id,
            &local,
            sync_options.max_new,
            &mut progress,
        )
        .await;
        for result in reports {
            match result {
                Ok(report) if !report.is_noop() => events_send(
                    &events,
                    EngineEvent::FolderSynced {
                        account_id,
                        report: Box::new(report),
                    },
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    account = %account.email,
                    %error,
                    "a folder could not be synchronised"
                ),
            }
        }
        let _ = client.logout().await;
        tracing::debug!(
            account = %account.from_address(),
            "the GroupWise account is up to date"
        );
    }

    events.send(EngineEvent::Stopped { account_id });
}

/// Waits for `IDLE` notifications, or polls when the server has none.
///
/// Takes the session by value because `IDLE` *owns* the connection: the
/// compiler then guarantees that no command can be interleaved into the middle
/// of an `IDLE` exchange.  The session is handed back whenever the connection is
/// still usable, and `None` means "reconnect".
async fn watch_loop(
    session: ImapSession,
    commands: &mut mpsc::UnboundedReceiver<AccountCommand>,
    shutdown: &mut watch::Receiver<bool>,
    poll: Option<Duration>,
    force_sync: &mut bool,
) -> (WatchOutcome, Option<ImapSession>) {
    if *shutdown.borrow() {
        return (WatchOutcome::Stop, Some(session));
    }
    if *force_sync {
        *force_sync = false;
        return (WatchOutcome::Refresh, Some(session));
    }

    if session.capabilities().has_idle() {
        let mut guard = session.into_idle();
        if let Err(error) = guard.start().await {
            tracing::warn!(%error, "IDLE could not be started; polling instead");
            return (WatchOutcome::Refresh, guard.finish().await.ok());
        }
        let outcome = tokio::select! {
            changed = shutdown.changed() => { let _ = changed; WatchOutcome::Stop }
            command = commands.recv() => match command {
                Some(AccountCommand::Stop) | None => WatchOutcome::Stop,
                Some(AccountCommand::SyncNow | AccountCommand::RefreshFolders) => {
                    WatchOutcome::Refresh
                }
            },
            event = guard.wait_reissue() => {
                match event {
                    // A notification, a keepalive or the 25 minute re-issue
                    // deadline: all three mean "synchronise and re-arm".
                    Ok(_) => WatchOutcome::Refresh,
                    Err(error) => {
                        tracing::debug!(%error, "IDLE ended");
                        WatchOutcome::Refresh
                    }
                }
            }
        };
        return (outcome, guard.finish().await.ok());
    }

    // No `IDLE`: `NOOP` is both the keepalive and the cheapest way to learn that
    // something changed.
    let poll = poll.unwrap_or(Duration::from_secs(24 * 60 * 60));
    let mut session = session;
    let outcome = tokio::select! {
        changed = shutdown.changed() => { let _ = changed; WatchOutcome::Stop }
        command = commands.recv() => match command {
            Some(AccountCommand::Stop) | None => WatchOutcome::Stop,
            Some(AccountCommand::SyncNow | AccountCommand::RefreshFolders) => WatchOutcome::Refresh,
        },
        () = tokio::time::sleep(poll) => match session.noop().await {
            Ok(()) => WatchOutcome::Refresh,
            Err(error) => {
                tracing::debug!(%error, "NOOP failed");
                WatchOutcome::Refresh
            }
        },
    };
    (outcome, Some(session))
}

/// The secret an account authenticates with.
///
/// OAuth2 accounts authenticate with an *access token*, which is refreshed in
/// place when the stored one is about to expire - the caller gets a usable
/// token either way.  Every other account uses the password from the vault.
///
/// The returned [`secrecy::SecretString`] is the *only* owner of the plaintext:
/// it is zeroed on drop and never formatted into a log record, which is why the
/// credential built from it is a borrow.
async fn account_secret(
    store: &dyn SecretStore,
    account: &Account,
    tls: &TlsPolicy,
) -> Result<Option<secrecy::SecretString>> {
    match account.auth {
        AuthKind::OAuth2 => oauth::ensure_access_token(store, account, tls)
            .await
            .map(Some),
        _ => store.get(&CredentialKey::new(
            account.id,
            CredentialKind::IncomingPassword,
        )),
    }
}

/// Stores the discovered folders under stable ids.
///
/// [`folders::upsert_folder_by_path`] keeps the local id of a folder whose path
/// did not change, which is what makes the message rows survive a restart.
/// Folders that disappeared on the server are deliberately left alone: removing
/// them would delete their messages, and a folder that a broken `LIST` merely
/// failed to report would take a lot of mail with it.
fn store_folders(database: &Database, discovered: &[Folder]) -> Result<()> {
    database.write(|connection| folders::upsert_tree(connection, discovered))
}

/// Sends an event, ignoring a closed channel (the UI is gone).
fn events_send(events: &EventSink, event: EngineEvent) {
    events.send(event);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::models::{utc_now, Endpoint};

    /// A plain, enabled IMAP account.
    fn account() -> Account {
        Account {
            id: AccountId::new(),
            display_name: "Test".to_string(),
            email: "anna@example.org".to_string(),
            kind: AccountKind::Imap,
            auth: AuthKind::Password,
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
    fn a_task_survives_only_while_its_account_stays_enabled_and_unchanged() {
        let running = account();
        assert!(task_still_current(&running, std::slice::from_ref(&running)));
        assert!(!task_still_current(&running, &[]));

        let mut disabled = running.clone();
        disabled.enabled = false;
        assert!(!task_still_current(&running, &[disabled]));

        let mut edited = running.clone();
        edited.display_name = "Anders".to_string();
        assert!(!task_still_current(&running, &[edited]));

        let mut replaced = running.clone();
        replaced.id = AccountId::new();
        assert!(!task_still_current(&running, &[replaced]));
    }

    #[test]
    fn states_only_connect_when_they_may() {
        assert!(AccountState::Idle.may_connect());
        assert!(AccountState::Waiting.may_connect());
        assert!(!AccountState::Syncing.may_connect());
        assert!(!AccountState::Watching.may_connect());
        assert!(!AccountState::Stopped.may_connect());
    }

    #[test]
    fn the_state_machine_moves_forward() {
        let state = AccountState::Idle;
        let state = state.after_connect();
        assert_eq!(state, AccountState::Syncing);
        let state = state.after_sync();
        assert_eq!(state, AccountState::Watching);
        let state = state.after_failure();
        assert_eq!(state, AccountState::Waiting);
        assert!(state.may_connect());
    }

    #[test]
    fn a_zero_interval_disables_the_timer_only_without_idle() {
        let mut settings = Settings::default();
        settings.sync_interval_minutes = 0;
        assert_eq!(next_poll_interval(&settings, false), None);
        // With `IDLE` the interval is a safety net, never `None`.
        assert!(next_poll_interval(&settings, true).is_some());
    }

    #[test]
    fn the_configured_interval_becomes_a_duration() {
        let mut settings = Settings::default();
        settings.sync_interval_minutes = 5;
        assert_eq!(
            next_poll_interval(&settings, false),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn the_handle_offers_a_full_sync_and_an_account_reload() {
        let (commands, mut queued) = mpsc::unbounded_channel();
        let (_events_tx, events) = mpsc::unbounded_channel();
        let (shutdown, _stop) = watch::channel(false);
        let handle = EngineHandle {
            commands,
            events,
            shutdown,
        };

        handle.sync_all().unwrap();
        assert_eq!(queued.try_recv().unwrap(), EngineCommand::SyncAll);

        handle.accounts_changed().unwrap();
        assert_eq!(queued.try_recv().unwrap(), EngineCommand::AccountsChanged);

        handle.settings_changed().unwrap();
        assert_eq!(queued.try_recv().unwrap(), EngineCommand::SettingsChanged);
    }

    #[test]
    fn a_closed_sink_drops_its_late_events() {
        let (sender, mut received) = mpsc::unbounded_channel();
        let sink = EventSink::new(sender);
        sink.send(EngineEvent::Finished);
        assert!(received.try_recv().is_ok(), "an open sink forwards");

        sink.close();
        sink.send(EngineEvent::Finished);
        assert!(
            received.try_recv().is_err(),
            "a superseded task must not reach the UI"
        );
    }

    #[test]
    fn events_describe_what_the_ui_has_to_redraw() {
        let account_id = AccountId::new();
        let report = FolderSyncReport {
            name: "INBOX".to_string(),
            new_messages: 3,
            ..FolderSyncReport::default()
        };
        let event = EngineEvent::FolderSynced {
            account_id,
            report: Box::new(report),
        };
        match event {
            EngineEvent::FolderSynced { report, .. } => {
                assert_eq!(report.new_messages, 3);
                assert!(!report.is_noop());
            }
            _ => panic!("wrong event"),
        }
    }
}
