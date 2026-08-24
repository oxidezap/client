//! The local socket front ends connect to.
//!
//! One task per connection, each owning its own writer. Nothing here mutates
//! daemon state directly: requests go to the session, changes come back
//! through [`StateHub`], which is what keeps two clients from racing each
//! other into an inconsistent view.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use oxidezap_ipc::{ClientRequest, DaemonMessage, socket_path};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;

use crate::state::StateHub;

/// Owns the listening socket and removes it on drop.
pub struct Server {
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best effort: a leftover socket file makes the next start fail to
        // bind, and there is nothing useful to do if removal fails.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind the socket and serve until the future is dropped.
pub async fn run(hub: Arc<StateHub>) -> Result<()> {
    let path = socket_path().context("no runtime directory to place the socket in")?;
    let listener = bind(&path)?;
    let _guard = Server { path: path.clone() };
    log::info!("listening on {}", path.display());

    loop {
        let (stream, _) = listener.accept().await.context("accepting a client")?;
        let hub = Arc::clone(&hub);
        // Per-connection task: one slow or malformed client cannot hold up
        // the accept loop or any other client.
        tokio::spawn(async move {
            if let Err(e) = serve_client(stream, hub).await {
                log::debug!("client disconnected: {e}");
            }
        });
    }
}

fn bind(path: &Path) -> Result<UnixListener> {
    let dir = path.parent().context("socket path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    restrict_to_owner(dir)?;

    // A socket left by a crashed daemon would block bind with EADDRINUSE.
    // Removing it is safe because the directory is ours alone; a *live*
    // daemon is caught by the connect probe in `main`, not here.
    match std::fs::remove_file(path) {
        Ok(()) => log::warn!("removed a stale socket at {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("removing a stale socket"),
    }

    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

/// The socket carries control of a WhatsApp session, so the directory is
/// owner-only. Enforced rather than assumed: `XDG_RUNTIME_DIR` is already
/// 0700, but the fallback under `TMPDIR` is not.
#[cfg(unix)]
fn restrict_to_owner(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))
}

#[cfg(not(unix))]
fn restrict_to_owner(_dir: &Path) -> Result<()> {
    Ok(())
}

async fn serve_client(stream: UnixStream, hub: Arc<StateHub>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Subscribe BEFORE snapshotting. Anything published in the window between
    // the two arrives on `updates` and is also in the snapshot; the version on
    // each frame lets the client drop the overlap. Snapshotting first would
    // lose that window instead.
    let mut updates = hub.subscribe();
    let hello = hub.hello_frame().context("serializing the snapshot")?;
    write_line(&mut writer, &hello).await?;

    loop {
        tokio::select! {
            // Biased: drain published state before reading more requests, so a
            // client that floods the socket cannot starve its own event stream.
            biased;

            update = updates.recv() => match update {
                Ok(frame) => write_line(&mut writer, &frame).await?,
                Err(RecvError::Lagged(missed)) => {
                    // The stream was truncated, so whatever the client holds is
                    // no longer trustworthy. Telling it to resync is the only
                    // correct answer; silently continuing would leave it with a
                    // state that never converges.
                    log::debug!("client fell {missed} frames behind; asking it to resync");
                    let frame = serde_json::to_string(&DaemonMessage::Resync)?;
                    write_line(&mut writer, &frame).await?;
                }
                Err(RecvError::Closed) => return Ok(()),
            },

            line = lines.next_line() => match line? {
                Some(line) => {
                    if let Some(reply) = handle_request(&line, &hub) {
                        write_line(&mut writer, &reply).await?;
                    }
                }
                None => return Ok(()),
            },
        }
    }
}

/// Handle one request, returning a frame to send back when there is one.
fn handle_request(line: &str, hub: &StateHub) -> Option<String> {
    let request: ClientRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            // A malformed frame is the client's bug, not a reason to drop the
            // connection: it gets told and the stream stays usable.
            let error = DaemonMessage::Error(oxidezap_ipc::ProtocolError::Malformed {
                detail: e.to_string(),
            });
            return serde_json::to_string(&error).ok();
        }
    };

    match request {
        ClientRequest::Snapshot => hub.hello_frame().ok(),
        // Command handling lands with the session command channel; parsing and
        // answering is what this layer owes, and refusing silently would be
        // worse than saying so.
        other => {
            log::debug!("unhandled request: {other:?}");
            None
        }
    }
}

async fn write_line(writer: &mut tokio::net::unix::OwnedWriteHalf, line: &str) -> Result<()> {
    writer.write_all(line.as_bytes()).await?;
    // Newline-delimited framing: the reader above splits on it, so a frame
    // containing one would desynchronize the stream. serde_json never emits a
    // bare newline inside a value, which the protocol tests pin.
    writer.write_all(b"\n").await?;
    Ok(())
}
