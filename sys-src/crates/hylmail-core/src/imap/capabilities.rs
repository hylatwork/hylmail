//! Server capabilities.
//!
//! Every IMAP extension HylMail depends on is optional in the protocol, so the
//! client has to *ask* rather than assume.  Answering "does this server support
//! `MOVE`?" is a single method call here, which keeps the protocol code free of
//! string comparisons — and, more importantly, keeps the fallback paths visible:
//! without `MOVE` a message has to be copied and then expunged, without
//! `UIDPLUS` an expunge cannot be limited to the messages we just copied.

use std::collections::BTreeSet;

/// The capability set a server advertised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Upper case capability names.  `AUTH=...` entries keep their value.
    atoms: BTreeSet<String>,
}

impl Capabilities {
    /// Builds a set from raw capability names.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let atoms = names
            .into_iter()
            .map(|name| name.as_ref().trim().to_ascii_uppercase())
            .filter(|name| !name.is_empty())
            .collect();
        Self { atoms }
    }

    /// Builds a set from the capability list of `async-imap`.
    #[must_use]
    pub fn from_async_imap(capabilities: &async_imap::types::Capabilities) -> Self {
        let mut atoms = BTreeSet::new();
        for capability in capabilities.iter() {
            match capability {
                async_imap::types::Capability::Imap4rev1 => {
                    atoms.insert("IMAP4REV1".to_string());
                }
                async_imap::types::Capability::Auth(mechanism) => {
                    atoms.insert(format!("AUTH={}", mechanism.to_ascii_uppercase()));
                }
                async_imap::types::Capability::Atom(atom) => {
                    atoms.insert(atom.to_ascii_uppercase());
                }
            }
        }
        Self { atoms }
    }

    /// `true` when the capability was advertised (case insensitively).
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.atoms.contains(&name.trim().to_ascii_uppercase())
    }

    /// `true` when the server supports the given SASL mechanism.
    #[must_use]
    pub fn has_auth(&self, mechanism: &str) -> bool {
        self.has(&format!("AUTH={mechanism}"))
    }

    /// All advertised SASL mechanisms, sorted.
    #[must_use]
    pub fn auth_mechanisms(&self) -> Vec<String> {
        self.atoms
            .iter()
            .filter_map(|atom| atom.strip_prefix("AUTH="))
            .map(ToString::to_string)
            .collect()
    }

    /// Every advertised capability, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.atoms.iter().map(String::as_str)
    }

    /// Number of advertised capabilities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.atoms.len()
    }

    /// `true` when the server advertised nothing (which is a protocol error).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.atoms.is_empty()
    }

    /// `true` when the server speaks the base protocol.
    #[must_use]
    pub fn is_imap4rev1(&self) -> bool {
        self.has("IMAP4rev1") || self.has("IMAP4rev2")
    }

    /// `STARTTLS` is available to upgrade a clear text connection.
    #[must_use]
    pub fn has_starttls(&self) -> bool {
        self.has("STARTTLS")
    }

    /// The server refuses the `LOGIN` command; `AUTHENTICATE` must be used.
    #[must_use]
    pub fn login_is_disabled(&self) -> bool {
        self.has("LOGINDISABLED")
    }

    /// `IDLE` keeps a mailbox open for push notifications.
    #[must_use]
    pub fn has_idle(&self) -> bool {
        self.has("IDLE")
    }

    /// `MOVE` moves a message without a copy/expunge round trip.
    #[must_use]
    pub fn has_move(&self) -> bool {
        self.has("MOVE")
    }

    /// `UIDPLUS` gives `UID EXPUNGE` and the `APPENDUID`/`COPYUID` responses.
    #[must_use]
    pub fn has_uidplus(&self) -> bool {
        self.has("UIDPLUS")
    }

    /// `SPECIAL-USE` / `LIST-EXTENDED` mark folders as `\Sent`, `\Trash`, ...
    #[must_use]
    pub fn has_special_use(&self) -> bool {
        self.has("SPECIAL-USE") || self.has("LIST-EXTENDED")
    }

    /// `NAMESPACE` tells the client which prefixes belong to the user.
    #[must_use]
    pub fn has_namespace(&self) -> bool {
        self.has("NAMESPACE")
    }

    /// `ID` exchanges client identification with the server.
    #[must_use]
    pub fn has_id(&self) -> bool {
        self.has("ID")
    }

    /// `CONDSTORE` reduces flag synchronisation to the changed messages.
    #[must_use]
    pub fn has_condstore(&self) -> bool {
        self.has("CONDSTORE")
    }

    /// `QRESYNC` reports flag changes since a known modification sequence.
    #[must_use]
    pub fn has_qresync(&self) -> bool {
        self.has("QRESYNC")
    }

    /// `UNSELECT` closes a mailbox without expunging it.
    #[must_use]
    pub fn has_unselect(&self) -> bool {
        self.has("UNSELECT")
    }

    /// `LITERAL+` allows a literal without waiting for a continuation.
    #[must_use]
    pub fn has_literal_plus(&self) -> bool {
        self.has("LITERAL+")
    }

    /// `UTF8=ACCEPT` lets the server return UTF-8 instead of modified UTF-7.
    #[must_use]
    pub fn has_utf8_accept(&self) -> bool {
        self.has("UTF8=ACCEPT")
    }

    /// `SASL-IR` allows an initial SASL response on the `AUTHENTICATE` line.
    #[must_use]
    pub fn has_sasl_ir(&self) -> bool {
        self.has("SASL-IR")
    }

    /// A one line summary for the log.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.has_idle() {
            parts.push("IDLE");
        }
        if self.has_move() {
            parts.push("MOVE");
        }
        if self.has_uidplus() {
            parts.push("UIDPLUS");
        }
        if self.has_special_use() {
            parts.push("SPECIAL-USE");
        }
        if self.has_condstore() {
            parts.push("CONDSTORE");
        }
        if self.has_qresync() {
            parts.push("QRESYNC");
        }
        if self.has_namespace() {
            parts.push("NAMESPACE");
        }
        if self.has_utf8_accept() {
            parts.push("UTF8=ACCEPT");
        }
        if parts.is_empty() {
            "no extensions beyond IMAP4rev1".to_string()
        } else {
            parts.join(", ")
        }
    }
}

impl std::fmt::Display for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.names().collect();
        write!(f, "{}", names.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dovecot() -> Capabilities {
        Capabilities::from_names([
            "IMAP4rev1",
            "STARTTLS",
            "LOGINDISABLED",
            "IDLE",
            "MOVE",
            "UIDPLUS",
            "LITERAL+",
            "NAMESPACE",
            "SPECIAL-USE",
            "AUTH=PLAIN",
            "AUTH=CRAM-MD5",
            "AUTH=XOAUTH2",
        ])
    }

    #[test]
    fn names_are_case_insensitive() {
        let capabilities = dovecot();
        assert!(capabilities.has("imap4rev1"));
        assert!(capabilities.has("IdLe"));
        assert!(capabilities.has("  MOVE  "));
        assert!(!capabilities.has("qresync"));
    }

    #[test]
    fn extension_helpers_answer_correctly() {
        let capabilities = dovecot();
        assert!(capabilities.is_imap4rev1());
        assert!(capabilities.has_idle());
        assert!(capabilities.has_move());
        assert!(capabilities.has_uidplus());
        assert!(capabilities.has_special_use());
        assert!(capabilities.has_namespace());
        assert!(capabilities.has_literal_plus());
        assert!(capabilities.has_starttls());
        assert!(capabilities.login_is_disabled());
        assert!(!capabilities.has_condstore());
        assert!(!capabilities.has_qresync());
        assert!(!capabilities.has_utf8_accept());
    }

    #[test]
    fn auth_mechanisms_are_extracted() {
        let capabilities = dovecot();
        assert!(capabilities.has_auth("PLAIN"));
        assert!(capabilities.has_auth("xoauth2"));
        assert!(!capabilities.has_auth("GSSAPI"));
        assert_eq!(
            capabilities.auth_mechanisms(),
            vec!["CRAM-MD5", "PLAIN", "XOAUTH2"]
        );
    }

    #[test]
    fn empty_and_blank_names_are_ignored() {
        let capabilities = Capabilities::from_names(["", "  ", "IDLE"]);
        assert_eq!(capabilities.len(), 1);
        assert!(capabilities.has_idle());
        assert!(Capabilities::default().is_empty());
    }

    #[test]
    fn the_summary_only_mentions_what_matters() {
        let summary = dovecot().describe();
        assert!(summary.contains("IDLE"));
        assert!(summary.contains("MOVE"));
        assert!(summary.contains("UIDPLUS"));
        assert!(summary.contains("SPECIAL-USE"));
        assert!(!summary.contains("AUTH="));

        let bare = Capabilities::from_names(["IMAP4rev1"]);
        assert_eq!(bare.describe(), "no extensions beyond IMAP4rev1");
    }

    #[test]
    fn display_is_space_separated_and_sorted() {
        let text = Capabilities::from_names(["UIDPLUS", "IDLE"]).to_string();
        assert_eq!(text, "IDLE UIDPLUS");
    }
}
