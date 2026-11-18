//! Address book contacts.
//!
//! Contacts are cached in SQLite.  The CardDAV client (phase 2) keeps the raw
//! vCard text in [`Contact::vcard_raw`] so that a round trip through HylMail
//! never loses properties the client does not understand - a hard requirement
//! from the specification ("verlustfreier vCard-Roundtrip").

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::ids::AccountId;
use crate::models::ids::ContactId;
use crate::models::message::MailAddress;

/// Where a contact came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactSource {
    /// Synchronised from a CardDAV collection.
    CardDav,
    /// Collected automatically from sent or received messages.
    #[default]
    Collected,
    /// Created by the user inside HylMail.
    Manual,
}

/// A single address book entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// Local identifier.
    pub id: ContactId,
    /// Account the contact was synchronised from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<AccountId>,
    /// Formatted name (`FN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Structured name components, when the vCard carried them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    /// Given name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    /// All e-mail addresses with their `TYPE` parameters.
    #[serde(default)]
    pub emails: Vec<ContactEmail>,
    /// Organisation (`ORG`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Unparsed vCard text, preserved verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcard_raw: Option<String>,
    /// HTTP `ETag` of the CardDAV resource, used for conditional requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Absolute CardDAV URL of the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    /// Origin of the entry.
    #[serde(default)]
    pub source: ContactSource,
    /// Last time the contact appeared in a message or was synchronised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
}

impl Contact {
    /// Creates a collected contact for the given address.
    #[must_use]
    pub fn collected(address: &MailAddress) -> Self {
        Self {
            id: ContactId::new(),
            account_id: None,
            display_name: address.name.clone(),
            family_name: None,
            given_name: None,
            emails: vec![ContactEmail {
                address: address.email.clone(),
                label: None,
                preferred: true,
            }],
            organization: None,
            vcard_raw: None,
            etag: None,
            href: None,
            source: ContactSource::Collected,
            last_seen: Some(Utc::now()),
        }
    }

    /// The primary e-mail address, if the contact has one.
    #[must_use]
    pub fn primary_email(&self) -> Option<&str> {
        self.emails
            .iter()
            .find(|email| email.preferred)
            .or_else(|| self.emails.first())
            .map(|email| email.address.as_str())
    }

    /// The label shown in the address book and in autocomplete.
    #[must_use]
    pub fn label(&self) -> String {
        match self.display_name.as_deref() {
            Some(name) if !name.trim().is_empty() => name.to_string(),
            _ => self
                .primary_email()
                .map(ToString::to_string)
                .unwrap_or_else(|| "(ohne Namen)".to_string()),
        }
    }

    /// Converts the contact into a [`MailAddress`] for the composer.
    #[must_use]
    pub fn to_mail_address(&self) -> Option<MailAddress> {
        let email = self.primary_email()?.to_string();
        Some(MailAddress {
            name: self.display_name.clone(),
            email,
        })
    }

    /// `true` when the given address belongs to this contact.
    #[must_use]
    pub fn matches_email(&self, address: &str) -> bool {
        let needle = address.trim().to_ascii_lowercase();
        self.emails
            .iter()
            .any(|email| email.address.eq_ignore_ascii_case(&needle))
    }

    /// `true` when the contact can be written back to CardDAV.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.source == ContactSource::CardDav && self.href.is_some()
    }
}

/// One e-mail address of a contact together with its `TYPE` label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactEmail {
    /// The bare address.
    pub address: String,
    /// `TYPE` parameter, e.g. `work` or `home`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `PREF` parameter.
    #[serde(default)]
    pub preferred: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collected_contact_uses_the_address() {
        let address = MailAddress::with_name("ada@example.org", "Ada");
        let contact = Contact::collected(&address);
        assert_eq!(contact.primary_email(), Some("ada@example.org"));
        assert_eq!(contact.label(), "Ada");
        assert_eq!(contact.source, ContactSource::Collected);
        assert!(!contact.is_remote());
        assert!(contact.matches_email("ADA@example.org"));
        assert!(!contact.matches_email("someone@else.org"));
    }

    #[test]
    fn contact_without_name_falls_back_to_the_address() {
        let mut contact = Contact::collected(&MailAddress::new("ada@example.org"));
        contact.display_name = None;
        assert_eq!(contact.label(), "ada@example.org");
    }

    #[test]
    fn contact_without_email_has_no_primary_address() {
        let mut contact = Contact::collected(&MailAddress::new("ada@example.org"));
        contact.emails.clear();
        assert!(contact.primary_email().is_none());
        assert!(contact.to_mail_address().is_none());
        assert_eq!(contact.label(), "(ohne Namen)");
    }

    #[test]
    fn preferred_email_wins_over_the_first_one() {
        let mut contact = Contact::collected(&MailAddress::new("first@example.org"));
        contact.emails[0].preferred = false;
        contact.emails.push(ContactEmail {
            address: "second@example.org".to_string(),
            label: Some("work".to_string()),
            preferred: true,
        });
        assert_eq!(contact.primary_email(), Some("second@example.org"));
    }

    #[test]
    fn remote_contacts_need_a_href() {
        let mut contact = Contact::collected(&MailAddress::new("a@b.c"));
        contact.source = ContactSource::CardDav;
        assert!(!contact.is_remote());
        contact.href = Some("https://dav.example.org/contacts/1.vcf".to_string());
        assert!(contact.is_remote());
    }

    #[test]
    fn contact_round_trips_through_json() {
        let mut contact = Contact::collected(&MailAddress::new("a@b.c"));
        contact.vcard_raw = Some("BEGIN:VCARD\r\nX-CUSTOM:1\r\nEND:VCARD\r\n".to_string());
        let json = serde_json::to_string(&contact).unwrap();
        let back: Contact = serde_json::from_str(&json).unwrap();
        assert_eq!(contact, back);
    }
}
