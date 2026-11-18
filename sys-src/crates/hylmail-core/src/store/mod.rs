//! Persistence of the human readable configuration documents.
//!
//! HylMail keeps everything that a user may want to inspect, back up or hand
//! edit in plain JSON:
//!
//! | File            | Document                                     |
//! |-----------------|----------------------------------------------|
//! | `settings.json` | [`Settings`]                                 |
//! | `accounts.json` | [`AccountsDocument`]                         |
//! | `rules.json`    | [`crate::models::RuleSet`]                   |
//! | `drafts.json`   | [`DraftsDocument`]                           |
//! | `outbox.json`   | [`OutboxDocument`]                           |
//!
//! Only the volatile parts - message headers, bodies, contacts and the search
//! index - live in SQLite.

pub mod json_store;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::models::{Account, Draft, OutboxItem, RuleSet, Settings};
use crate::paths::AppPaths;

pub use json_store::{quarantine_corrupt_file, read_json, read_json_or_default, write_json_atomic};

/// Contents of `accounts.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountsDocument {
    /// All configured accounts.
    #[serde(default)]
    pub accounts: Vec<Account>,
}

/// Contents of `drafts.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftsDocument {
    /// All locally stored drafts.
    #[serde(default)]
    pub drafts: Vec<Draft>,
}

/// Contents of `outbox.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxDocument {
    /// All queued outgoing messages.
    #[serde(default)]
    pub items: Vec<OutboxItem>,
}

impl OutboxDocument {
    /// Number of entries that still have to be delivered.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.state.is_pending())
            .count()
    }

    /// Removes entries that were sent successfully more than `keep` items ago.
    pub fn prune_sent(&mut self, keep: usize) {
        let mut remaining = keep;
        let mut result = Vec::with_capacity(self.items.len());
        for item in self.items.drain(..).rev() {
            if item.state == crate::models::OutboxState::Sent {
                if remaining > 0 {
                    remaining -= 1;
                    result.push(item);
                }
            } else {
                result.push(item);
            }
        }
        result.reverse();
        self.items = result;
    }
}

/// Typed access to every configuration document.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    paths: AppPaths,
}

impl ConfigStore {
    /// Creates a store for the given path set.
    #[must_use]
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    /// The underlying path set.
    #[must_use]
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// Creates the configuration and data directories.
    pub fn ensure_dirs(&self) -> Result<()> {
        self.paths.ensure_dirs()
    }

    /// Loads the settings, applying clamping and schema migration.
    ///
    /// The document is rewritten when the migration changed something, so that
    /// the file on disk always matches the running schema.
    pub fn load_settings(&self) -> Result<Settings> {
        let path = self.paths.settings_file();
        let mut settings = read_json_or_default::<Settings>(&path)?;
        let changed = settings.migrate();
        settings.validate()?;
        if changed || !path.exists() {
            self.save_settings(&settings)?;
        }
        Ok(settings)
    }

    /// Writes the settings.
    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        write_json_atomic(&self.paths.settings_file(), settings)
    }

    /// Loads the account list.
    pub fn load_accounts(&self) -> Result<AccountsDocument> {
        read_json_or_default(&self.paths.accounts_file())
    }

    /// Writes the account list.
    pub fn save_accounts(&self, document: &AccountsDocument) -> Result<()> {
        write_json_atomic(&self.paths.accounts_file(), document)
    }

    /// Loads the rule set.
    pub fn load_rules(&self) -> Result<RuleSet> {
        read_json_or_default(&self.paths.rules_file())
    }

    /// Writes the rule set.
    pub fn save_rules(&self, rules: &RuleSet) -> Result<()> {
        rules.validate()?;
        write_json_atomic(&self.paths.rules_file(), rules)
    }

    /// Loads the local drafts.
    pub fn load_drafts(&self) -> Result<DraftsDocument> {
        read_json_or_default(&self.paths.drafts_file())
    }

    /// Writes the local drafts.
    pub fn save_drafts(&self, document: &DraftsDocument) -> Result<()> {
        write_json_atomic(&self.paths.drafts_file(), document)
    }

    /// Loads the outgoing queue.
    pub fn load_outbox(&self) -> Result<OutboxDocument> {
        read_json_or_default(&self.paths.outbox_file())
    }

    /// Writes the outgoing queue.
    pub fn save_outbox(&self, document: &OutboxDocument) -> Result<()> {
        write_json_atomic(&self.paths.outbox_file(), document)
    }

    /// Writes every configuration document that does not exist yet.
    ///
    /// Existing files are never touched, so a user who hand edited
    /// `rules.json` keeps it when `hylmail-cli --init` runs again.  The point of the
    /// method is that a fresh installation gets all five documents on disk
    /// instead of only the ones that happen to have been saved before.
    ///
    /// # Errors
    ///
    /// Returns an [`crate::error::Error`] when a document cannot be written.
    pub fn materialise_documents(&self) -> Result<Vec<PathBuf>> {
        let mut written = Vec::new();

        if !self.paths.settings_file().exists() {
            self.save_settings(&self.load_settings()?)?;
            written.push(self.paths.settings_file());
        }
        if !self.paths.accounts_file().exists() {
            self.save_accounts(&AccountsDocument::default())?;
            written.push(self.paths.accounts_file());
        }
        if !self.paths.rules_file().exists() {
            self.save_rules(&RuleSet::default())?;
            written.push(self.paths.rules_file());
        }
        if !self.paths.drafts_file().exists() {
            self.save_drafts(&DraftsDocument::default())?;
            written.push(self.paths.drafts_file());
        }
        if !self.paths.outbox_file().exists() {
            self.save_outbox(&OutboxDocument::default())?;
            written.push(self.paths.outbox_file());
        }

        Ok(written)
    }

    /// Renames an unreadable document aside and returns its new location.
    pub fn quarantine(&self, path: &Path) -> Result<Option<std::path::PathBuf>> {
        quarantine_corrupt_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        Account, AccountId, AccountKind, AuthKind, Draft, Endpoint, MailAddress, OutboxItem,
        OutboxState, Rule,
    };
    use crate::paths::{AppPaths, MapEnv, Os, PORTABLE_ENV};

    fn store() -> (tempfile::TempDir, ConfigStore) {
        let dir = tempfile::tempdir().unwrap();
        let env = MapEnv::new().with(PORTABLE_ENV, dir.path().to_string_lossy().to_string());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        let store = ConfigStore::new(paths);
        store.ensure_dirs().unwrap();
        (dir, store)
    }

    fn account() -> Account {
        Account {
            id: AccountId::new(),
            display_name: "Privat".to_string(),
            email: "ada@example.org".to_string(),
            kind: AccountKind::Imap,
            auth: AuthKind::Password,
            username: "ada@example.org".to_string(),
            incoming: Endpoint::imaps("imap.example.org"),
            outgoing: Endpoint::smtps("smtp.example.org"),
            proxy: None,
            oauth: None,
            enabled: true,
            created_at: crate::models::utc_now(),
        }
    }

    #[test]
    fn settings_are_written_on_first_load() {
        let (_dir, store) = store();
        let settings = store.load_settings().unwrap();
        let mut expected = Settings::default();
        // `updated_at` is the timestamp of the write, not of the default value.
        expected.updated_at = settings.updated_at;
        assert_eq!(settings, expected);
        assert!(store.paths().settings_file().is_file());
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let (_dir, store) = store();
        let mut settings = Settings::default();
        settings.show_all_inboxes_on_start = true;
        settings.page_size = 250;
        store.save_settings(&settings).unwrap();
        let loaded = store.load_settings().unwrap();
        assert!(loaded.show_all_inboxes_on_start);
        assert_eq!(loaded.page_size, 250);
    }

    #[test]
    fn corrupt_settings_do_not_silently_reset_the_application() {
        let (_dir, store) = store();
        std::fs::write(store.paths().settings_file(), "{ broken").unwrap();
        let error = store.load_settings().unwrap_err();
        assert!(matches!(error, crate::error::Error::Config(_)));
    }

    #[test]
    fn accounts_round_trip() {
        let (_dir, store) = store();
        assert!(store.load_accounts().unwrap().accounts.is_empty());
        let document = AccountsDocument {
            accounts: vec![account()],
        };
        store.save_accounts(&document).unwrap();
        assert_eq!(store.load_accounts().unwrap(), document);
    }

    #[test]
    fn rules_round_trip_and_are_validated() {
        let (_dir, store) = store();
        let mut rules = RuleSet::new();
        rules.add(Rule::new("Spam")).unwrap();
        store.save_rules(&rules).unwrap();
        assert_eq!(store.load_rules().unwrap(), rules);

        let mut broken = RuleSet::new();
        broken.rules.push(Rule::new(""));
        assert!(store.save_rules(&broken).is_err());
    }

    #[test]
    fn drafts_and_outbox_round_trip() {
        let (_dir, store) = store();
        let mut draft = Draft::new(AccountId::new());
        draft.to.push(MailAddress::new("x@example.org"));
        let drafts = DraftsDocument {
            drafts: vec![draft.clone()],
        };
        store.save_drafts(&drafts).unwrap();
        assert_eq!(store.load_drafts().unwrap(), drafts);

        let outbox = OutboxDocument {
            items: vec![OutboxItem::new(draft)],
        };
        store.save_outbox(&outbox).unwrap();
        assert_eq!(store.load_outbox().unwrap(), outbox);
        assert_eq!(store.load_outbox().unwrap().pending(), 1);
    }

    #[test]
    fn sent_items_are_pruned_but_pending_ones_survive() {
        let mut document = OutboxDocument::default();
        for index in 0..5 {
            let mut item = OutboxItem::new(Draft::new(AccountId::new()));
            if index % 2 == 0 {
                item.record_success();
            }
            document.items.push(item);
        }
        assert_eq!(document.pending(), 2);
        let total_before = document.items.len();
        document.prune_sent(1);
        assert!(document.items.len() < total_before);
        assert_eq!(document.pending(), 2);
        assert_eq!(
            document
                .items
                .iter()
                .filter(|item| item.state == OutboxState::Sent)
                .count(),
            1
        );
    }

    #[test]
    fn documents_tolerate_missing_fields() {
        let (_dir, store) = store();
        std::fs::write(store.paths().accounts_file(), "{}").unwrap();
        assert!(store.load_accounts().unwrap().accounts.is_empty());
        std::fs::write(store.paths().outbox_file(), "{}").unwrap();
        assert!(store.load_outbox().unwrap().items.is_empty());
    }

    #[test]
    fn materialise_creates_every_document_once() {
        let (_dir, store) = store();
        store.ensure_dirs().unwrap();

        let created = store.materialise_documents().unwrap();
        assert_eq!(created.len(), 5, "expected all five documents: {created:?}");
        for path in [
            store.paths().settings_file(),
            store.paths().accounts_file(),
            store.paths().rules_file(),
            store.paths().drafts_file(),
            store.paths().outbox_file(),
        ] {
            assert!(path.is_file(), "{} missing", path.display());
        }

        // A second run has nothing to do ...
        assert!(store.materialise_documents().unwrap().is_empty());

        // ... and a hand edited document survives.
        std::fs::write(store.paths().rules_file(), r#"{"rules": []}"#).unwrap();
        assert!(store.materialise_documents().unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(store.paths().rules_file()).unwrap(),
            r#"{"rules": []}"#
        );
    }
}
