//! The HylMail launchers.
//!
//! Two executables are built from this crate:
//!
//! * `hylmail-cli` - the diagnostic and scripting entry point.  Without an
//!   argument it prints the usage text, exactly as before.
//! * `hylmail-gui` - the desktop entry point.  Without an argument it opens the
//!   native window; every option of `hylmail-cli` keeps working, so `--where`,
//!   `--check-config`, `--sync` and a bare `.eml` file behave identically.
//!
//! Both share one startup path:
//!
//! * resolve the data locations and perform the legacy migration,
//! * install the redacting logger and fold a pending crash report into it,
//! * boot [`hylmail_core::Core`] (settings, JSON documents, SQLite, secrets),
//! * dispatch the parsed command line.
//!
//! The single-instance lock ([`hylmail_core::ipc`]) lives in the window, not in
//! the launcher, so a second start of *either* executable hands its `.eml` file
//! to the running window instead of opening a second one.

// A command line tool exists to print; the workspace wide `print_stdout` /
// `print_stderr` lints are therefore relaxed for this crate and only for it.
#![allow(clippy::print_stdout, clippy::print_stderr)]
// Tests are allowed to panic.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod args;
mod mail;
mod sync;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};

use hylmail_core::secrets::KeyringStore;
use hylmail_core::{logging, AppPaths, ConfigStore, Core};

use crate::args::{Args, Command, USAGE};

/// Which binary is running, and therefore what an empty command line means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// `hylmail-cli`: an empty command line prints the usage text.
    OnRequest,
    /// `hylmail-gui`: an empty command line opens the window.
    Window,
}

/// Entry point of the `hylmail-cli` executable.
///
/// `ExitCode` is already `#[must_use]`, so the attribute is not repeated here.
pub fn cli() -> ExitCode {
    finish(run(Launch::OnRequest))
}

/// Entry point of the `hylmail-gui` executable.
///
/// `ExitCode` is already `#[must_use]`, so the attribute is not repeated here.
pub fn desktop() -> ExitCode {
    finish(run(Launch::Window))
}

/// The command that is really executed.
///
/// `hylmail-gui` opens the window when the command line asked for nothing at
/// all.  Every explicit option - `--help`, `--where`, a `.eml` file, even a
/// lone `--verbose` - keeps its usual meaning, so the desktop executable can
/// still be used from a script or as a file handler.
#[must_use]
pub fn effective(launch: Launch, args: &Args) -> Command {
    match launch {
        Launch::Window if !args.explicit => Command::Ui,
        _ => args.command.clone(),
    }
}

/// Turns the result of a launch into a process exit code.
fn finish(result: Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("HylMail: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(launch: Launch) -> Result<ExitCode> {
    let args = args::parse(std::env::args().skip(1))?;
    let command = effective(launch, &args);

    // `--version` and `--help` must work even when the data directories are not
    // readable, so they are answered before any file system access.
    match &command {
        Command::Version => {
            println!("HylMail {version}", version = hylmail_core::VERSION);
            return Ok(ExitCode::SUCCESS);
        }
        Command::Help => {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    let paths = AppPaths::resolve().context("resolving the HylMail data locations")?;

    if command == Command::Where {
        print_locations(&paths);
        return Ok(ExitCode::SUCCESS);
    }

    let core = start_core(&paths, &args)?;

    match command {
        Command::Init => {
            let created = core
                .config()
                .materialise_documents()
                .context("writing the configuration documents")?;
            println!(
                "HylMail {version} initialised ({created} file(s) created).",
                version = hylmail_core::VERSION,
                created = created.len()
            );
            print_locations(core.paths());
            println!(
                "Database schema version: {}",
                core.database()
                    .schema_version()
                    .context("reading the schema version")?
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Stats => {
            print_statistics(&core)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::CheckConfig => check_config(&core),
        Command::Sync(request) => sync::run(&core, &request),
        Command::OAuthLogin(who) => sync::login(&core, &who),
        Command::Parse(path) => {
            let save = args.save_attachments.clone();
            mail::parse_file(&core, &path, save.as_deref())
        }
        Command::Outbox => mail::outbox_report(&core),
        Command::Retry => mail::retry_all(&core),
        Command::Send => mail::send(&core),
        Command::Compose { draft, out } => mail::compose(&core, &draft, out.as_deref()),
        Command::Ui => {
            println!(
                "Starting the {} user interface (bundle id {}) ...",
                hylmail_gui::FRAMEWORK,
                hylmail_gui::BUNDLE_ID
            );
            hylmail_gui::run(core).context("starting the user interface")?;
            Ok(ExitCode::SUCCESS)
        }
        Command::OpenEml(path) => {
            // `run_with` hands the file to a running instance over the
            // single-instance endpoint, or opens the window with it.
            hylmail_gui::run_with(core, vec![path]).context("opening the message")?;
            Ok(ExitCode::SUCCESS)
        }
        // Handled above.
        Command::Version | Command::Help | Command::Where => Ok(ExitCode::SUCCESS),
    }
}

/// Installs logging and boots the core.
fn start_core(paths: &AppPaths, args: &Args) -> Result<Core> {
    // Logging needs the settings, so the document is read before the core is
    // booted.  A broken `settings.json` must not prevent diagnostics: the error
    // is reported and defaults are used.
    let store = ConfigStore::new(paths.clone());
    let settings = match store.load_settings() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("HylMail: could not read settings ({error}); using defaults");
            let mut fallback = hylmail_core::models::Settings::sanitized_default();
            if args.verbose {
                fallback.log_level = "debug".to_string();
            }
            fallback
        }
    };
    let mut settings = settings;
    if args.verbose {
        settings.log_level = "debug".to_string();
    }

    let _guard = logging::init_logging(paths, &settings).context("installing the logger")?;
    // From here on a panic is captured in `absturz.txt` and folded into the log
    // on the next start, so a crash is diagnosable without the user attaching a
    // debugger.  Installing it before the core means a boot failure is captured
    // too.
    logging::install_panic_hook(paths);
    let _ = hylmail_core::logging::trim_log_file(
        &paths.logs_dir().join("hylmail.log"),
        16 * 1024 * 1024,
    );

    let report = logging::drain_crash_reports(paths).context("reading a crash report")?;
    if !report.is_empty() {
        eprintln!("HylMail: a previous run crashed; details were written to the log");
        tracing::error!(report, "crash report from the previous run");
    }

    tracing::info!(
        version = hylmail_core::VERSION,
        portable = paths.mode().is_portable(),
        config = %paths.config_dir().display(),
        data = %paths.data_dir().display(),
        "HylMail starting"
    );

    Core::bootstrap(paths.clone(), Arc::new(KeyringStore::new())).context("booting hylmail-core")
}

fn print_locations(paths: &AppPaths) {
    println!("HylMail data locations");
    println!("  mode        : {:?}", paths.mode());
    println!("  platform    : {:?}", paths.os());
    println!("  config dir  : {}", paths.config_dir().display());
    println!("  data dir    : {}", paths.data_dir().display());
    println!("  settings    : {}", paths.settings_file().display());
    println!("  accounts    : {}", paths.accounts_file().display());
    println!("  rules       : {}", paths.rules_file().display());
    println!("  drafts      : {}", paths.drafts_file().display());
    println!("  outbox      : {}", paths.outbox_file().display());
    println!("  database    : {}", paths.database_file().display());
    println!("  logs        : {}", paths.logs_dir().display());
    if let Some(legacy) = paths.legacy_config_dir() {
        println!("  legacy dir  : {}", legacy.display());
    }
}

fn print_statistics(core: &Core) -> Result<()> {
    let stats = core
        .database()
        .statistics()
        .context("collecting database statistics")?;
    let limit = core.settings().cache_limit_bytes();
    let used = stats.content_bytes;
    let percent = if limit == 0 {
        0.0
    } else {
        (used as f64 / limit as f64) * 100.0
    };

    println!("HylMail {version} statistics", version = core.version());
    println!("  accounts          : {}", stats.accounts);
    println!("  folders           : {}", stats.folders);
    println!("  message headers   : {}", stats.messages);
    println!("  cached bodies     : {}", stats.cached_bodies);
    println!("  conversations     : {}", stats.conversations);
    println!("  contacts          : {}", stats.contacts);
    println!(
        "  content cache     : {} of {} MiB used ({percent:.1}%)",
        used / (1024 * 1024),
        core.settings().cache_limit_mb
    );
    Ok(())
}

fn check_config(core: &Core) -> Result<ExitCode> {
    let mut problems = 0usize;

    match core.config().load_accounts() {
        Ok(document) => {
            for account in &document.accounts {
                if let Err(error) = account.validate() {
                    println!("accounts.json: {}: {error}", account.display_name);
                    problems += 1;
                }
            }
            println!(
                "accounts.json: {} account(s) checked",
                document.accounts.len()
            );
        }
        Err(error) => {
            println!("accounts.json: {error}");
            problems += 1;
        }
    }

    match core.config().load_rules() {
        Ok(rules) => {
            for rule in &rules.rules {
                if let Err(error) = rule.validate() {
                    println!("rules.json: {}: {error}", rule.name);
                    problems += 1;
                }
            }
            println!("rules.json: {} rule(s) checked", rules.rules.len());
        }
        Err(error) => {
            println!("rules.json: {error}");
            problems += 1;
        }
    }

    match core.config().load_drafts() {
        Ok(drafts) => println!("drafts.json: {} draft(s) checked", drafts.drafts.len()),
        Err(error) => {
            println!("drafts.json: {error}");
            problems += 1;
        }
    }

    match core.config().load_outbox() {
        Ok(outbox) => println!("outbox.json: {} queued message(s)", outbox.items.len()),
        Err(error) => {
            println!("outbox.json: {error}");
            problems += 1;
        }
    }

    if let Err(error) = core.settings().validate() {
        println!("settings.json: {error}");
        problems += 1;
    }

    if problems == 0 {
        println!("Configuration is valid.");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("Configuration has {problems} problem(s).");
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hylmail_core::paths::{MapEnv, Os, PORTABLE_ENV};

    #[test]
    fn locations_are_printed_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let env = MapEnv::new().with(PORTABLE_ENV, dir.path().to_string_lossy().to_string());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        print_locations(&paths);
    }

    #[test]
    fn where_does_not_create_anything() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("does-not-exist");
        let env = MapEnv::new().with(PORTABLE_ENV, target.to_string_lossy().to_string());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        print_locations(&paths);
        assert!(!target.exists());
    }

    #[test]
    fn statistics_can_be_printed_for_an_empty_database() {
        let dir = tempfile::tempdir().unwrap();
        let env = MapEnv::new().with(PORTABLE_ENV, dir.path().to_string_lossy().to_string());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        let core = Core::bootstrap(
            paths,
            std::sync::Arc::new(hylmail_core::secrets::InMemorySecretStore::new()),
        )
        .unwrap();
        print_statistics(&core).unwrap();
    }

    #[test]
    fn an_empty_configuration_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let env = MapEnv::new().with(PORTABLE_ENV, dir.path().to_string_lossy().to_string());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        let core = Core::bootstrap(
            paths,
            std::sync::Arc::new(hylmail_core::secrets::InMemorySecretStore::new()),
        )
        .unwrap();
        let code = check_config(&core).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[test]
    fn launch_modes_pick_the_right_default() {
        let empty = Args::default();
        // `hylmail-gui` with nothing on the command line opens the window ...
        assert_eq!(effective(Launch::Window, &empty), Command::Ui);
        // ... while `hylmail-cli` keeps printing the usage text.
        assert_eq!(effective(Launch::OnRequest, &empty), Command::Help);
    }

    #[test]
    fn an_explicit_option_beats_the_desktop_default() {
        for (arguments, expected) in [
            ("--help", Command::Help),
            ("--where", Command::Where),
            ("--check-config", Command::CheckConfig),
            ("--ui", Command::Ui),
        ] {
            let args = args::parse(arguments.split_whitespace().map(ToString::to_string)).unwrap();
            assert_eq!(effective(Launch::Window, &args), expected, "{arguments}");
        }

        let args = args::parse(["nachricht.eml"]).unwrap();
        assert_eq!(
            effective(Launch::Window, &args),
            Command::OpenEml(std::path::PathBuf::from("nachricht.eml"))
        );

        // Logging is not an action: `hylmail-gui --verbose` is still the window.
        let args = args::parse(["--verbose"]).unwrap();
        assert_eq!(effective(Launch::Window, &args), Command::Ui);
    }

    #[test]
    fn a_broken_account_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let env = MapEnv::new().with(PORTABLE_ENV, dir.path().to_string_lossy().to_string());
        let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
        let core = Core::bootstrap(
            paths,
            std::sync::Arc::new(hylmail_core::secrets::InMemorySecretStore::new()),
        )
        .unwrap();
        let mut document = core.config().load_accounts().unwrap();
        document.accounts.push(hylmail_core::models::Account {
            id: hylmail_core::models::AccountId::new(),
            display_name: "Broken".to_string(),
            email: "no-at-sign".to_string(),
            kind: hylmail_core::models::AccountKind::Imap,
            auth: hylmail_core::models::AuthKind::Password,
            username: "user".to_string(),
            incoming: hylmail_core::models::Endpoint::imaps("imap.example.org"),
            outgoing: hylmail_core::models::Endpoint::smtps("smtp.example.org"),
            proxy: None,
            oauth: None,
            enabled: true,
            created_at: hylmail_core::models::utc_now(),
        });
        core.config().save_accounts(&document).unwrap();
        assert_eq!(check_config(&core).unwrap(), ExitCode::FAILURE);
    }
}
