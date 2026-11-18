//! Wiring of the core subsystems behind one small façade.
//!
//! The GUI and the CLI both need the same five things: resolved paths, the
//! configuration documents, the SQLite database, a secret store and the current
//! settings.  [`Core`] owns exactly those and nothing else - it is deliberately
//! *not* a god object, it does not know about protocols, and it never spawns
//! background work.  Phase 2 builds the sync engine on top of it.

use std::sync::Arc;

use crate::db::{content, Database};
use crate::error::Result;
use crate::models::Settings;
use crate::paths::{AppPaths, MigrationOutcome};
use crate::secrets::{KeyringStore, SecretStore};
use crate::store::ConfigStore;

/// The assembled core of a running HylMail instance.
pub struct Core {
    paths: AppPaths,
    config: ConfigStore,
    database: Arc<Database>,
    secrets: Arc<dyn SecretStore>,
    settings: Settings,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("paths", &self.paths)
            .field("database", &self.database.path())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Core {
    /// Boots the core using the operating system credential vault.
    pub fn bootstrap_from_env() -> Result<Self> {
        Self::bootstrap(AppPaths::resolve()?, Arc::new(KeyringStore::new()))
    }

    /// Boots the core with an explicit path set and secret store.
    ///
    /// The sequence is:
    ///
    /// 1. migrate a legacy `~/.HylMail` directory (Windows only, transactional),
    /// 2. create the configuration / data / log directories,
    /// 3. load and migrate `settings.json`,
    /// 4. open and migrate the SQLite database,
    /// 5. enforce the configured content cache limit.
    pub fn bootstrap(paths: AppPaths, secrets: Arc<dyn SecretStore>) -> Result<Self> {
        match paths.migrate_legacy_layout()? {
            MigrationOutcome::Migrated {
                source,
                target,
                files,
                bytes,
                ..
            } => tracing::info!(
                from = %source.display(),
                to = %target.display(),
                files,
                bytes,
                "migrated legacy HylMail directory"
            ),
            MigrationOutcome::NotNeeded { reason } => {
                tracing::debug!(reason, "no legacy migration performed");
            }
        }

        paths.ensure_dirs()?;
        let config = ConfigStore::new(paths.clone());
        let settings = config.load_settings()?;

        let database = Database::open(&paths.database_file())?;
        let report = database.migrate()?;
        if report.changed() {
            tracing::info!(
                from = report.from,
                to = report.to,
                applied = ?report.applied,
                "database schema migrated"
            );
        }

        let freed = database
            .write(|connection| content::enforce_limit(connection, settings.cache_limit_bytes()))?;
        if freed > 0 {
            tracing::info!(freed, "content cache trimmed at startup");
        }

        Ok(Self {
            paths,
            config,
            database: Arc::new(database),
            secrets,
            settings,
        })
    }

    /// Resolved directories and well-known files.
    #[must_use]
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// Access to the configuration documents.
    #[must_use]
    pub fn config(&self) -> &ConfigStore {
        &self.config
    }

    /// The SQLite database.
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// The database as a shared handle, for background tasks.
    ///
    /// The synchronization engine runs its accounts in spawned tasks, so it
    /// needs an owning handle rather than a borrow.
    #[must_use]
    pub fn database_handle(&self) -> Arc<Database> {
        Arc::clone(&self.database)
    }

    /// The secret store.
    #[must_use]
    pub fn secrets(&self) -> &dyn SecretStore {
        self.secrets.as_ref()
    }

    /// The secret store as a shared handle, for background tasks.
    #[must_use]
    pub fn secrets_handle(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.secrets)
    }

    /// The settings currently in effect.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replaces the in-memory settings and persists them.
    pub fn save_settings(&mut self, mut settings: Settings) -> Result<()> {
        settings.touch();
        settings.clamp();
        settings.validate()?;
        self.config.save_settings(&settings)?;
        self.settings = settings;
        Ok(())
    }

    /// Re-reads `settings.json` from disk.
    pub fn reload_settings(&mut self) -> Result<()> {
        self.settings = self.config.load_settings()?;
        Ok(())
    }

    /// Trims the content cache to the currently configured limit.
    ///
    /// Called after the user changes the cache size in the settings dialog.
    pub fn enforce_cache_limit(&self) -> Result<u64> {
        let limit = self.settings.cache_limit_bytes();
        self.database
            .write(|connection| content::enforce_limit(connection, limit))
    }

    /// Library version, taken from the workspace manifest.
    #[must_use]
    pub fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{MapEnv, Os, PORTABLE_ENV};
    use crate::secrets::InMemorySecretStore;

    fn core_in(dir: &std::path::Path) -> Core {
        let env = MapEnv::new().with(PORTABLE_ENV, dir.to_string_lossy().to_string());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        Core::bootstrap(paths, Arc::new(InMemorySecretStore::new())).unwrap()
    }

    #[test]
    fn bootstrap_creates_every_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let core = core_in(dir.path());
        assert!(core.paths().settings_file().is_file());
        assert!(core.paths().database_file().is_file());
        assert!(core.paths().logs_dir().is_dir());
        assert_eq!(
            core.database().schema_version().unwrap(),
            crate::db::SCHEMA_VERSION
        );
        assert_eq!(core.version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn bootstrap_is_repeatable_and_keeps_settings() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut core = core_in(dir.path());
            let mut settings = core.settings().clone();
            settings.page_size = 250;
            core.save_settings(settings).unwrap();
        }
        let core = core_in(dir.path());
        assert_eq!(core.settings().page_size, 250);
    }

    #[test]
    fn settings_are_clamped_before_they_are_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_in(dir.path());
        let mut settings = core.settings().clone();
        settings.page_size = 0;
        core.save_settings(settings).unwrap();
        assert_eq!(core.settings().page_size, 50);
    }

    #[test]
    fn invalid_settings_are_rejected_without_changing_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_in(dir.path());
        let before = core.settings().clone();
        let mut broken = before.clone();
        broken.schema_version = crate::models::SETTINGS_SCHEMA_VERSION + 1;
        assert!(core.save_settings(broken).is_err());
        assert_eq!(core.settings(), &before);
    }

    #[test]
    fn reload_picks_up_external_edits() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_in(dir.path());
        let mut settings = core.settings().clone();
        settings.page_size = 100;
        crate::store::write_json_atomic(&core.paths().settings_file(), &settings).unwrap();
        core.reload_settings().unwrap();
        assert_eq!(core.settings().page_size, 100);
    }

    #[test]
    fn secret_store_is_shared() {
        let dir = tempfile::tempdir().unwrap();
        let core = core_in(dir.path());
        let handle = core.secrets_handle();
        let key = crate::secrets::CredentialKey::new(
            crate::models::AccountId::new(),
            crate::secrets::CredentialKind::IncomingPassword,
        );
        handle.set(&key, &crate::secrets::secret("pw")).unwrap();
        assert!(core.secrets().has(&key).unwrap());
    }

    #[test]
    fn enforcing_the_cache_limit_is_harmless_on_an_empty_database() {
        let dir = tempfile::tempdir().unwrap();
        let core = core_in(dir.path());
        assert_eq!(core.enforce_cache_limit().unwrap(), 0);
    }
}
