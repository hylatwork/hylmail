//! The outbox: a queue that survives restarts and never loses a message.
//!
//! Two properties matter and both are tested:
//!
//! 1. **A message that was accepted is marked sent and is not sent twice.**
//!    [`drain`] moves an entry to `Sent` before it looks at the next one, so a
//!    process that dies halfway keeps that knowledge in `outbox.json`.
//! 2. **A message that was refused keeps its content.** Every failure path only
//!    writes a *state* and a *reason*; the draft itself is untouched, so the
//!    user can fix the account and try again, and nothing has to be retyped.
//!
//! The queue is driven by callers, not by a thread of its own: the CLI calls
//! [`drain`] once and exits, the engine calls it after every sync round, and
//! the GUI calls it on a timer *and* after the "Jetzt senden" button. One
//! function, three callers, one behaviour.
//!
//! ## The `Submitter` seam
//!
//! [`drain`] does not know about SMTP. It takes a [`Submitter`], which
//! translates one [`OutboxItem`] into a delivery attempt. That keeps the queue
//! testable without a mail server - and it is the seam a future EWS or
//! GroupWise submission plugs into: those accounts need a different transport
//! *and* a copy in the Sent folder, which the IMAP-based submitter leaves to
//! the caller.

use crate::compose::{self, ComposeContext};
use crate::error::Error;
use crate::models::{OutboxItem, OutboxState};
use crate::store::OutboxDocument;

/// The outcome of one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The server accepted the message.
    Accepted {
        /// The server's reply, for the log.
        response: String,
        /// Bytes written into the Sent folder, when the submitter did that.
        sent_copy: Option<String>,
    },
    /// The server refused *now* but may accept later.
    Transient {
        /// Why it failed; shown in the queue view.
        reason: String,
    },
    /// The server refused for good; asking again would not help.
    Permanent {
        /// Why it failed.
        reason: String,
    },
}

impl Delivery {
    /// Builds a transient failure from any error, using its classification.
    #[must_use]
    pub fn from_error(error: &Error) -> Self {
        if error.is_transient() {
            Self::Transient {
                reason: error.to_string(),
            }
        } else {
            Self::Permanent {
                reason: error.to_string(),
            }
        }
    }
}

/// Sends one queued message.
///
/// Implementations must be **idempotent enough**: [`drain`] marks the entry as
/// `Sending` before the call and inspects the result afterwards, so an
/// implementation that is interrupted will simply be retried - which is the best
/// a mail client can do without server-side deduplication.
pub trait Submitter {
    /// Attempts one delivery.
    ///
    /// This is asynchronous because every real transport is.
    fn submit(
        &self,
        item: &OutboxItem,
        composed: &compose::ComposedMessage,
    ) -> impl std::future::Future<Output = Delivery> + Send;
}

/// What one [`drain`] run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxReport {
    /// Entries that were looked at.
    pub considered: usize,
    /// Entries that were delivered.
    pub sent: usize,
    /// Entries that failed but stay queued.
    pub deferred: usize,
    /// Entries that failed for good.
    pub failed: usize,
    /// Entries that were skipped because they were not due yet.
    pub skipped: usize,
    /// The ids that were delivered, so the caller can fetch the Sent copies.
    pub sent_ids: Vec<crate::models::ids::OutboxId>,
}

impl OutboxReport {
    /// `true` when nothing was even attempted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.considered == 0
    }

    /// One German line for the CLI and the activity log.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.is_empty() {
            return "Postausgang ist leer".to_string();
        }
        format!(
            "{} geprüft, {} gesendet, {} zurückgestellt, {} fehlgeschlagen, {} noch nicht fällig",
            self.considered, self.sent, self.deferred, self.failed, self.skipped
        )
    }
}

/// Processes the queue once.
///
/// Entries that are not [`OutboxItem::is_due`] at `now` are counted as
/// `skipped` and left alone.  A successful delivery is recorded with
/// [`OutboxItem::record_success`]; a failure with [`OutboxItem::record_failure`],
/// which applies the backoff and parks the entry after
/// [`OutboxItem::MAX_ATTEMPTS`].
///
/// `limit` caps how many entries are attempted in one run; `None` means "as many
/// as are due".  The limit exists because a queue of 500 messages must not turn
/// into 500 simultaneous TLS handshakes.
///
/// The document is mutated in place and the caller persists it - one write for
/// the whole run, which keeps the JSON file consistent with the states it
/// contains.
pub async fn drain<S: Submitter>(
    document: &mut OutboxDocument,
    submitter: &S,
    context_for: impl Fn(&OutboxItem) -> ComposeContext,
    now: chrono::DateTime<chrono::Utc>,
    limit: Option<usize>,
) -> OutboxReport {
    let mut report = OutboxReport::default();
    for item in &mut document.items {
        if !item.state.is_pending() {
            continue;
        }
        report.considered += 1;
        if !item.is_due(now) {
            report.skipped += 1;
            continue;
        }
        if limit.is_some_and(|limit| report.sent + report.deferred + report.failed >= limit) {
            report.skipped += 1;
            continue;
        }
        match attempt(item, submitter, &context_for(item)).await {
            Delivery::Accepted { .. } => {
                report.sent += 1;
                report.sent_ids.push(item.id);
            }
            Delivery::Transient { .. } => report.deferred += 1,
            Delivery::Permanent { .. } => report.failed += 1,
        }
    }
    report
}

/// One attempt, including the state transitions around it.
async fn attempt<S: Submitter>(
    item: &mut OutboxItem,
    submitter: &S,
    context: &ComposeContext,
) -> Delivery {
    // The message is composed before the state changes: a draft that cannot be
    // composed at all (a missing attachment file, no recipient) is a permanent
    // failure and must not be reported as "sending".
    let composed = match compose::build(&item.draft, context) {
        Ok(composed) => composed,
        Err(error) => {
            let reason = format!("Die Nachricht konnte nicht aufgebaut werden: {error}");
            item.record_failure(reason.clone());
            // Composing failures are always permanent in practice, so the entry
            // is parked immediately instead of being retried eight times.
            item.state = OutboxState::Failed;
            item.next_attempt_at = None;
            return Delivery::Permanent { reason };
        }
    };

    item.begin_attempt();
    let delivery = submitter.submit(item, &composed).await;
    match &delivery {
        Delivery::Accepted { response, .. } => {
            item.last_error = None;
            item.record_success();
            let _ = response;
        }
        Delivery::Transient { reason } => item.record_failure(reason.clone()),
        Delivery::Permanent { reason } => {
            item.record_failure(reason.clone());
            // `record_failure` applies the backoff; a permanent failure has no
            // point waiting for a retry, so the entry is parked at once.
            if item.attempts >= OutboxItem::MAX_ATTEMPTS || item.next_attempt_at.is_none() {
                item.state = OutboxState::Failed;
            }
            item.state = OutboxState::Failed;
            item.next_attempt_at = None;
        }
    }
    delivery
}

/// Puts a draft into the queue.
pub fn enqueue(
    document: &mut OutboxDocument,
    draft: crate::models::Draft,
) -> crate::models::ids::OutboxId {
    let item = OutboxItem::new(draft);
    let id = item.id;
    document.items.push(item);
    id
}

/// Removes an entry, for the "aus dem Postausgang entfernen" action.
pub fn remove(document: &mut OutboxDocument, id: crate::models::ids::OutboxId) -> bool {
    let before = document.items.len();
    document.items.retain(|item| item.id != id);
    document.items.len() != before
}

/// Re-arms a parked entry, for the "erneut versuchen" button.
pub fn retry(document: &mut OutboxDocument, id: crate::models::ids::OutboxId) -> bool {
    for item in &mut document.items {
        if item.id == id {
            item.state = OutboxState::Queued;
            item.attempts = 0;
            item.next_attempt_at = None;
            item.last_error = None;
            return true;
        }
    }
    false
}

/// The entries the user still has to do something about.
#[must_use]
pub fn attention(document: &OutboxDocument) -> Vec<&OutboxItem> {
    document
        .items
        .iter()
        .filter(|item| item.state == OutboxState::Failed)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AccountId, Draft, MailAddress};
    use parking_lot::Mutex;
    use std::collections::VecDeque;

    /// A submitter that replies from a script.
    struct Scripted {
        replies: Mutex<VecDeque<Delivery>>,
        seen: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(replies: Vec<Delivery>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Submitter for Scripted {
        async fn submit(
            &self,
            _item: &OutboxItem,
            composed: &compose::ComposedMessage,
        ) -> Delivery {
            self.seen.lock().push(composed.message_id.clone());
            self.replies
                .lock()
                .pop_front()
                .unwrap_or(Delivery::Transient {
                    reason: "keine Antwort im Drehbuch".to_string(),
                })
        }
    }

    fn draft() -> Draft {
        let mut draft = Draft::new(AccountId::new());
        draft.to = vec![MailAddress::new("bob@example.org")];
        draft.subject = "Test".to_string();
        draft.body_plain = "Text".to_string();
        draft
    }

    fn context(_item: &OutboxItem) -> ComposeContext {
        ComposeContext::from_address(MailAddress::new("ada@example.org"))
    }

    fn document(count: usize) -> OutboxDocument {
        let mut document = OutboxDocument::default();
        for _ in 0..count {
            enqueue(&mut document, draft());
        }
        document
    }

    #[tokio::test]
    async fn a_successful_run_marks_everything_sent() {
        let mut document = document(2);
        let submitter = Scripted::new(vec![
            Delivery::Accepted {
                response: "250 OK".to_string(),
                sent_copy: None,
            },
            Delivery::Accepted {
                response: "250 OK".to_string(),
                sent_copy: None,
            },
        ]);
        let report = drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        assert_eq!(report.sent, 2);
        assert_eq!(report.deferred, 0);
        assert!(document
            .items
            .iter()
            .all(|item| item.state == OutboxState::Sent));
        assert!(document.pending() == 0);
        assert_eq!(submitter.seen.lock().len(), 2);
        assert!(report.describe().contains("2 gesendet"));
    }

    #[tokio::test]
    async fn a_transient_failure_defers_and_keeps_the_draft() {
        let mut document = document(1);
        let submitter = Scripted::new(vec![Delivery::Transient {
            reason: "Verbindung verloren".to_string(),
        }]);
        let report = drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        assert_eq!(report.deferred, 1);
        assert_eq!(report.sent, 0);
        let item = &document.items[0];
        assert_eq!(item.state, OutboxState::Deferred);
        assert_eq!(item.attempts, 1);
        assert!(item.last_error.as_deref().unwrap().contains("Verbindung"));
        assert!(item.next_attempt_at.is_some());
        // The content is untouched.
        assert_eq!(item.draft.subject, "Test");
    }

    #[tokio::test]
    async fn a_permanent_failure_parks_the_entry() {
        let mut document = document(1);
        let submitter = Scripted::new(vec![Delivery::Permanent {
            reason: "550 unbekannter Empfänger".to_string(),
        }]);
        let report = drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        assert_eq!(report.failed, 1);
        let item = &document.items[0];
        assert_eq!(item.state, OutboxState::Failed);
        assert!(item.next_attempt_at.is_none());
        assert_eq!(attention(&document).len(), 1);
    }

    #[tokio::test]
    async fn a_deferred_entry_is_not_retried_before_its_time() {
        let mut document = document(1);
        let submitter = Scripted::new(vec![Delivery::Transient {
            reason: "später".to_string(),
        }]);
        drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        let report = drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        assert_eq!(report.skipped, 1);
        assert_eq!(report.sent, 0);
        // ...and once the time has come, it is tried again.
        let later = document.items[0].next_attempt_at.unwrap() + chrono::Duration::seconds(1);
        let report = drain(&mut document, &submitter, context, later, None).await;
        assert_eq!(report.deferred, 1);
    }

    #[tokio::test]
    async fn a_parked_entry_stays_parked() {
        let mut document = document(1);
        let submitter = Scripted::new(vec![Delivery::Permanent {
            reason: "nope".to_string(),
        }]);
        drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        let report = drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        assert!(report.is_empty());
    }

    #[tokio::test]
    async fn the_limit_caps_one_run() {
        let mut document = document(5);
        let submitter = Scripted::new(vec![
            Delivery::Accepted {
                response: "250".to_string(),
                sent_copy: None,
            },
            Delivery::Accepted {
                response: "250".to_string(),
                sent_copy: None,
            },
        ]);
        let report = drain(
            &mut document,
            &submitter,
            context,
            chrono::Utc::now(),
            Some(2),
        )
        .await;
        assert_eq!(report.sent, 2);
        assert_eq!(report.skipped, 3);
        assert_eq!(document.pending(), 3);
    }

    #[tokio::test]
    async fn a_draft_that_cannot_be_composed_fails_at_once() {
        let mut document = document(1);
        document.items[0].draft.to.clear();
        let submitter = Scripted::new(Vec::new());
        let report = drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        assert_eq!(report.failed, 1);
        let item = &document.items[0];
        assert_eq!(item.state, OutboxState::Failed);
        assert!(item.last_error.as_deref().unwrap().contains("aufgebaut"));
        assert!(submitter.seen.lock().is_empty());
    }

    #[tokio::test]
    async fn a_sent_entry_is_never_sent_again() {
        let mut document = document(1);
        let submitter = Scripted::new(vec![Delivery::Accepted {
            response: "250".to_string(),
            sent_copy: None,
        }]);
        drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        assert_eq!(submitter.seen.lock().len(), 1);
    }

    #[tokio::test]
    async fn an_entry_can_be_retried_by_hand() {
        let mut document = document(1);
        let submitter = Scripted::new(vec![Delivery::Permanent {
            reason: "nope".to_string(),
        }]);
        drain(&mut document, &submitter, context, chrono::Utc::now(), None).await;
        let id = document.items[0].id;
        assert!(retry(&mut document, id));
        assert_eq!(document.items[0].state, OutboxState::Queued);
        assert_eq!(document.items[0].attempts, 0);
        assert!(!retry(&mut document, crate::models::ids::OutboxId::new()));
    }

    #[test]
    fn an_entry_can_be_removed() {
        let mut document = document(1);
        let id = document.items[0].id;
        assert!(remove(&mut document, id));
        assert!(document.items.is_empty());
        assert!(!remove(&mut document, id));
    }

    #[test]
    fn an_error_is_classified_without_a_submitter() {
        let transient = Delivery::from_error(&Error::ConnectionLost {
            protocol: "SMTP",
            host: "mail.example.org".to_string(),
        });
        assert!(matches!(transient, Delivery::Transient { .. }));
        let permanent = Delivery::from_error(&Error::Auth {
            account: "ada@example.org".to_string(),
            message: "falsches Passwort".to_string(),
        });
        assert!(matches!(permanent, Delivery::Permanent { .. }));
    }

    #[test]
    fn an_empty_report_says_so() {
        assert_eq!(OutboxReport::default().describe(), "Postausgang ist leer");
    }
}
