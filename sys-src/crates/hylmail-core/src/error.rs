//! Error types for `hylmail-core`.
//!
//! The crate deliberately avoids `anyhow` in its public API: every fallible
//! operation returns [`Result`], a type alias over the structured [`Error`]
//! enum.  This keeps the boundary to the UI layer explicit and lets callers
//! decide how to present a failure to the user.

use std::path::PathBuf;

/// Result alias used throughout `hylmail-core`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every recoverable failure raised by the core crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A configuration file was structurally valid JSON but semantically
    /// invalid (missing host, out-of-range port, duplicate account id, ...).
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The requested setting value is outside of the documented range.
    #[error("invalid settings value for `{field}`: {message}")]
    InvalidSettings {
        /// Name of the offending field.
        field: &'static str,
        /// Why the value was rejected.
        message: String,
    },

    /// A filesystem operation failed; the path is preserved for the user.
    #[error("i/o error on `{}`: {source}", path.display())]
    Io {
        /// Path the operation was performed on.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// JSON encoding or decoding failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// SQLite reported an error.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// A schema migration could not be applied.
    #[error("database migration to version {version} (`{name}`) failed: {source}")]
    Migration {
        /// Target schema version of the failing migration.
        version: u32,
        /// Human readable migration name.
        name: &'static str,
        /// Underlying database error.
        #[source]
        source: rusqlite::Error,
    },

    /// The stored schema is newer than the running binary understands.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version found in the database file.
        found: u32,
        /// Highest version this build knows about.
        supported: u32,
    },

    /// The OS credential vault refused an operation.
    #[error("secret store error: {0}")]
    Secret(String),

    /// A required environment variable was not present.
    #[error("required environment variable `{name}` is not set")]
    MissingEnv {
        /// Name of the missing variable.
        name: &'static str,
    },

    /// Data migration between two locations failed; the original data was
    /// left untouched (or restored) wherever possible.
    #[error("migration from `{}` to `{}` failed: {message}", path_from.display(), path_to.display())]
    MigrationFailed {
        /// Source directory of the migration.
        path_from: PathBuf,
        /// Target directory of the migration.
        path_to: PathBuf,
        /// Human readable explanation.
        message: String,
    },

    /// The requested identifier does not exist in the local database.
    #[error("no {entity} with id `{id}`")]
    NotFound {
        /// Logical entity name, e.g. `folder`.
        entity: &'static str,
        /// Identifier that could not be resolved.
        id: String,
    },

    /// A TCP connection could not be established.
    #[error("cannot connect to {endpoint}: {source}")]
    Connect {
        /// `host:port` that was dialed.
        endpoint: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The TLS handshake failed: unknown issuer, expired certificate, protocol
    /// mismatch or an ALPN failure.
    #[error("TLS handshake with `{host}` failed: {message}")]
    Tls {
        /// Host name that was requested (also the SNI name).
        host: String,
        /// Human readable reason.
        message: String,
    },

    /// An operation exceeded its deadline.
    #[error("`{operation}` timed out after {seconds} seconds")]
    Timeout {
        /// Human readable operation name, e.g. `login`.
        operation: String,
        /// Deadline in seconds.
        seconds: u64,
    },

    /// The server answered a command with `NO` or `BAD`.
    #[error("{protocol} `{command}` failed: {message}")]
    Protocol {
        /// Protocol name, e.g. `IMAP`.
        protocol: &'static str,
        /// Command that failed.
        command: String,
        /// Server supplied reason, already stripped of credentials.
        message: String,
    },

    /// Authentication was rejected by the server.
    #[error("authentication for `{account}` failed: {message}")]
    Auth {
        /// Account that was being authenticated.
        account: String,
        /// Reason reported by the server.
        message: String,
    },

    /// The server does not offer a capability the operation needs.
    #[error("{protocol} server `{host}` does not support {capability}")]
    MissingCapability {
        /// Protocol name, e.g. `IMAP`.
        protocol: &'static str,
        /// Host of the server.
        host: String,
        /// Capability that is missing, e.g. `MOVE`.
        capability: String,
    },

    /// A response could not be parsed or violated the protocol.
    #[error("malformed {protocol} response: {message}")]
    MalformedResponse {
        /// Protocol name, e.g. `IMAP`.
        protocol: &'static str,
        /// What exactly was unexpected.
        message: String,
    },

    /// The peer closed the connection, or the connection was reset.
    #[error("the {protocol} connection to `{host}` was lost")]
    ConnectionLost {
        /// Protocol name, e.g. `IMAP`.
        protocol: &'static str,
        /// Host of the server.
        host: String,
    },

    /// The TLS configuration could not be built (unreadable extra root, no
    /// crypto provider).
    #[error("invalid TLS configuration: {0}")]
    TlsConfig(String),

    /// The account kind cannot perform the requested operation.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

impl Error {
    /// Builds an [`Error::Io`] from a path and an OS error.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Returns the raw [`std::io::ErrorKind`] if this error wraps an i/o error.
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            Self::Io { source, .. } => Some(source.kind()),
            _ => None,
        }
    }

    /// Convenience constructor for [`Error::Config`].
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Convenience constructor for [`Error::Secret`].
    pub fn secret(message: impl Into<String>) -> Self {
        Self::Secret(message.into())
    }

    /// Convenience constructor for [`Error::Protocol`].
    pub fn protocol(
        protocol: &'static str,
        command: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Protocol {
            protocol,
            command: command.into(),
            message: message.into(),
        }
    }

    /// Convenience constructor for [`Error::MalformedResponse`].
    pub fn malformed(protocol: &'static str, message: impl Into<String>) -> Self {
        Self::MalformedResponse {
            protocol,
            message: message.into(),
        }
    }

    /// Convenience constructor for [`Error::Timeout`].
    pub fn timeout(operation: impl Into<String>, seconds: u64) -> Self {
        Self::Timeout {
            operation: operation.into(),
            seconds,
        }
    }

    /// `true` when the operation is worth retrying later: the network failed,
    /// the peer went away, or the server answered `NO`/`BAD` because it is
    /// temporarily overloaded.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Connect { source, .. } => !matches!(
                source.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::AddrNotAvailable
            ),
            Self::Timeout { .. } | Self::ConnectionLost { .. } | Self::Io { .. } => true,
            Self::Protocol { .. } | Self::Tls { .. } => false,
            _ => false,
        }
    }
}

/// Extension trait that maps `std::io::Result` onto the crate [`Result`] while
/// attaching the path that was being accessed.  Keeping the path in the error
/// is essential for a desktop application: the user must be able to see *which*
/// file blocked the operation.
pub trait IoResultExt<T> {
    /// Converts the i/o result, attaching `path` to any error.
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::io(path, source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_helper_preserves_path_and_kind() {
        let err = std::fs::read_to_string("definitely/missing/file.txt")
            .with_path("definitely/missing/file.txt")
            .unwrap_err();
        assert_eq!(err.io_kind(), Some(std::io::ErrorKind::NotFound));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn constructs_structured_errors() {
        let err = Error::config("host missing");
        assert!(matches!(err, Error::Config(_)));
        let err = Error::secret("vault locked");
        assert!(matches!(err, Error::Secret(_)));
    }

    #[test]
    fn schema_too_new_is_reported() {
        let err = Error::SchemaTooNew {
            found: 9,
            supported: 2,
        };
        assert!(err.to_string().contains('9'));
        assert!(err.to_string().contains('2'));
    }

    #[test]
    fn protocol_errors_read_well() {
        let err = Error::protocol("IMAP", "SELECT", "mailbox does not exist");
        assert_eq!(
            err.to_string(),
            "IMAP `SELECT` failed: mailbox does not exist"
        );
        let err = Error::malformed("IMAP", "missing closing parenthesis");
        assert!(err.to_string().contains("missing closing parenthesis"));
        let err = Error::timeout("login", 30);
        assert!(err.to_string().contains("30 seconds"));
    }

    #[test]
    fn transient_failures_are_recognised() {
        assert!(Error::timeout("login", 30).is_transient());
        assert!(Error::ConnectionLost {
            protocol: "IMAP",
            host: "imap.example.org".to_string(),
        }
        .is_transient());
        assert!(!Error::protocol("IMAP", "SELECT", "no such mailbox").is_transient());
        assert!(!Error::Tls {
            host: "imap.example.org".to_string(),
            message: "bad certificate".to_string(),
        }
        .is_transient());
    }
}
