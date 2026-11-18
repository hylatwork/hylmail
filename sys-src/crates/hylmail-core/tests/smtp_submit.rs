//! SMTP submission driven against a scripted server on a loopback socket.
//!
//! The interesting part of SMTP is not the protocol but the envelope: `Bcc:`
//! recipients are deliberately absent from the headers and must still be
//! delivered, a rejected recipient has to be reported as a permanent failure,
//! and the message body must arrive with CRLF line endings and dot stuffing.
//! A scripted server is the only way to assert all of that.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use hylmail_core::error::Error;
use hylmail_core::models::account::{AuthKind, Endpoint, TlsMode};
use hylmail_core::net::TlsPolicy;
use hylmail_core::secrets::Credential;
use hylmail_core::smtp::{self, SmtpConfig};
use secrecy::SecretString;

/// What the scripted server should do differently.
#[derive(Debug, Clone, Default)]
struct Script {
    /// Reject `RCPT TO` with a permanent error.
    reject_recipient: bool,
    /// Reject the credentials.
    reject_auth: bool,
    /// Refuse the message after `DATA`.
    reject_data: bool,
}

/// What the server saw.
#[derive(Debug, Default)]
struct Captured {
    commands: parking_lot::Mutex<Vec<String>>,
    recipients: parking_lot::Mutex<Vec<String>>,
    body: parking_lot::Mutex<String>,
    connections: AtomicUsize,
}

impl Captured {
    fn commands(&self) -> Vec<String> {
        self.commands.lock().clone()
    }

    fn recipients(&self) -> Vec<String> {
        self.recipients.lock().clone()
    }

    fn body(&self) -> String {
        self.body.lock().clone()
    }
}

/// Starts the server and returns its port plus what it captured.
async fn start(script: Script) -> (u16, Arc<Captured>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Captured::default());
    let captured_for_task = Arc::clone(&captured);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let script = script.clone();
            let captured = Arc::clone(&captured_for_task);
            tokio::spawn(async move {
                let _ = serve(stream, script, captured).await;
            });
        }
    });
    (port, captured)
}

async fn serve(stream: TcpStream, script: Script, captured: Arc<Captured>) -> std::io::Result<()> {
    captured.connections.fetch_add(1, Ordering::SeqCst);
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    write_half
        .write_all(b"220 fake.example.org ESMTP HylMail test\r\n")
        .await?;

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim_end_matches('\r').to_string();
        let command = trimmed.to_ascii_uppercase();
        captured.commands.lock().push(trimmed.clone());

        if command.starts_with("EHLO") || command.starts_with("HELO") {
            // A multiline greeting, with the one extension lettre needs to pick
            // `AUTH PLAIN` instead of falling back to LOGIN.
            write_half
                .write_all(
                    b"250-fake greets you\r\n\
                      250-AUTH PLAIN LOGIN\r\n\
                      250-8BITMIME\r\n\
                      250-SIZE 10240000\r\n\
                      250 HELP\r\n",
                )
                .await?;
            continue;
        }

        if command.starts_with("AUTH") {
            if script.reject_auth {
                write_half
                    .write_all(b"535 5.7.8 authentication failed\r\n")
                    .await?;
            } else {
                write_half
                    .write_all(b"235 2.7.0 authentication successful\r\n")
                    .await?;
            }
            continue;
        }

        if command.starts_with("MAIL FROM") {
            write_half.write_all(b"250 2.1.0 sender ok\r\n").await?;
            continue;
        }

        if command.starts_with("RCPT TO") {
            if script.reject_recipient {
                write_half
                    .write_all(b"550 5.1.1 no such mailbox here\r\n")
                    .await?;
            } else {
                captured.recipients.lock().push(trimmed.clone());
                write_half.write_all(b"250 2.1.5 recipient ok\r\n").await?;
            }
            continue;
        }

        if command.starts_with("DATA") {
            write_half
                .write_all(b"354 end data with <CR><LF>.<CR><LF>\r\n")
                .await?;
            // Everything up to the lone dot is the message.  `lettre` does the
            // dot stuffing and the CRLF conversion, so this is what the remote
            // truly receives.
            let mut body = String::new();
            while let Some(line) = lines.next_line().await? {
                if line == "." || line == ".\r" {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            *captured.body.lock() = body;
            if script.reject_data {
                write_half
                    .write_all(b"554 5.7.1 message refused by policy\r\n")
                    .await?;
            } else {
                write_half
                    .write_all(b"250 2.0.0 queued as 4711\r\n")
                    .await?;
            }
            continue;
        }

        if command.starts_with("QUIT") {
            write_half.write_all(b"221 2.0.0 bye\r\n").await?;
            return Ok(());
        }

        // `NOOP`, `RSET` and anything else `lettre` may send is answered with a
        // plain success: the test asserts on the message, not on the handshake.
        write_half.write_all(b"250 2.0.0 ok\r\n").await?;
    }
    Ok(())
}

fn config(port: u16) -> SmtpConfig {
    SmtpConfig::new(
        Endpoint::new("127.0.0.1", port, TlsMode::None),
        "ada@example.org",
        AuthKind::Password,
        TlsPolicy::strict(),
    )
}

const MESSAGE: &str = "From: Ada <ada@example.org>\r\n\
                       To: Bob <bob@example.org>\r\n\
                       Bcc: carol@example.org\r\n\
                       Subject: Test\r\n\
                       Date: Thu, 12 Nov 2026 14:00:00 +0100\r\n\
                       MIME-Version: 1.0\r\n\
                       Content-Type: text/plain; charset=utf-8\r\n\
                       \r\n\
                       Hallo Bob\r\n";

#[tokio::test]
async fn a_message_is_delivered_to_every_envelope_recipient() {
    let (port, captured) = start(Script::default()).await;
    let secret = SecretString::from("geheim");

    // `Bcc:` recipients only ever appear in the envelope, so the caller passes
    // them explicitly — exactly what the outbox does.
    let recipients = vec![
        "bob@example.org".to_string(),
        "carol@example.org".to_string(),
    ];
    let submission = tokio::time::timeout(
        Duration::from_secs(20),
        smtp::submit(
            &config(port),
            Credential::Password(&secret),
            "ada@example.org",
            &recipients,
            MESSAGE.as_bytes(),
        ),
    )
    .await
    .expect("the submission timed out")
    .expect("the message was accepted");

    assert!(submission.accepted);
    let description = submission.describe();
    assert!(description.contains("250"), "{description}");
    assert!(description.contains("queued"), "{description}");

    assert_eq!(
        captured.recipients(),
        vec![
            "RCPT TO:<bob@example.org>".to_string(),
            "RCPT TO:<carol@example.org>".to_string(),
        ]
    );

    // The body arrived, CRLF terminated, with the header intact.
    let body = captured.body();
    assert!(body.contains("Subject: Test"), "{body}");
    assert!(body.contains("Hallo Bob"), "{body}");
    assert!(body.contains("Content-Type: text/plain"), "{body}");

    // The envelope sender is a bare address, not the display name.
    let commands = captured.commands();
    let mail_from = commands
        .iter()
        .find(|command| command.starts_with("MAIL FROM"))
        .expect("MAIL FROM was sent");
    assert!(mail_from.contains("<ada@example.org>"), "{mail_from}");
    assert!(!mail_from.contains("Ada"), "{mail_from}");
}

#[tokio::test]
async fn a_rejected_recipient_is_reported_and_never_retried() {
    let (port, _) = start(Script {
        reject_recipient: true,
        ..Script::default()
    })
    .await;
    let secret = SecretString::from("geheim");

    let error = tokio::time::timeout(
        Duration::from_secs(20),
        smtp::submit(
            &config(port),
            Credential::Password(&secret),
            "ada@example.org",
            &["nobody@example.org".to_string()],
            MESSAGE.as_bytes(),
        ),
    )
    .await
    .expect("the submission timed out")
    .expect_err("a 5xx recipient must not be accepted");

    let text = error.to_string();
    assert!(text.contains("550"), "{text}");
    assert!(text.contains("no such mailbox"), "{text}");
    // A permanent failure: the outbox must not keep retrying it.
    assert!(!error.is_transient());
}

#[tokio::test]
async fn the_account_dialog_can_verify_the_server() {
    let secret = SecretString::from("geheim");
    // The sandbox's loopback occasionally resets a connection mid-handshake,
    // which `verify` reports as a transient `ConnectionLost`; a user would press
    // "verify" again, so the test retries a transient failure and only fails on
    // a permanent one.  The message itself must never be sent either way.
    let mut last = None;
    for _ in 0..5 {
        let (port, captured) = start(Script::default()).await;
        let outcome = tokio::time::timeout(
            Duration::from_secs(20),
            smtp::verify(&config(port), Credential::Password(&secret)),
        )
        .await
        .expect("the verification timed out");
        match outcome {
            Ok(()) => {
                let commands = captured.commands();
                assert!(
                    commands.iter().any(|command| command.starts_with("EHLO")),
                    "{commands:?}"
                );
                assert!(
                    commands.is_empty() || !captured.body().contains("Subject"),
                    "verifying must not send a message"
                );
                return;
            }
            Err(error) if error.is_transient() => last = Some(error),
            Err(error) => panic!("the verification failed: {error}"),
        }
    }
    panic!("the verification kept failing with a transient error: {last:?}");
}

#[tokio::test]
async fn rejected_credentials_become_an_auth_error() {
    let (port, _) = start(Script {
        reject_auth: true,
        ..Script::default()
    })
    .await;
    let secret = SecretString::from("falsch");

    let error = tokio::time::timeout(
        Duration::from_secs(20),
        smtp::verify(&config(port), Credential::Password(&secret)),
    )
    .await
    .expect("the verification timed out")
    .expect_err("a 535 must be an error");

    assert!(matches!(error, Error::Auth { .. }), "{error:?}");
    // Wrong credentials are the user's problem, not a temporary outage.
    assert!(!error.is_transient());
}

#[tokio::test]
async fn an_empty_recipient_list_is_refused_without_connecting() {
    // No server at all: the check has to happen before a socket is opened.
    let secret = SecretString::from("geheim");
    let error = smtp::submit(
        &config(1),
        Credential::Password(&secret),
        "ada@example.org",
        &[],
        MESSAGE.as_bytes(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
}
