//! Local drafts and the offline outgoing queue.
//!
//! Both documents are plain JSON files (`drafts.json`, `outbox.json`) so that
//! a crash while composing can never destroy a message and so that the queue
//! survives a restart.  The queue is drained by a retry worker in phase 2.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::models::ids::{utc_now, AccountId, DraftId, OutboxId, SignatureId};
use crate::models::message::MailAddress;

/// A file attached to a draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAttachment {
    /// Name presented to the recipient.
    pub filename: String,
    /// Declared media type.
    pub mime_type: String,
    /// Absolute path of the staged copy on disk.
    pub path: std::path::PathBuf,
    /// Size in bytes.
    #[serde(default)]
    pub size_bytes: u64,
    /// Whether the part is referenced from the HTML body via `cid:`.
    #[serde(default)]
    pub inline: bool,
}

impl DraftAttachment {
    /// Human readable size used by the composer.
    #[must_use]
    pub fn human_size(&self) -> String {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = self.size_bytes as f64;
        let mut unit = 0usize;
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{} {}", self.size_bytes, UNITS[unit])
        } else {
            format!("{value:.1} {}", UNITS[unit])
        }
    }
}

/// A message being composed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    /// Local identifier.
    pub id: DraftId,
    /// Account the draft will be sent from.
    pub account_id: AccountId,
    /// Recipients.
    #[serde(default)]
    pub to: Vec<MailAddress>,
    /// Carbon copy recipients.
    #[serde(default)]
    pub cc: Vec<MailAddress>,
    /// Blind carbon copy recipients.
    ///
    /// The Bcc header is removed before submission so that it can never leak
    /// into the delivered message.
    #[serde(default)]
    pub bcc: Vec<MailAddress>,
    /// Subject line.
    #[serde(default)]
    pub subject: String,
    /// Rich text body.
    #[serde(default)]
    pub body_html: String,
    /// Generated plain text alternative.
    #[serde(default)]
    pub body_plain: String,
    /// `In-Reply-To` of the message this draft answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// `References` chain of the message this draft answers.
    #[serde(default)]
    pub references: Vec<String>,
    /// Attachments staged on disk.
    #[serde(default)]
    pub attachments: Vec<DraftAttachment>,
    /// Signature chosen by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_id: Option<SignatureId>,
    /// UID of the server side draft, once it has been saved there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_uid: Option<u32>,
    /// Whether the server copy is newer than the local one.
    #[serde(default)]
    pub server_newer: bool,
    /// Last local modification.
    #[serde(default = "utc_now")]
    pub updated_at: DateTime<Utc>,
    /// Creation timestamp.
    #[serde(default = "utc_now")]
    pub created_at: DateTime<Utc>,
}

impl Draft {
    /// Creates an empty draft for an account.
    #[must_use]
    pub fn new(account_id: AccountId) -> Self {
        let now = Utc::now();
        Self {
            id: DraftId::new(),
            account_id,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            body_html: String::new(),
            body_plain: String::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            signature_id: None,
            server_uid: None,
            server_newer: false,
            updated_at: now,
            created_at: now,
        }
    }

    /// A draft is empty when it carries neither recipients, subject, body nor
    /// attachments - such drafts are never persisted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.to.is_empty()
            && self.cc.is_empty()
            && self.bcc.is_empty()
            && self.subject.trim().is_empty()
            && self.body_html.trim().is_empty()
            && self.body_plain.trim().is_empty()
            && self.attachments.is_empty()
    }

    /// All recipients of every kind, in `To`, `Cc`, `Bcc` order.
    #[must_use]
    pub fn all_recipients(&self) -> Vec<&MailAddress> {
        self.to
            .iter()
            .chain(self.cc.iter())
            .chain(self.bcc.iter())
            .collect()
    }

    /// Renders the recipient list the way the composer shows it.
    #[must_use]
    pub fn recipient_summary(&self) -> String {
        let names: Vec<String> = self
            .all_recipients()
            .iter()
            .map(|address| address.to_string())
            .collect();
        names.join(", ")
    }

    /// Returns `true` when the draft is a reply.
    #[must_use]
    pub fn is_reply(&self) -> bool {
        self.in_reply_to.is_some() || !self.references.is_empty()
    }

    /// Validates the draft before it is queued.
    pub fn validate(&self) -> Result<()> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err(Error::InvalidSettings {
                field: "draft.recipients",
                message: "a message needs at least one recipient".to_string(),
            });
        }
        for address in self.all_recipients() {
            if !address.email.contains('@') {
                return Err(Error::InvalidSettings {
                    field: "draft.recipients",
                    message: format!("`{}` is not a valid e-mail address", address.email),
                });
            }
        }
        for attachment in &self.attachments {
            if attachment.filename.trim().is_empty() {
                return Err(Error::InvalidSettings {
                    field: "draft.attachments",
                    message: "an attachment needs a file name".to_string(),
                });
            }
        }
        Ok(())
    }

    /// Marks the draft as modified now.
    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    /// Height of the message in the list, used for stable ordering.
    #[must_use]
    pub fn age(&self) -> chrono::Duration {
        Utc::now() - self.updated_at
    }
}

/// Lifecycle state of a queued message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    /// Waiting to be sent.
    #[default]
    Queued,
    /// Currently being transmitted.
    Sending,
    /// Successfully handed over to the submission server.
    Sent,
    /// Transmission failed permanently.
    Failed,
    /// Transmission failed temporarily and will be retried.
    Deferred,
}

impl OutboxState {
    /// `true` while the entry still needs work.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Queued | Self::Sending | Self::Deferred)
    }

    /// Label for the outgoing queue view.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Queued => "Wartend",
            Self::Sending => "Wird gesendet",
            Self::Sent => "Gesendet",
            Self::Failed => "Fehlgeschlagen",
            Self::Deferred => "Zurückgestellt",
        }
    }
}

/// One entry of the outgoing queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxItem {
    /// Local identifier.
    pub id: OutboxId,
    /// The message to send.
    pub draft: Draft,
    /// Current state.
    #[serde(default)]
    pub state: OutboxState,
    /// Number of delivery attempts so far.
    #[serde(default)]
    pub attempts: u32,
    /// Earliest time the next attempt may start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Last error message, shown in the queue view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// When the message was queued.
    #[serde(default = "utc_now")]
    pub created_at: DateTime<Utc>,
    /// When the message left the queue successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<DateTime<Utc>>,
}

impl OutboxItem {
    /// Queues a draft.
    #[must_use]
    pub fn new(draft: Draft) -> Self {
        Self {
            id: OutboxId::new(),
            draft,
            state: OutboxState::Queued,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
            created_at: Utc::now(),
            sent_at: None,
        }
    }

    /// Records a failed attempt with exponential backoff.
    ///
    /// The delay doubles per attempt and is capped at one hour; after
    /// [`OutboxItem::MAX_ATTEMPTS`] attempts the entry is marked as failed so
    /// that the user has to look at it.
    pub fn record_failure(&mut self, error: impl Into<String>) {
        self.attempts = self.attempts.saturating_add(1);
        self.last_error = Some(error.into());
        if self.attempts >= Self::MAX_ATTEMPTS {
            self.state = OutboxState::Failed;
            self.next_attempt_at = None;
            return;
        }
        self.state = OutboxState::Deferred;
        let delay_seconds = 30u64.saturating_mul(1 << self.attempts.min(10));
        let delay_seconds = delay_seconds.min(3_600);
        self.next_attempt_at = Some(Utc::now() + chrono::Duration::seconds(delay_seconds as i64));
    }

    /// Marks the message as sent.
    pub fn record_success(&mut self) {
        self.state = OutboxState::Sent;
        self.sent_at = Some(Utc::now());
        self.next_attempt_at = None;
        self.last_error = None;
    }

    /// Maximum number of delivery attempts before the entry is parked.
    pub const MAX_ATTEMPTS: u32 = 8;

    /// `true` when the entry may be attempted at `now`.
    #[must_use]
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        match self.state {
            OutboxState::Queued => true,
            OutboxState::Deferred => self.next_attempt_at.is_none_or(|at| at <= now),
            _ => false,
        }
    }

    /// Prepares the entry for an attempt.
    pub fn begin_attempt(&mut self) {
        self.state = OutboxState::Sending;
    }

    /// Returns the entry to the queue, for example after a restart.
    pub fn requeue_after_restart(&mut self) {
        if self.state == OutboxState::Sending {
            self.state = OutboxState::Deferred;
            self.next_attempt_at = Some(Utc::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_draft_is_detected() {
        let draft = Draft::new(AccountId::new());
        assert!(draft.is_empty());
        let mut draft = draft;
        draft.subject = "Hi".to_string();
        assert!(!draft.is_empty());
    }

    #[test]
    fn whitespace_only_fields_do_not_count_as_content() {
        let mut draft = Draft::new(AccountId::new());
        draft.body_html = "   \n".to_string();
        draft.subject = "\t".to_string();
        assert!(draft.is_empty());
    }

    #[test]
    fn draft_validation_requires_a_recipient() {
        let draft = Draft::new(AccountId::new());
        assert!(draft.validate().is_err());
    }

    #[test]
    fn draft_validation_checks_addresses_and_attachments() {
        let mut draft = Draft::new(AccountId::new());
        draft.to.push(MailAddress::new("not-an-address"));
        assert!(draft.validate().is_err());

        draft.to = vec![MailAddress::new("ada@example.org")];
        draft.validate().unwrap();

        draft.attachments.push(DraftAttachment {
            filename: "  ".to_string(),
            mime_type: "application/pdf".to_string(),
            path: std::path::PathBuf::from("/tmp/x.pdf"),
            size_bytes: 10,
            inline: false,
        });
        assert!(draft.validate().is_err());
    }

    #[test]
    fn cc_only_drafts_are_valid() {
        let mut draft = Draft::new(AccountId::new());
        draft.cc.push(MailAddress::new("ada@example.org"));
        draft.validate().unwrap();
    }

    #[test]
    fn recipient_summary_lists_every_kind() {
        let mut draft = Draft::new(AccountId::new());
        draft.to.push(MailAddress::new("a@example.org"));
        draft.cc.push(MailAddress::new("b@example.org"));
        draft.bcc.push(MailAddress::new("c@example.org"));
        assert_eq!(draft.all_recipients().len(), 3);
        assert!(draft.recipient_summary().contains("c@example.org"));
    }

    #[test]
    fn reply_detection_uses_thread_headers() {
        let mut draft = Draft::new(AccountId::new());
        assert!(!draft.is_reply());
        draft.in_reply_to = Some("<x@y>".to_string());
        assert!(draft.is_reply());

        let mut draft = Draft::new(AccountId::new());
        draft.references.push("<x@y>".to_string());
        assert!(draft.is_reply());
    }

    #[test]
    fn human_readable_attachment_sizes() {
        let mut attachment = DraftAttachment {
            filename: "a".to_string(),
            mime_type: "text/plain".to_string(),
            path: std::path::PathBuf::from("/tmp/a"),
            size_bytes: 512,
            inline: false,
        };
        assert_eq!(attachment.human_size(), "512 B");
        attachment.size_bytes = 2048;
        assert_eq!(attachment.human_size(), "2.0 KiB");
        attachment.size_bytes = 5 * 1024 * 1024;
        assert_eq!(attachment.human_size(), "5.0 MiB");
    }

    #[test]
    fn failure_backoff_is_capped_and_eventually_parks() {
        let mut item = OutboxItem::new(Draft::new(AccountId::new()));
        assert!(item.is_due(Utc::now()));
        item.begin_attempt();
        assert!(!item.is_due(Utc::now()));
        item.record_failure("connection refused");
        assert_eq!(item.state, OutboxState::Deferred);
        assert!(item.next_attempt_at.is_some());

        for _ in 0..OutboxItem::MAX_ATTEMPTS {
            item.record_failure("still down");
        }
        assert_eq!(item.state, OutboxState::Failed);
        assert!(item.next_attempt_at.is_none());
        assert!(!item.is_due(Utc::now()));
    }

    #[test]
    fn success_clears_the_error_and_timestamp() {
        let mut item = OutboxItem::new(Draft::new(AccountId::new()));
        item.record_failure("nope");
        item.record_success();
        assert_eq!(item.state, OutboxState::Sent);
        assert!(item.sent_at.is_some());
        assert!(item.last_error.is_none());
        assert!(!item.is_due(Utc::now()));
    }

    #[test]
    fn interrupted_sends_are_requeued() {
        let mut item = OutboxItem::new(Draft::new(AccountId::new()));
        item.begin_attempt();
        item.requeue_after_restart();
        assert_eq!(item.state, OutboxState::Deferred);
        assert!(item.is_due(Utc::now()));
    }

    #[test]
    fn outbox_state_labels_are_complete() {
        for state in [
            OutboxState::Queued,
            OutboxState::Sending,
            OutboxState::Sent,
            OutboxState::Failed,
            OutboxState::Deferred,
        ] {
            assert!(!state.label().is_empty());
        }
        assert!(OutboxState::Queued.is_pending());
        assert!(!OutboxState::Sent.is_pending());
    }

    #[test]
    fn draft_and_outbox_round_trip_through_json() {
        let mut draft = Draft::new(AccountId::new());
        draft.to.push(MailAddress::with_name("a@b.c", "A"));
        draft.subject = "Test".to_string();
        let item = OutboxItem::new(draft.clone());

        let json = serde_json::to_string(&draft).unwrap();
        assert_eq!(serde_json::from_str::<Draft>(&json).unwrap(), draft);
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(serde_json::from_str::<OutboxItem>(&json).unwrap(), item);
    }
}
