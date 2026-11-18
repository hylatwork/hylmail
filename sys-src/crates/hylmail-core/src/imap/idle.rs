//! `IDLE`: being told about new mail instead of asking for it.
//!
//! RFC 2177 requires the client to re-issue `IDLE` at least every 30 minutes,
//! and imposes no upper bound on how long the server may stay silent.  Two
//! consequences shape this module:
//!
//! * the wait is always bounded by [`REISSUE_AFTER`], after which the caller
//!   ends the `IDLE` and starts a new one, so a server that ignores the
//!   recommendation cannot leave the client in a silent stalemate;
//! * the connection is **owned** by [`IdleGuard`] while idling, which is what
//!   makes it impossible to accidentally run a command in the middle of an
//!   `IDLE` — the classic source of "the client hangs after a while" bugs.
//!
//! Servers without `IDLE` are not an error: [`super::Capabilities::has_idle`] is
//! checked by the engine, which falls back to polling with `NOOP`.

use std::time::Duration;

use async_imap::extensions::idle::{Handle, IdleResponse};
use async_imap::Session;

use crate::error::{Error, Result};
use crate::net::Stream;

use super::connection::{ImapSession, SessionMeta};

/// A server may close an idle connection after 30 minutes, so the client
/// re-issues well before that.
pub const REISSUE_AFTER: Duration = Duration::from_secs(25 * 60);

/// How long a server is allowed to stay silent before the connection is
/// considered dead.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(29 * 60);

/// Something the server said while the client was idling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleEvent {
    /// The mailbox changed: new messages arrived or messages disappeared.
    MailboxChanged {
        /// New value of `EXISTS`, when the server announced one.
        exists: Option<u32>,
        /// Sequence number that was expunged, when the server announced one.
        expunged: Option<u32>,
        /// UID of a message whose flags changed, when the server announced one.
        flags_changed: Option<u32>,
    },
    /// A `* OK` keepalive or an unrelated notification.
    KeepAlive,
    /// Nothing arrived within the wait; the caller should re-issue `IDLE`.
    Timeout,
    /// The caller asked to stop idling.
    Interrupted,
    /// The server said goodbye; the connection has to be re-established.
    Bye,
}

impl IdleEvent {
    /// `true` when the local store has to be consulted again.
    #[must_use]
    pub fn needs_sync(&self) -> bool {
        matches!(self, Self::MailboxChanged { .. } | Self::Bye)
    }

    /// `true` when the `IDLE` has to be re-issued.
    #[must_use]
    pub fn needs_reissue(&self) -> bool {
        matches!(self, Self::Timeout | Self::Interrupted)
    }
}

/// Owns the connection for the duration of an `IDLE`.
///
/// Dropping the guard without calling [`IdleGuard::finish`] leaves the server
/// in the `IDLE` state; the socket then has to be closed, which is what happens
/// when the engine drops the whole session.
pub struct IdleGuard {
    handle: Option<Handle<Stream>>,
    host: String,
    meta: SessionMeta,
    /// Number of `IDLE` commands issued on this connection.
    rounds: u32,
}

impl std::fmt::Debug for IdleGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdleGuard")
            .field("host", &self.host)
            .field("rounds", &self.rounds)
            .finish()
    }
}

impl IdleGuard {
    /// Wraps a session that is about to idle.
    pub(crate) fn new(session: Session<Stream>, host: String, meta: SessionMeta) -> Self {
        Self {
            handle: Some(session.idle()),
            host,
            meta,
            rounds: 0,
        }
    }

    /// The host the connection belongs to.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// How often `IDLE` was issued on this connection.
    #[must_use]
    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    /// Sends `IDLE` and waits for the server's continuation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] when the server refuses `IDLE`, which is what
    /// servers do when a mailbox is selected read-only in a broken way.
    pub async fn start(&mut self) -> Result<()> {
        let handle = self.handle.as_mut().ok_or_else(|| {
            Error::protocol("IMAP", "IDLE", "the idle connection was already finished")
        })?;
        handle
            .init()
            .await
            .map_err(|error| Error::protocol("IMAP", "IDLE", error.to_string()))?;
        self.rounds = self.rounds.saturating_add(1);
        Ok(())
    }

    /// Waits for a notification, up to `timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Timeout`] only when the caller passes a zero timeout;
    /// a silent server yields [`IdleEvent::Timeout`], not an error.
    pub async fn wait(&mut self, timeout: Duration) -> Result<IdleEvent> {
        let handle = self.handle.as_mut().ok_or_else(|| {
            Error::protocol("IMAP", "IDLE", "the idle connection was already finished")
        })?;
        let (future, _stop) = handle.wait_with_timeout(timeout);
        match future.await {
            Ok(IdleResponse::NewData(data)) => Ok(classify(data.parsed())),
            Ok(IdleResponse::Timeout) => Ok(IdleEvent::Timeout),
            Ok(IdleResponse::ManualInterrupt) => Ok(IdleEvent::Interrupted),
            Err(error) => Err(Error::protocol("IMAP", "IDLE", error.to_string())),
        }
    }

    /// Waits with the re-issue deadline.
    ///
    /// # Errors
    ///
    /// See [`IdleGuard::wait`].
    pub async fn wait_reissue(&mut self) -> Result<IdleEvent> {
        self.wait(REISSUE_AFTER).await
    }

    /// Ends the `IDLE` and gives the connection back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] when the server refuses `DONE`.
    pub async fn finish(self) -> Result<ImapSession> {
        let Some(handle) = self.handle else {
            return Err(Error::protocol(
                "IMAP",
                "IDLE",
                "the idle connection was already finished",
            ));
        };
        let session = handle
            .done()
            .await
            .map_err(|error| Error::protocol("IMAP", "DONE", error.to_string()))?;
        Ok(ImapSession::from_parts(
            session,
            self.host,
            self.meta,
            self.rounds,
        ))
    }
}

/// Maps a parsed server response onto an [`IdleEvent`].
fn classify(response: &async_imap::imap_proto::Response<'_>) -> IdleEvent {
    use async_imap::imap_proto::{MailboxDatum, Response, Status};
    match response {
        Response::MailboxData(MailboxDatum::Exists(count)) => IdleEvent::MailboxChanged {
            exists: Some(*count),
            expunged: None,
            flags_changed: None,
        },
        Response::Expunge(sequence) => IdleEvent::MailboxChanged {
            exists: None,
            expunged: Some(*sequence),
            flags_changed: None,
        },
        Response::Fetch(sequence, _) => IdleEvent::MailboxChanged {
            exists: None,
            expunged: None,
            flags_changed: Some(*sequence),
        },
        Response::Done {
            status: Status::Bye,
            ..
        }
        | Response::Data {
            status: Status::Bye,
            ..
        } => IdleEvent::Bye,
        _ => IdleEvent::KeepAlive,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reissuing_happens_before_the_server_deadline() {
        assert!(REISSUE_AFTER < Duration::from_secs(30 * 60));
        assert!(REISSUE_AFTER < SILENCE_LIMIT);
    }

    #[test]
    fn events_describe_what_the_engine_has_to_do() {
        let changed = IdleEvent::MailboxChanged {
            exists: Some(4),
            expunged: None,
            flags_changed: None,
        };
        assert!(changed.needs_sync());
        assert!(!changed.needs_reissue());

        assert!(!IdleEvent::KeepAlive.needs_sync());
        assert!(IdleEvent::Timeout.needs_reissue());
        assert!(IdleEvent::Interrupted.needs_reissue());
        assert!(IdleEvent::Bye.needs_sync());
    }
}
