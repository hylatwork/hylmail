//! The IMAP client driven against a scripted server on a loopback socket.
//!
//! Everything below the UI is exercised for real: `net::connect`, the greeting,
//! `LOGIN`, `LIST`, `EXAMINE`, `UID FETCH`, `UID SEARCH`, the header/envelope
//! parsing and the SQLite writes.  A fake server is the only way to test the
//! interesting failures (a changed `UIDVALIDITY`, a message deleted on the
//! server, a refused login) without depending on somebody's mailbox.
//!
//! The server speaks just enough IMAP to answer the commands HylMail issues; it
//! echoes the tag of the command it answers, so it does not have to know how the
//! client numbers its requests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use hylmail_core::db::{folders, messages, Database};
use hylmail_core::delete::{self, DeleteMode, DeleteOutcome};
use hylmail_core::error::Error;
use hylmail_core::imap::connection::{Credential, ImapConfig, ImapSession};
use hylmail_core::imap::idle::IdleEvent;
use hylmail_core::imap::synchronise::{self, SyncDepth, SyncOptions, SyncProgress};
use hylmail_core::models::account::{Account, AccountKind, AuthKind, Endpoint, TlsMode};
use hylmail_core::models::folder::{Folder, FolderKind};
use hylmail_core::models::message::{MessageFlags, MessageHeader};
use hylmail_core::net::TlsPolicy;
use hylmail_core::relocate::{self, MoveOutcome};
use hylmail_core::secrets::CredentialKind;
use secrecy::SecretString;

/// Counters so a test can prove that a second synchronisation did *not* fetch
/// the headers again.
#[derive(Debug, Default)]
struct Counters {
    logins: AtomicUsize,
    header_fetches: AtomicUsize,
    flag_fetches: AtomicUsize,
    searches: AtomicUsize,
    idles: AtomicUsize,
    logouts: AtomicUsize,
    /// The commands that changed the mailbox, in the order they arrived.
    ///
    /// Read-only commands are left out on purpose: a test that proves a delete
    /// kept `\Seen` wants exactly the `STORE` line and nothing else.
    writes: Mutex<Vec<String>>,
}

impl Counters {
    fn header_fetches(&self) -> usize {
        self.header_fetches.load(Ordering::SeqCst)
    }

    fn searches(&self) -> usize {
        self.searches.load(Ordering::SeqCst)
    }

    fn idles(&self) -> usize {
        self.idles.load(Ordering::SeqCst)
    }

    /// Remembers a mutating command.
    fn record(&self, command: &str) {
        self.writes
            .lock()
            .expect("the command log is not poisoned")
            .push(command.to_string());
    }

    /// The mutating commands that start with `prefix`, in order.
    fn writes_starting_with(&self, prefix: &str) -> Vec<String> {
        self.writes
            .lock()
            .expect("the command log is not poisoned")
            .iter()
            .filter(|command| command.starts_with(prefix))
            .cloned()
            .collect()
    }
}

/// What the scripted server should answer.
#[derive(Debug, Clone)]
struct Script {
    /// `UIDVALIDITY` reported by `EXAMINE`.
    uid_validity: u32,
    /// `UIDNEXT` reported by `EXAMINE`.
    uid_next: u32,
    /// UIDs returned by `SEARCH ALL`.
    all: Vec<u32>,
    /// UIDs returned by `SEARCH UNSEEN`.
    unseen: Vec<u32>,
    /// UIDs returned by `SEARCH FLAGGED`.
    flagged: Vec<u32>,
    /// Messages returned by `UID FETCH`.
    messages: Vec<FakeMessage>,
    /// Reject `LOGIN` with `NO`.
    reject_login: bool,
    /// Advertise `IDLE`.
    idle: bool,
    /// `* <n> EXISTS` to push right after `+ idling`.
    idle_pushes: Option<u32>,
    /// Advertise `MOVE`, which makes a delete atomic.
    move_capability: bool,
    /// Advertise `UIDPLUS`, which lets an expunge name a single message.
    uidplus: bool,
}

impl Script {
    /// The names the server advertises in its `CAPABILITY` response.
    fn capabilities(&self) -> String {
        let mut names = vec!["IMAP4rev1"];
        if self.uidplus {
            names.push("UIDPLUS");
        }
        if self.move_capability {
            names.push("MOVE");
        }
        if self.idle {
            names.push("IDLE");
        }
        names.join(" ")
    }
}

impl Default for Script {
    fn default() -> Self {
        Self {
            uid_validity: 42,
            uid_next: 3,
            all: vec![1, 2],
            unseen: vec![2],
            flagged: vec![],
            messages: vec![
                FakeMessage {
                    uid: 1,
                    flags: vec!["\\Seen"],
                    subject: "Rechnung",
                    envelope_date: "Thu, 12 Nov 2026 14:00:00 +0100",
                    internal_date: "12-Nov-2026 13:00:00 +0000",
                    message_id: "<rechnung@example.org>",
                    references: Some("<thread@example.org>"),
                    structure: "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 120 4)",
                },
                FakeMessage {
                    uid: 2,
                    flags: vec![],
                    subject: "=?UTF-8?Q?Gr=C3=BC=C3=9Fe?=",
                    envelope_date: "Fri, 13 Nov 2026 09:30:00 +0100",
                    internal_date: "13-Nov-2026 08:30:00 +0000",
                    message_id: "<gruesse@example.org>",
                    references: None,
                    // A multipart body with a PDF attachment: the client must
                    // report `has_attachments` without downloading anything.
                    structure: "((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 40 2)(\"APPLICATION\" \"PDF\" (\"NAME\" \"anhang.pdf\") NIL NIL \"BASE64\" 2048 NIL (\"ATTACHMENT\" (\"FILENAME\" \"anhang.pdf\")) NIL) \"MIXED\" (\"BOUNDARY\" \"x\") NIL NIL)",
                },
            ],
            reject_login: false,
            idle: false,
            idle_pushes: None,
            move_capability: true,
            uidplus: true,
        }
    }
}

/// One message the fake server knows about.
#[derive(Debug, Clone)]
struct FakeMessage {
    uid: u32,
    flags: Vec<&'static str>,
    subject: &'static str,
    envelope_date: &'static str,
    internal_date: &'static str,
    message_id: &'static str,
    references: Option<&'static str>,
    structure: &'static str,
}

impl FakeMessage {
    fn envelope(&self) -> String {
        format!(
            "(\"{}\" \"{}\" ((\"Ada\" NIL \"ada\" \"example.org\")) NIL NIL \
             ((\"Bob\" NIL \"bob\" \"example.org\")) NIL NIL {} \"{}\")",
            self.envelope_date,
            self.subject,
            self.references
                .map(|reference| format!("\"{reference}\""))
                .unwrap_or_else(|| "NIL".to_string()),
            self.message_id
        )
    }
}

/// Starts the server and returns its port plus the counters.
async fn start(script: Script) -> (u16, Arc<Counters>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let counters = Arc::new(Counters::default());
    let counters_for_task = Arc::clone(&counters);
    tokio::spawn(async move {
        // One connection per synchronisation round is what the engine does.
        while let Ok((stream, _)) = listener.accept().await {
            let script = script.clone();
            let counters = Arc::clone(&counters_for_task);
            tokio::spawn(async move {
                let _ = serve(stream, script, counters).await;
            });
        }
    });
    (port, counters)
}

async fn serve(stream: TcpStream, script: Script, counters: Arc<Counters>) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    write_half
        .write_all(
            format!(
                "* OK [CAPABILITY {}] HylMail test server ready\r\n",
                script.capabilities()
            )
            .as_bytes(),
        )
        .await?;

    while let Some(line) = lines.next_line().await? {
        let (tag, rest) = match line.split_once(' ') {
            Some((tag, rest)) => (tag.to_string(), rest.to_string()),
            None => continue,
        };
        let command = rest.to_ascii_uppercase();

        if command.starts_with("LOGIN") {
            counters.logins.fetch_add(1, Ordering::SeqCst);
            if script.reject_login {
                write_half
                    .write_all(
                        format!("{tag} NO [AUTHENTICATIONFAILED] invalid credentials\r\n")
                            .as_bytes(),
                    )
                    .await?;
            } else {
                write_half
                    .write_all(
                        format!(
                            "{tag} OK [CAPABILITY {}] LOGIN completed\r\n",
                            script.capabilities()
                        )
                        .as_bytes(),
                    )
                    .await?;
            }
            continue;
        }

        // Small helpers keep the match arms readable.
        let finish_ok = |action: &str| format!("{tag} OK {action} completed\r\n");

        if command.starts_with("LIST") {
            write_half
                .write_all(
                    format!(
                        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                         * LIST (\\HasChildren) \"/\" \"Projekte\"\r\n\
                         * LIST (\\HasNoChildren \\Sent) \"/\" \"Projekte/Gesendet\"\r\n\
                         {}",
                        finish_ok("LIST")
                    )
                    .as_bytes(),
                )
                .await?;
            continue;
        }

        if command.starts_with("EXAMINE") || command.starts_with("SELECT") {
            let write = format!(
                "* 2 EXISTS\r\n\
                 * FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
                 * OK [UIDVALIDITY {}] UIDs valid\r\n\
                 * OK [UIDNEXT {}] Predicted next UID\r\n\
                 {}",
                script.uid_validity,
                script.uid_next,
                finish_ok("EXAMINE")
            );
            write_half.write_all(write.as_bytes()).await?;
            continue;
        }

        if command.starts_with("UID SEARCH") {
            counters.searches.fetch_add(1, Ordering::SeqCst);
            let uids = if command.contains("UNSEEN") {
                &script.unseen
            } else if command.contains("FLAGGED") {
                &script.flagged
            } else {
                &script.all
            };
            let list = uids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            write_half
                .write_all(format!("* SEARCH {list}\r\n{}", finish_ok("SEARCH")).as_bytes())
                .await?;
            continue;
        }

        if command.starts_with("UID FETCH") {
            let flags_only = !command.contains("RFC822.SIZE");
            if flags_only {
                counters.flag_fetches.fetch_add(1, Ordering::SeqCst);
            } else {
                counters.header_fetches.fetch_add(1, Ordering::SeqCst);
            }
            for (index, message) in script.messages.iter().enumerate() {
                if !command.contains("UID") || !contains_uid(&command, message.uid) {
                    continue;
                }
                let sequence = u32::try_from(index + 1).unwrap_or(1);
                let response = if flags_only {
                    format!(
                        "* {sequence} FETCH (UID {} FLAGS ({}))\r\n",
                        message.uid,
                        message.flags.join(" ")
                    )
                    .into_bytes()
                } else {
                    let header = format!("References: {}\r\n", message.references.unwrap_or(""));
                    let mut response = format!(
                        "* {sequence} FETCH (UID {} FLAGS ({}) RFC822.SIZE 1204 \
                         INTERNALDATE \"{}\" ENVELOPE {} BODYSTRUCTURE {} \
                         BODY[HEADER.FIELDS (REFERENCES)] {{{}}}\r\n",
                        message.uid,
                        message.flags.join(" "),
                        message.internal_date,
                        message.envelope(),
                        message.structure,
                        header.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(header.as_bytes());
                    response.extend_from_slice(b")\r\n");
                    response
                };
                write_half.write_all(&response).await?;
            }
            write_half
                .write_all(finish_ok("UID FETCH").as_bytes())
                .await?;
            continue;
        }

        // Deleting a message: the `STORE` and the move or expunge the client
        // composes around it.  Every mutating command is remembered, so a test
        // can assert that a delete did not clear `\Seen` on the way.
        if command.starts_with("UID STORE") {
            counters.record(&rest);
            write_half
                .write_all(finish_ok("UID STORE").as_bytes())
                .await?;
            continue;
        }

        if command.starts_with("UID COPY") {
            counters.record(&rest);
            write_half
                .write_all(finish_ok("UID COPY").as_bytes())
                .await?;
            continue;
        }

        if command.starts_with("UID MOVE") {
            counters.record(&rest);
            write_half
                .write_all(finish_ok("UID MOVE").as_bytes())
                .await?;
            continue;
        }

        if command.starts_with("EXPUNGE") || command.starts_with("UID EXPUNGE") {
            counters.record(&rest);
            write_half
                .write_all(finish_ok("EXPUNGE").as_bytes())
                .await?;
            continue;
        }

        if command.starts_with("NOOP") {
            write_half.write_all(finish_ok("NOOP").as_bytes()).await?;
            continue;
        }

        // A real `IDLE`: the continuation, an optional notification, and only
        // then the tagged completion — which is what `DONE` is answered with.
        if command.starts_with("IDLE") {
            counters.idles.fetch_add(1, Ordering::SeqCst);
            write_half.write_all(b"+ idling\r\n").await?;
            if let Some(exists) = script.idle_pushes {
                write_half
                    .write_all(format!("* {exists} EXISTS\r\n").as_bytes())
                    .await?;
            }
            while let Some(line) = lines.next_line().await? {
                if line.trim().eq_ignore_ascii_case("DONE") {
                    break;
                }
            }
            write_half.write_all(finish_ok("IDLE").as_bytes()).await?;
            continue;
        }

        if command.starts_with("LOGOUT") {
            counters.logouts.fetch_add(1, Ordering::SeqCst);
            write_half
                .write_all(format!("* BYE\r\n{}", finish_ok("LOGOUT")).as_bytes())
                .await?;
            return Ok(());
        }

        write_half
            .write_all(format!("{tag} BAD unknown command\r\n").as_bytes())
            .await?;
    }
    Ok(())
}

/// `true` when the `UID FETCH <set>` argument contains `uid`.
///
/// Only the forms HylMail generates are handled: a single UID, `n:*` and a
/// comma separated list.
fn contains_uid(command: &str, uid: u32) -> bool {
    let Some(start) = command.find("UID FETCH ") else {
        return false;
    };
    let set = &command[start + "UID FETCH ".len()..];
    let set = set.split(' ').next().unwrap_or_default();
    for part in set.split(',') {
        if let Some((first, last)) = part.split_once(':') {
            let first = first.parse::<u32>().ok();
            let last = last.parse::<u32>().ok();
            let unlimited = part.ends_with(":*") || last.is_none();
            if let Some(first) = first {
                if uid >= first && (unlimited || last.is_some_and(|last| uid <= last)) {
                    return true;
                }
            }
        } else if part.parse::<u32>() == Ok(uid) {
            return true;
        }
    }
    false
}

/// An account and folder fixture on an in-memory database.
struct Fixture {
    database: Database,
    folder: Folder,
    config: ImapConfig,
    secret: SecretString,
}

impl Fixture {
    fn new(port: u16, uid_validity: Option<u32>) -> Self {
        let database = Database::open_in_memory_migrated().unwrap();
        let account_id = hylmail_core::models::ids::AccountId::new();
        let mut folder = Folder::new(account_id, "INBOX", "INBOX", FolderKind::Inbox);
        folder.uid_validity = uid_validity;
        database
            .write(|connection| {
                folders::upsert_folder_by_path(connection, &mut folder)?;
                Ok(())
            })
            .unwrap();
        let config = ImapConfig::new(
            Endpoint::new("127.0.0.1", port, TlsMode::None),
            "ada@example.org",
            AuthKind::Password,
            TlsPolicy::strict(),
        );
        Self {
            database,
            folder,
            config,
            secret: SecretString::from("geheim"),
        }
    }

    async fn login(&self) -> ImapSession {
        ImapSession::login(&self.config, Credential::Password(&self.secret))
            .await
            .unwrap()
    }

    fn sync_options(depth: SyncDepth) -> SyncOptions {
        SyncOptions {
            depth,
            batch_size: 100,
            max_new: 0,
        }
    }

    /// The account model of the same server, as the core sees it.
    fn account(&self) -> Account {
        Account {
            id: self.folder.account_id,
            display_name: "Ada".to_string(),
            email: "ada@example.org".to_string(),
            kind: AccountKind::Imap,
            auth: self.config.auth,
            username: self.config.username.clone(),
            incoming: self.config.endpoint.clone(),
            outgoing: self.config.endpoint.clone(),
            proxy: None,
            oauth: None,
            enabled: true,
            created_at: hylmail_core::models::ids::utc_now(),
        }
    }

    /// The stored header of one UID, after a synchronisation.
    fn header(&self, uid: u32) -> MessageHeader {
        let id = self
            .database
            .read(|connection| messages::list_uid_states(connection, self.folder.id))
            .unwrap()
            .into_iter()
            .find(|state| state.uid == uid)
            .expect("the uid was synchronised")
            .id;
        self.database
            .read(|connection| messages::get_message(connection, id))
            .unwrap()
            .expect("the state points at a stored message")
    }

    /// A trash folder of the same account.
    fn trash(&self) -> Folder {
        Folder::new(
            self.folder.account_id,
            "Papierkorb",
            "Papierkorb",
            FolderKind::Trash,
        )
    }

    /// An archive folder of the same account.
    fn archive(&self) -> Folder {
        Folder::new(
            self.folder.account_id,
            "Archiv",
            "Archiv",
            FolderKind::Archive,
        )
    }
}

#[tokio::test]
async fn a_folder_is_synchronised_from_scratch() {
    let (port, counters) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    let mut session = fixture.login().await;

    assert_eq!(session.host(), "127.0.0.1");
    assert!(session.capabilities().has_uidplus());
    assert!(!session.capabilities().has_idle());

    let mut progress = Vec::new();
    let report = synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Incremental),
        &mut |update: SyncProgress| progress.push(update),
    )
    .await
    .unwrap();

    assert_eq!(report.new_messages, 2);
    assert!(!report.uid_validity_reset);
    assert_eq!(report.uid_next, Some(3));
    assert!(progress
        .iter()
        .any(|update| matches!(update, SyncProgress::FolderFinished { .. })));
    assert!(counters.header_fetches() >= 1);
    assert!(
        counters.searches() >= 2,
        "UNSEEN and FLAGGED are reconciled"
    );

    // -- what landed in the database ----------------------------------------
    let stored = fixture
        .database
        .read(|connection| {
            let states = messages::list_uid_states(connection, fixture.folder.id)?;
            let headers = states
                .iter()
                .map(|state| messages::get_message(connection, state.id))
                .collect::<hylmail_core::Result<Vec<_>>>()?;
            Ok((states, headers))
        })
        .unwrap();
    let (states, headers) = stored;
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].uid, 1);
    assert!(states[0].flags.contains(MessageFlags::SEEN));
    assert!(!states[1].flags.contains(MessageFlags::SEEN));

    let first = headers[0].as_ref().unwrap();
    assert_eq!(first.subject, "Rechnung");
    assert_eq!(first.from[0].email, "ada@example.org");
    assert_eq!(first.to[0].email, "bob@example.org");
    assert_eq!(first.message_id.as_deref(), Some("rechnung@example.org"));
    // The `References` header of the fetch wins over `In-Reply-To` from the
    // envelope, which the server did not send.
    assert_eq!(first.in_reply_to.as_deref(), Some("thread@example.org"));
    assert!(first.date.is_some());
    assert!(first.received_at.is_some());
    assert_eq!(first.size_bytes, 1204);
    assert!(!first.has_attachments);

    let second = headers[1].as_ref().unwrap();
    assert_eq!(second.subject, "Grüße");
    assert!(second.has_attachments, "the PDF part must be detected");
    assert!(second.in_reply_to.is_none());

    // -- the folder state ---------------------------------------------------
    let folder = fixture
        .database
        .read(|connection| folders::get_folder(connection, fixture.folder.id))
        .unwrap()
        .unwrap();
    assert_eq!(folder.uid_validity, Some(42));
    assert_eq!(folder.uid_next, Some(3));
    assert_eq!(folder.total_count, 2);
    assert_eq!(folder.unread_count, 1);

    session.logout().await;
    assert_eq!(counters.logins.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn list_folders_builds_the_tree() {
    let (port, _) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    let mut session = fixture.login().await;

    let discovered = session
        .list_folders(fixture.folder.account_id)
        .await
        .unwrap();
    let paths: Vec<&str> = discovered
        .iter()
        .map(|folder| folder.path.as_str())
        .collect();
    assert!(paths.contains(&"INBOX"));
    assert!(paths.contains(&"Projekte"));
    assert!(paths.contains(&"Projekte/Gesendet"));

    let sent = discovered
        .iter()
        .find(|folder| folder.path == "Projekte/Gesendet")
        .unwrap();
    assert_eq!(sent.kind, FolderKind::Sent);
    let parent = discovered
        .iter()
        .find(|folder| folder.path == "Projekte")
        .unwrap();
    assert_eq!(sent.parent_id, Some(parent.id));

    session.logout().await;
}

#[tokio::test]
async fn a_second_pass_fetches_nothing() {
    let (port, counters) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    let mut session = fixture.login().await;

    let options = Fixture::sync_options(SyncDepth::Incremental);
    let first = synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        options,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(first.new_messages, 2);
    let fetches_after_first = counters.header_fetches();

    let second = synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        options,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(second.new_messages, 0);
    assert!(second.is_noop());
    assert_eq!(
        counters.header_fetches(),
        fetches_after_first,
        "the second pass must not refetch the headers"
    );
}

#[tokio::test]
async fn a_changed_uid_validity_rebuilds_the_folder() {
    let (port, _) = start(Script::default()).await;
    // The local copy claims a different generation of UIDs.
    let fixture = Fixture::new(port, Some(7));
    let mut session = fixture.login().await;

    let report = synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Incremental),
        &mut |_| {},
    )
    .await
    .unwrap();

    assert!(report.uid_validity_reset);
    assert_eq!(report.new_messages, 2);
    let folder = fixture
        .database
        .read(|connection| folders::get_folder(connection, fixture.folder.id))
        .unwrap()
        .unwrap();
    assert_eq!(folder.uid_validity, Some(42));
    assert_eq!(folder.total_count, 2);
}

#[tokio::test]
async fn a_full_pass_removes_messages_the_server_dropped() {
    let script = Script {
        // The server only knows about UID 1 any more.
        all: vec![1],
        uid_next: 3,
        ..Script::default()
    };
    let (port, _) = start(script).await;
    let fixture = Fixture::new(port, Some(42));
    let mut session = fixture.login().await;

    synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Incremental),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        fixture
            .database
            .read(|connection| messages::list_uid_states(connection, fixture.folder.id))
            .unwrap()
            .len(),
        2
    );

    let report = synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Full),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.removed_messages, 1);
    let states = fixture
        .database
        .read(|connection| messages::list_uid_states(connection, fixture.folder.id))
        .unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].uid, 1);
}

#[tokio::test]
async fn flags_are_pulled_back_from_the_server() {
    let (port, _) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    let mut session = fixture.login().await;

    synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Incremental),
        &mut |_| {},
    )
    .await
    .unwrap();

    let script = Script {
        // The user read the second message in another client and flagged it; the
        // server now reports no unseen message at all.
        unseen: vec![],
        flagged: vec![2],
        ..Script::default()
    };
    let (port, _) = start(script).await;
    let mut fixture = fixture;
    fixture.config.endpoint.port = port;
    let mut session = fixture.login().await;

    let report = synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Incremental),
        &mut |_| {},
    )
    .await
    .unwrap();

    assert_eq!(report.updated_flags, 1);
    let states = fixture
        .database
        .read(|connection| messages::list_uid_states(connection, fixture.folder.id))
        .unwrap();
    let second = states.iter().find(|state| state.uid == 2).unwrap();
    // `\Seen` was *not* added in an incremental pass (the server only said it is
    // no longer unseen), but the `\Flagged` bit was.
    assert!(second.flags.contains(MessageFlags::FLAGGED));
}

#[tokio::test]
async fn a_refused_login_is_an_auth_error() {
    let script = Script {
        reject_login: true,
        ..Script::default()
    };
    let (port, _) = start(script).await;
    let fixture = Fixture::new(port, Some(42));

    let error = ImapSession::login(&fixture.config, Credential::Password(&fixture.secret))
        .await
        .unwrap_err();
    // The account dialog has to point at the password field, not at the server.
    assert!(matches!(error, Error::Auth { .. }), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("invalid credentials"), "{text}");
    // A rejected password is the user's problem, not a temporary outage.
    assert!(!error.is_transient());
}

#[tokio::test]
async fn idle_reports_new_mail_and_gives_the_session_back() {
    let script = Script {
        idle: true,
        idle_pushes: Some(3),
        ..Script::default()
    };
    let (port, counters) = start(script).await;
    let fixture = Fixture::new(port, Some(42));
    let mut session = fixture.login().await;
    assert!(session.capabilities().has_idle());
    session.examine("INBOX").await.unwrap();

    let mut guard = session.into_idle();
    guard.start().await.unwrap();
    // The server pushes `* 3 EXISTS` straight after `+ idling`, so this returns
    // without waiting for the 25 minute re-issue deadline.
    let event = guard.wait_reissue().await.unwrap();
    assert_eq!(
        event,
        IdleEvent::MailboxChanged {
            exists: Some(3),
            expunged: None,
            flags_changed: None,
        }
    );
    assert!(event.needs_sync());
    assert!(!event.needs_reissue());

    // `DONE` ends the `IDLE`, and the very same connection keeps working.
    let mut session = guard.finish().await.unwrap();
    assert_eq!(session.idle_rounds(), 1);
    assert_eq!(session.selected(), Some("INBOX"));
    session.noop().await.unwrap();
    session.logout().await;
    assert_eq!(counters.idles(), 1);
}

#[tokio::test]
async fn the_engine_synchronises_an_account_and_stops_cleanly() {
    use hylmail_core::models::account::{Account, AccountKind};
    use hylmail_core::models::ids::AccountId;
    use hylmail_core::models::Settings;
    use hylmail_core::paths::{MapEnv, Os, PORTABLE_ENV};
    use hylmail_core::sync::{EngineConfig, EngineEvent, SyncEngine};
    use hylmail_core::{
        AccountsDocument, AppPaths, ConfigStore, CredentialKey, InMemorySecretStore, SecretStore,
    };

    let script = Script {
        idle: true,
        ..Script::default()
    };
    let (port, counters) = start(script).await;

    // A portable home inside a temporary directory: the engine reads its account
    // list from disk, exactly like the real application does.
    let home = tempfile::tempdir().unwrap();
    let env = MapEnv::new().with(PORTABLE_ENV, home.path().to_string_lossy().to_string());
    let paths = AppPaths::resolve_for(Os::Linux, &env).unwrap();
    let store = ConfigStore::new(paths.clone());
    store.ensure_dirs().unwrap();

    let account_id = AccountId::new();
    let account = Account {
        id: account_id,
        display_name: "Ada".to_string(),
        email: "ada@example.org".to_string(),
        kind: AccountKind::Imap,
        auth: AuthKind::Password,
        username: "ada@example.org".to_string(),
        incoming: Endpoint::new("127.0.0.1", port, TlsMode::None),
        outgoing: Endpoint::new("127.0.0.1", port, TlsMode::None),
        proxy: None,
        oauth: None,
        enabled: true,
        created_at: hylmail_core::models::ids::utc_now(),
    };
    account.validate().unwrap();
    store
        .save_accounts(&AccountsDocument {
            accounts: vec![account.clone()],
        })
        .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
    secrets
        .set(
            &CredentialKey::new(account_id, CredentialKind::IncomingPassword),
            &SecretString::from("geheim"),
        )
        .unwrap();

    let database = Arc::new(Database::open_in_memory_migrated().unwrap());
    let config = EngineConfig::new(
        paths,
        Arc::clone(&database),
        secrets,
        Settings::sanitized_default(),
    );
    let (mut handle, task) = SyncEngine::spawn(config);

    // Collect events until the account sits in `IDLE`; the server never pushes
    // anything on its own, so this is where the round ends.
    let mut connected = false;
    let mut stored = 0usize;
    let mut idling = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, handle.recv()).await {
        match event {
            EngineEvent::Connected {
                host, encrypted, ..
            } => {
                assert_eq!(host, "127.0.0.1");
                assert!(!encrypted, "the fake server speaks cleartext");
                connected = true;
            }
            EngineEvent::FolderSynced { report, .. } => stored += report.new_messages,
            EngineEvent::Idling { .. } => {
                idling = true;
                break;
            }
            EngineEvent::Disconnected { reason, .. } => {
                panic!("the engine lost the connection: {reason}")
            }
            _ => {}
        }
    }
    assert!(connected, "no `Connected` event arrived");
    assert!(idling, "the account never reached `IDLE`");
    assert!(stored >= 2);

    // Shutting down ends the `IDLE`, logs out and joins every task.
    handle.shutdown().unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the engine did not stop")
        .unwrap();
    assert_eq!(counters.logouts.load(Ordering::SeqCst), 1);

    // The account really is in the database the engine was given.
    let inbox = database
        .read(|connection| {
            let folders = folders::list_folders(connection, account_id)?;
            let inbox = folders
                .into_iter()
                .find(|folder| folder.path == "INBOX")
                .expect("INBOX was stored");
            let states = messages::list_uid_states(connection, inbox.id)?;
            Ok(states)
        })
        .unwrap();
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox[0].uid, 1);
    assert!(inbox[0].flags.contains(MessageFlags::SEEN));
    assert!(!inbox[1].flags.contains(MessageFlags::SEEN));
}

#[tokio::test]
async fn credentials_never_appear_in_the_debug_output() {
    let secret = SecretString::from("super-geheim");
    let credential = Credential::Password(&secret);
    let text = format!("{credential:?}");
    assert!(!text.contains("super-geheim"));
    assert!(text.contains("REDACTED"));

    let token = SecretString::from("token-wert");
    let credential = Credential::OAuth2 {
        user: "ada@example.org".to_string(),
        token: &token,
    };
    let text = format!("{credential:?}");
    assert!(!text.contains("token-wert"));
    assert!(text.contains("ada@example.org"));
    assert!(credential.is_token());
}

#[tokio::test]
async fn a_missing_password_is_reported_as_such() {
    // Nothing to connect to: the engine must never try.
    let store = hylmail_core::InMemorySecretStore::default();
    let account_id = hylmail_core::models::ids::AccountId::new();
    let loaded =
        hylmail_core::secrets::load_secret(&store, account_id, CredentialKind::IncomingPassword)
            .unwrap();
    assert!(loaded.is_none());
}

// ---------------------------------------------------------------------------
// deleting a message
// ---------------------------------------------------------------------------

/// Synchronises the inbox once and then deletes UID 1 through the core.
async fn delete_uid_one(
    fixture: &Fixture,
    trash: Option<&Folder>,
    mode: DeleteMode,
) -> DeleteOutcome {
    let mut session = fixture.login().await;
    synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Incremental),
        &mut |_| {},
    )
    .await
    .unwrap();
    session.logout().await;

    delete::delete(
        &fixture.account(),
        &fixture.folder,
        trash,
        &fixture.header(1),
        mode,
        Credential::Password(&fixture.secret),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn deleting_a_message_moves_it_into_the_trash() {
    let (port, counters) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    let trash = fixture.trash();

    let outcome = delete_uid_one(&fixture, Some(&trash), DeleteMode::ToTrash).await;

    assert_eq!(outcome, DeleteOutcome::MovedToTrash);
    // `MOVE` is atomic, so neither a `STORE` nor an expunge may be needed.
    assert_eq!(
        counters.writes_starting_with("UID MOVE"),
        vec!["UID MOVE 1 \"Papierkorb\"".to_string()]
    );
    assert!(counters.writes_starting_with("UID STORE").is_empty());
    assert!(counters.writes_starting_with("UID COPY").is_empty());
    assert!(counters.writes_starting_with("EXPUNGE").is_empty());
}

#[tokio::test]
async fn deleting_without_move_copies_flags_and_expunges() {
    let script = Script {
        move_capability: false,
        ..Script::default()
    };
    let (port, counters) = start(script).await;
    let fixture = Fixture::new(port, Some(42));
    let trash = fixture.trash();

    let outcome = delete_uid_one(&fixture, Some(&trash), DeleteMode::ToTrash).await;

    assert_eq!(outcome, DeleteOutcome::MovedToTrash);
    assert_eq!(
        counters.writes_starting_with("UID COPY"),
        vec!["UID COPY 1 \"Papierkorb\"".to_string()]
    );
    // The copy is in the trash now; the original has to go.
    assert_eq!(
        counters.writes_starting_with("UID EXPUNGE"),
        vec!["UID EXPUNGE 1".to_string()]
    );
}

#[tokio::test]
async fn a_permanent_delete_flags_and_expunges_without_copying() {
    let (port, counters) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    let trash = fixture.trash();

    // A trash folder is known, but the user asked for a permanent delete.
    let outcome = delete_uid_one(&fixture, Some(&trash), DeleteMode::Permanently).await;

    assert_eq!(outcome, DeleteOutcome::Removed);
    assert!(counters.writes_starting_with("UID MOVE").is_empty());
    assert!(counters.writes_starting_with("UID COPY").is_empty());
    // UID 1 is `\Seen` on the server.  The `STORE` marks it deleted *and keeps
    // the read state*: a bare `DELETED` would have cleared it.
    assert_eq!(
        counters.writes_starting_with("UID STORE"),
        vec!["UID STORE 1 +FLAGS.SILENT (\\Deleted)".to_string()]
    );
    assert_eq!(
        counters.writes_starting_with("UID EXPUNGE"),
        vec!["UID EXPUNGE 1".to_string()]
    );
}

#[tokio::test]
async fn without_uidplus_the_whole_mailbox_is_expunged() {
    let script = Script {
        uidplus: false,
        ..Script::default()
    };
    let (port, counters) = start(script).await;
    let fixture = Fixture::new(port, Some(42));

    let outcome = delete_uid_one(&fixture, None, DeleteMode::Permanently).await;

    assert_eq!(outcome, DeleteOutcome::Removed);
    // Without `UIDPLUS` there is no way to name a single message; the client
    // falls back to the mailbox-wide expunge instead of failing.
    assert_eq!(
        counters.writes_starting_with("EXPUNGE"),
        vec!["EXPUNGE".to_string()]
    );
    assert!(counters.writes_starting_with("UID EXPUNGE").is_empty());
}

#[tokio::test]
async fn deleting_from_the_trash_is_permanent() {
    let (port, counters) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    // The message already is in the folder the account calls its trash, so a
    // move would change nothing; deleting from the trash is final.
    let trash = fixture.folder.clone();

    let outcome = delete_uid_one(&fixture, Some(&trash), DeleteMode::ToTrash).await;

    assert_eq!(outcome, DeleteOutcome::Removed);
    assert!(counters.writes_starting_with("UID MOVE").is_empty());
    assert_eq!(
        counters.writes_starting_with("UID EXPUNGE"),
        vec!["UID EXPUNGE 1".to_string()]
    );
}

// ---------------------------------------------------------------------------
// moving a message into another folder
// ---------------------------------------------------------------------------

/// Synchronises the inbox once and then moves UID 1 through the core.
async fn move_uid_one(fixture: &Fixture, target: &Folder) -> MoveOutcome {
    let mut session = fixture.login().await;
    synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Incremental),
        &mut |_| {},
    )
    .await
    .unwrap();
    session.logout().await;

    relocate::move_to(
        &fixture.account(),
        &fixture.folder,
        target,
        &fixture.header(1),
        Credential::Password(&fixture.secret),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn moving_a_message_uses_uid_move() {
    let (port, counters) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    let archive = fixture.archive();

    let outcome = move_uid_one(&fixture, &archive).await;

    // IMAP keeps addressing the message by UID, so nothing is renumbered.
    assert_eq!(outcome, MoveOutcome::default());
    assert_eq!(
        counters.writes_starting_with("UID MOVE"),
        vec!["UID MOVE 1 \"Archiv\"".to_string()]
    );
    assert!(counters.writes_starting_with("UID COPY").is_empty());
    assert!(counters.writes_starting_with("EXPUNGE").is_empty());
}

#[tokio::test]
async fn moving_without_move_copies_flags_and_expunges() {
    let script = Script {
        move_capability: false,
        ..Script::default()
    };
    let (port, counters) = start(script).await;
    let fixture = Fixture::new(port, Some(42));
    let archive = fixture.archive();

    let outcome = move_uid_one(&fixture, &archive).await;

    assert_eq!(outcome, MoveOutcome::default());
    assert_eq!(
        counters.writes_starting_with("UID COPY"),
        vec!["UID COPY 1 \"Archiv\"".to_string()]
    );
    // The copy is in place; the original has to be expunged.
    assert_eq!(
        counters.writes_starting_with("UID EXPUNGE"),
        vec!["UID EXPUNGE 1".to_string()]
    );
}

#[tokio::test]
async fn moving_into_the_current_folder_is_refused() {
    let (port, counters) = start(Script::default()).await;
    let fixture = Fixture::new(port, Some(42));
    let mut session = fixture.login().await;
    synchronise::sync_folder(
        &mut session,
        &fixture.database,
        &fixture.folder,
        Fixture::sync_options(SyncDepth::Incremental),
        &mut |_| {},
    )
    .await
    .unwrap();
    session.logout().await;

    let error = relocate::move_to(
        &fixture.account(),
        &fixture.folder,
        &fixture.folder,
        &fixture.header(1),
        Credential::Password(&fixture.secret),
        &TlsPolicy::strict(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Config(_)));
    assert!(counters.writes_starting_with("UID MOVE").is_empty());
}
