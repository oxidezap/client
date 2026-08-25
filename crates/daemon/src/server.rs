//! The local socket front ends connect to.
//!
//! One task per connection, each owning its own writer. Nothing here mutates
//! daemon state directly: requests go to the session, changes come back
//! through [`StateHub`], which is what keeps two clients from racing each
//! other into an inconsistent view.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use oxidezap_ipc::{ClientRequest, DaemonMessage, PROTOCOL_VERSION, ProtocolError, socket_path};
use tokio::io::{AsyncBufReadExt, AsyncReadExt as _, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;

use crate::session_bridge::{Action, CommandOutcome, Commands, Outbox, SessionCommand};
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

/// This process's claim on being *the* daemon for this user.
///
/// Taken before anything touches the account. Holding it is what makes a
/// second daemon fail fast instead of racing the first.
pub struct Claim {
    path: PathBuf,
    _lock: StartupLock,
}

/// Prepare the socket directory and take the per-user lock.
///
/// Separate from [`run`], and called first, for two reasons that both come
/// down to ordering:
///
/// * The directory has to exist, and be verified as ours, *before* the lock
///   file inside it is opened. Opening the lock first fails with `ENOENT` on
///   a first start, and under the `TMPDIR` fallback it would also create a
///   path before the checks that decide whether that path is safe.
/// * The lock has to be held before the session starts. The socket is only
///   the visible half of "one daemon per user"; the real invariant is one
///   WhatsApp session over one SQLite file. A second process that opened the
///   store and connected before discovering the lock was taken would have
///   already broken it.
pub fn claim() -> Result<Claim> {
    let path = socket_path().context("no runtime directory to place the socket in")?;
    let dir = path.parent().context("socket path has no parent")?;
    prepare_socket_dir(dir)?;
    let lock = acquire_startup_lock(&path)?;
    Ok(Claim { path, _lock: lock })
}

/// How long a client has to send its hello.
///
/// A connection that never speaks holds a task and a file descriptor for as
/// long as it stays open, and the accept loop keeps taking more. A front end
/// on the same machine that cannot manage its opening frame in this long is
/// not going to manage it at all.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How many front ends may be connected at once.
///
/// Far above any real desktop, which runs one window and perhaps a notifier,
/// and low enough that a reconnect loop in a broken client cannot exhaust the
/// daemon's descriptors. Turned away with an error frame rather than a
/// silently dropped connection, so the client can tell why.
pub const MAX_CLIENTS: usize = 32;

/// How many frames may queue for one connection's own answers.
///
/// Only downloads land here, and a front end asks for as many as it has
/// visible media. Past this the answer is dropped rather than parking the
/// download task, and the client retries — which costs nothing, because the
/// bytes are already in the cache.
const OUTBOX_CAPACITY: usize = 64;

/// Serve until the future is dropped.
///
/// Borrows the claim rather than taking it: this future is a `select!` branch
/// and can be dropped while the session is still disconnecting, and the lock
/// has to outlive that. Handing it over here would release it mid-teardown,
/// which is exactly the window a second daemon must not find open.
pub async fn run(claim: &Claim, hub: Arc<StateHub>, commands: Commands) -> Result<()> {
    let path = claim.path.clone();
    let listener = bind(&path)?;
    let _guard = Server { path: path.clone() };
    log::info!("listening on {}", path.display());

    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CLIENTS));

    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            // Per-connection failures, not listener failures: the peer went
            // away between the SYN and the accept, or the process is briefly
            // out of descriptors. Tearing down the WhatsApp session over one
            // of these would turn a transient condition into an outage, and a
            // supervisor restarting us would land in the same state.
            Err(e) if is_transient_accept_error(&e) => {
                log::warn!("skipping a connection we could not accept: {e}");
                // Without this, an EMFILE that persists spins the loop at
                // full speed; the descriptors it is waiting on are freed by
                // other tasks, which need to be scheduled.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            Err(e) => return Err(e).context("accepting a client"),
        };

        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            tokio::spawn(reject(stream));
            continue;
        };

        let hub = Arc::clone(&hub);
        let commands = commands.clone();
        // Per-connection task: one slow or malformed client cannot hold up
        // the accept loop or any other client.
        tokio::spawn(async move {
            if let Err(e) = serve_client(stream, hub, commands).await {
                log::debug!("client disconnected: {e}");
            }
            drop(slot);
        });
    }
}

/// Whether an `accept` failure describes one connection rather than the
/// listener.
fn is_transient_accept_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::Interrupted | ErrorKind::WouldBlock
    ) || matches!(e.raw_os_error(), Some(EMFILE | ENFILE))
}

/// Out of descriptors, for this process and for the machine. Spelled out
/// because neither has an `std::io::ErrorKind`: both land in
/// `Uncategorized`, which is unstable to match on.
const EMFILE: i32 = 24;
const ENFILE: i32 = 23;

/// Tell a client we are full, then close.
///
/// Spawned rather than written inline: the accept loop must not wait on a
/// peer. The task is still bounded — one small frame into a socket nobody has
/// had a chance to fill, then done — so a refused client costs a write, not a
/// slot.
async fn reject(stream: UnixStream) {
    log::warn!("refusing a client: already serving {MAX_CLIENTS}");
    let (_, mut writer) = stream.into_split();
    if let Ok(frame) = serde_json::to_string(&DaemonMessage::Error(ProtocolError::TooManyClients {
        limit: MAX_CLIENTS,
    })) {
        let _ = write_line(&mut writer, &frame).await;
    }
}

/// Bind the socket, reclaiming a stale one from a crashed daemon.
///
/// The directory is already prepared and the startup lock already held: see
/// [`claim`], which is why this can treat the path as ours alone.
fn bind(path: &Path) -> Result<UnixListener> {
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
    let path = socket.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    // rustix rather than a hand-rolled `extern "C"`: the same syscall,
    // without an `unsafe` block and without redeclaring `LOCK_EX`/`LOCK_NB`
    // as local constants that nothing checks against the platform.
    if let Err(e) = rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        anyhow::bail!(
            "another daemon holds {} ({e}); refusing to start a second session",
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
    rustix::process::getuid().as_raw()
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
///
/// # Cancellation
///
/// `buf` belongs to the connection, not to one call, and this future is a
/// `select!` branch that loses races with the update stream. `read_until`
/// keeps whatever it consumed before being dropped, so those bytes are the
/// front of a frame that has not arrived yet: the next call continues from
/// them instead of starting over. Clearing `buf` on entry — the obvious
/// shape — would silently eat the head of every request that happened to be
/// in flight when a chat update landed, and the client would see its command
/// answered with a parse error it did not cause.
async fn read_frame(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    buf: &mut Vec<u8>,
) -> Result<Option<FrameRead>> {
    read_frame_generic(reader, buf).await
}

/// The body of [`read_frame`], over any reader, so the framing rules can be
/// tested without a socket.
async fn read_frame_generic<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
) -> Result<Option<FrameRead>> {
    // What is already here is a prefix a cancelled call left behind, and it
    // counts against this frame's budget: the cap is per frame, and a frame
    // read across three cancellations is still one frame.
    let carried = buf.len();
    if carried >= MAX_REQUEST_BYTES {
        buf.clear();
        return Ok(Some(FrameRead::TooLong));
    }

    let read = {
        let mut limited = reader.take((MAX_REQUEST_BYTES - carried) as u64);
        limited.read_until(b'\n', buf).await?
    };

    if read == 0 {
        // End of stream. A carried prefix here is a frame the client never
        // finished sending, and there is nobody left to answer.
        buf.clear();
        return Ok(None);
    }

    if buf.last() != Some(&b'\n') {
        // `read_until` stops at the delimiter, at the cap, or at EOF. Without
        // a delimiter it was one of the other two, and they are not the same
        // answer: a frame that hit the cap leaves a client that is still
        // there and deserves to be told, while one that hit EOF is a client
        // that went away mid-frame. Acting on the partial bytes is not an
        // option either way — the framing says where a frame ends, and this
        // one never said.
        let hit_the_cap = buf.len() == MAX_REQUEST_BYTES;
        buf.clear();
        return Ok(hit_the_cap.then_some(FrameRead::TooLong));
    }

    buf.pop();
    let frame = match std::str::from_utf8(buf) {
        Ok(line) => FrameRead::Line(line.to_string()),
        Err(_) => FrameRead::NotUtf8,
    };
    // Cleared here, at the end of a complete frame, rather than at the start
    // of the next call: only a cancelled read may leave anything behind.
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
    /// there is no way to tell where this frame was meant to end, so this
    /// ends the connection — unlike the other two, which the client recovers
    /// from.
    TooLong,
}

async fn serve_client(stream: UnixStream, hub: Arc<StateHub>, commands: Commands) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::with_capacity(1024);

    // Version first, state second. A client that cannot parse this daemon's
    // frames should never be handed a snapshot, and a daemon that cannot parse
    // that client's commands must not act on them.
    //
    // Bounded, because until this succeeds the connection has cost the daemon
    // a task and a descriptor and given nothing back. A peer that connects
    // and says nothing would otherwise sit here for as long as it liked, and
    // a reconnect loop doing it would take the listener down with it.
    let attached = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake(&mut reader, &mut writer, &mut buf),
    )
    .await
    {
        Ok(result) => match result? {
            Some(attached) => attached,
            None => return Ok(()),
        },
        Err(_) => {
            log::debug!("client never completed its handshake within {HANDSHAKE_TIMEOUT:?}");
            let frame = malformed("no hello within the handshake window")?;
            // Best effort: a peer that never spoke may not be reading either.
            let _ = write_line(&mut writer, &frame).await;
            return Ok(());
        }
    };

    // Subscribe BEFORE snapshotting. Anything published in the window between
    // the two arrives on `updates` and is also in the snapshot; the version on
    // each frame lets the client drop the overlap. Snapshotting first would
    // lose that window instead.
    let mut updates = hub.subscribe();
    // Never resubscribed and never paused, unlike `updates`: a window request
    // dropped here is gone for good, since it carries no version and no
    // snapshot contains it.
    let mut signals = hub.subscribe_signals();
    // Subscribed before the reload is asked for, and for the same reason the
    // snapshot is taken after subscribing: the load must not land in the
    // window between the two.
    let mut sessions = if attached.session_events {
        hub.subscribe_sessions()
    } else {
        // A receiver whose sender is already gone, so the branch below can be
        // unconditional rather than an `Option` inside a `select!`. It costs
        // the daemon nothing: with no real subscriber it never serializes a
        // session event in the first place.
        tokio::sync::broadcast::channel(1).1
    };

    // Frames addressed to this connection alone: a download's answer belongs
    // to whoever asked, and the ids are client-chosen.
    let (outbox, mut inbox) = tokio::sync::mpsc::channel::<String>(OUTBOX_CAPACITY);

    let hello = hub.hello_frame().context("serializing the snapshot")?;
    write_line(&mut writer, &hello).await?;

    if attached.session_events {
        // Nothing in the store has changed, so the session's invalidation
        // stream has nothing to say and this client would sit empty until the
        // next message arrived. Asked for after the subscription so the load
        // it produces cannot be missed.
        let _ = dispatch(&hub, &commands, Action::ReloadHistory).await;
    }

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

            // Neither of these is gated on `awaiting_resync`: a session
            // event is not a summary, and a download this client asked for is
            // its own answer. Both are lost for good if dropped.
            session = sessions.recv() => match session {
                Ok(frame) => write_line(&mut writer, &frame).await?,
                // A front end that overruns cannot patch the gap from a
                // snapshot: it holds messages, not summaries. Telling it to
                // resync is the only answer, and it reloads history when it
                // reattaches.
                Err(RecvError::Lagged(missed)) => {
                    log::debug!("front end fell {missed} session events behind");
                    let frame = serde_json::to_string(&DaemonMessage::Resync)?;
                    write_line(&mut writer, &frame).await?;
                }
                Err(RecvError::Closed) => return Ok(()),
            },

            Some(frame) = inbox.recv() => write_line(&mut writer, &frame).await?,

            // Not gated on `awaiting_resync`. A client recovering its state
            // is exactly when the tray's Open item is most likely to be
            // clicked, and there is nothing here for a snapshot to restore.
            signal = signals.recv() => match signal {
                Ok(frame) => write_line(&mut writer, &frame).await?,
                // Nothing to converge: these are news, not state, and a
                // client that missed one has missed one. Said out loud so it
                // is not mistaken for a state gap.
                Err(RecvError::Lagged(missed)) => {
                    log::debug!("client missed {missed} pass-through frames");
                }
                Err(RecvError::Closed) => return Ok(()),
            },

            // Cancellation-safe: `read_frame` carries a partial frame in
            // `buf` across losing this race. See its documentation.
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
                    let answer = handle_request(&line, &hub, &commands, &outbox).await;
                    if let Some(frame) = answer.frame {
                        write_line(&mut writer, &frame).await?;
                    }
                    if answer.shutdown {
                        // After the write, not before. Signalling first lets
                        // the daemon tear this task down mid-answer, and a
                        // client that asked politely to stop the daemon would
                        // see EOF where the protocol promised it a reply.
                        crate::shutdown::request("ipc client");
                        return Ok(());
                    }
                }
                Some(FrameRead::NotUtf8) => {
                    write_line(&mut writer, &not_utf8()?).await?;
                }
                Some(FrameRead::TooLong) => {
                    // Unlike the other two this ends the connection: with no
                    // newline there is no way to know where the frame was meant
                    // to end, so the stream cannot be resynchronized.
                    let frame = malformed(&format!("frame exceeded {MAX_REQUEST_BYTES} bytes"))?;
                    write_line(&mut writer, &frame).await?;
                    return Ok(());
                }
                None => return Ok(()),
            },
        }
    }
}

/// Read frames until one is an acceptable hello.
///
/// Returns whether the connection may proceed. A frame that is not text is
/// answered and waited past rather than closed on, for the same reason it is
/// after the handshake: an encoding bug is something a client can be told
/// about and recover from, and a client that is told nothing cannot tell a
/// rejected hello from a dead socket. The caller bounds the whole thing in
/// time, which is what stops that leniency from being a way to hold a slot
/// open forever.
async fn handshake(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    buf: &mut Vec<u8>,
) -> Result<Option<Attached>> {
    loop {
        match read_frame(reader, buf).await? {
            Some(FrameRead::Line(line)) => match check_hello(&line) {
                Ok(attached) => return Ok(Some(attached)),
                Err(rejection) => {
                    if let Some(rejection) = rejection {
                        write_line(writer, &rejection).await?;
                    }
                    return Ok(None);
                }
            },
            Some(FrameRead::NotUtf8) => write_line(writer, &not_utf8()?).await?,
            Some(FrameRead::TooLong) => {
                let frame = malformed(&format!("frame exceeded {MAX_REQUEST_BYTES} bytes"))?;
                write_line(writer, &frame).await?;
                return Ok(None);
            }
            None => return Ok(None),
        }
    }
}

fn malformed(detail: &str) -> Result<String> {
    Ok(serde_json::to_string(&DaemonMessage::Error(
        ProtocolError::Malformed {
            detail: detail.into(),
        },
    ))?)
}

fn not_utf8() -> Result<String> {
    malformed("frame was not valid UTF-8")
}

/// Whether a frame is a snapshot request. Parsing it here only gates update
/// delivery; a frame that fails to parse is answered by `handle_request`.
fn is_snapshot_request(line: &str) -> bool {
    matches!(
        serde_json::from_str::<ClientRequest>(line),
        Ok(ClientRequest::Snapshot)
    )
}

/// What an accepted hello asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Attached {
    /// Whether this client wants the session's own events as well as
    /// summaries. See [`ClientRequest::Hello`].
    session_events: bool,
}

/// Validate the client's opening frame.
///
/// `Err` carries the rejection to send; `Ok` carries what the client asked to
/// be served.
fn check_hello(line: &str) -> Result<Attached, Option<String>> {
    let request: ClientRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Err(malformed(&e.to_string()).ok()),
    };

    match request {
        ClientRequest::Hello {
            protocol,
            session_events,
        } if protocol == PROTOCOL_VERSION => Ok(Attached { session_events }),
        ClientRequest::Hello { protocol, .. } => Err(serde_json::to_string(&DaemonMessage::Error(
            ProtocolError::VersionMismatch {
                client: protocol,
                daemon: PROTOCOL_VERSION,
            },
        ))
        .ok()),
        _ => Err(malformed("first frame must be a hello").ok()),
    }
}

/// What the connection does with one request.
struct Answer {
    /// The frame to send back, if there is one. Every request has one today;
    /// the option is what keeps a future fire-and-forget request from having
    /// to invent an acknowledgement.
    frame: Option<String>,
    /// Whether to stop the daemon once that frame is on the wire.
    shutdown: bool,
}

impl Answer {
    fn frame(frame: Option<String>) -> Self {
        Self {
            frame,
            shutdown: false,
        }
    }
}

/// Handle one request.
///
/// Every request gets exactly one answer, including the ones that fail: a
/// client waiting on a reply that was never going to arrive is worse than a
/// client told no.
async fn handle_request(
    line: &str,
    hub: &StateHub,
    commands: &Commands,
    outbox: &Outbox,
) -> Answer {
    let request: ClientRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            // A malformed frame is the client's bug, not a reason to drop the
            // connection: it gets told and the stream stays usable.
            return Answer::frame(malformed(&e.to_string()).ok());
        }
    };

    match request {
        ClientRequest::Snapshot => Answer::frame(hub.hello_frame().ok()),
        // A second hello is harmless but says nothing; acknowledging keeps the
        // rule that every request gets exactly one answer.
        ClientRequest::Hello { .. } => Answer::frame(accepted()),
        ClientRequest::SendText {
            jid,
            text,
            local_id,
        } => Answer::frame(
            dispatch(
                hub,
                commands,
                Action::SendText {
                    jid,
                    text,
                    local_id,
                },
            )
            .await,
        ),
        ClientRequest::SendAudio {
            jid,
            upload,
            duration_secs,
            waveform,
            local_id,
        } => Answer::frame(
            dispatch(
                hub,
                commands,
                Action::SendAudio {
                    jid,
                    upload,
                    duration_secs,
                    waveform,
                    local_id,
                },
            )
            .await,
        ),
        ClientRequest::Typing { jid, composing } => {
            Answer::frame(dispatch(hub, commands, Action::Typing { jid, composing }).await)
        }
        ClientRequest::Call(action) => {
            Answer::frame(dispatch(hub, commands, Action::Call(action)).await)
        }
        ClientRequest::Download { id, media } => Answer::frame(
            dispatch(
                hub,
                commands,
                Action::Download {
                    id,
                    media,
                    answer_to: outbox.clone(),
                },
            )
            .await,
        ),
        ClientRequest::ReloadHistory => {
            Answer::frame(dispatch(hub, commands, Action::ReloadHistory).await)
        }
        // Not gated on being connected: dead credentials are exactly when the
        // account is unreachable, and refusing the only recovery then would
        // leave the user with no way out.
        ClientRequest::ForgetSession => Answer::frame(
            match commands.try_send(SessionCommand {
                action: Action::ForgetSession,
                reply: {
                    let (reply, _) = tokio::sync::oneshot::channel();
                    reply
                },
            }) {
                Ok(()) => accepted(),
                Err(_) => no_session("the session is shutting down"),
            },
        ),
        ClientRequest::MarkRead {
            jid,
            through_message_id,
        } => Answer::frame(
            dispatch(
                hub,
                commands,
                Action::MarkRead {
                    jid,
                    through_message_id,
                },
            )
            .await,
        ),
        // The daemon has no window of its own, so this is relayed rather than
        // acted on: whoever owns a window is the only one that can raise it.
        // Published to every client, including the one that asked, because a
        // front end that sent this on a user's behalf wants the window up
        // regardless of which process is holding it.
        ClientRequest::ShowWindow => {
            hub.signal(&DaemonMessage::ShowWindow);
            Answer::frame(accepted())
        }
        // The acknowledgement goes out first; see the caller.
        ClientRequest::Shutdown => Answer {
            frame: accepted(),
            shutdown: true,
        },
    }
}

/// Hand a command to the session and wait for what became of it.
///
/// Waiting, rather than answering on admission to the queue, is what makes
/// `Accepted` mean something: the account can drop between the check here and
/// the moment the bridge picks the command up, and a client told yes on
/// admission would never learn its message went nowhere. It is also the
/// backpressure — a connection has one command outstanding at a time, so the
/// client cap is also the cap on queued work.
async fn dispatch(hub: &StateHub, commands: &Commands, action: Action) -> Option<String> {
    // Refused early as well as late: a client that is watching the connection
    // state should get the answer it can already predict, without the round
    // trip.
    let connection = hub.connection();
    if !connection.is_connected() {
        return no_session(&format!("not connected: {connection:?}"));
    }

    let (reply, answer) = tokio::sync::oneshot::channel();
    if commands
        .send(SessionCommand { action, reply })
        .await
        .is_err()
    {
        // The bridge is gone: the daemon is on its way down.
        return no_session("the session is shutting down");
    }

    match answer.await {
        Ok(CommandOutcome::Accepted) => accepted(),
        Ok(CommandOutcome::NoSession(detail)) => no_session(&detail),
        Ok(CommandOutcome::Refused(detail)) => refused(&detail),
        // The bridge took the command and died before answering.
        Err(_) => no_session("the session stopped before it answered"),
    }
}

fn accepted() -> Option<String> {
    serde_json::to_string(&DaemonMessage::Accepted).ok()
}

fn no_session(detail: &str) -> Option<String> {
    serde_json::to_string(&DaemonMessage::Error(ProtocolError::NoSession {
        detail: detail.into(),
    }))
    .ok()
}

fn refused(detail: &str) -> Option<String> {
    serde_json::to_string(&DaemonMessage::Error(ProtocolError::Refused {
        detail: detail.into(),
    }))
    .ok()
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

    fn hello(protocol: u32, session_events: bool) -> String {
        serde_json::to_string(&ClientRequest::Hello {
            protocol,
            session_events,
        })
        .unwrap()
    }

    #[test]
    fn a_matching_hello_is_accepted() {
        assert_eq!(
            check_hello(&hello(PROTOCOL_VERSION, false)),
            Ok(Attached {
                session_events: false
            })
        );
    }

    /// The session stream is opt-in: a tray that never asked must not be sent
    /// every message in the account.
    #[test]
    fn the_session_stream_is_only_served_when_asked_for() {
        assert_eq!(
            check_hello(&hello(PROTOCOL_VERSION, true)),
            Ok(Attached {
                session_events: true
            })
        );
        // An older client that does not know the field at all still connects,
        // and gets summaries.
        let line = format!(r#"{{"request":"hello","protocol":{PROTOCOL_VERSION}}}"#);
        assert_eq!(
            check_hello(&line),
            Ok(Attached {
                session_events: false
            })
        );
    }

    /// A client speaking another version must be turned away before it is
    /// handed a snapshot it cannot parse, and before the daemon acts on
    /// commands it may be misreading.
    #[test]
    fn a_mismatched_hello_is_rejected_with_both_versions() {
        let rejection = check_hello(&hello(PROTOCOL_VERSION + 1, false))
            .expect_err("a mismatch is turned away");
        let reply: DaemonMessage = serde_json::from_str(&rejection.unwrap()).unwrap();
        match reply {
            DaemonMessage::Error(ProtocolError::VersionMismatch { client, daemon }) => {
                assert_eq!(client, PROTOCOL_VERSION + 1);
                assert_eq!(daemon, PROTOCOL_VERSION);
            }
            other => panic!("expected a version mismatch, got {other:?}"),
        }
    }

    #[test]
    fn state_is_not_served_before_a_hello() {
        let line = serde_json::to_string(&ClientRequest::Snapshot).unwrap();
        let rejection = check_hello(&line).expect_err("anything else is turned away");
        let reply: DaemonMessage = serde_json::from_str(&rejection.unwrap()).unwrap();
        assert!(matches!(
            reply,
            DaemonMessage::Error(ProtocolError::Malformed { .. })
        ));
    }

    /// A connected session: the state a command is allowed to run against.
    fn connected_hub() -> Arc<StateHub> {
        let hub = StateHub::new();
        hub.apply(crate::state::Change::live(
            oxidezap_ipc::DaemonEvent::ConnectionChanged(oxidezap_ipc::ConnectionState::Connected),
        ));
        hub
    }

    fn request_line(request: &ClientRequest) -> String {
        serde_json::to_string(request).unwrap()
    }

    /// A connection's own answer channel. Tests that do not read it only
    /// need it to exist.
    fn outbox() -> Outbox {
        tokio::sync::mpsc::channel(OUTBOX_CAPACITY).0
    }

    fn parse(frame: Option<String>) -> DaemonMessage {
        serde_json::from_str(&frame.expect("every request gets an answer")).unwrap()
    }

    /// A stand-in bridge: takes one command and answers it. The join handle
    /// yields what it was asked to do, so a test can assert on both halves.
    fn bridge(outcome: CommandOutcome) -> (Commands, tokio::task::JoinHandle<Option<Action>>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(MAX_CLIENTS);
        let task = tokio::spawn(async move {
            let SessionCommand { action, reply } = rx.recv().await?;
            let _ = reply.send(outcome);
            Some(action)
        });
        (tx, task)
    }

    /// The follow-up this replaced: these parsed and were answered
    /// `Unsupported`. They now reach the session.
    #[tokio::test]
    async fn a_command_reaches_the_session_rather_than_being_refused() {
        let hub = connected_hub();
        let (commands, taken) = bridge(CommandOutcome::Accepted);

        let line = request_line(&ClientRequest::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
            local_id: None,
        });
        let answer = handle_request(&line, &hub, &commands, &outbox()).await;
        assert_eq!(parse(answer.frame), DaemonMessage::Accepted);
        assert!(!answer.shutdown);
        assert!(matches!(
            taken.await.unwrap(),
            Some(Action::SendText { jid, text, .. }) if jid == "a@s.whatsapp.net" && text == "hi"
        ));
    }

    /// `Accepted` has to mean the session took it, not that a queue did. The
    /// account can drop between the check at the door and the moment the
    /// bridge picks the command up, and a client told yes on admission alone
    /// would never learn its message went nowhere.
    #[tokio::test]
    async fn a_refusal_at_execution_time_reaches_the_client() {
        // Connected as far as this connection can see, and refused anyway:
        // exactly the race the answer channel exists for.
        let hub = connected_hub();
        let (commands, _taken) = bridge(CommandOutcome::Refused("has moved on".into()));

        let line = request_line(&ClientRequest::MarkRead {
            jid: "a@s.whatsapp.net".into(),
            through_message_id: None,
        });
        let answer = handle_request(&line, &hub, &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Error(ProtocolError::Refused { ref detail })
                if detail == "has moved on"
        ));
    }

    /// The other way a command can come back: the account went away while the
    /// request was in the bridge's hands. A different answer, because a client
    /// can see that state coming and wait it out rather than change anything.
    #[tokio::test]
    async fn a_session_lost_mid_command_is_reported_as_such() {
        let hub = connected_hub();
        let (commands, _taken) = bridge(CommandOutcome::NoSession("not connected".into()));

        let line = request_line(&ClientRequest::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
            local_id: None,
        });
        let answer = handle_request(&line, &hub, &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Error(ProtocolError::NoSession { .. })
        ));
    }

    /// Accepting a send the account cannot carry out would answer `Accepted`
    /// and then fail out of sight, where the client can never learn of it.
    #[tokio::test]
    async fn a_command_is_refused_while_there_is_no_session_to_carry_it() {
        // Fresh hub: `Connecting`, which is what a daemon looks like before it
        // has an account and after it loses one.
        let hub = StateHub::new();
        let (commands, taken) = bridge(CommandOutcome::Accepted);

        let line = request_line(&ClientRequest::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
            local_id: None,
        });
        let answer = handle_request(&line, &hub, &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Error(ProtocolError::NoSession { .. })
        ));

        drop(commands);
        assert!(
            taken.await.unwrap().is_none(),
            "nothing was queued behind the no"
        );
    }

    /// The acknowledgement has to be on the wire before the daemon is asked
    /// to stop, or the shutdown races the answer and a client that asked
    /// politely sees EOF where the protocol promised it a reply. Signalling
    /// is therefore the caller's job, after the write.
    #[tokio::test]
    async fn a_shutdown_is_acknowledged_before_it_is_carried_out() {
        let hub = connected_hub();
        let (commands, _taken) = bridge(CommandOutcome::Accepted);

        let answer = handle_request(
            &request_line(&ClientRequest::Shutdown),
            &hub,
            &commands,
            &outbox(),
        )
        .await;
        assert_eq!(parse(answer.frame), DaemonMessage::Accepted);
        assert!(answer.shutdown, "and only then is the daemon asked to stop");
    }

    /// A frame that does not parse is the client's bug, not a reason to drop
    /// its connection: it gets told, and its next request still works.
    #[tokio::test]
    async fn a_malformed_frame_is_answered_and_does_not_end_the_connection() {
        let hub = StateHub::new();
        let (commands, _taken) = bridge(CommandOutcome::Accepted);
        let answer = handle_request("not json at all", &hub, &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Error(ProtocolError::Malformed { .. })
        ));
        assert!(!answer.shutdown);
    }

    /// The daemon owns no window, so this is a relay: whoever has one is the
    /// only party that can raise it.
    #[tokio::test]
    async fn a_window_request_is_published_rather_than_acted_on() {
        let hub = StateHub::new();
        let (commands, taken) = bridge(CommandOutcome::Accepted);
        let mut signals = hub.subscribe_signals();

        let line = request_line(&ClientRequest::ShowWindow);
        let answer = handle_request(&line, &hub, &commands, &outbox()).await;
        assert_eq!(parse(answer.frame), DaemonMessage::Accepted);

        let frame: DaemonMessage = serde_json::from_str(&signals.recv().await.unwrap()).unwrap();
        assert_eq!(frame, DaemonMessage::ShowWindow);

        drop(commands);
        assert!(
            taken.await.unwrap().is_none(),
            "the session has no part in a window"
        );
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

    /// The reader is a `select!` branch, so it loses races with the update
    /// stream mid-frame. What it already consumed has to survive that, or a
    /// client's command comes back as a parse error for a frame it sent
    /// correctly — and only when the account happened to be busy.
    #[tokio::test]
    async fn a_frame_interrupted_by_an_update_is_not_lost() {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();

        client.write_all(b"{\"request\":\"snap").await.unwrap();

        // The read polls first, consumes what is there, and then parks
        // because the frame is not finished; the ready branch wins.
        tokio::select! {
            biased;
            frame = read_frame_generic(&mut reader, &mut buf) => {
                panic!("an unterminated frame must not complete: {frame:?}");
            }
            () = std::future::ready(()) => {}
        }
        assert!(!buf.is_empty(), "the prefix was consumed and kept");

        client.write_all(b"shot\"}\n").await.unwrap();
        match read_frame_generic(&mut reader, &mut buf).await {
            Ok(Some(FrameRead::Line(line))) => {
                assert!(
                    matches!(
                        serde_json::from_str::<ClientRequest>(&line),
                        Ok(ClientRequest::Snapshot)
                    ),
                    "the frame reassembled as it was sent: {line}"
                );
            }
            other => panic!("the prefix was dropped: {other:?}"),
        }
    }

    /// The cap covers a frame, not a read. Letting a carried prefix start the
    /// budget over would make the limit a suggestion: a client could send a
    /// megabyte at a time forever and never trip it.
    #[tokio::test]
    async fn a_carried_prefix_still_counts_against_the_cap() {
        let (client, server) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(server);
        // As if a cancelled read had already consumed a full frame's worth.
        let mut buf = vec![b'x'; MAX_REQUEST_BYTES];

        assert!(matches!(
            read_frame_generic(&mut reader, &mut buf).await,
            Ok(Some(FrameRead::TooLong))
        ));
        assert!(buf.is_empty(), "a refused frame leaves nothing behind");
        drop(client);
    }

    /// An encoding bug in the opening frame is as recoverable as one after it.
    /// Closing on it silently leaves the client unable to tell a rejected
    /// hello from a dead socket.
    #[tokio::test]
    async fn a_hello_that_is_not_text_is_answered_rather_than_dropped() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (reader, mut writer) = server.into_split();
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();

        let hello = hello(PROTOCOL_VERSION, false);
        client.write_all(&[0xff, 0xfe, b'\n']).await.unwrap();
        client.write_all(hello.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();

        assert!(
            handshake(&mut reader, &mut writer, &mut buf)
                .await
                .unwrap()
                .is_some(),
            "the client recovered and was let in"
        );

        let mut answer = String::new();
        BufReader::new(client).read_line(&mut answer).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonMessage>(&answer).unwrap(),
            DaemonMessage::Error(ProtocolError::Malformed { .. })
        ));
    }

    /// A peer that connects and says nothing costs a task and a descriptor
    /// for as long as it likes. A reconnect loop doing it takes the listener
    /// down, and the daemon treats a dead listener as fatal.
    #[tokio::test(start_paused = true)]
    async fn a_client_that_never_speaks_does_not_hold_its_slot_forever() {
        let (client, server) = UnixStream::pair().unwrap();
        let hub = StateHub::new();
        let (commands, _taken) = bridge(CommandOutcome::Accepted);

        // Returns rather than parking forever; the paused clock reaches the
        // handshake deadline as soon as nothing else can run.
        serve_client(server, hub, commands).await.unwrap();

        let mut answer = String::new();
        BufReader::new(client).read_line(&mut answer).await.unwrap();
        assert!(
            matches!(
                serde_json::from_str::<DaemonMessage>(&answer).unwrap(),
                DaemonMessage::Error(ProtocolError::Malformed { .. })
            ),
            "and it is told why: {answer}"
        );
    }

    /// A client turned away has to be able to tell "full" from "broken", or
    /// it retries against a daemon that will keep refusing it.
    #[tokio::test]
    async fn a_refused_client_is_told_the_daemon_is_full() {
        let (client, server) = UnixStream::pair().unwrap();
        reject(server).await;

        let mut answer = String::new();
        BufReader::new(client).read_line(&mut answer).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonMessage>(&answer).unwrap(),
            DaemonMessage::Error(ProtocolError::TooManyClients { limit }) if limit == MAX_CLIENTS
        ));
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
