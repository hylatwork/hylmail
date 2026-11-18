//! # hylmail-core
//!
//! Business logic of **HylMail**, a cross-platform desktop mail client that
//! unifies IMAP, Exchange (on-premises, EWS) and Novell/MicroFocus GroupWise
//! proxy mailboxes behind one native user interface.
//!
//! This crate is deliberately free of any UI dependency: it can be embedded in
//! the desktop application, driven from the command line or exercised by the
//! test suite without a display server.  Everything a caller needs is reachable
//! through the modules below:
//!
//! | Module        | Responsibility                                                    |
//! |---------------|-------------------------------------------------------------------|
//! | [`models`]    | domain types - accounts, folders, messages, rules, settings        |
//! | [`paths`]     | platform specific data locations, portable mode, Windows migration |
//! | [`store`]     | atomic JSON documents (`settings.json`, `accounts.json`, ...)      |
//! | [`db`]        | SQLite schema, migrations, FTS5 search, LRU content cache          |
//! | [`secrets`]   | OS credential vault abstraction                                    |
//! | [`logging`]   | redacting `tracing` subscriber and crash reports                   |
//! | [`net`]       | TCP dialling and the rustls configuration shared by all protocols  |
//! | [`imap`]      | IMAP: mailbox names, folders, incremental sync, IDLE, flags         |
//! | [`smtp`]      | SMTP: delivery of queued messages                                  |
//! | [`send`]      | submitting a composed message over the account's own protocol       |
//! | [`soap`]      | SOAP DOM, envelopes and the HTTP transport of EWS / GroupWise      |
//! | [`ews`]       | Exchange Web Services: folders, items, MIME, flags                 |
//! | [`groupwise`] | the GroupWise SOAP proxy: sessions, folders, items                 |
//! | [`mime`]      | the MIME pipeline: bodies, attachments, charsets, invitations       |
//! | [`body`]      | on-demand download of a single message body                        |
//! | [`flags`]     | pushing read / flagged state back to the server                    |
//! | [`delete`]    | moving a message into the trash, or removing it for good           |
//! | [`relocate`]  | moving a message into another folder                              |
//! | [`html`]      | HTML sanitising, image policy, plain text generation                |
//! | [`calendar`]  | `text/calendar` invitations                                         |
//! | [`image`]     | SVG rasterisation for the reading pane                             |
//! | [`rules`]     | the local rule engine                                              |
//! | [`compose`]   | building an outbound message from a draft                           |
//! | [`outbox`]    | the outgoing queue and its retry worker                             |
//! | [`ipc`]       | single-instance lock and inter-process command delivery             |
//! | [`oauth`]     | OAuth2 authorisation code flow with PKCE over a loopback listener   |
//! | [`sync`]      | the background engine that drives accounts and publishes events     |
//! | [`bootstrap`] | [`Core`], the façade that wires the subsystems together             |
//!
//! ## Quick start
//!
//! ```no_run
//! use hylmail_core::Core;
//!
//! # fn main() -> hylmail_core::Result<()> {
//! let mut core = Core::bootstrap_from_env()?;
//! let mut settings = core.settings().clone();
//! settings.show_all_inboxes_on_start = true;
//! core.save_settings(settings)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Guarantees
//!
//! * **No secrets on disk.** Passwords and tokens live in the OS credential
//!   vault; the JSON documents only reference them by account id.
//! * **No secrets in the log.** The `tracing` layer scrubs passwords, bearer
//!   tokens, SASL payloads and cookie headers out of every rendered event.
//! * **No `unwrap` in production code.** Every failure is a structured
//!   [`Error`], never a panic.  The workspace denies the corresponding clippy
//!   lints outside of `#[cfg(test)]` code.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::field_reassign_with_default
    )
)]
#![warn(missing_docs)]

pub mod body;
pub mod bootstrap;
pub mod calendar;
pub mod carddav;
pub mod compose;
pub mod db;
pub mod delete;
pub mod error;
pub mod ews;
pub mod flags;
pub mod groupwise;
pub mod html;
pub mod image;
pub mod imap;
pub mod ipc;
pub mod logging;
pub mod mime;
pub mod models;
pub mod net;
pub mod oauth;
pub mod outbox;
pub mod paths;
pub mod relocate;
pub mod rules;
pub mod secrets;
pub mod send;
pub mod smtp;
pub mod soap;
pub mod store;
pub mod sync;

pub use bootstrap::Core;
pub use error::{Error, IoResultExt, Result};
pub use paths::{AppMode, AppPaths, MigrationOutcome, Os};
pub use secrets::{CredentialKey, CredentialKind, InMemorySecretStore, KeyringStore, SecretStore};
pub use store::{AccountsDocument, ConfigStore, DraftsDocument, OutboxDocument};

/// Version of HylMail, single-sourced from `[workspace.package]`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the database schema this build ships.
pub const SCHEMA_VERSION: u32 = db::SCHEMA_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_single_sourced() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
        assert_eq!(version_components(), 3);
    }

    fn version_components() -> usize {
        VERSION.split('.').count()
    }

    #[test]
    fn schema_version_matches_the_migration_list() {
        assert_eq!(db::SCHEMA_VERSION, SCHEMA_VERSION);
        assert_eq!(
            db::migrations::MIGRATIONS.last().map(|m| m.version),
            Some(SCHEMA_VERSION)
        );
    }

    #[test]
    fn core_types_are_reachable_from_the_crate_root() {
        // Compile-time check that the public surface is wired up.
        fn _assert_paths(_: AppPaths) {}
        fn _assert_error(_: Error) {}
        let _: fn() -> Result<()> = || Ok(());
    }
}
