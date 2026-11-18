//! Platform specific data locations and the portable-mode override.
//!
//! # Layout
//!
//! | Platform          | Configuration                          | Cache / logs / database             |
//! |-------------------|----------------------------------------|-------------------------------------|
//! | macOS, Linux      | `~/.HylMail`                            | `~/.HylMail`                         |
//! | Windows           | `%APPDATA%\HylMail`                     | `%LOCALAPPDATA%\HylMail`             |
//! | Portable          | `$HYLMAIL_HOME`                         | `$HYLMAIL_HOME`                      |
//!
//! The portable mode is active whenever `HYLMAIL_HOME` is set to a non-empty
//! value; in that case *all* state lives inside that single directory which is
//! what makes the application usable from a USB stick.
//!
//! On Windows the first start also performs a transactional migration of a
//! legacy `%USERPROFILE%\.HylMail` directory (see
//! [`AppPaths::migrate_legacy_layout`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoResultExt, Result};

/// Environment variable that switches HylMail into portable mode.
pub const PORTABLE_ENV: &str = "HYLMAIL_HOME";

/// Name of the configuration directory used on every platform.
pub const APP_DIR_NAME: &str = "HylMail";

/// Name of the legacy single-directory layout (`~/.HylMail`).
pub const LEGACY_DIR_NAME: &str = ".HylMail";

/// File name of the settings document.
pub const SETTINGS_FILE: &str = "settings.json";
/// File name of the account list.
pub const ACCOUNTS_FILE: &str = "accounts.json";
/// File name of the rule list.
pub const RULES_FILE: &str = "rules.json";
/// File name of the local drafts.
pub const DRAFTS_FILE: &str = "drafts.json";
/// File name of the outgoing queue.
pub const OUTBOX_FILE: &str = "outbox.json";
/// File name of the SQLite database.
pub const DATABASE_FILE: &str = "hylmail.db";
/// File name of the crash report written by the panic hook.
pub const CRASH_FILE: &str = "absturz.txt";
/// Name of the log directory.
pub const LOGS_DIR: &str = "logs";

/// Operating system family, abstracted so that path resolution is unit
/// testable on any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    /// Microsoft Windows.
    Windows,
    /// Apple macOS.
    MacOs,
    /// Linux and other unixoid systems.
    Linux,
}

impl Os {
    /// Detects the operating system this binary was compiled for.
    pub const fn current() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Self::Linux
        }
    }

    /// `true` for [`Os::Windows`].
    pub const fn is_windows(self) -> bool {
        matches!(self, Self::Windows)
    }
}

/// Read-only view onto process environment variables.
///
/// The indirection exists purely for testing: [`MapEnv`] allows the path
/// resolver to be driven without mutating the real process environment (which
/// is not thread safe and would make the test suite flaky).
pub trait EnvLookup {
    /// Returns the value of `name`, or `None` when it is unset or empty.
    fn var(&self, name: &str) -> Option<String>;
}

/// [`EnvLookup`] implementation backed by the real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }
}

/// [`EnvLookup`] implementation backed by an in-memory map.
#[derive(Debug, Clone, Default)]
pub struct MapEnv {
    vars: BTreeMap<String, String>,
}

impl MapEnv {
    /// Creates an empty environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a variable. Returns `self` to allow chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    /// Inserts a variable.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }
}

impl EnvLookup for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.vars
            .get(name)
            .filter(|value| !value.trim().is_empty())
            .cloned()
    }
}

/// Which set of paths is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppMode {
    /// Regular per-user installation.
    Standard,
    /// Everything inside `$HYLMAIL_HOME`.
    Portable,
}

impl AppMode {
    /// `true` when the portable override is active.
    pub const fn is_portable(self) -> bool {
        matches!(self, Self::Portable)
    }
}

/// Outcome of the legacy-layout migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// There was nothing to migrate.
    NotNeeded {
        /// Why the migration was skipped.
        reason: &'static str,
    },
    /// Files were moved successfully.
    Migrated {
        /// Directory the data came from.
        source: PathBuf,
        /// Directory the data went to.
        target: PathBuf,
        /// Number of files that were moved.
        files: u64,
        /// Number of bytes that were moved.
        bytes: u64,
        /// Whether the legacy directory could be removed afterwards.
        source_removed: bool,
    },
}

impl MigrationOutcome {
    /// `true` when data was actually moved.
    pub const fn moved(&self) -> bool {
        matches!(self, Self::Migrated { .. })
    }
}

/// All directories and well-known files HylMail uses.
///
/// The type is cheap to clone and knows nothing about the process environment
/// once constructed, which makes it safe to pass into background tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    os: Os,
    mode: AppMode,
    config_dir: PathBuf,
    data_dir: PathBuf,
    legacy_config_dir: Option<PathBuf>,
}

impl AppPaths {
    /// Resolves the paths for the current process.
    pub fn resolve() -> Result<Self> {
        Self::resolve_for(Os::current(), &ProcessEnv)
    }

    /// Resolves the paths for an explicit OS and environment.
    pub fn resolve_for<E: EnvLookup + ?Sized>(os: Os, env: &E) -> Result<Self> {
        if let Some(home) = env.var(PORTABLE_ENV) {
            let root = PathBuf::from(home);
            return Ok(Self {
                os,
                mode: AppMode::Portable,
                config_dir: root.clone(),
                data_dir: root,
                legacy_config_dir: None,
            });
        }

        match os {
            Os::Windows => {
                let roaming = env
                    .var("APPDATA")
                    .ok_or(Error::MissingEnv { name: "APPDATA" })?;
                let local = env.var("LOCALAPPDATA").unwrap_or_else(|| roaming.clone());
                let legacy = env
                    .var("USERPROFILE")
                    .map(|profile| Path::new(&profile).join(LEGACY_DIR_NAME));
                Ok(Self {
                    os,
                    mode: AppMode::Standard,
                    config_dir: PathBuf::from(roaming).join(APP_DIR_NAME),
                    data_dir: PathBuf::from(local).join(APP_DIR_NAME),
                    legacy_config_dir: legacy,
                })
            }
            Os::MacOs | Os::Linux => {
                let home = env.var("HOME").ok_or(Error::MissingEnv { name: "HOME" })?;
                let root = PathBuf::from(home).join(LEGACY_DIR_NAME);
                Ok(Self {
                    os,
                    mode: AppMode::Standard,
                    config_dir: root.clone(),
                    data_dir: root,
                    legacy_config_dir: None,
                })
            }
        }
    }

    /// Operating system these paths were resolved for.
    pub const fn os(&self) -> Os {
        self.os
    }

    /// Whether the portable override is active.
    pub const fn mode(&self) -> AppMode {
        self.mode
    }

    /// Directory holding `settings.json`, `accounts.json`, ... .
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Directory holding the SQLite database, the content cache and the logs.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Legacy `%USERPROFILE%\.HylMail` directory, when one may exist.
    pub fn legacy_config_dir(&self) -> Option<&Path> {
        self.legacy_config_dir.as_deref()
    }

    /// Path of the settings document.
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join(SETTINGS_FILE)
    }

    /// Path of the account list.
    pub fn accounts_file(&self) -> PathBuf {
        self.config_dir.join(ACCOUNTS_FILE)
    }

    /// Path of the rule list.
    pub fn rules_file(&self) -> PathBuf {
        self.config_dir.join(RULES_FILE)
    }

    /// Path of the local drafts.
    pub fn drafts_file(&self) -> PathBuf {
        self.config_dir.join(DRAFTS_FILE)
    }

    /// Path of the outgoing queue.
    pub fn outbox_file(&self) -> PathBuf {
        self.config_dir.join(OUTBOX_FILE)
    }

    /// Path of the SQLite database.
    pub fn database_file(&self) -> PathBuf {
        self.data_dir.join(DATABASE_FILE)
    }

    /// Path of the log directory.
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join(LOGS_DIR)
    }

    /// Path of the crash report consumed on the next start.
    pub fn crash_file(&self) -> PathBuf {
        self.data_dir.join(CRASH_FILE)
    }

    /// Creates the configuration, data and log directories when missing.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.data_dir, &self.logs_dir()] {
            std::fs::create_dir_all(dir).with_path(dir)?;
        }
        Ok(())
    }

    /// Reports whether the configuration directory already holds user data.
    ///
    /// Used as the guard for the legacy migration: an installation that has
    /// already written a configuration must never be overwritten.
    pub fn config_dir_has_data(&self) -> bool {
        [
            self.settings_file(),
            self.accounts_file(),
            self.rules_file(),
        ]
        .iter()
        .any(|file| file.is_file())
    }

    /// Transactionally migrates a legacy `%USERPROFILE%\.HylMail` directory into
    /// the split Windows layout.
    ///
    /// The operation is a no-op unless *all* of the following hold:
    ///
    /// * the process runs on Windows,
    /// * the legacy directory exists,
    /// * the new configuration directory does not contain user data yet.
    ///
    /// Configuration and data files of the legacy layout are moved into
    /// [`AppPaths::config_dir`]; the SQLite database and the log directory are
    /// additionally relocated to [`AppPaths::data_dir`] so that the cache does
    /// not end up on a roaming profile.  If anything fails during the copy or
    /// the atomic rename, the staging directory is removed and the legacy
    /// directory is left untouched.
    pub fn migrate_legacy_layout(&self) -> Result<MigrationOutcome> {
        if !self.os.is_windows() {
            return Ok(MigrationOutcome::NotNeeded {
                reason: "not running on windows",
            });
        }
        if self.mode.is_portable() {
            return Ok(MigrationOutcome::NotNeeded {
                reason: "portable mode active",
            });
        }
        let Some(legacy) = self.legacy_config_dir.as_deref() else {
            return Ok(MigrationOutcome::NotNeeded {
                reason: "no legacy directory known",
            });
        };
        if !legacy.is_dir() {
            return Ok(MigrationOutcome::NotNeeded {
                reason: "legacy directory does not exist",
            });
        }
        if self.config_dir.starts_with(legacy) || legacy.starts_with(&self.config_dir) {
            return Ok(MigrationOutcome::NotNeeded {
                reason: "legacy and target directory overlap",
            });
        }
        if self.config_dir_has_data() {
            return Ok(MigrationOutcome::NotNeeded {
                reason: "target directory already contains data",
            });
        }

        let MigrationOutcome::Migrated {
            files,
            bytes,
            source_removed,
            ..
        } = migrate_tree(legacy, &self.config_dir)?
        else {
            return Ok(MigrationOutcome::NotNeeded {
                reason: "legacy directory could not be migrated",
            });
        };

        // Second step: relocate the cache artefacts into the local (non
        // roaming) data directory.  A failure here is not fatal - the files
        // simply stay inside the roaming configuration directory, which is
        // still functional.
        for artefact in [DATABASE_FILE, LOGS_DIR] {
            let from = self.config_dir.join(artefact);
            if !from.exists() {
                continue;
            }
            let to = self.data_dir.join(artefact);
            match move_path(&from, &to) {
                Ok(()) => {
                    tracing::debug!(from = %from.display(), to = %to.display(), "relocated cache artefact");
                }
                Err(error) => {
                    tracing::warn!(
                        from = %from.display(),
                        to = %to.display(),
                        %error,
                        "could not relocate cache artefact, leaving it in place"
                    );
                }
            }
        }
        if !self.data_dir.exists() {
            std::fs::create_dir_all(&self.data_dir).with_path(&self.data_dir)?;
        }

        Ok(MigrationOutcome::Migrated {
            source: legacy.to_path_buf(),
            target: self.config_dir.clone(),
            files,
            bytes,
            source_removed,
        })
    }
}

/// Moves a directory tree transactionally.
///
/// The tree is first copied into a staging directory next to `target`, then
/// verified (file count and file sizes) and finally renamed into place with a
/// single atomic filesystem operation.  Any error before the rename leaves
/// `source` untouched and removes the staging directory.
///
/// This function is platform independent so that it can be tested on every
/// host, even though only the Windows code path calls it in production.
pub fn migrate_tree(source: &Path, target: &Path) -> Result<MigrationOutcome> {
    if !source.is_dir() {
        return Ok(MigrationOutcome::NotNeeded {
            reason: "source directory does not exist",
        });
    }
    if target.starts_with(source) || source.starts_with(target) {
        return Err(Error::MigrationFailed {
            path_from: source.to_path_buf(),
            path_to: target.to_path_buf(),
            message: "source and target directories must not overlap".to_string(),
        });
    }
    if target.exists() && !dir_is_empty(target)? {
        return Ok(MigrationOutcome::NotNeeded {
            reason: "target directory is not empty",
        });
    }

    let parent = target.parent().ok_or_else(|| Error::MigrationFailed {
        path_from: source.to_path_buf(),
        path_to: target.to_path_buf(),
        message: "target directory has no parent".to_string(),
    })?;
    std::fs::create_dir_all(parent).with_path(parent)?;

    let staging = staging_dir(parent, target);
    // Defensive: a leftover staging directory from a crashed run must not be
    // reused, otherwise verification could compare against stale content.
    if staging.exists() {
        std::fs::remove_dir_all(&staging).with_path(&staging)?;
    }

    let copy = || -> Result<(u64, u64)> {
        std::fs::create_dir_all(&staging).with_path(&staging)?;
        let stats = copy_tree(source, &staging)?;
        let expected = scan_tree(source)?;
        let actual = scan_tree(&staging)?;
        if expected.len() != actual.len() {
            return Err(Error::MigrationFailed {
                path_from: source.to_path_buf(),
                path_to: target.to_path_buf(),
                message: format!(
                    "verification failed: expected {} files, found {}",
                    expected.len(),
                    actual.len()
                ),
            });
        }
        for (relative, size) in &expected {
            match actual.get(relative) {
                Some(copied) if copied == size => {}
                Some(copied) => {
                    return Err(Error::MigrationFailed {
                        path_from: source.to_path_buf(),
                        path_to: target.to_path_buf(),
                        message: format!(
                            "verification failed for `{}`: expected {size} bytes, found {copied}",
                            relative.display()
                        ),
                    });
                }
                None => {
                    return Err(Error::MigrationFailed {
                        path_from: source.to_path_buf(),
                        path_to: target.to_path_buf(),
                        message: format!(
                            "verification failed: `{}` is missing",
                            relative.display()
                        ),
                    });
                }
            }
        }
        Ok(stats)
    };

    let (files, bytes) = match copy() {
        Ok(stats) => stats,
        Err(error) => {
            // Rollback: drop the partial copy, keep the source.
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    if target.exists() {
        // Only an empty directory can reach this point.
        std::fs::remove_dir(target).with_path(target)?;
    }
    if let Err(source_error) = std::fs::rename(&staging, target) {
        // Rollback: the rename is the commit point, so nothing was committed.
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Error::MigrationFailed {
            path_from: source.to_path_buf(),
            path_to: target.to_path_buf(),
            message: format!("atomic rename failed: {source_error}"),
        });
    }

    let source_removed = match std::fs::remove_dir_all(source) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(path = %source.display(), %error, "legacy source could not be removed");
            false
        }
    };

    Ok(MigrationOutcome::Migrated {
        source: source.to_path_buf(),
        target: target.to_path_buf(),
        files,
        bytes,
        source_removed,
    })
}

fn staging_dir(parent: &Path, target: &Path) -> PathBuf {
    let stem = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hylmail".to_string());
    let unique = format!(
        ".{stem}.migrating-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    parent.join(unique)
}

fn dir_is_empty(path: &Path) -> Result<bool> {
    let mut entries = std::fs::read_dir(path).with_path(path)?;
    Ok(entries.next().is_none())
}

fn copy_tree(source: &Path, target: &Path) -> Result<(u64, u64)> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![(source.to_path_buf(), target.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to).with_path(&to)?;
        for entry in std::fs::read_dir(&from).with_path(&from)? {
            let entry = entry.with_path(&from)?;
            let file_type = entry.file_type().with_path(entry.path())?;
            let destination = to.join(entry.file_name());
            if file_type.is_dir() {
                stack.push((entry.path(), destination));
            } else {
                let copied = std::fs::copy(entry.path(), &destination).with_path(entry.path())?;
                files += 1;
                bytes = bytes.saturating_add(copied);
            }
        }
    }
    Ok((files, bytes))
}

fn scan_tree(root: &Path) -> Result<BTreeMap<PathBuf, u64>> {
    let mut map = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).with_path(&current)? {
            let entry = entry.with_path(&current)?;
            let file_type = entry.file_type().with_path(entry.path())?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else {
                let metadata = entry.metadata().with_path(entry.path())?;
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| entry.path());
                map.insert(relative, metadata.len());
            }
        }
    }
    Ok(map)
}

fn move_path(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).with_path(parent)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Cross-volume fallback: copy, verify the size, then delete.
            if from.is_dir() {
                copy_tree(from, to)?;
            } else {
                std::fs::copy(from, to).with_path(from)?;
            }
            let source_size = std::fs::metadata(from).map(|m| m.len()).unwrap_or(0);
            let target_size = std::fs::metadata(to).map(|m| m.len()).unwrap_or(u64::MAX);
            if from.is_file() && source_size != target_size {
                return Err(Error::MigrationFailed {
                    path_from: from.to_path_buf(),
                    path_to: to.to_path_buf(),
                    message: "cross-volume copy produced a different file size".to_string(),
                });
            }
            if from.is_dir() {
                std::fs::remove_dir_all(from).with_path(from)?;
            } else {
                std::fs::remove_file(from).with_path(from)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_windows(appdata: &str, local: &str, profile: &str) -> MapEnv {
        MapEnv::new()
            .with("APPDATA", appdata)
            .with("LOCALAPPDATA", local)
            .with("USERPROFILE", profile)
    }

    #[test]
    fn portable_mode_wins_over_everything() {
        let env = env_windows("C:\\Roaming", "C:\\Local", "C:\\Users\\x")
            .with(PORTABLE_ENV, "D:\\stick\\HylMail");
        let paths = AppPaths::resolve_for(Os::Windows, &env).unwrap();
        assert_eq!(paths.mode(), AppMode::Portable);
        assert_eq!(paths.config_dir(), Path::new("D:\\stick\\HylMail"));
        assert_eq!(paths.data_dir(), Path::new("D:\\stick\\HylMail"));
        assert!(paths.legacy_config_dir().is_none());
    }

    #[test]
    fn windows_uses_roaming_and_local_appdata() {
        let env = env_windows("C:\\Roaming", "C:\\Local", "C:\\Users\\x");
        let paths = AppPaths::resolve_for(Os::Windows, &env).unwrap();
        assert_eq!(
            paths.config_dir(),
            Path::new("C:\\Roaming").join("HylMail").as_path()
        );
        assert_eq!(
            paths.data_dir(),
            Path::new("C:\\Local").join("HylMail").as_path()
        );
        assert_eq!(
            paths.legacy_config_dir(),
            Some(Path::new("C:\\Users\\x").join(".HylMail").as_path())
        );
    }

    #[test]
    fn windows_falls_back_to_roaming_when_local_missing() {
        let env = MapEnv::new()
            .with("APPDATA", "C:\\Roaming")
            .with("USERPROFILE", "C:\\Users\\x");
        let paths = AppPaths::resolve_for(Os::Windows, &env).unwrap();
        assert_eq!(
            paths.data_dir(),
            Path::new("C:\\Roaming").join("HylMail").as_path()
        );
    }

    #[test]
    fn unix_uses_home_directory() {
        let env = MapEnv::new().with("HOME", "/home/ada");
        for os in [Os::Linux, Os::MacOs] {
            let paths = AppPaths::resolve_for(os, &env).unwrap();
            assert_eq!(paths.mode(), AppMode::Standard);
            assert_eq!(paths.config_dir(), Path::new("/home/ada/.HylMail"));
            assert_eq!(paths.data_dir(), Path::new("/home/ada/.HylMail"));
        }
    }

    #[test]
    fn missing_environment_variable_is_reported() {
        let env = MapEnv::new();
        let err = AppPaths::resolve_for(Os::Windows, &env).unwrap_err();
        assert!(matches!(err, Error::MissingEnv { name: "APPDATA" }));

        let err = AppPaths::resolve_for(Os::Linux, &env).unwrap_err();
        assert!(matches!(err, Error::MissingEnv { name: "HOME" }));
    }

    #[test]
    fn empty_environment_variable_is_treated_as_unset() {
        let env = MapEnv::new().with("HOME", "   ");
        assert!(AppPaths::resolve_for(Os::Linux, &env).is_err());
    }

    #[test]
    fn well_known_files_are_placed_correctly() {
        let env = env_windows("C:\\Roaming", "C:\\Local", "C:\\Users\\x");
        let paths = AppPaths::resolve_for(Os::Windows, &env).unwrap();
        assert!(paths.settings_file().ends_with(SETTINGS_FILE));
        assert!(paths.accounts_file().ends_with(ACCOUNTS_FILE));
        assert!(paths.rules_file().ends_with(RULES_FILE));
        assert!(paths.drafts_file().ends_with(DRAFTS_FILE));
        assert!(paths.outbox_file().ends_with(OUTBOX_FILE));
        assert_eq!(paths.database_file(), paths.data_dir().join(DATABASE_FILE));
        assert_eq!(paths.logs_dir(), paths.data_dir().join(LOGS_DIR));
        assert_eq!(paths.crash_file(), paths.data_dir().join(CRASH_FILE));
    }

    #[test]
    fn ensure_dirs_creates_the_full_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let env = MapEnv::new().with(PORTABLE_ENV, tmp.path().to_string_lossy());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        paths.ensure_dirs().unwrap();
        assert!(paths.config_dir().is_dir());
        assert!(paths.data_dir().is_dir());
        assert!(paths.logs_dir().is_dir());
        // idempotent
        paths.ensure_dirs().unwrap();
    }

    #[test]
    fn config_dir_has_data_detects_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let env = MapEnv::new().with(PORTABLE_ENV, tmp.path().to_string_lossy());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        paths.ensure_dirs().unwrap();
        assert!(!paths.config_dir_has_data());
        std::fs::write(paths.settings_file(), "{}").unwrap();
        assert!(paths.config_dir_has_data());
    }
}
