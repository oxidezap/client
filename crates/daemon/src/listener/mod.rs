//! The daemon's side of the local transport.
//!
//! A Unix socket where there is one, a named pipe on Windows. The two differ
//! in more than a type name — a socket is a filesystem entry that outlives a
//! crash, a pipe is a name that does not — so the differences are gathered
//! here rather than spread through the server, which is written once against
//! [`Listener`] and never mentions either.
//!
//! A third endpoint lives in [`web`]: a page can open neither of the above,
//! so it gets a loopback TCP port speaking the same protocol over a
//! WebSocket. It is here rather than beside the server for the same reason
//! the other two are — /AGENTS.md keeps every server-side transport in this
//! directory, so the protocol above them is written once.
//!
//! The client half lives in `oxidezap_ipc::endpoint`, where a front end can
//! reach it.

use anyhow::{Context, Result};

#[cfg(windows)]
mod security;
#[cfg(test)]
mod transport_tests;
/// The endpoint a browser can reach. Off unless asked for; see its own
/// documentation for why that matters.
pub mod web;

/// One accepted connection.
#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;

#[cfg(windows)]
pub type Stream = tokio::net::windows::named_pipe::NamedPipeServer;

/// Accepts connections, and cleans up after itself.
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    /// The instance waiting for the next client.
    ///
    /// A named pipe serves one client per instance, so there is always one
    /// created and idle: a client that connects between instances gets
    /// `ERROR_FILE_NOT_FOUND` rather than waiting, which would look to it like
    /// no daemon at all.
    #[cfg(windows)]
    pending: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    /// The name this listener bound, kept rather than derived again.
    ///
    /// `accept` creates the next instance, and deriving the name a second
    /// time makes the two agree only for as long as every caller passes the
    /// same path. Diverging would put the first client on one pipe and every
    /// later one on another — the two-endpoint state this module exists to
    /// prevent.
    #[cfg(windows)]
    name: String,
    /// Only a Unix socket leaves something behind to remove.
    #[cfg(unix)]
    path: std::path::PathBuf,
}

impl Listener {
    /// Start listening, reclaiming a stale endpoint from a crashed daemon.
    ///
    /// The startup lock is already held by the time this runs — see
    /// `server::claim` — which is what makes reclaiming safe to do at all.
    #[cfg(unix)]
    pub fn bind(path: &std::path::Path) -> Result<Self> {
        // Bind first, and only treat the address as stale after proving
        // nothing answers on it. Unlinking first would let a second daemon
        // steal the path from a running one: the first keeps its
        // already-connected clients while every new client reaches the second,
        // and two sessions then drive the same account with neither aware of
        // the other.
        let inner = match tokio::net::UnixListener::bind(path) {
            Ok(listener) => listener,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if socket_is_live(path) {
                    anyhow::bail!("another daemon is already listening on {}", path.display());
                }
                log::warn!("removing a stale socket at {}", path.display());
                std::fs::remove_file(path).context("removing a stale socket")?;
                tokio::net::UnixListener::bind(path)
                    .with_context(|| format!("binding {}", path.display()))?
            }
            Err(e) => return Err(e).with_context(|| format!("binding {}", path.display())),
        };
        Ok(Self {
            inner,
            path: path.to_path_buf(),
        })
    }

    /// Start listening.
    ///
    /// `first_pipe_instance` is the exclusion: creating the first instance of
    /// a name another process already owns fails, which is the same answer a
    /// bound socket gives. Nothing is left behind when the process dies, so
    /// there is no stale endpoint to reclaim.
    #[cfg(windows)]
    pub fn bind(path: &std::path::Path) -> Result<Self> {
        let name = path.to_string_lossy().into_owned();
        let pending = create_pipe(&name, true).with_context(|| format!("creating {name}"))?;
        Ok(Self {
            pending: Some(pending),
            name,
        })
    }

    /// Wait for the next client.
    #[cfg(unix)]
    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        self.inner.accept().await.map(|(stream, _)| stream)
    }

    #[cfg(windows)]
    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        let server = match self.pending.take() {
            Some(server) => server,
            None => create_pipe(&self.name, false)?,
        };
        server.connect().await?;
        // The next instance before handing this one over, so the name is
        // never momentarily unserved.
        self.pending = Some(create_pipe(&self.name, false)?);
        Ok(server)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Best effort, and only where there is something to remove: a
        // leftover socket file makes the next start reclaim rather than bind,
        // and there is nothing useful to do if removal fails.
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One pipe instance, readable and writable by this user alone.
///
/// The default security descriptor grants read access to `Everyone` and the
/// anonymous account, and this endpoint carries the session stream. Every
/// instance gets the restricted descriptor, not just the first: they are
/// separate objects and a client can land on any of them.
#[cfg(windows)]
fn create_pipe(name: &str, first: bool) -> std::io::Result<Stream> {
    let security = security::UserOnly::new()?;
    let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
    options.first_pipe_instance(first);
    // SAFETY: `security` lives across the call, and the pointer is to a
    // `SECURITY_ATTRIBUTES` it keeps valid for exactly that long. The kernel
    // copies the descriptor into the object it creates.
    unsafe { options.create_with_security_attributes_raw(name, security.as_ptr()) }
}

/// How long the probe waits for an answer before deciding one way.
///
/// A local connect is microseconds; anything past this is not a socket
/// answering slowly, it is one whose backlog is full and which nobody is
/// accepting from.
#[cfg(unix)]
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether something is accepting connections on `path`.
///
/// `ECONNREFUSED` is the answer that matters. Anything else — a permission
/// error, a path that is no longer a socket, no answer at all — is treated as
/// live, because refusing to start is recoverable while stealing a live
/// daemon's socket is not.
///
/// Off this thread and bounded, because the comment this replaces was no
/// longer true: it said the probe runs before the runtime has any work, and
/// by now `server::run` has the tray, the plugins and the session bridge
/// started. A connect that does not come back — Linux answers a full backlog
/// with `EAGAIN`, other unices make the caller wait — would stop the daemon
/// here with not even a "listening" line to say where.
#[cfg(unix)]
fn socket_is_live(path: &std::path::Path) -> bool {
    let (answer, asked) = std::sync::mpsc::channel();
    let path = path.to_path_buf();
    // Detached: if the connect ever returns, the send fails and the thread
    // ends. There is one of these per start, and the alternative to leaving
    // it is waiting on it.
    std::thread::spawn(move || {
        let live = match std::os::unix::net::UnixStream::connect(&path) {
            Ok(_) => true,
            Err(e) => e.kind() != std::io::ErrorKind::ConnectionRefused,
        };
        let _ = answer.send(live);
    });
    asked.recv_timeout(PROBE_TIMEOUT).unwrap_or(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The shape this guards against: a process holding the path and never
    /// accepting from it. The probe has to answer either way, and it must
    /// answer "live" — refusing to start is recoverable, taking a running
    /// daemon's socket is not.
    #[test]
    fn a_socket_nobody_accepts_from_does_not_stop_the_probe() {
        let dir = std::env::temp_dir().join(format!("oxidezap-probe-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("daemon.sock");
        let _ = std::fs::remove_file(&path);

        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        // Past any plausible backlog, and held open so none of them drain.
        let mut held = Vec::new();
        for _ in 0..600 {
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }

        let started = wacore::time::Instant::now();
        assert!(
            socket_is_live(&path),
            "no answer is not a reason to take a live daemon's socket"
        );
        assert!(
            started.elapsed() < PROBE_TIMEOUT * 4,
            "the probe answers rather than waiting on the kernel"
        );

        drop(held);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this replaced: unlinking before binding let a second daemon
    /// steal a live one's path, leaving two sessions on one account.
    ///
    /// There is nothing to reclaim on Windows: a named pipe is a name the
    /// kernel drops when its owner dies, so a crashed daemon leaves no stale
    /// endpoint behind.
    // tokio's UnixListener registers with the reactor, so binding needs a
    // runtime even though the rest of this check is synchronous.
    #[tokio::test]
    async fn binding_over_a_live_endpoint_fails_instead_of_stealing_it() {
        let dir = std::env::temp_dir().join(format!("oxidezap-bind-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("daemon.sock");
        let _ = std::fs::remove_file(&path);

        let first = Listener::bind(&path).expect("first bind succeeds");
        assert!(
            Listener::bind(&path).is_err(),
            "a live endpoint must not be taken over"
        );

        drop(first);
        // With the listener gone the path is stale, and reclaiming it is
        // exactly what lets a daemon restart after a crash.
        assert!(Listener::bind(&path).is_ok(), "a stale socket is reclaimed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
