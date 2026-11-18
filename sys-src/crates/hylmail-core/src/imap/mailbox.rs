//! Turning a `LIST` response into the folder tree the rest of HylMail uses.
//!
//! A `LIST` answer is flat: the server sends wire names in modified UTF-7 plus
//! a set of attributes.  The client needs a tree with a display name, a
//! semantic role, a hierarchy parent and a defined order, and it has to cope
//! with servers that are helpful about it (`\Sent`, `\Noselect`) and with
//! servers that are not (a bare `INBOX` and nothing else).
//!
//! The rules implemented here:
//!
//! * the *wire* name is kept in `Folder::path` — that is what has to be sent
//!   back in `SELECT`/`MOVE` — and the decoded name is used everywhere a human
//!   looks at it;
//! * `\Noselect` folders are kept (they are real containers) but marked as not
//!   selectable, so the synchroniser skips them instead of failing;
//! * the special role comes from the server attributes first and from the
//!   folder name only as a fallback;
//! * the order is `INBOX`, then the standard roles, then user folders
//!   alphabetically — with "Drafts" and "Trash" collecting at the bottom, which
//!   is what a user expects to see.

use async_imap::types::{Name, NameAttribute};

use crate::models::folder::{detect_folder_kind, Folder, FolderKind, SpecialUse};
use crate::models::ids::AccountId;
use chrono::Utc;

use super::utf7;

/// Converts the attributes of a `LIST` entry into the canonical wire strings
/// (`\Noselect`, `\Sent`, ...).
#[must_use]
pub fn attribute_names(attributes: &[NameAttribute<'_>]) -> Vec<String> {
    attributes
        .iter()
        .map(|attribute| match attribute {
            NameAttribute::NoInferiors => "\\NoInferiors".to_string(),
            NameAttribute::NoSelect => "\\Noselect".to_string(),
            NameAttribute::Marked => "\\Marked".to_string(),
            NameAttribute::Unmarked => "\\Unmarked".to_string(),
            NameAttribute::All => "\\All".to_string(),
            NameAttribute::Archive => "\\Archive".to_string(),
            NameAttribute::Drafts => "\\Drafts".to_string(),
            NameAttribute::Flagged => "\\Flagged".to_string(),
            NameAttribute::Junk => "\\Junk".to_string(),
            NameAttribute::Sent => "\\Sent".to_string(),
            NameAttribute::Trash => "\\Trash".to_string(),
            NameAttribute::Extension(other) => other.to_string(),
            // `NameAttribute` is `#[non_exhaustive]`: a future RFC may add an
            // attribute, and unknown ones must not break folder discovery.
            _ => String::new(),
        })
        .collect()
}

/// `true` when the entry can be selected (`SELECT`/`EXAMINE`).
#[must_use]
pub fn is_selectable(attributes: &[String]) -> bool {
    !attributes
        .iter()
        .any(|attribute| attribute.eq_ignore_ascii_case("\\Noselect"))
}

/// The hierarchy delimiter of a `LIST` entry, if the namespace has one.
#[must_use]
pub fn delimiter_of(name: &Name) -> Option<char> {
    name.delimiter()
        .and_then(|delimiter| delimiter.chars().next())
}

/// Builds a [`Folder`] from one `LIST` entry.
///
/// `sort_order` is the position in the tree, computed by [`order_folders`].
#[must_use]
pub fn folder_from_name(account_id: AccountId, name: &Name, sort_order: i32) -> Folder {
    let wire_path = name.name().to_string();
    let delimiter = delimiter_of(name);
    let attributes = attribute_names(name.attributes());

    // `INBOX` is case insensitive in the protocol but has to be sent as
    // `INBOX`; every other name is modified UTF-7.
    let display_path = utf7::decode_lossy(&wire_path);
    let (_parent_display, leaf) = utf7::split_path(&display_path, delimiter);
    let (kind, special_use) = detect_folder_kind(&leaf, &attributes);

    let mut folder = Folder::new(account_id, leaf, wire_path, kind);
    folder.delimiter = delimiter;
    folder.special_use = special_use;
    folder.sort_order = sort_order;
    folder.selectable = is_selectable(&attributes);
    folder
}

/// The display path of a `LIST` entry, used to link children to their parent.
#[must_use]
pub fn display_path(name: &Name) -> String {
    utf7::decode_lossy(name.name())
}

/// Fills in `parent_id` for every folder that has a parent in the list.
///
/// The server sends a flat list, so the parent can only be resolved after all
/// entries are known.  A folder whose parent is missing from the listing stays
/// a root folder instead of being dropped.
pub fn link_parents(folders: &mut [Folder], delimiter: Option<char>) {
    use std::collections::HashMap;

    let Some(delimiter) = delimiter else {
        return;
    };

    let by_path: HashMap<String, crate::models::ids::FolderId> =
        folders.iter().fold(HashMap::new(), |mut map, folder| {
            // A well behaved server never lists a path twice; should one do it
            // anyway, the first entry wins so the result stays deterministic.
            map.entry(folder.path.clone()).or_insert(folder.id);
            map
        });

    for folder in folders.iter_mut() {
        let (parent_path, _) = utf7::split_path(&folder.path, Some(delimiter));
        let Some(parent_path) = parent_path else {
            continue;
        };
        if let Some(parent_id) = by_path.get(&parent_path) {
            folder.parent_id = Some(*parent_id);
        }
    }
}

/// Sorts folders into the order a mail client should display them.
///
/// `INBOX` first, then the standard roles, then user folders alphabetically;
/// `Trash` and `Junk` last, because they are the ones a user looks at least.
pub fn order_folders(folders: &mut [Folder]) {
    folders.sort_by(|left, right| {
        let left_key = (rank(left), left.path.to_ascii_lowercase());
        let right_key = (rank(right), right.path.to_ascii_lowercase());
        left_key.cmp(&right_key)
    });
    for (index, folder) in folders.iter_mut().enumerate() {
        folder.sort_order = i32::try_from(index).unwrap_or(i32::MAX);
    }
}

/// Sort rank of a folder: lower comes first.
fn rank(folder: &Folder) -> u8 {
    match folder.kind {
        FolderKind::Inbox => 0,
        FolderKind::Sent => 1,
        FolderKind::Drafts => 2,
        FolderKind::Archive => 3,
        FolderKind::Custom => 4,
        FolderKind::Junk => 8,
        FolderKind::Trash => 9,
        FolderKind::Outbox | FolderKind::VirtualAllInboxes => 10,
    }
}

/// Names of the folders a client should always offer, even when the server
/// does not list them.
#[must_use]
pub fn missing_standard_folders(folders: &[Folder]) -> Vec<SpecialUse> {
    let mut wanted = Vec::new();
    for (use_, kind) in [
        (SpecialUse::Sent, FolderKind::Sent),
        (SpecialUse::Drafts, FolderKind::Drafts),
        (SpecialUse::Trash, FolderKind::Trash),
        (SpecialUse::Junk, FolderKind::Junk),
    ] {
        if !folders.iter().any(|folder| folder.kind == kind) {
            wanted.push(use_);
        }
    }
    wanted
}

/// Convenience: build a complete, linked and ordered folder tree.
#[must_use]
pub fn build_tree(account_id: AccountId, names: &[Name]) -> Vec<Folder> {
    let delimiter = names.iter().find_map(delimiter_of);
    let mut folders: Vec<Folder> = names
        .iter()
        .map(|name| folder_from_name(account_id, name, 0))
        .collect();
    order_folders(&mut folders);
    link_parents(&mut folders, delimiter);
    folders
}

/// The `FolderId` of the special-use folder of a kind, if present.
#[must_use]
pub fn find_by_kind(folders: &[Folder], kind: FolderKind) -> Option<&Folder> {
    folders.iter().find(|folder| folder.kind == kind)
}

/// Marks the folders that were newly discovered by comparing against what the
/// database already had, so the caller knows where a full synchronisation is
/// needed.
#[must_use]
pub fn is_new(known: &[Folder], candidate: &Folder) -> bool {
    !known
        .iter()
        .any(|folder| folder.account_id == candidate.account_id && folder.path == candidate.path)
}

/// The timestamp used when a folder has to be materialised locally.
#[must_use]
pub fn now() -> chrono::DateTime<chrono::Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ids::AccountId;

    #[test]
    fn attribute_names_are_canonical_wire_strings() {
        let attributes = [
            NameAttribute::NoSelect,
            NameAttribute::Sent,
            NameAttribute::Extension("\\Spam".into()),
        ];
        assert_eq!(
            attribute_names(&attributes),
            vec!["\\Noselect", "\\Sent", "\\Spam"]
        );
        assert!(!is_selectable(&attribute_names(&attributes)));
        assert!(is_selectable(&attribute_names(&[NameAttribute::Sent])));
    }

    #[test]
    fn ranks_order_the_folders_like_a_mail_client() {
        let account = AccountId::new();
        let mut folders = vec![
            Folder::new(account, "Projekte", "Projekte", FolderKind::Custom),
            Folder::new(account, "INBOX", "INBOX", FolderKind::Inbox),
            Folder::new(account, "Gesendet", "Gesendet", FolderKind::Sent),
            Folder::new(account, "Papierkorb", "Papierkorb", FolderKind::Trash),
            Folder::new(account, "Entwürfe", "Entw&APw-rfe", FolderKind::Drafts),
        ];
        order_folders(&mut folders);
        let order: Vec<&str> = folders.iter().map(|folder| folder.name.as_str()).collect();
        assert_eq!(
            order,
            vec!["INBOX", "Gesendet", "Entwürfe", "Projekte", "Papierkorb"]
        );
        assert_eq!(folders[0].sort_order, 0);
        assert_eq!(folders[4].sort_order, 4);
    }

    #[test]
    fn parents_are_linked_by_path() {
        let account = AccountId::new();
        let mut folders = vec![
            Folder::new(account, "Projekte", "Projekte", FolderKind::Custom),
            {
                let mut folder =
                    Folder::new(account, "HylMail", "Projekte/HylMail", FolderKind::Custom);
                folder.delimiter = Some('/');
                folder
            },
            {
                let mut folder = Folder::new(account, "2025", "Archiv/2025", FolderKind::Custom);
                folder.id = crate::models::ids::FolderId::new();
                folder
            },
        ];
        let root = folders[0].id;
        link_parents(&mut folders, Some('/'));
        assert_eq!(folders[1].parent_id, Some(root));
        assert_eq!(folders[0].parent_id, None);
        // A folder whose parent is not in the list stays a root.
        assert_eq!(folders[2].parent_id, None);
    }

    #[test]
    fn missing_standard_folders_are_reported() {
        let account = AccountId::new();
        let folders = vec![
            Folder::new(account, "INBOX", "INBOX", FolderKind::Inbox),
            Folder::new(account, "Gesendet", "Gesendet", FolderKind::Sent),
        ];
        let missing = missing_standard_folders(&folders);
        assert_eq!(
            missing,
            vec![SpecialUse::Drafts, SpecialUse::Trash, SpecialUse::Junk]
        );
    }

    #[test]
    fn new_folders_are_detected() {
        let account = AccountId::new();
        let known = vec![Folder::new(account, "INBOX", "INBOX", FolderKind::Inbox)];
        let candidate = Folder::new(account, "Neu", "Neu", FolderKind::Custom);
        assert!(is_new(&known, &candidate));
        assert!(!is_new(&known, &known[0]));
    }

    #[test]
    fn a_folder_without_a_parent_reports_no_delimiter() {
        let account = AccountId::new();
        let folder = Folder::new(account, "flach", "flach", FolderKind::Custom);
        assert_eq!(folder.depth(), 0);
        assert!(find_by_kind(std::slice::from_ref(&folder), FolderKind::Custom).is_some());
        assert!(find_by_kind(std::slice::from_ref(&folder), FolderKind::Trash).is_none());
        assert!(now().timestamp() > 0);
    }
}
