//! Secret storage.
//!
//! Passwords and OAuth2 tokens never touch the JSON configuration files.
//! They are stored in the operating system credential vault (DPAPI backed
//! Credential Manager on Windows, Keychain on macOS, Secret Service on Linux)
//! and are addressed by a [`CredentialKey`] that is derived purely from the
//! account id and the purpose of the secret.
//!
//! In memory every secret is wrapped in [`secrecy::SecretString`], which zeroes
//! the buffer on drop and refuses to implement `Display`, so a secret cannot
//! accidentally end up in a log line or a formatted error message.

use std::collections::BTreeMap;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::models::AccountId;

/// A secret together with the identity it belongs to, as a protocol layer needs
/// it when it logs in.
///
/// The same shape serves IMAP, SMTP, EWS and the GroupWise proxy, so it lives
/// next to the vault rather than in one of the protocol modules.  The `Debug`
/// implementation never prints the secret.
pub enum Credential<'a> {
    /// Password or application specific password.
    Password(&'a SecretString),
    /// OAuth2 access token plus the user name it was issued for.
    OAuth2 {
        /// The account the token belongs to, as the server expects it.
        user: String,
        /// The bearer token.
        token: &'a SecretString,
    },
}

impl std::fmt::Debug for Credential<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password(_) => f.write_str("Credential::Password([REDACTED])"),
            Self::OAuth2 { user, .. } => f
                .debug_struct("Credential::OAuth2")
                .field("user", user)
                .field("token", &"[REDACTED]")
                .finish(),
        }
    }
}

impl Credential<'_> {
    /// `true` when the credential is a bearer token.
    #[must_use]
    pub fn is_token(&self) -> bool {
        matches!(self, Self::OAuth2 { .. })
    }
}

/// Purpose of a stored secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// Password for the incoming (IMAP / EWS / GroupWise) server.
    IncomingPassword,
    /// Password for the outgoing (SMTP submission) server.
    OutgoingPassword,
    /// Password for the GroupWise SOAP proxy.
    ProxyPassword,
    /// OAuth2 refresh token.
    OAuthRefreshToken,
    /// OAuth2 access token, cached until it expires.
    OAuthAccessToken,
    /// PKCE code verifier of an authorisation flow in progress.
    OAuthCodeVerifier,
}

impl CredentialKind {
    /// Stable slug used inside the vault entry name.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::IncomingPassword => "incoming-password",
            Self::OutgoingPassword => "outgoing-password",
            Self::ProxyPassword => "proxy-password",
            Self::OAuthRefreshToken => "oauth-refresh-token",
            Self::OAuthAccessToken => "oauth-access-token",
            Self::OAuthCodeVerifier => "oauth-code-verifier",
        }
    }

    /// Human readable purpose, used in error messages.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::IncomingPassword => "incoming server password",
            Self::OutgoingPassword => "outgoing server password",
            Self::ProxyPassword => "proxy password",
            Self::OAuthRefreshToken => "OAuth2 refresh token",
            Self::OAuthAccessToken => "OAuth2 access token",
            Self::OAuthCodeVerifier => "OAuth2 code verifier",
        }
    }

    /// All kinds, useful for a "forget this account" clean-up.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::IncomingPassword,
            Self::OutgoingPassword,
            Self::ProxyPassword,
            Self::OAuthRefreshToken,
            Self::OAuthAccessToken,
            Self::OAuthCodeVerifier,
        ]
    }
}

/// Address of one secret inside the vault.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CredentialKey {
    /// Account the secret belongs to.
    pub account_id: AccountId,
    /// Purpose of the secret.
    pub kind: CredentialKind,
}

impl CredentialKey {
    /// Creates a key.
    #[must_use]
    pub const fn new(account_id: AccountId, kind: CredentialKind) -> Self {
        Self { account_id, kind }
    }

    /// The `user` component of the vault entry.
    ///
    /// The account id is a UUID and therefore contains no characters that the
    /// platform vaults treat specially, which keeps the name portable.
    #[must_use]
    pub fn entry_user(&self) -> String {
        format!("{}:{}", self.account_id, self.kind.slug())
    }
}

/// Pluggable secret backend.
///
/// The trait exists so that the test suite (and the headless CLI diagnostics)
/// can run without touching the user's real credential vault.
pub trait SecretStore: Send + Sync {
    /// Stores or replaces a secret.
    fn set(&self, key: &CredentialKey, secret: &SecretString) -> Result<()>;

    /// Reads a secret; `Ok(None)` means "no entry".
    fn get(&self, key: &CredentialKey) -> Result<Option<SecretString>>;

    /// Removes a secret; removing a missing entry is not an error.
    fn delete(&self, key: &CredentialKey) -> Result<()>;

    /// Reports whether an entry exists.
    fn has(&self, key: &CredentialKey) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Removes every secret of an account.
    fn forget_account(&self, account_id: AccountId) -> Result<()> {
        for kind in CredentialKind::all() {
            self.delete(&CredentialKey::new(account_id, *kind))?;
        }
        Ok(())
    }
}

/// Service name used for every vault entry.
pub const DEFAULT_SERVICE: &str = "HylMail";

/// [`SecretStore`] backed by the operating system credential vault.
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: String,
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyringStore {
    /// Creates a store using the default service name.
    #[must_use]
    pub fn new() -> Self {
        Self::with_service(DEFAULT_SERVICE)
    }

    /// Creates a store with a custom service name.
    #[must_use]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Service name of this store.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    fn entry(&self, key: &CredentialKey) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &key.entry_user()).map_err(|error| {
            Error::secret(format!(
                "could not open the vault entry for {}: {error}",
                key.kind.label()
            ))
        })
    }
}

impl SecretStore for KeyringStore {
    fn set(&self, key: &CredentialKey, secret: &SecretString) -> Result<()> {
        self.entry(key)?
            .set_password(secret.expose_secret())
            .map_err(|error| {
                Error::secret(format!("could not store the {}: {error}", key.kind.label()))
            })
    }

    fn get(&self, key: &CredentialKey) -> Result<Option<SecretString>> {
        match self.entry(key)?.get_password() {
            Ok(password) => Ok(Some(SecretString::from(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(Error::secret(format!(
                "could not read the {}: {error}",
                key.kind.label()
            ))),
        }
    }

    fn delete(&self, key: &CredentialKey) -> Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(Error::secret(format!(
                "could not delete the {}: {error}",
                key.kind.label()
            ))),
        }
    }
}

/// In-memory [`SecretStore`], used by tests and by the diagnostics mode.
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    entries: parking_lot::Mutex<BTreeMap<CredentialKey, SecretString>>,
}

impl InMemorySecretStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// `true` when nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecretStore for InMemorySecretStore {
    fn set(&self, key: &CredentialKey, secret: &SecretString) -> Result<()> {
        self.entries.lock().insert(
            key.clone(),
            SecretString::from(secret.expose_secret().to_string()),
        );
        Ok(())
    }

    fn get(&self, key: &CredentialKey) -> Result<Option<SecretString>> {
        Ok(self
            .entries
            .lock()
            .get(key)
            .map(|secret| SecretString::from(secret.expose_secret().to_string())))
    }

    fn delete(&self, key: &CredentialKey) -> Result<()> {
        self.entries.lock().remove(key);
        Ok(())
    }
}

/// Wraps a plain string into a [`SecretString`].
#[must_use]
pub fn secret(value: impl Into<String>) -> SecretString {
    SecretString::from(value.into())
}

/// Reads a secret from a store, returning `None` when it is absent.
///
/// A small helper that keeps call sites free of the `Option`/`Result` dance.
pub fn load_secret(
    store: &dyn SecretStore,
    account_id: AccountId,
    kind: CredentialKind,
) -> Result<Option<String>> {
    let key = CredentialKey::new(account_id, kind);
    Ok(store
        .get(&key)?
        .map(|secret| secret.expose_secret().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_are_stable_and_purpose_specific() {
        let account = AccountId::new();
        let key = CredentialKey::new(account, CredentialKind::IncomingPassword);
        assert_eq!(key.entry_user(), format!("{account}:incoming-password"));
        let other = CredentialKey::new(account, CredentialKind::OutgoingPassword);
        assert_ne!(key, other);
    }

    #[test]
    fn credential_kinds_cover_every_account() {
        assert_eq!(CredentialKind::all().len(), 6);
        for kind in CredentialKind::all() {
            assert!(!kind.slug().is_empty());
            assert!(!kind.label().is_empty());
        }
    }

    #[test]
    fn in_memory_store_round_trips() {
        let store = InMemorySecretStore::new();
        let key = CredentialKey::new(AccountId::new(), CredentialKind::IncomingPassword);
        assert!(store.get(&key).unwrap().is_none());
        assert!(!store.has(&key).unwrap());

        store.set(&key, &secret("hunter2")).unwrap();
        assert!(store.has(&key).unwrap());
        assert_eq!(store.get(&key).unwrap().unwrap().expose_secret(), "hunter2");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn in_memory_store_can_be_overwritten_and_cleared() {
        let store = InMemorySecretStore::new();
        let key = CredentialKey::new(AccountId::new(), CredentialKind::OAuthRefreshToken);
        store.set(&key, &secret("first")).unwrap();
        store.set(&key, &secret("second")).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(&key).unwrap().unwrap().expose_secret(), "second");
        store.delete(&key).unwrap();
        assert!(store.is_empty());
        // Deleting twice is fine.
        store.delete(&key).unwrap();
    }

    #[test]
    fn forget_account_removes_every_kind() {
        let store = InMemorySecretStore::new();
        let account = AccountId::new();
        let other = AccountId::new();
        for kind in CredentialKind::all() {
            store
                .set(&CredentialKey::new(account, *kind), &secret("x"))
                .unwrap();
        }
        store
            .set(
                &CredentialKey::new(other, CredentialKind::IncomingPassword),
                &secret("keep"),
            )
            .unwrap();
        assert_eq!(store.len(), 7);

        store.forget_account(account).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store
            .get(&CredentialKey::new(other, CredentialKind::IncomingPassword))
            .unwrap()
            .is_some());
    }

    #[test]
    fn load_secret_helper_returns_plain_text_only_on_request() {
        let store = InMemorySecretStore::new();
        let account = AccountId::new();
        assert!(
            load_secret(&store, account, CredentialKind::IncomingPassword)
                .unwrap()
                .is_none()
        );
        store
            .set(
                &CredentialKey::new(account, CredentialKind::IncomingPassword),
                &secret("pw"),
            )
            .unwrap();
        assert_eq!(
            load_secret(&store, account, CredentialKind::IncomingPassword).unwrap(),
            Some("pw".to_string())
        );
    }

    #[test]
    fn secret_strings_do_not_reveal_their_content_when_formatted() {
        let secret = secret("super-secret");
        let rendered = format!("{secret:?}");
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("super-secret"));
    }

    #[test]
    fn keyring_store_exposes_its_service_name() {
        let store = KeyringStore::new();
        assert_eq!(store.service(), DEFAULT_SERVICE);
        let custom = KeyringStore::with_service("HylMail-Test");
        assert_eq!(custom.service(), "HylMail-Test");
        // The entry name must be reproducible without touching the vault.
        let key = CredentialKey::new(AccountId::new(), CredentialKind::ProxyPassword);
        assert!(key.entry_user().ends_with(":proxy-password"));
    }

    /// Round trip against the real OS vault.
    ///
    /// Ignored by default because it writes to the credential vault of the
    /// machine running the tests.  Run explicitly with
    /// `cargo test -- --ignored keyring_round_trip`.
    #[test]
    #[ignore = "writes to the operating system credential vault"]
    fn keyring_round_trip() {
        let store = KeyringStore::with_service("HylMail-Test-RoundTrip");
        let key = CredentialKey::new(AccountId::new(), CredentialKind::IncomingPassword);
        store.delete(&key).unwrap();
        assert!(store.get(&key).unwrap().is_none());
        store.set(&key, &secret("round-trip")).unwrap();
        assert_eq!(
            store.get(&key).unwrap().unwrap().expose_secret(),
            "round-trip"
        );
        store.delete(&key).unwrap();
        assert!(store.get(&key).unwrap().is_none());
    }
}
