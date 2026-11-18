//! User settings, stored as an atomically written `settings.json`.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::models::ids::{AccountId, SignatureId};

/// Schema version of `settings.json`.
///
/// Bumped whenever a field is renamed or its meaning changes; the loader then
/// runs the matching upgrade step instead of silently falling back to the
/// defaults.
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// Default content cache limit mandated by the specification.
pub const DEFAULT_CACHE_LIMIT_MB: u64 = 500;

/// Default page size for the message list (initial load and "load more").
pub const DEFAULT_PAGE_SIZE: u32 = 500;

/// Default debounce before a server side search is issued.
pub const DEFAULT_SERVER_SEARCH_DELAY_MS: u64 = 700;

/// Minimum number of characters before a server side search is issued.
pub const DEFAULT_SERVER_SEARCH_MIN_CHARS: usize = 3;

/// Maximum number of hits the server side search may return.
pub const DEFAULT_SERVER_SEARCH_LIMIT: u32 = 500;

/// How far back a local search reaches on a GroupWise proxy account.
///
/// The proxy cannot search on the server, so its local search is bounded to the
/// most recent messages (`BR-GW-3`).  Returned to the query as
/// [`crate::models::MessageQuery::window`].
pub const GROUPWISE_SEARCH_WINDOW: u32 = 1000;

/// Colour theme.  HylMail ships a dark theme; the light theme exists for
/// accessibility and is not the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// The default dark theme.
    #[default]
    Dark,
    /// Light theme.
    Light,
    /// Follow the operating system.
    System,
}

/// Behaviour of the `E` shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneMode {
    /// Move the message to the archive folder (the default).
    #[default]
    Archive,
    /// Keep the message in place and set the done marker.
    MarkDone,
}

impl DoneMode {
    /// Label shown in the settings dialog.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Archive => "Ins Archiv verschieben",
            Self::MarkDone => "Als erledigt markieren",
        }
    }
}

/// Whether external images are loaded in the reading pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalImagePolicy {
    /// Block them and show the banner; the default.
    #[default]
    Block,
    /// Show the banner and remember a per-sender decision.
    Ask,
    /// Load them immediately.
    Allow,
}

/// Position of a signature relative to the quoted original message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignaturePosition {
    /// Above the quote (top posting).
    #[default]
    AboveQuote,
    /// Below the quote.
    BelowQuote,
}

/// One signature of one account; a account may own several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Local identifier referenced by drafts and accounts.
    pub id: SignatureId,
    /// Account the signature belongs to.
    pub account_id: AccountId,
    /// Name shown in the signature picker.
    pub name: String,
    /// HTML body of the signature.
    #[serde(default)]
    pub html: String,
    /// Plain text rendering.
    #[serde(default)]
    pub plain: String,
    /// Where the signature is inserted in a reply.
    #[serde(default)]
    pub position: SignaturePosition,
    /// Whether this is the account default.
    #[serde(default)]
    pub is_default: bool,
}

impl Signature {
    /// Creates a signature.
    #[must_use]
    pub fn new(account_id: AccountId, name: impl Into<String>) -> Self {
        Self {
            id: SignatureId::new(),
            account_id,
            name: name.into(),
            html: String::new(),
            plain: String::new(),
            position: SignaturePosition::AboveQuote,
            is_default: false,
        }
    }
}

/// A manual override of the automatic special-folder mapping (`FR-FOLDER-3`).
///
/// The synchroniser detects Inbox/Sent/Trash/... from the server's `SPECIAL-USE`
/// attributes and from common names; when a server uses an unusual name the user
/// points the role at the right folder and it wins over the detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderOverride {
    /// Account the override belongs to.
    pub account_id: crate::models::ids::AccountId,
    /// Role the folder should play.
    pub kind: crate::models::folder::FolderKind,
    /// Local id of the folder chosen for the role.
    pub folder_id: crate::models::ids::FolderId,
}

/// The complete settings document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Schema version of this document.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Colour theme.
    #[serde(default)]
    pub theme: Theme,
    /// UI language (`de` or `en`).
    #[serde(default = "default_language")]
    pub language: String,
    /// Behaviour of the `E` shortcut.
    #[serde(default)]
    pub done_mode: DoneMode,
    /// Whether the virtual "All inboxes" folder is selected on start.
    #[serde(default)]
    pub show_all_inboxes_on_start: bool,
    /// Content cache limit in megabytes.
    #[serde(default = "default_cache_limit_mb")]
    pub cache_limit_mb: u64,
    /// Number of messages fetched per page.
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Debounce in milliseconds before a server side search is issued.
    #[serde(default = "default_server_search_delay_ms")]
    pub server_search_delay_ms: u64,
    /// Minimum number of characters before a server side search is issued.
    #[serde(default = "default_server_search_min_chars")]
    pub server_search_min_chars: usize,
    /// Maximum number of hits a server side search may return.
    #[serde(default = "default_server_search_limit")]
    pub server_search_limit: u32,
    /// External image policy.
    #[serde(default)]
    pub external_images: ExternalImagePolicy,
    /// Senders for which external images were permanently allowed.
    #[serde(default)]
    pub trusted_image_senders: Vec<String>,
    /// HTML sanitisation; disabling it is only meant for diagnostics.
    #[serde(default = "default_true")]
    pub sanitize_html: bool,
    /// Apply the dark-mode colour transformation to simple messages.
    #[serde(default = "default_true")]
    pub dark_mode_transform: bool,
    /// At or above this number of images a message counts as a newsletter and
    /// is rendered with its original colours.
    #[serde(default = "default_newsletter_threshold")]
    pub newsletter_image_threshold: usize,
    /// Domains whose resources are always blocked (tracking pixels).
    #[serde(default)]
    pub blocked_tracking_domains: Vec<String>,
    /// Maximum size of the session cache for downloaded images, in megabytes.
    #[serde(default = "default_image_cache_mb")]
    pub image_cache_mb: u64,
    /// Timeout for a single external image download, in seconds.
    #[serde(default = "default_image_timeout_seconds")]
    pub image_timeout_seconds: u64,
    /// Maximum size of a single downloaded resource, in megabytes.
    #[serde(default = "default_image_max_mb")]
    pub image_max_mb: u64,
    /// All configured signatures of all accounts.
    #[serde(default)]
    pub signatures: Vec<Signature>,
    /// Log level filter for `tracing`.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Log verbatim SQL statements (development aid, never on by default).
    #[serde(default)]
    pub debug_sql: bool,
    /// Ask before permanently deleting a message.
    #[serde(default = "default_true")]
    pub confirm_delete: bool,
    /// Empty the trash on exit.
    #[serde(default)]
    pub empty_trash_on_exit: bool,
    /// Check for new mail every N minutes; `0` disables the timer.
    #[serde(default = "default_sync_interval")]
    pub sync_interval_minutes: u32,
    /// CardDAV collection URL for the address book; empty disables the sync.
    #[serde(default)]
    pub carddav_url: String,
    /// User name for the CardDAV server; falls back to the account's when empty.
    #[serde(default)]
    pub carddav_username: String,
    /// Manual special-folder overrides, one per account and role at most.
    #[serde(default)]
    pub folder_overrides: Vec<FolderOverride>,
    /// Last time the settings were written.
    #[serde(default = "crate::models::ids::utc_now")]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

fn default_schema_version() -> u32 {
    SETTINGS_SCHEMA_VERSION
}
fn default_language() -> String {
    "de".to_string()
}
fn default_cache_limit_mb() -> u64 {
    DEFAULT_CACHE_LIMIT_MB
}
fn default_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}
fn default_server_search_delay_ms() -> u64 {
    DEFAULT_SERVER_SEARCH_DELAY_MS
}
fn default_server_search_min_chars() -> usize {
    DEFAULT_SERVER_SEARCH_MIN_CHARS
}
fn default_server_search_limit() -> u32 {
    DEFAULT_SERVER_SEARCH_LIMIT
}
fn default_true() -> bool {
    true
}
fn default_newsletter_threshold() -> usize {
    3
}
fn default_image_cache_mb() -> u64 {
    100
}
fn default_image_timeout_seconds() -> u64 {
    30
}
fn default_image_max_mb() -> u64 {
    15
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_sync_interval() -> u32 {
    15
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            theme: Theme::default(),
            language: default_language(),
            done_mode: DoneMode::default(),
            show_all_inboxes_on_start: false,
            cache_limit_mb: DEFAULT_CACHE_LIMIT_MB,
            page_size: DEFAULT_PAGE_SIZE,
            server_search_delay_ms: DEFAULT_SERVER_SEARCH_DELAY_MS,
            server_search_min_chars: DEFAULT_SERVER_SEARCH_MIN_CHARS,
            server_search_limit: DEFAULT_SERVER_SEARCH_LIMIT,
            external_images: ExternalImagePolicy::default(),
            trusted_image_senders: Vec::new(),
            sanitize_html: true,
            dark_mode_transform: true,
            newsletter_image_threshold: 3,
            blocked_tracking_domains: vec![
                "open.spotify.com".to_string(),
                "pixel.wp.com".to_string(),
                "email.mg".to_string(),
            ],
            image_cache_mb: 100,
            image_timeout_seconds: 30,
            image_max_mb: 15,
            signatures: Vec::new(),
            log_level: default_log_level(),
            debug_sql: false,
            confirm_delete: true,
            empty_trash_on_exit: false,
            sync_interval_minutes: 15,
            carddav_url: String::new(),
            carddav_username: String::new(),
            folder_overrides: Vec::new(),
            updated_at: chrono::Utc::now(),
        }
    }
}

impl Settings {
    /// Loads defaults and clamps every value into its documented range.
    #[must_use]
    pub fn sanitized_default() -> Self {
        let mut settings = Self::default();
        settings.clamp();
        settings
    }

    /// Upper bound of the content cache in bytes.
    #[must_use]
    pub fn cache_limit_bytes(&self) -> u64 {
        self.cache_limit_mb.saturating_mul(1024 * 1024)
    }

    /// Session image cache limit in bytes.
    #[must_use]
    pub fn image_cache_bytes(&self) -> u64 {
        self.image_cache_mb.saturating_mul(1024 * 1024)
    }

    /// Maximum size of a single image download in bytes.
    #[must_use]
    pub fn image_max_bytes(&self) -> u64 {
        self.image_max_mb.saturating_mul(1024 * 1024)
    }

    /// Returns the default signature of an account, if one is configured.
    #[must_use]
    pub fn default_signature(&self, account_id: AccountId) -> Option<&Signature> {
        self.signatures
            .iter()
            .find(|signature| signature.account_id == account_id && signature.is_default)
    }

    /// All signatures of an account, in configuration order.
    #[must_use]
    pub fn signatures_for(&self, account_id: AccountId) -> Vec<&Signature> {
        self.signatures
            .iter()
            .filter(|signature| signature.account_id == account_id)
            .collect()
    }

    /// Ensures exactly one default signature per account.
    pub fn normalize_signatures(&mut self) {
        let mut seen_defaults: Vec<AccountId> = Vec::new();
        let mut keep_defaults: Vec<SignatureId> = Vec::new();
        for signature in &self.signatures {
            if signature.is_default {
                if seen_defaults.contains(&signature.account_id) {
                    continue;
                }
                seen_defaults.push(signature.account_id);
                keep_defaults.push(signature.id);
            }
        }
        let mut accounts_with_default = seen_defaults.clone();
        for signature in &mut self.signatures {
            if signature.is_default && !keep_defaults.contains(&signature.id) {
                signature.is_default = false;
            }
            if !signature.is_default && !accounts_with_default.contains(&signature.account_id) {
                // First signature of an account without a default becomes it.
                signature.is_default = true;
                accounts_with_default.push(signature.account_id);
            }
        }
    }

    /// Clamps every numeric field into a sane range.
    ///
    /// Called after loading so that a hand-edited file can never make the UI
    /// unusable (for example a page size of zero).
    pub fn clamp(&mut self) {
        self.cache_limit_mb = self.cache_limit_mb.clamp(16, 64 * 1024);
        self.page_size = self.page_size.clamp(50, 5_000);
        self.server_search_delay_ms = self.server_search_delay_ms.clamp(100, 10_000);
        self.server_search_min_chars = self.server_search_min_chars.clamp(1, 10);
        self.server_search_limit = self.server_search_limit.clamp(1, 5_000);
        self.newsletter_image_threshold = self.newsletter_image_threshold.clamp(1, 100);
        self.image_cache_mb = self.image_cache_mb.clamp(8, 2_048);
        self.image_timeout_seconds = self.image_timeout_seconds.clamp(5, 300);
        self.image_max_mb = self.image_max_mb.clamp(1, 512);
        self.sync_interval_minutes = self.sync_interval_minutes.min(24 * 60);
        if self.language.trim().is_empty() {
            self.language = default_language();
        }
        if self.log_level.trim().is_empty() {
            self.log_level = default_log_level();
        }
    }

    /// Validates the settings after clamping.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version > SETTINGS_SCHEMA_VERSION {
            return Err(Error::InvalidSettings {
                field: "settings.schema_version",
                message: format!(
                    "file was written by a newer version ({})",
                    self.schema_version
                ),
            });
        }
        if self.page_size == 0 {
            return Err(Error::InvalidSettings {
                field: "settings.page_size",
                message: "page size must not be zero".to_string(),
            });
        }
        if self.cache_limit_mb == 0 {
            return Err(Error::InvalidSettings {
                field: "settings.cache_limit_mb",
                message: "cache limit must not be zero".to_string(),
            });
        }
        for signature in &self.signatures {
            if signature.name.trim().is_empty() {
                return Err(Error::InvalidSettings {
                    field: "settings.signatures.name",
                    message: "a signature needs a name".to_string(),
                });
            }
        }
        Ok(())
    }

    /// Upgrades a document written by an older build in place.
    ///
    /// Returns `true` when something changed and the file should be rewritten.
    pub fn migrate(&mut self) -> bool {
        let mut changed = false;
        if self.schema_version < SETTINGS_SCHEMA_VERSION {
            self.schema_version = SETTINGS_SCHEMA_VERSION;
            changed = true;
        }
        let before = self.clone();
        self.clamp();
        self.normalize_signatures();
        changed || *self != before
    }

    /// Marks the settings as freshly updated.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_specification() {
        let settings = Settings::default();
        assert_eq!(settings.cache_limit_mb, 500);
        assert_eq!(settings.page_size, 500);
        assert_eq!(settings.server_search_delay_ms, 700);
        assert_eq!(settings.server_search_min_chars, 3);
        assert_eq!(settings.server_search_limit, 500);
        assert_eq!(settings.done_mode, DoneMode::Archive);
        assert_eq!(settings.theme, Theme::Dark);
        assert_eq!(settings.external_images, ExternalImagePolicy::Block);
        assert_eq!(settings.newsletter_image_threshold, 3);
        assert_eq!(settings.image_timeout_seconds, 30);
        assert_eq!(settings.image_max_mb, 15);
        assert_eq!(settings.image_cache_mb, 100);
        settings.validate().unwrap();
    }

    #[test]
    fn byte_limits_are_derived_correctly() {
        let settings = Settings::default();
        assert_eq!(settings.cache_limit_bytes(), 500 * 1024 * 1024);
        assert_eq!(settings.image_max_bytes(), 15 * 1024 * 1024);
        assert_eq!(settings.image_cache_bytes(), 100 * 1024 * 1024);
    }

    #[test]
    fn clamp_repairs_hand_edited_files() {
        let mut settings = Settings::default();
        settings.page_size = 0;
        settings.cache_limit_mb = 0;
        settings.server_search_min_chars = 0;
        settings.image_timeout_seconds = 100_000;
        settings.image_max_mb = 0;
        settings.newsletter_image_threshold = 0;
        settings.sync_interval_minutes = 100_000;
        settings.clamp();
        assert_eq!(settings.page_size, 50);
        assert_eq!(settings.cache_limit_mb, 16);
        assert_eq!(settings.server_search_min_chars, 1);
        assert_eq!(settings.image_timeout_seconds, 300);
        assert_eq!(settings.image_max_mb, 1);
        assert_eq!(settings.newsletter_image_threshold, 1);
        assert_eq!(settings.sync_interval_minutes, 24 * 60);
        settings.validate().unwrap();
    }

    #[test]
    fn newer_schema_is_rejected() {
        let mut settings = Settings::default();
        settings.schema_version = SETTINGS_SCHEMA_VERSION + 1;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn signature_defaults_are_normalised_per_account() {
        let account_a = AccountId::new();
        let account_b = AccountId::new();
        let mut settings = Settings::default();

        let mut first = Signature::new(account_a, "A1");
        first.is_default = true;
        let mut second = Signature::new(account_a, "A2");
        second.is_default = true;
        settings.signatures.push(first);
        settings.signatures.push(second);
        settings.signatures.push(Signature::new(account_b, "B1"));

        settings.normalize_signatures();

        let a_defaults: Vec<&str> = settings
            .signatures
            .iter()
            .filter(|s| s.account_id == account_a && s.is_default)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(a_defaults, vec!["A1"]);
        assert!(settings
            .default_signature(account_b)
            .is_some_and(|s| s.name == "B1"));
        assert_eq!(settings.signatures_for(account_a).len(), 2);
    }

    #[test]
    fn unnamed_signature_is_rejected() {
        let mut settings = Settings::default();
        settings
            .signatures
            .push(Signature::new(AccountId::new(), "   "));
        assert!(settings.validate().is_err());
    }

    #[test]
    fn migrate_bumps_the_version_and_reports_changes() {
        let mut settings = Settings::default();
        settings.schema_version = 0;
        assert!(settings.migrate());
        assert_eq!(settings.schema_version, SETTINGS_SCHEMA_VERSION);
        // A second run is a no-op.
        assert!(!settings.migrate());
    }

    #[test]
    fn settings_round_trip_and_tolerate_missing_fields() {
        let settings = Settings::default();
        let json = serde_json::to_string_pretty(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(settings, back);

        // A partial document must be filled with defaults.
        let partial: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(partial.cache_limit_mb, DEFAULT_CACHE_LIMIT_MB);
        assert_eq!(partial.theme, Theme::Dark);
        assert!(partial.sanitize_html);
    }
}
