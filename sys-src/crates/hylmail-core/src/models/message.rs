//! Message headers, bodies, attachments and the query description used by the
//! virtualised message list.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::ids::{AccountId, FolderId, MessageId};

bitflags::bitflags! {
    /// Flags a message carries on the server (or, for the proxy account, the
    /// subset the client is able to mirror).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct MessageFlags: u32 {
        /// `\Seen`
        const SEEN = 1 << 0;
        /// `\Answered` - the message has been replied to.
        const ANSWERED = 1 << 1;
        /// `\Flagged`
        const FLAGGED = 1 << 2;
        /// `\Draft`
        const DRAFT = 1 << 3;
        /// `\Deleted` - marked for deletion, waiting for `EXPUNGE`.
        const DELETED = 1 << 4;
        /// `$Forwarded`
        const FORWARDED = 1 << 5;
        /// `\Recent`
        const RECENT = 1 << 6;
        /// `$Done` - the IMAP keyword used by the "done" feature.
        const DONE = 1 << 7;
        /// Message contains an `text/calendar` part with a `METHOD:REQUEST`.
        const INVITE = 1 << 8;
        /// Message is part of a thread that has unread replies.
        const THREAD_UNREAD = 1 << 9;
    }
}

impl MessageFlags {
    /// Flags that are synchronised with an IMAP server.
    #[must_use]
    pub const fn imap_representable(self) -> Self {
        Self::from_bits_truncate(self.bits() & 0b1111_1111)
    }

    /// Parses a set of IMAP flag atoms such as `["\\Seen", "$Done"]`.
    #[must_use]
    pub fn from_imap_atoms<'a, I>(atoms: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut flags = Self::empty();
        for atom in atoms {
            flags |= match atom.trim().to_ascii_lowercase().as_str() {
                "\\seen" => Self::SEEN,
                "\\answered" => Self::ANSWERED,
                "\\flagged" => Self::FLAGGED,
                "\\draft" => Self::DRAFT,
                "\\deleted" => Self::DELETED,
                "$forwarded" => Self::FORWARDED,
                "\\recent" => Self::RECENT,
                "$done" | "$completed" => Self::DONE,
                _ => Self::empty(),
            };
        }
        flags
    }

    /// Whether the message has been read.
    #[must_use]
    pub const fn is_seen(self) -> bool {
        self.contains(Self::SEEN)
    }

    /// Whether the user replied to the message.
    #[must_use]
    pub const fn is_answered(self) -> bool {
        self.contains(Self::ANSWERED)
    }

    /// Whether the message is flagged / starred.
    #[must_use]
    pub const fn is_flagged(self) -> bool {
        self.contains(Self::FLAGGED)
    }

    /// Whether the user marked the message as done.
    #[must_use]
    pub const fn is_done(self) -> bool {
        self.contains(Self::DONE)
    }
}

impl Default for MessageFlags {
    fn default() -> Self {
        Self::empty()
    }
}

bitflags::bitflags! {
    /// State that is *only* stored locally.
    ///
    /// GroupWise proxy accounts have no way of persisting "flagged" or "done"
    /// state on the server, so those two bits are kept in SQLite for every
    /// account kind and never written back to a protocol.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct LocalFlags: u32 {
        /// The user flagged the message locally.
        const FLAGGED = 1 << 0;
        /// The user marked the message as done locally.
        const DONE = 1 << 1;
        /// The user pinned the message locally.
        const PINNED = 1 << 2;
        /// The message has been offered to the rule engine already.
        const RULES_APPLIED = 1 << 3;
    }
}

impl LocalFlags {
    /// `true` when the message is flagged in the local store.
    #[must_use]
    pub const fn is_flagged(self) -> bool {
        self.contains(Self::FLAGGED)
    }

    /// `true` when the message is marked done in the local store.
    #[must_use]
    pub const fn is_done(self) -> bool {
        self.contains(Self::DONE)
    }
}

impl Default for LocalFlags {
    fn default() -> Self {
        Self::empty()
    }
}

/// A single mailbox participant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MailAddress {
    /// Display name as it appeared in the header, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The bare `addr-spec`, always lower-cased for comparison purposes.
    pub email: String,
}

impl MailAddress {
    /// Creates an address without a display name.
    #[must_use]
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            name: None,
            email: email.into(),
        }
    }

    /// Creates an address with a display name.
    #[must_use]
    pub fn with_name(email: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            email: email.into(),
        }
    }

    /// Value for the `To:`-style header, quoting the display name if needed.
    #[must_use]
    pub fn to_header_value(&self) -> String {
        match &self.name {
            None => self.email.clone(),
            Some(name) if name.trim().is_empty() => self.email.clone(),
            Some(name) => format!("\"{}\" <{}>", name.replace('"', "'"), self.email),
        }
    }

    /// The part before the `@`, used to build the autocomplete label.
    #[must_use]
    pub fn local_part(&self) -> &str {
        self.email.split('@').next().unwrap_or(&self.email)
    }
}

impl std::fmt::Display for MailAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) if !name.trim().is_empty() => write!(f, "{name} <{}>", self.email),
            _ => write!(f, "{}", self.email),
        }
    }
}

impl From<&str> for MailAddress {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// One MIME part that can be offered as a download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Stable MIME part identifier, e.g. `1.2`.
    pub part_id: String,
    /// File name from `Content-Disposition` or `Content-Type` `name=`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Declared media type.
    pub mime_type: String,
    /// Size in bytes, when the parser could determine it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// `Content-ID` for `cid:` inline images, without angle brackets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// `true` when the part is referenced from the HTML body.
    #[serde(default)]
    pub inline: bool,
    /// `true` when the part carries an `text/calendar` payload.
    #[serde(default)]
    pub is_calendar: bool,
}

impl Attachment {
    /// Name to show in the attachment list.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.filename
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("unnamed")
    }

    /// Whether the part may be rendered inside the message pane.
    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime_type.to_ascii_lowercase().starts_with("image/")
    }

    /// A file name that is safe to create inside a directory the user chose.
    ///
    /// See the free function [`safe_file_name`] for the rules.
    #[must_use]
    pub fn safe_file_name(&self) -> String {
        safe_file_name(self.display_name())
    }
}

/// A file name that is safe to create inside a directory the user chose.
///
/// The name comes from the message, and the message comes from whoever sent
/// it: `filename="../../.ssh/authorized_keys"` would otherwise write outside
/// the chosen folder.  Only the last path component survives, everything
/// Windows forbids becomes `_`, and a name that is empty or nothing but dots
/// falls back to `anhang`.
#[must_use]
pub fn safe_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut cleaned = String::with_capacity(base.len());
    for character in base.chars() {
        if character.is_control() || "<>:\"|?*".contains(character) {
            cleaned.push('_');
        } else {
            cleaned.push(character);
        }
    }
    let cleaned = cleaned.trim();
    let cleaned = cleaned.trim_start_matches('.').trim_end_matches(['.', ' ']);
    if cleaned.is_empty() {
        "anhang".to_string()
    } else {
        cleaned.to_string()
    }
}

/// One [`Attachment::safe_file_name`] per attachment, without collisions.
///
/// Two attachments called `bericht.txt` are a normal thing to receive; they
/// become `bericht (2).txt` instead of the second silently replacing the
/// first.  The comparison ignores case, because two of the three supported
/// platforms do.
#[must_use]
pub fn unique_file_names(attachments: &[Attachment]) -> Vec<String> {
    let mut used: Vec<String> = Vec::with_capacity(attachments.len());
    let mut names: Vec<String> = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let base = attachment.safe_file_name();
        let (stem, extension) = split_extension(&base);
        let mut candidate = base.clone();
        let mut counter = 2;
        while used
            .iter()
            .any(|taken| taken.eq_ignore_ascii_case(&candidate))
        {
            candidate = format!("{stem} ({counter}){extension}");
            counter += 1;
        }
        used.push(candidate.clone());
        names.push(candidate);
    }
    names
}

/// Split a file name into stem and extension, keeping the dot.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(index) => name.split_at(index),
    }
}

/// Cached body of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    /// Message this body belongs to.
    pub message_id: MessageId,
    /// `text/plain` rendering, either from the message or generated from HTML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plain_text: Option<String>,
    /// Original `text/html` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_raw: Option<String>,
    /// Sanitised HTML produced by the `ammonia` pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_sanitized: Option<String>,
    /// All parts offered for download.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Serialised `text/calendar` part, if the message is an invitation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar_ics: Option<String>,
    /// `true` while external images are still blocked.
    #[serde(default = "default_true")]
    pub external_images_blocked: bool,
    /// Number of distinct images found in the body.
    #[serde(default)]
    pub image_count: usize,
    /// Size of the cached representation, used for LRU eviction.
    #[serde(default)]
    pub cached_bytes: u64,
    /// When the body entered the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<DateTime<Utc>>,
}

fn default_true() -> bool {
    true
}

impl MessageBody {
    /// Creates an empty body shell for the given message.
    #[must_use]
    pub fn empty(message_id: MessageId) -> Self {
        Self {
            message_id,
            plain_text: None,
            html_raw: None,
            html_sanitized: None,
            attachments: Vec::new(),
            calendar_ics: None,
            external_images_blocked: true,
            image_count: 0,
            cached_bytes: 0,
            fetched_at: None,
        }
    }

    /// The best available text representation.
    #[must_use]
    pub fn best_text(&self) -> Option<&str> {
        self.plain_text
            .as_deref()
            .or(self.html_sanitized.as_deref())
            .or(self.html_raw.as_deref())
    }

    /// Updates [`MessageBody::cached_bytes`] from the current content.
    pub fn recompute_size(&mut self) {
        let mut total = 0u64;
        for text in [
            self.plain_text.as_deref(),
            self.html_raw.as_deref(),
            self.html_sanitized.as_deref(),
            self.calendar_ics.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            total = total.saturating_add(text.len() as u64);
        }
        self.cached_bytes = total;
    }

    /// `true` when the body holds no renderable content yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plain_text.is_none()
            && self.html_raw.is_none()
            && self.html_sanitized.is_none()
            && self.attachments.is_empty()
    }
}

/// Sort direction of a message query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Newest message first (the default for a mailbox view).
    #[default]
    NewestFirst,
    /// Oldest message first.
    OldestFirst,
}

/// Everything the header carries, mirroring row for row the `messages` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHeader {
    /// Local identifier.
    pub id: MessageId,
    /// Owning account.
    pub account_id: AccountId,
    /// Folder the message currently lives in.
    pub folder_id: FolderId,
    /// IMAP UID (absent for EWS / GroupWise proxied messages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Server side identifier for non-IMAP accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    /// RFC 5322 `Message-ID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// RFC 5322 `In-Reply-To`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// RFC 5322 `References` chain, oldest first.
    #[serde(default)]
    pub references: Vec<String>,
    /// Locally computed conversation identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Unprefixed subject line.
    #[serde(default)]
    pub subject: String,
    /// Normalised subject (`Re:`/`Fwd:` stripped) used for threading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_normalized: Option<String>,
    /// `From:` addresses (a syntactically invalid message may carry several).
    #[serde(default)]
    pub from: Vec<MailAddress>,
    /// `To:` addresses.
    #[serde(default)]
    pub to: Vec<MailAddress>,
    /// `Cc:` addresses.
    #[serde(default)]
    pub cc: Vec<MailAddress>,
    /// `Bcc:` addresses - never rendered for received mail, kept for drafts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<MailAddress>,
    /// `Reply-To:` addresses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_to: Vec<MailAddress>,
    /// `Date:` header converted to UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<DateTime<Utc>>,
    /// `INTERNALDATE` / received timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at: Option<DateTime<Utc>>,
    /// Size of the message as reported by the server.
    #[serde(default)]
    pub size_bytes: u64,
    /// Server side flags.
    #[serde(default)]
    pub flags: MessageFlags,
    /// Flags that only exist in the local database.
    #[serde(default)]
    pub local_flags: LocalFlags,
    /// `true` when at least one non-inline attachment exists.
    #[serde(default)]
    pub has_attachments: bool,
    /// First ~200 characters of the body, used as a list preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// `true` when the message carries a calendar invitation.
    #[serde(default)]
    pub is_calendar_invite: bool,
    /// `true` once the body has been written to the content cache.
    #[serde(default)]
    pub body_cached: bool,
    /// Last time this row was touched.
    #[serde(default = "crate::models::ids::utc_now")]
    pub updated_at: DateTime<Utc>,
}

impl MessageHeader {
    /// Creates a header skeleton for a message inside `folder_id`.
    #[must_use]
    pub fn new(account_id: AccountId, folder_id: FolderId) -> Self {
        Self {
            id: MessageId::new(),
            account_id,
            folder_id,
            uid: None,
            server_id: None,
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_id: None,
            subject: String::new(),
            subject_normalized: None,
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            date: None,
            received_at: None,
            size_bytes: 0,
            flags: MessageFlags::empty(),
            local_flags: LocalFlags::empty(),
            has_attachments: false,
            snippet: None,
            is_calendar_invite: false,
            body_cached: false,
            updated_at: Utc::now(),
        }
    }

    /// The address shown in the `Von` column.
    #[must_use]
    pub fn sender(&self) -> Option<&MailAddress> {
        self.from.first()
    }

    /// Returns the effective conversation key.
    ///
    /// The specification requires threading strictly inside a single account,
    /// therefore the key is derived from the `References` chain, falling back
    /// to `In-Reply-To` and finally the own `Message-ID`.  The account id is
    /// part of the returned string so that two accounts can never merge.
    #[must_use]
    pub fn conversation_key(&self) -> String {
        let root = self
            .references
            .first()
            .cloned()
            .or_else(|| self.in_reply_to.clone())
            .or_else(|| self.message_id.clone())
            .unwrap_or_else(|| self.id.to_string());
        format!("{}:{}", self.account_id, normalize_message_id(&root))
    }

    /// Whether the message should be counted as unread in the folder tree.
    #[must_use]
    pub fn is_unread(&self) -> bool {
        !self.flags.is_seen()
    }

    /// `true` when the message is flagged either server side or locally.
    #[must_use]
    pub fn is_flagged(&self) -> bool {
        self.flags.is_flagged() || self.local_flags.is_flagged()
    }

    /// `true` when the message was answered (`\Answered`).
    #[must_use]
    pub fn is_answered(&self) -> bool {
        self.flags.is_answered()
    }

    /// `true` when the message counts as "done" in either mode.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.flags.is_done() || self.local_flags.is_done()
    }

    /// The effective subject, never empty.
    #[must_use]
    pub fn subject_or_placeholder(&self) -> &str {
        if self.subject.trim().is_empty() {
            "(kein Betreff)"
        } else {
            &self.subject
        }
    }

    /// Timestamp used for sorting; falls back to `received_at` and then epoch.
    #[must_use]
    pub fn sort_timestamp(&self) -> i64 {
        self.date
            .or(self.received_at)
            .map(|value| value.timestamp_millis())
            .unwrap_or(0)
    }

    /// Recomputes [`MessageHeader::thread_id`] and
    /// [`MessageHeader::subject_normalized`].
    pub fn refresh_derived_fields(&mut self) {
        self.thread_id = Some(self.conversation_key());
        self.subject_normalized = Some(normalize_subject(&self.subject));
    }
}

/// Strips the leading reply/forward prefixes from a subject.
#[must_use]
pub fn normalize_subject(subject: &str) -> String {
    let mut current = subject.trim().to_string();
    loop {
        let lowered = current.to_ascii_lowercase();
        let mut stripped: Option<String> = None;
        for prefix in ["re:", "aw:", "fwd:", "fw:", "wg:", "antw:", "tr:"] {
            if let Some(rest) = lowered.strip_prefix(prefix) {
                let rest = rest.trim_start();
                if !rest.is_empty() {
                    stripped = Some(rest.to_string());
                }
                break;
            }
        }
        match stripped {
            Some(rest) => current = rest,
            None => break,
        }
    }
    current.to_ascii_lowercase()
}

/// Normalises an RFC 5322 message id (trims whitespace and angle brackets).
#[must_use]
pub fn normalize_message_id(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_ascii_lowercase()
}

/// Everything the message list and the local index need to execute a query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageQuery {
    /// Restrict to a single account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<AccountId>,
    /// Restrict to a set of folders; empty means "all folders".
    #[serde(default)]
    pub folder_ids: Vec<FolderId>,
    /// Only unread messages.
    #[serde(default)]
    pub unread_only: bool,
    /// Only flagged messages.
    #[serde(default)]
    pub flagged_only: bool,
    /// Only messages with attachments.
    #[serde(default)]
    pub has_attachment: bool,
    /// Only messages marked done.
    #[serde(default)]
    pub done_only: bool,
    /// Exclude messages marked done.
    #[serde(default)]
    pub exclude_done: bool,
    /// Full text search term evaluated against the FTS5 index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Consider only the newest `window` messages of the query scope.
    ///
    /// A backend that cannot search on the server has to filter its local
    /// cache; the GroupWise proxy is bounded to a recent window for that
    /// (`BR-GW-3`).  `None` searches the whole cached folder or account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u32>,
    /// Maximum number of rows; `None` means "no limit" (used by tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Number of rows to skip, for the "load more" paging.
    #[serde(default)]
    pub offset: u32,
    /// Sort direction.
    #[serde(default)]
    pub order: SortOrder,
}

impl MessageQuery {
    /// Creates a query returning the newest messages of a folder.
    #[must_use]
    pub fn in_folder(folder_id: FolderId, limit: u32) -> Self {
        Self {
            folder_ids: vec![folder_id],
            limit: Some(limit),
            order: SortOrder::NewestFirst,
            ..Self::default()
        }
    }

    /// Restricts the query to unread messages.
    #[must_use]
    pub fn unread(mut self) -> Self {
        self.unread_only = true;
        self
    }

    /// Restricts the query to messages carrying an attachment.
    #[must_use]
    pub fn with_attachment(mut self) -> Self {
        self.has_attachment = true;
        self
    }

    /// Adds a full text search term.
    #[must_use]
    pub fn matching(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Bounds the query to the newest `window` messages of its scope.
    #[must_use]
    pub fn within(mut self, window: u32) -> Self {
        self.window = Some(window);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(part_id: &str, filename: Option<&str>) -> Attachment {
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
    fn a_path_in_a_file_name_cannot_escape_the_target_folder() {
        assert_eq!(
            safe_file_name("../../.ssh/authorized_keys"),
            "authorized_keys"
        );
        assert_eq!(
            named("1", Some(r"..\..\Windows\System32\evil.exe")).safe_file_name(),
            "evil.exe"
        );
        assert_eq!(safe_file_name("/etc/passwd"), "passwd");
        assert_eq!(safe_file_name("..\\..\\notiz"), "notiz");
    }

    #[test]
    fn forbidden_characters_become_underscores() {
        assert_eq!(safe_file_name("g|h?i*j.txt"), "g_h_i_j.txt");
        assert_eq!(safe_file_name("a<b>c:d\"e.txt"), "a_b_c_d_e.txt");
        assert!(!safe_file_name("a\u{7}b.txt").contains('\u{7}'));
    }

    #[test]
    fn a_name_with_nothing_left_falls_back() {
        for raw in ["", "   ", ".", "..", "...", " . . ", "////"] {
            assert_eq!(safe_file_name(raw), "anhang", "for {raw:?}");
        }
        assert_eq!(safe_file_name("bericht.txt. . "), "bericht.txt");
    }

    #[test]
    fn an_empty_filename_is_shown_as_unnamed() {
        // `display_name` decides what the list shows; the file name follows it.
        assert_eq!(named("1", Some("   ")).safe_file_name(), "unnamed");
    }

    #[test]
    fn an_attachment_without_a_name_is_still_writable() {
        assert_eq!(named("1", None).safe_file_name(), "unnamed");
    }

    #[test]
    fn duplicate_file_names_are_numbered_apart() {
        let attachments = vec![
            named("1", Some("bericht.txt")),
            named("2", Some("Bericht.TXT")),
            named("3", Some("bericht.txt")),
            named("4", None),
        ];
        let names = unique_file_names(&attachments);
        assert_eq!(names[0], "bericht.txt");
        assert_eq!(names[1], "Bericht (2).TXT");
        assert_eq!(names[2], "bericht (3).txt");
        assert_eq!(names[3], "unnamed");
    }

    #[test]
    fn a_name_without_an_extension_is_still_numbered() {
        let attachments = vec![named("1", Some("notiz")), named("2", Some("notiz"))];
        assert_eq!(unique_file_names(&attachments), ["notiz", "notiz (2)"]);
    }

    #[test]
    fn flag_bits_are_independent() {
        let flags = MessageFlags::SEEN | MessageFlags::FLAGGED;
        assert!(flags.is_seen());
        assert!(flags.is_flagged());
        assert!(!flags.is_answered());
        assert!(!flags.is_done());
    }

    #[test]
    fn imap_atoms_are_parsed_case_insensitively() {
        let flags = MessageFlags::from_imap_atoms(["\\Seen", "$Done", "\\ANSWERED", "garbage"]);
        assert!(flags.is_seen());
        assert!(flags.is_done());
        assert!(flags.is_answered());
        assert!(!flags.is_flagged());
    }

    #[test]
    fn imap_representable_strips_local_only_bits() {
        let flags = MessageFlags::SEEN | MessageFlags::THREAD_UNREAD;
        assert_eq!(flags.imap_representable(), MessageFlags::SEEN);
    }

    #[test]
    fn local_flags_track_proxy_state() {
        let mut local = LocalFlags::empty();
        assert!(!local.is_flagged());
        local |= LocalFlags::FLAGGED;
        assert!(local.is_flagged());
        assert!(!local.is_done());
    }

    #[test]
    fn addresses_render_and_compare() {
        let address = MailAddress::with_name("ada@example.org", "Ada Lovelace");
        assert_eq!(
            address.to_header_value(),
            "\"Ada Lovelace\" <ada@example.org>"
        );
        assert_eq!(address.to_string(), "Ada Lovelace <ada@example.org>");
        assert_eq!(address.local_part(), "ada");

        let plain = MailAddress::new("ada@example.org");
        assert_eq!(plain.to_header_value(), "ada@example.org");
        assert_eq!(plain.to_string(), "ada@example.org");
    }

    #[test]
    fn address_quoting_escapes_embedded_quotes() {
        let address = MailAddress::with_name("a@b.c", "He said \"hi\"");
        assert_eq!(address.to_header_value(), "\"He said 'hi'\" <a@b.c>");
    }

    #[test]
    fn subject_normalisation_strips_reply_prefixes() {
        assert_eq!(normalize_subject("Re: Re: AW: Hello"), "hello");
        assert_eq!(
            normalize_subject("Fwd:  Quarterly report"),
            "quarterly report"
        );
        assert_eq!(normalize_subject("Hello"), "hello");
        // A subject consisting only of prefixes must survive.
        assert_eq!(normalize_subject("Re:"), "re:");
    }

    #[test]
    fn message_id_normalisation_strips_brackets() {
        assert_eq!(normalize_message_id(" <ABC@Example> "), "abc@example");
    }

    #[test]
    fn conversation_key_is_scoped_to_the_account() {
        let account = AccountId::new();
        let mut header = MessageHeader::new(account, FolderId::new());
        header.message_id = Some("<root@example>".to_string());
        let key = header.conversation_key();
        assert!(key.starts_with(&account.to_string()));
        assert!(key.ends_with("root@example"));

        header.references = vec!["<older@example>".to_string()];
        let with_reference = header.conversation_key();
        assert!(with_reference.ends_with("older@example"));
    }

    #[test]
    fn derived_fields_are_refreshed() {
        let mut header = MessageHeader::new(AccountId::new(), FolderId::new());
        header.subject = "Re: Status".to_string();
        header.message_id = Some("<m@x>".to_string());
        header.refresh_derived_fields();
        assert_eq!(header.subject_normalized.as_deref(), Some("status"));
        assert_eq!(header.thread_id, Some(header.conversation_key()));
    }

    #[test]
    fn sorting_falls_back_to_received_then_epoch() {
        let mut header = MessageHeader::new(AccountId::new(), FolderId::new());
        assert_eq!(header.sort_timestamp(), 0);
        header.received_at = Some(Utc::now());
        assert!(header.sort_timestamp() > 0);
        header.date = Some(Utc::now());
        assert_eq!(
            header.sort_timestamp(),
            header.date.unwrap().timestamp_millis()
        );
    }

    #[test]
    fn placeholder_subject_is_used_for_empty_ones() {
        let mut header = MessageHeader::new(AccountId::new(), FolderId::new());
        assert_eq!(header.subject_or_placeholder(), "(kein Betreff)");
        header.subject = "  ".to_string();
        assert_eq!(header.subject_or_placeholder(), "(kein Betreff)");
        header.subject = "Hi".to_string();
        assert_eq!(header.subject_or_placeholder(), "Hi");
    }

    #[test]
    fn body_size_is_recomputed() {
        let mut body = MessageBody::empty(MessageId::new());
        assert!(body.is_empty());
        body.plain_text = Some("hello".to_string());
        body.html_raw = Some("<p>hello</p>".to_string());
        body.recompute_size();
        assert_eq!(body.cached_bytes, 5 + 12);
        assert!(!body.is_empty());
        assert_eq!(body.best_text(), Some("hello"));
    }

    #[test]
    fn attachment_helpers() {
        let attachment = Attachment {
            part_id: "1.2".to_string(),
            filename: None,
            mime_type: "image/png".to_string(),
            size_bytes: Some(10),
            content_id: Some("cid1".to_string()),
            inline: true,
            is_calendar: false,
        };
        assert_eq!(attachment.display_name(), "unnamed");
        assert!(attachment.is_image());
    }

    #[test]
    fn query_builder_sets_expected_fields() {
        let query = MessageQuery::in_folder(FolderId::new(), 500)
            .unread()
            .with_attachment()
            .matching("invoice");
        assert_eq!(query.limit, Some(500));
        assert!(query.unread_only);
        assert!(query.has_attachment);
        assert_eq!(query.text.as_deref(), Some("invoice"));
        assert_eq!(query.order, SortOrder::NewestFirst);
        assert!(query.account_id.is_none());
    }

    #[test]
    fn header_round_trips_through_json() {
        let mut header = MessageHeader::new(AccountId::new(), FolderId::new());
        header.subject = "Hallo".to_string();
        header.from = vec![MailAddress::with_name("a@b.c", "A")];
        header.flags = MessageFlags::SEEN | MessageFlags::FLAGGED;
        header.local_flags = LocalFlags::DONE;
        let json = serde_json::to_string(&header).unwrap();
        let back: MessageHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(header, back);
    }
}
