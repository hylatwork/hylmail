//! Mail account descriptions.
//!
//! An [`Account`] contains everything needed to connect to a mailbox *except*
//! the credentials themselves.  Passwords and OAuth2 tokens live in the OS
//! credential vault and are addressed through [`crate::secrets::CredentialKey`]
//! derived from the account id, so that `accounts.json` can be backed up or
//! synchronised without ever leaking a secret.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::models::ids::AccountId;

/// Transport security mode of a TCP endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Plain connection, no encryption (only offered for on-premises relays).
    None,
    /// The connection starts in clear text and is upgraded via `STARTTLS`.
    StartTls,
    /// TLS from the first byte ("implicit TLS", e.g. IMAPS / SMTPS).
    #[default]
    Implicit,
}

impl TlsMode {
    /// Human readable label for the account wizard.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::StartTls => "STARTTLS",
            Self::Implicit => "SSL/TLS",
        }
    }

    /// `true` when the payload is encrypted.
    #[must_use]
    pub const fn is_encrypted(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// A host/port/TLS triple describing one server endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Endpoint {
    /// Host name or IP address.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// How the connection is secured.
    pub tls: TlsMode,
}

impl Endpoint {
    /// Creates a new endpoint.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16, tls: TlsMode) -> Self {
        Self {
            host: host.into(),
            port,
            tls,
        }
    }

    /// Well-known IMAPS endpoint.
    #[must_use]
    pub fn imaps(host: impl Into<String>) -> Self {
        Self::new(host, 993, TlsMode::Implicit)
    }

    /// Well-known submission endpoint (SMTPS).
    #[must_use]
    pub fn smtps(host: impl Into<String>) -> Self {
        Self::new(host, 465, TlsMode::Implicit)
    }

    /// Default GroupWise SOAP proxy endpoint.
    #[must_use]
    pub fn groupwise_proxy(host: impl Into<String>) -> Self {
        Self::new(host, 7191, TlsMode::Implicit)
    }

    /// Validates host and port.
    pub fn validate(&self, field: &'static str) -> Result<()> {
        if self.host.trim().is_empty() {
            return Err(Error::InvalidSettings {
                field,
                message: "host must not be empty".to_string(),
            });
        }
        if self.host.contains(char::is_whitespace) {
            return Err(Error::InvalidSettings {
                field,
                message: format!("host `{}` must not contain whitespace", self.host),
            });
        }
        if self.port == 0 {
            return Err(Error::InvalidSettings {
                field,
                message: "port must be between 1 and 65535".to_string(),
            });
        }
        Ok(())
    }
}

/// Which family of protocols an account speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// IMAP4rev1 + SMTP submission.
    Imap,
    /// Exchange Web Services (on-premises).
    Exchange,
    /// Novell / MicroFocus GroupWise via a SOAP proxy.
    GroupWiseProxy,
}

impl AccountKind {
    /// Label used in the account list.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Imap => "IMAP",
            Self::Exchange => "Exchange (EWS)",
            Self::GroupWiseProxy => "GroupWise Proxy",
        }
    }

    /// The rule engine from the specification is disabled for GroupWise proxy
    /// accounts because the proxy offers no way to move messages server side.
    #[must_use]
    pub const fn supports_rules(self) -> bool {
        !matches!(self, Self::GroupWiseProxy)
    }

    /// Whether a calendar view can be offered for this account.
    #[must_use]
    pub const fn supports_calendar(self) -> bool {
        !matches!(self, Self::GroupWiseProxy)
    }

    /// Whether contacts can be synchronised for this account.
    #[must_use]
    pub const fn supports_contacts(self) -> bool {
        !matches!(self, Self::GroupWiseProxy)
    }

    /// Whether "flagged" and "done" are stored only in the local database.
    ///
    /// The GroupWise proxy does not expose message categories, so the client
    /// keeps those two bits in SQLite and never pushes them to the server.
    #[must_use]
    pub const fn uses_local_only_flags(self) -> bool {
        matches!(self, Self::GroupWiseProxy)
    }

    /// Whether the account can create archive folders.
    #[must_use]
    pub const fn supports_archive_folders(self) -> bool {
        !matches!(self, Self::GroupWiseProxy)
    }

    /// Whether server side search is available.
    #[must_use]
    pub const fn supports_server_search(self) -> bool {
        true
    }
}

/// How the client authenticates against the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// Username + password (or application specific password) in the vault.
    Password,
    /// OAuth2 authorisation code flow with PKCE (RFC 8252).
    ///
    /// The interactive part lives in [`crate::oauth`]: the loopback listener,
    /// the token exchange and the refresh against the vault.
    OAuth2,
    /// Integrated Windows authentication (NTLM / Negotiate) for EWS.
    Ntlm,
}

impl AuthKind {
    /// Label used in the account wizard.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Password => "Password",
            Self::OAuth2 => "OAuth 2.0 (PKCE)",
            Self::Ntlm => "Windows authentication",
        }
    }
}

/// OAuth2 configuration for providers such as Gmail or Outlook.com.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// Token issuer / realm, used for the keyring entry label.
    pub issuer: String,
    /// Public client id registered with the provider.
    pub client_id: String,
    /// Space separated scope list.
    pub scopes: Vec<String>,
    /// Fully qualified authorisation endpoint.
    pub authorization_endpoint: String,
    /// Fully qualified token endpoint.
    pub token_endpoint: String,
    /// Loopback port for the redirect listener; `0` lets the OS choose one.
    #[serde(default)]
    pub redirect_port: u16,
}

impl OAuthConfig {
    /// PKCE (`S256`) is mandatory for public clients per RFC 8252 section 8.1,
    /// therefore the crate never offers a way to turn it off.
    #[must_use]
    pub const fn uses_pkce(&self) -> bool {
        true
    }

    /// Returns the scopes joined by a single space.
    #[must_use]
    pub fn scope_string(&self) -> String {
        self.scopes.join(" ")
    }

    /// Builds the loopback redirect URI for the given port.
    #[must_use]
    pub fn redirect_uri(&self, port: u16) -> String {
        format!("http://127.0.0.1:{port}/oauth2/callback")
    }

    /// `true` when the endpoints and the client id are filled in.
    ///
    /// The wizard uses this to decide whether the sign-in button can be
    /// offered; an OAuth2 account with an empty client id cannot be used and
    /// the browser flow would fail with a confusing provider error.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.client_id.trim().is_empty()
            && !self.authorization_endpoint.trim().is_empty()
            && !self.token_endpoint.trim().is_empty()
    }

    /// Validates the static parts of the configuration.
    pub fn validate(&self) -> Result<()> {
        if self.client_id.trim().is_empty() {
            return Err(Error::InvalidSettings {
                field: "oauth.client_id",
                message: "client id must not be empty".to_string(),
            });
        }
        if self.authorization_endpoint.trim().is_empty() || self.token_endpoint.trim().is_empty() {
            return Err(Error::InvalidSettings {
                field: "oauth.endpoint",
                message: "authorisation and token endpoint are required".to_string(),
            });
        }
        Ok(())
    }
}

/// GroupWise proxy specific settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupWiseProxyConfig {
    /// SOAP endpoint of the proxy (default port 7191).
    pub proxy: Endpoint,
    /// Post office the mailbox belongs to; used for redirect resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_office: Option<String>,
    /// Directory entry / user id on the proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Mailboxes the user may act for (proxied / shared mailboxes).
    #[serde(default)]
    pub shared_mailboxes: Vec<String>,
    /// Follow a redirect to a proxy host whose certificate common name differs
    /// from the configured host as long as it stays inside the same domain.
    #[serde(default = "default_true")]
    pub allow_certificate_cn_fallback: bool,
}

fn default_true() -> bool {
    true
}

impl GroupWiseProxyConfig {
    /// Creates a configuration for the given proxy host.
    #[must_use]
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            proxy: Endpoint::groupwise_proxy(host),
            post_office: None,
            user_id: None,
            shared_mailboxes: Vec::new(),
            allow_certificate_cn_fallback: true,
        }
    }
}

/// A configured mail account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Stable identifier referenced by folders, messages and rules.
    pub id: AccountId,
    /// Free-form display name shown in the folder tree.
    pub display_name: String,
    /// Primary e-mail address of the account.
    pub email: String,
    /// Protocol family.
    pub kind: AccountKind,
    /// Authentication mechanism.
    pub auth: AuthKind,
    /// Login name sent to the server (often, but not always, the address).
    pub username: String,
    /// Incoming server.
    pub incoming: Endpoint,
    /// Outgoing submission server.
    pub outgoing: Endpoint,
    /// Present for [`AccountKind::GroupWiseProxy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<GroupWiseProxyConfig>,
    /// Present for [`AuthKind::OAuth2`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    /// Whether the account is included in synchronisation.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Creation timestamp, used to keep the account order stable.
    #[serde(default = "crate::models::ids::utc_now")]
    pub created_at: DateTime<Utc>,
}

impl Account {
    /// Validates the account definition before it is written to disk.
    pub fn validate(&self) -> Result<()> {
        if self.email.trim().is_empty() {
            return Err(Error::InvalidSettings {
                field: "account.email",
                message: "e-mail address must not be empty".to_string(),
            });
        }
        if !self.email.contains('@') {
            return Err(Error::InvalidSettings {
                field: "account.email",
                message: format!("`{}` is not a valid e-mail address", self.email),
            });
        }
        if self.username.trim().is_empty() {
            return Err(Error::InvalidSettings {
                field: "account.username",
                message: "username must not be empty".to_string(),
            });
        }
        self.incoming.validate("account.incoming")?;
        self.outgoing.validate("account.outgoing")?;

        match self.kind {
            AccountKind::GroupWiseProxy => {
                let proxy = self.proxy.as_ref().ok_or_else(|| Error::InvalidSettings {
                    field: "account.proxy",
                    message: "GroupWise proxy accounts require a proxy configuration".to_string(),
                })?;
                proxy.proxy.validate("account.proxy.proxy")?;
            }
            AccountKind::Imap | AccountKind::Exchange => {
                if self.proxy.is_some() {
                    return Err(Error::InvalidSettings {
                        field: "account.proxy",
                        message: "proxy configuration is only valid for GroupWise proxy accounts"
                            .to_string(),
                    });
                }
            }
        }

        if self.auth == AuthKind::Ntlm && self.kind != AccountKind::Exchange {
            return Err(Error::InvalidSettings {
                field: "account.auth",
                message: "Windows authentication (NTLM) is only available for Exchange accounts"
                    .to_string(),
            });
        }

        if self.auth == AuthKind::OAuth2 {
            self.oauth
                .as_ref()
                .ok_or_else(|| Error::InvalidSettings {
                    field: "account.oauth",
                    message: "OAuth2 accounts require an OAuth configuration".to_string(),
                })?
                .validate()?;
        }

        Ok(())
    }

    /// The address that should be used in the `From` header.
    #[must_use]
    pub fn from_address(&self) -> &str {
        &self.email
    }

    /// Strips whitespace that users copy along with application passwords.
    ///
    /// Google displays application passwords as `abcd efgh ijkl mnop`; the
    /// specification requires those spaces to be removed automatically.
    #[must_use]
    pub fn sanitize_password(raw: &str) -> String {
        raw.chars().filter(|c| !c.is_whitespace()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imap_account() -> Account {
        Account {
            id: AccountId::new(),
            display_name: "Test".to_string(),
            email: "ada@example.org".to_string(),
            kind: AccountKind::Imap,
            auth: AuthKind::Password,
            username: "ada@example.org".to_string(),
            incoming: Endpoint::imaps("imap.example.org"),
            outgoing: Endpoint::smtps("smtp.example.org"),
            proxy: None,
            oauth: None,
            enabled: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn valid_imap_account_passes_validation() {
        imap_account().validate().unwrap();
    }

    #[test]
    fn endpoint_validation_catches_empty_host_and_zero_port() {
        let mut endpoint = Endpoint::imaps("");
        assert!(endpoint.validate("x").is_err());
        endpoint.host = "has space".to_string();
        assert!(endpoint.validate("x").is_err());
        endpoint.host = "imap.example.org".to_string();
        endpoint.port = 0;
        assert!(endpoint.validate("x").is_err());
    }

    #[test]
    fn email_must_contain_at_sign() {
        let mut account = imap_account();
        account.email = "not-an-address".to_string();
        assert!(account.validate().is_err());
    }

    #[test]
    fn ntlm_is_only_valid_for_exchange() {
        let mut account = imap_account();
        account.auth = AuthKind::Ntlm;
        assert!(account.validate().is_err());
        account.kind = AccountKind::Exchange;
        account.validate().unwrap();
    }

    #[test]
    fn proxy_config_only_allowed_for_proxy_accounts() {
        let mut account = imap_account();
        account.proxy = Some(GroupWiseProxyConfig::new("gw.example.org"));
        assert!(account.validate().is_err());
    }

    #[test]
    fn proxy_account_requires_proxy_config() {
        let mut account = imap_account();
        account.kind = AccountKind::GroupWiseProxy;
        assert!(account.validate().is_err());
        account.proxy = Some(GroupWiseProxyConfig::new("gw.example.org"));
        account.validate().unwrap();
    }

    #[test]
    fn oauth_account_requires_oauth_config() {
        let mut account = imap_account();
        account.auth = AuthKind::OAuth2;
        assert!(account.validate().is_err());
        account.oauth = Some(OAuthConfig {
            issuer: "example".to_string(),
            client_id: "client".to_string(),
            scopes: vec!["mail".to_string(), "offline_access".to_string()],
            authorization_endpoint: "https://example.org/auth".to_string(),
            token_endpoint: "https://example.org/token".to_string(),
            redirect_port: 0,
        });
        account.validate().unwrap();
        assert_eq!(
            account.oauth.as_ref().unwrap().scope_string(),
            "mail offline_access"
        );
    }

    #[test]
    fn application_passwords_lose_their_spaces() {
        assert_eq!(Account::sanitize_password("abcd efgh ijkl"), "abcdefghijkl");
        assert_eq!(Account::sanitize_password("  secret \n"), "secret");
    }

    #[test]
    fn account_kind_capabilities_match_specification() {
        assert!(AccountKind::Imap.supports_rules());
        assert!(AccountKind::Exchange.supports_calendar());
        assert!(!AccountKind::GroupWiseProxy.supports_rules());
        assert!(!AccountKind::GroupWiseProxy.supports_calendar());
        assert!(!AccountKind::GroupWiseProxy.supports_contacts());
        assert!(AccountKind::GroupWiseProxy.uses_local_only_flags());
        assert!(!AccountKind::GroupWiseProxy.supports_archive_folders());
    }

    #[test]
    fn account_round_trips_through_json() {
        let account = imap_account();
        let json = serde_json::to_string_pretty(&account).unwrap();
        let back: Account = serde_json::from_str(&json).unwrap();
        assert_eq!(account, back);
    }
}
