//! Folder (mailbox) descriptions and the special-folder detection heuristics.

use serde::{Deserialize, Serialize};

use crate::models::ids::{AccountId, FolderId};

/// RFC 6154 special-use attributes plus the client-side "inbox" marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecialUse {
    /// `\Inbox` - the primary inbox.  Not part of RFC 6154 but universally
    /// understood and needed for the "All inboxes" virtual folder.
    Inbox,
    /// `\All`
    All,
    /// `\Archive`
    Archive,
    /// `\Drafts`
    Drafts,
    /// `\Flagged`
    Flagged,
    /// `\Junk` / `\Spam`
    Junk,
    /// `\Sent`
    Sent,
    /// `\Trash`
    Trash,
}

impl SpecialUse {
    /// The IMAP attribute name, including the leading backslash.
    #[must_use]
    pub const fn imap_attribute(self) -> &'static str {
        match self {
            Self::Inbox => "\\Inbox",
            Self::All => "\\All",
            Self::Archive => "\\Archive",
            Self::Drafts => "\\Drafts",
            Self::Flagged => "\\Flagged",
            Self::Junk => "\\Junk",
            Self::Sent => "\\Sent",
            Self::Trash => "\\Trash",
        }
    }

    /// Parses an IMAP special-use attribute, case insensitively.
    #[must_use]
    pub fn from_imap_attribute(attribute: &str) -> Option<Self> {
        let normalised = attribute
            .trim()
            .trim_start_matches('\\')
            .to_ascii_lowercase();
        match normalised.as_str() {
            "inbox" => Some(Self::Inbox),
            "all" => Some(Self::All),
            "archive" => Some(Self::Archive),
            "drafts" => Some(Self::Drafts),
            "flagged" => Some(Self::Flagged),
            "junk" | "spam" => Some(Self::Junk),
            "sent" | "sentmail" | "sentitems" => Some(Self::Sent),
            "trash" | "deleteditems" | "deleted" => Some(Self::Trash),
            _ => None,
        }
    }
}

/// Semantic role of a folder inside the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderKind {
    /// Primary inbox of an account.
    Inbox,
    /// Sent items.
    Sent,
    /// Drafts.
    Drafts,
    /// Deleted items.
    Trash,
    /// Spam / junk.
    Junk,
    /// Archive.
    Archive,
    /// Local outgoing queue (never synchronised).
    Outbox,
    /// The synthetic "All inboxes" folder across every account.
    VirtualAllInboxes,
    /// Any user created folder without a special role.
    Custom,
}

impl FolderKind {
    /// `true` for folders that only exist locally.
    #[must_use]
    pub const fn is_synthetic(self) -> bool {
        matches!(self, Self::VirtualAllInboxes | Self::Outbox)
    }

    /// `true` when the folder can receive an incoming message by default.
    #[must_use]
    pub const fn is_delivery_target(self) -> bool {
        matches!(self, Self::Inbox)
    }

    /// Maps a special-use attribute onto a folder kind.
    #[must_use]
    pub const fn from_special_use(use_: SpecialUse) -> Self {
        match use_ {
            SpecialUse::Inbox => Self::Inbox,
            SpecialUse::All => Self::Archive,
            SpecialUse::Archive => Self::Archive,
            SpecialUse::Drafts => Self::Drafts,
            SpecialUse::Flagged => Self::Custom,
            SpecialUse::Junk => Self::Junk,
            SpecialUse::Sent => Self::Sent,
            SpecialUse::Trash => Self::Trash,
        }
    }
}

/// One mailbox of one account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Local identifier.
    pub id: FolderId,
    /// Owning account.
    pub account_id: AccountId,
    /// Parent folder, `None` for a root folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<FolderId>,
    /// Display name (last hierarchy component).
    pub name: String,
    /// Full path as understood by the server, including the delimiter.
    pub path: String,
    /// Hierarchy delimiter reported by the server (`/` or `.`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<char>,
    /// Semantic role.
    pub kind: FolderKind,
    /// Special-use attribute reported by the server or guessed from the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub special_use: Option<SpecialUse>,
    /// Number of unseen messages.
    #[serde(default)]
    pub unread_count: u32,
    /// Total number of messages.
    #[serde(default)]
    pub total_count: u32,
    /// IMAP `UIDVALIDITY`; a change invalidates every cached UID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid_validity: Option<u32>,
    /// IMAP `UIDNEXT`, used for incremental synchronisation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid_next: Option<u32>,
    /// EWS `SyncState` / GroupWise sequence marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_marker: Option<String>,
    /// Manual ordering hint inside the tree.
    #[serde(default)]
    pub sort_order: i32,
    /// `false` for `\Noselect` parents.
    #[serde(default = "default_true")]
    pub selectable: bool,
}

fn default_true() -> bool {
    true
}

impl Folder {
    /// Creates a folder.
    #[must_use]
    pub fn new(
        account_id: AccountId,
        name: impl Into<String>,
        path: impl Into<String>,
        kind: FolderKind,
    ) -> Self {
        Self {
            id: FolderId::new(),
            account_id,
            parent_id: None,
            name: name.into(),
            path: path.into(),
            delimiter: None,
            kind,
            special_use: None,
            unread_count: 0,
            total_count: 0,
            uid_validity: None,
            uid_next: None,
            sync_marker: None,
            sort_order: 0,
            selectable: true,
        }
    }

    /// `true` when this folder only exists inside the client.
    #[must_use]
    pub fn is_virtual(&self) -> bool {
        self.kind.is_synthetic()
    }

    /// `true` when the folder can be selected (`\Noselect` flag absent).
    #[must_use]
    pub fn is_selectable(&self) -> bool {
        self.selectable && !self.is_virtual()
    }

    /// `true` when messages can be moved into the folder.
    #[must_use]
    pub fn can_receive(&self) -> bool {
        self.selectable && !matches!(self.kind, FolderKind::Outbox)
    }

    /// Number of hierarchy levels, used for stable tree ordering.
    #[must_use]
    pub fn depth(&self) -> usize {
        let Some(delimiter) = self.delimiter else {
            return 0;
        };
        self.path.matches(delimiter).count()
    }

    /// The synthetic "All inboxes" root folder shared by every account.
    #[must_use]
    pub fn all_inboxes(account_id: AccountId) -> Self {
        let mut folder = Self::new(
            account_id,
            "Alle Posteingänge",
            "/virtual/all-inboxes",
            FolderKind::VirtualAllInboxes,
        );
        folder.special_use = Some(SpecialUse::Inbox);
        folder.selectable = false;
        folder
    }
}

/// Derives the semantic role of a folder from server attributes and its name.
///
/// `attributes` contains the raw IMAP flags (e.g. `["\\HasNoChildren",
/// "\\Sent"]`).  Anything the server does not tell us is guessed from the
/// English and German folder names that Exchange, Gmail, Dovecot and
/// GroupWise use in practice.  Users can always override the result through
/// the folder properties dialog.
#[must_use]
pub fn detect_folder_kind(name: &str, attributes: &[String]) -> (FolderKind, Option<SpecialUse>) {
    for attribute in attributes {
        if let Some(use_) = SpecialUse::from_imap_attribute(attribute) {
            return (FolderKind::from_special_use(use_), Some(use_));
        }
    }
    match detect_special_use_from_name(name) {
        Some(use_) => (FolderKind::from_special_use(use_), Some(use_)),
        None => (FolderKind::Custom, None),
    }
}

/// Guesses a special-use attribute from a (possibly localised) folder name.
#[must_use]
pub fn detect_special_use(name: &str) -> Option<SpecialUse> {
    detect_special_use_from_name(name)
}

fn detect_special_use_from_name(name: &str) -> Option<SpecialUse> {
    let normalised = name
        .trim()
        .trim_matches('"')
        .to_ascii_lowercase()
        .replace([' ', '-', '_'], "");
    // Only the last hierarchy component matters; servers such as Dovecot
    // report `INBOX/Trash`.
    let leaf = normalised
        .rsplit(['/', '.', '\\'])
        .next()
        .unwrap_or(&normalised)
        .to_string();

    let use_ = match leaf.as_str() {
        "inbox" | "posteingang" => SpecialUse::Inbox,
        "sent" | "sentitems" | "sentmail" | "sentmessages" | "gesendet" | "gesendeteobjekte"
        | "gesendetelemente" | "gesendeteelemente" | "gesendete" => SpecialUse::Sent,
        "drafts" | "draft" | "entwurf" | "entwuerfe" | "entwürfe" => SpecialUse::Drafts,
        "trash" | "deleted" | "deleteditems" | "deletedmessages" | "papierkorb"
        | "gelöschteobjekte" | "geloeschteobjekte" | "gelöschteelemente" | "geloeschteelemente" => {
            SpecialUse::Trash
        }
        "junk" | "junkemail" | "junkeemail" | "spam" | "junke" => SpecialUse::Junk,
        "archive" | "archiv" | "allmail" | "allemails" | "allenachrichten" => SpecialUse::Archive,
        "flagged" | "starred" | "markiert" => SpecialUse::Flagged,
        _ => return None,
    };
    Some(use_)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_attributes_win_over_names() {
        let attributes = vec!["\\HasNoChildren".to_string(), "\\Sent".to_string()];
        let (kind, use_) = detect_folder_kind("Whatever", &attributes);
        assert_eq!(kind, FolderKind::Sent);
        assert_eq!(use_, Some(SpecialUse::Sent));
    }

    #[test]
    fn english_names_are_detected() {
        let cases = [
            ("Sent", FolderKind::Sent),
            ("Sent Items", FolderKind::Sent),
            ("Drafts", FolderKind::Drafts),
            ("Trash", FolderKind::Trash),
            ("Junk E-Mail", FolderKind::Junk),
            ("Archive", FolderKind::Archive),
            ("INBOX", FolderKind::Inbox),
        ];
        for (name, expected) in cases {
            let (kind, _) = detect_folder_kind(name, &[]);
            assert_eq!(kind, expected, "name = {name}");
        }
    }

    #[test]
    fn german_names_are_detected() {
        let cases = [
            ("Gesendete Elemente", FolderKind::Sent),
            ("Gesendete Objekte", FolderKind::Sent),
            ("Entwürfe", FolderKind::Drafts),
            ("Entwuerfe", FolderKind::Drafts),
            ("Papierkorb", FolderKind::Trash),
            ("Gelöschte Elemente", FolderKind::Trash),
            ("Junk-E-Mail", FolderKind::Junk),
            ("Archiv", FolderKind::Archive),
            ("Alle Nachrichten", FolderKind::Archive),
            ("Posteingang", FolderKind::Inbox),
        ];
        for (name, expected) in cases {
            let (kind, _) = detect_folder_kind(name, &[]);
            assert_eq!(kind, expected, "name = {name}");
        }
    }

    #[test]
    fn hierarchical_names_use_the_leaf_component() {
        let (kind, _) = detect_folder_kind("INBOX.Trash", &[]);
        assert_eq!(kind, FolderKind::Trash);
        let (kind, _) = detect_folder_kind("Projects/Archive", &[]);
        assert_eq!(kind, FolderKind::Archive);
    }

    #[test]
    fn unknown_names_are_custom() {
        let (kind, use_) = detect_folder_kind("Rechnungen 2024", &[]);
        assert_eq!(kind, FolderKind::Custom);
        assert!(use_.is_none());
    }

    #[test]
    fn imap_attribute_parsing_is_case_insensitive() {
        assert_eq!(
            SpecialUse::from_imap_attribute("\\SENT"),
            Some(SpecialUse::Sent)
        );
        assert_eq!(
            SpecialUse::from_imap_attribute("flagged"),
            Some(SpecialUse::Flagged)
        );
        assert_eq!(SpecialUse::from_imap_attribute("\\Unknown"), None);
    }

    #[test]
    fn folder_helpers_behave_as_documented() {
        let account = AccountId::new();
        let mut folder = Folder::new(account, "Archive", "Archive", FolderKind::Archive);
        folder.delimiter = Some('.');
        folder.path = "Projects.2024.Archive".to_string();
        assert_eq!(folder.depth(), 2);
        assert!(!folder.is_virtual());
        assert!(folder.is_selectable());
        assert!(folder.can_receive());

        let all = Folder::all_inboxes(account);
        assert!(all.is_virtual());
        assert!(!all.is_selectable());
    }

    #[test]
    fn noselect_folders_cannot_receive() {
        let mut folder = Folder::new(AccountId::new(), "Parent", "Parent", FolderKind::Custom);
        folder.selectable = false;
        assert!(!folder.is_selectable());
        assert!(!folder.can_receive());
    }

    #[test]
    fn folder_round_trips_through_json() {
        let folder = Folder::all_inboxes(AccountId::new());
        let json = serde_json::to_string(&folder).unwrap();
        let back: Folder = serde_json::from_str(&json).unwrap();
        assert_eq!(folder, back);
    }
}
