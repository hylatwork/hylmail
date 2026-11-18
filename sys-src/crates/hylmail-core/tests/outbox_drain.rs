//! The outbox queue driven through its public API with a scripted submitter.
//!
//! `drain` is the single entry point the CLI, the engine and the GUI all call,
//! so its state machine is worth an end-to-end test: a delivery promotes the
//! entry to `Sent`, a transient failure defers it and sets a retry time, and a
//! permanent failure parks it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use hylmail_core::compose::ComposeContext;
use hylmail_core::models::{utc_now, AccountId, Draft, MailAddress, OutboxState};
use hylmail_core::outbox::{self, Delivery, Submitter};
use hylmail_core::store::OutboxDocument;

/// A submitter that answers according to a scripted list and counts its calls.
struct ScriptedSubmitter {
    answers: parking_lot::Mutex<Vec<Delivery>>,
    calls: AtomicUsize,
}

impl ScriptedSubmitter {
    fn new(answers: Vec<Delivery>) -> Arc<Self> {
        Arc::new(Self {
            answers: parking_lot::Mutex::new(answers),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Submitter for ScriptedSubmitter {
    async fn submit(
        &self,
        _item: &hylmail_core::models::OutboxItem,
        _composed: &hylmail_core::compose::ComposedMessage,
    ) -> Delivery {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut answers = self.answers.lock();
        if answers.is_empty() {
            return Delivery::Permanent {
                reason: "keine weitere Antwort".to_string(),
            };
        }
        answers.remove(0)
    }
}

fn draft(subject: &str) -> Draft {
    let mut draft = Draft::new(AccountId::new());
    draft.to = vec![MailAddress::new("bob@example.org")];
    draft.subject = subject.to_string();
    draft.body_plain = "Hallo".to_string();
    draft
}

fn context_for(_item: &hylmail_core::models::OutboxItem) -> ComposeContext {
    ComposeContext::from_address(MailAddress::new("ada@example.org"))
}

#[tokio::test]
async fn a_successful_delivery_marks_the_entry_as_sent() {
    let submitter = ScriptedSubmitter::new(vec![Delivery::Accepted {
        response: "250 OK".to_string(),
        sent_copy: None,
    }]);
    let mut document = OutboxDocument::default();
    let id = outbox::enqueue(&mut document, draft("Hallo"));

    let report = outbox::drain(
        &mut document,
        submitter.as_ref(),
        context_for,
        utc_now(),
        None,
    )
    .await;

    assert_eq!(report.sent, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(submitter.calls(), 1);
    let item = document.items.iter().find(|item| item.id == id).unwrap();
    assert_eq!(item.state, OutboxState::Sent);
}

#[tokio::test]
async fn a_transient_failure_defers_the_entry_with_a_retry_time() {
    let submitter = ScriptedSubmitter::new(vec![Delivery::Transient {
        reason: "Server nicht erreichbar".to_string(),
    }]);
    let mut document = OutboxDocument::default();
    let id = outbox::enqueue(&mut document, draft("Hallo"));

    let report = outbox::drain(
        &mut document,
        submitter.as_ref(),
        context_for,
        utc_now(),
        None,
    )
    .await;

    assert_eq!(report.deferred, 1);
    assert_eq!(report.sent, 0);
    let item = document.items.iter().find(|item| item.id == id).unwrap();
    assert_eq!(item.state, OutboxState::Deferred);
    assert!(item.next_attempt_at.is_some(), "a retry time is set");
    assert_eq!(item.attempts, 1);
    assert!(item.last_error.is_some());
}

#[tokio::test]
async fn a_permanent_failure_parks_the_entry() {
    let submitter = ScriptedSubmitter::new(vec![Delivery::Permanent {
        reason: "550 unbekannter Empfänger".to_string(),
    }]);
    let mut document = OutboxDocument::default();
    let id = outbox::enqueue(&mut document, draft("Hallo"));

    let report = outbox::drain(
        &mut document,
        submitter.as_ref(),
        context_for,
        utc_now(),
        None,
    )
    .await;

    assert_eq!(report.failed, 1);
    let item = document.items.iter().find(|item| item.id == id).unwrap();
    assert_eq!(item.state, OutboxState::Failed);
}

#[tokio::test]
async fn a_parked_entry_stays_parked_until_retried() {
    let submitter = ScriptedSubmitter::new(vec![Delivery::Permanent {
        reason: "550".to_string(),
    }]);
    let mut document = OutboxDocument::default();
    let id = outbox::enqueue(&mut document, draft("Hallo"));

    outbox::drain(
        &mut document,
        submitter.as_ref(),
        context_for,
        utc_now(),
        None,
    )
    .await;
    // A second run does not even look at the parked entry.
    let report = outbox::drain(
        &mut document,
        submitter.as_ref(),
        context_for,
        utc_now(),
        None,
    )
    .await;
    assert_eq!(report.considered, 0);

    assert!(outbox::retry(&mut document, id));
    let item = document.items.iter().find(|item| item.id == id).unwrap();
    assert_eq!(item.state, OutboxState::Queued);
    assert_eq!(item.attempts, 0);
    assert!(outbox::attention(&document).is_empty());
}

#[tokio::test]
async fn the_limit_bounds_how_many_are_attempted() {
    let submitter = ScriptedSubmitter::new(vec![
        Delivery::Accepted {
            response: "250 OK".to_string(),
            sent_copy: None,
        },
        Delivery::Accepted {
            response: "250 OK".to_string(),
            sent_copy: None,
        },
        Delivery::Accepted {
            response: "250 OK".to_string(),
            sent_copy: None,
        },
    ]);
    let mut document = OutboxDocument::default();
    outbox::enqueue(&mut document, draft("Eins"));
    outbox::enqueue(&mut document, draft("Zwei"));
    outbox::enqueue(&mut document, draft("Drei"));

    let report = outbox::drain(
        &mut document,
        submitter.as_ref(),
        context_for,
        utc_now(),
        Some(2),
    )
    .await;

    assert_eq!(report.sent, 2);
    assert_eq!(report.skipped, 1);
    assert_eq!(submitter.calls(), 2);
}

#[tokio::test]
async fn a_not_yet_due_entry_is_skipped_without_calling_the_submitter() {
    let submitter = ScriptedSubmitter::new(vec![Delivery::Accepted {
        response: "250 OK".to_string(),
        sent_copy: None,
    }]);
    let mut document = OutboxDocument::default();
    let id = outbox::enqueue(&mut document, draft("Hallo"));
    // A `Deferred` entry whose retry time is in the future is not due.
    for item in &mut document.items {
        if item.id == id {
            item.state = OutboxState::Deferred;
            item.next_attempt_at = Some(utc_now() + chrono::Duration::hours(1));
        }
    }

    let report = outbox::drain(
        &mut document,
        submitter.as_ref(),
        context_for,
        utc_now(),
        None,
    )
    .await;

    assert_eq!(report.skipped, 1);
    assert_eq!(report.sent, 0);
    assert_eq!(submitter.calls(), 0);
}
