//! The daemon's side of the local transport.
//!
//! A Unix socket where there is one, a named pipe on Windows. The two differ
//! in more than a type name — a socket is a filesystem entry that outlives a
//! crash, a pipe is a name that does not — so the differences are gathered
//! here rather than spread through the server, which is written once against
//! [`Listener`] and never mentions either.
//!
//! The client half lives in `oxidezap_ipc::Endpoint`, where a front end can
//! reach it.

use anyhow::{Context, Result};

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
        let pending = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .with_context(|| format!("creating {name}"))?;
        Ok(Self {
            pending: Some(pending),
        })
    }

    /// Wait for the next client.
    #[cfg(unix)]
    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        self.inner.accept().await.map(|(stream, _)| stream)
    }

    #[cfg(windows)]
    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        let name = crate::server::endpoint_name()?;
        let server = match self.pending.take() {
            Some(server) => server,
            None => tokio::net::windows::named_pipe::ServerOptions::new().create(&name)?,
        };
        server.connect().await?;
        // The next instance before handing this one over, so the name is
        // never momentarily unserved.
        self.pending = Some(tokio::net::windows::named_pipe::ServerOptions::new().create(&name)?);
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

/// Whether something is accepting connections on `path`.
///
/// A blocking connect, deliberately: it runs once at startup before the
/// runtime has any work, and `ECONNREFUSED` is the answer that matters.
/// Anything else (a permission error, a path that is no longer a socket) is
/// treated as live, because refusing to start is recoverable while stealing a
/// live daemon's socket is not.
#[cfg(unix)]
fn socket_is_live(path: &std::path::Path) -> bool {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::ConnectionRefused,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
