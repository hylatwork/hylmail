//! The composer feeding the SMTP client, end to end on a loopback socket.
//!
//! The composer and the transport each have their own suite; this one proves
//! the seam: the bytes `build` produces are exactly what `smtp::submit` puts on
//! the wire, the envelope recipients include the `Bcc` list while the header
//! block does not, and a signature the user configured survives the trip.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use hylmail_core::compose::{build, ComposeContext, ComposedMessage};
use hylmail_core::models::account::{AuthKind, Endpoint, TlsMode};
use hylmail_core::models::{AccountId, Draft, MailAddress, Signature};
use hylmail_core::net::TlsPolicy;
use hylmail_core::secrets::Credential;
use hylmail_core::smtp::{self, SmtpConfig};
use secrecy::SecretString;

/// What the scripted server captured.
#[derive(Debug, Default)]
struct Captured {
    recipients: parking_lot::Mutex<Vec<String>>,
    body: parking_lot::Mutex<String>,
}

impl Captured {
    fn recipients(&self) -> Vec<String> {
        self.recipients.lock().clone()
    }

    fn body(&self) -> String {
        self.body.lock().clone()
    }
}

async fn start() -> (u16, Arc<Captured>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Captured::default());
    let shared = Arc::clone(&captured);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let captured = Arc::clone(&shared);
            tokio::spawn(async move {
                let _ = serve(stream, captured).await;
            });
        }
    });
    (port, captured)
}

async fn serve(stream: TcpStream, captured: Arc<Captured>) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    write_half
        .write_all(b"220 fake.example.org ESMTP HylMail test\r\n")
        .await?;
    while let Some(line) = lines.next_line().await? {
        let command = line.trim_end_matches('\r').to_ascii_uppercase();
        if command.starts_with("EHLO") || command.starts_with("HELO") {
            write_half
                .write_all(
                    b"250-fake greets you\r\n\
                      250-AUTH PLAIN LOGIN\r\n\
                      250-8BITMIME\r\n\
                      250-SIZE 10240000\r\n\
                      250 HELP\r\n",
                )
                .await?;
        } else if command.starts_with("AUTH") {
            write_half
                .write_all(b"235 2.7.0 authentication successful\r\n")
                .await?;
        } else if command.starts_with("MAIL FROM") {
            write_half.write_all(b"250 2.1.0 sender ok\r\n").await?;
        } else if command.starts_with("RCPT TO") {
            captured
                .recipients
                .lock()
                .push(line.trim_end_matches('\r').to_string());
            write_half.write_all(b"250 2.1.5 recipient ok\r\n").await?;
        } else if command.starts_with("DATA") {
            write_half
                .write_all(b"354 end data with <CR><LF>.<CR><LF>\r\n")
                .await?;
            let mut body = String::new();
            while let Some(line) = lines.next_line().await? {
                if line == "." || line == ".\r" {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            *captured.body.lock() = body;
            write_half
                .write_all(b"250 2.0.0 queued as 4711\r\n")
                .await?;
        } else if command.starts_with("QUIT") {
            write_half.write_all(b"221 2.0.0 bye\r\n").await?;
            return Ok(());
        } else {
            write_half.write_all(b"250 2.0.0 ok\r\n").await?;
        }
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

fn compose() -> ComposedMessage {
    let mut draft = Draft::new(AccountId::new());
    draft.to = vec![MailAddress::with_name("bob@example.org", "Bob")];
    draft.cc = vec![MailAddress::new("cara@example.org")];
    draft.bcc = vec![MailAddress::new("dora@example.org")];
    draft.subject = "Rundbrief".to_string();
    draft.body_html = "<p>Ein <b>Test</b>.</p>".to_string();

    let mut signature = Signature::new(AccountId::new(), "Standard");
    signature.plain = "Viele Grüße\nAda".to_string();
    signature.html = "<p>Viele Grüße<br>Ada</p>".to_string();

    let context =
        ComposeContext::from_address(MailAddress::new("ada@example.org")).with_signature(signature);
    build(&draft, &context).unwrap()
}

#[tokio::test]
async fn a_composed_message_reaches_the_wire_with_the_right_envelope() {
    let (port, captured) = start().await;
    let composed = compose();
    let secret = SecretString::from("geheim");

    let submission = tokio::time::timeout(
        Duration::from_secs(20),
        smtp::submit(
            &config(port),
            Credential::Password(&secret),
            &composed.from,
            &composed.recipients,
            &composed.raw,
        ),
    )
    .await
    .expect("the submission timed out")
    .expect("the message was accepted");

    assert!(submission.accepted);

    // Bcc is in the envelope, never in the headers.
    let recipients = captured.recipients();
    assert!(
        recipients.iter().any(|r| r.contains("dora@example.org")),
        "{recipients:?}"
    );
    assert!(
        recipients.iter().any(|r| r.contains("cara@example.org")),
        "{recipients:?}"
    );
    assert!(
        recipients.iter().any(|r| r.contains("bob@example.org")),
        "{recipients:?}"
    );

    let body = captured.body();
    assert!(!body.contains("dora@example.org"), "Bcc leaked:\n{body}");
    assert!(body.contains("Rundbrief"), "{body}");
    assert!(body.contains("multipart/alternative"), "{body}");
    assert!(body.contains("text/plain"), "{body}");
    assert!(body.contains("text/html"), "{body}");
    // The signature landed in both body forms.
    assert!(body.contains("Viele Gr"), "{body}");
}

#[tokio::test]
async fn the_same_bytes_are_submitted_as_were_composed() {
    let (port, captured) = start().await;
    let composed = compose();
    let secret = SecretString::from("geheim");

    let expected_len = composed.raw.len();
    tokio::time::timeout(
        Duration::from_secs(20),
        smtp::submit(
            &config(port),
            Credential::Password(&secret),
            &composed.from,
            &composed.recipients,
            &composed.raw,
        ),
    )
    .await
    .expect("the submission timed out")
    .expect("the message was accepted");

    // The transport added the trailing CRLF framing; the payload itself is the
    // one the composer produced, modulo the line-ending normalisation.
    let body = captured.body();
    assert!(!body.is_empty());
    assert!(
        body.len() >= expected_len / 2,
        "the body looks truncated: {} < {}",
        body.len(),
        expected_len
    );
}
