//! Ready made server presets for well known providers.
//!
//! The account wizard fills host names, ports and TLS modes from a preset so
//! that a user only has to type their address and password.  A preset never
//! contains credentials; OAuth2 presets carry the provider endpoints and an
//! empty client id that the administrator has to fill in for the deployment.
//!
//! Presets are built by functions rather than `const` items because
//! [`Endpoint`] owns its host name and [`OAuthConfig`] owns its endpoints -
//! neither can be constructed in a constant expression.

use serde::Serialize;

use crate::models::account::{Account, AuthKind, Endpoint, OAuthConfig, TlsMode};

/// A reusable server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DomainPreset {
    /// Stable identifier, also used as the combo box key.
    pub id: &'static str,
    /// Label shown in the wizard.
    pub label: &'static str,
    /// Domains this preset matches; the longest matching suffix wins.
    pub domains: &'static [&'static str],
    /// Incoming server.
    pub incoming: Endpoint,
    /// Outgoing server.
    pub outgoing: Endpoint,
    /// Suggested authentication mechanism.
    pub auth: AuthKind,
    /// OAuth2 endpoints for [`AuthKind::OAuth2`].
    pub oauth: Option<OAuthConfig>,
    /// Short hint shown below the form.
    pub hint: &'static str,
}

impl DomainPreset {
    /// `true` when `domain` matches one of the preset's suffixes.
    #[must_use]
    pub fn matches_domain(&self, domain: &str) -> bool {
        let domain = domain.trim().to_ascii_lowercase();
        self.domains.iter().any(|candidate| {
            let candidate = candidate.to_ascii_lowercase();
            domain == candidate || domain.ends_with(&format!(".{candidate}"))
        })
    }

    /// Length of the longest matching suffix, used to resolve ties.
    #[must_use]
    pub fn match_length(&self, domain: &str) -> Option<usize> {
        let domain = domain.trim().to_ascii_lowercase();
        self.domains
            .iter()
            .filter(|candidate| {
                let candidate = candidate.to_ascii_lowercase();
                domain == candidate || domain.ends_with(&format!(".{candidate}"))
            })
            .map(|candidate| candidate.len())
            .max()
    }
}

/// Google Gmail / Google Workspace.
#[must_use]
pub fn gmail() -> DomainPreset {
    DomainPreset {
        id: "gmail",
        label: "Gmail / Google Workspace",
        domains: &["gmail.com", "googlemail.com", "google.com"],
        incoming: Endpoint::new("imap.gmail.com", 993, TlsMode::Implicit),
        outgoing: Endpoint::new("smtp.gmail.com", 465, TlsMode::Implicit),
        auth: AuthKind::OAuth2,
        oauth: Some(OAuthConfig {
            issuer: "https://accounts.google.com".to_string(),
            client_id: String::new(),
            scopes: vec![
                "https://mail.google.com/".to_string(),
                "openid".to_string(),
                "email".to_string(),
            ],
            authorization_endpoint: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            token_endpoint: "https://oauth2.googleapis.com/token".to_string(),
            redirect_port: 0,
        }),
        hint: "App passwords still work as a fallback; HylMail removes the spaces automatically.",
    }
}

/// Microsoft Outlook.com / Microsoft 365.
#[must_use]
pub fn outlook() -> DomainPreset {
    DomainPreset {
        id: "outlook",
        label: "Outlook.com / Microsoft 365",
        domains: &[
            "outlook.com",
            "outlook.de",
            "hotmail.com",
            "hotmail.de",
            "live.com",
            "office365.com",
        ],
        incoming: Endpoint::new("outlook.office365.com", 993, TlsMode::Implicit),
        outgoing: Endpoint::new("smtp.office365.com", 587, TlsMode::StartTls),
        auth: AuthKind::OAuth2,
        oauth: Some(OAuthConfig {
            issuer: "https://login.microsoftonline.com/common".to_string(),
            client_id: String::new(),
            scopes: vec![
                "https://outlook.office.com/IMAP.AccessAsUser.All".to_string(),
                "https://outlook.office.com/SMTP.Send".to_string(),
                "offline_access".to_string(),
            ],
            authorization_endpoint:
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".to_string(),
            token_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/token"
                .to_string(),
            redirect_port: 0,
        }),
        hint: "Use \"Windows authentication\" for an on-premises Exchange server instead.",
    }
}

/// A university that runs its own IMAP/SMTP mail service.
///
/// The host names follow the institutional `<service>.oth-aw.de` scheme.
/// Tenants that host their mail on a different cluster must adjust the hosts
/// in the wizard; the preset is a starting point, not a guarantee.
#[must_use]
pub fn oth_amberg_weiden() -> DomainPreset {
    DomainPreset {
        id: "oth-aw",
        label: "OTH Amberg-Weiden",
        domains: &["oth-aw.de"],
        incoming: Endpoint::new("imap.oth-aw.de", 993, TlsMode::Implicit),
        outgoing: Endpoint::new("smtp.oth-aw.de", 587, TlsMode::StartTls),
        auth: AuthKind::Password,
        oauth: None,
        hint: "Please confirm the server names with the university IT before saving.",
    }
}

/// A generic fallback for any other provider.
#[must_use]
pub fn generic() -> DomainPreset {
    DomainPreset {
        id: "generic",
        label: "Other provider (IMAP/SMTP)",
        domains: &[],
        incoming: Endpoint::new("imap.example.org", 993, TlsMode::Implicit),
        outgoing: Endpoint::new("smtp.example.org", 587, TlsMode::StartTls),
        auth: AuthKind::Password,
        oauth: None,
        hint: "Ask your administrator for the server names if you are unsure.",
    }
}

/// All presets, in the order they are offered in the wizard.
#[must_use]
pub fn all() -> Vec<DomainPreset> {
    vec![gmail(), outlook(), oth_amberg_weiden(), generic()]
}

/// Returns the preset matching an e-mail address, if any.
///
/// Matching is done on the domain suffix: `alice@student.oth-aw.de` matches the
/// `oth-aw.de` preset.  The longest matching suffix wins so that a more
/// specific preset always beats a generic one.
#[must_use]
pub fn preset_for_email(email: &str) -> Option<DomainPreset> {
    let domain = email.rsplit_once('@').map(|(_, domain)| domain)?;
    if domain.trim().is_empty() {
        return None;
    }
    let mut best: Option<(DomainPreset, usize)> = None;
    for preset in all() {
        if let Some(length) = preset.match_length(domain) {
            if best
                .as_ref()
                .is_none_or(|(_, best_length)| length > *best_length)
            {
                best = Some((preset, length));
            }
        }
    }
    best.map(|(preset, _)| preset)
}

/// Looks a preset up by its identifier.
#[must_use]
pub fn preset_by_id(id: &str) -> Option<DomainPreset> {
    all().into_iter().find(|preset| preset.id == id)
}

impl DomainPreset {
    /// Copies the server settings of the preset into an account.
    ///
    /// Display name, address, user name, id and the enabled flag are left
    /// alone: a preset describes the server, not the user.  An OAuth2 client id
    /// that the administrator already typed in survives, because it is a
    /// deployment value that the provider presets deliberately leave empty.
    pub fn apply_to(&self, account: &mut Account) {
        account.incoming = self.incoming.clone();
        account.outgoing = self.outgoing.clone();
        account.auth = self.auth;
        account.oauth = match (&self.oauth, &account.oauth) {
            (Some(preset), Some(existing)) => {
                let mut next = preset.clone();
                // The provider presets leave the client id empty on purpose:
                // it is registered per deployment, so whatever is there wins.
                next.client_id = existing.client_id.clone();
                if existing.redirect_port != 0 {
                    next.redirect_port = existing.redirect_port;
                }
                Some(next)
            }
            (Some(preset), None) => Some(preset.clone()),
            (None, _) => None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_addresses_are_detected() {
        assert_eq!(preset_for_email("ada@gmail.com").unwrap().id, "gmail");
        assert_eq!(preset_for_email("ADA@GoogleMail.com").unwrap().id, "gmail");
    }

    #[test]
    fn outlook_family_is_detected() {
        for address in [
            "a@outlook.com",
            "a@outlook.de",
            "a@hotmail.com",
            "a@hotmail.de",
            "a@live.com",
        ] {
            assert_eq!(
                preset_for_email(address).unwrap().id,
                "outlook",
                "{address}"
            );
        }
    }

    #[test]
    fn subdomains_match_the_parent_preset() {
        assert_eq!(
            preset_for_email("alice@student.oth-aw.de").unwrap().id,
            "oth-aw"
        );
    }

    #[test]
    fn lookalike_domains_do_not_match() {
        assert!(preset_for_email("a@notgmail.com").is_none());
        assert!(preset_for_email("a@gmail.com.evil.org").is_none());
    }

    #[test]
    fn unknown_domains_have_no_preset() {
        assert!(preset_for_email("a@example.org").is_none());
        assert!(preset_for_email("no-at-sign").is_none());
        assert!(preset_for_email("a@").is_none());
    }

    #[test]
    fn preset_ids_are_unique_and_resolvable() {
        let presets = all();
        let mut ids: Vec<&str> = presets.iter().map(|preset| preset.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
        for preset in &presets {
            assert_eq!(preset_by_id(preset.id).unwrap().id, preset.id);
        }
        assert!(preset_by_id("does-not-exist").is_none());
    }

    #[test]
    fn domain_matching_is_case_insensitive() {
        let preset = oth_amberg_weiden();
        assert!(preset.matches_domain("OTH-AW.DE"));
        assert!(preset.matches_domain(" Student.OTH-AW.DE "));
        assert!(!preset.matches_domain("example.org"));
        assert_eq!(preset.match_length("oth-aw.de"), Some("oth-aw.de".len()));
        assert_eq!(generic().match_length("example.org"), None);
    }

    #[test]
    fn oauth_presets_carry_both_endpoints() {
        for preset in [gmail(), outlook()] {
            let oauth = preset.oauth.as_ref().unwrap();
            assert!(oauth.authorization_endpoint.starts_with("https://"));
            assert!(oauth.token_endpoint.starts_with("https://"));
            assert!(oauth.uses_pkce());
            assert!(oauth.redirect_uri(1234).ends_with(":1234/oauth2/callback"));
        }
    }

    #[test]
    fn presets_serialise_for_the_ui_layer() {
        let json = serde_json::to_string(&gmail()).unwrap();
        assert!(json.contains("\"id\":\"gmail\""));
        assert!(json.contains("imap.gmail.com"));
    }

    #[test]
    fn generic_preset_is_always_last() {
        let presets = all();
        assert_eq!(presets.last().map(|preset| preset.id), Some("generic"));
    }

    use crate::models::{utc_now, AccountId};

    fn blank_account() -> Account {
        Account {
            id: AccountId::new(),
            display_name: "Ada".to_string(),
            email: "ada@gmail.com".to_string(),
            kind: crate::models::AccountKind::Imap,
            auth: AuthKind::Password,
            username: "ada@gmail.com".to_string(),
            incoming: crate::models::Endpoint::imaps("old.example.org"),
            outgoing: crate::models::Endpoint::smtps("old.example.org"),
            proxy: None,
            oauth: None,
            enabled: false,
            created_at: utc_now(),
        }
    }

    #[test]
    fn applying_a_preset_replaces_the_server_but_not_the_user() {
        let mut account = blank_account();
        gmail().apply_to(&mut account);
        assert_eq!(account.incoming.host, "imap.gmail.com");
        assert_eq!(account.outgoing.host, "smtp.gmail.com");
        assert_eq!(account.auth, AuthKind::OAuth2);
        assert_eq!(
            account.oauth.as_ref().unwrap().issuer,
            "https://accounts.google.com"
        );
        assert!(!account.oauth.as_ref().unwrap().is_configured());
        // The user's own data is untouched, including a disabled account.
        assert_eq!(account.display_name, "Ada");
        assert_eq!(account.email, "ada@gmail.com");
        assert!(!account.enabled);
    }

    #[test]
    fn applying_a_preset_keeps_a_registered_client_id() {
        let mut account = blank_account();
        account.oauth = Some(OAuthConfig {
            client_id: "registered-id".to_string(),
            redirect_port: 4040,
            ..OAuthConfig::default()
        });
        gmail().apply_to(&mut account);
        let oauth = account.oauth.as_ref().unwrap();
        assert_eq!(oauth.client_id, "registered-id");
        assert_eq!(oauth.redirect_port, 4040);
        assert_eq!(oauth.token_endpoint, "https://oauth2.googleapis.com/token");
        assert!(oauth.is_configured());
    }

    #[test]
    fn a_password_preset_clears_an_oauth_configuration() {
        let mut account = blank_account();
        gmail().apply_to(&mut account);
        assert!(account.oauth.is_some());
        // The generic preset is password based, so a leftover OAuth2 block
        // would make the account claim an auth kind it cannot use.
        generic().apply_to(&mut account);
        assert_eq!(account.auth, AuthKind::Password);
        assert_eq!(account.oauth, None);
    }
}
