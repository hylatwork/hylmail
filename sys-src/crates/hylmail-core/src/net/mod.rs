//! Networking primitives shared by every protocol.
//!
//! Everything a mail client needs in order to *reach* a server lives here, so
//! that IMAP, SMTP and the phase 3 SOAP clients behave identically:
//!
//! * a single place that turns a [`TlsMode`] into an encrypted stream,
//! * mandatory timeouts on connect *and* handshake, because a mail client that
//!   hangs on a dead server is worse than one that reports an error,
//! * `TCP_NODELAY`, which matters more than usual here: IMAP is a
//!   request/response protocol with tiny payloads, and Nagle's algorithm adds
//!   a round trip to every command.
//!
//! [`ntlm`] is the exception to "reaching a server": it does not open a
//! connection, it authenticates on a connection someone else opened.

pub mod ntlm;
pub mod pem;
pub mod stream;
pub mod tls;

pub use stream::Stream;
pub use tls::{server_name, TlsPolicy};

use std::time::Duration;

use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::error::{Error, Result};
use crate::models::account::{Endpoint, TlsMode};

/// Default deadline for opening a TCP connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default deadline for the TLS handshake.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadlines applied to every network operation of a session.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Opening the TCP connection.
    pub connect: Duration,
    /// The TLS handshake.
    pub handshake: Duration,
    /// A single command/response exchange.
    pub command: Duration,
    /// Waiting for the server greeting.
    pub greeting: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: DEFAULT_CONNECT_TIMEOUT,
            handshake: DEFAULT_HANDSHAKE_TIMEOUT,
            // A slow server that is fetching a large mailbox can legitimately
            // need minutes; the per command timeout is a guard against a hung
            // socket, not a service level objective.
            command: Duration::from_secs(300),
            greeting: Duration::from_secs(30),
        }
    }
}

impl Timeouts {
    /// Fast deadlines for the connection test in the account dialog.
    #[must_use]
    pub const fn probe() -> Self {
        Self {
            connect: Duration::from_secs(10),
            handshake: Duration::from_secs(10),
            command: Duration::from_secs(20),
            greeting: Duration::from_secs(10),
        }
    }

    /// Seconds of [`Self::connect`], for the error message.
    #[must_use]
    pub const fn connect_seconds(&self) -> u64 {
        self.connect.as_secs()
    }
}

/// Runs `future`, mapping a missed deadline onto [`Error::Timeout`].
pub(crate) async fn with_deadline<F, T>(deadline: Duration, operation: &str, future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    match tokio::time::timeout(deadline, future).await {
        Ok(result) => result,
        Err(_) => Err(Error::timeout(operation, deadline.as_secs())),
    }
}

/// Opens a TCP connection to `host:port`.
///
/// # Errors
///
/// Returns [`Error::Timeout`] when the deadline passes and [`Error::Connect`]
/// when the OS refuses or the name cannot be resolved.
pub async fn connect_tcp(host: &str, port: u16, timeouts: &Timeouts) -> Result<TcpStream> {
    let endpoint = format!("{host}:{port}");
    let operation = format!("connecting to {endpoint}");
    let deadline = timeouts.connect;

    let stream = with_deadline(deadline, &operation, async {
        TcpStream::connect((host, port))
            .await
            .map_err(|source| Error::Connect {
                endpoint: endpoint.clone(),
                source,
            })
    })
    .await?;

    // IMAP exchanges are tiny and latency sensitive; without this every command
    // waits for a delayed ACK.
    if let Err(source) = stream.set_nodelay(true) {
        tracing::debug!(error = %source, "could not disable Nagle's algorithm");
    }

    Ok(stream)
}

/// Wraps a TCP stream in TLS.
///
/// # Errors
///
/// Returns [`Error::Timeout`] when the handshake does not finish in time,
/// [`Error::TlsConfig`] when `host` cannot be used as a server name and
/// [`Error::Tls`] when the handshake itself fails.
pub async fn upgrade(
    stream: TcpStream,
    host: &str,
    policy: &TlsPolicy,
    timeouts: &Timeouts,
) -> Result<Stream> {
    let name = server_name(host)?;
    let config = policy.client_config()?;
    let connector = TlsConnector::from(config);
    let operation = format!("the TLS handshake with {host}");
    let deadline = timeouts.handshake;

    if !policy.is_verifying() {
        tracing::warn!(
            host,
            "certificate verification is disabled for this connection"
        );
    }

    let stream = with_deadline(deadline, &operation, async {
        connector
            .connect(name, stream)
            .await
            .map_err(|error| Error::Tls {
                host: host.to_string(),
                message: error.to_string(),
            })
    })
    .await?;

    Ok(Stream::Tls(Box::new(stream)))
}

/// Connects to an endpoint according to its [`TlsMode`].
///
/// `TlsMode::StartTls` returns the *plain* stream: the caller has to send the
/// protocol's own `STARTTLS` command and then call [`start_tls`], because
/// only the protocol layer knows how to talk to the server at that point.
///
/// # Errors
///
/// See [`connect_tcp`] and [`upgrade`].
pub async fn connect(
    endpoint: &Endpoint,
    policy: &TlsPolicy,
    timeouts: &Timeouts,
) -> Result<Stream> {
    let stream = connect_tcp(&endpoint.host, endpoint.port, timeouts).await?;
    match endpoint.tls {
        TlsMode::None => Ok(Stream::Plain(stream)),
        TlsMode::Implicit => upgrade(stream, &endpoint.host, policy, timeouts).await,
        TlsMode::StartTls => Ok(Stream::Plain(stream)),
    }
}

/// Upgrades a clear text stream after a successful `STARTTLS` command.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when the stream is already encrypted and
/// otherwise the same errors as [`upgrade`].
pub async fn start_tls(
    stream: Stream,
    host: &str,
    policy: &TlsPolicy,
    timeouts: &Timeouts,
) -> Result<Stream> {
    let plain = stream.into_plain()?;
    upgrade(plain, host, policy, timeouts).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::account::Endpoint;
    use tokio::io::AsyncWriteExt;

    /// Test-only plumbing: a TLS server built from the committed fixture
    /// certificate (see `tests/fixtures/tls/README.md`).
    mod server {
        use super::*;

        const CERT: &str = include_str!("../../tests/fixtures/tls/localhost.crt.pem");
        const KEY: &str = include_str!("../../tests/fixtures/tls/localhost.key.pem");

        pub(super) fn config() -> std::sync::Arc<rustls::ServerConfig> {
            let certificates = pem::certificates(CERT).expect("fixture certificate");
            let block = pem::decode_blocks(KEY)
                .expect("fixture key")
                .into_iter()
                .next()
                .expect("one PEM block");
            let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
                rustls::pki_types::PrivatePkcs8KeyDer::from(block.der),
            );
            std::sync::Arc::new(
                rustls::ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(certificates, key)
                    .expect("server configuration"),
            )
        }

        /// Accepts one connection, performs the handshake **immediately** and
        /// then echoes every byte back.  Because neither side waits for a
        /// command before the handshake, this works for both implicit TLS and
        /// an explicit `STARTTLS` upgrade.
        pub(super) async fn spawn_echo() -> u16 {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let acceptor = tokio_rustls::TlsAcceptor::from(config());
            tokio::spawn(async move {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buffer = vec![0_u8; 4096];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut tls, &mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if tls.write_all(&buffer[..read]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            port
        }

        /// The fixture certificate as a PEM document, for
        /// [`TlsPolicy::with_extra_roots_pem`].
        pub(super) const fn certificate() -> &'static str {
            CERT
        }
    }

    async fn tls_policy_for_fixture() -> TlsPolicy {
        TlsPolicy::strict()
            .with_extra_roots_pem(server::certificate())
            .expect("extra root")
    }

    #[tokio::test]
    async fn an_implicit_tls_connection_verifies_against_an_extra_root() {
        let port = server::spawn_echo().await;
        let endpoint = Endpoint::new("localhost", port, TlsMode::Implicit);
        let policy = tls_policy_for_fixture().await;

        let mut stream = connect(&endpoint, &policy, &Timeouts::probe())
            .await
            .expect("handshake");
        assert!(stream.is_encrypted());
        assert!(matches!(
            stream.tls_version(),
            Some(rustls::ProtocolVersion::TLSv1_3) | Some(rustls::ProtocolVersion::TLSv1_2)
        ));
        assert!(stream.alpn_protocol().is_none());

        stream.write_all(b"PING\r\n").await.expect("write");
        let mut buffer = [0_u8; 6];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut buffer)
            .await
            .expect("read");
        assert_eq!(&buffer, b"PING\r\n");
    }

    #[tokio::test]
    async fn an_unknown_certificate_is_rejected() {
        let port = server::spawn_echo().await;
        let endpoint = Endpoint::new("localhost", port, TlsMode::Implicit);

        // The platform trust store does not know the fixture CA, so this must
        // fail: the test proves that verification is not silently disabled.
        let error = connect(&endpoint, &TlsPolicy::strict(), &Timeouts::probe())
            .await
            .unwrap_err();
        match &error {
            Error::Tls { host, .. } => assert_eq!(host, "localhost"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn an_insecure_policy_connects_anyway() {
        let port = server::spawn_echo().await;
        let endpoint = Endpoint::new("localhost", port, TlsMode::Implicit);
        let stream = connect(&endpoint, &TlsPolicy::insecure(), &Timeouts::probe())
            .await
            .expect("handshake");
        assert!(stream.is_encrypted());
    }

    #[tokio::test]
    async fn starttls_upgrades_a_clear_text_connection() {
        let port = server::spawn_echo().await;
        let endpoint = Endpoint::new("localhost", port, TlsMode::StartTls);
        let policy = tls_policy_for_fixture().await;

        let stream = connect(&endpoint, &policy, &Timeouts::probe())
            .await
            .expect("connect");
        assert!(!stream.is_encrypted(), "STARTTLS starts in clear text");

        let mut stream = start_tls(stream, "localhost", &policy, &Timeouts::probe())
            .await
            .expect("upgrade");
        assert!(stream.is_encrypted());
        stream.write_all(b"HI\r\n").await.expect("write");
        let mut buffer = [0_u8; 4];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut buffer)
            .await
            .expect("read");
        assert_eq!(&buffer, b"HI\r\n");
    }

    #[tokio::test]
    async fn starttls_refuses_to_run_twice() {
        let port = server::spawn_echo().await;
        let endpoint = Endpoint::new("localhost", port, TlsMode::Implicit);
        let policy = tls_policy_for_fixture().await;
        let stream = connect(&endpoint, &policy, &Timeouts::probe())
            .await
            .expect("handshake");

        let error = start_tls(stream, "localhost", &policy, &Timeouts::probe())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
    }

    #[tokio::test]
    async fn a_plain_endpoint_connects() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accept = tokio::spawn(async move { listener.accept().await.is_ok() });

        let endpoint = Endpoint::new("127.0.0.1", port, TlsMode::None);
        let stream = connect(&endpoint, &TlsPolicy::strict(), &Timeouts::probe())
            .await
            .expect("connect");
        assert!(!stream.is_encrypted());
        assert!(accept.await.expect("join"));
    }

    #[tokio::test]
    async fn starttls_endpoints_connect_in_clear_text_first() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accept = tokio::spawn(async move { listener.accept().await.is_ok() });

        let endpoint = Endpoint::new("127.0.0.1", port, TlsMode::StartTls);
        let stream = connect(&endpoint, &TlsPolicy::strict(), &Timeouts::probe())
            .await
            .expect("connect");
        assert!(!stream.is_encrypted());
        assert!(accept.await.expect("join"));
    }

    #[tokio::test]
    async fn a_refused_connection_is_reported_with_the_endpoint() {
        // Port 1 on loopback is reserved and never listening.
        let endpoint = Endpoint::new("127.0.0.1", 1, TlsMode::None);
        let error = connect(&endpoint, &TlsPolicy::strict(), &Timeouts::probe())
            .await
            .unwrap_err();
        match &error {
            Error::Connect { endpoint, .. } => assert_eq!(endpoint, "127.0.0.1:1"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn an_unresolvable_host_is_reported() {
        let endpoint = Endpoint::new("no-such-host.invalid", 993, TlsMode::None);
        let error = connect(&endpoint, &TlsPolicy::strict(), &Timeouts::probe())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Connect { .. } | Error::Timeout { .. }
        ));
    }

    #[tokio::test]
    async fn a_deadline_is_enforced() {
        let result: Result<()> = with_deadline(
            Duration::from_millis(5),
            "waiting for the impossible",
            async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            },
        )
        .await;
        let error = result.unwrap_err();
        assert!(matches!(error, Error::Timeout { seconds: 0, .. }));
        assert!(error.to_string().contains("waiting for the impossible"));
    }

    #[tokio::test]
    async fn an_implicit_tls_endpoint_against_a_plain_socket_fails_cleanly() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        // Accept and immediately close, so the handshake cannot complete.
        let accept = tokio::spawn(async move {
            drop(listener.accept().await);
        });

        let endpoint = Endpoint::new("127.0.0.1", port, TlsMode::Implicit);
        let error = connect(&endpoint, &TlsPolicy::strict(), &Timeouts::probe())
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Tls { .. } | Error::Timeout { .. }),
            "unexpected error: {error}"
        );
        let _ = accept.await;
    }

    #[test]
    fn timeouts_have_sane_defaults() {
        let timeouts = Timeouts::default();
        assert!(timeouts.connect <= Duration::from_secs(60));
        assert!(timeouts.handshake <= Duration::from_secs(60));
        assert!(Timeouts::probe().command < timeouts.command);
        assert_eq!(Timeouts::probe().connect_seconds(), 10);
    }
}
