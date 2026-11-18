// Integration tests are allowed to panic: every failing assertion is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Proves the specification's "no secrets in the log" rule against a real log
//! file.
//!
//! There is exactly **one** test in this file on purpose: `tracing` allows a
//! single global subscriber per process, and every integration test file is
//! compiled into its own process, which is exactly what this test needs.

use std::path::Path;

use hylmail_core::logging::{self, REDACTED};
use hylmail_core::models::Settings;
use hylmail_core::paths::{MapEnv, Os, PORTABLE_ENV};
use hylmail_core::AppPaths;

fn paths_in(dir: &Path) -> AppPaths {
    let env = MapEnv::new().with(PORTABLE_ENV, dir.to_string_lossy().to_string());
    AppPaths::resolve_for(Os::Linux, &env).unwrap()
}

fn read_all_logs(directory: &Path) -> String {
    let mut combined = String::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return combined;
    };
    for entry in entries.flatten() {
        if entry.path().is_file() {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                combined.push_str(&content);
            }
        }
    }
    combined
}

#[test]
fn secrets_never_reach_the_log_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    let mut settings = Settings::sanitized_default();
    settings.log_level = "trace".to_string();

    let guard = logging::init_logging(&paths, &settings).expect("logger installs");
    assert!(guard.is_active(), "the file appender must be active");
    assert!(
        paths.logs_dir().is_dir(),
        "the log directory is created on demand"
    );

    tracing::error!(
        password = "topsecret-field-value",
        "login attempt with a structured field"
    );
    tracing::error!("connecting with password=topsecret-inline");
    tracing::error!("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.not-a-real-signature");
    tracing::error!("C: AUTH PLAIN AGpvZQBiYXI=");
    tracing::error!("GET /oauth2/callback?code=4/0AY0e-g7Example&state=x");
    tracing::error!("<Password>soap-password</Password>");
    tracing::error!("1 LOGIN ada@example.org imap-password");
    // The trace line `async-imap` emits for every command it writes.  It only
    // becomes a log record because of the `log` -> `tracing` bridge, and it must
    // be redacted on that path too.
    tracing::trace!(
        r#"encode: input: Some("A001"), Some("A001 LOGIN ada@example.org trace-password\r\n")"#
    );
    tracing::error!("harmless message that must survive");

    // Dropping the guard flushes the non-blocking writer.
    drop(guard);

    let log = read_all_logs(&paths.logs_dir());
    assert!(
        !log.is_empty(),
        "nothing was written to {}",
        paths.logs_dir().display()
    );

    for secret in [
        "topsecret-field-value",
        "topsecret-inline",
        "eyJhbGciOiJIUzI1NiJ9",
        "AGpvZQBiYXI=",
        "4/0AY0e-g7Example",
        "soap-password",
        "imap-password",
        "trace-password",
    ] {
        assert!(
            !log.contains(secret),
            "`{secret}` leaked into the log:\n{log}"
        );
    }

    assert!(log.contains(REDACTED), "no redaction marker in:\n{log}");
    assert!(
        log.contains("harmless message that must survive"),
        "redaction was too aggressive:\n{log}"
    );
    // The user name of an IMAP LOGIN line is not a secret, and ordinary prose
    // around a login attempt must stay readable.
    assert!(log.contains("ada@example.org"));
    assert!(log.contains("login attempt with a structured field"));
}
