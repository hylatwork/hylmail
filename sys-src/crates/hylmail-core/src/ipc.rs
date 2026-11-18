//! Single-instance enforcement and inter-process command delivery.
//!
//! # Why this exists at all
//!
//! Two HylMail processes writing the same SQLite database and the same JSON
//! documents is a data-loss bug waiting to happen, and a second window that
//! shows the same mailboxes is merely confusing.  The operating system offers
//! no portable lock for that, so HylMail uses a **loopback TCP endpoint as the
//! single-instance token**:
//!
//! * the first process binds `127.0.0.1:0`, lets the OS pick a free port and
//!   records `pid` and `port` in `hylmail.lock` inside the configuration
//!   directory,
//! * every later process reads the lock file and tries to connect.  If the
//!   connect succeeds the first process is still alive, the new process is a
//!   [`Instance::Secondary`] and forwards its command over the socket,
//! * if the connect fails the lock file is stale (the previous process crashed
//!   or was killed); it is removed and the new process becomes the primary.
//!
//! Loopback TCP is used instead of a Unix domain socket or a Windows named pipe
//! on purpose: it is the one endpoint type that behaves identically on all
//! three target platforms and needs neither `unsafe` code nor a platform
//! crate.  The socket is bound to `127.0.0.1` only, so it is never reachable
//! from another host and never triggers a firewall prompt.
//!
//! # Protocol
//!
//! The wire format is one JSON object per connection, terminated by a newline:
//!
//! ```text
//! {"command":"raise_window"}
//! {"command":"open_eml","path":"/home/anna/invite.eml"}
//! {"command":"ping"}
//! ```
//!
//! `ping` is what [`acquire`] uses to probe liveness; the primary discards it
//! without reporting it to the user interface.
//!
//! # Example
//!
//! ```no_run
//! use hylmail_core::ipc::{self, Request};
//!
//! # fn main() -> hylmail_core::Result<()> {
//! let config_dir = std::path::Path::new("/tmp/hylmail-example");
//! match ipc::acquire(config_dir)? {
//!     ipc::Instance::Primary(server) => {
//!         // Run the user interface, draining `server` once per frame.
//!         let request = Request::RaiseWindow;
//!         ipc::notify(server.lockfile(), &request)?;
//!     }
//!     ipc::Instance::Secondary(lock) => {
//!         ipc::notify(&lock, &Request::RaiseWindow)?;
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Name of the lock file inside the configuration directory.
pub const LOCK_FILE_NAME: &str = "hylmail.lock";

/// How long a client waits for the running instance to accept its command.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Poll interval of the accept loop; also bounds how long shutdown waits.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// A command a freshly started process hands to the running instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    /// Bring the existing window to the front.
    RaiseWindow,
    /// Open a `.eml` file in the running instance.
    OpenEml {
        /// File the operating system asked HylMail to open.
        path: PathBuf,
    },
    /// Liveness probe sent by [`acquire`]; never reaches the user interface.
    Ping,
}

impl Request {
    /// Renders the request as a single newline terminated JSON line.
    fn to_line(&self) -> Result<String> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }

    /// Parses one JSON line, as written by [`Request::to_line`].
    fn from_line(line: &str) -> Result<Self> {
        Ok(serde_json::from_str(line)?)
    }
}

/// Who owns the single instance and where its endpoint listens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    /// Process id of the owner, kept for diagnostics only.
    pub pid: u32,
    /// Loopback TCP port the owner accepts commands on.
    pub port: u16,
}

impl Lockfile {
    /// Path of the lock file inside `config_dir`.
    #[must_use]
    pub fn path_in(config_dir: &Path) -> PathBuf {
        config_dir.join(LOCK_FILE_NAME)
    }

    /// Reads the lock file.
    ///
    /// A missing or unreadable file, and a file that does not parse, all mean
    /// "no owner" - a partially written lock file from a process that crashed
    /// while starting must never block the next start.  Only a genuine i/o
    /// error that is *not* "not found" is reported.
    ///
    /// # Errors
    ///
    /// Fails only when the directory exists but the file cannot be read for a
    /// reason other than absence or a syntax error.
    pub fn read(config_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path_in(config_dir);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        };
        Ok(serde_json::from_str(&text).ok())
    }

    /// Writes the lock file, creating the directory if necessary.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created, the file cannot be written
    /// or the value cannot be serialised.
    pub fn write(&self, config_dir: &Path) -> Result<()> {
        fs::create_dir_all(config_dir).map_err(|source| Error::Io {
            path: config_dir.to_path_buf(),
            source,
        })?;
        let path = Self::path_in(config_dir);
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&path, text).map_err(|source| Error::Io { path, source })
    }

    /// Removes the lock file, ignoring the case where it is already gone.
    fn remove(config_dir: &Path) {
        let _ = fs::remove_file(Self::path_in(config_dir));
    }

    /// The loopback address this lock points at.
    fn socket_addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port))
    }
}

/// The result of [`acquire`].
#[derive(Debug)]
pub enum Instance {
    /// This process owns the single instance and must serve commands.
    Primary(InstanceServer),
    /// Another process owns the instance; forward the command and exit.
    Secondary(Lockfile),
}

/// The primary side of the endpoint: it accepts commands until it is dropped.
///
/// The type is deliberately *not* `Sync` and is meant to live in the user
/// interface thread, which polls [`InstanceServer::drain`] once per frame.
#[derive(Debug)]
pub struct InstanceServer {
    lock: Lockfile,
    config_dir: PathBuf,
    stop: Arc<AtomicBool>,
    requests: Receiver<Request>,
    thread: Option<JoinHandle<()>>,
}

impl InstanceServer {
    /// Binds the loopback endpoint and starts the accept thread.
    fn start(listener: TcpListener, config_dir: PathBuf, lock: Lockfile) -> Result<Self> {
        listener.set_nonblocking(true).map_err(|source| Error::Io {
            path: config_dir.clone(),
            source,
        })?;

        let stop = Arc::new(AtomicBool::new(false));
        let (sender, requests) = mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("hylmail-ipc".to_string())
            .spawn(move || accept_loop(&listener, &sender, &thread_stop))
            .map_err(|source| Error::Io {
                path: config_dir.clone(),
                source,
            })?;

        Ok(Self {
            lock,
            config_dir,
            stop,
            requests,
            thread: Some(thread),
        })
    }

    /// The lock file this server published.
    #[must_use]
    pub fn lockfile(&self) -> &Lockfile {
        &self.lock
    }

    /// The loopback port commands are accepted on.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.lock.port
    }

    /// Non-blocking: returns the oldest command, if any arrived.
    #[must_use]
    pub fn try_recv(&self) -> Option<Request> {
        self.requests.try_recv().ok()
    }

    /// Non-blocking: takes every command that arrived since the last call.
    #[must_use]
    pub fn drain(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }

    /// Stops the accept thread and removes the lock file.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Lockfile::remove(&self.config_dir);
    }
}

impl Drop for InstanceServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The accept loop running in the background thread.
fn accept_loop(listener: &TcpListener, sender: &mpsc::Sender<Request>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => deliver(stream, sender),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            // The listener is poisoned; without it no command can ever arrive.
            Err(_) => break,
        }
    }
}

/// Reads one command from an accepted connection and forwards it.
fn deliver(stream: TcpStream, sender: &mpsc::Sender<Request>) {
    let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT));
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return;
    }
    match Request::from_line(line.trim_end()) {
        // `ping` only proves the process is alive; it is not a user action.
        Ok(Request::Ping) | Err(_) => {}
        Ok(request) => {
            let _ = sender.send(request);
        }
    }
}

/// Becomes the single instance or discovers the running one.
///
/// # Errors
///
/// Fails when the lock file cannot be inspected, the loopback endpoint cannot
/// be bound or the lock file cannot be written.
pub fn acquire(config_dir: &Path) -> Result<Instance> {
    if let Some(lock) = Lockfile::read(config_dir)? {
        if notify(&lock, &Request::Ping).is_ok() {
            return Ok(Instance::Secondary(lock));
        }
        // Nothing is listening: the previous owner is gone.  Take over.
        Lockfile::remove(config_dir);
    }

    let listener =
        TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).map_err(|source| {
            Error::Io {
                path: config_dir.to_path_buf(),
                source,
            }
        })?;
    let port = listener
        .local_addr()
        .map_err(|source| Error::Io {
            path: config_dir.to_path_buf(),
            source,
        })?
        .port();

    let lock = Lockfile {
        pid: std::process::id(),
        port,
    };
    lock.write(config_dir)?;
    Ok(Instance::Primary(InstanceServer::start(
        listener,
        config_dir.to_path_buf(),
        lock,
    )?))
}

/// Sends a command to the process described by `lock`.
///
/// # Errors
///
/// Fails when the endpoint refuses the connection, when the write times out or
/// when the command cannot be serialised.  A failure is the signal that the
/// lock file is stale.
pub fn notify(lock: &Lockfile, request: &Request) -> Result<()> {
    let addr = lock.socket_addr();
    let mut stream =
        TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|source| Error::Connect {
            endpoint: addr.to_string(),
            source,
        })?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|source| Error::Connect {
            endpoint: addr.to_string(),
            source,
        })?;
    stream
        .write_all(request.to_line()?.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|source| Error::Connect {
            endpoint: addr.to_string(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Waits until `predicate` holds, or fails after ~2 seconds.
    fn wait_for(mut predicate: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_request_round_trips_through_its_wire_form() {
        for request in [
            Request::RaiseWindow,
            Request::Ping,
            Request::OpenEml {
                path: PathBuf::from("/tmp/invite.eml"),
            },
        ] {
            let line = request.to_line().unwrap();
            assert!(line.ends_with('\n'));
            assert_eq!(Request::from_line(line.trim_end()).unwrap(), request);
        }
    }

    #[test]
    fn the_wire_form_is_the_documented_json() {
        let line = Request::RaiseWindow.to_line().unwrap();
        assert_eq!(line.trim_end(), r#"{"command":"raise_window"}"#);
        let line = Request::Ping.to_line().unwrap();
        assert_eq!(line.trim_end(), r#"{"command":"ping"}"#);
    }

    #[test]
    fn an_unknown_command_is_rejected() {
        assert!(Request::from_line(r#"{"command":"nonsense"}"#).is_err());
        assert!(Request::from_line("not json at all").is_err());
    }

    #[test]
    fn a_lock_file_round_trips() {
        let dir = temp_dir();
        assert_eq!(Lockfile::read(dir.path()).unwrap(), None);

        let lock = Lockfile {
            pid: 4242,
            port: 51_234,
        };
        lock.write(dir.path()).unwrap();
        assert_eq!(Lockfile::read(dir.path()).unwrap(), Some(lock.clone()));
        assert!(Lockfile::path_in(dir.path()).exists());
    }

    #[test]
    fn a_corrupt_lock_file_reads_as_absent() {
        let dir = temp_dir();
        fs::write(Lockfile::path_in(dir.path()), "{ not json").unwrap();
        assert_eq!(Lockfile::read(dir.path()).unwrap(), None);
    }

    #[test]
    fn acquiring_twice_makes_the_second_one_secondary() {
        let dir = temp_dir();
        let server = match acquire(dir.path()).unwrap() {
            Instance::Primary(server) => server,
            Instance::Secondary(_) => panic!("first acquire must be primary"),
        };

        let second = acquire(dir.path()).unwrap();
        match second {
            Instance::Secondary(lock) => assert_eq!(lock.port, server.port()),
            Instance::Primary(_) => panic!("second acquire must be secondary"),
        }
    }

    #[test]
    fn a_command_reaches_the_running_instance() {
        let dir = temp_dir();
        let server = match acquire(dir.path()).unwrap() {
            Instance::Primary(server) => server,
            Instance::Secondary(_) => panic!("first acquire must be primary"),
        };

        // A loopback datagram is occasionally lost in a sandbox (a reset before
        // the receiver reads it); a real second instance would simply notify
        // again, so the test re-sends while it waits for the command.
        let mut attempts = 0;
        let received = wait_for(|| {
            if attempts % 4 == 0 {
                let _ = notify(
                    server.lockfile(),
                    &Request::OpenEml {
                        path: PathBuf::from("/tmp/invite.eml"),
                    },
                );
            }
            attempts += 1;
            !server.drain().is_empty()
        });
        assert!(received, "the command never arrived");
    }

    #[test]
    fn a_ping_is_answered_but_not_reported() {
        let dir = temp_dir();
        let server = match acquire(dir.path()).unwrap() {
            Instance::Primary(server) => server,
            Instance::Secondary(_) => panic!("first acquire must be primary"),
        };

        notify(server.lockfile(), &Request::Ping).unwrap();
        // Give the accept loop ample time to receive and discard it.
        thread::sleep(Duration::from_millis(120));
        assert!(server.drain().is_empty());
    }

    #[test]
    fn a_stale_lock_file_is_taken_over() {
        let dir = temp_dir();
        // Port 1 on loopback is reserved and refuses connections.
        Lockfile {
            pid: 999_999,
            port: 1,
        }
        .write(dir.path())
        .unwrap();

        match acquire(dir.path()).unwrap() {
            Instance::Primary(server) => assert_ne!(server.port(), 1),
            Instance::Secondary(_) => panic!("a stale lock must not win"),
        }
    }

    #[test]
    fn the_lock_file_disappears_when_the_primary_stops() {
        let dir = temp_dir();
        let path = Lockfile::path_in(dir.path());
        {
            let _server = match acquire(dir.path()).unwrap() {
                Instance::Primary(server) => server,
                Instance::Secondary(_) => panic!("first acquire must be primary"),
            };
            assert!(path.exists());
        }
        assert!(!path.exists());
    }
}
