//! Phase 4 diagnostics: parsing `.eml` files, the composer and the outbox.
//!
//! These commands call exactly the core code paths the user interface uses, so
//! what a diagnostic run prints is what the UI will show.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};

use hylmail_core::compose::{build, ComposeContext, ComposedMessage};
use hylmail_core::html::HtmlPolicy;
use hylmail_core::mime;
use hylmail_core::models::{
    unique_file_names, AuthKind, Draft, MailAddress, MessageId, OutboxItem, Settings,
};
use hylmail_core::net::TlsPolicy;
use hylmail_core::outbox::{self, Delivery, Submitter};
use hylmail_core::secrets::{self, CredentialKind};
use hylmail_core::send;
use hylmail_core::Core;

/// Handles `--parse FILE.eml`.
pub fn parse_file(core: &Core, path: &Path, save_attachments: Option<&Path>) -> Result<ExitCode> {
    let raw = std::fs::read(path).with_context(|| format!("reading `{}`", path.display()))?;
    let settings = core.settings();
    let policy = HtmlPolicy::from_settings(settings, None);

    let message = mime::parse(&raw, MessageId::new(), &policy)
        .with_context(|| format!("parsing `{}`", path.display()))?;

    print_headers(&message);
    print_body(&message, &policy);
    print_calendar(&message);
    if let Some(directory) = save_attachments {
        save_parts(&raw, &message, directory)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Writes every attachment of the file into `directory`.
///
/// The names are made safe against the file name the sender chose; a message
/// that arrives with `filename="../../authorized_keys"` still lands inside the
/// directory the user named on the command line.
fn save_parts(raw: &[u8], message: &mime::ParsedMessage, directory: &Path) -> Result<()> {
    if message.body.attachments.is_empty() {
        println!("Attachments");
        println!("  (none)");
        return Ok(());
    }
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating `{}`", directory.display()))?;
    let names = unique_file_names(&message.body.attachments);
    let targets: Vec<(String, std::path::PathBuf)> = message
        .body
        .attachments
        .iter()
        .zip(names)
        .map(|(attachment, name)| (attachment.part_id.clone(), directory.join(name)))
        .collect();
    let written = hylmail_core::body::save_attachments(raw, &targets)
        .with_context(|| format!("writing into `{}`", directory.display()))?;

    println!("Attachments");
    for (_, destination) in &targets {
        println!("  {}", destination.display());
    }
    println!("  {written} file(s) written");
    Ok(())
}

fn print_headers(message: &mime::ParsedMessage) {
    let headers = &message.headers;
    println!("Headers");
    println!(
        "  message id     : {}",
        headers.message_id.as_deref().unwrap_or("(none)")
    );
    println!("  subject        : {}", headers.subject);
    println!("  from           : {}", addresses(&headers.from));
    println!("  to             : {}", addresses(&headers.to));
    if !headers.cc.is_empty() {
        println!("  cc             : {}", addresses(&headers.cc));
    }
    if let Some(date) = headers.date {
        println!("  date           : {date}");
    }
}

/// Joins addresses the way a header would show them.
fn addresses(list: &[MailAddress]) -> String {
    if list.is_empty() {
        return "(none)".to_string();
    }
    list.iter()
        .map(MailAddress::to_header_value)
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_body(message: &mime::ParsedMessage, _policy: &HtmlPolicy) {
    let body = &message.body;
    println!("Body");
    println!(
        "  plain text     : {}",
        body.plain_text
            .as_deref()
            .map_or("(none)".to_string(), |text| format!(
                "{} char(s)",
                text.chars().count()
            ))
    );
    println!(
        "  html           : {}",
        if message.html.html.is_empty() {
            "(none)".to_string()
        } else {
            format!("{} char(s)", message.html.html.chars().count())
        }
    );
    println!("  attachments    : {}", body.attachments.len());
    for attachment in &body.attachments {
        println!(
            "    - {} ({}, {} byte(s){})",
            attachment.display_name(),
            attachment.mime_type,
            attachment.size_bytes.unwrap_or(0),
            if attachment.inline { ", inline" } else { "" }
        );
    }
    println!("  images         : {}", body.image_count);
    println!(
        "  external images: {}",
        if body.external_images_blocked {
            "blocked"
        } else {
            "allowed"
        }
    );
    println!("  snippet        : {:?}", message.snippet);
    println!("  calendar invite: {}", message.is_calendar_invite);

    let stats = message.html.images;
    println!(
        "  sanitised      : {} external / {} blocked / {} inline",
        stats.external, stats.blocked, stats.inline
    );
}

fn print_calendar(message: &mime::ParsedMessage) {
    if let Some(calendar) = &message.calendar {
        println!("Calendar");
        println!("  method         : {}", calendar.method.label());
        println!("  title          : {}", calendar.title());
        println!(
            "  when           : {}",
            calendar.when().as_deref().unwrap_or("(unknown)")
        );
    }
}

/// Handles `--outbox` and `--outbox --retry`.
pub fn outbox_report(core: &Core) -> Result<ExitCode> {
    let document = core.config().load_outbox().context("reading outbox.json")?;
    if document.items.is_empty() {
        println!("Outbox is empty.");
        return Ok(ExitCode::SUCCESS);
    }
    println!("Outbox ({} item(s))", document.items.len());
    for item in &document.items {
        print_item(item);
    }

    let stale = outbox::attention(&document);
    if !stale.is_empty() {
        println!();
        println!("{} message(s) need attention:", stale.len());
        for item in &stale {
            println!("  - {} ({})", item.draft.subject, item.state.label());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_item(item: &OutboxItem) {
    println!(
        "  [{}] {} -> {} (attempt {}, next {})",
        item.id,
        item.state.label(),
        item.draft
            .to
            .iter()
            .map(MailAddress::to_header_value)
            .collect::<Vec<_>>()
            .join(", "),
        item.attempts,
        item.next_attempt_at
            .map_or("(none)".to_string(), |at| at.to_rfc3339())
    );
    println!("      subject : {}", item.draft.subject);
    if let Some(error) = &item.last_error {
        println!("      last    : {error}");
    }
}

/// Handles `--retry`: put every parked message back into the queue.
pub fn retry_all(core: &Core) -> Result<ExitCode> {
    let mut document = core.config().load_outbox().context("reading outbox.json")?;
    let ids: Vec<_> = document
        .items
        .iter()
        .filter(|item| item.state == hylmail_core::models::OutboxState::Failed)
        .map(|item| item.id)
        .collect();
    if ids.is_empty() {
        println!("Nothing to retry.");
        return Ok(ExitCode::SUCCESS);
    }
    for id in &ids {
        let _ = outbox::retry(&mut document, *id);
    }
    core.config()
        .save_outbox(&document)
        .context("writing outbox.json")?;
    println!(
        "{count} message(s) queued for another attempt.",
        count = ids.len()
    );
    Ok(ExitCode::SUCCESS)
}

/// Handles `--send`: submit every due queued message once.
pub fn send(core: &Core) -> Result<ExitCode> {
    let mut document = core.config().load_outbox().context("reading outbox.json")?;
    let now = hylmail_core::models::utc_now();

    if !document.items.iter().any(|item| item.state.is_pending()) {
        println!("Nothing to send: the outbox holds no queued message.");
        return Ok(ExitCode::SUCCESS);
    }

    let accounts = core
        .config()
        .load_accounts()
        .context("reading accounts.json")?;
    let settings = core.settings().clone();
    let submitter = LiveSubmitter {
        core,
        accounts: &accounts,
    };
    let context = |item: &OutboxItem| {
        let account = accounts
            .accounts
            .iter()
            .find(|account| account.id == item.draft.account_id);
        context_for(account, &settings, now)
    };

    let report = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting a Tokio runtime")?
        .block_on(async { outbox::drain(&mut document, &submitter, &context, now, None).await });

    core.config()
        .save_outbox(&document)
        .context("writing outbox.json")?;

    println!("{}", report.describe());
    Ok(ExitCode::SUCCESS)
}

/// Handles `--compose DRAFT.json [--out FILE.eml]`.
pub fn compose(core: &Core, draft_path: &Path, out: Option<&Path>) -> Result<ExitCode> {
    let raw = std::fs::read_to_string(draft_path)
        .with_context(|| format!("reading `{}`", draft_path.display()))?;
    let draft: Draft = serde_json::from_str(&raw)
        .with_context(|| format!("`{}` is not a draft document", draft_path.display()))?;
    let settings = core.settings().clone();
    let accounts = core
        .config()
        .load_accounts()
        .context("reading accounts.json")?;
    let account = accounts
        .accounts
        .iter()
        .find(|account| account.id == draft.account_id);
    let now = hylmail_core::models::utc_now();
    let context = context_for(account, &settings, now);
    let composed = build(&draft, &context)?;

    match out {
        Some(path) => {
            std::fs::write(path, &composed.raw)
                .with_context(|| format!("writing `{}`", path.display()))?;
            println!(
                "Wrote {} byte(s) to {} (message id {}).",
                composed.raw.len(),
                path.display(),
                composed.message_id
            );
        }
        None => println!("{}", describe(&composed)),
    }
    Ok(ExitCode::SUCCESS)
}

/// The composer context for one account, with its default signature attached.
fn context_for(
    account: Option<&hylmail_core::models::Account>,
    settings: &Settings,
    now: chrono::DateTime<chrono::Utc>,
) -> ComposeContext {
    let Some(account) = account else {
        return ComposeContext::default();
    };
    let mut context = ComposeContext::from_address(MailAddress::with_name(
        account.email.clone(),
        account.display_name.clone(),
    ));
    context.date = Some(now);
    context.sender = Some(account.email.clone());
    if let Some(signature) = settings.default_signature(account.id) {
        context.signature = Some(signature.clone());
    }
    context
}

fn describe(message: &ComposedMessage) -> String {
    if message.recipients.is_empty() {
        return format!(
            "From {} Subject {:?} ({} byte(s), no recipient)",
            message.from,
            message.subject,
            message.raw.len()
        );
    }
    format!(
        "From {} To {} Subject {:?} ({} byte(s))",
        message.from,
        message.recipients.join(", "),
        message.subject,
        message.raw.len()
    )
}

/// Submits through [`hylmail_core::send`], which picks the protocol the account
/// speaks, using the account's stored secret.
struct LiveSubmitter<'a> {
    core: &'a Core,
    accounts: &'a hylmail_core::store::AccountsDocument,
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
        let stored = match secrets::load_secret(self.core.secrets(), account.id, kind) {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                return Delivery::Permanent {
                    reason: format!("Für dieses Konto ist kein {} gespeichert.", kind.label()),
                }
            }
            Err(error) => return Delivery::from_error(&error),
        };

        let stored = secrets::secret(stored);
        let credential = if account.auth == AuthKind::OAuth2 {
            secrets::Credential::OAuth2 {
                user: account.username.clone(),
                token: &stored,
            }
        } else {
            secrets::Credential::Password(&stored)
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
