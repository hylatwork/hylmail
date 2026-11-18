// Integration tests are allowed to panic: every failing assertion is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Integration tests for the transactional migration of a legacy
//! `%USERPROFILE%\.HylMail` directory into the split Windows layout.
//!
//! The migration is the only operation in phase 1 that moves user data, so it
//! is tested against a real file system instead of a mocked one.

use std::fs;
use std::path::{Path, PathBuf};

use hylmail_core::paths::{migrate_tree, AppMode, MapEnv, MigrationOutcome, Os, PORTABLE_ENV};
use hylmail_core::{AppPaths, Error};

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// Builds a legacy directory that looks like an old HylMail installation.
fn legacy_tree(root: &Path) -> PathBuf {
    let legacy = root.join("home").join(".HylMail");
    write(&legacy.join("settings.json"), r#"{"page_size": 250}"#);
    write(&legacy.join("accounts.json"), r#"{"accounts": []}"#);
    write(&legacy.join("rules.json"), r#"{"rules": []}"#);
    write(&legacy.join("hylmail.db"), "SQLite format 3\0");
    write(&legacy.join("logs").join("hylmail.log"), "old log line\n");
    legacy
}

fn windows_paths(root: &Path) -> AppPaths {
    let env = MapEnv::new()
        .with(
            "APPDATA",
            root.join("roaming").to_string_lossy().to_string(),
        )
        .with(
            "LOCALAPPDATA",
            root.join("local").to_string_lossy().to_string(),
        )
        .with(
            "USERPROFILE",
            root.join("home").to_string_lossy().to_string(),
        );
    AppPaths::resolve_for(Os::Windows, &env).unwrap()
}

#[test]
fn legacy_configuration_is_moved_into_the_roaming_profile() {
    let root = tempfile::tempdir().unwrap();
    let legacy = legacy_tree(root.path());
    let paths = windows_paths(root.path());

    let outcome = paths.migrate_legacy_layout().unwrap();
    let MigrationOutcome::Migrated {
        files,
        bytes,
        source_removed,
        ..
    } = outcome
    else {
        panic!("expected a migration, got {outcome:?}");
    };

    assert_eq!(files, 5);
    assert!(bytes > 0);
    assert!(source_removed, "the legacy directory must be gone");
    assert!(!legacy.exists());

    // Configuration documents end up in the roaming profile ...
    assert!(paths.settings_file().is_file());
    assert!(paths.accounts_file().is_file());
    assert!(paths.rules_file().is_file());
    assert_eq!(
        fs::read_to_string(paths.settings_file()).unwrap(),
        r#"{"page_size": 250}"#
    );

    // ... while the cache (database and logs) is relocated to the local profile.
    assert!(paths.database_file().is_file());
    assert!(paths.logs_dir().join("hylmail.log").is_file());
    assert!(!paths.config_dir().join("hylmail.db").exists());
}

#[test]
fn migration_is_a_no_op_when_there_is_nothing_to_migrate() {
    let root = tempfile::tempdir().unwrap();
    let paths = windows_paths(root.path());
    let outcome = paths.migrate_legacy_layout().unwrap();
    assert_eq!(
        outcome,
        MigrationOutcome::NotNeeded {
            reason: "legacy directory does not exist"
        }
    );
}

#[test]
fn an_existing_installation_is_never_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let legacy = legacy_tree(root.path());
    let paths = windows_paths(root.path());
    paths.ensure_dirs().unwrap();
    write(&paths.settings_file(), r#"{"page_size": 999}"#);

    let outcome = paths.migrate_legacy_layout().unwrap();
    assert_eq!(
        outcome,
        MigrationOutcome::NotNeeded {
            reason: "target directory already contains data"
        }
    );
    // The legacy data must still be intact.
    assert!(legacy.join("settings.json").is_file());
    assert_eq!(
        fs::read_to_string(paths.settings_file()).unwrap(),
        r#"{"page_size": 999}"#
    );
}

#[test]
fn portable_mode_never_migrates() {
    let root = tempfile::tempdir().unwrap();
    legacy_tree(root.path());
    let env = MapEnv::new()
        .with(
            PORTABLE_ENV,
            root.path().join("stick").to_string_lossy().to_string(),
        )
        .with(
            "APPDATA",
            root.path().join("roaming").to_string_lossy().to_string(),
        )
        .with(
            "USERPROFILE",
            root.path().join("home").to_string_lossy().to_string(),
        );
    let paths = AppPaths::resolve_for(Os::Windows, &env).unwrap();
    assert_eq!(paths.mode(), AppMode::Portable);
    let outcome = paths.migrate_legacy_layout().unwrap();
    assert_eq!(
        outcome,
        MigrationOutcome::NotNeeded {
            reason: "portable mode active"
        }
    );
}

#[test]
fn unix_installations_do_not_use_the_windows_migration() {
    let root = tempfile::tempdir().unwrap();
    let env = MapEnv::new().with(
        "HOME",
        root.path().join("home").to_string_lossy().to_string(),
    );
    let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
    let outcome = paths.migrate_legacy_layout().unwrap();
    assert_eq!(
        outcome,
        MigrationOutcome::NotNeeded {
            reason: "not running on windows"
        }
    );
}

#[test]
fn overlapping_directories_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("a");
    fs::create_dir_all(source.join("nested")).unwrap();
    let error = migrate_tree(&source, &source.join("nested")).unwrap_err();
    assert!(matches!(error, Error::MigrationFailed { .. }));
    assert!(
        source.is_dir(),
        "the source must survive a rejected migration"
    );
}

#[test]
fn a_failing_migration_leaves_the_source_untouched() {
    let root = tempfile::tempdir().unwrap();
    let legacy = legacy_tree(root.path());

    // The target's parent is a regular file, so the staging directory cannot be
    // created and the migration must abort before it commits anything.
    let blocker = root.path().join("blocker");
    write(&blocker, "not a directory");
    let target = blocker.join("HylMail");

    let error = migrate_tree(&legacy, &target).unwrap_err();
    assert!(
        matches!(error, Error::MigrationFailed { .. } | Error::Io { .. }),
        "unexpected error: {error}"
    );

    // Nothing was copied and the source is complete.
    assert_eq!(fs::read_dir(&legacy).unwrap().count(), 5);
    assert!(legacy.join("settings.json").is_file());
    assert!(legacy.join("hylmail.db").is_file());
    assert!(legacy.join("logs").join("hylmail.log").is_file());
}

#[test]
fn a_non_empty_target_is_left_alone() {
    let root = tempfile::tempdir().unwrap();
    let legacy = legacy_tree(root.path());
    let target = root.path().join("target");
    write(&target.join("keep.txt"), "keep me");

    let outcome = migrate_tree(&legacy, &target).unwrap();
    assert_eq!(
        outcome,
        MigrationOutcome::NotNeeded {
            reason: "target directory is not empty"
        }
    );
    assert!(target.join("keep.txt").is_file());
    assert!(legacy.join("settings.json").is_file());
}

#[test]
fn an_empty_target_directory_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    let legacy = legacy_tree(root.path());
    let target = root.path().join("target");
    fs::create_dir_all(&target).unwrap();

    let outcome = migrate_tree(&legacy, &target).unwrap();
    assert!(outcome.moved());
    assert!(target.join("settings.json").is_file());
    assert!(!target.join("keep.txt").exists());
    assert!(!legacy.exists());
}

#[test]
fn no_staging_directory_survives_a_successful_migration() {
    let root = tempfile::tempdir().unwrap();
    legacy_tree(root.path());
    let paths = windows_paths(root.path());
    paths.migrate_legacy_layout().unwrap();

    let stray: Vec<String> = fs::read_dir(root.path().join("roaming"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("migrating"))
        .collect();
    assert!(
        stray.is_empty(),
        "staging directories left behind: {stray:?}"
    );
}
