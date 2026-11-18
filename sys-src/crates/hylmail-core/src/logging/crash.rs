//! Crash reports.
//!
//! A panic hook appends a report to `absturz.txt` in the data directory.  The
//! file survives the process, and [`drain_crash_reports`] folds it into the
//! regular log on the next start so that a user can simply send their log
//! directory when something goes wrong.
//!
//! The hook never allocates through panicking paths: it opens the file, writes
//! what it can and ignores every error.  A crash report must not be able to
//! crash the crash handler.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{IoResultExt, Result};
use crate::models::utc_now;
use crate::paths::AppPaths;

/// Installs the global panic hook writing to [`AppPaths::crash_file`].
///
/// Calling this more than once replaces the previous hook; installing it is a
/// deliberate one-time action in the binary entry point.
pub fn install_panic_hook(paths: &AppPaths) {
    let crash_file = paths.crash_file();
    if let Some(parent) = crash_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::panic::set_hook(Box::new(move |info| {
        let timestamp = utc_now().to_rfc3339();
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let payload = panic_payload(info);
        let backtrace = std::backtrace::Backtrace::force_capture();

        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&crash_file)
        {
            let _ = writeln!(file, "----- HylMail crash report -----");
            let _ = writeln!(file, "time     : {timestamp}");
            let _ = writeln!(file, "version  : {}", env!("CARGO_PKG_VERSION"));
            let _ = writeln!(file, "location : {location}");
            let _ = writeln!(file, "message  : {payload}");
            let _ = writeln!(file, "backtrace:\n{backtrace}");
            let _ = writeln!(file);
            let _ = file.flush();
        }
    }));
}

fn panic_payload(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}

/// Reads a pending crash report and moves it out of the way.
///
/// Returns the report text exactly once; subsequent calls return an empty
/// string because the file is renamed to `absturz.txt.reported-<timestamp>`.
pub fn drain_crash_reports(paths: &AppPaths) -> Result<String> {
    let crash_file = paths.crash_file();
    if !crash_file.is_file() {
        return Ok(String::new());
    }
    let text = std::fs::read_to_string(&crash_file).with_path(&crash_file)?;
    let stamp = utc_now().format("%Y%m%dT%H%M%S");
    let archive = crash_file.with_file_name(format!("absturz.txt.reported-{stamp}"));
    std::fs::rename(&crash_file, &archive).with_path(&crash_file)?;
    Ok(text)
}

/// Reports the version of HylMail, taken from `Cargo.toml`.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Truncates a log file that grew beyond `max_bytes`, keeping the newest half.
///
/// Long running installations would otherwise accumulate gigabytes of logs.
pub fn trim_log_file(path: &Path, max_bytes: u64) -> Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::error::Error::io(path, error)),
    };
    if metadata.len() <= max_bytes {
        return Ok(());
    }
    let content = std::fs::read(path).with_path(path)?;
    let keep_from = content.len().saturating_sub((max_bytes / 2) as usize);
    // Start the kept section at a line boundary so the file stays readable.
    let start = content[keep_from..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| keep_from + offset + 1)
        .unwrap_or(keep_from);
    std::fs::write(path, &content[start..]).with_path(path)?;
    Ok(())
}

/// Returns the path a pending crash report would be read from.
#[must_use]
pub fn pending_report_path(paths: &AppPaths) -> PathBuf {
    paths.crash_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{AppPaths, MapEnv, Os, PORTABLE_ENV};

    fn paths(dir: &Path) -> AppPaths {
        let env = MapEnv::new().with(PORTABLE_ENV, dir.to_string_lossy().to_string());
        AppPaths::resolve_for(Os::Linux, &env).unwrap()
    }

    #[test]
    fn missing_report_returns_empty_text() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        assert_eq!(drain_crash_reports(&paths).unwrap(), "");
    }

    #[test]
    fn report_is_returned_once_and_archived() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(paths.crash_file(), "boom").unwrap();
        assert_eq!(drain_crash_reports(&paths).unwrap(), "boom");
        assert_eq!(drain_crash_reports(&paths).unwrap(), "");
        let archived: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("absturz.txt.reported-"))
            .collect();
        assert_eq!(archived.len(), 1);
    }

    #[test]
    fn version_comes_from_the_manifest() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
        assert!(!version().is_empty());
    }

    #[test]
    fn log_trimming_keeps_the_newest_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hylmail.log");
        let mut content = String::new();
        for index in 0..100 {
            content.push_str(&format!("line {index:03}\n"));
        }
        std::fs::write(&log, &content).unwrap();
        trim_log_file(&log, 200).unwrap();
        let trimmed = std::fs::read_to_string(&log).unwrap();
        assert!(trimmed.len() <= 200);
        assert!(trimmed.contains("line 099"));
        assert!(!trimmed.contains("line 000"));
    }

    #[test]
    fn small_logs_are_not_touched() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hylmail.log");
        std::fs::write(&log, "one\ntwo\n").unwrap();
        trim_log_file(&log, 1024).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        // A missing file is not an error either.
        trim_log_file(&dir.path().join("missing.log"), 10).unwrap();
    }

    #[test]
    fn pending_report_path_points_at_the_crash_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        assert_eq!(pending_report_path(&paths), paths.crash_file());
    }
}
