//! The local socket front ends connect to.
//!
//! One task per connection, each owning its own writer. Nothing here mutates
//! daemon state directly: requests go to the session, changes come back
//! through [`StateHub`], which is what keeps two clients from racing each
//! other into an inconsistent view.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use oxidezap_ipc::{ClientRequest, DaemonMessage, PROTOCOL_VERSION, socket_path};
use tokio::io::{AsyncBufReadExt, AsyncReadExt as _, AsyncWriteExt, BufReader};
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
    prepare_socket_dir(dir)?;

    // Bind first, and only treat the address as stale after proving nothing
    // answers on it. Unlinking first would let a second daemon steal the path
    // from a running one: the first keeps its already-connected clients while
    // every new client reaches the second, and two sessions then drive the
    // same account with neither aware of the other.
    match UnixListener::bind(path) {
        Ok(listener) => return Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
        Err(e) => return Err(e).with_context(|| format!("binding {}", path.display())),
    }

    if socket_is_live(path) {
        anyhow::bail!("another daemon is already listening on {}", path.display());
    }

    log::warn!("removing a stale socket at {}", path.display());
    std::fs::remove_file(path).context("removing a stale socket")?;
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

/// Whether something is accepting connections on `path`.
///
/// A blocking connect, deliberately: it runs once at startup before the
/// runtime has any work, and `ECONNREFUSED` is the answer that matters.
/// Anything else (a permission error, a path that is no longer a socket) is
/// treated as live, because refusing to start is recoverable while stealing a
/// live daemon's socket is not.
fn socket_is_live(path: &Path) -> bool {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::ConnectionRefused,
    }
}

/// Create the socket directory, or verify an existing one is safe to use.
///
/// The socket carries control of a WhatsApp session. Under `XDG_RUNTIME_DIR`
/// that is already a private per-user directory, but the `TMPDIR` fallback
/// sits in a world-writable place at a predictable path, where another local
/// user can pre-create it, or replace it with a symlink pointing somewhere
/// they can read. Creating it blindly and chmod-ing afterwards checks neither.
///
/// So: create it with the right mode from the start, and if it already exists,
/// refuse unless it is a real directory, owned by us, and inaccessible to
/// anyone else. Refusing to start is a bad outcome; putting a socket that
/// controls the account somewhere another user can reach is a worse one.
#[cfg(unix)]
fn prepare_socket_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }

    // `symlink_metadata`, not `metadata`: the latter follows the link, which
    // would report on the target and miss exactly the substitution this
    // guards against.
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;

    if !meta.is_dir() {
        anyhow::bail!(
            "{} exists but is not a directory; refusing to place the socket there",
            dir.display()
        );
    }
    if meta.uid() != current_uid() {
        anyhow::bail!(
            "{} is owned by uid {}, not by us; refusing to place the socket there",
            dir.display(),
            meta.uid()
        );
    }

    // Tighten rather than reject: a directory that is ours but too permissive
    // is recoverable, and this is the common case when an earlier version
    // created it.
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o700 {
        log::warn!("tightening {} from {mode:o} to 700", dir.display());
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid reads a process property, cannot fail and touches no
    // memory we own.
    unsafe { getuid() }
}

#[cfg(not(unix))]
fn prepare_socket_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

/// Longest frame a client may send.
///
/// `lines()` grows its buffer until it sees a newline, so a client that never
/// sends one would grow it without limit and take the WhatsApp session down
/// with the daemon. Requests are small; a megabyte is far past any legitimate
/// one and still cheap to refuse.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

async fn serve_client(stream: UnixStream, hub: Arc<StateHub>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader.take(MAX_REQUEST_BYTES)).lines();

    // Version first, state second. A client that cannot parse this daemon's
    // frames should never be handed a snapshot, and a daemon that cannot parse
    // that client's commands must not act on them.
    match lines.next_line().await? {
        Some(line) => {
            if let Some(rejection) = check_hello(&line) {
                write_line(&mut writer, &rejection).await?;
                return Ok(());
            }
        }
        None => return Ok(()),
    }

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

/// Validate the client's opening frame, returning a rejection to send when it
/// is not acceptable.
fn check_hello(line: &str) -> Option<String> {
    let request: ClientRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return serde_json::to_string(&DaemonMessage::Error(
                oxidezap_ipc::ProtocolError::Malformed {
                    detail: e.to_string(),
                },
            ))
            .ok();
        }
    };

    match request {
        ClientRequest::Hello { protocol } if protocol == PROTOCOL_VERSION => None,
        ClientRequest::Hello { protocol } => serde_json::to_string(&DaemonMessage::Error(
            oxidezap_ipc::ProtocolError::VersionMismatch {
                client: protocol,
                daemon: PROTOCOL_VERSION,
            },
        ))
        .ok(),
        _ => serde_json::to_string(&DaemonMessage::Error(
            oxidezap_ipc::ProtocolError::Malformed {
                detail: "first frame must be a hello".into(),
            },
        ))
        .ok(),
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
        // A second hello is harmless but says nothing; acknowledging keeps the
        // rule that every request gets exactly one answer.
        ClientRequest::Hello { .. } => serde_json::to_string(&DaemonMessage::Accepted).ok(),
        // Routing these into the session command channel is still to come.
        // Answering `Unsupported` rather than nothing is the difference
        // between a client that reports the gap and one that waits forever for
        // a reply that was never going to arrive.
        other => {
            let request = match other {
                ClientRequest::SendText { .. } => "send_text",
                ClientRequest::MarkRead { .. } => "mark_read",
                ClientRequest::ShowWindow => "show_window",
                ClientRequest::Shutdown => "shutdown",
                ClientRequest::Hello { .. } | ClientRequest::Snapshot => unreachable!(),
            };
            serde_json::to_string(&DaemonMessage::Unsupported {
                request: request.into(),
            })
            .ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_hello_is_accepted() {
        let line = serde_json::to_string(&ClientRequest::Hello {
            protocol: PROTOCOL_VERSION,
        })
        .unwrap();
        assert!(check_hello(&line).is_none(), "no rejection frame");
    }

    /// A client speaking another version must be turned away before it is
    /// handed a snapshot it cannot parse, and before the daemon acts on
    /// commands it may be misreading.
    #[test]
    fn a_mismatched_hello_is_rejected_with_both_versions() {
        let line = serde_json::to_string(&ClientRequest::Hello {
            protocol: PROTOCOL_VERSION + 1,
        })
        .unwrap();
        let reply: DaemonMessage = serde_json::from_str(&check_hello(&line).unwrap()).unwrap();
        match reply {
            DaemonMessage::Error(oxidezap_ipc::ProtocolError::VersionMismatch {
                client,
                daemon,
            }) => {
                assert_eq!(client, PROTOCOL_VERSION + 1);
                assert_eq!(daemon, PROTOCOL_VERSION);
            }
            other => panic!("expected a version mismatch, got {other:?}"),
        }
    }

    #[test]
    fn state_is_not_served_before_a_hello() {
        let line = serde_json::to_string(&ClientRequest::Snapshot).unwrap();
        let reply: DaemonMessage = serde_json::from_str(&check_hello(&line).unwrap()).unwrap();
        assert!(matches!(
            reply,
            DaemonMessage::Error(oxidezap_ipc::ProtocolError::Malformed { .. })
        ));
    }

    /// Every request gets exactly one answer. A command that parses but cannot
    /// be acted on yet must say so, or a client waiting for a reply waits for
    /// one that was never going to come.
    #[test]
    fn an_unroutable_command_is_answered_rather_than_ignored() {
        let hub = crate::state::StateHub::new();
        let line = serde_json::to_string(&ClientRequest::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
        })
        .unwrap();
        let reply: DaemonMessage =
            serde_json::from_str(&handle_request(&line, &hub).unwrap()).unwrap();
        assert!(matches!(
            reply,
            DaemonMessage::Unsupported { ref request } if request == "send_text"
        ));
    }

    #[test]
    fn a_malformed_frame_is_answered_and_does_not_end_the_connection() {
        let hub = crate::state::StateHub::new();
        let reply: DaemonMessage =
            serde_json::from_str(&handle_request("not json at all", &hub).unwrap()).unwrap();
        assert!(matches!(
            reply,
            DaemonMessage::Error(oxidezap_ipc::ProtocolError::Malformed { .. })
        ));
    }

    /// The fallback directory sits at a predictable path in a world-writable
    /// place, so a symlink planted there must not be followed.
    #[test]
    fn a_symlinked_socket_dir_is_refused() {
        let base = std::env::temp_dir().join(format!("oxidezap-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let target = base.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = base.join("sockdir");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = prepare_socket_dir(&link).expect_err("a symlink must be refused");
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected reason: {err}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A directory we already own is reused, and tightened if it is loose.
    #[test]
    fn a_loose_but_owned_dir_is_tightened_rather_than_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("oxidezap-loose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        prepare_socket_dir(&dir).expect("our own directory is usable");

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "left readable by other users");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this replaced: unlinking before binding let a second daemon
    /// steal a live one's path, leaving two sessions on one account.
    // tokio's UnixListener registers with the reactor, so binding needs a
    // runtime even though the rest of this check is synchronous.
    #[tokio::test]
    async fn binding_over_a_live_socket_fails_instead_of_stealing_it() {
        let dir = std::env::temp_dir().join(format!("oxidezap-bind-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("daemon.sock");
        let _ = std::fs::remove_file(&path);

        let first = bind(&path).expect("first bind succeeds");
        let second = bind(&path);
        assert!(second.is_err(), "a live socket must not be taken over");

        drop(first);
        // With the listener gone the path is stale, and reclaiming it is
        // exactly what lets a daemon restart after a crash.
        assert!(bind(&path).is_ok(), "a stale socket is reclaimed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
