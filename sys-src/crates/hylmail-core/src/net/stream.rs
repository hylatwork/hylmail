//! One stream type for every transport so that the protocol layers do not have
//! to be generic over TLS.
//!
//! [`async_imap::Client`] and lettre both want a concrete `AsyncRead +
//! AsyncWrite` type.  Instead of duplicating the IMAP client for the plain and
//! the encrypted case, [`Stream`] wraps both variants and forwards the two
//! `poll_*` methods — and it can be upgraded in place when `STARTTLS` is
//! negotiated, which is the whole reason the variant is not a type parameter.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

/// A connection to a mail server, with or without TLS.
#[derive(Debug)]
pub enum Stream {
    /// A clear text connection.  Only ever used for `TlsMode::None` (a local
    /// relay) or for the first bytes of a `STARTTLS` negotiation.
    Plain(TcpStream),
    /// A TLS protected connection.
    Tls(Box<TlsStream<TcpStream>>),
}

impl Stream {
    /// `true` when the payload is encrypted.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Tls(_))
    }

    /// The underlying TCP stream, also when TLS is already active.
    #[must_use]
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(stream) => stream,
            Self::Tls(stream) => stream.get_ref().0,
        }
    }

    /// The local address of the socket.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket is already closed.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.tcp().local_addr()
    }

    /// The peer address of the socket.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket is already closed.
    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.tcp().peer_addr()
    }

    /// The negotiated ALPN protocol, if any.
    #[must_use]
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        match self {
            Self::Plain(_) => None,
            Self::Tls(stream) => stream.get_ref().1.alpn_protocol(),
        }
    }

    /// The negotiated TLS version, e.g. `TLSv1.3`.
    #[must_use]
    pub fn tls_version(&self) -> Option<rustls::ProtocolVersion> {
        match self {
            Self::Plain(_) => None,
            Self::Tls(stream) => stream.get_ref().1.protocol_version(),
        }
    }

    /// Takes the clear text stream out of a `STARTTLS` connection.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Unsupported`] when TLS is already active,
    /// which would mean a second `STARTTLS` — a protocol error the caller must
    /// not paper over.
    pub fn into_plain(self) -> crate::error::Result<TcpStream> {
        match self {
            Self::Plain(stream) => Ok(stream),
            Self::Tls(_) => Err(crate::error::Error::Unsupported(
                "the connection is already encrypted, STARTTLS cannot run twice".to_string(),
            )),
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// A loopback pair: `Stream::Plain` on the client side, a bare `TcpStream`
    /// on the server side.
    async fn loopback_pair() -> (Stream, TcpStream) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move { listener.accept().await.expect("accept").0 });
        let client = TcpStream::connect(address).await.expect("connect");
        let server = server.await.expect("join");
        (Stream::Plain(client), server)
    }

    #[tokio::test]
    async fn a_plain_stream_forwards_reads_and_writes() {
        let (mut client, mut server) = loopback_pair().await;
        assert!(!client.is_encrypted());
        assert!(client.tls_version().is_none());
        assert!(client.alpn_protocol().is_none());

        client.write_all(b"PING\r\n").await.expect("write");
        let mut buffer = [0_u8; 6];
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut buffer)
            .await
            .expect("read");
        assert_eq!(&buffer, b"PING\r\n");

        server.write_all(b"PONG\r\n").await.expect("write");
        let mut buffer = [0_u8; 6];
        tokio::io::AsyncReadExt::read_exact(&mut client, &mut buffer)
            .await
            .expect("read");
        assert_eq!(&buffer, b"PONG\r\n");
    }

    #[tokio::test]
    async fn addresses_are_reachable_through_the_wrapper() {
        let (client, _server) = loopback_pair().await;
        assert!(client.local_addr().is_ok());
        assert!(client.peer_addr().is_ok());
        assert_eq!(
            client.peer_addr().unwrap().ip(),
            std::net::Ipv4Addr::LOCALHOST
        );
    }

    #[tokio::test]
    async fn a_plain_stream_can_be_unwrapped_once() {
        let (client, _server) = loopback_pair().await;
        let plain = client.into_plain().expect("a plain stream unwraps");
        assert!(plain.peer_addr().is_ok());
    }
}
