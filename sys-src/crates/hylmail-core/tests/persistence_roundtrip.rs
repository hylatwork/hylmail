// Integration tests are allowed to panic: every failing assertion is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! End-to-end persistence test.
//!
//! Boots a complete HylMail core in a temporary portable directory, writes one
//! of every kind of record, closes it, and boots a second instance from the very
//! same files.  This is the test that would catch a broken migration, a missing
//! `WAL` checkpoint or a JSON document that cannot be re-read.

use std::sync::Arc;

use hylmail_core::db::{content, conversations, folders, messages};
use hylmail_core::models::{
    Account, AccountId, AccountKind, Attachment, AuthKind, Contact, ContactSource, Draft, Endpoint,
    Folder, FolderKind, LocalFlags, MailAddress, MessageBody, MessageFlags, MessageHeader,
    MessageQuery, OutboxItem, Rule, RuleSet, Settings, Signature,
};
use hylmail_core::paths::{MapEnv, Os, PORTABLE_ENV};
use hylmail_core::secrets::{
    secret, CredentialKey, CredentialKind, InMemorySecretStore, SecretStore,
};
use hylmail_core::{AppPaths, Core, SCHEMA_VERSION};

fn paths_in(dir: &std::path::Path) -> AppPaths {
    let env = MapEnv::new().with(PORTABLE_ENV, dir.to_string_lossy().to_string());
    AppPaths::resolve_for(Os::Linux, &env).unwrap()
}

fn bootstrap(dir: &std::path::Path, secrets: Arc<InMemorySecretStore>) -> Core {
    Core::bootstrap(paths_in(dir), secrets).unwrap()
}

#[test]
fn everything_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(InMemorySecretStore::new());
    let account_id = AccountId::new();
    let folder_id;
    let message_id;

    // ------------------------------------------------------------------
    // First run: write one of everything.
    // ------------------------------------------------------------------
    {
        let mut core = bootstrap(dir.path(), Arc::clone(&secrets));

        // 1. settings.json -------------------------------------------------
        let mut settings = core.settings().clone();
        settings.page_size = 250;
        settings.show_all_inboxes_on_start = true;
        settings.done_mode = hylmail_core::models::DoneMode::MarkDone;
        let mut signature = Signature::new(account_id, "Standard");
        signature.plain = "Grüße\nAda".to_string();
        signature.is_default = true;
        settings.signatures.push(signature);
        core.save_settings(settings).unwrap();

        // 2. accounts.json -------------------------------------------------
        let account = Account {
            id: account_id,
            display_name: "Privat".to_string(),
            email: "ada@example.org".to_string(),
            kind: AccountKind::Imap,
            auth: AuthKind::Password,
            username: "ada@example.org".to_string(),
            incoming: Endpoint::imaps("imap.example.org"),
            outgoing: Endpoint::smtps("smtp.example.org"),
            proxy: None,
            oauth: None,
            enabled: true,
            created_at: hylmail_core::models::utc_now(),
        };
        core.config()
            .save_accounts(&hylmail_core::AccountsDocument {
                accounts: vec![account],
            })
            .unwrap();

        // 3. rules.json ----------------------------------------------------
        let mut rules = RuleSet::new();
        let mut rule = Rule::new("Rechnungen");
        rule.conditions
            .push(hylmail_core::models::Condition::contains(
                hylmail_core::models::ConditionField::Subject,
                "Rechnung",
            ));
        rule.actions.push(hylmail_core::models::Action::MoveTo {
            folder_path: "Archive".to_string(),
        });
        rules.add(rule).unwrap();
        core.config().save_rules(&rules).unwrap();

        // 4. A secret in the vault ----------------------------------------
        secrets
            .set(
                &CredentialKey::new(account_id, CredentialKind::IncomingPassword),
                &secret("hunter2"),
            )
            .unwrap();

        // 5. Folders, messages, bodies, contacts and conversations ---------
        let mut folder = Folder::new(account_id, "Inbox", "INBOX", FolderKind::Inbox);
        folder.delimiter = Some('/');
        folder.special_use = Some(hylmail_core::models::SpecialUse::Inbox);
        folder_id = folder.id;

        message_id = core
            .database()
            .write(|connection| {
                folders::upsert_folder(connection, &folder)?;

                let mut header = MessageHeader::new(account_id, folder.id);
                header.uid = Some(1);
                header.message_id = Some("<first@example.org>".to_string());
                header.subject = "Rechnung 2024".to_string();
                header.from = vec![MailAddress::with_name("billing@example.org", "Billing")];
                header.to = vec![MailAddress::new("ada@example.org")];
                header.size_bytes = 4096;
                header.flags = MessageFlags::FLAGGED;
                header.local_flags = LocalFlags::PINNED;
                header.has_attachments = true;
                header.received_at = Some(hylmail_core::models::utc_now());
                messages::upsert_message(connection, &mut header)?;

                let mut body = MessageBody::empty(header.id);
                body.plain_text = Some("Bitte zahlen Sie den Betrag.".to_string());
                body.html_raw = Some("<p>Bitte zahlen Sie den Betrag.</p>".to_string());
                body.attachments.push(Attachment {
                    part_id: "2".to_string(),
                    filename: Some("rechnung.pdf".to_string()),
                    mime_type: "application/pdf".to_string(),
                    size_bytes: Some(1024),
                    content_id: None,
                    inline: false,
                    is_calendar: false,
                });
                content::put_content(connection, &body)?;

                conversations::rebuild_for_account(connection, account_id)?;

                let mut contact =
                    Contact::collected(&MailAddress::with_name("billing@example.org", "Billing"));
                contact.account_id = Some(account_id);
                contact.source = ContactSource::Collected;
                hylmail_core::db::contacts::upsert_contact(connection, &contact)?;

                Ok(header.id)
            })
            .unwrap();

        // 6. drafts.json and outbox.json -----------------------------------
        let mut draft = Draft::new(account_id);
        draft.subject = "Antwort".to_string();
        draft.to.push(MailAddress::new("billing@example.org"));
        draft.signature_id = core
            .settings()
            .signatures
            .first()
            .map(|signature| signature.id);
        core.config()
            .save_drafts(&hylmail_core::DraftsDocument {
                drafts: vec![draft.clone()],
            })
            .unwrap();
        core.config()
            .save_outbox(&hylmail_core::OutboxDocument {
                items: vec![OutboxItem::new(draft)],
            })
            .unwrap();
    }

    // ------------------------------------------------------------------
    // Second run: everything must still be there.
    // ------------------------------------------------------------------
    {
        let core = bootstrap(dir.path(), Arc::clone(&secrets));

        let settings = core.settings();
        assert_eq!(settings.page_size, 250);
        assert!(settings.show_all_inboxes_on_start);
        assert_eq!(settings.signatures.len(), 1);
        assert_eq!(settings.signatures[0].name, "Standard");

        let accounts = core.config().load_accounts().unwrap();
        assert_eq!(accounts.accounts.len(), 1);
        assert_eq!(accounts.accounts[0].id, account_id);
        accounts.accounts[0].validate().unwrap();

        let rules = core.config().load_rules().unwrap();
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].move_target(), Some("Archive"));

        assert_eq!(core.config().load_drafts().unwrap().drafts.len(), 1);
        assert_eq!(core.config().load_outbox().unwrap().pending(), 1);

        assert_eq!(
            hylmail_core::secrets::load_secret(
                secrets.as_ref(),
                account_id,
                CredentialKind::IncomingPassword
            )
            .unwrap(),
            Some("hunter2".to_string())
        );

        assert_eq!(core.database().schema_version().unwrap(), SCHEMA_VERSION);

        let headers = core
            .database()
            .read(|connection| {
                messages::list_messages(connection, &MessageQuery::in_folder(folder_id, 500))
            })
            .unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].id, message_id);
        assert_eq!(headers[0].subject, "Rechnung 2024");
        assert!(headers[0].flags.is_flagged());
        assert!(headers[0].local_flags.contains(LocalFlags::PINNED));
        assert!(headers[0].body_cached);

        let body = core
            .database()
            .read(|connection| content::get_content(connection, message_id))
            .unwrap()
            .unwrap();
        assert_eq!(
            body.plain_text.as_deref(),
            Some("Bitte zahlen Sie den Betrag.")
        );
        assert_eq!(body.attachments.len(), 1);
        assert_eq!(body.attachments[0].display_name(), "rechnung.pdf");

        let found = core
            .database()
            .read(|connection| {
                hylmail_core::db::fts::search(connection, Some(account_id), "rechnung", 10)
            })
            .unwrap();
        assert_eq!(found, vec![message_id]);

        let conversations = core
            .database()
            .read(|connection| conversations::list_conversations(connection, account_id, 10))
            .unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].message_count, 1);

        let contacts = core
            .database()
            .read(|connection| hylmail_core::db::contacts::list_contacts(connection, None))
            .unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].primary_email(), Some("billing@example.org"));

        let stats = core.database().statistics().unwrap();
        assert_eq!(stats.folders, 1);
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.cached_bodies, 1);
        assert_eq!(stats.contacts, 1);
        assert_eq!(stats.conversations, 1);
    }
}

#[test]
fn the_content_cache_is_trimmed_to_the_configured_limit() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(InMemorySecretStore::new());
    let mut core = bootstrap(dir.path(), secrets);
    let account = AccountId::new();

    let mut settings = Settings::sanitized_default();
    settings.cache_limit_mb = 16; // the minimum
    core.save_settings(settings).unwrap();

    let folder = Folder::new(account, "Inbox", "INBOX", FolderKind::Inbox);
    core.database()
        .write(|connection| {
            folders::upsert_folder(connection, &folder)?;
            for uid in 0..8 {
                let mut header = MessageHeader::new(account, folder.id);
                header.uid = Some(uid);
                header.subject = format!("Message {uid}");
                header.from = vec![MailAddress::new("a@example.org")];
                messages::upsert_message(connection, &mut header)?;

                let mut body = MessageBody::empty(header.id);
                body.plain_text = Some("x".repeat(4 * 1024 * 1024));
                content::put_content(connection, &body)?;
            }
            Ok(())
        })
        .unwrap();

    let before = core.database().read(content::count).unwrap();
    assert_eq!(before, 8);

    let freed = core.enforce_cache_limit().unwrap();
    assert!(freed > 0);
    let after = core.database().read(content::count).unwrap();
    assert!(after < before, "the cache must have shrunk");
    assert!(
        core.database().read(content::cached_bytes).unwrap() <= core.settings().cache_limit_bytes()
    );
}

#[test]
fn deleting_an_account_can_be_done_through_the_repositories() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(InMemorySecretStore::new());
    let core = bootstrap(dir.path(), secrets);
    let account = AccountId::new();
    let other = AccountId::new();

    core.database()
        .write(|connection| {
            for id in [account, other] {
                let folder = Folder::new(id, "Inbox", "INBOX", FolderKind::Inbox);
                folders::upsert_folder(connection, &folder)?;
                let mut header = MessageHeader::new(id, folder.id);
                header.uid = Some(1);
                header.subject = "Hi".to_string();
                messages::upsert_message(connection, &mut header)?;
            }
            assert_eq!(folders::delete_account_folders(connection, account)?, 1);
            Ok(())
        })
        .unwrap();

    assert_eq!(
        core.database()
            .read(|connection| folders::count_folders(connection, account))
            .unwrap(),
        0
    );
    assert_eq!(
        core.database()
            .read(|connection| folders::count_folders(connection, other))
            .unwrap(),
        1
    );
    // The cascade removed the messages of the deleted account only.
    assert_eq!(
        core.database()
            .read(|connection| messages::count_messages(connection, &MessageQuery::default()))
            .unwrap(),
        1
    );
}
