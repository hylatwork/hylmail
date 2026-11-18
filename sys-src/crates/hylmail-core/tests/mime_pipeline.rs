//! The MIME pipeline, the sanitiser, the rule engine and the composer, driven
//! through the public API against `.eml` fixtures.
//!
//! The unit tests cover the individual functions; this suite wires the parts
//! together the way the engine and the UI will: parse a message, sanitise its
//! HTML, let the rules look at it, and build a reply.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use hylmail_core::compose::{build, forward_subject, reply_subject, ComposeContext, ReplyContext};
use hylmail_core::html::{sanitize, HtmlPolicy};
use hylmail_core::mime;
use hylmail_core::models::{
    utc_now, AccountId, Action, Condition, ConditionField, ConditionOp, Conjunction, Draft,
    ExternalImagePolicy, FolderId, MailAddress, MessageFlags, MessageId, Rule, RuleSet, Settings,
    Signature,
};
use hylmail_core::rules;

/// Reads a fixture from `tests/fixtures/eml`.
fn fixture(name: &str) -> Vec<u8> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("eml");
    path.push(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

fn policy() -> HtmlPolicy {
    HtmlPolicy::from_settings(&Settings::sanitized_default(), None)
}

/// Parses a fixture and turns its headers into a storable header row.
fn header_of(name: &str) -> hylmail_core::models::MessageHeader {
    let parsed = mime::parse(&fixture(name), MessageId::new(), &policy()).unwrap();
    let mut header = parsed.headers.to_header(AccountId::new(), FolderId::new());
    header.refresh_derived_fields();
    header
}

fn rule_matching(from_needle: &str, actions: Vec<Action>) -> Rule {
    Rule {
        id: hylmail_core::models::RuleId::new(),
        name: "Test".to_string(),
        enabled: true,
        priority: 10,
        conjunction: Conjunction::And,
        conditions: vec![Condition::new(
            ConditionField::From,
            ConditionOp::Contains,
            from_needle,
        )],
        actions,
        apply_on_arrival: true,
        created_at: utc_now(),
    }
}

#[test]
fn a_plain_message_keeps_its_decoded_subject_and_body() {
    let parsed = mime::parse(&fixture("plain.eml"), MessageId::new(), &policy()).unwrap();

    assert_eq!(parsed.headers.subject, "Grüße aus Meßemail");
    assert_eq!(parsed.headers.from[0].email, "ada@example.org");
    assert_eq!(parsed.headers.from[0].name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(
        parsed.headers.message_id.as_deref(),
        Some("plain-1@example.org")
    );

    let text = parsed.body.plain_text.as_deref().unwrap();
    assert!(text.contains("Viele Grüße"), "{text}");
    assert!(!parsed.has_attachments);
    assert!(!parsed.is_calendar_invite);
    assert!(parsed.snippet.contains("Hallo Bob"));
}

#[test]
fn a_newsletter_html_is_sanitised_and_its_tracker_is_dropped() {
    // The default policy blocks external images, so the tracking pixel is
    // removed and counted.
    let parsed = mime::parse(&fixture("newsletter.eml"), MessageId::new(), &policy()).unwrap();

    let html = parsed.body.html_sanitized.as_deref().unwrap();
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("tracker.example"), "{html}");
    assert!(html.contains("Wochenangebot"), "{html}");
    assert!(parsed.body.external_images_blocked);
    assert!(parsed.body.image_count >= 1);

    // The plain text alternative is still there.
    assert!(parsed
        .body
        .plain_text
        .as_deref()
        .unwrap()
        .contains("20% sparen"));
}

#[test]
fn an_allow_policy_keeps_external_images() {
    let mut settings = Settings::sanitized_default();
    settings.external_images = ExternalImagePolicy::Allow;
    let policy = HtmlPolicy::from_settings(&settings, None);

    let parsed = mime::parse(&fixture("newsletter.eml"), MessageId::new(), &policy).unwrap();
    let html = parsed.body.html_sanitized.as_deref().unwrap();
    assert!(!html.contains("<script"), "{html}");
    // External images are kept, and no tracker domain is configured here, so
    // the image survives the sanitiser untouched.
    assert!(html.contains("tracker.example"), "{html}");
    assert!(!parsed.body.external_images_blocked);
}

#[test]
fn a_configured_tracker_domain_is_dropped_even_when_images_are_allowed() {
    let mut settings = Settings::sanitized_default();
    settings.external_images = ExternalImagePolicy::Allow;
    settings.blocked_tracking_domains = vec!["tracker.example".to_string()];
    let policy = HtmlPolicy::from_settings(&settings, None);

    let parsed = mime::parse(&fixture("newsletter.eml"), MessageId::new(), &policy).unwrap();
    let html = parsed.body.html_sanitized.as_deref().unwrap();
    assert!(!html.contains("tracker.example"), "{html}");
    assert!(parsed.html.images.tracking >= 1);
}

#[test]
fn a_message_with_an_attachment_reports_it() {
    let parsed = mime::parse(&fixture("attachment.eml"), MessageId::new(), &policy()).unwrap();

    assert!(parsed.has_attachments);
    assert_eq!(parsed.body.attachments.len(), 1);
    let attachment = &parsed.body.attachments[0];
    assert_eq!(attachment.display_name(), "bericht.txt");
    assert_eq!(attachment.mime_type, "text/plain");
}

#[test]
fn a_calendar_invite_is_recognised_and_summarised() {
    let parsed = mime::parse(&fixture("invite.eml"), MessageId::new(), &policy()).unwrap();

    assert!(parsed.is_calendar_invite);
    let calendar = parsed.calendar.as_ref().unwrap();
    assert_eq!(calendar.title(), "Projektbesprechung");
    assert!(calendar.is_invitation());
    assert!(calendar
        .organizer_line()
        .as_deref()
        .unwrap()
        .contains("rooms@example.org"));
    assert!(parsed.has_attachments, "the calendar part is an attachment");
}

#[test]
fn a_message_without_a_content_type_is_still_parsed() {
    let parsed = mime::parse(&fixture("broken.eml"), MessageId::new(), &policy()).unwrap();
    let text = parsed.body.plain_text.as_deref().unwrap_or_default();
    assert!(text.contains("Koerper"), "{text}");
}

#[test]
fn a_malformed_message_never_leaks_its_body_in_the_error() {
    let secret = "GEHEIMER-INHALT-DER-NICHT-LECKEN-DARF";
    let raw = format!("\u{0}\u{1}{secret}\u{2}");
    // The parser may accept this, so only assert the contract that matters:
    // if it fails, the message text must not appear in the error.
    if let Err(error) = mime::parse(raw.as_bytes(), MessageId::new(), &policy()) {
        let rendered = format!("{error}");
        assert!(
            !rendered.contains(secret),
            "the error quoted the message: {rendered}"
        );
    }
}

#[test]
fn the_sanitiser_keeps_links_but_strips_event_handlers() {
    let html = r#"<p>Text</p><a href="https://example.org" onclick="evil()">Link</a>"#;
    let sanitised = sanitize(html, &policy());
    assert!(sanitised.html.contains("example.org"));
    assert!(!sanitised.html.contains("onclick"));
    assert!(sanitised.html.contains("noopener"));
    assert!(sanitised.html.contains("noreferrer"));
}

#[test]
fn the_rules_move_a_newsletter_to_its_folder() {
    let header = header_of("newsletter.eml");
    let mut set = RuleSet::new();
    set.add(rule_matching(
        "news@shop.example",
        vec![Action::MoveTo {
            folder_path: "INBOX/Newsletter".to_string(),
        }],
    ))
    .unwrap();

    let plan = rules::plan_on_arrival(&set, &header, None);
    assert!(!plan.matched_nothing);
    assert_eq!(plan.move_to.as_deref(), Some("INBOX/Newsletter"));
}

#[test]
fn a_rule_that_does_not_match_plans_nothing() {
    let header = header_of("plain.eml");
    let mut set = RuleSet::new();
    set.add(rule_matching(
        "news@shop.example",
        vec![Action::Flag, Action::MarkRead],
    ))
    .unwrap();

    let plan = rules::plan_on_arrival(&set, &header, None);
    assert!(plan.matched_nothing);
    assert!(!plan.flag);
    assert!(!plan.mark_read);
}

#[test]
fn a_reply_carries_the_quote_and_the_thread_headers() {
    let parsed = mime::parse(&fixture("plain.eml"), MessageId::new(), &policy()).unwrap();
    let header = parsed.headers.to_header(AccountId::new(), FolderId::new());

    let mut draft = Draft::new(AccountId::new());
    draft.to = vec![MailAddress::with_name("ada@example.org", "Ada Lovelace")];
    draft.subject = "Re: Grüße aus Meßemail".to_string();
    draft.body_plain = "Danke!\n".to_string();

    let reply = ReplyContext::from_message(&header, parsed.body.plain_text.as_deref(), None);
    let context =
        ComposeContext::from_address(MailAddress::new("bob@example.org")).in_reply_to(reply);
    let composed = build(&draft, &context).unwrap();

    let raw = String::from_utf8(composed.raw.clone()).unwrap();
    assert!(raw.contains("In-Reply-To: plain-1@example.org"), "{raw}");
    assert!(raw.contains("References: <plain-1@example.org>"), "{raw}");
    assert!(composed.recipients.contains(&"ada@example.org".to_string()));
}

#[test]
fn the_composer_writes_both_body_forms_and_a_signature() {
    let mut draft = Draft::new(AccountId::new());
    draft.to = vec![MailAddress::new("bob@example.org")];
    draft.subject = "Hallo".to_string();
    draft.body_html = "<p>Ein <b>Test</b>.</p>".to_string();

    let mut signature = Signature::new(AccountId::new(), "Standard");
    signature.plain = "Ada".to_string();
    signature.html = "<p>Ada</p>".to_string();

    let context =
        ComposeContext::from_address(MailAddress::new("ada@example.org")).with_signature(signature);
    let composed = build(&draft, &context).unwrap();

    let raw = String::from_utf8(composed.raw.clone()).unwrap();
    assert!(raw.contains("multipart/alternative"), "{raw}");
    assert!(raw.contains("text/plain"), "{raw}");
    assert!(raw.contains("text/html"), "{raw}");
    // The plain part was generated from the HTML and still gained the signature.
    assert!(raw.contains("Ada"), "{raw}");
}

#[test]
fn a_bcc_recipient_is_in_the_envelope_but_never_in_the_headers() {
    let mut draft = Draft::new(AccountId::new());
    draft.to = vec![MailAddress::new("bob@example.org")];
    draft.cc = vec![MailAddress::new("cara@example.org")];
    draft.bcc = vec![MailAddress::new("dora@example.org")];
    draft.subject = "Vertraulich".to_string();
    draft.body_html = "<p>Hallo</p>".to_string();

    let context = ComposeContext::from_address(MailAddress::new("ada@example.org"));
    let composed = build(&draft, &context).unwrap();

    let raw = String::from_utf8(composed.raw.clone()).unwrap();
    assert!(!raw.contains("dora@example.org"), "{raw}");
    assert!(composed
        .recipients
        .contains(&"dora@example.org".to_string()));
    assert!(composed
        .recipients
        .contains(&"cara@example.org".to_string()));
    assert!(composed.recipients.contains(&"bob@example.org".to_string()));
}

#[test]
fn subject_prefixes_never_stack() {
    assert_eq!(reply_subject("Frage"), "Re: Frage");
    assert_eq!(reply_subject("Re: Frage"), "Re: Frage");
    assert_eq!(forward_subject("Frage"), "WG: Frage");
    assert_eq!(forward_subject("WG: Frage"), "WG: Frage");
}

#[test]
fn a_flagged_message_is_recognised_through_the_header() {
    let mut header = header_of("plain.eml");
    header.flags.insert(MessageFlags::FLAGGED);
    assert!(header.is_flagged());
}

#[test]
fn a_message_with_a_calendar_part_reports_it_as_an_invitation() {
    let parsed = mime::parse(&fixture("invite.eml"), MessageId::new(), &policy()).unwrap();
    assert!(parsed.is_calendar_invite);
    assert!(parsed.body.is_invitation());
}
