//! Domain models shared by every layer of HylMail.
//!
//! The module is intentionally free of any persistence or protocol detail: the
//! types only describe *what* an account, folder, message, rule or setting is.
//! Serialisation of these types into the JSON documents in the configuration
//! directory is handled by [`crate::store`], persistence of the volatile parts
//! (headers, bodies, contacts) by [`crate::db`].

pub mod account;
pub mod contact;
pub mod draft;
pub mod folder;
pub mod ids;
pub mod message;
pub mod presets;
pub mod rule;
pub mod settings;

pub use account::{
    Account, AccountKind, AuthKind, Endpoint, GroupWiseProxyConfig, OAuthConfig, TlsMode,
};
pub use contact::{Contact, ContactEmail, ContactSource};
pub use draft::{Draft, DraftAttachment, OutboxItem, OutboxState};
pub use folder::{detect_folder_kind, detect_special_use, Folder, FolderKind, SpecialUse};
pub use ids::{
    from_millis, to_millis, utc_now, AccountId, ContactId, CorrelationId, DraftId, FolderId,
    MessageId, OutboxId, RuleId, SignatureId, SyncJobId,
};
pub use message::{
    normalize_message_id, normalize_subject, safe_file_name, unique_file_names, Attachment,
    LocalFlags, MailAddress, MessageBody, MessageFlags, MessageHeader, MessageQuery, SortOrder,
};
pub use presets::{preset_by_id, preset_for_email, DomainPreset};
pub use rule::{Action, Condition, ConditionField, ConditionOp, Conjunction, Rule, RuleSet};
pub use settings::{
    DoneMode, ExternalImagePolicy, FolderOverride, Settings, Signature, SignaturePosition, Theme,
    DEFAULT_CACHE_LIMIT_MB, DEFAULT_PAGE_SIZE, DEFAULT_SERVER_SEARCH_DELAY_MS,
    DEFAULT_SERVER_SEARCH_LIMIT, DEFAULT_SERVER_SEARCH_MIN_CHARS, GROUPWISE_SEARCH_WINDOW,
    SETTINGS_SCHEMA_VERSION,
};
