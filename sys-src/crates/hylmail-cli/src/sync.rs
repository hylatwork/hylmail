//! `hylmail-cli --sync`: one synchronisation pass, straight from the terminal.
//!
//! The graphical client runs the same code in the background engine
//! ([`hylmail_core::sync`]); this module exists so that a support case can be
//! diagnosed without the UI: which accounts are configured, whether the
//! credentials in the vault still work, what the server advertises, and how
//! many messages a pass stores.
//!
//! Two deliberate differences from the engine:
//!
//! * exactly **one** round is run per account and the process then exits, so the
//!   command can be used from a script;
//! * the result is printed as a small report instead of
//!   [`hylmail_core::sync::EngineEvent`]s, because there is no UI to consume
//!   those here.

use std::process::ExitCode;

use anyhow::{Context, Result};
use secrecy::SecretString;

use hylmail_core::db::{folders as folder_store, Database};
use hylmail_core::ews::{sync_folders as sync_exchange_folders, EwsClient, EwsConfig};
use hylmail_core::groupwise::{
    sync_folders as sync_groupwise_folders, GroupWiseClient, GroupWiseConfig,
};
use hylmail_core::imap::synchronise::{
    self, FolderSyncReport, SyncDepth, SyncOptions, SyncProgress,
};
use hylmail_core::imap::{Credential, ImapConfig, ImapSession};
use hylmail_core::models::account::{Account, AccountKind, AuthKind};
use hylmail_core::models::folder::Folder;
use hylmail_core::net::TlsPolicy;
use hylmail_core::secrets::{load_secret, CredentialKind};
use hylmail_core::Core;
/// What the user asked for on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// Only synchronise the account whose name, address or id matches.
    pub account: Option<String>,
    /// Also drop messages the server no longer has.
    pub full: bool,
    /// Stop after this many new messages per folder (`0` = no limit).
    pub max_new: usize,
}

/// The totals of one account, plus what the connection said about itself.
#[derive(Debug, Default)]
struct AccountReport {
    host: String,
    encrypted: bool,
    tls_version: Option<String>,
    capabilities: Vec<String>,
    /// Protocol specific identity of the server, e.g. the EWS schema version.
    server_info: Option<String>,
    folders: Vec<FolderSyncReport>,
    new_messages: usize,
    updated_flags: usize,
    removed_messages: usize,
    rebuilt: usize,
    deferred: bool,
}

/// Runs one pass per selected account.
///
/// Returns [`ExitCode::FAILURE`] when at least one account could not be
/// synchronised; the report is printed in every case.
pub fn run(core: &Core, request: &Request) -> Result<ExitCode> {
    let document = core
        .config()
        .load_accounts()
        .context("reading accounts.json")?;
    let selected: Vec<&Account> = document
        .accounts
        .iter()
        .filter(|account| matches(account, request.account.as_deref()))
        .collect();

    if selected.is_empty() {
        if document.accounts.is_empty() {
            println!("No account is configured; run `hylmail-cli --init` and add one.");
        } else {
            println!(
                "No account matches `{}`; {} account(s) are configured.",
                request.account.as_deref().unwrap_or_default(),
                document.accounts.len()
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    // `rustls` and the IMAP client both want to spawn tasks, so the runtime has
    // worker threads; the accounts themselves are visited one after the other.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;

    let mut failures = 0usize;
    for account in selected {
        if !account.enabled {
            println!(
                "{} ({}) is disabled and was skipped.",
                account.display_name, account.email
            );
            continue;
        }
        match runtime.block_on(synchronise_account(core, account, request)) {
            Ok(report) => print_report(account, &report),
            Err(error) => {
                failures += 1;
                // `{error:#}` walks the source chain, which is where the
                // interesting DNS, TLS and parser causes live.
                println!(
                    "{} ({}) could not be synchronised: {error:#}",
                    account.display_name, account.email
                );
                tracing::warn!(
                    account = %account.id,
                    %error,
                    "diagnostic synchronisation failed"
                );
            }
        }
    }

    if failures > 0 {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// `true` when the account is the one the user asked for.
fn matches(account: &Account, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let filter = filter.trim();
    filter.eq_ignore_ascii_case(&account.email)
        || filter.eq_ignore_ascii_case(&account.display_name)
        || account.id.to_string() == filter
}

/// `hylmail-cli --oauth-login WHO`: runs the OAuth2 PKCE flow in the browser.
///
/// The flow needs a browser and a human, so this is the one command that
/// deliberately blocks: it binds the loopback listener, opens the
/// authorisation URL, waits for the redirect and stores the tokens in the
/// vault.  The token itself is never printed.
pub fn login(core: &Core, who: &str) -> Result<ExitCode> {
    let document = core
        .config()
        .load_accounts()
        .context("reading accounts.json")?;
    let selected: Vec<&Account> = document
        .accounts
        .iter()
        .filter(|account| matches(account, Some(who)))
        .collect();

    let account = match selected.as_slice() {
        [] => {
            println!(
                "No account matches `{who}`; {} account(s) are configured.",
                document.accounts.len()
            );
            return Ok(ExitCode::FAILURE);
        }
        [account] => *account,
        many => {
            println!(
                "`{who}` matches {} accounts; please use the mail address.",
                many.len()
            );
            return Ok(ExitCode::FAILURE);
        }
    };

    if account.auth != AuthKind::OAuth2 {
        println!(
            "{} signs in with {}; nothing to do.",
            account.from_address(),
            account.auth.label()
        );
        return Ok(ExitCode::FAILURE);
    }
    if account.oauth.is_none() {
        println!(
            "{} uses OAuth 2.0 but has no endpoints configured.",
            account.from_address()
        );
        return Ok(ExitCode::FAILURE);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;
    let tls = TlsPolicy::strict();
    println!(
        "Opening the browser for {} - please confirm the sign-in there.",
        account.from_address()
    );
    let tokens = runtime
        .block_on(hylmail_core::oauth::sign_in(core.secrets(), account, &tls))
        .with_context(|| format!("signing in {}", account.from_address()))?;
    println!(
        "Sign-in complete for {}; {} token(s) stored in the vault.",
        account.from_address(),
        if tokens.refresh_token().is_some() {
            2
        } else {
            1
        }
    );
    Ok(ExitCode::SUCCESS)
}

/// Connects, lists the folders, and synchronises them once.
async fn synchronise_account(
    core: &Core,
    account: &Account,
    request: &Request,
) -> Result<AccountReport> {
    match account.kind {
        AccountKind::Imap => imap_account(core, account, request).await,
        AccountKind::Exchange => exchange_account(core, account, request).await,
        AccountKind::GroupWiseProxy => groupwise_account(core, account, request).await,
    }
}

/// Reads the stored credential of an account as an owned secret.
fn credential_secret(core: &Core, account: &Account) -> Result<SecretString> {
    let secret = load_secret(
        core.secrets(),
        account.id,
        match account.auth {
            AuthKind::OAuth2 => CredentialKind::OAuthAccessToken,
            _ => CredentialKind::IncomingPassword,
        },
    )
    .context("reading the credential from the vault")?;
    let Some(secret) = secret else {
        anyhow::bail!("no credential is stored for this account; add it in the account dialog");
    };
    Ok(SecretString::from(secret))
}

/// One pass against an Exchange mailbox over EWS.
async fn exchange_account(
    core: &Core,
    account: &Account,
    request: &Request,
) -> Result<AccountReport> {
    let secret = credential_secret(core, account)?;
    let credential = match account.auth {
        AuthKind::OAuth2 => hylmail_core::secrets::Credential::OAuth2 {
            user: account.username.clone(),
            token: &secret,
        },
        _ => hylmail_core::secrets::Credential::Password(&secret),
    };
    let mut config = EwsConfig::new(
        account.incoming.clone(),
        account.username.clone(),
        account.auth,
    );
    config.policy = TlsPolicy::strict();
    let mut client = EwsClient::login(&config, credential)
        .await
        .context("connecting to Exchange (EWS)")?;
    let outcome = client.outcome();

    let mut report = AccountReport {
        host: outcome.host.clone(),
        encrypted: outcome.encrypted,
        server_info: Some(outcome.describe()),
        ..AccountReport::default()
    };

    let discovered = client
        .list_folders(account.id)
        .await
        .context("listing the folders")?;
    store_folders(core.database(), &discovered).context("storing the folder list")?;
    let selectable: Vec<Folder> = discovered
        .into_iter()
        .filter(Folder::is_selectable)
        .collect();

    let options = SyncOptions {
        depth: if request.full {
            SyncDepth::Full
        } else {
            SyncDepth::Incremental
        },
        max_new: request.max_new,
        ..SyncOptions::default()
    };
    let results = sync_exchange_folders(
        &mut client,
        core.database(),
        account.id,
        &selectable,
        options,
        &mut print_progress,
    )
    .await;
    accumulate(&mut report, results)?;
    Ok(report)
}

/// One pass against a GroupWise mailbox over the SOAP proxy.
async fn groupwise_account(
    core: &Core,
    account: &Account,
    request: &Request,
) -> Result<AccountReport> {
    let Some(proxy) = account.proxy.as_ref() else {
        anyhow::bail!("the account has no GroupWise proxy configuration");
    };
    let secret = credential_secret(core, account)?;
    if account.auth == AuthKind::OAuth2 {
        anyhow::bail!("GroupWise has no OAuth2 support; use a password");
    }
    let mut config = GroupWiseConfig::new(proxy.proxy.clone(), account.username.clone(), proxy);
    config.policy = TlsPolicy::strict();
    let mut client = GroupWiseClient::login(&config, Credential::Password(&secret))
        .await
        .context("connecting to the GroupWise proxy")?;
    let outcome = client.outcome();

    let mut report = AccountReport {
        host: outcome.host.clone(),
        encrypted: outcome.encrypted,
        server_info: Some(outcome.describe()),
        ..AccountReport::default()
    };

    let discovered = client
        .list_folders(account.id)
        .await
        .context("listing the folders")?;
    store_folders(core.database(), &discovered).context("storing the folder list")?;
    let selectable: Vec<Folder> = discovered
        .into_iter()
        .filter(Folder::is_selectable)
        .collect();

    // GroupWise always compares the whole folder: there is no delta protocol.
    let results = sync_groupwise_folders(
        &mut client,
        core.database(),
        account.id,
        &selectable,
        request.max_new,
        &mut print_progress,
    )
    .await;
    accumulate(&mut report, results)?;
    client
        .logout()
        .await
        .context("closing the GroupWise session")?;
    Ok(report)
}

/// Connects, lists the folders, and synchronises them once over IMAP.
async fn imap_account(core: &Core, account: &Account, request: &Request) -> Result<AccountReport> {
    let secret = credential_secret(core, account)?;
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
        TlsPolicy::strict(),
    );
    let mut session = ImapSession::login(&config, credential)
        .await
        .context("connecting to the IMAP server")?;
    let outcome = session.outcome();

    let mut report = AccountReport {
        host: outcome.host.clone(),
        encrypted: outcome.encrypted,
        tls_version: outcome.tls_version.clone(),
        capabilities: session.capabilities().names().map(str::to_string).collect(),
        ..AccountReport::default()
    };

    // The folder list is the one piece of state the pass needs, and storing it
    // is what makes `hylmail-cli --stats` show something after a diagnostic run.
    let discovered = session
        .list_folders(account.id)
        .await
        .context("listing the folders")?;
    store_folders(core.database(), &discovered).context("storing the folder list")?;

    let selectable: Vec<Folder> = discovered
        .into_iter()
        .filter(Folder::is_selectable)
        .collect();

    let options = SyncOptions {
        depth: if request.full {
            SyncDepth::Full
        } else {
            SyncDepth::Incremental
        },
        max_new: request.max_new,
        ..SyncOptions::default()
    };
    // The pass is printed as it happens: a first synchronisation of a large
    // mailbox takes a while, and silence looks like a hang.
    let results = synchronise::sync_folders(
        &mut session,
        core.database(),
        account.id,
        &selectable,
        options,
        &mut print_progress,
    )
    .await;

    for result in results {
        let folder = result.context("synchronising a folder")?;
        report.new_messages += folder.new_messages;
        report.updated_flags += folder.updated_flags;
        report.removed_messages += folder.removed_messages;
        report.rebuilt += usize::from(folder.uid_validity_reset);
        report.deferred |= folder.deferred;
        report.folders.push(folder);
    }

    session.logout().await;
    Ok(report)
}

/// Adds the folder results of a pass to the running totals.
fn accumulate(
    report: &mut AccountReport,
    results: Vec<hylmail_core::Result<FolderSyncReport>>,
) -> Result<()> {
    for result in results {
        let folder = result.context("synchronising a folder")?;
        report.new_messages += folder.new_messages;
        report.updated_flags += folder.updated_flags;
        report.removed_messages += folder.removed_messages;
        report.rebuilt += usize::from(folder.uid_validity_reset);
        report.deferred |= folder.deferred;
        report.folders.push(folder);
    }
    Ok(())
}

/// Stores the discovered folders under their stable local ids.
fn store_folders(database: &Database, discovered: &[Folder]) -> Result<()> {
    database
        .write(|connection| folder_store::upsert_tree(connection, discovered))
        .map_err(Into::into)
}

fn print_progress(progress: SyncProgress) {
    match progress {
        SyncProgress::FolderStarted { name, full } => {
            let suffix = if full { " (full pass)" } else { "" };
            println!("  ... {name}{suffix}");
        }
        SyncProgress::HeadersStored { added, highest_uid } => {
            // Exchange has no UID space, so the field is only shown when the
            // server filled it in.
            if highest_uid == 0 {
                println!("      {added} new message(s)");
            } else {
                println!("      {added} new message(s), highest UID {highest_uid}");
            }
        }
        SyncProgress::MessagesRemoved { removed } => {
            println!("      {removed} message(s) are no longer on the server");
        }
        SyncProgress::FlagsUpdated { updated } => {
            println!("      {updated} message(s) with corrected flags");
        }
        SyncProgress::Deferred { .. } => {
            println!("      pause: the rest is fetched by the next pass");
        }
        SyncProgress::FolderFinished { .. } => {}
    }
}

fn print_report(account: &Account, report: &AccountReport) {
    println!(
        "{name} <{email}> [{kind}]",
        name = account.display_name,
        email = account.email,
        kind = account.kind.label()
    );
    println!(
        "  server      : {} [{}]",
        report.host,
        match (&report.tls_version, report.encrypted) {
            (Some(version), _) => version.as_str(),
            (None, true) => "encrypted",
            (None, false) => "cleartext",
        }
    );
    println!("  capabilities: {}", report.capabilities.join(" "));
    if let Some(info) = &report.server_info {
        println!("  protocol    : {info}");
    }
    println!("  folders     : {}", report.folders.len());
    for folder in &report.folders {
        let note = if folder.deferred {
            "  (deferred)"
        } else if folder.uid_validity_reset {
            "  (rebuilt)"
        } else {
            ""
        };
        println!(
            "      {:<28} {:>6} new  {:>5} flags  {:>5} removed{note}",
            folder.name, folder.new_messages, folder.updated_flags, folder.removed_messages
        );
    }
    println!(
        "  stored here : {} new, {} flag update(s), {} removed, {} rebuilt",
        report.new_messages, report.updated_flags, report.removed_messages, report.rebuilt
    );
    if report.deferred {
        println!("  note        : the pass stopped early; run `hylmail-cli --sync` again");
    }
}
