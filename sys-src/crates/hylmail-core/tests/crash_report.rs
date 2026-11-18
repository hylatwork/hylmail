// Integration tests are allowed to panic: every failing assertion is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The crash handler: a panic must leave a report behind that the next start
//! can fold into the regular log.
//!
//! Everything lives in a single test function because the panic hook is global
//! to the process and the test harness runs tests in parallel.

use std::path::Path;

use hylmail_core::logging::{drain_crash_reports, install_panic_hook};
use hylmail_core::paths::{MapEnv, Os, PORTABLE_ENV};
use hylmail_core::AppPaths;

fn paths_in(dir: &Path) -> AppPaths {
    let env = MapEnv::new().with(PORTABLE_ENV, dir.to_string_lossy().to_string());
    AppPaths::resolve_for(Os::Linux, &env).unwrap()
}

#[test]
fn panics_are_reported_and_archived() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    paths.ensure_dirs().unwrap();
    install_panic_hook(&paths);

    // --- a normal panic -------------------------------------------------
    let caught = std::panic::catch_unwind(|| {
        panic!("deliberate test panic");
    });
    assert!(caught.is_err(), "the panic must have been raised");

    let report = drain_crash_reports(&paths).expect("report can be read");
    assert!(
        report.contains("deliberate test panic"),
        "panic message missing from:\n{report}"
    );
    assert!(report.contains("HylMail crash report"));
    assert!(report.contains("version"));
    assert!(report.contains(env!("CARGO_PKG_VERSION")));
    assert!(report.contains("backtrace:"));

    // Reading the report archives it, so it is reported exactly once.
    assert!(drain_crash_reports(&paths).unwrap().is_empty());
    assert!(!paths.crash_file().exists());

    let archived = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("absturz.txt.reported-")
        })
        .count();
    assert_eq!(archived, 1, "the report must have been archived");

    // --- a panic with a non-string payload ------------------------------
    install_panic_hook(&paths);
    let caught = std::panic::catch_unwind(|| {
        std::panic::panic_any(42_u32);
    });
    assert!(caught.is_err());
    let report = drain_crash_reports(&paths).expect("report can be read");
    assert!(
        report.contains("non-string panic payload"),
        "unexpected report:\n{report}"
    );

    // --- a second report does not overwrite the first -------------------
    install_panic_hook(&paths);
    let first = std::panic::catch_unwind(|| panic!("first"));
    assert!(first.is_err());
    let second = std::panic::catch_unwind(|| panic!("second"));
    assert!(second.is_err());
    let report = drain_crash_reports(&paths).expect("report can be read");
    assert!(report.contains("first"));
    assert!(report.contains("second"));
}
