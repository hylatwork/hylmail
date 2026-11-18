//! The IMAP client: folder discovery, incremental synchronisation, IDLE and
//! flag handling.
//!
//! The layer is split so that almost everything can be tested without a server:
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`utf7`] | modified UTF-7 mailbox names ([RFC 3501 §5.1.3]) |
//! | [`capabilities`] | what the server supports (`IDLE`, `MOVE`, `UIDPLUS`, ...) |
//! | [`mailbox`] | `LIST` responses turned into [`crate::models::Folder`] |
//! | [`header`] | raw header bytes, RFC 2047 words, addresses and dates |
//! | [`flags`] | IMAP flag atoms ↔ the local flag bit sets |
//! | [`fetch`] | `FETCH` responses turned into headers and attachments |
//! | [`connection`] | TCP + TLS + login, one socket at a time |
//! | [`synchronise`] | the algorithm that keeps the local database in step |
//! | [`idle`] | IDLE with the mandatory 29 minute re-issue |
//!
//! [RFC 3501 §5.1.3]: https://datatracker.ietf.org/doc/html/rfc3501#section-5.1.3

pub mod capabilities;
pub mod connection;
pub mod fetch;
pub mod flags;
pub mod header;
pub mod idle;
pub mod mailbox;
pub mod synchronise;
pub mod utf7;

pub use capabilities::Capabilities;
pub use connection::{Credential, ImapConfig, ImapSession, LoginOutcome};
pub use synchronise::{FolderSyncReport, SyncDepth, SyncOptions, SyncProgress};
pub use utf7::{decode as decode_mailbox_name, encode as encode_mailbox_name};
