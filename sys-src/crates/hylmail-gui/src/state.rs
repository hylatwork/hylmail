//! The interface state, independent of any drawing code.
//!
//! Everything the user can see or do is decided here: which folders exist,
//! which message is selected, what the reading pane holds, what the composer
//! is about to send.  `egui` only paints the result, which is what makes this
//! module unit testable without a window.
//!
//! The rule of the module is that no method blocks: loading a body is a
//! [`Task`] handed to [`crate::worker`], never a network call in a `draw`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use hylmail_core::db::{
    contacts as db_contacts, content, conversations as db_conversations, folders as db_folders,
    messages as db_messages,
};
use hylmail_core::html::{sanitize, HtmlPolicy, SanitisedHtml};
use hylmail_core::models::{
    safe_file_name, unique_file_names, Account, AccountId, AccountKind, AuthKind, Contact,
    ContactId, DoneMode, Draft, DraftAttachment, DraftId, Folder, FolderId, FolderKind,
    MailAddress, MessageBody, MessageFlags, MessageHeader, MessageId, MessageQuery, OutboxId, Rule,
    RuleId, RuleSet, Settings, Signature, DEFAULT_SERVER_SEARCH_MIN_CHARS, GROUPWISE_SEARCH_WINDOW,
};
use hylmail_core::secrets::CredentialKind;
use hylmail_core::store::AccountsDocument;
use hylmail_core::Core;

use crate::action::{Action, SortKey};
use crate::format::plural;
use crate::worker::UiEvent;

/// Something only the background worker can do.
///
/// The interface never performs I/O; it produces a task, and the worker turns
/// it into an event that comes back through the channel.
pub(crate) enum Task {
    /// Synchronise one account, or every account when `None`.
    Sync(Option<hylmail_core::models::AccountId>),
    /// Re-read the folder list of one account.
    ReloadFolders(hylmail_core::models::AccountId),
    /// Tell the engine the account list changed, so new accounts start syncing.
    ReloadAccounts,
    /// Tell the engine the settings changed, so it restarts with the new ones.
    ReloadSettings,
    /// Download and cache the body of one message.
    FetchBody {
        /// Folder that holds the message.
        folder_id: FolderId,
        /// Message to fetch.
        message_id: MessageId,
    },
    /// Download and decode the remote images of one message (`FR-READ-2/3`).
    LoadImages {
        /// Message the images belong to.
        message_id: MessageId,
        /// Absolute image URLs to fetch.
        urls: Vec<String>,
    },
    /// Run a server-side search on the enabled IMAP accounts (`FR-SEARCH-2`).
    ServerSearch {
        /// The free-text query.
        query: String,
    },
    /// Push read / flagged state to the server.
    PushFlags {
        /// Message whose flags changed.
        message_id: MessageId,
    },
    /// Delete a message on the server and drop the local copy.
    DeleteMessage {
        /// Folder that holds the message.
        folder_id: FolderId,
        /// Message to delete.
        message_id: MessageId,
        /// Remove it for good instead of moving it into the trash.
        permanent: bool,
    },
    /// Download a message again and write the chosen parts to disk.
    SaveAttachments {
        /// Folder that holds the message.
        folder_id: FolderId,
        /// Message the attachments belong to.
        message_id: MessageId,
        /// `(part_id, destination)` pairs to write.
        targets: Vec<(String, std::path::PathBuf)>,
    },
    /// Move a message into another folder on the server.
    MoveMessage {
        /// Folder that holds the message.
        folder_id: FolderId,
        /// Message to move.
        message_id: MessageId,
        /// Folder it is moved into.
        target_folder_id: FolderId,
    },
    /// Download one message and write it as a `.eml` file.
    SaveEml {
        /// Folder that holds the message.
        folder_id: FolderId,
        /// Message to write.
        message_id: MessageId,
        /// Destination path the user picked.
        path: std::path::PathBuf,
    },
    /// Submit a composed message through the outbox.
    Submit {
        /// Account the message is sent from.
        account_id: hylmail_core::models::AccountId,
        /// The draft as typed in the composer.
        draft: Box<Draft>,
        /// The signature selected in the composer.
        signature: Option<Signature>,
    },
    /// Run the OAuth2 sign-in in the browser and store the tokens.
    ///
    /// The account travels with the task instead of being looked up again: the
    /// user may have edited the form, and the sign-in has to use exactly the
    /// configuration that is on screen.
    OAuthLogin {
        /// Account whose OAuth2 configuration is used.
        account: Box<Account>,
    },
    /// Synchronise the address book from the account's CardDAV collection.
    SyncContacts {
        /// Account whose CardDAV configuration is used.
        account_id: hylmail_core::models::AccountId,
    },
    /// Drain the outgoing queue now (`FR-OUTBOX-3`).
    DrainOutbox,
}

/// What the reading pane is showing.
pub(crate) struct Reader {
    /// Header of the displayed message.
    pub header: MessageHeader,
    /// Cached body, once it has been downloaded.
    pub body: Option<MessageBody>,
    /// Sanitised HTML, ready for [`crate::html_view`].
    pub html: Option<SanitisedHtml>,
    /// The invitation, when the message carries one.
    pub calendar: Option<hylmail_core::calendar::CalendarSummary>,
    /// A body download is in flight.
    pub loading: bool,
    /// Why the last download failed.
    pub load_error: Option<String>,
    /// The user asked for the external images of this message.
    pub show_images: bool,
    /// Show the sanitised HTML source instead of the rendered body.
    pub show_source: bool,
    /// The rendered body, built once per sanitisation.
    pub document: Option<crate::html_view::Document>,
    /// Render in the colours the message declared, not the theme (`FR-READ-7`).
    pub original_colours: bool,
}

impl Reader {
    /// Wraps a header, before its body is known.
    #[must_use]
    pub fn new(header: MessageHeader) -> Self {
        Self {
            header,
            body: None,
            html: None,
            calendar: None,
            loading: false,
            load_error: None,
            show_images: false,
            show_source: false,
            document: None,
            original_colours: false,
        }
    }

    /// Re-sanitises the raw HTML under the current image policy.
    pub fn resanitise(&mut self, settings: &Settings) {
        // `FR-READ-7`: a message with enough images is a newsletter and keeps
        // the colours it declared; a simple message is colour-adapted for the
        // dark theme (unless the user turned the transformation off).
        let images = self.body.as_ref().map_or(0, |body| body.image_count);
        let newsletter = settings.newsletter_image_threshold > 0
            && images >= settings.newsletter_image_threshold;
        self.original_colours = newsletter || !settings.dark_mode_transform;
        let Some(body) = &self.body else { return };
        let Some(raw) = body.html_raw.as_deref() else {
            self.html = None;
            self.document = None;
            return;
        };
        let sender = self
            .header
            .from
            .first()
            .map(|address| address.email.as_str());
        let mut policy = HtmlPolicy::from_settings(settings, sender);
        if self.show_images {
            policy.allow_external_images = true;
        }
        let sanitised = sanitize(raw, &policy);
        self.document = Some(crate::html_view::render(&sanitised.html));
        self.html = Some(sanitised);
    }
}

/// Which kind of message the composer is writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposeMode {
    /// An empty message.
    New,
    /// An answer, with `all` selecting "Allen antworten".
    Reply {
        /// Reply to every recipient, not just the author.
        all: bool,
    },
    /// A forwarded message with the original quoted.
    Forward,
}

/// The composer window.
pub(crate) struct Composer {
    /// Account the message is sent from.
    pub account_id: hylmail_core::models::AccountId,
    /// Why the window is open.
    pub mode: ComposeMode,
    /// From address with display name.
    pub from: MailAddress,
    /// Raw `An` field.
    pub to: String,
    /// Raw `Cc` field.
    pub cc: String,
    /// Raw `Bcc` field.
    pub bcc: String,
    /// Subject line.
    pub subject: String,
    /// Plain text body.
    pub body: String,
    /// Rich HTML body, edited when [`Composer::html_mode`] is set.
    pub body_html: String,
    /// The composer is editing HTML (rich text) rather than plain text.
    pub html_mode: bool,
    /// The `Cc` row is visible.
    pub show_cc: bool,
    /// The `Bcc` row is visible.
    pub show_bcc: bool,
    /// Existing draft being edited, when it came from the drafts folder.
    pub draft_id: Option<DraftId>,
    /// `In-Reply-To` of the answered message.
    pub in_reply_to: Option<String>,
    /// `References` of the answered message.
    pub references: Vec<String>,
    /// Files staged for the attachment list.
    pub attachments: Vec<DraftAttachment>,
    /// Signature of the account, if any.
    pub signature: Option<Signature>,
    /// Result of the last send attempt.
    pub status: Option<String>,
    /// Reason the last send attempt was refused.
    pub error: Option<String>,
    /// A send is in flight.
    pub busy: bool,
}

impl Composer {
    /// An empty message for `account`.
    #[must_use]
    pub fn new(account: &Account, signature: Option<Signature>) -> Self {
        Self {
            account_id: account.id,
            mode: ComposeMode::New,
            from: MailAddress::with_name(account.email.clone(), account.display_name.clone()),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            body: String::new(),
            body_html: String::new(),
            html_mode: false,
            show_cc: false,
            show_bcc: false,
            draft_id: None,
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            signature,
            status: None,
            error: None,
            busy: false,
        }
    }

    /// An answer to `header`.
    #[must_use]
    pub fn reply(
        account: &Account,
        signature: Option<Signature>,
        header: &MessageHeader,
        body: Option<&str>,
        html: Option<&str>,
        all: bool,
    ) -> Self {
        let mut composer = Self::new(account, signature);
        composer.mode = ComposeMode::Reply { all };
        composer.subject = hylmail_core::compose::reply_subject(&header.subject);
        composer.to = header
            .from
            .iter()
            .map(|address| address.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if all {
            let mut extra: Vec<String> = Vec::new();
            for address in header.to.iter().chain(header.cc.iter()) {
                if address.email.eq_ignore_ascii_case(&account.email) {
                    continue;
                }
                let rendered = address.to_string();
                if rendered != composer.to && !extra.contains(&rendered) {
                    extra.push(rendered);
                }
            }
            composer.cc = extra.join(", ");
            composer.show_cc = !composer.cc.is_empty();
        }
        let context = hylmail_core::compose::ReplyContext::from_message(header, body, html);
        composer.in_reply_to = context.in_reply_to.clone();
        composer.references = context.references.clone();
        composer.body =
            hylmail_core::compose::quote_original(composer.body.as_str(), &context.attribution());
        if let Some(quoted) = context.quoted_plain.as_deref() {
            composer.body = format!("{}\n\n{}", composer.body.trim_end(), quoted);
        }
        composer
    }

    /// A forwarded copy of `header`.
    #[must_use]
    pub fn forward(
        account: &Account,
        signature: Option<Signature>,
        header: &MessageHeader,
        body: Option<&str>,
    ) -> Self {
        let mut composer = Self::new(account, signature);
        composer.mode = ComposeMode::Forward;
        composer.subject = hylmail_core::compose::forward_subject(&header.subject);
        composer.body = format!(
            "{}\n\n--- Weitergeleitete Nachricht ---\nVon: {}\nDatum: {}\nBetreff: {}\n\n{}",
            composer.body.trim_end(),
            header
                .from
                .iter()
                .map(|address| address.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            header
                .date
                .or(header.received_at)
                .map(|date| date.to_rfc3339())
                .unwrap_or_default(),
            header.subject,
            body.unwrap_or_default(),
        );
        composer
    }

    /// The draft the composer currently describes.
    #[must_use]
    pub fn to_draft(&self) -> Draft {
        let mut draft = Draft::new(self.account_id);
        draft.id = self.draft_id.unwrap_or_default();
        draft.to = parse_recipients(&self.to);
        draft.cc = parse_recipients(&self.cc);
        draft.bcc = parse_recipients(&self.bcc);
        draft.subject = self.subject.trim().to_string();
        draft.body_plain = self.body.clone();
        // The HTML body is left empty in plain mode, so the builder generates a
        // plain-text alternative from the rich body (and vice versa).
        draft.body_html = self.body_html.clone();
        draft.in_reply_to = self.in_reply_to.clone();
        draft.references = self.references.clone();
        draft.attachments = self.attachments.clone();
        draft.signature_id = self.signature.as_ref().map(|signature| signature.id);
        draft
    }

    /// Attaches a file that already exists on disk.
    ///
    /// # Errors
    ///
    /// [`hylmail_core::Error::Io`] when the file cannot be read.
    pub fn attach(&mut self, path: &std::path::Path) -> hylmail_core::Result<()> {
        let metadata = std::fs::metadata(path).map_err(|error| hylmail_core::Error::Io {
            path: path.to_path_buf(),
            source: error,
        })?;
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "Anhang".to_string());
        self.attachments.push(DraftAttachment {
            mime_type: mime_type_for(&filename).to_string(),
            filename,
            path: path.to_path_buf(),
            size_bytes: metadata.len(),
            inline: false,
        });
        Ok(())
    }

    /// Attaches a file as an inline image and returns its `cid:` address.
    ///
    /// The caller inserts `<img src="cid:…">` into the rich body and switches the
    /// editor to HTML mode (`FR-COMPOSE-3`).
    pub fn attach_inline(
        &mut self,
        path: &std::path::Path,
    ) -> hylmail_core::Result<Option<String>> {
        let metadata = std::fs::metadata(path).map_err(|error| hylmail_core::Error::Io {
            path: path.to_path_buf(),
            source: error,
        })?;
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "Bild".to_string());
        let content_id = hylmail_core::compose::inline_content_id(&filename);
        self.attachments.push(DraftAttachment {
            mime_type: mime_type_for(&filename).to_string(),
            filename,
            path: path.to_path_buf(),
            size_bytes: metadata.len(),
            inline: true,
        });
        Ok(content_id)
    }

    /// Appends an embedded image to the rich body and switches to HTML mode.
    pub fn embed_image(&mut self, content_id: &str, alt: &str) {
        self.body_html.push_str(&format!(
            "<img src=\"cid:{content_id}\" alt=\"{}\"><br>\n",
            alt.replace('"', "")
        ));
        self.html_mode = true;
    }
}

/// Splits a comma or semicolon separated address field.
///
/// Accepts both `name@host` and `Display Name <name@host>`.  Anything without
/// an `@` is dropped, because a half typed address must not reach the envelope.
#[must_use]
pub(crate) fn parse_recipients(input: &str) -> Vec<MailAddress> {
    crate::format::split_address_tokens(input)
        .into_iter()
        .filter_map(|token| parse_recipient(&token))
        .collect()
}

/// Parses one address in either of the two accepted spellings.
#[must_use]
pub(crate) fn parse_recipient(part: &str) -> Option<MailAddress> {
    let part = part.trim();
    if part.is_empty() {
        return None;
    }
    if let Some(start) = part.find('<') {
        let email = part[start + 1..].trim_end_matches('>').trim();
        if !looks_like_address(email) {
            return None;
        }
        let name = part[..start].trim().trim_matches('"').trim();
        return Some(if name.is_empty() {
            MailAddress::new(email)
        } else {
            MailAddress::with_name(email, name)
        });
    }
    if looks_like_address(part) {
        return Some(MailAddress::new(part));
    }
    None
}

/// Whether `text` can plausibly be a bare `local@domain` address.
fn looks_like_address(text: &str) -> bool {
    if text.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = text.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => !local.is_empty() && !domain.is_empty(),
        _ => false,
    }
}

/// The vault slots a password typed into the account dialog fills.
///
/// Most providers use one application password for both directions, so an
/// IMAP or Exchange account gets the same secret in both slots.  GroupWise
/// proxies authenticate separately, and an OAuth2 account has no password slot
/// at all.
#[must_use]
fn credential_slots(account: &Account) -> Vec<CredentialKind> {
    match account.kind {
        AccountKind::GroupWiseProxy => vec![CredentialKind::ProxyPassword],
        _ if account.auth == AuthKind::OAuth2 => Vec::new(),
        _ => vec![
            CredentialKind::IncomingPassword,
            CredentialKind::OutgoingPassword,
        ],
    }
}

/// A file extension based media type, good enough for the `Content-Type` line.
#[must_use]
pub(crate) fn mime_type_for(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ics" => "text/calendar",
        _ => "application/octet-stream",
    }
}

/// The settings window.
pub(crate) struct SettingsForm {
    /// Working copy, written back only when `Speichern` is pressed.
    pub settings: Settings,
    /// Index of the signature currently being edited.
    pub signature_index: Option<usize>,
    /// Name field of the "new signature" row.
    pub new_signature_name: String,
    /// Account the new signature is created for; `None` picks the first one.
    pub new_signature_account: Option<AccountId>,
    /// Editable text of the tracking-domain field.
    pub tracking_domains: String,
    /// Confirmation of the last save.
    pub status: Option<String>,
    /// Why the last save failed.
    pub error: Option<String>,
}

impl SettingsForm {
    /// Opens the window on a snapshot of the live settings.
    #[must_use]
    pub fn new(settings: &Settings) -> Self {
        Self {
            settings: settings.clone(),
            signature_index: None,
            new_signature_name: String::new(),
            new_signature_account: None,
            tracking_domains: settings.blocked_tracking_domains.join(", "),
            status: None,
            error: None,
        }
    }
}

/// The German name of a folder role, used when no such folder exists yet.
fn kind_label(kind: FolderKind) -> &'static str {
    match kind {
        FolderKind::Inbox => "Posteingang",
        FolderKind::Sent => "Gesendet",
        FolderKind::Drafts => "Entwürfe",
        FolderKind::Trash => "Papierkorb",
        FolderKind::Junk => "Spam",
        FolderKind::Archive => "Archiv",
        FolderKind::Outbox => "Postausgang",
        FolderKind::VirtualAllInboxes => "Alle Posteingänge",
        FolderKind::Custom => "Ordner",
    }
}

/// The folder picker behind "Verschieben".
///
/// Only the filter is state; the list itself is derived from the folder tree on
/// every frame, so a synchronisation that adds a folder shows up immediately.
pub(crate) struct MovePicker {
    /// Folder the message is in; the destination list excludes it.
    pub folder_id: FolderId,
    /// The filter box.
    pub filter: String,
}

impl MovePicker {
    /// A picker for a message in `folder_id`.
    #[must_use]
    pub fn new(folder_id: FolderId) -> Self {
        Self {
            folder_id,
            filter: String::new(),
        }
    }

    /// `true` when `folder` passes a filter box.
    #[must_use]
    pub fn passes(filter: &str, folder: &Folder) -> bool {
        let needle = filter.trim().to_lowercase();
        needle.is_empty()
            || folder.name.to_lowercase().contains(&needle)
            || folder.path.to_lowercase().contains(&needle)
    }
}

/// The accounts window.
pub(crate) struct AccountsForm {
    /// Working copy of `accounts.json`.
    pub document: AccountsDocument,
    /// Index of the account in the detail pane.
    pub selected: usize,
    /// Index into [`hylmail_core::models::presets::all`] chosen in the form.
    pub preset: usize,
    /// Value typed into the password field.
    pub password: String,
    /// Confirmation of the last action.
    pub status: Option<String>,
    /// Why the last action failed.
    pub error: Option<String>,
}

impl AccountsForm {
    /// Opens the window on a snapshot of `accounts.json`.
    #[must_use]
    pub fn new(document: &AccountsDocument) -> Self {
        Self {
            document: document.clone(),
            selected: 0,
            preset: 0,
            password: String::new(),
            status: None,
            error: None,
        }
    }

    /// The account currently shown in the detail pane.
    #[must_use]
    pub fn current(&self) -> Option<&Account> {
        self.document.accounts.get(self.selected)
    }
}

/// The rule editor window.
pub(crate) struct RulesForm {
    /// Working copy of `rules.json`.
    pub rules: RuleSet,
    /// The rule currently open in the detail pane.
    pub selected: Option<RuleId>,
    /// Value typed into the "new rule" box.
    pub new_rule_name: String,
    /// Confirmation of the last save.
    pub status: Option<String>,
    /// Why the last action failed.
    pub error: Option<String>,
}

impl RulesForm {
    /// Opens the editor on a snapshot of `rules.json`.
    #[must_use]
    pub fn new(rules: &RuleSet) -> Self {
        Self {
            rules: rules.clone(),
            selected: rules.rules.first().map(|rule| rule.id),
            new_rule_name: String::new(),
            status: None,
            error: None,
        }
    }
}

/// The address book window.
pub(crate) struct ContactsForm {
    /// Every contact, in display order.
    pub contacts: Vec<Contact>,
    /// The contact open in the detail pane.
    pub selected: usize,
    /// The filter box.
    pub filter: String,
    /// Name field of the "new contact" row.
    pub new_name: String,
    /// Address field of the "new contact" row.
    pub new_email: String,
    /// Confirmation of the last action.
    pub status: Option<String>,
    /// Why the last action failed.
    pub error: Option<String>,
}

impl ContactsForm {
    /// Opens the address book on the loaded contact list.
    #[must_use]
    pub fn new(contacts: Vec<Contact>) -> Self {
        Self {
            contacts,
            selected: 0,
            filter: String::new(),
            new_name: String::new(),
            new_email: String::new(),
            status: None,
            error: None,
        }
    }

    /// The indices of the contacts that pass the filter box.
    #[must_use]
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        self.contacts
            .iter()
            .enumerate()
            .filter(|(_, contact)| {
                needle.is_empty()
                    || contact.label().to_lowercase().contains(&needle)
                    || contact
                        .primary_email()
                        .is_some_and(|email| email.to_lowercase().contains(&needle))
            })
            .map(|(index, _)| index)
            .collect()
    }
}

/// The outgoing-queue window (`FR-OUTBOX-3`).
pub(crate) struct OutboxForm {
    /// Working copy of `outbox.json`.
    pub document: hylmail_core::store::OutboxDocument,
    /// Confirmation of the last action.
    pub status: Option<String>,
}

/// Everything the interface draws from.
///
/// One value per process; the worker mutates it through [`UiState::apply`].
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct UiState {
    /// Accounts in the order they appear in `accounts.json`.
    pub accounts: Vec<Account>,
    /// Every folder of every account, flat.
    pub folders: Vec<Folder>,
    /// Accounts whose folder tree is collapsed.
    pub collapsed: HashSet<AccountId>,
    /// Account the toolbar actions apply to.
    pub active_account: Option<AccountId>,
    /// Folder whose messages are shown.
    pub selected_folder: Option<FolderId>,
    /// The "Alle Posteing\u00e4nge" aggregate view is active.
    pub all_inboxes: bool,
    /// The loaded page of the message list.
    pub messages: Vec<MessageHeader>,
    /// How many messages the query matches in total.
    pub total: u64,
    /// The selected row.
    pub selected: Option<MessageId>,
    /// The messages the bulk actions apply to, as checked in the list.
    ///
    /// Empty means "no checkbox is ticked", and a bulk action then falls back
    /// to [`UiState::selected`], so the toolbar behaves as it did before bulk
    /// selection existed.
    pub selected_ids: BTreeSet<MessageId>,
    /// How many rows of the query have been loaded.
    pub loaded: u32,
    /// Rows per page.
    pub page_size: u32,
    /// Sort column.
    pub sort: SortKey,
    /// Sort direction; `false` is the natural direction of the column.
    pub ascending: bool,
    /// Full text search box.
    pub search: String,
    /// Minimum characters before the search reaches the index, local and server
    /// (`FR-SEARCH-1/2`).
    pub search_min_chars: usize,
    /// When the debounced server-side search is due (`FR-SEARCH-2`).
    pub search_due: Option<std::time::Instant>,
    /// The search box asked for the keyboard focus.
    pub focus_search: bool,
    /// "Nur ungelesene" filter.
    pub unread_only: bool,
    /// "Nur markierte" filter.
    pub flagged_only: bool,
    /// "Nur mit Anhang" filter.
    pub attachment_only: bool,
    /// The reading pane (the lower half of the main view) is shown.
    pub preview_visible: bool,
    /// The reading pane.
    pub reader: Option<Reader>,
    /// One line of feedback for the status bar.
    pub status: String,
    /// The last error, shown until the next action.
    pub error: Option<String>,
    /// Per account connection state, as reported by the engine.
    pub connections: BTreeMap<AccountId, String>,
    /// Unread messages that arrived since the last refresh.
    pub new_mail: u32,
    /// A sync event stored to the database; the list is re-queried once per
    /// frame instead of once per event.
    pub sync_dirty: bool,
    /// The composer window.
    pub composer: Option<Composer>,
    /// The settings window.
    pub settings_form: Option<SettingsForm>,
    /// The accounts window.
    pub accounts_form: Option<AccountsForm>,
    /// The rule editor.
    pub rules_form: Option<RulesForm>,
    /// The address book.
    pub contacts_form: Option<ContactsForm>,
    /// The outgoing queue.
    pub outbox_form: Option<OutboxForm>,
    /// The message list is grouped into conversations.
    pub threaded: bool,
    /// The conversation summaries of the active account.
    pub conversations: Vec<hylmail_core::db::conversations::Conversation>,
    /// Conversations the user expanded.
    pub expanded_threads: HashSet<String>,
    /// The address book, cached for the composer's autocomplete.
    pub contacts: Vec<Contact>,
    /// Messages the rule engine has already seen, so arrival rules run once per
    /// message (`FR-RULE-1`).
    pub processed: HashSet<MessageId>,
    /// Decoded remote images of this session, keyed by URL (`FR-READ-5`).
    pub image_cache: std::collections::HashMap<String, hylmail_core::image::DecodedImage>,
    /// Bytes held by [`UiState::image_cache`], against the configured cap.
    pub image_cache_bytes: usize,
    /// Textures uploaded for the cached images, keyed by URL.
    pub textures: std::collections::HashMap<String, egui::TextureHandle>,
    /// The folder picker of "Verschieben".
    pub picker: Option<MovePicker>,
    /// A delete the confirmation prompt is holding back (`FR-FLAG-4`).
    pub pending_delete: Vec<MessageId>,
    /// A `.eml` file waiting to be opened.
    pub pending_eml: Vec<PathBuf>,
    /// Set by [`Action::Quit`].
    pub quit: bool,
}

impl UiState {
    /// An empty state with the given page size.
    #[must_use]
    pub fn new(page_size: u32) -> Self {
        Self {
            accounts: Vec::new(),
            folders: Vec::new(),
            collapsed: HashSet::new(),
            active_account: None,
            selected_folder: None,
            all_inboxes: true,
            messages: Vec::new(),
            total: 0,
            selected: None,
            selected_ids: BTreeSet::new(),
            loaded: 0,
            page_size: page_size.max(1),
            sort: SortKey::default(),
            ascending: false,
            search: String::new(),
            search_min_chars: DEFAULT_SERVER_SEARCH_MIN_CHARS,
            search_due: None,
            focus_search: false,
            unread_only: false,
            flagged_only: false,
            attachment_only: false,
            preview_visible: true,
            reader: None,
            status: String::new(),
            error: None,
            connections: BTreeMap::new(),
            new_mail: 0,
            sync_dirty: false,
            composer: None,
            settings_form: None,
            accounts_form: None,
            rules_form: None,
            contacts_form: None,
            outbox_form: None,
            threaded: false,
            conversations: Vec::new(),
            expanded_threads: HashSet::new(),
            contacts: Vec::new(),
            processed: HashSet::new(),
            image_cache: std::collections::HashMap::new(),
            image_cache_bytes: 0,
            textures: std::collections::HashMap::new(),
            picker: None,
            pending_delete: Vec::new(),
            pending_eml: Vec::new(),
            quit: false,
        }
    }

    /// Reads accounts, folders and the first page of the list.
    ///
    /// # Errors
    ///
    /// Every configuration and database error of the first load.
    pub fn load(&mut self, core: &Core) -> hylmail_core::Result<()> {
        self.accounts = core.config().load_accounts()?.accounts;
        self.reload_folders(core)?;
        self.apply_folder_overrides(core);
        self.reload_contacts(core);
        self.seed_processed(core);
        // `FR-UNIFIED-3`: the aggregate inbox is the default start view; when the
        // setting is off, the first account's inbox is shown instead.
        if !core.settings().show_all_inboxes_on_start {
            if let Some(account) = self.accounts.first().map(|account| account.id) {
                return self.select_all_inboxes(core, Some(account));
            }
        }
        self.refresh(core)
    }

    /// Marks every already-stored message as rule-processed.
    ///
    /// The arrival rule engine must only touch mail that arrives *now*, so the
    /// mailbox present at startup is seeded once; anything not in the set is a
    /// message a later sync stored.
    fn seed_processed(&mut self, core: &Core) {
        match core.database().read(db_messages::all_ids) {
            Ok(ids) => self.processed.extend(ids),
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    /// Re-reads every folder of every account.
    ///
    /// # Errors
    ///
    /// [`hylmail_core::Error::Database`] when the folder table cannot be read.
    pub fn reload_folders(&mut self, core: &Core) -> hylmail_core::Result<()> {
        self.folders = core.database().read(db_folders::list_all_folders)?;
        Ok(())
    }

    /// Pushes the manual special-folder overrides into the database and re-reads
    /// the tree (`FR-FOLDER-3`).
    fn apply_folder_overrides(&mut self, core: &Core) {
        let overrides = core.settings().folder_overrides.clone();
        if overrides.is_empty() {
            return;
        }
        match core
            .database()
            .write(|connection| db_folders::apply_overrides(connection, &overrides))
        {
            Ok(_) => {
                if let Err(error) = self.reload_folders(core) {
                    self.error = Some(error.to_string());
                }
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    /// The account with `id`, if it is known.
    #[must_use]
    pub fn account(&self, id: AccountId) -> Option<&Account> {
        self.accounts.iter().find(|account| account.id == id)
    }

    /// The account a new message is written for.
    #[must_use]
    pub fn default_account(&self) -> Option<&Account> {
        self.active_account
            .and_then(|id| self.account(id))
            .or_else(|| self.accounts.first())
    }

    /// The folders the current view queries, empty for "no folder filter".
    fn view_folder_ids(&self) -> Vec<FolderId> {
        if let Some(id) = self.selected_folder {
            return vec![id];
        }
        if self.all_inboxes {
            return self
                .folders
                .iter()
                .filter(|folder| folder.kind == FolderKind::Inbox)
                .map(|folder| folder.id)
                .collect();
        }
        Vec::new()
    }

    /// The database query behind the message list.
    #[must_use]
    pub fn build_query(&self) -> MessageQuery {
        let mut query = MessageQuery {
            unread_only: self.unread_only,
            flagged_only: self.flagged_only,
            has_attachment: self.attachment_only,
            limit: Some(self.page_size),
            offset: 0,
            ..MessageQuery::default()
        };
        let ids = self.view_folder_ids();
        if ids.is_empty() {
            query.account_id = self.active_account;
        } else {
            query.folder_ids = ids;
        }
        let term = self.search.trim();
        if term.chars().count() >= self.search_min_chars {
            query.text = Some(term.to_string());
            // Exchange and the GroupWise proxy cannot search on the server, so
            // the search filters the local cache only; for the proxy that is at
            // most the most recent window (`FR-SEARCH-4`, `BR-GW-3`).
            if self.active_account_is_groupwise() {
                query.window = Some(GROUPWISE_SEARCH_WINDOW);
            }
        }
        query
    }

    /// `true` when the account whose mail is shown is a GroupWise proxy.
    #[must_use]
    fn active_account_is_groupwise(&self) -> bool {
        self.active_account
            .and_then(|id| self.account(id))
            .is_some_and(|account| account.kind == AccountKind::GroupWiseProxy)
    }

    /// Sorts the loaded page in place.
    ///
    /// Only the loaded page is ordered; the query itself always returns the
    /// newest first, so a folder larger than one page is sorted by date.
    pub fn apply_sort(&mut self) {
        let key = self.sort;
        let ascending = self.ascending;
        self.messages.sort_by(|left, right| {
            let ordering = match key {
                SortKey::Date => date_key(left).cmp(&date_key(right)),
                SortKey::From => sender_label(left).cmp(&sender_label(right)),
                SortKey::Subject => left
                    .subject
                    .to_lowercase()
                    .cmp(&right.subject.to_lowercase()),
                SortKey::Unread => left.is_unread().cmp(&right.is_unread()),
                SortKey::Flagged => left.is_flagged().cmp(&right.is_flagged()),
                SortKey::Attachment => left.has_attachments.cmp(&right.has_attachments),
            };
            let ordering = ordering.then_with(|| date_key(left).cmp(&date_key(right)));
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        });
    }

    /// Re-runs the query and replaces the loaded page.
    ///
    /// # Errors
    ///
    /// [`hylmail_core::Error::Database`] when the list cannot be read.
    pub fn refresh(&mut self, core: &Core) -> hylmail_core::Result<()> {
        let query = self.build_query();
        let (messages, total) = core.database().read(|connection| {
            let messages = db_messages::list_messages(connection, &query)?;
            let total = db_messages::count_messages(connection, &query)?;
            Ok((messages, total))
        })?;
        self.messages = messages;
        self.total = total;
        self.loaded = self.messages.len() as u32;
        self.apply_sort();
        self.reload_conversations(core)?;
        Ok(())
    }

    /// Like [`UiState::refresh`], but keeps the rows the user already loaded.
    ///
    /// A sync that runs while the user is reading must not collapse a list they
    /// paged past the first page (`FR-PAGE-1`), so the loaded window is
    /// re-queried instead of only the first page.
    fn refresh_keeping_window(&mut self, core: &Core) -> hylmail_core::Result<()> {
        let mut query = self.build_query();
        query.limit = Some(self.loaded.max(self.page_size));
        let (messages, total) = core.database().read(|connection| {
            let messages = db_messages::list_messages(connection, &query)?;
            let total = db_messages::count_messages(connection, &query)?;
            Ok((messages, total))
        })?;
        self.messages = messages;
        self.total = total;
        self.loaded = self.messages.len() as u32;
        self.apply_sort();
        self.reload_conversations(core)?;
        Ok(())
    }

    /// The address book, loaded for the composer's autocomplete.
    ///
    /// A failure is not fatal: a mail client without an address book still
    /// sends mail, so the error only lands in the status bar.
    fn reload_contacts(&mut self, core: &Core) {
        match core
            .database()
            .read(|connection| db_contacts::list_contacts(connection, None))
        {
            Ok(contacts) => self.contacts = contacts,
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    /// Rebuilds and loads the conversation summaries of the active account.
    ///
    /// The `conversations` table is a pure cache (see
    /// [`hylmail_core::db::conversations`]); it is recomputed whenever the view
    /// changes so the collapsed thread shows the counts the messages now have.
    fn reload_conversations(&mut self, core: &Core) -> hylmail_core::Result<()> {
        self.conversations.clear();
        if !self.threaded {
            return Ok(());
        }
        let Some(account) = self.active_account else {
            return Ok(());
        };
        let limit = self.page_size;
        self.conversations = core.database().write(|connection| {
            db_conversations::rebuild_for_account(connection, account)?;
            db_conversations::list_conversations(connection, account, limit)
        })?;
        Ok(())
    }

    /// Arms the debounced server-side search (`FR-SEARCH-2/3`).
    ///
    /// `force` skips the delay, which is what `Enter` does.  A query shorter
    /// than the configured minimum never leaves the machine.
    fn arm_server_search(&mut self, core: &Core, force: bool) {
        let settings = core.settings();
        if self.search.trim().chars().count() < self.search_min_chars {
            self.search_due = None;
            return;
        }
        let delay = if force {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_millis(settings.server_search_delay_ms)
        };
        self.search_due = Some(std::time::Instant::now() + delay);
    }

    /// The server-side search task, once the debounce has elapsed.
    #[must_use]
    pub fn due_server_search(&mut self) -> Option<Task> {
        let due = self.search_due?;
        if std::time::Instant::now() < due {
            return None;
        }
        self.search_due = None;
        let query = self.search.trim().to_string();
        if query.is_empty() {
            return None;
        }
        Some(Task::ServerSearch { query })
    }

    /// Appends the next page to the list.
    ///
    /// # Errors
    ///
    /// [`hylmail_core::Error::Database`] when the list cannot be read.
    pub fn load_more(&mut self, core: &Core) -> hylmail_core::Result<()> {
        if self.loaded as u64 >= self.total {
            return Ok(());
        }
        let mut query = self.build_query();
        query.offset = self.loaded;
        let more = core
            .database()
            .read(|connection| db_messages::list_messages(connection, &query))?;
        if more.is_empty() {
            self.total = self.loaded as u64;
            return Ok(());
        }
        self.messages.extend(more);
        self.loaded = self.messages.len() as u32;
        self.apply_sort();
        Ok(())
    }
}

/// The unread messages of every inbox of `folders`.
#[must_use]
pub(crate) fn unread_total(folders: &[Folder]) -> u32 {
    folders
        .iter()
        .filter(|folder| folder.kind == FolderKind::Inbox)
        .map(|folder| folder.unread_count)
        .sum()
}

/// The sidebar of one account as `(depth, folder)` rows.
///
/// Parents always come before their children, and a folder whose parent
/// disappeared is promoted to the root instead of vanishing.
#[must_use]
pub(crate) fn folder_rows(folders: &[Folder], account_id: AccountId) -> Vec<(usize, FolderId)> {
    let mine: Vec<&Folder> = folders
        .iter()
        .filter(|folder| folder.account_id == account_id)
        .collect();
    let mut rows = Vec::new();
    let mut visited = HashSet::new();
    let ids: HashSet<FolderId> = mine.iter().map(|folder| folder.id).collect();
    push_children(&mine, None, 0, &mut rows, &mut visited);
    for folder in &mine {
        let orphan = folder.parent_id.is_none_or(|parent| !ids.contains(&parent));
        if orphan && visited.insert(folder.id) {
            rows.push((0, folder.id));
            push_children(&mine, Some(folder.id), 1, &mut rows, &mut visited);
        }
    }
    for folder in &mine {
        if visited.insert(folder.id) {
            rows.push((0, folder.id));
        }
    }
    rows
}

/// Appends the children of `parent` to `rows`, parents first.
fn push_children(
    folders: &[&Folder],
    parent: Option<FolderId>,
    depth: usize,
    rows: &mut Vec<(usize, FolderId)>,
    visited: &mut HashSet<FolderId>,
) {
    let mut children: Vec<&&Folder> = folders
        .iter()
        .filter(|folder| folder.parent_id == parent)
        .collect();
    children.sort_by(|left, right| {
        left.sort_order
            .cmp(&right.sort_order)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    for child in children {
        if !visited.insert(child.id) {
            continue;
        }
        rows.push((depth, child.id));
        push_children(folders, Some(child.id), depth + 1, rows, visited);
    }
}

/// The sort key of a message's timestamp.
fn date_key(header: &MessageHeader) -> i64 {
    header
        .date
        .or(header.received_at)
        .map(|moment| moment.timestamp_millis())
        .unwrap_or(0)
}

/// The `Von` label of a message.
fn sender_label(header: &MessageHeader) -> String {
    header
        .from
        .first()
        .map(|address| match &address.name {
            Some(name) if !name.trim().is_empty() => name.to_lowercase(),
            _ => address.email.to_lowercase(),
        })
        .unwrap_or_default()
}

/// The invitation a body carries, if any.
fn calendar_of(body: &MessageBody) -> Option<hylmail_core::calendar::CalendarSummary> {
    body.calendar_ics
        .as_deref()
        .and_then(|ics| hylmail_core::calendar::parse(ics).ok())
}

impl UiState {
    /// Selects a folder and reloads the list.
    ///
    /// # Errors
    ///
    /// [`hylmail_core::Error::Database`] when the list cannot be read.
    pub fn select_folder(&mut self, core: &Core, id: FolderId) -> hylmail_core::Result<()> {
        self.selected_folder = Some(id);
        self.all_inboxes = false;
        if let Some(folder) = self.folders.iter().find(|folder| folder.id == id) {
            self.active_account = Some(folder.account_id);
        }
        self.selected = None;
        self.selected_ids.clear();
        self.reader = None;
        self.new_mail = 0;
        self.refresh(core)
    }

    /// Selects the aggregate "Alle Posteingänge" view.
    ///
    /// # Errors
    ///
    /// [`hylmail_core::Error::Database`] when the list cannot be read.
    pub fn select_all_inboxes(
        &mut self,
        core: &Core,
        account: Option<AccountId>,
    ) -> hylmail_core::Result<()> {
        self.all_inboxes = true;
        self.selected_folder = None;
        self.active_account = account;
        self.selected = None;
        self.selected_ids.clear();
        self.reader = None;
        self.new_mail = 0;
        self.refresh(core)
    }

    /// Selects a message and starts the body download when needed.
    pub fn select_message(&mut self, core: &Core, id: MessageId) -> Vec<Task> {
        let Some(header) = self.messages.iter().find(|header| header.id == id).cloned() else {
            return Vec::new();
        };
        self.selected = Some(id);
        let mut tasks = Vec::new();
        let folder_id = header.folder_id;
        let mut reader = Reader::new(header);
        match core
            .database()
            .read(|connection| content::get_content(connection, id))
        {
            Ok(Some(body)) => {
                reader.calendar = calendar_of(&body);
                reader.body = Some(body);
                reader.resanitise(core.settings());
            }
            Ok(None) => {
                reader.loading = true;
                tasks.push(Task::FetchBody {
                    folder_id,
                    message_id: id,
                });
            }
            Err(error) => reader.load_error = Some(error.to_string()),
        }
        let unread = reader.header.is_unread();
        self.reader = Some(reader);
        if unread {
            tasks.extend(self.change_flags(core, id, MessageFlags::SEEN, true));
        }
        tasks
    }

    /// Moves the selection by `delta` rows.
    pub fn step_selection(&mut self, core: &Core, delta: isize) -> Vec<Task> {
        if self.messages.is_empty() {
            return Vec::new();
        }
        let current = self
            .selected
            .and_then(|id| self.messages.iter().position(|header| header.id == id));
        let next = match current {
            Some(index) => (index as isize + delta).clamp(0, self.messages.len() as isize - 1),
            None if delta >= 0 => 0,
            None => self.messages.len() as isize - 1,
        } as usize;
        let id = self.messages[next].id;
        self.select_message(core, id)
    }

    /// Applies a flag change locally and schedules the server push.
    fn change_flags(
        &mut self,
        core: &Core,
        id: MessageId,
        flag: MessageFlags,
        on: bool,
    ) -> Vec<Task> {
        let mut updated: Option<MessageHeader> = None;
        for header in &mut self.messages {
            if header.id == id {
                if on {
                    header.flags.insert(flag);
                } else {
                    header.flags.remove(flag);
                }
                updated = Some(header.clone());
            }
        }
        if let Some(reader) = self.reader.as_mut() {
            if reader.header.id == id {
                if on {
                    reader.header.flags.insert(flag);
                } else {
                    reader.header.flags.remove(flag);
                }
                updated = Some(reader.header.clone());
            }
        }
        let Some(header) = updated else {
            return Vec::new();
        };
        let flags = header.flags;
        let folder_id = header.folder_id;
        let result = core.database().write(|connection| {
            db_messages::set_flags(connection, id, flags)?;
            db_messages::refresh_folder_counts(connection, folder_id)
        });
        match result {
            Ok((unread, total)) => {
                self.set_counts(folder_id, unread, total);
                vec![Task::PushFlags { message_id: id }]
            }
            Err(error) => {
                self.error = Some(error.to_string());
                Vec::new()
            }
        }
    }

    /// Marks every loaded message as read.
    pub fn mark_all_read(&mut self, core: &Core) -> Vec<Task> {
        let ids: Vec<MessageId> = self
            .messages
            .iter()
            .filter(|header| header.is_unread())
            .map(|header| header.id)
            .collect();
        let mut tasks = Vec::new();
        for id in ids {
            tasks.extend(self.change_flags(core, id, MessageFlags::SEEN, true));
        }
        if !tasks.is_empty() {
            self.status = "Alle Nachrichten als gelesen markiert".to_string();
        }
        tasks
    }

    /// Opens an empty composer for the active account.
    pub fn open_composer(&mut self, core: &Core) -> Vec<Task> {
        let account = self.default_account().cloned();
        let Some(account) = account else {
            self.error = Some("Es ist noch kein Konto eingerichtet".to_string());
            return Vec::new();
        };
        let signature = core.settings().default_signature(account.id).cloned();
        self.composer = Some(Composer::new(&account, signature));
        Vec::new()
    }

    /// Opens a composer answering the displayed message.
    pub fn reply(&mut self, core: &Core, all: bool) -> Vec<Task> {
        let Some(reader) = self.reader.as_ref() else {
            self.error = Some("Es ist keine Nachricht ausgewählt".to_string());
            return Vec::new();
        };
        let account = self.account(reader.header.account_id).cloned();
        let Some(account) = account else {
            self.error = Some("Das Konto der Nachricht ist nicht mehr eingerichtet".to_string());
            return Vec::new();
        };
        let signature = core.settings().default_signature(account.id).cloned();
        let plain = reader
            .body
            .as_ref()
            .and_then(|body| body.plain_text.as_deref());
        let html = reader
            .body
            .as_ref()
            .and_then(|body| body.html_raw.as_deref());
        self.composer = Some(Composer::reply(
            &account,
            signature,
            &reader.header,
            plain,
            html,
            all,
        ));
        Vec::new()
    }

    /// Opens a composer forwarding the displayed message.
    pub fn forward(&mut self, core: &Core) -> Vec<Task> {
        let Some(reader) = self.reader.as_ref() else {
            self.error = Some("Es ist keine Nachricht ausgewählt".to_string());
            return Vec::new();
        };
        let account = self.account(reader.header.account_id).cloned();
        let Some(account) = account else {
            self.error = Some("Das Konto der Nachricht ist nicht mehr eingerichtet".to_string());
            return Vec::new();
        };
        let signature = core.settings().default_signature(account.id).cloned();
        let plain = reader
            .body
            .as_ref()
            .and_then(|body| body.plain_text.as_deref());
        self.composer = Some(Composer::forward(
            &account,
            signature,
            &reader.header,
            plain,
        ));
        Vec::new()
    }

    /// Saves the composer into the local drafts (`FR-COMPOSE-7`).
    ///
    /// An empty composer is not stored: a draft the user never typed into would
    /// only clutter `drafts.json`.  The draft keeps one stable id for the whole
    /// editing session, so saving twice updates the same entry.
    fn save_draft(&mut self, core: &Core) {
        if self.composer.is_none() {
            return;
        }
        // A stable id for the whole editing session, so saving twice updates the
        // same entry instead of creating a new one each time.
        if self
            .composer
            .as_ref()
            .is_some_and(|composer| composer.draft_id.is_none())
        {
            if let Some(composer) = self.composer.as_mut() {
                composer.draft_id = Some(DraftId::new());
            }
        }
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        let draft = composer.to_draft();
        let empty = draft.to.is_empty()
            && draft.cc.is_empty()
            && draft.bcc.is_empty()
            && draft.subject.trim().is_empty()
            && draft.body_plain.trim().is_empty()
            && draft.attachments.is_empty();
        if empty {
            return;
        }
        let store = core.config();
        let mut document = match store.load_drafts() {
            Ok(document) => document,
            Err(error) => {
                if let Some(composer) = self.composer.as_mut() {
                    composer.error = Some(error.to_string());
                }
                return;
            }
        };
        match document
            .drafts
            .iter_mut()
            .find(|existing| existing.id == draft.id)
        {
            Some(existing) => *existing = draft.clone(),
            None => document.drafts.push(draft.clone()),
        }
        match store.save_drafts(&document) {
            Ok(()) => {
                if let Some(composer) = self.composer.as_mut() {
                    composer.status = Some("Entwurf gespeichert.".to_string());
                    composer.error = None;
                }
                self.status = "Entwurf gespeichert".to_string();
            }
            Err(error) => {
                if let Some(composer) = self.composer.as_mut() {
                    composer.error = Some(error.to_string());
                }
            }
        }
    }

    /// Validates the composer and hands the message to the worker.
    pub fn submit_composer(&mut self, core: &Core) -> Vec<Task> {
        let draft = {
            let Some(composer) = self.composer.as_mut() else {
                return Vec::new();
            };
            composer.error = None;
            composer.status = None;
            let draft = composer.to_draft();
            if let Some(blocker) = hylmail_core::compose::send_blocker(&draft) {
                composer.error = Some(blocker.to_string());
                return Vec::new();
            }
            composer.busy = true;
            draft
        };
        let signature = self
            .composer
            .as_ref()
            .and_then(|composer| composer.signature.clone());
        // Recipients of sent mail supplement the address book (`FR-ADDR-3`).
        self.collect_recipients(core, &draft);
        Vec::from([Task::Submit {
            account_id: draft.account_id,
            draft: Box::new(draft),
            signature,
        }])
    }

    /// Opens a `.eml` file in the reading pane.
    pub fn open_eml(&mut self, core: &Core, path: &Path) -> Vec<Task> {
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(error) => {
                self.error = Some(format!("{}: {error}", path.display()));
                return Vec::new();
            }
        };
        let account_id = self
            .accounts
            .first()
            .map(|account| account.id)
            .unwrap_or_default();
        let policy = HtmlPolicy::from_settings(core.settings(), None);
        let parsed = match hylmail_core::mime::parse(&raw, MessageId::new(), &policy) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.error = Some(error.to_string());
                return Vec::new();
            }
        };
        let mut header = parsed.headers.to_header(account_id, FolderId::new());
        header.id = parsed.body.message_id;
        header.snippet = Some(parsed.snippet.clone());
        header.has_attachments = parsed.has_attachments;
        header.is_calendar_invite = parsed.is_calendar_invite;
        let mut reader = Reader::new(header);
        reader.calendar = calendar_of(&parsed.body);
        reader.body = Some(parsed.body);
        reader.resanitise(core.settings());
        self.reader = Some(reader);
        self.status = format!("{} geöffnet", path.display());
        Vec::new()
    }

    /// Interprets one action.
    pub fn handle(&mut self, action: Action, core: &mut Core) -> Vec<Task> {
        match action {
            Action::SelectFolder(id) => self.run(core, |state| state.select_folder(core, id)),
            Action::SelectAccount(account) => {
                self.run(core, |state| state.select_all_inboxes(core, account))
            }
            Action::ToggleAccount(id) => {
                if !self.collapsed.remove(&id) {
                    self.collapsed.insert(id);
                }
                Vec::new()
            }
            Action::SelectMessage(id) => self.select_message(core, id),
            Action::NextMessage => self.step_selection(core, 1),
            Action::PreviousMessage => self.step_selection(core, -1),
            Action::Search(text) => {
                self.search = text;
                self.arm_server_search(core, false);
                self.run(core, |state| state.refresh(core))
            }
            Action::ForceSearch(text) => {
                self.search = text;
                self.arm_server_search(core, true);
                self.run(core, |state| state.refresh(core))
            }
            Action::ClearSearch => {
                self.search.clear();
                self.search_due = None;
                self.run(core, |state| state.refresh(core))
            }
            Action::FocusSearch => {
                self.focus_search = true;
                Vec::new()
            }
            Action::ToggleUnreadOnly => {
                self.unread_only = !self.unread_only;
                self.run(core, |state| state.refresh(core))
            }
            Action::ToggleFlaggedOnly => {
                self.flagged_only = !self.flagged_only;
                self.run(core, |state| state.refresh(core))
            }
            Action::ToggleAttachmentOnly => {
                self.attachment_only = !self.attachment_only;
                self.run(core, |state| state.refresh(core))
            }
            Action::SortBy(key) => {
                if self.sort == key {
                    self.ascending = !self.ascending;
                } else {
                    self.sort = key;
                    self.ascending = false;
                }
                self.apply_sort();
                Vec::new()
            }
            Action::ToggleThreading => {
                self.threaded = !self.threaded;
                self.expanded_threads.clear();
                self.run(core, |state| state.refresh(core))
            }
            Action::TogglePreview => {
                self.preview_visible = !self.preview_visible;
                Vec::new()
            }
            Action::ToggleThread { thread_id } => {
                if !self.expanded_threads.remove(&thread_id) {
                    self.expanded_threads.insert(thread_id);
                }
                Vec::new()
            }
            Action::LoadMore => self.run(core, |state| state.load_more(core)),
            Action::SyncAccount(id) => Vec::from([Task::Sync(Some(id))]),
            Action::SyncAll => Vec::from([Task::Sync(None)]),
            Action::ReloadFolders(id) => Vec::from([Task::ReloadFolders(id)]),
            Action::Compose => self.open_composer(core),
            Action::Reply => self.reply(core, false),
            Action::ReplyAll => self.reply(core, true),
            Action::Forward => self.forward(core),
            Action::Delete => {
                let targets = self.targets();
                if core.settings().confirm_delete
                    && !targets.is_empty()
                    && !self.selection_is_in_trash()
                {
                    // `FR-FLAG-4`: ask before a destructive action.
                    self.pending_delete = targets;
                    Vec::new()
                } else {
                    self.delete_selected()
                }
            }
            Action::ConfirmDelete => {
                self.pending_delete.clear();
                self.delete_selected()
            }
            Action::CancelDelete => {
                self.pending_delete.clear();
                Vec::new()
            }
            Action::ToggleSelection(id) => {
                if !self.selected_ids.remove(&id) {
                    self.selected_ids.insert(id);
                }
                Vec::new()
            }
            Action::SelectAll => {
                self.selected_ids = self.messages.iter().map(|header| header.id).collect();
                Vec::new()
            }
            Action::ClearSelection => {
                self.selected_ids.clear();
                Vec::new()
            }
            Action::ToggleFlag => self.toggle_flag(core),
            Action::MarkRead => self.selected_flag(core, MessageFlags::SEEN, true),
            Action::MarkUnread => self.selected_flag(core, MessageFlags::SEEN, false),
            Action::Done => self.done_action(core),
            Action::ToggleAnswered => self.toggle_answered(core),
            Action::MarkAllRead => self.mark_all_read(core),
            Action::ShowImages => self.load_message_images(core, false),
            Action::TrustSenderImages => self.load_message_images(core, true),
            Action::ToggleSource => {
                if let Some(reader) = self.reader.as_mut() {
                    reader.show_source = !reader.show_source;
                }
                Vec::new()
            }
            Action::SaveAttachment { part_id } => self.save_attachment(&part_id),
            Action::SaveAllAttachments => self.save_all_attachments(),
            Action::OpenSettings => {
                self.settings_form = Some(SettingsForm::new(core.settings()));
                Vec::new()
            }
            Action::OpenAccounts => {
                let document = core.config().load_accounts().unwrap_or_default();
                self.accounts_form = Some(AccountsForm::new(&document));
                Vec::new()
            }
            Action::OpenRules => {
                let rules = core.config().load_rules().unwrap_or_default();
                self.rules_form = Some(RulesForm::new(&rules));
                Vec::new()
            }
            Action::SaveRules => self.save_rules(core),
            Action::AddRule => {
                if let Some(form) = self.rules_form.as_mut() {
                    let name = if form.new_rule_name.trim().is_empty() {
                        "Neue Regel".to_string()
                    } else {
                        form.new_rule_name.trim().to_string()
                    };
                    let rule = Rule::new(name);
                    form.selected = Some(rule.id);
                    form.rules.rules.push(rule);
                    form.new_rule_name.clear();
                }
                Vec::new()
            }
            Action::RemoveRule { rule_id } => {
                if let Some(form) = self.rules_form.as_mut() {
                    form.rules.remove(rule_id);
                    form.selected = form.rules.rules.first().map(|rule| rule.id);
                }
                Vec::new()
            }
            Action::RunRules => self.run_rules(core),
            Action::OpenContacts => {
                self.reload_contacts(core);
                self.contacts_form = Some(ContactsForm::new(self.contacts.clone()));
                Vec::new()
            }
            Action::SaveContacts => self.save_contacts(core),
            Action::DeleteContact { contact_id } => self.delete_contact(core, contact_id),
            Action::SyncContacts => self.sync_contacts(),
            Action::OpenOutbox => {
                self.open_outbox(core);
                Vec::new()
            }
            Action::OutboxRemove { id } => self.outbox_remove(core, id),
            Action::OutboxRetry { id } => {
                self.outbox_retry(core, id);
                Vec::new()
            }
            Action::OutboxSendNow => vec![Task::DrainOutbox],
            Action::CloseDialog => {
                // A composer that is closed keeps its content as a local draft,
                // so nothing typed is lost (`FR-COMPOSE-7`).
                self.save_draft(core);
                self.settings_form = None;
                self.accounts_form = None;
                self.rules_form = None;
                self.contacts_form = None;
                self.outbox_form = None;
                self.composer = None;
                self.picker = None;
                self.pending_delete.clear();
                Vec::new()
            }
            Action::OpenEml(path) => self.open_eml(core, &path),
            Action::Send => self.submit_composer(core),
            Action::SaveDraft => {
                self.save_draft(core);
                Vec::new()
            }
            Action::Attach => {
                self.attach_files();
                Vec::new()
            }
            Action::EmbedImage => {
                self.embed_image();
                Vec::new()
            }
            Action::SaveSettings => self.save_settings(core),
            Action::SaveAccounts => self.save_accounts(core),
            Action::SavePassword => self.save_password(core),
            Action::MoveTo { target } => self.move_selected(target),
            Action::MoveDragged { message_id, target } => self.move_dragged_to(message_id, target),
            Action::ExportEml { message_id } => self.export_eml(message_id),
            Action::CalendarReply { status } => self.calendar_reply(core, status),
            Action::Archive => self.move_selected_to_kind(FolderKind::Archive),
            Action::MarkJunk => self.move_selected_to_kind(FolderKind::Junk),
            Action::OpenMovePicker => {
                self.open_move_picker();
                Vec::new()
            }
            Action::OAuthSignIn => self.oauth_sign_in(),
            Action::Quit => {
                self.quit = true;
                Vec::new()
            }
        }
    }

    /// Stages the files chosen in the native picker.
    ///
    /// The picker is modal, so it blocks this frame; it is the only place in
    /// the interface that reads from disk, and it only reads metadata.
    fn attach_files(&mut self) {
        let Some(paths) = rfd::FileDialog::new()
            .set_title("Anhang auswählen")
            .pick_files()
        else {
            return;
        };
        let mut failure = None;
        if let Some(composer) = self.composer.as_mut() {
            for path in paths {
                if let Err(error) = composer.attach(&path) {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }
        if let Some(error) = failure {
            if let Some(composer) = self.composer.as_mut() {
                composer.error = Some(error);
            }
        }
    }

    /// Embeds one or more picture files into the rich body (`FR-COMPOSE-3`).
    fn embed_image(&mut self) {
        if self.composer.is_none() {
            return;
        }
        let Some(paths) = rfd::FileDialog::new()
            .set_title("Bild einbetten")
            .add_filter("Bilder", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
            .pick_files()
        else {
            return;
        };
        let mut failure = None;
        for path in paths {
            let Some(composer) = self.composer.as_mut() else {
                return;
            };
            match composer.attach_inline(&path) {
                Ok(Some(content_id)) => {
                    let alt = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default();
                    composer.embed_image(&content_id, &alt);
                }
                Ok(None) => {
                    failure =
                        Some("Der Dateiname lässt sich nicht als Bild referenzieren.".to_string());
                    break;
                }
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }
        if let Some(error) = failure {
            if let Some(composer) = self.composer.as_mut() {
                composer.error = Some(error);
            }
        }
    }

    /// Writes the attachment the user clicked into a file they choose.
    ///
    /// The suggested name comes from the parsed body, never from the widget:
    /// the part id is the only thing the interface passes around.
    fn save_attachment(&mut self, part_id: &str) -> Vec<Task> {
        let Some((folder_id, message_id, suggestion)) = self.attachment_target(part_id) else {
            return Vec::new();
        };
        let Some(path) = rfd::FileDialog::new()
            .set_title("Anhang speichern")
            .set_file_name(suggestion)
            .save_file()
        else {
            return Vec::new();
        };
        self.status = format!("Anhang wird nach {} gespeichert …", path.display());
        vec![Task::SaveAttachments {
            folder_id,
            message_id,
            targets: vec![(part_id.to_string(), path)],
        }]
    }

    /// Writes every attachment of the displayed message into one directory.
    fn save_all_attachments(&mut self) -> Vec<Task> {
        let Some(reader) = self.reader.as_ref() else {
            return Vec::new();
        };
        let Some(body) = reader.body.as_ref() else {
            return Vec::new();
        };
        if body.attachments.is_empty() {
            self.status = "Diese Nachricht hat keine Anhänge.".to_string();
            return Vec::new();
        }
        let Some(directory) = rfd::FileDialog::new()
            .set_title("Zielordner für die Anhänge")
            .pick_folder()
        else {
            return Vec::new();
        };
        let folder_id = reader.header.folder_id;
        let message_id = reader.header.id;
        let names = unique_file_names(&body.attachments);
        let targets = body
            .attachments
            .iter()
            .zip(names)
            .map(|(attachment, name)| (attachment.part_id.clone(), directory.join(name)))
            .collect::<Vec<_>>();
        self.status = format!(
            "{} werden nach {} gespeichert …",
            plural(targets.len() as u64, "Anhang", "Anhänge"),
            directory.display()
        );
        vec![Task::SaveAttachments {
            folder_id,
            message_id,
            targets,
        }]
    }

    /// Folder, message and suggested file name of one listed attachment.
    fn attachment_target(&self, part_id: &str) -> Option<(FolderId, MessageId, String)> {
        let reader = self.reader.as_ref()?;
        let body = reader.body.as_ref()?;
        let attachment = body
            .attachments
            .iter()
            .find(|attachment| attachment.part_id == part_id)?;
        Some((
            reader.header.folder_id,
            reader.header.id,
            safe_file_name(attachment.display_name()),
        ))
    }

    /// Writes the settings dialog back to disk.
    fn save_settings(&mut self, core: &mut Core) -> Vec<Task> {
        let Some(form) = self.settings_form.as_ref() else {
            return Vec::new();
        };
        let outcome = core.save_settings(form.settings.clone());
        let saved = outcome.is_ok();
        if let Some(form) = self.settings_form.as_mut() {
            match outcome {
                Ok(()) => {
                    form.status = Some("Einstellungen gespeichert.".to_string());
                    form.error = None;
                }
                Err(error) => {
                    form.error = Some(error.to_string());
                    form.status = None;
                }
            }
        }
        if saved {
            self.page_size = core.settings().page_size;
            self.search_min_chars = core.settings().server_search_min_chars;
            let settings = core.settings().clone();
            if let Some(reader) = self.reader.as_mut() {
                reader.resanitise(&settings);
            }
            // The engine froze the settings at start-up; let it pick up a new
            // sync interval or cap without an application restart.
            return Vec::from([Task::ReloadSettings]);
        }
        Vec::new()
    }

    /// Writes the accounts dialog back to disk.
    fn save_accounts(&mut self, core: &mut Core) -> Vec<Task> {
        let Some(form) = self.accounts_form.as_ref() else {
            return Vec::new();
        };
        let document = form.document.clone();
        let outcome = core.config().save_accounts(&document);
        let saved = outcome.is_ok();
        if let Some(form) = self.accounts_form.as_mut() {
            match outcome {
                Ok(()) => {
                    form.status = Some("Konten gespeichert.".to_string());
                    form.error = None;
                }
                Err(error) => {
                    form.error = Some(error.to_string());
                    form.status = None;
                }
            }
        }
        if saved {
            self.accounts = document.accounts.clone();
            // A password that is still in the field is written to the vault too,
            // so a freshly created account can authenticate without a second
            // click (`FR-AUTH-6`).
            let password_pending = self
                .accounts_form
                .as_ref()
                .is_some_and(|form| !form.password.is_empty());
            if password_pending {
                self.save_password(core);
            }
            if let Some(form) = self.accounts_form.as_mut() {
                let note = if password_pending && form.error.is_none() {
                    " Das Kennwort wurde im Tresor abgelegt."
                } else {
                    ""
                };
                form.status = Some(format!("Konten gespeichert.{note}"));
            }
            // Visible even with the dialog open: the account is contacted now.
            self.status = "Konten gespeichert – die Synchronisierung wird gestartet.".to_string();
            // The engine only knows the accounts it started with; make it
            // re-read the list so a newly created account is synchronised right
            // away instead of after the next restart (`AC-1`).
            return Vec::from([Task::ReloadAccounts]);
        }
        Vec::new()
    }

    /// Writes the rule editor back to disk.
    fn save_rules(&mut self, core: &mut Core) -> Vec<Task> {
        let Some(form) = self.rules_form.as_ref() else {
            return Vec::new();
        };
        let rules = form.rules.clone();
        let outcome = core.config().save_rules(&rules);
        if let Some(form) = self.rules_form.as_mut() {
            match outcome {
                Ok(()) => {
                    form.status = Some("Regeln gespeichert.".to_string());
                    form.error = None;
                }
                Err(error) => {
                    form.error = Some(error.to_string());
                    form.status = None;
                }
            }
        }
        Vec::new()
    }

    /// Runs the enabled rules over the messages currently listed.
    ///
    /// This is the manual "Regeln jetzt anwenden" command (`FR-RULE-1`): the
    /// evaluator decides what should happen, and each planned action becomes the
    /// same [`Task`] a click would produce.  GroupWise proxy accounts are
    /// excluded (`BR-GW-2`).
    fn run_rules(&mut self, core: &Core) -> Vec<Task> {
        let rules = core.config().load_rules().unwrap_or_default();
        let headers = self.messages.clone();
        let mut tasks = Vec::new();
        let mut applied = 0usize;
        for header in &headers {
            if self
                .account(header.account_id)
                .is_some_and(|account| !account.kind.supports_rules())
            {
                continue;
            }
            let plan = hylmail_core::rules::plan(&rules, header, None);
            if !plan.is_effective() {
                continue;
            }
            applied += 1;
            self.apply_plan(core, header, &plan, &mut tasks);
        }
        if let Some(form) = self.rules_form.as_mut() {
            form.status = Some(format!(
                "{applied} von {} Nachricht(en) durch Regeln bearbeitet.",
                headers.len()
            ));
        }
        self.status = format!("{applied} Nachricht(en) durch Regeln bearbeitet.");
        tasks
    }

    /// Turns one evaluated rule plan into the tasks that carry it out.
    ///
    /// Shared by the manual "Regeln jetzt anwenden" and by the automatic arrival
    /// pass, so both produce exactly the same move/flag/delete tasks a click
    /// would.
    fn apply_plan(
        &mut self,
        core: &Core,
        header: &MessageHeader,
        plan: &hylmail_core::rules::RulePlan,
        tasks: &mut Vec<Task>,
    ) {
        if plan.mark_read && header.is_unread() {
            tasks.extend(self.change_flags(core, header.id, MessageFlags::SEEN, true));
        }
        if plan.flag && !header.is_flagged() {
            tasks.extend(self.change_flags(core, header.id, MessageFlags::FLAGGED, true));
        }
        if let Some(path) = &plan.move_to {
            let target = self
                .folders
                .iter()
                .find(|folder| {
                    folder.account_id == header.account_id
                        && (folder.path == *path || folder.name == *path)
                })
                .map(|folder| folder.id);
            match target {
                Some(target_folder_id) => tasks.push(Task::MoveMessage {
                    folder_id: header.folder_id,
                    message_id: header.id,
                    target_folder_id,
                }),
                None => {
                    self.error = Some(format!(
                        "Regelziel „{path}“ ist in diesem Konto nicht bekannt."
                    ))
                }
            }
        } else if plan.delete {
            tasks.push(Task::DeleteMessage {
                folder_id: header.folder_id,
                message_id: header.id,
                permanent: false,
            });
        }
    }

    /// Runs the enabled arrival rules over the mail a sync just stored.
    ///
    /// Only messages the engine has not seen before are considered (`FR-RULE-1`),
    /// and GroupWise proxy accounts are skipped (`BR-GW-2`).
    fn process_arrival_rules(&mut self, core: &Core, account_id: AccountId) -> Vec<Task> {
        let mut tasks = Vec::new();
        if !self
            .account(account_id)
            .is_some_and(|account| account.kind.supports_rules())
        {
            return tasks;
        }
        let rules = match core.config().load_rules() {
            Ok(rules) => rules,
            Err(_) => return tasks,
        };
        if rules.rules.is_empty() {
            return tasks;
        }
        let inboxes: Vec<FolderId> = self
            .folders
            .iter()
            .filter(|folder| folder.account_id == account_id && folder.kind == FolderKind::Inbox)
            .map(|folder| folder.id)
            .collect();
        if inboxes.is_empty() {
            return tasks;
        }
        let query = MessageQuery {
            folder_ids: inboxes,
            limit: Some(self.page_size.max(200)),
            ..MessageQuery::default()
        };
        let headers = core
            .database()
            .read(|connection| db_messages::list_messages(connection, &query))
            .unwrap_or_default();
        for header in headers {
            if !self.processed.insert(header.id) {
                continue;
            }
            let plan = hylmail_core::rules::plan_on_arrival(&rules, &header, None);
            if !plan.is_effective() {
                continue;
            }
            self.apply_plan(core, &header, &plan, &mut tasks);
        }
        if !tasks.is_empty() {
            self.status = format!(
                "Regeln auf neu eingegangene Nachrichten angewandt ({} Aktion(en)).",
                tasks.len()
            );
        }
        tasks
    }

    /// Persists the edited and newly created contacts.
    fn save_contacts(&mut self, core: &mut Core) -> Vec<Task> {
        let Some(form) = self.contacts_form.as_ref() else {
            return Vec::new();
        };
        let contacts = form.contacts.clone();
        let outcome = core.database().write(|connection| {
            for contact in &contacts {
                db_contacts::upsert_contact(connection, contact)?;
            }
            Ok(())
        });
        match outcome {
            Ok(()) => {
                if let Some(form) = self.contacts_form.as_mut() {
                    form.status = Some("Adressbuch gespeichert.".to_string());
                    form.error = None;
                }
            }
            Err(error) => {
                if let Some(form) = self.contacts_form.as_mut() {
                    form.error = Some(error.to_string());
                    form.status = None;
                }
            }
        }
        self.reload_contacts(core);
        Vec::new()
    }

    /// Deletes one contact from the database and the open form.
    fn delete_contact(&mut self, core: &Core, contact_id: ContactId) -> Vec<Task> {
        match core
            .database()
            .write(|connection| db_contacts::delete_contact(connection, contact_id))
        {
            Ok(_) => {
                if let Some(form) = self.contacts_form.as_mut() {
                    form.contacts.retain(|contact| contact.id != contact_id);
                    form.selected = form.selected.min(form.contacts.len().saturating_sub(1));
                }
                self.reload_contacts(core);
                self.status = "Kontakt gelöscht".to_string();
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        Vec::new()
    }

    /// Queues a CardDAV synchronisation for the account that has one configured.
    fn sync_contacts(&mut self) -> Vec<Task> {
        let account = self
            .contacts_form
            .as_ref()
            .and_then(|form| form.contacts.iter().find_map(|contact| contact.account_id))
            .or(self.active_account)
            .or_else(|| self.accounts.first().map(|account| account.id));
        match account {
            Some(account_id) => {
                if let Some(form) = self.contacts_form.as_mut() {
                    form.status = Some("Adressbuch wird abgeglichen …".to_string());
                    form.error = None;
                }
                vec![Task::SyncContacts { account_id }]
            }
            None => {
                if let Some(form) = self.contacts_form.as_mut() {
                    form.error = Some("Es ist noch kein Konto eingerichtet.".to_string());
                    form.status = None;
                }
                Vec::new()
            }
        }
    }

    /// The outgoing queue (`FR-OUTBOX-3`).
    fn open_outbox(&mut self, core: &Core) {
        match core.config().load_outbox() {
            Ok(document) => {
                self.outbox_form = Some(OutboxForm {
                    document,
                    status: None,
                });
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    /// Removes one entry from the outgoing queue and writes the document back.
    fn outbox_remove(&mut self, core: &Core, id: OutboxId) -> Vec<Task> {
        let Some(form) = self.outbox_form.as_mut() else {
            return Vec::new();
        };
        hylmail_core::outbox::remove(&mut form.document, id);
        form.status = match core.config().save_outbox(&form.document) {
            Ok(()) => Some("Aus dem Postausgang entfernt.".to_string()),
            Err(error) => Some(error.to_string()),
        };
        Vec::new()
    }

    /// Re-arms a parked entry and writes the document back.
    fn outbox_retry(&mut self, core: &Core, id: OutboxId) {
        let Some(form) = self.outbox_form.as_mut() else {
            return;
        };
        let changed = hylmail_core::outbox::retry(&mut form.document, id);
        if changed {
            form.status = match core.config().save_outbox(&form.document) {
                Ok(()) => Some("Erneut zur Zustellung vorgemerkt.".to_string()),
                Err(error) => Some(error.to_string()),
            };
        }
    }

    /// Stores the recipients of a message that was just composed.
    fn collect_recipients(&self, core: &Core, draft: &Draft) {
        let addresses: Vec<MailAddress> = draft
            .to
            .iter()
            .chain(draft.cc.iter())
            .chain(draft.bcc.iter())
            .cloned()
            .collect();
        if addresses.is_empty() {
            return;
        }
        if let Err(error) = core.database().write(|connection| {
            for address in &addresses {
                db_contacts::collect_address(connection, address)?;
            }
            Ok(())
        }) {
            tracing::debug!(%error, "collecting the recipients failed");
        }
    }

    /// Starts the OAuth2 sign-in of the account shown in the accounts dialog.
    ///
    /// Nothing is written here: the flow needs a browser and a human, so it
    /// runs in the worker, which reports back through
    /// [`UiEvent::OAuthFinished`].  The account is *not* saved beforehand - the
    /// user decides when the form goes to disk - but the tokens are stored
    /// under the account id, which the saved form will keep.
    fn oauth_sign_in(&mut self) -> Vec<Task> {
        let Some(form) = self.accounts_form.as_ref() else {
            return Vec::new();
        };
        let Some(account) = form.current() else {
            return Vec::new();
        };
        let usable = account.auth == AuthKind::OAuth2
            && account
                .oauth
                .as_ref()
                .is_some_and(hylmail_core::models::OAuthConfig::is_configured);
        if !usable {
            if let Some(form) = self.accounts_form.as_mut() {
                form.error = Some(
                    "Für die Anmeldung werden Client-ID und beide Endpunkte benötigt.".to_string(),
                );
                form.status = None;
            }
            return Vec::new();
        }
        let account = Box::new(account.clone());
        if let Some(form) = self.accounts_form.as_mut() {
            form.error = None;
            form.status = Some("Der Browser wurde geöffnet; bitte dort anmelden.".to_string());
        }
        vec![Task::OAuthLogin { account }]
    }

    /// Stores the password typed into the accounts dialog.
    fn save_password(&mut self, core: &mut Core) -> Vec<Task> {
        let Some(form) = self.accounts_form.as_ref() else {
            return Vec::new();
        };
        let Some(account) = form.current() else {
            return Vec::new();
        };
        if form.password.is_empty() {
            if let Some(form) = self.accounts_form.as_mut() {
                form.error = Some("Das Kennwort darf nicht leer sein.".to_string());
            }
            return Vec::new();
        }
        let account_id = account.id;
        let slots = credential_slots(account);
        if slots.is_empty() {
            if let Some(form) = self.accounts_form.as_mut() {
                form.error = Some(
                    "Dieses Konto verwendet OAuth2; das Kennwort wird nicht im Tresor abgelegt."
                        .to_string(),
                );
            }
            return Vec::new();
        }
        let value = hylmail_core::secrets::secret(form.password.clone());
        let mut failure = None;
        for kind in slots {
            let key = hylmail_core::secrets::CredentialKey::new(account_id, kind);
            if let Err(error) = core.secrets().set(&key, &value) {
                failure = Some(error.to_string());
                break;
            }
        }
        if let Some(form) = self.accounts_form.as_mut() {
            match failure {
                Some(error) => {
                    form.error = Some(error);
                    form.status = None;
                }
                None => {
                    form.password.clear();
                    form.status = Some("Kennwort gespeichert.".to_string());
                    form.error = None;
                }
            }
        }
        Vec::new()
    }

    /// Runs a fallible state transition, reporting the error in the status bar.
    fn run(
        &mut self,
        _core: &Core,
        operation: impl FnOnce(&mut Self) -> hylmail_core::Result<()>,
    ) -> Vec<Task> {
        match operation(self) {
            Ok(()) => Vec::new(),
            Err(error) => {
                self.error = Some(error.to_string());
                Vec::new()
            }
        }
    }

    /// Toggles the flag of every target.
    fn toggle_flag(&mut self, core: &Core) -> Vec<Task> {
        let mut tasks = Vec::new();
        for id in self.targets() {
            let flagged = self
                .messages
                .iter()
                .find(|header| header.id == id)
                .is_some_and(|header| header.is_flagged());
            tasks.extend(self.change_flags(core, id, MessageFlags::FLAGGED, !flagged));
        }
        tasks
    }

    /// Sets one flag on every target.
    fn selected_flag(&mut self, core: &Core, flag: MessageFlags, on: bool) -> Vec<Task> {
        let mut tasks = Vec::new();
        for id in self.targets() {
            tasks.extend(self.change_flags(core, id, flag, on));
        }
        tasks
    }

    /// The "done" action (`E`), interpreted through the configured mode
    /// (`FR-FLAG-2`/`FR-FLAG-3`).
    ///
    /// `Archive` moves the message into the archive folder; `MarkDone` sets the
    /// backend-appropriate done mark, which is `$Done` on IMAP, the item status
    /// on Exchange and a local mark on a GroupWise proxy.
    fn done_action(&mut self, core: &Core) -> Vec<Task> {
        match core.settings().done_mode {
            DoneMode::Archive => self.move_selected_to_kind(FolderKind::Archive),
            DoneMode::MarkDone => {
                let mut tasks = Vec::new();
                let mut already_all = true;
                for id in self.targets() {
                    let already = self
                        .messages
                        .iter()
                        .find(|header| header.id == id)
                        .is_some_and(|header| header.is_done());
                    already_all &= already;
                    tasks.extend(self.change_flags(core, id, MessageFlags::DONE, !already));
                }
                if !tasks.is_empty() {
                    self.status = if already_all {
                        "Erledigt-Markierung entfernt".to_string()
                    } else {
                        "Als erledigt markiert".to_string()
                    };
                }
                tasks
            }
        }
    }

    /// Toggles the `\Answered` mark of every target (`FR-FLAG-1`).
    fn toggle_answered(&mut self, core: &Core) -> Vec<Task> {
        let mut tasks = Vec::new();
        for id in self.targets() {
            let answered = self
                .messages
                .iter()
                .find(|header| header.id == id)
                .is_some_and(|header| header.is_answered());
            tasks.extend(self.change_flags(core, id, MessageFlags::ANSWERED, !answered));
        }
        tasks
    }

    /// Allows the images of the displayed message and schedules their download.
    ///
    /// `remember` also trusts the sender for the future (`FR-READ-2`).  The URLs
    /// are de-duplicated so each image is fetched once.
    fn load_message_images(&mut self, core: &mut Core, remember: bool) -> Vec<Task> {
        let Some(reader) = self.reader.as_ref() else {
            return Vec::new();
        };
        let message_id = reader.header.id;
        let sender = reader
            .header
            .from
            .first()
            .map(|address| address.email.clone());
        if remember {
            if let Some(sender) = sender {
                let mut settings = core.settings().clone();
                if !settings
                    .trusted_image_senders
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(&sender))
                {
                    settings.trusted_image_senders.push(sender);
                    if let Err(error) = core.save_settings(settings) {
                        self.error = Some(error.to_string());
                    }
                }
            }
        }
        let settings = core.settings().clone();
        if let Some(reader) = self.reader.as_mut() {
            reader.show_images = true;
            reader.resanitise(&settings);
        }
        let mut seen = HashSet::new();
        let urls: Vec<String> = self
            .reader
            .as_ref()
            .and_then(|reader| reader.document.as_ref())
            .map(|document| {
                document
                    .images
                    .iter()
                    .filter(|image| crate::html_view::is_remote_source(&image.src))
                    .map(|image| image.src.clone())
                    .filter(|url| seen.insert(url.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if urls.is_empty() {
            return Vec::new();
        }
        vec![Task::LoadImages { message_id, urls }]
    }

    /// The messages a bulk action applies to.
    ///
    /// A ticked checkbox wins; with nothing ticked the action falls back to the
    /// focused message, so the toolbar keeps working exactly as it did before
    /// bulk selection existed.  Ids that left the list are dropped, so a stale
    /// checkbox can never act on a message the user no longer sees.
    fn targets(&self) -> Vec<MessageId> {
        let ids: Vec<MessageId> = if self.selected_ids.is_empty() {
            self.selected.into_iter().collect()
        } else {
            self.selected_ids.iter().copied().collect()
        };
        ids.into_iter()
            .filter(|id| self.messages.iter().any(|header| header.id == *id))
            .collect()
    }

    /// `true` when `folder_id` is the trash of its account.
    fn folder_is_trash(&self, folder_id: FolderId) -> bool {
        self.folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .is_some_and(|folder| folder.kind == FolderKind::Trash)
    }

    /// `true` when the selected message already sits in the trash.
    ///
    /// Deleting it then removes it for good, which is what pressing the same
    /// button a second time means.
    #[must_use]
    pub fn selection_is_in_trash(&self) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        let Some(header) = self.messages.iter().find(|header| header.id == id) else {
            return false;
        };
        self.folder_is_trash(header.folder_id)
    }

    /// Asks the worker to delete the selected message on the server.
    ///
    /// The row is only dropped once the server confirmed, so a failed delete
    /// cannot make a message disappear from the list while it still exists in
    /// the mailbox.
    fn delete_selected(&mut self) -> Vec<Task> {
        let mut tasks = Vec::new();
        for id in self.targets() {
            let Some(folder_id) = self
                .messages
                .iter()
                .find(|header| header.id == id)
                .map(|header| header.folder_id)
            else {
                continue;
            };
            tasks.push(Task::DeleteMessage {
                folder_id,
                message_id: id,
                permanent: self.folder_is_trash(folder_id),
            });
        }
        tasks
    }

    /// Opens the folder picker for the selected message.
    fn open_move_picker(&mut self) {
        let Some(id) = self.selected else {
            return;
        };
        let Some(folder_id) = self
            .messages
            .iter()
            .find(|header| header.id == id)
            .map(|header| header.folder_id)
        else {
            return;
        };
        if self.move_targets(folder_id).next().is_none() {
            self.error =
                Some("Es gibt keinen anderen Ordner, in den die Nachricht kann.".to_string());
            return;
        }
        self.picker = Some(MovePicker::new(folder_id));
    }

    /// The folders the message in `folder_id` can be moved into.
    ///
    /// Same account, able to receive mail, and neither the folder it is already
    /// in, by id as well as by server path, because a re-created row gets a new
    /// local id for an unchanged mailbox.
    pub(crate) fn move_targets(&self, folder_id: FolderId) -> impl Iterator<Item = &Folder> {
        let source = self.folders.iter().find(|folder| folder.id == folder_id);
        self.folders.iter().filter(move |folder| match source {
            Some(source) => hylmail_core::relocate::usable_target(source, folder),
            None => false,
        })
    }

    /// Moves the selected message into `target`.
    fn move_selected(&mut self, target: FolderId) -> Vec<Task> {
        self.picker = None;
        let mut tasks = Vec::new();
        for id in self.targets() {
            let Some(folder_id) = self
                .messages
                .iter()
                .find(|header| header.id == id)
                .map(|header| header.folder_id)
            else {
                continue;
            };
            if !self
                .move_targets(folder_id)
                .any(|folder| folder.id == target)
            {
                self.error = Some("Dieser Ordner kann die Nachricht nicht aufnehmen.".to_string());
                continue;
            }
            tasks.push(Task::MoveMessage {
                folder_id,
                message_id: id,
                target_folder_id: target,
            });
        }
        tasks
    }

    /// Moves one dragged message into `target`.
    ///
    /// The drag carries the message id, so the move does not depend on what is
    /// currently selected.  The target is validated exactly like the picker
    /// validates it, so a drop cannot move a message into a folder of another
    /// account (`FR-DND-1`).
    fn move_dragged_to(&mut self, message_id: MessageId, target: FolderId) -> Vec<Task> {
        let Some(folder_id) = self
            .messages
            .iter()
            .find(|header| header.id == message_id)
            .map(|header| header.folder_id)
        else {
            return Vec::new();
        };
        if !self
            .move_targets(folder_id)
            .any(|folder| folder.id == target)
        {
            self.error = Some("Dieser Ordner kann die Nachricht nicht aufnehmen.".to_string());
            return Vec::new();
        }
        self.selected = Some(message_id);
        vec![Task::MoveMessage {
            folder_id,
            message_id,
            target_folder_id: target,
        }]
    }

    /// Writes one message to a `.eml` file the user picks (`FR-DND-2`).
    ///
    /// The drag ends without a folder accepting it, so the message is exported
    /// instead.  The bytes come from the server on demand, exactly like saving
    /// an attachment: the reader keeps no raw mail in memory.
    fn export_eml(&mut self, message_id: MessageId) -> Vec<Task> {
        let Some(header) = self
            .messages
            .iter()
            .find(|header| header.id == message_id)
            .cloned()
        else {
            return Vec::new();
        };
        let stem = if header.subject.trim().is_empty() {
            "nachricht".to_string()
        } else {
            safe_file_name(&header.subject)
        };
        let suggestion = format!("{}.eml", stem.trim_end_matches(".eml"));
        let Some(path) = rfd::FileDialog::new()
            .set_title("Nachricht als .eml speichern")
            .set_file_name(suggestion)
            .save_file()
        else {
            return Vec::new();
        };
        self.status = format!("Nachricht wird nach {} geschrieben …", path.display());
        vec![Task::SaveEml {
            folder_id: header.folder_id,
            message_id,
            path,
        }]
    }

    /// Answers a calendar invitation with a `METHOD:REPLY` (`FR-CAL-2`).
    ///
    /// The reply travels as an ordinary submission through the outbox, with the
    /// iCalendar object attached as `text/calendar; method=REPLY` - the shape
    /// other clients expect.  GroupWise proxy accounts never reach this, because
    /// the buttons are hidden for them (`FR-CAL-3`).
    fn calendar_reply(
        &mut self,
        core: &Core,
        status: hylmail_core::calendar::PartStat,
    ) -> Vec<Task> {
        let Some(reader) = self.reader.as_ref() else {
            return Vec::new();
        };
        let Some(calendar) = reader.calendar.as_ref() else {
            return Vec::new();
        };
        let Some(organizer) = calendar
            .organizer
            .clone()
            .filter(|organizer| !organizer.trim().is_empty())
        else {
            self.error = Some("Diese Einladung nennt keinen Organisator.".to_string());
            return Vec::new();
        };
        let Some(ics) = reader
            .body
            .as_ref()
            .and_then(|body| body.calendar_ics.as_deref())
        else {
            self.error = Some("Die Einladung ist nicht mehr verfügbar.".to_string());
            return Vec::new();
        };
        let Some(account) = self.account(reader.header.account_id).cloned() else {
            self.error = Some("Das Konto der Nachricht ist nicht mehr eingerichtet.".to_string());
            return Vec::new();
        };
        let reply = match hylmail_core::calendar::build_reply(ics, &account.email, status) {
            Ok(reply) => reply,
            Err(error) => {
                self.error = Some(error.to_string());
                return Vec::new();
            }
        };
        // The composer attaches a file, so the reply is written next to the
        // other temporary data before it is submitted.  The name is scoped to
        // the message, so two replies cannot collide.
        let path = std::env::temp_dir().join(format!("hylmail-antwort-{}.ics", reader.header.id));
        if let Err(error) = std::fs::write(&path, reply.as_bytes()) {
            self.error = Some(format!(
                "Die Antwort konnte nicht vorbereitet werden: {error}"
            ));
            return Vec::new();
        }
        let mut draft = Draft::new(account.id);
        draft.to = vec![MailAddress::new(organizer)];
        draft.subject = hylmail_core::compose::reply_subject(calendar.title());
        draft.body_plain = format!("{}: {}", status.label(), calendar.title());
        draft.attachments = vec![DraftAttachment {
            filename: "antwort.ics".to_string(),
            mime_type: "text/calendar; method=REPLY; charset=utf-8".to_string(),
            path,
            size_bytes: reply.len() as u64,
            inline: false,
        }];
        self.status = format!("Antwort „{}“ wird gesendet …", status.label());
        let account_id = account.id;
        let signature = core.settings().default_signature(account_id).cloned();
        vec![Task::Submit {
            account_id,
            draft: Box::new(draft),
            signature,
        }]
    }

    /// Moves the selected message into the first folder of a kind.
    ///
    /// "Archivieren" and "Spam" need a special-use folder of the account; when
    /// the server has none the user is told instead of the message silently
    /// staying where it is.
    fn move_selected_to_kind(&mut self, kind: FolderKind) -> Vec<Task> {
        let mut tasks = Vec::new();
        let mut missing = false;
        for id in self.targets() {
            let Some(folder_id) = self
                .messages
                .iter()
                .find(|header| header.id == id)
                .map(|header| header.folder_id)
            else {
                continue;
            };
            let target = self
                .move_targets(folder_id)
                .find(|folder| folder.kind == kind)
                .map(|folder| folder.id);
            match target {
                Some(target) => tasks.push(Task::MoveMessage {
                    folder_id,
                    message_id: id,
                    target_folder_id: target,
                }),
                None => missing = true,
            }
        }
        if missing {
            self.error = Some(format!(
                "Kein Ordner „{}“ für dieses Konto bekannt; bitte zuerst synchronisieren.",
                kind_label(kind)
            ));
        }
        tasks
    }

    /// Removes the message from the current list after the server moved it.
    ///
    /// The reader is closed when it showed that message: the body is no longer
    /// addressed correctly, and the row is gone from this folder either way.
    fn relocate_message(&mut self, message_id: MessageId) {
        let before = self.messages.len();
        self.messages.retain(|header| header.id != message_id);
        let removed = before - self.messages.len();
        self.total = self
            .total
            .saturating_sub(u64::try_from(removed).unwrap_or(0));
        if self
            .reader
            .as_ref()
            .is_some_and(|reader| reader.header.id == message_id)
        {
            self.reader = None;
        }
        if self.selected == Some(message_id) {
            self.selected = self.messages.first().map(|header| header.id);
        }
        self.selected_ids.remove(&message_id);
        // The folder counters are stale now; the next load recomputes them.
        self.loaded = 0;
    }

    /// Stores the message counters the database reported for one folder.
    fn set_counts(&mut self, folder_id: FolderId, unread: u32, total: u32) {
        if let Some(folder) = self
            .folders
            .iter_mut()
            .find(|folder| folder.id == folder_id)
        {
            folder.unread_count = unread;
            folder.total_count = total;
        }
    }

    /// Re-reads the counters of one folder, after a message changed or left.
    fn refresh_counts(&mut self, core: &Core, folder_id: FolderId) {
        match core
            .database()
            .write(|connection| db_messages::refresh_folder_counts(connection, folder_id))
        {
            Ok((unread, total)) => self.set_counts(folder_id, unread, total),
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    /// Drops a message the server has confirmed as deleted.
    ///
    /// The reading pane is closed when it showed that message; picking the
    /// next one is left to the user, because loading a body is a [`Task`] and
    /// this runs while events are drained, where no task can be returned.
    fn forget_message(&mut self, message_id: MessageId) {
        let Some(index) = self
            .messages
            .iter()
            .position(|header| header.id == message_id)
        else {
            return;
        };
        self.messages.remove(index);
        self.selected_ids.remove(&message_id);
        if self.selected == Some(message_id) {
            self.selected = None;
            self.reader = None;
        }
    }

    /// Merges one worker event into the state.
    ///
    /// An event about a message that is no longer displayed is dropped: the
    /// user may have moved on while the body was downloading.
    pub fn apply(&mut self, event: UiEvent, core: &Core) {
        match event {
            UiEvent::BodyLoaded {
                message_id,
                body,
                error,
            } => {
                let current = self
                    .reader
                    .as_ref()
                    .is_some_and(|reader| reader.header.id == message_id);
                if !current {
                    tracing::debug!(%message_id, "body arrived for a message that is gone");
                    return;
                }
                if let Some(header) = self
                    .messages
                    .iter_mut()
                    .find(|header| header.id == message_id)
                {
                    header.body_cached = body.is_some();
                }
                if let Some(reader) = self.reader.as_mut() {
                    reader.loading = false;
                    match body {
                        Some(body) => {
                            reader.calendar = calendar_of(&body);
                            reader.body = Some(*body);
                            reader.load_error = None;
                            reader.resanitise(core.settings());
                        }
                        None => reader.load_error = error,
                    }
                }
            }
            UiEvent::FlagsPushed { message_id, error } => match error {
                Some(error) => {
                    self.error = Some(format!(
                        "Die Markierung konnte nicht übertragen werden: {error}"
                    ));
                }
                None => tracing::debug!(%message_id, "flags pushed"),
            },
            UiEvent::AttachmentsSaved {
                message_id,
                count,
                error,
            } => match error {
                Some(error) => {
                    self.error = Some(format!(
                        "Der Anhang konnte nicht gespeichert werden: {error}"
                    ));
                }
                None => {
                    tracing::debug!(%message_id, count, "attachments written");
                    self.status =
                        format!("{} gespeichert.", plural(count as u64, "Anhang", "Anhänge"));
                }
            },
            UiEvent::Deleted {
                folder_id,
                message_id,
                moved,
                error,
            } => match error {
                Some(error) => {
                    self.error = Some(format!(
                        "Die Nachricht konnte nicht gelöscht werden: {error}"
                    ));
                }
                None => {
                    tracing::debug!(%message_id, moved, "message deleted");
                    self.forget_message(message_id);
                    self.refresh_counts(core, folder_id);
                    self.status = if moved {
                        "In den Papierkorb verschoben".to_string()
                    } else {
                        "Nachricht gelöscht".to_string()
                    };
                }
            },
            UiEvent::MailSent { detail, ok } => {
                if let Some(composer) = self.composer.as_mut() {
                    composer.busy = false;
                    if ok {
                        composer.status = Some(detail.clone());
                    } else {
                        composer.error = Some(detail.clone());
                    }
                }
                if ok {
                    self.composer = None;
                    self.status = detail;
                } else {
                    self.error = Some(detail);
                }
            }
            UiEvent::Moved {
                folder_id,
                message_id,
                target_folder_id,
                target_name,
                error,
            } => match error {
                Some(error) => self.error = Some(error),
                None => {
                    self.relocate_message(message_id);
                    self.refresh_counts(core, folder_id);
                    self.refresh_counts(core, target_folder_id);
                    self.status = format!("Nach „{target_name}“ verschoben.");
                }
            },
            UiEvent::OAuthFinished {
                account_id,
                email,
                detail,
                error,
            } => match error {
                Some(error) => {
                    let text = format!("Die Anmeldung für {email} ist fehlgeschlagen: {error}");
                    if let Some(form) = self.accounts_form.as_mut() {
                        form.error = Some(error.clone());
                        form.status = None;
                    }
                    self.error = Some(text);
                }
                None => {
                    if let Some(form) = self.accounts_form.as_mut() {
                        form.error = None;
                        form.status = Some(detail.clone());
                    }
                    self.status = detail.clone();
                    // The engine picks the account up on its next round; the
                    // token is in the vault by now.
                    tracing::debug!(account = %account_id, "OAuth2 sign-in completed");
                }
            },
            UiEvent::ContactsSynced { fetched, error } => match error {
                Some(error) => {
                    if let Some(form) = self.contacts_form.as_mut() {
                        form.error = Some(error.clone());
                        form.status = None;
                    }
                    self.error = Some(format!("Adressbuch-Abgleich fehlgeschlagen: {error}"));
                }
                None => {
                    self.reload_contacts(core);
                    let contacts = self.contacts.clone();
                    if let Some(form) = self.contacts_form.as_mut() {
                        form.contacts = contacts;
                        form.error = None;
                        form.status = Some(format!("{fetched} Kontakt(e) abgeglichen."));
                    }
                    self.status = format!("Adressbuch abgeglichen ({fetched} Änderung(en)).");
                }
            },
            UiEvent::EmlSaved { path, error } => match error {
                Some(error) => {
                    self.error = Some(format!(
                        "Die Nachricht konnte nicht gespeichert werden: {error}"
                    ));
                }
                None => self.status = format!("Nachricht gespeichert: {}", path.display()),
            },
            UiEvent::OutboxSent { count } => {
                self.status = format!(
                    "{} aus dem Postausgang versendet.",
                    plural(count as u64, "Nachricht", "Nachrichten")
                );
            }
            UiEvent::ImagesLoaded { message_id, images } => {
                // The session cache is bounded (`FR-READ-5`); an image that would
                // push it over the limit is simply not kept.
                let cap = core.settings().image_cache_bytes() as usize;
                let mut kept = 0usize;
                for (url, decoded) in images {
                    if self.image_cache.contains_key(&url) {
                        continue;
                    }
                    let bytes = decoded.byte_len();
                    if self.image_cache_bytes.saturating_add(bytes) > cap {
                        continue;
                    }
                    self.image_cache_bytes += bytes;
                    self.image_cache.insert(url, decoded);
                    kept += 1;
                }
                tracing::debug!(%message_id, kept, "remote images loaded");
            }
            UiEvent::SearchResults { found, error } => match error {
                Some(error) => {
                    self.error = Some(format!("Serverdurchsuchung fehlgeschlagen: {error}"));
                }
                None => {
                    self.status = if found == 0 {
                        "Serverdurchsuchung: keine Treffer".to_string()
                    } else {
                        format!("Serverdurchsuchung: {found} Treffer")
                    };
                    // The hits were just stored, so the list can show them.
                    if let Err(error) = self.refresh(core) {
                        self.error = Some(error.to_string());
                    }
                }
            },
            UiEvent::Error(text) => self.error = Some(text),
        }
    }

    /// Re-queries the folders and the list when a sync event marked them dirty.
    ///
    /// `apply_engine` only sets [`UiState::sync_dirty`]; the window calls this
    /// once per frame, so a burst of engine events costs one refresh, not one
    /// per event.
    pub fn flush_sync(&mut self, core: &Core) {
        if std::mem::take(&mut self.sync_dirty) {
            self.reload_after_sync(core);
        }
    }

    /// Re-reads the folder tree and the message list after the engine wrote to
    /// the database.
    ///
    /// The engine only stores to SQLite; without this the list kept its old
    /// contents and a freshly synchronised mailbox only appeared after a
    /// restart.
    fn reload_after_sync(&mut self, core: &Core) {
        if let Err(error) = self.reload_folders(core) {
            self.error = Some(error.to_string());
            return;
        }
        self.apply_folder_overrides(core);
        if let Err(error) = self.refresh_keeping_window(core) {
            self.error = Some(error.to_string());
        }
    }

    /// Merges one engine event into the state.
    ///
    /// Returns the tasks a handler produced (arrival rules can move, flag or
    /// delete a message), so the caller can dispatch them exactly like the tasks
    /// of a user action.
    pub fn apply_engine(
        &mut self,
        event: &hylmail_core::sync::EngineEvent,
        core: &Core,
    ) -> Vec<Task> {
        use hylmail_core::sync::EngineEvent;
        let mut tasks = Vec::new();
        match event {
            EngineEvent::Connected {
                account_id,
                host,
                encrypted,
                tls_version,
            } => {
                let described = match (tls_version, encrypted) {
                    (Some(version), _) => format!("{host} · {version}"),
                    (None, true) => format!("{host} · verschlüsselt"),
                    (None, false) => format!("{host} · unverschlüsselt"),
                };
                self.connections.insert(*account_id, described);
            }
            EngineEvent::Disconnected {
                account_id,
                reason,
                retrying,
            } => {
                let described = if *retrying {
                    format!("{reason} · neuer Versuch läuft")
                } else {
                    reason.clone()
                };
                self.connections.insert(*account_id, described);
            }
            EngineEvent::Stopped { account_id } => {
                self.connections.remove(account_id);
            }
            EngineEvent::NewMail { account_id, count } => {
                self.new_mail = self
                    .new_mail
                    .saturating_add(u32::try_from(*count).unwrap_or(u32::MAX));
                // Newly arrived mail runs the enabled arrival rules once.
                tasks.extend(self.process_arrival_rules(core, *account_id));
                self.sync_dirty = true;
            }
            EngineEvent::FoldersDiscovered { .. } => self.sync_dirty = true,
            EngineEvent::FolderSynced { report, .. } => {
                // A no-op pass changed nothing, so the list does not have to be
                // re-queried; anything else (new, moved or re-flagged mail) does.
                if !report.is_noop() {
                    self.sync_dirty = true;
                }
            }
            EngineEvent::Error { message } => self.error = Some(message.clone()),
            EngineEvent::Idling { .. } | EngineEvent::Finished => {}
        }
        tasks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hylmail_core::models::{utc_now, Attachment, AuthKind, Endpoint, SortOrder};

    fn account() -> Account {
        Account {
            id: AccountId::new(),
            display_name: "Anna Beispiel".to_string(),
            email: "anna@example.org".to_string(),
            kind: hylmail_core::models::AccountKind::Imap,
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

    fn header(subject: &str, minutes: i64) -> MessageHeader {
        let mut header = MessageHeader::new(AccountId::new(), FolderId::new());
        header.subject = subject.to_string();
        header.from = vec![MailAddress::with_name("bob@example.org", "Bob")];
        header.date = Some(utc_now() + chrono::Duration::minutes(minutes));
        header
    }

    #[test]
    fn recipients_are_split_on_commas_and_semicolons() {
        let parsed = parse_recipients("a@example.org; Bob <b@example.org>, c@example.org");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].email, "a@example.org");
        assert_eq!(parsed[1].email, "b@example.org");
        assert_eq!(parsed[1].name.as_deref(), Some("Bob"));
        assert_eq!(parsed[2].email, "c@example.org");
    }

    #[test]
    fn incomplete_addresses_never_reach_the_envelope() {
        assert!(parse_recipients("Bob, , @").is_empty());
        assert!(parse_recipient("Bob <bob>").is_none());
        assert!(parse_recipient("").is_none());
    }

    #[test]
    fn known_file_extensions_get_a_media_type() {
        assert_eq!(mime_type_for("bericht.PDF"), "application/pdf");
        assert_eq!(mime_type_for("bild.png"), "image/png");
        assert_eq!(mime_type_for("ohne-endung"), "application/octet-stream");
    }

    #[test]
    fn folder_rows_put_parents_before_children() {
        let account_id = AccountId::new();
        let mut state = UiState::new(50);
        let parent = Folder::new(account_id, "Projekte", "Projekte", FolderKind::Custom);
        let mut child = Folder::new(account_id, "Alpha", "Projekte/Alpha", FolderKind::Custom);
        child.parent_id = Some(parent.id);
        let mut late = Folder::new(account_id, "Zeta", "Projekte/Zeta", FolderKind::Custom);
        late.parent_id = Some(parent.id);
        state.folders = vec![child.clone(), late.clone(), parent.clone()];

        let rows = folder_rows(&state.folders, account_id);
        assert_eq!(rows[0], (0, parent.id));
        assert_eq!(rows[1], (1, child.id));
        assert_eq!(rows[2], (1, late.id));
    }

    #[test]
    fn a_folder_with_a_missing_parent_is_promoted_to_the_root() {
        let account_id = AccountId::new();
        let mut state = UiState::new(50);
        let mut orphan = Folder::new(account_id, "Waise", "Waise", FolderKind::Custom);
        orphan.parent_id = Some(FolderId::new());
        state.folders = vec![orphan.clone()];

        let rows = folder_rows(&state.folders, account_id);
        assert_eq!(rows, vec![(0, orphan.id)]);
    }

    #[test]
    fn sorting_by_date_reverses_the_page() {
        let mut state = UiState::new(50);
        state.messages = vec![header("alt", -60), header("neu", 0), header("mittel", -30)];
        state.apply_sort();
        let subjects: Vec<&str> = state
            .messages
            .iter()
            .map(|header| header.subject.as_str())
            .collect();
        assert_eq!(subjects, vec!["neu", "mittel", "alt"]);

        state.ascending = true;
        state.apply_sort();
        let subjects: Vec<&str> = state
            .messages
            .iter()
            .map(|header| header.subject.as_str())
            .collect();
        assert_eq!(subjects, vec!["alt", "mittel", "neu"]);
    }

    #[test]
    fn the_default_view_queries_newest_first() {
        let state = UiState::new(250);
        let query = state.build_query();
        assert_eq!(query.limit, Some(250));
        assert_eq!(query.offset, 0);
        assert_eq!(query.order, SortOrder::NewestFirst);
        assert!(!query.unread_only);
        assert!(query.text.is_none());
    }

    #[test]
    fn search_only_reaches_the_index_from_three_characters_on() {
        let mut state = UiState::new(50);
        state.search = "ab".to_string();
        assert!(state.build_query().text.is_none());
        state.search = " abc ".to_string();
        assert_eq!(state.build_query().text.as_deref(), Some("abc"));
    }

    #[test]
    fn a_groupwise_search_is_bounded_to_the_recent_window() {
        let mut state = UiState::new(50);
        let mut groupwise = account();
        groupwise.kind = AccountKind::GroupWiseProxy;
        let inbox = Folder::new(groupwise.id, "Posteingang", "INBOX", FolderKind::Inbox);
        state.folders = vec![inbox];
        state.active_account = Some(groupwise.id);
        state.accounts = vec![groupwise];
        state.search = "rechnung".to_string();

        let query = state.build_query();
        assert_eq!(query.text.as_deref(), Some("rechnung"));
        assert_eq!(query.window, Some(GROUPWISE_SEARCH_WINDOW));

        // The same search on an IMAP account is not windowed (`FR-SEARCH-4`).
        let mut imap = account();
        imap.id = AccountId::new();
        imap.kind = AccountKind::Imap;
        state.active_account = Some(imap.id);
        state.accounts = vec![imap];
        assert!(state.build_query().window.is_none());
    }

    #[test]
    fn a_selected_folder_replaces_the_aggregate_view() {
        let account_id = AccountId::new();
        let mut state = UiState::new(50);
        let inbox = Folder::new(account_id, "Posteingang", "INBOX", FolderKind::Inbox);
        let sent = Folder::new(account_id, "Gesendet", "Sent", FolderKind::Sent);
        state.folders = vec![inbox.clone(), sent.clone()];

        let aggregate = state.build_query();
        assert_eq!(aggregate.folder_ids, vec![inbox.id]);

        state.selected_folder = Some(sent.id);
        state.all_inboxes = false;
        assert_eq!(state.build_query().folder_ids, vec![sent.id]);
    }

    #[test]
    fn choosing_a_view_clears_the_new_mail_badge() {
        let core = core_stub();
        let account_id = AccountId::new();
        let inbox = Folder::new(account_id, "Posteingang", "INBOX", FolderKind::Inbox);
        let mut state = UiState::new(50);
        state.folders = vec![inbox.clone()];

        state.new_mail = 4;
        state.select_folder(&core, inbox.id).unwrap();
        assert_eq!(state.new_mail, 0, "opening a folder is seeing the new mail");

        state.new_mail = 7;
        state.select_all_inboxes(&core, None).unwrap();
        assert_eq!(state.new_mail, 0);
    }

    #[test]
    fn the_composer_turns_the_typed_rows_into_a_draft() {
        let account = account();
        let mut composer = Composer::new(&account, None);
        composer.to = "Bob <bob@example.org>".to_string();
        composer.cc = "c@example.org".to_string();
        composer.subject = "  Betreff  ".to_string();
        composer.body = "Hallo".to_string();

        let draft = composer.to_draft();
        assert_eq!(draft.to.len(), 1);
        assert_eq!(draft.to[0].email, "bob@example.org");
        assert_eq!(draft.cc[0].email, "c@example.org");
        assert_eq!(draft.subject, "Betreff");
        assert_eq!(draft.body_plain, "Hallo");
        assert!(hylmail_core::compose::is_sendable(&draft));
    }

    #[test]
    fn an_answer_quotes_the_original_and_keeps_the_thread() {
        let account = account();
        let mut original = header("Frage", 0);
        original.message_id = Some("frage@example.org".to_string());
        original.to = vec![MailAddress::new("anna@example.org")];

        let composer = Composer::reply(
            &account,
            None,
            &original,
            Some("Wie geht es dir?"),
            None,
            false,
        );
        assert_eq!(composer.subject, "Re: Frage");
        assert_eq!(composer.to, "Bob <bob@example.org>");
        assert_eq!(composer.in_reply_to.as_deref(), Some("frage@example.org"));
        assert!(composer.body.contains("Wie geht es dir?"));
    }

    #[test]
    fn answering_all_skips_the_own_address() {
        let account = account();
        let mut original = header("Frage", 0);
        original.to = vec![
            MailAddress::new("anna@example.org"),
            MailAddress::new("carl@example.org"),
        ];

        let composer = Composer::reply(&account, None, &original, None, None, true);
        assert!(composer.cc.contains("carl@example.org"));
        assert!(!composer.cc.contains("anna@example.org"));
        assert!(composer.show_cc);
    }

    #[test]
    fn forwarding_prefixes_the_subject() {
        let account = account();
        let composer = Composer::forward(&account, None, &header("Bericht", 0), Some("Inhalt"));
        assert_eq!(composer.subject, "WG: Bericht");
        assert!(composer.body.contains("Inhalt"));
        assert_eq!(composer.mode, ComposeMode::Forward);
    }

    fn attachment(part_id: &str, filename: Option<&str>) -> Attachment {
        Attachment {
            part_id: part_id.to_string(),
            filename: filename.map(str::to_string),
            mime_type: "application/octet-stream".to_string(),
            size_bytes: Some(12),
            content_id: None,
            inline: false,
            is_calendar: false,
        }
    }

    #[test]
    fn an_empty_composer_cannot_be_sent() {
        let account = account();
        let composer = Composer::new(&account, None);
        let draft = composer.to_draft();
        assert!(!hylmail_core::compose::is_sendable(&draft));
        assert!(hylmail_core::compose::send_blocker(&draft).is_some());
    }

    #[test]
    fn an_attachment_of_an_unknown_message_is_not_saved() {
        let mut state = UiState::new(50);
        assert!(state.attachment_target("1").is_none());
        assert!(state.save_all_attachments().is_empty());

        // A message the user is reading, but which carries no attachment.
        let mut reader = Reader::new(header("Leer", 0));
        reader.body = Some(MessageBody::empty(reader.header.id));
        state.reader = Some(reader);
        assert!(state.save_all_attachments().is_empty());
        assert_eq!(state.status, "Diese Nachricht hat keine Anhänge.");
    }

    #[test]
    fn saving_a_listed_attachment_names_the_task_after_it() {
        let mut state = UiState::new(50);
        let mut reader = Reader::new(header("Bericht", 0));
        let message_id = reader.header.id;
        let folder_id = reader.header.folder_id;
        let mut body = MessageBody::empty(message_id);
        body.attachments = vec![attachment("1.2", Some("../bericht.txt"))];
        reader.body = Some(body);
        state.reader = Some(reader);

        let (folder, message, suggestion) = state.attachment_target("1.2").unwrap();
        assert_eq!(folder, folder_id);
        assert_eq!(message, message_id);
        assert_eq!(suggestion, "bericht.txt");
        assert!(state.attachment_target("9").is_none());
    }

    #[test]
    fn deleting_an_inbox_message_goes_through_the_trash() {
        let account_id = AccountId::new();
        let folder = Folder::new(account_id, "Posteingang", "INBOX", FolderKind::Inbox);
        let mut state = UiState::new(50);
        let mut message = header("Werbung", 0);
        message.folder_id = folder.id;
        state.messages = vec![message.clone()];
        state.selected = Some(message.id);
        state.folders = vec![folder];

        let tasks = state.delete_selected();
        assert_eq!(tasks.len(), 1);
        assert!(matches!(
            &tasks[0],
            Task::DeleteMessage {
                message_id,
                permanent: false,
                ..
            } if *message_id == message.id
        ));
    }

    #[test]
    fn deleting_a_message_in_the_trash_is_permanent() {
        let account_id = AccountId::new();
        let folder = Folder::new(account_id, "Papierkorb", "Papierkorb", FolderKind::Trash);
        let mut state = UiState::new(50);
        let mut message = header("Werbung", 0);
        message.folder_id = folder.id;
        state.messages = vec![message.clone()];
        state.selected = Some(message.id);
        state.folders = vec![folder];

        let tasks = state.delete_selected();
        assert_eq!(tasks.len(), 1);
        assert!(matches!(
            &tasks[0],
            Task::DeleteMessage {
                message_id,
                permanent: true,
                ..
            } if *message_id == message.id
        ));
    }

    #[test]
    fn a_delete_without_a_listed_message_does_nothing() {
        let mut state = UiState::new(50);
        assert!(state.delete_selected().is_empty());

        // Selected, but the list no longer holds it: nothing is sent.
        state.selected = Some(MessageId::new());
        assert!(state.delete_selected().is_empty());
    }

    #[test]
    fn a_deleted_message_leaves_the_list_and_closes_the_reader() {
        let mut state = UiState::new(50);
        let message = header("Werbung", 0);
        let other = header("Rechnung", -10);
        state.messages = vec![message.clone(), other.clone()];
        state.selected = Some(message.id);
        state.reader = Some(Reader::new(message.clone()));

        state.forget_message(message.id);

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].id, other.id);
        assert_eq!(state.selected, None);
        assert!(state.reader.is_none());
    }

    #[test]
    fn deleting_another_message_keeps_the_open_reader() {
        let mut state = UiState::new(50);
        let message = header("Werbung", 0);
        let other = header("Rechnung", -10);
        state.messages = vec![message.clone(), other.clone()];
        state.selected = Some(other.id);
        state.reader = Some(Reader::new(other.clone()));

        state.forget_message(message.id);

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.selected, Some(other.id));
        assert!(state.reader.is_some());
    }

    fn oauth_document(oauth: Option<hylmail_core::models::OAuthConfig>) -> AccountsDocument {
        let mut account = account();
        account.auth = AuthKind::OAuth2;
        account.oauth = oauth;
        AccountsDocument {
            accounts: vec![account],
        }
    }

    fn configured_oauth() -> hylmail_core::models::OAuthConfig {
        hylmail_core::models::OAuthConfig {
            issuer: "https://accounts.example.org".to_string(),
            client_id: "client-123".to_string(),
            scopes: vec!["mail.read".to_string()],
            authorization_endpoint: "https://accounts.example.org/authorize".to_string(),
            token_endpoint: "https://accounts.example.org/token".to_string(),
            redirect_port: 0,
        }
    }

    /// A throwaway core; the sign-in tests only need the argument, not a
    /// database.  The temporary directory is leaked so the open database file
    /// is never deleted underneath the connection.
    fn core_stub() -> Core {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        std::mem::forget(dir);
        let env = hylmail_core::paths::MapEnv::new().with(hylmail_core::paths::PORTABLE_ENV, path);
        let paths = hylmail_core::AppPaths::resolve_for(hylmail_core::Os::Linux, &env).unwrap();
        Core::bootstrap(
            paths,
            std::sync::Arc::new(hylmail_core::secrets::InMemorySecretStore::new()),
        )
        .unwrap()
    }

    #[test]
    fn opening_a_message_puts_its_cached_body_into_the_reader() {
        let core = core_stub();
        let folder = Folder::new(AccountId::new(), "Posteingang", "INBOX", FolderKind::Inbox);
        core.database()
            .write(|connection| db_folders::upsert_folder(connection, &folder))
            .unwrap();

        let mut cached = header("Quartalsbericht", 0);
        cached.folder_id = folder.id;
        let mut missing = header("Noch nicht geladen", -1);
        missing.folder_id = folder.id;
        core.database()
            .write(|connection| {
                let mut cached = cached.clone();
                let mut missing = missing.clone();
                db_messages::upsert_message(connection, &mut cached)?;
                db_messages::upsert_message(connection, &mut missing)
            })
            .unwrap();

        let mut body = MessageBody::empty(cached.id);
        body.plain_text = Some("Der Bericht liegt bei.".to_string());
        body.html_raw = Some("<p>Der <b>Bericht</b> liegt bei.</p>".to_string());
        core.database()
            .write(|connection| content::put_content(connection, &body))
            .unwrap();

        let mut state = UiState::new(50);
        state.messages = vec![cached.clone(), missing.clone()];

        // The cached body is on screen immediately, and the unread message is
        // marked as read on the server in the same step.
        let tasks = state.select_message(&core, cached.id);
        let reader = state.reader.as_ref().expect("the reader is open");
        assert!(reader.body.is_some(), "the cached body is shown at once");
        assert!(!reader.loading);
        let document = reader.document.as_ref().expect("the body is rendered");
        assert!(document.plain_text().contains("Bericht"));
        assert!(tasks
            .iter()
            .any(|task| matches!(task, Task::PushFlags { .. })));

        // Without a cached body the window asks the worker for it.
        let tasks = state.select_message(&core, missing.id);
        let reader = state.reader.as_ref().expect("the reader is open");
        assert!(reader.body.is_none());
        assert!(reader.loading, "a missing body is downloaded");
        assert!(tasks
            .iter()
            .any(|task| matches!(task, Task::FetchBody { .. })));
    }

    #[test]
    fn saving_accounts_makes_the_engine_reload_them() {
        let mut core = core_stub();
        let document = AccountsDocument {
            accounts: vec![account()],
        };
        let mut state = UiState::new(50);
        state.accounts_form = Some(AccountsForm::new(&document));

        let tasks = state.save_accounts(&mut core);
        assert!(
            matches!(tasks.as_slice(), [Task::ReloadAccounts]),
            "a saved account must make the engine re-read the account list, \
             otherwise a newly created account never synchronises"
        );
    }

    #[test]
    fn a_sync_event_defers_its_list_refresh_to_the_next_flush() {
        let core = core_stub();
        let mut state = UiState::new(50);
        let event = hylmail_core::sync::EngineEvent::FoldersDiscovered {
            account_id: AccountId::new(),
            count: 3,
        };

        state.apply_engine(&event, &core);
        assert!(state.sync_dirty, "the event asks for a refresh");

        state.flush_sync(&core);
        assert!(!state.sync_dirty, "the flush clears the flag");
    }

    #[test]
    fn saving_accounts_also_stores_a_typed_password() {
        let mut core = core_stub();
        let document = AccountsDocument {
            accounts: vec![account()],
        };
        let account_id = document.accounts[0].id;
        let mut form = AccountsForm::new(&document);
        form.password = "geheim".to_string();
        let mut state = UiState::new(50);
        state.accounts_form = Some(form);

        let tasks = state.save_accounts(&mut core);
        assert!(matches!(tasks.as_slice(), [Task::ReloadAccounts]));
        let stored = core
            .secrets()
            .get(&hylmail_core::secrets::CredentialKey::new(
                account_id,
                CredentialKind::IncomingPassword,
            ))
            .unwrap();
        assert!(stored.is_some(), "the typed password reaches the vault");
        assert!(
            state
                .accounts_form
                .as_ref()
                .is_some_and(|form| form.password.is_empty()),
            "the field is cleared once the password is stored"
        );
    }

    #[test]
    fn saving_settings_asks_the_engine_to_reload_them() {
        let mut core = core_stub();
        let mut state = UiState::new(50);
        state.settings_form = Some(SettingsForm::new(core.settings()));

        let tasks = state.save_settings(&mut core);
        assert!(
            matches!(tasks.as_slice(), [Task::ReloadSettings]),
            "a saved setting must reach the engine without a restart"
        );
    }

    #[test]
    fn a_sync_refresh_keeps_the_rows_the_user_loaded() {
        let core = core_stub();
        let folder = Folder::new(AccountId::new(), "Posteingang", "INBOX", FolderKind::Inbox);
        core.database()
            .write(|connection| db_folders::upsert_folder(connection, &folder))
            .unwrap();
        core.database()
            .write(|connection| {
                for index in 0..5 {
                    let mut header = header(&format!("Mail {index}"), -(index as i64));
                    header.account_id = folder.account_id;
                    header.folder_id = folder.id;
                    db_messages::upsert_message(connection, &mut header)?;
                }
                Ok(())
            })
            .unwrap();

        let mut state = UiState::new(2);
        state.folders = vec![folder.clone()];
        state.selected_folder = Some(folder.id);
        state.all_inboxes = false;
        state.refresh(&core).unwrap();
        assert_eq!(state.loaded, 2, "the first page");
        state.load_more(&core).unwrap();
        assert_eq!(state.loaded, 4, "one page more");

        state.refresh_keeping_window(&core).unwrap();
        assert_eq!(state.loaded, 4, "a sync must not collapse the window");
    }

    #[test]
    fn toggling_the_preview_flips_the_flag() {
        let mut core = core_stub();
        let mut state = UiState::new(50);
        assert!(state.preview_visible);

        let tasks = state.handle(Action::TogglePreview, &mut core);
        assert!(tasks.is_empty());
        assert!(!state.preview_visible);
    }

    #[test]
    fn signing_in_hands_the_edited_account_to_the_worker() {
        let mut state = UiState::new(50);
        let mut account = account();
        account.auth = AuthKind::OAuth2;
        account.oauth = Some(configured_oauth());
        let expected = account.clone();
        state.accounts_form = Some(AccountsForm::new(&AccountsDocument {
            accounts: vec![account],
        }));

        let tasks = state.handle(Action::OAuthSignIn, &mut core_stub());

        match tasks.as_slice() {
            [Task::OAuthLogin { account }] => assert_eq!(**account, expected),
            _ => panic!("expected exactly one sign-in task"),
        }
        let form = state.accounts_form.as_ref().unwrap();
        assert!(form.error.is_none());
        assert!(form.status.is_some());
    }

    #[test]
    fn signing_in_without_endpoints_is_refused_before_the_worker() {
        let mut state = UiState::new(50);
        state.accounts_form = Some(AccountsForm::new(&oauth_document(Some(
            hylmail_core::models::OAuthConfig::default(),
        ))));

        let tasks = state.handle(Action::OAuthSignIn, &mut core_stub());

        assert!(tasks.is_empty());
        let form = state.accounts_form.as_ref().unwrap();
        assert!(form.error.as_deref().unwrap().contains("Client-ID"));
        assert!(form.status.is_none());
    }

    #[test]
    fn signing_in_without_a_configuration_is_refused() {
        let mut state = UiState::new(50);
        state.accounts_form = Some(AccountsForm::new(&oauth_document(None)));
        assert!(state
            .handle(Action::OAuthSignIn, &mut core_stub())
            .is_empty());
        assert!(state.accounts_form.as_ref().unwrap().error.is_some());
    }

    #[test]
    fn signing_in_without_an_account_does_nothing() {
        let mut state = UiState::new(50);
        state.accounts_form = Some(AccountsForm::new(&AccountsDocument::default()));
        assert!(state
            .handle(Action::OAuthSignIn, &mut core_stub())
            .is_empty());
        assert!(state.accounts_form.as_ref().unwrap().error.is_none());
    }

    /// A state with one account, an inbox, an archive and one message.
    fn movable_state() -> (UiState, Folder, Folder, MessageHeader) {
        let account_id = AccountId::new();
        let inbox = Folder::new(account_id, "Posteingang", "INBOX", FolderKind::Inbox);
        let archive = Folder::new(account_id, "Archiv", "Archiv", FolderKind::Archive);
        let mut message = header("Antrag", 0);
        message.folder_id = inbox.id;
        let mut state = UiState::new(50);
        state.folders = vec![inbox.clone(), archive.clone()];
        state.messages = vec![message.clone()];
        state.total = 1;
        state.selected = Some(message.id);
        state.selected_folder = Some(inbox.id);
        (state, inbox, archive, message)
    }

    #[test]
    fn the_move_picker_offers_every_other_delivery_target() {
        let (state, inbox, archive, _) = movable_state();
        let targets: Vec<FolderId> = state
            .move_targets(inbox.id)
            .map(|folder| folder.id)
            .collect();
        assert_eq!(targets, vec![archive.id]);
    }

    #[test]
    fn the_move_picker_keeps_foreign_folders_out() {
        let (mut state, inbox, _, _) = movable_state();
        let foreign = Folder::new(AccountId::new(), "Fremd", "Fremd", FolderKind::Archive);
        state.folders.push(foreign.clone());
        let irrelevant = Folder::new(AccountId::new(), "Weg", "Weg", FolderKind::Custom);
        assert!(!state.move_targets(inbox.id).any(|f| f.id == foreign.id));
        assert!(!state.move_targets(irrelevant.id).any(|f| f.id == inbox.id));
    }

    #[test]
    fn archiving_asks_the_worker_to_move_into_the_archive_folder() {
        let (mut state, _, archive, message) = movable_state();
        let tasks = state.handle(Action::Archive, &mut core_stub());
        match tasks.as_slice() {
            [Task::MoveMessage {
                message_id,
                target_folder_id,
                ..
            }] => {
                assert_eq!(*message_id, message.id);
                assert_eq!(*target_folder_id, archive.id);
            }
            _ => panic!("expected exactly one move task"),
        }
    }

    #[test]
    fn archiving_without_an_archive_folder_says_so() {
        let (mut state, _, _, _) = movable_state();
        state
            .folders
            .retain(|folder| folder.kind != FolderKind::Archive);
        assert!(state.handle(Action::Archive, &mut core_stub()).is_empty());
        assert!(state.error.as_deref().unwrap().contains("Archiv"));
    }

    #[test]
    fn a_move_into_the_current_folder_is_refused() {
        let (mut state, inbox, _, _) = movable_state();
        assert!(state
            .handle(Action::MoveTo { target: inbox.id }, &mut core_stub())
            .is_empty());
        assert!(state.error.is_some());
    }

    #[test]
    fn opening_the_picker_without_a_destination_says_so() {
        let (mut state, _, _, _) = movable_state();
        state
            .folders
            .retain(|folder| folder.kind == FolderKind::Inbox);
        state.handle(Action::OpenMovePicker, &mut core_stub());
        assert!(state.picker.is_none());
        assert!(state.error.is_some());
    }

    #[test]
    fn opening_the_picker_selects_the_current_folder_as_the_source() {
        let (mut state, inbox, _, _) = movable_state();
        state.handle(Action::OpenMovePicker, &mut core_stub());
        let picker = state.picker.as_ref().unwrap();
        assert_eq!(picker.folder_id, inbox.id);
        assert!(picker.filter.is_empty());
    }

    #[test]
    fn the_selected_message_leaves_the_list_after_a_move() {
        let (mut state, _, archive, message) = movable_state();
        state.picker = Some(MovePicker::new(message.folder_id));
        let tasks = state.handle(Action::MoveTo { target: archive.id }, &mut core_stub());
        assert_eq!(tasks.len(), 1);
        assert!(state.picker.is_none());
    }

    #[test]
    fn a_confirmed_move_drops_the_row_and_reports_the_target() {
        let (mut state, inbox, archive, message) = movable_state();
        state.reader = Some(Reader::new(message.clone()));
        state.apply(
            UiEvent::Moved {
                folder_id: inbox.id,
                message_id: message.id,
                target_folder_id: archive.id,
                target_name: "Archiv".to_string(),
                error: None,
            },
            &core_stub(),
        );
        assert!(state.messages.is_empty());
        assert!(state.reader.is_none());
        assert_eq!(state.total, 0);
        assert!(state.status.contains("Archiv"));
        assert!(state.error.is_none());
    }

    #[test]
    fn a_failed_move_keeps_the_row() {
        let (mut state, inbox, archive, message) = movable_state();
        state.apply(
            UiEvent::Moved {
                folder_id: inbox.id,
                message_id: message.id,
                target_folder_id: archive.id,
                target_name: String::new(),
                error: Some("UID MOVE nicht unterstützt".to_string()),
            },
            &core_stub(),
        );
        assert_eq!(state.messages.len(), 1);
        assert!(state.error.as_deref().unwrap().contains("UID MOVE"));
    }

    #[test]
    fn the_filter_box_narrows_the_folder_list() {
        let (_, _, archive, _) = movable_state();
        assert!(MovePicker::passes("", &archive));
        assert!(MovePicker::passes("arch", &archive));
        assert!(!MovePicker::passes("gesendet", &archive));
    }

    #[test]
    fn the_conversation_view_groups_the_list_by_thread() {
        let core = core_stub();
        let account = account();
        let account_id = account.id;
        let folder = Folder::new(account_id, "Posteingang", "INBOX", FolderKind::Inbox);
        core.database()
            .write(|connection| db_folders::upsert_folder(connection, &folder))
            .unwrap();
        core.database()
            .write(|connection| {
                let mut root = MessageHeader::new(account_id, folder.id);
                root.uid = Some(1);
                root.subject = "Frage".to_string();
                root.message_id = Some("<root@example.org>".to_string());
                root.from = vec![MailAddress::new("a@example.org")];
                db_messages::upsert_message(connection, &mut root)?;

                let mut reply = MessageHeader::new(account_id, folder.id);
                reply.uid = Some(2);
                reply.subject = "Re: Frage".to_string();
                reply.message_id = Some("<reply@example.org>".to_string());
                reply.references = vec!["<root@example.org>".to_string()];
                reply.from = vec![MailAddress::new("a@example.org")];
                db_messages::upsert_message(connection, &mut reply)?;
                Ok(())
            })
            .unwrap();

        let mut state = UiState::new(50);
        state.accounts = vec![account];
        state.folders = vec![folder];
        state.active_account = Some(account_id);
        state.threaded = true;
        state.refresh(&core).unwrap();

        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.conversations[0].message_count, 2);
        assert_eq!(state.conversations[0].unread_count, 2);
        assert!(state.conversations[0].is_unread());
    }

    #[test]
    fn running_the_rules_moves_a_matching_message() {
        let core = core_stub();
        let account = account();
        let account_id = account.id;
        let inbox = Folder::new(account_id, "Posteingang", "INBOX", FolderKind::Inbox);
        let archive = Folder::new(account_id, "Archiv", "Archiv", FolderKind::Archive);
        let mut message = header("Rechnung 4711", 0);
        message.account_id = account_id;
        message.folder_id = inbox.id;

        let mut rule = Rule::new("Rechnungen");
        rule.conditions
            .push(hylmail_core::models::rule::Condition::contains(
                hylmail_core::models::rule::ConditionField::Subject,
                "Rechnung",
            ));
        rule.actions.push(hylmail_core::models::Action::MoveTo {
            folder_path: "Archiv".to_string(),
        });
        let mut rules = RuleSet::new();
        rules.add(rule).unwrap();
        core.config().save_rules(&rules).unwrap();

        let mut state = UiState::new(50);
        state.accounts = vec![account];
        state.folders = vec![inbox, archive.clone()];
        state.messages = vec![message.clone()];

        let tasks = state.run_rules(&core);
        match tasks.as_slice() {
            [Task::MoveMessage {
                message_id,
                target_folder_id,
                ..
            }] => {
                assert_eq!(*message_id, message.id);
                assert_eq!(*target_folder_id, archive.id);
            }
            other => panic!("expected one move task, got {}", other.len()),
        }
    }

    #[test]
    fn a_groupwise_rule_never_fires() {
        let core = core_stub();
        let mut account = account();
        account.kind = AccountKind::GroupWiseProxy;
        let account_id = account.id;
        let inbox = Folder::new(account_id, "Posteingang", "INBOX", FolderKind::Inbox);
        let mut message = header("Rechnung 4711", 0);
        message.account_id = account_id;
        message.folder_id = inbox.id;

        let mut rule = Rule::new("Rechnungen");
        rule.actions.push(hylmail_core::models::Action::Delete);
        let mut rules = RuleSet::new();
        rules.add(rule).unwrap();
        core.config().save_rules(&rules).unwrap();

        let mut state = UiState::new(50);
        state.accounts = vec![account];
        state.folders = vec![inbox];
        state.messages = vec![message];
        assert!(
            state.run_rules(&core).is_empty(),
            "rules are off for proxies"
        );
    }

    #[test]
    fn collected_recipients_reach_the_address_book() {
        let core = core_stub();
        let mut draft = Draft::new(AccountId::new());
        draft
            .to
            .push(MailAddress::with_name("bob@example.org", "Bob"));

        let mut state = UiState::new(50);
        state.collect_recipients(&core, &draft);
        state.reload_contacts(&core);

        assert_eq!(state.contacts.len(), 1);
        assert_eq!(state.contacts[0].primary_email(), Some("bob@example.org"));
        assert_eq!(state.contacts[0].label(), "Bob");
    }

    #[test]
    fn the_address_book_filter_matches_name_and_address() {
        let mut form = ContactsForm::new(Vec::new());
        let mut ada = Contact::collected(&MailAddress::with_name("ada@example.org", "Ada"));
        ada.organization = Some("Example University".to_string());
        form.contacts.push(ada);
        form.contacts
            .push(Contact::collected(&MailAddress::new("bob@example.org")));

        form.filter = "ada".to_string();
        assert_eq!(form.visible(), vec![0]);
        form.filter = "bob@".to_string();
        assert_eq!(form.visible(), vec![1]);
        form.filter.clear();
        assert_eq!(form.visible(), vec![0, 1]);
    }

    #[test]
    fn dragging_a_message_onto_a_folder_moves_it() {
        let (mut state, _inbox, archive, message) = movable_state();
        let tasks = state.move_dragged_to(message.id, archive.id);
        match tasks.as_slice() {
            [Task::MoveMessage {
                message_id,
                target_folder_id,
                ..
            }] => {
                assert_eq!(*message_id, message.id);
                assert_eq!(*target_folder_id, archive.id);
            }
            other => panic!("expected one move task, got {}", other.len()),
        }

        // A folder of another account can never receive the drag.
        let foreign = Folder::new(AccountId::new(), "Fremd", "Fremd", FolderKind::Archive);
        state.folders.push(foreign.clone());
        assert!(state.move_dragged_to(message.id, foreign.id).is_empty());
        assert!(state.error.is_some());
    }

    #[test]
    fn answering_an_invitation_attaches_a_calendar_reply() {
        const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//DE\r\n\
            METHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:1@example.org\r\n\
            DTSTART:20261120T140000Z\r\nSUMMARY:Standup\r\n\
            ORGANIZER;CN=Ada:mailto:ada@example.org\r\n\
            ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:anna@example.org\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";

        let mut core = core_stub();
        let account = account();
        let mut message = header("Standup", 0);
        message.account_id = account.id;
        message.is_calendar_invite = true;
        let message_id = message.id;

        let mut body = MessageBody::empty(message_id);
        body.calendar_ics = Some(ICS.to_string());

        let mut reader = Reader::new(message);
        reader.calendar = Some(hylmail_core::calendar::parse(ICS).unwrap());
        reader.body = Some(body);

        let mut state = UiState::new(50);
        state.accounts = vec![account];
        state.reader = Some(reader);

        let tasks = state.handle(
            Action::CalendarReply {
                status: hylmail_core::calendar::PartStat::Accepted,
            },
            &mut core,
        );
        match tasks.as_slice() {
            [Task::Submit { draft, .. }] => {
                assert_eq!(draft.to.len(), 1);
                assert_eq!(draft.to[0].email, "ada@example.org");
                assert_eq!(draft.subject, "Re: Standup");
                assert_eq!(draft.attachments.len(), 1);
                assert!(draft.attachments[0].mime_type.contains("text/calendar"));
                let written = std::fs::read_to_string(&draft.attachments[0].path).unwrap();
                assert!(written.contains("METHOD:REPLY"), "{written}");
                assert!(written.contains("PARTSTAT=ACCEPTED"), "{written}");
                assert!(!written.to_ascii_uppercase().contains("RSVP=TRUE"));
            }
            other => panic!("expected a submit task, got {}", other.len()),
        }
        // Housekeeping: the temporary reply file is under the system temp dir.
        if let [Task::Submit { draft, .. }] = tasks.as_slice() {
            let _ = std::fs::remove_file(&draft.attachments[0].path);
        }
    }

    #[test]
    fn an_invitation_without_an_organiser_cannot_be_answered() {
        const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
            BEGIN:VEVENT\r\nUID:9@example.org\r\nDTSTART:20261120T140000Z\r\n\
            SUMMARY:Ohne\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let mut core = core_stub();
        let account = account();
        let mut message = header("Ohne", 0);
        message.account_id = account.id;

        let mut body = MessageBody::empty(message.id);
        body.calendar_ics = Some(ICS.to_string());
        let mut reader = Reader::new(message);
        reader.calendar = Some(hylmail_core::calendar::parse(ICS).unwrap());
        reader.body = Some(body);

        let mut state = UiState::new(50);
        state.accounts = vec![account];
        state.reader = Some(reader);
        assert!(state
            .handle(
                Action::CalendarReply {
                    status: hylmail_core::calendar::PartStat::Declined,
                },
                &mut core,
            )
            .is_empty());
        assert!(state.error.is_some());
    }

    #[test]
    fn a_composer_that_is_closed_keeps_a_local_draft() {
        let core = core_stub();
        let account = account();
        let mut composer = Composer::new(&account, None);
        composer.to = "bob@example.org".to_string();
        composer.subject = "Bericht".to_string();
        composer.body = "Hallo".to_string();
        let mut state = UiState::new(50);
        state.composer = Some(composer);

        state.save_draft(&core);
        // A second save updates the same entry instead of adding a new one.
        state.save_draft(&core);

        let drafts = core.config().load_drafts().unwrap();
        assert_eq!(drafts.drafts.len(), 1);
        assert_eq!(drafts.drafts[0].subject, "Bericht");
        assert_eq!(
            drafts.drafts[0].id,
            state.composer.as_ref().unwrap().draft_id.unwrap()
        );
    }

    #[test]
    fn a_delete_is_held_back_until_it_is_confirmed() {
        let (mut state, _, _, message) = movable_state();

        let tasks = state.handle(Action::Delete, &mut core_stub());
        assert!(tasks.is_empty(), "the prompt must come before the delete");
        assert_eq!(state.pending_delete, vec![message.id]);

        let tasks = state.handle(Action::ConfirmDelete, &mut core_stub());
        assert_eq!(tasks.len(), 1);
        assert!(matches!(tasks[0], Task::DeleteMessage { .. }));
        assert!(state.pending_delete.is_empty());
    }

    #[test]
    fn a_checked_box_adds_a_bulk_target_and_unchecking_removes_it() {
        let (mut state, _, _, message) = movable_state();
        state.handle(Action::ToggleSelection(message.id), &mut core_stub());
        assert!(state.selected_ids.contains(&message.id));
        state.handle(Action::ToggleSelection(message.id), &mut core_stub());
        assert!(state.selected_ids.is_empty());
    }

    #[test]
    fn select_all_ticks_every_loaded_message_and_clear_empties_it() {
        let (mut state, _, _, message) = movable_state();
        let mut second = header("Zweite", -1);
        second.folder_id = message.folder_id;
        state.messages.push(second.clone());

        state.handle(Action::SelectAll, &mut core_stub());
        assert_eq!(state.selected_ids.len(), 2);
        assert!(state.selected_ids.contains(&message.id));
        assert!(state.selected_ids.contains(&second.id));

        state.handle(Action::ClearSelection, &mut core_stub());
        assert!(state.selected_ids.is_empty());
    }

    #[test]
    fn a_bulk_delete_asks_once_and_then_deletes_the_whole_batch() {
        let (mut state, _, _, first) = movable_state();
        let mut second = header("Zweite", -1);
        second.folder_id = first.folder_id;
        state.messages.push(second.clone());
        state.selected_ids = [first.id, second.id].into_iter().collect();

        let tasks = state.handle(Action::Delete, &mut core_stub());
        assert!(tasks.is_empty(), "the prompt must come before the delete");
        assert_eq!(state.pending_delete.len(), 2);

        let tasks = state.handle(Action::ConfirmDelete, &mut core_stub());
        assert_eq!(tasks.len(), 2, "one delete per ticked message");
        assert!(tasks
            .iter()
            .all(|task| matches!(task, Task::DeleteMessage { .. })));
    }

    #[test]
    fn a_bulk_flag_change_touches_every_ticked_message() {
        let (mut state, _, _, first) = movable_state();
        let mut second = header("Zweite", -1);
        second.folder_id = first.folder_id;
        state.messages.push(second.clone());
        state.selected_ids = [first.id, second.id].into_iter().collect();

        let tasks = state.handle(Action::MarkRead, &mut core_stub());
        assert_eq!(tasks.len(), 2, "one flag push per ticked message");
    }

    #[test]
    fn the_done_key_archives_when_the_mode_is_archive() {
        let (mut state, _, archive, message) = movable_state();
        let tasks = state.handle(Action::Done, &mut core_stub());
        match tasks.as_slice() {
            [Task::MoveMessage {
                message_id,
                target_folder_id,
                ..
            }] => {
                assert_eq!(*message_id, message.id);
                assert_eq!(*target_folder_id, archive.id);
            }
            other => panic!("expected one move task, got {}", other.len()),
        }
    }

    #[test]
    fn an_arrival_rule_moves_newly_stored_mail() {
        let core = core_stub();
        let account = account();
        let account_id = account.id;
        let inbox = Folder::new(account_id, "Posteingang", "INBOX", FolderKind::Inbox);
        let archive = Folder::new(account_id, "Archiv", "Archiv", FolderKind::Archive);
        let mut message = header("Rechnung 4711", 0);
        message.account_id = account_id;
        message.folder_id = inbox.id;
        core.database()
            .write(|connection| {
                db_folders::upsert_folder(connection, &inbox)?;
                db_folders::upsert_folder(connection, &archive)?;
                let mut message = message.clone();
                db_messages::upsert_message(connection, &mut message)
            })
            .unwrap();

        let mut rule = Rule::new("Rechnungen");
        rule.conditions
            .push(hylmail_core::models::rule::Condition::contains(
                hylmail_core::models::rule::ConditionField::Subject,
                "Rechnung",
            ));
        rule.actions.push(hylmail_core::models::Action::MoveTo {
            folder_path: "Archiv".to_string(),
        });
        let mut rules = RuleSet::new();
        rules.add(rule).unwrap();
        core.config().save_rules(&rules).unwrap();

        let mut state = UiState::new(50);
        state.accounts = vec![account];
        state.folders = vec![inbox, archive.clone()];
        // `processed` is deliberately empty, so the stored message is "new".

        let tasks = state.process_arrival_rules(&core, account_id);
        match tasks.as_slice() {
            [Task::MoveMessage {
                target_folder_id, ..
            }] => assert_eq!(*target_folder_id, archive.id),
            other => panic!("expected one move task, got {}", other.len()),
        }

        // A second pass must not touch the same message again.
        assert!(state.process_arrival_rules(&core, account_id).is_empty());
    }

    #[test]
    fn a_newsletter_keeps_its_colours() {
        let mut reader = Reader::new(header("Newsletter", 0));
        let mut body = MessageBody::empty(reader.header.id);
        body.image_count = 5;
        reader.body = Some(body);
        reader.resanitise(&Settings::default());
        assert!(reader.original_colours, "five images is a newsletter");

        let mut reader = Reader::new(header("Einfach", 0));
        let mut body = MessageBody::empty(reader.header.id);
        body.image_count = 1;
        reader.body = Some(body);
        reader.resanitise(&Settings::default());
        assert!(
            !reader.original_colours,
            "a single image is a simple message"
        );

        // Without the dark-mode transformation every message keeps its colours.
        let settings = Settings {
            dark_mode_transform: false,
            ..Settings::default()
        };
        let mut reader = Reader::new(header("Einfach", 0));
        reader.body = Some(MessageBody::empty(reader.header.id));
        reader.resanitise(&settings);
        assert!(reader.original_colours);
    }
}
