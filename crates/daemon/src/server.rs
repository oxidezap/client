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

    // Held for the whole run. Probe-remove-bind is three syscalls and cannot
    // be made atomic on its own: two daemons starting together can both see a
    // stale socket, and the second would then unlink the first's freshly bound
    // one and take its place. The lock makes that sequence exclusive, and
    // holding it afterwards is what makes a second daemon fail fast instead of
    // racing at all.
    let _lock = acquire_startup_lock(&path)?;
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

/// An exclusive lock on this user's daemon, released when the file closes.
struct StartupLock {
    _file: std::fs::File,
}

/// Take the per-user startup lock, or report who already holds it.
///
/// `flock` rather than a pid file: the kernel releases it when the process
/// dies however it dies, so a crashed daemon leaves nothing to clean up and
/// no stale pid to misread.
#[cfg(unix)]
fn acquire_startup_lock(socket: &Path) -> Result<StartupLock> {
    use std::os::unix::io::AsRawFd;

    let path = socket.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    // SAFETY: a valid fd from the handle above; flock touches no memory.
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(
            "another daemon holds {} ({err}); refusing to start a second session",
            path.display()
        );
    }

    Ok(StartupLock { _file: file })
}

#[cfg(not(unix))]
fn acquire_startup_lock(_socket: &Path) -> Result<StartupLock> {
    Ok(StartupLock {
        _file: std::fs::File::open(std::env::temp_dir()).context("opening a placeholder handle")?,
    })
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

/// Longest single frame a client may send.
///
/// Per frame, not per connection: a reader capped for its whole lifetime would
/// give a long-lived front end an artificial EOF once its small, valid
/// requests happened to add up. Requests are tiny; a megabyte is far past any
/// legitimate one and still cheap to refuse.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Read one newline-delimited frame, bounded independently of every other.
///
/// Returns `None` at end of stream. Reads bytes rather than lines because a
/// frame with invalid UTF-8 is a malformed frame the client can recover from,
/// not a reason to drop a connection: `next_line` would surface it as an I/O
/// error indistinguishable from a broken socket.
async fn read_frame(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    buf: &mut Vec<u8>,
) -> Result<Option<FrameRead>> {
    read_frame_generic(reader, buf).await
}

/// The body of [`read_frame`], over any reader, so the framing rules can be
/// tested without a socket.
///
/// `buf` is owned by the caller across calls, and is deliberately NOT cleared
/// on entry: `read_until` is not cancellation-safe and this future is one arm
/// of the connection's `select!`, so a read that loses the race is dropped
/// after having already consumed bytes off the socket. Those bytes are the
/// head of the next frame and exist nowhere else — clearing here would drop
/// them and hand the caller the tail of a request as if it were a whole one.
/// Only a frame that has been returned (or abandoned) empties the buffer.
async fn read_frame_generic<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
) -> Result<Option<FrameRead>> {
    loop {
        // The cap bounds a frame, so bytes carried over from a cancelled read
        // count against it: reading a frame in slices must not add up to an
        // unbounded one.
        let allowance = MAX_REQUEST_BYTES.saturating_sub(buf.len());
        let read = {
            let mut limited = reader.take(allowance as u64);
            limited.read_until(b'\n', buf).await?
        };

        if buf.last() == Some(&b'\n') {
            buf.pop();
            break;
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            // No newline within the cap: there is no way to tell where this
            // frame was meant to end, so nothing carries over.
            buf.clear();
            return Ok(Some(FrameRead::TooLong));
        }
        if read == 0 {
            // End of stream: an empty buffer is the ordinary close, and a
            // trailing frame that never got its newline is still a frame.
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
    }

    let frame = match std::str::from_utf8(buf) {
        Ok(line) => FrameRead::Line(line.to_string()),
        Err(_) => FrameRead::NotUtf8,
    };
    buf.clear();
    Ok(Some(frame))
}

/// The outcome of reading one frame.
#[derive(Debug)]
enum FrameRead {
    Line(String),
    /// Well-framed bytes that are not text. Answerable, so the connection
    /// survives a client with an encoding bug.
    NotUtf8,
    /// No newline within the cap. The stream cannot be resynchronized, since
    /// there is no way to tell where this frame was meant to end.
    TooLong,
}

async fn serve_client(stream: UnixStream, hub: Arc<StateHub>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::with_capacity(1024);

    // Version first, state second. A client that cannot parse this daemon's
    // frames should never be handed a snapshot, and a daemon that cannot parse
    // that client's commands must not act on them.
    match read_frame(&mut reader, &mut buf).await? {
        Some(FrameRead::Line(line)) => {
            if let Some(rejection) = check_hello(&line) {
                write_line(&mut writer, &rejection).await?;
                return Ok(());
            }
        }
        Some(_) | None => return Ok(()),
    }

    // Subscribe BEFORE snapshotting. Anything published in the window between
    // the two arrives on `updates` and is also in the snapshot; the version on
    // each frame lets the client drop the overlap. Snapshotting first would
    // lose that window instead.
    let mut updates = hub.subscribe();
    let hello = hub.hello_frame().context("serializing the snapshot")?;
    write_line(&mut writer, &hello).await?;

    // Set once the client has been told to resync. Until it asks for a
    // snapshot its view is known-stale, so further updates are worthless to it
    // and the request side takes priority: otherwise a continuous backlog
    // keeps `updates` ready forever and the recovery request is never read.
    let mut awaiting_resync = false;

    loop {
        tokio::select! {
            // Normally drain published state before reading more requests, so
            // a client that floods the socket cannot starve its own event
            // stream. After a lag that inverts, because recovery comes first.
            biased;

            update = updates.recv(), if !awaiting_resync => match update {
                Ok(frame) => write_line(&mut writer, &frame).await?,
                Err(RecvError::Lagged(missed)) => {
                    // The stream was truncated, so whatever the client holds is
                    // no longer trustworthy. Telling it to resync is the only
                    // correct answer; silently continuing would leave it with a
                    // state that never converges.
                    log::debug!("client fell {missed} frames behind; asking it to resync");
                    let frame = serde_json::to_string(&DaemonMessage::Resync)?;
                    write_line(&mut writer, &frame).await?;
                    awaiting_resync = true;
                }
                Err(RecvError::Closed) => return Ok(()),
            },

            frame = read_frame(&mut reader, &mut buf) => match frame? {
                Some(FrameRead::Line(line)) => {
                    if is_snapshot_request(&line) {
                        // Resubscribe BEFORE snapshotting, the same ordering
                        // the connection opened with. Reusing the old receiver
                        // would leave it at the cursor that already lagged, so
                        // a client recovering during heavy traffic would lag
                        // again immediately on events the new snapshot already
                        // covers, and loop through `Resync` forever.
                        updates = hub.subscribe();
                        awaiting_resync = false;
                    }
                    if let Some(reply) = handle_request(&line, &hub) {
                        write_line(&mut writer, &reply).await?;
                    }
                }
                Some(FrameRead::NotUtf8) => {
                    let frame = serde_json::to_string(&DaemonMessage::Error(
                        oxidezap_ipc::ProtocolError::Malformed {
                            detail: "frame was not valid UTF-8".into(),
                        },
                    ))?;
                    write_line(&mut writer, &frame).await?;
                }
                Some(FrameRead::TooLong) => {
                    // Unlike the other two this ends the connection: with no
                    // newline there is no way to know where the frame was meant
                    // to end, so the stream cannot be resynchronized.
                    let frame = serde_json::to_string(&DaemonMessage::Error(
                        oxidezap_ipc::ProtocolError::Malformed {
                            detail: format!("frame exceeded {MAX_REQUEST_BYTES} bytes"),
                        },
                    ))?;
                    write_line(&mut writer, &frame).await?;
                    return Ok(());
                }
                None => return Ok(()),
            },
        }
    }
}

/// Whether a frame is a snapshot request. Parsing it here only gates update
/// delivery; a frame that fails to parse is answered by `handle_request`.
fn is_snapshot_request(line: &str) -> bool {
    matches!(
        serde_json::from_str::<ClientRequest>(line),
        Ok(ClientRequest::Snapshot)
    )
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

    /// The cap is per frame, not per connection: a long-lived client sending
    /// small valid requests must never hit an artificial EOF because they
    /// added up.
    #[tokio::test]
    async fn the_size_cap_applies_to_each_frame_separately() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();

        // More total bytes than the cap, in frames far below it.
        let frame = "x".repeat(1000);
        let frames = (MAX_REQUEST_BYTES / 1000) + 10;
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            for _ in 0..frames {
                let _ = client.write_all(frame.as_bytes()).await;
                let _ = client.write_all(b"\n").await;
            }
        });

        for i in 0..frames {
            match read_frame_generic(&mut reader, &mut buf).await {
                Ok(Some(FrameRead::Line(line))) => assert_eq!(line.len(), 1000),
                other => panic!("frame {i} of {frames} was cut short: {other:?}"),
            }
        }
    }

    /// A frame that is not text is the client's bug, and it can recover from
    /// being told. Dropping the connection would take its valid requests with
    /// it.
    #[tokio::test]
    async fn invalid_utf8_is_a_recoverable_frame_not_a_dead_connection() {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();

        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let _ = client.write_all(&[0xff, 0xfe, b'\n']).await;
            let _ = client.write_all(b"{\"request\":\"snapshot\"}\n").await;
        });

        assert!(matches!(
            read_frame_generic(&mut reader, &mut buf).await,
            Ok(Some(FrameRead::NotUtf8))
        ));
        // The stream survives it.
        assert!(matches!(
            read_frame_generic(&mut reader, &mut buf).await,
            Ok(Some(FrameRead::Line(_)))
        ));
    }

    /// `read_until` is not cancellation-safe, and this read is one arm of the
    /// connection's `select!`: an update winning the race drops the future
    /// mid-frame. The bytes it already consumed are gone from the socket, so
    /// they have to survive in the buffer or the client's request comes back
    /// truncated — as a malformed frame at best, and as a different, valid
    /// request at worst.
    #[tokio::test]
    async fn a_cancelled_read_keeps_the_bytes_it_already_took() {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();

        use tokio::io::AsyncWriteExt as _;
        client.write_all(b"{\"request\":\"snap").await.unwrap();

        // No newline yet, so the read is still pending when the other arm of
        // the select wins and this future is dropped.
        tokio::select! {
            frame = read_frame_generic(&mut reader, &mut buf) => {
                panic!("a partial frame must not complete: {frame:?}")
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }

        client.write_all(b"shot\"}\n").await.unwrap();
        match read_frame_generic(&mut reader, &mut buf).await {
            Ok(Some(FrameRead::Line(line))) => assert!(
                is_snapshot_request(&line),
                "frame lost its head across the cancellation: {line}"
            ),
            other => panic!("expected the whole frame, got {other:?}"),
        }
    }

    /// The cap bounds a frame, so what a cancelled read left behind counts
    /// against it: carrying bytes over must not become a way to send an
    /// unbounded one in slices.
    #[tokio::test]
    async fn carried_over_bytes_still_count_against_the_cap() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();

        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let chunk = "x".repeat(8 * 1024);
            // Never a newline: this is one frame, and it outgrows the cap.
            for _ in 0..(MAX_REQUEST_BYTES / chunk.len()) + 2 {
                if client.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
            }
        });

        match read_frame_generic(&mut reader, &mut buf).await {
            Ok(Some(FrameRead::TooLong)) => {}
            Ok(Some(FrameRead::Line(line))) => {
                panic!("an unterminated frame parsed as a line of {}", line.len())
            }
            other => panic!("expected the cap to bite, got {other:?}"),
        }
        assert!(buf.is_empty(), "an abandoned frame must not carry over");
    }

    /// Two daemons starting together can both see a stale socket; the lock is
    /// what stops the second from unlinking the first's freshly bound one.
    #[test]
    fn the_startup_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("oxidezap-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("daemon.sock");

        let first = acquire_startup_lock(&socket).expect("first daemon takes the lock");
        assert!(
            acquire_startup_lock(&socket).is_err(),
            "a second daemon must not get in"
        );

        // Released with the handle, so a restart is not blocked by the last run.
        drop(first);
        assert!(
            acquire_startup_lock(&socket).is_ok(),
            "lock outlived its holder"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
