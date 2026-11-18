//! Strongly typed identifiers and time helpers.
//!
//! Every persistent entity gets its own newtype around [`uuid::Uuid`] so that a
//! `FolderId` can never be passed where a `MessageId` is expected.  The
//! newtypes serialise transparently as plain strings, which keeps the JSON
//! documents readable and the SQLite columns simple `TEXT` values.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};

/// Generates a newtype identifier around a UUID v4.
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(uuid::Uuid);

        impl $name {
            /// Creates a fresh, random identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }

            /// Wraps an existing UUID.
            #[must_use]
            pub const fn from_uuid(uuid: uuid::Uuid) -> Self {
                Self(uuid)
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub const fn as_uuid(self) -> uuid::Uuid {
                self.0
            }

            /// Parses an identifier from its textual representation.
            pub fn parse(input: &str) -> Result<Self, uuid::Error> {
                Ok(Self(uuid::Uuid::parse_str(input)?))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl From<uuid::Uuid> for $name {
            fn from(value: uuid::Uuid) -> Self {
                Self(value)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl AsRef<uuid::Uuid> for $name {
            fn as_ref(&self) -> &uuid::Uuid {
                &self.0
            }
        }
    };
}

define_id!(
    /// Identifies a configured mail account.
    AccountId
);
define_id!(
    /// Identifies a folder (mailbox) of an account.
    FolderId
);
define_id!(
    /// Identifies a locally cached message.
    MessageId
);
define_id!(
    /// Identifies an automatic rule.
    RuleId
);
define_id!(
    /// Identifies an address book contact.
    ContactId
);
define_id!(
    /// Identifies a locally stored draft.
    DraftId
);
define_id!(
    /// Identifies an entry of the outgoing queue.
    OutboxId
);
define_id!(
    /// Identifies a mail signature.
    SignatureId
);
define_id!(
    /// Identifies a logical conversation thread.
    ///
    /// Thread identifiers are derived from the RFC 5322 `References` chain
    /// (falling back to `Message-ID`) and are stable across restarts.
    CorrelationId
);
define_id!(
    /// Identifies a synchronisation job.
    SyncJobId
);

/// Returns the current wall-clock time in UTC.
///
/// A single helper keeps the timestamp source swappable and makes it obvious in
/// review that no local time zone leaks into persisted data.
#[must_use]
pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// Converts an optional timestamp into Unix milliseconds for SQLite storage.
#[must_use]
pub fn to_millis(value: Option<DateTime<Utc>>) -> Option<i64> {
    value.map(|instant| instant.timestamp_millis())
}

/// Converts Unix milliseconds from SQLite back into a UTC timestamp.
#[must_use]
pub fn from_millis(value: Option<i64>) -> Option<DateTime<Utc>> {
    value.and_then(|millis| Utc.timestamp_millis_opt(millis).single())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip() {
        let a = AccountId::new();
        let b = AccountId::new();
        assert_ne!(a, b);

        let parsed: AccountId = a.to_string().parse().unwrap();
        assert_eq!(a, parsed);
        assert_eq!(AccountId::from_uuid(a.as_uuid()), a);
    }

    #[test]
    fn ids_serialise_transparently() {
        let id = MessageId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        let back: MessageId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn ids_have_distinct_types() {
        // Compile-time proof that the newtypes are not interchangeable: this
        // would not type check if `FolderId` were a plain UUID.
        fn takes_folder(_: FolderId) {}
        takes_folder(FolderId::new());
    }

    #[test]
    fn millis_conversion_round_trips() {
        let now = utc_now();
        let millis = to_millis(Some(now)).unwrap();
        let back = from_millis(Some(millis)).unwrap();
        assert_eq!(now.timestamp_millis(), back.timestamp_millis());
        assert_eq!(to_millis(None), None);
        assert_eq!(from_millis(None), None);
    }
}
