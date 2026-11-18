//! Logging bootstrap.
//!
//! [`init_logging`] installs a global `tracing` subscriber with two sinks:
//!
//! * a rolling file appender in `<data-dir>/logs/hylmail.log`, and
//! * standard error, so that a user starting HylMail from a terminal sees what
//!   is going on.
//!
//! Both sinks are wrapped in [`RedactingFormat`], which is what enforces the
//! specification's "no secrets in the log" rule.  Redaction happens on the
//! *rendered* event, so it also covers fields the application adds later.

pub mod crash;
pub mod redact;

use std::sync::Arc;

use tracing_subscriber::fmt::format::Format;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::error::Result;
use crate::models::Settings;
use crate::paths::AppPaths;

pub use crash::{
    drain_crash_reports, install_panic_hook, pending_report_path, trim_log_file, version,
};
pub use redact::{RedactingFormat, Redactor, REDACTED};

/// Keeps the non-blocking log writer alive.
///
/// Dropping the guard flushes the log and stops the writer thread; hold it for
/// the whole lifetime of the application.
#[derive(Debug, Default)]
pub struct LoggingGuard {
    worker: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl LoggingGuard {
    /// `true` when file logging is actually active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.worker.is_some()
    }
}

/// Installs the global subscriber.
///
/// The log level is taken from `RUST_LOG` when set, otherwise from
/// [`Settings::log_level`].  Calling the function twice is not an error: the
/// second call keeps the already installed subscriber and returns an inactive
/// guard, which keeps the unit tests free of ordering hazards.
pub fn init_logging(paths: &AppPaths, settings: &Settings) -> Result<LoggingGuard> {
    paths.ensure_dirs()?;

    let redactor = Arc::new(Redactor::default());
    let filter = build_filter(&settings.log_level);

    let file_appender = tracing_appender::rolling::daily(paths.logs_dir(), "hylmail.log");
    let (writer, worker) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .event_format(RedactingFormat::new(
            Format::default().with_ansi(false),
            Arc::clone(&redactor),
        ));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .event_format(RedactingFormat::new(Format::default(), redactor));

    let install_result = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();

    match install_result {
        Ok(()) => Ok(LoggingGuard {
            worker: Some(worker),
        }),
        Err(error) => {
            // Something already installed a global subscriber (a test harness
            // or an embedding application).  Diagnostics still go to that
            // subscriber, so this is not a reason to abort the start.
            #[allow(clippy::print_stderr)]
            {
                eprintln!("HylMail: a global tracing subscriber is already installed ({error:?})");
            }
            Ok(LoggingGuard::default())
        }
    }
}

fn build_filter(configured_level: &str) -> EnvFilter {
    if let Ok(environment) = std::env::var("RUST_LOG") {
        if let Ok(filter) = EnvFilter::try_new(&environment) {
            return filter;
        }
    }
    EnvFilter::try_new(configured_level).unwrap_or_else(|_| EnvFilter::new("info"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{MapEnv, Os, PORTABLE_ENV};

    #[test]
    fn filter_falls_back_to_info_for_garbage() {
        // A broken level must never prevent the application from starting.
        let filter = build_filter("definitely not a filter ,,,");
        assert!(!filter.to_string().is_empty());
    }

    #[test]
    fn filter_accepts_a_plain_level() {
        let filter = build_filter("debug");
        assert!(filter.to_string().contains("debug"));
    }

    #[test]
    fn logging_guard_default_is_inactive() {
        assert!(!LoggingGuard::default().is_active());
    }

    #[test]
    fn init_logging_creates_the_log_directory() {
        let dir = tempfile::tempdir().unwrap();
        let env = MapEnv::new().with(PORTABLE_ENV, dir.path().to_string_lossy().to_string());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        let settings = Settings::default();
        // Installing twice must not panic; the first call may or may not win
        // depending on whether another test installed a subscriber already.
        let first = init_logging(&paths, &settings);
        assert!(first.is_ok());
        let _ = init_logging(&paths, &settings);
        assert!(paths.logs_dir().is_dir());
    }
}
